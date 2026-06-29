//! Local Axum web server + `/ws/live_signals` WebSocket.
//!
//! Serves the dashboard, streams the latest `SignalPacket` on every update
//! (woken by `notify`), and applies inbound budget/risk-meter changes to
//! `settings` so the analytics loop re-sizes on its next tick.
//!
//! It also backs the Intraday Suggestion page: a per-stock deep-dive
//! (`/api/suggest/:symbol`), a cached 10-Buy / 10-Sell scanner (`/api/scanner`),
//! the NIFTY regime/breadth context (`/api/regime`), and the symbol picker
//! (`/api/symbols`). The heavy engine calls run on `spawn_blocking` (they use
//! DuckDB + rayon) so the async runtime is never blocked.
//!
//! Signals-only: nothing here ever reaches a broker. The server merely renders
//! advisory rankings and echoes user settings back to the UI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::cache::{CapRiskKey, Cached, KeyedCache};
use crate::config::UserSettings;
use crate::strategy_engine::EdgeIndex;
use crate::tradability::TradabilityResult;
use crate::types::{
    FinderResult, HoldingInput, JournalEntry, JournalUpdate, PortfolioAnalysis, PortfolioMetrics,
    RegimeInfo, ScanResult, SignalPacket, StockSuggestion, SwingCatalog,
};

/// Shared application state handed to the server and updated by the analytics
/// loop. Cheaply cloneable (all fields are `Arc`).
#[derive(Clone)]
pub struct AppState {
    /// Latest signal packet to push to clients.
    pub packet: Arc<RwLock<SignalPacket>>,
    /// User-controlled budget/risk, mutated by the UI.
    pub settings: Arc<RwLock<UserSettings>>,
    /// Fired whenever `packet` is refreshed, to wake WS pushers.
    pub notify: Arc<Notify>,
    /// Directory containing `index.html` / `intraday.html`.
    pub static_dir: PathBuf,
    /// Parquet archive root, for on-demand suggestion analysis.
    pub root: PathBuf,
    /// Warm, stale-while-revalidate caches for the heavy universe scans. Each is
    /// served instantly and refreshed in the background (see [`read_through`]).
    pub scanner: Arc<Cached<ScanResult>>,
    pub regime: Arc<Cached<RegimeInfo>>,
    pub swing: Arc<Cached<SwingCatalog>>,
    /// Capital+risk-keyed cache for the finder (and the desk's staging console,
    /// which reuses the same keys at the pool capital × risk tier).
    pub finder: Arc<KeyedCache<FinderResult>>,
    /// Signal-freeze state (manual button or synthetic-drawdown circuit breaker).
    pub freeze: Arc<RwLock<crate::types::FreezeState>>,
    /// Manual-validation journal (file-based DuckDB), behind a Mutex for the
    /// async handlers (each op runs on spawn_blocking).
    pub journal: Arc<std::sync::Mutex<duckdb::Connection>>,
    /// Eligible-edge index, for cross-referencing the user's holdings against
    /// our edge map (honest context — never advice).
    pub edge_index: Arc<EdgeIndex>,
    /// Timeframe this server loaded its edge map for (the "live" map behind the
    /// Top-10). Used by the freshness panel to mark which tf is live.
    pub edge_tf: crate::config::Timeframe,
    /// Display-only tradability/liquidity/surveillance flags (warm cache).
    pub tradability: Arc<Cached<TradabilityResult>>,
}

/// Run the Axum server until the process exits.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws/live_signals", get(ws_handler))
        .route("/intraday", get(intraday_handler))
        .route("/api/symbols", get(symbols_handler))
        .route("/api/edge_map_status", get(edge_map_status_handler))
        .route("/api/tradability", get(tradability_handler))
        .route("/api/suggest/{symbol}", get(suggest_handler))
        .route("/api/scanner", get(scanner_handler))
        .route("/api/finder", get(finder_handler))
        .route("/api/regime", get(regime_handler))
        // --- Trading Desk ---
        .route("/desk", get(desk_handler))
        .route("/api/freeze", get(freeze_get_handler))
        .route("/api/signal_freeze", post(signal_freeze_handler))
        .route("/api/staging", get(staging_handler))
        .route("/api/swing", get(swing_handler))
        .route("/api/portfolio", get(portfolio_handler))
        .route("/api/holdings", post(holdings_handler))
        // --- Portfolio Analytics (dedicated page: upload PDF/Excel/CSV) ---
        .route("/portfolio", get(portfolio_page_handler))
        .route("/api/portfolio/upload", post(portfolio_upload_handler))
        .route("/api/capital_plan", get(capital_plan_handler))
        .route("/add_stock", get(add_stock_page_handler))
        .route("/api/add_stock", post(add_stock_handler))
        .route("/api/onboard_symbol", post(onboard_symbol_handler))
        .route("/api/enrich_symbol", post(enrich_symbol_handler))
        .route("/api/journal", get(journal_get_handler))
        .route("/api/journal/log", post(journal_log_handler))
        .route("/api/journal/update", post(journal_update_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    info!("RAM_ISTP dashboard listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("axum server error")?;

    Ok(())
}

/// `GET /` — read `{static_dir}/index.html` at request time so UI edits show on
/// refresh without a rebuild.
async fn index_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("index.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            warn!("failed to read {}: {e:#}", path.display());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read {}: {e}", path.display()),
            )
                .into_response()
        }
    }
}

/// `GET /intraday` — read `{static_dir}/intraday.html` at request time so UI
/// edits show on refresh without a rebuild.
async fn intraday_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("intraday.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            warn!("failed to read {}: {e:#}", path.display());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read {}: {e}", path.display()),
            )
                .into_response()
        }
    }
}

/// `GET /ws/live_signals` — upgrade to a WebSocket that streams packets and
/// accepts settings updates.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Optional `capital` / `risk` query params shared by the suggest + scanner
/// endpoints. `risk` arrives as a percent (e.g. 2.5), converted to a fraction
/// before reaching the engine.
#[derive(Debug, Clone, Copy, Deserialize)]
struct SuggestParams {
    capital: Option<f64>,
    risk: Option<f64>,
    /// Optional max-ATR (₹/share) ceiling for the finder — keep only stocks at or
    /// below this volatility. Applied per-request so the (capital,risk) cache stays valid.
    max_atr: Option<f64>,
}

/// `GET /api/symbols` — the intraday-tradeable universe, sorted; `[]` on error.
async fn symbols_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let symbols = tokio::task::spawn_blocking(move || {
        crate::storage_kernel::discover_symbols(&root).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    Json(symbols).into_response()
}

// ---------------------------------------------------------------------------
// Edge-map freshness / scope panel (honesty-layer, display-only)
// ---------------------------------------------------------------------------

/// Per-timeframe scope+freshness for the freshness banner.
#[derive(Serialize)]
struct EdgeMapTfStatus {
    timeframe: String,
    /// This is the tf the running server loaded its live Top-10 from.
    is_live_tf: bool,
    /// Build timestamp (IST). From the `.meta.json` sidecar when present,
    /// otherwise the map file's last-modified time (see `built_is_file_mtime`).
    built_ist: String,
    /// True when `built_ist` is the map file's mtime, not a recorded build stamp
    /// (i.e. the map predates freshness tracking). Surfaced so the UI never
    /// presents an approximate time as an exact one.
    built_is_file_mtime: bool,
    /// Universe size recorded at build time (None when only the mtime is known).
    universe_at_build: Option<usize>,
    /// Distinct symbols carried in the map.
    backtested_symbols: usize,
    /// Distinct symbols with ≥1 eligible edge.
    eligible_symbols: usize,
    /// Total eligible edges.
    eligible_edges: usize,
    /// Total records (eligible + ineligible).
    total_records: usize,
    /// Symbols on disk now that are NOT in this map (incl. just-added stocks).
    new_since_build: usize,
    /// First handful of those new names, for the UI tooltip.
    new_symbols_sample: Vec<String>,
    /// Backtested symbols whose `minute/` candles were modified after the build
    /// (their rows in the map may be stale).
    files_changed: usize,
}

/// Top-level freshness payload across all populated timeframes.
#[derive(Serialize)]
struct EdgeMapStatus {
    checked_ist: String,
    /// The tf the live Top-10 is served from.
    live_tf: String,
    /// Current on-disk intraday universe (symbols with a `minute/` file).
    universe_now: usize,
    per_tf: Vec<EdgeMapTfStatus>,
}

/// File last-modified as a `SystemTime`, if it can be read.
fn file_mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// `SystemTime` → `YYYY-MM-DD HH:MM:SS` in IST.
fn systemtime_to_ist(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.with_timezone(&crate::config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Parse a recorded `YYYY-MM-DD HH:MM:SS` IST build stamp back to a `SystemTime`
/// (best-effort; used only to count candle files modified since the build).
fn parse_ist_to_systemtime(s: &str) -> Option<std::time::SystemTime> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let dt = crate::config::IST.from_local_datetime(&naive).single()?;
    let unix = dt.timestamp();
    if unix < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix as u64))
}

/// `GET /api/edge_map_status` — honest freshness/scope of every populated edge
/// map: how many symbols were backtested vs how many are on disk now, how many
/// carry an eligible edge, and how many are new or have fresher candles since
/// the map was built. Pure display surface — never gates or ranks anything.
async fn edge_map_status_handler(State(state): State<AppState>) -> Response {
    use crate::config::Timeframe;
    use std::collections::BTreeSet;

    let root = state.root.clone();
    let live_tf = state.edge_tf;

    let status = tokio::task::spawn_blocking(move || {
        // Live universe (the diff target), computed once.
        let universe_syms = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        let universe_now = universe_syms.len();

        // Candidate timeframes the product builds edge maps for; include only
        // those with a map file actually present.
        let candidates = [
            Timeframe::Min5,
            Timeframe::Min15,
            Timeframe::Min30,
            Timeframe::Min60,
            Timeframe::Daily,
        ];

        let mut per_tf: Vec<EdgeMapTfStatus> = Vec::new();
        for tf in candidates {
            let map_path = crate::strategy_engine::edge_map_path(tf);
            if !map_path.exists() {
                continue;
            }

            // Prefer the freshness sidecar; fall back to parsing the map and
            // using its file mtime as an honestly-labelled approximate build time.
            let (
                built_ist,
                built_is_file_mtime,
                universe_at_build,
                backtested_set,
                eligible_symbols,
                eligible_edges,
                total_records,
                build_instant,
            ): (String, bool, Option<usize>, BTreeSet<String>, usize, usize, usize, Option<std::time::SystemTime>) =
                match crate::strategy_engine::load_edge_map_meta(tf) {
                    Ok(meta) => {
                        let set: BTreeSet<String> = meta.symbols.iter().cloned().collect();
                        let instant = parse_ist_to_systemtime(&meta.built_ist);
                        (
                            meta.built_ist,
                            false,
                            Some(meta.universe_at_build),
                            set,
                            meta.eligible_symbols,
                            meta.eligible_edges,
                            meta.total_records,
                            instant,
                        )
                    }
                    Err(_) => {
                        // No sidecar (map predates freshness tracking): derive
                        // counts from the map, time from the file mtime.
                        let records = crate::strategy_engine::load_edge_map(tf).unwrap_or_default();
                        let mut set: BTreeSet<String> = BTreeSet::new();
                        let mut elig_syms: BTreeSet<String> = BTreeSet::new();
                        let mut elig_edges = 0usize;
                        for r in &records {
                            set.insert(r.symbol.clone());
                            if r.eligible {
                                elig_syms.insert(r.symbol.clone());
                                elig_edges += 1;
                            }
                        }
                        let mtime = file_mtime(&map_path);
                        let stamp = mtime.map(systemtime_to_ist).unwrap_or_else(|| "unknown".to_string());
                        (
                            stamp,
                            true,
                            None,
                            set,
                            elig_syms.len(),
                            elig_edges,
                            records.len(),
                            mtime,
                        )
                    }
                };

            // NEW-since-build: on disk now but not in this map.
            let new_syms: Vec<String> = universe_syms
                .iter()
                .filter(|s| !backtested_set.contains(s.as_str()))
                .cloned()
                .collect();
            let new_since_build = new_syms.len();
            let new_symbols_sample: Vec<String> = new_syms.into_iter().take(20).collect();

            // files-changed: backtested symbols whose minute candles were touched
            // after the build (their map rows may be stale).
            let files_changed = match build_instant {
                Some(build_t) => backtested_set
                    .iter()
                    .filter(|sym| {
                        let p = crate::config::parquet_path(&root, sym, Timeframe::Minute);
                        file_mtime(&p).map(|m| m > build_t).unwrap_or(false)
                    })
                    .count(),
                None => 0,
            };

            per_tf.push(EdgeMapTfStatus {
                timeframe: tf.dir().to_string(),
                is_live_tf: tf == live_tf,
                built_ist,
                built_is_file_mtime,
                universe_at_build,
                backtested_symbols: backtested_set.len(),
                eligible_symbols,
                eligible_edges,
                total_records,
                new_since_build,
                new_symbols_sample,
                files_changed,
            });
        }

        // Live tf first, then by name — so the banner leads with the live map.
        per_tf.sort_by(|a, b| {
            b.is_live_tf
                .cmp(&a.is_live_tf)
                .then_with(|| a.timeframe.cmp(&b.timeframe))
        });

        EdgeMapStatus {
            checked_ist: chrono::Utc::now()
                .with_timezone(&crate::config::IST)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            live_tf: live_tf.dir().to_string(),
            universe_now,
            per_tf,
        }
    })
    .await;

    match status {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            warn!("edge_map_status task panicked: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "edge_map_status failed").into_response()
        }
    }
}

/// `GET /api/tradability` — display-only tradability/liquidity/surveillance flags
/// for the whole daily universe (series/T2T, median ₹ turnover, price floor,
/// micro-cap; ASM/GSM honestly "not loaded"). Warm, stale-while-revalidate.
/// Never gates, ranks, or sizes anything — the UI joins it client-side to add a
/// caption next to a signal.
async fn tradability_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let compute = move || {
        let conn = crate::storage_kernel::open_conn().ok()?;
        let built = now_ist_string();
        Some(crate::tradability::build_index(
            &conn,
            &root,
            std::path::Path::new("cache"),
            built,
        ))
    };
    match read_through(state.tradability.clone(), "tradability", compute, |t| t.built_ist.clone()).await {
        Some(t) => Json(t).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "tradability scan failed").into_response(),
    }
}

/// `GET /api/suggest/:symbol` — full per-stock deep-dive (4 strategy blocks).
/// Heavy DuckDB + rayon work runs on a blocking thread. 500 on engine error.
async fn suggest_handler(
    State(state): State<AppState>,
    Path(symbol): Path<String>,
    Query(params): Query<SuggestParams>,
) -> Response {
    let root = state.root.clone();
    let capital = params.capital.unwrap_or(100000.0);
    let risk = params.risk.unwrap_or(2.5) / 100.0;
    let symbol_uc = symbol.to_uppercase();

    let result: Result<StockSuggestion> = tokio::task::spawn_blocking(move || {
        crate::suggestion_engine::analyze_symbol(&root, &symbol_uc, capital, risk)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("suggest task panicked: {e}")));

    match result {
        Ok(s) => Json(s).into_response(),
        Err(e) => {
            warn!("analyze_symbol({symbol}) failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

/// Stale-while-revalidate read-through over a [`Cached`] slot.
///
/// - **Fresh hit** → return it immediately.
/// - **Stale hit** → return the stale value now *and* kick a detached background
///   refresh (only if we win the single-flight race). The request never waits.
/// - **Cold miss** → if we win single-flight, compute inline once and store;
///   otherwise wait for the in-flight winner (startup warm / another request) so
///   the universe scan runs at most once.
///
/// `compute` is the heavy synchronous work; it runs on `spawn_blocking`, so the
/// async runtime is never blocked, and no lock is ever held across an `.await`
/// (every guard lives entirely inside a `Cached` method call).
async fn read_through<T, F>(
    cache: Arc<Cached<T>>,
    label: &'static str,
    compute: F,
    built_ist_of: fn(&T) -> String,
) -> Option<T>
where
    T: Clone + Send + Sync + 'static,
    F: Fn() -> Option<T> + Send + Sync + Clone + 'static,
{
    let look = cache.lookup();
    if let Some(value) = look.value {
        if look.stale && cache.try_begin_refresh() {
            let cache2 = cache.clone();
            let compute2 = compute.clone();
            tokio::spawn(async move {
                let t0 = std::time::Instant::now();
                let computed =
                    tokio::task::spawn_blocking(move || compute2()).await.ok().flatten();
                match computed {
                    Some(val) => {
                        let stamp = built_ist_of(&val);
                        cache2.store(val, stamp);
                        info!("api={label} cache=refresh compute_ms={}", t0.elapsed().as_millis());
                    }
                    None => cache2.abort_refresh(),
                }
            });
        }
        return Some(value);
    }

    // Cold miss.
    if cache.try_begin_refresh() {
        let t0 = std::time::Instant::now();
        let compute2 = compute.clone();
        let computed = tokio::task::spawn_blocking(move || compute2()).await.ok().flatten();
        match computed {
            Some(val) => {
                cache.store(val.clone(), built_ist_of(&val));
                info!("api={label} cache=miss compute_ms={}", t0.elapsed().as_millis());
                Some(val)
            }
            None => {
                cache.abort_refresh();
                None
            }
        }
    } else {
        // Someone else (startup warm or a concurrent request) owns the compute.
        // Wait for it rather than duplicating the ~minute-long scan.
        for _ in 0..1800 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if let Some(v) = cache.lookup().value {
                info!("api={label} cache=wait-hit");
                return Some(v);
            }
        }
        // Winner stalled/died: fall back to computing inline.
        let compute2 = compute.clone();
        tokio::task::spawn_blocking(move || compute2()).await.ok().flatten()
    }
}

/// `GET /api/scanner` — the Top-10 Buy / Sell scanner. The ~minute-long universe
/// scan is warmed at startup and refreshed in the background; requests serve the
/// cached result instantly. Never holds a sync lock across an `.await`.
async fn scanner_handler(
    State(state): State<AppState>,
    Query(params): Query<SuggestParams>,
) -> Response {
    let root = state.root.clone();
    let capital = params.capital.unwrap_or(100000.0);
    let risk = params.risk.unwrap_or(2.5) / 100.0;
    let compute = move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Some(crate::suggestion_engine::scan_universe(&root, &symbols, capital, risk))
    };
    match read_through(state.scanner.clone(), "scanner", compute, |r: &ScanResult| {
        r.built_ist.clone()
    })
    .await
    {
        // The cached scan is capital/risk-INDEPENDENT; size the returned clone to
        // the request's capital + risk% (shares + net P&L) before sending.
        Some(mut r) => {
            crate::suggestion_engine::size_scan_result(&mut r, capital, risk);
            Json(r).into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "scan_universe failed").into_response(),
    }
}

/// `GET /api/finder` — Capital-Fit ATR finder. Sizes every symbol's best edge
/// to the requested capital + risk and returns ALL that fit, ranked by
/// fit-adjusted edge. Computed on demand (depends on capital/risk), ~30s.
async fn finder_handler(
    State(state): State<AppState>,
    Query(params): Query<SuggestParams>,
) -> Response {
    let root = state.root.clone();
    let capital = params.capital.unwrap_or(100000.0);
    let risk = params.risk.unwrap_or(2.5) / 100.0;
    let slot = state.finder.slot(CapRiskKey::new(capital, risk));
    let compute = move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Some(crate::suggestion_engine::find_capital_fit(&root, &symbols, capital, risk))
    };
    match read_through(slot, "finder", compute, |r: &FinderResult| r.built_ist.clone()).await {
        // ATR ceiling is applied per-request on the cached (capital,risk) result,
        // so the cache stays valid and `max_atr_universe` (the slider top) is kept.
        Some(mut r) => {
            if let Some(cap_atr) = params.max_atr {
                if cap_atr > 0.0 {
                    r.rows.retain(|row| row.atr <= cap_atr);
                    r.qualifying = r.rows.len();
                }
            }
            Json(r).into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "find_capital_fit failed").into_response(),
    }
}

/// `GET /api/regime` — NIFTY regime + market breadth (display-only context).
/// Warmed at startup, refreshed in the background; served instantly.
async fn regime_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let compute = move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Some(crate::suggestion_engine::compute_regime(&root, &symbols))
    };
    match read_through(state.regime.clone(), "regime", compute, |r: &RegimeInfo| {
        r.built_ist.clone()
    })
    .await
    {
        Some(r) => Json(r).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "compute_regime failed").into_response(),
    }
}

/// Serialize the current packet to JSON, cloning out of the guard so we never
/// hold a sync lock across an `.await`.
fn current_packet_json(state: &AppState) -> Option<String> {
    let packet = {
        let guard = state.packet.read().ok()?;
        guard.clone()
    };
    serde_json::to_string(&packet).ok()
}

/// Drive one WebSocket connection: push the latest packet on every notify and
/// apply inbound `SettingsMsg` updates.
async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();

    // Send the current packet immediately so a fresh client renders at once.
    if let Some(json) = current_packet_json(&state) {
        if sink.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            // (a) Push loop: wake on a packet refresh, send the latest snapshot.
            _ = state.notify.notified() => {
                if let Some(json) = current_packet_json(&state) {
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            // (b) Recv loop: apply settings updates; echo packet to confirm.
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let raw = text.as_str();
                        if let Ok(update) =
                            serde_json::from_str::<crate::types::SettingsMsg>(raw)
                        {
                            {
                                let mut guard = match state.settings.write() {
                                    Ok(g) => g,
                                    Err(_) => break,
                                };
                                *guard = UserSettings::new(update.budget, update.risk_pct);
                            }
                            // Confirm the applied settings by echoing the packet.
                            if let Some(json) = current_packet_json(&state) {
                                if sink.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        // Ignore parse errors; keep the connection open.
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(_)))
                    | Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Binary(_))) => {
                        // Non-text frames carry no settings; ignore.
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
}

// ===========================================================================
// Trading Desk handlers
// ===========================================================================

/// `GET /desk` — serve the Trading Desk page.
async fn desk_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("desk.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not read {}: {e}", path.display()))
            .into_response(),
    }
}

/// `GET /api/freeze` — current signal-freeze state.
async fn freeze_get_handler(State(state): State<AppState>) -> Response {
    let fs = state.freeze.read().map(|g| g.clone()).ok();
    match fs {
        Some(f) => Json(f).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "freeze lock poisoned").into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct FreezeReq {
    frozen: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /api/signal_freeze` — manual kill switch. Sets/clears the freeze flag;
/// the live broadcast loop checks it and stops emitting new signals when frozen.
/// Background data logging is unaffected.
async fn signal_freeze_handler(
    State(state): State<AppState>,
    Json(req): Json<FreezeReq>,
) -> Response {
    let result = {
        let mut g = match state.freeze.write() {
            Ok(g) => g,
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "freeze lock poisoned").into_response(),
        };
        g.frozen = req.frozen;
        g.reason = if req.frozen {
            req.reason.unwrap_or_else(|| "Manual signal freeze".to_string())
        } else {
            String::new()
        };
        g.clone()
    };
    state.notify.notify_waiters(); // wake the pusher so the UI reflects it at once
    if result.frozen {
        warn!("SIGNAL FREEZE engaged: {}", result.reason);
    } else {
        info!("signal freeze cleared");
    }
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
struct StagingQuery {
    #[serde(default)]
    risk: Option<String>,
}

#[derive(serde::Serialize)]
struct StagingResp {
    top_long: Vec<crate::types::StagedSignal>,
    top_short: Vec<crate::types::StagedSignal>,
    capital: f64,
    risk_tier: String,
}

fn parse_tier(s: &str) -> crate::config::RiskTier {
    match s {
        "Conservative" => crate::config::RiskTier::Conservative,
        "Aggressive" => crate::config::RiskTier::Aggressive,
        _ => crate::config::RiskTier::Moderate,
    }
}

/// `GET /api/staging?risk=Moderate` — Top 5 Long / Short staged Bracket Orders,
/// sized to the capital pool at the chosen risk tier. Synthetic / manual only.
async fn staging_handler(State(state): State<AppState>, Query(q): Query<StagingQuery>) -> Response {
    let root = state.root.clone();
    let tier = parse_tier(q.risk.as_deref().unwrap_or("Moderate"));
    let capital = crate::config::CAPITAL_POOL;
    let risk_pct = tier.pct();

    // Reuse the finder cache: the desk's three tiers all map to (CAPITAL_POOL ×
    // tier.pct()), so a warm finder key serves staging instantly too.
    let slot = state.finder.slot(CapRiskKey::new(capital, risk_pct));
    let compute = {
        let root = root.clone();
        move || {
            let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
            Some(crate::suggestion_engine::find_capital_fit(&root, &symbols, capital, risk_pct))
        }
    };
    let fit = match read_through(slot, "staging", compute, |r: &FinderResult| r.built_ist.clone()).await {
        Some(f) => f,
        None => return (StatusCode::INTERNAL_SERVER_ERROR, "staging fit failed").into_response(),
    };

    // Cheap synchronous transform: turn fit rows into staged bracket orders.
    let mut longs = Vec::new();
    let mut shorts = Vec::new();
    for row in fit.rows.iter() {
        let dir = if row.side == "BUY" {
            crate::config::Direction::Long
        } else {
            crate::config::Direction::Short
        };
        let staged = crate::execution_staging::stage_signal(
            &row.symbol, 0, dir, row.entry, row.atr, row.shares,
            crate::config::SL_ATR_MULT, crate::config::DEFAULT_RR,
        );
        if dir == crate::config::Direction::Long {
            if longs.len() < 5 { longs.push(staged); }
        } else if shorts.len() < 5 {
            shorts.push(staged);
        }
        if longs.len() >= 5 && shorts.len() >= 5 { break; }
    }
    Json(StagingResp {
        top_long: longs,
        top_short: shorts,
        capital,
        risk_tier: tier.as_str().to_string(),
    })
    .into_response()
}

/// `GET /api/swing` — multi-day Swing Trades Catalog (DuckDB daily scan).
/// Warmed at startup, refreshed in the background; served instantly.
async fn swing_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let compute = move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Some(crate::swing_analyzer::scan_swing(&root, &symbols))
    };
    match read_through(state.swing.clone(), "swing", compute, |c: &SwingCatalog| {
        c.built_ist.clone()
    })
    .await
    {
        Some(c) => Json(c).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "scan_swing failed").into_response(),
    }
}

/// `GET /api/portfolio` — post-trade analytics from the manual journal.
async fn portfolio_handler(State(state): State<AppState>) -> Response {
    let journal = state.journal.clone();
    let metrics = tokio::task::spawn_blocking(move || {
        let conn = journal.lock().map_err(|_| ()).ok()?;
        let entries = crate::journal_sync::all_entries(&conn).ok()?;
        Some(crate::portfolio_analytics::compute(&entries))
    })
    .await
    .ok()
    .flatten();
    match metrics {
        Some(m) => Json::<PortfolioMetrics>(m).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "portfolio compute failed").into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct HoldingsRequest {
    #[serde(default)]
    holdings: Vec<HoldingInput>,
    /// Raw CSV text (broker export). Parsed with header-aliasing.
    #[serde(default)]
    csv: Option<String>,
    /// Pasted / PDF-extracted text. Best-effort parse.
    #[serde(default)]
    text: Option<String>,
    /// Load the built-in sample portfolio instead of any supplied holdings.
    #[serde(default)]
    use_sample: bool,
    /// Load the owner's real consolidated book (the one-click preset).
    #[serde(default)]
    use_mine: bool,
}

#[derive(serde::Serialize)]
struct HoldingsResponse {
    analysis: PortfolioAnalysis,
    /// Rotation & growth layer: per-holding leader/laggard read, edge-backed
    /// uptrend buy candidates, an illustrative rebalance, and growth scenarios.
    /// Display-only — descriptive evidence, never advice or an order.
    rotation: crate::types::RotationAnalysis,
    warnings: Vec<String>,
}

/// Where a holdings request's rows come from: a resolved preset (the owner's book
/// / sample, already trading symbols) or raw rows that still need company-name →
/// symbol resolution.
enum HoldingSource {
    Preset(Vec<crate::types::Holding>),
    Raw(Vec<HoldingInput>),
}

/// Shared holdings pipeline (used by both `/api/holdings` and the upload path):
/// resolve names → NSE symbols, merge duplicate symbols, mark (statement close
/// preferred, archive close otherwise), analyze, attach the correlation read, and
/// build the rotation/growth layer. Display-only throughout; never an order.
fn build_holdings_response(
    root: &std::path::Path,
    edges: &crate::strategy_engine::EdgeIndex,
    source: HoldingSource,
    now: String,
    mut warnings: Vec<String>,
) -> HoldingsResponse {
    let conn = crate::storage_kernel::open_conn().ok();

    // 1) Resolve (raw only) + merge duplicate symbols into one row each.
    let holdings: Vec<crate::types::Holding> = match source {
        HoldingSource::Preset(hs) => crate::holdings_analytics::merge_holdings(hs),
        HoldingSource::Raw(inputs) => {
            let resolver = conn.as_ref().map(|c| crate::symbol_resolver::SymbolResolver::load(c, root));
            let resolved: Vec<crate::types::Holding> = inputs
                .iter()
                .map(|inp| {
                    let mut h = crate::holdings_analytics::normalize(inp);
                    if let Some(res) = &resolver {
                        let r = res.resolve(&h.symbol, inp.isin.as_deref());
                        if !r.matched {
                            warnings.push(format!(
                                "couldn't map \"{}\" to an NSE symbol — shown as-is; trend/edge analysis may be unavailable for it",
                                h.symbol
                            ));
                        } else if r.how == "fuzzy" {
                            warnings.push(format!(
                                "mapped \"{}\" to {} by name similarity — please verify it's the right stock",
                                h.symbol, r.symbol
                            ));
                        }
                        h.symbol = r.symbol;
                        if h.sector.is_none() {
                            h.sector = r.sector;
                        }
                    }
                    h
                })
                .collect();
            crate::holdings_analytics::merge_holdings(resolved)
        }
    };
    if holdings.is_empty() {
        warnings.push("No holdings were recognised — upload an Excel/CSV export or paste the rows.".to_string());
    }

    // 2) Marks: a statement close (carried on the holding) is used inside analyze;
    //    only fetch the archive close for names that didn't bring one.
    let mut marks: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    if let Some(ref conn) = conn {
        for h in &holdings {
            if h.last_price.is_none() {
                if let Some(px) = crate::holdings_analytics::latest_daily_close(conn, root, &h.symbol) {
                    marks.insert(h.symbol.clone(), px);
                }
            }
        }
    }

    let mut analysis = crate::holdings_analytics::analyze(&holdings, &marks, edges, now.clone());
    // Real "independent bets": upgrade the weight-only figure with a
    // return-correlation read over the names that have archive history.
    if let Some(ref conn) = conn {
        let syms: Vec<String> = analysis.holdings.iter().map(|h| h.symbol.clone()).collect();
        if let Some(ctx) = crate::holdings_analytics::compute_correlation(
            conn,
            root,
            &syms,
            crate::config::CORR_LOOKBACK_SESSIONS,
            crate::config::CORR_MIN_SESSIONS,
        ) {
            crate::holdings_analytics::attach_correlation(&mut analysis, &ctx);
        }
    }
    // If correlation couldn't be computed at all (< 2 names with history), still
    // surface which holdings lack daily history — never a silently-partial read.
    if analysis.corr_effective_bets.is_none() && analysis.corr_names_dropped.is_empty() {
        let dropped: Vec<String> = analysis
            .holdings
            .iter()
            .filter(|h| !crate::config::parquet_path(root, &h.symbol, crate::config::Timeframe::Daily).exists())
            .map(|h| h.symbol.clone())
            .collect();
        analysis.corr_names_dropped = dropped;
    }
    // Rotation & growth (display-only): per-holding trend/relative-strength read,
    // edge-backed uptrend buy candidates, an illustrative rebalance + scenarios.
    let rotation = match conn {
        Some(ref c) => crate::portfolio_rotation::build(c, root, &analysis.holdings, edges, now),
        None => crate::portfolio_rotation::empty(now),
    };
    HoldingsResponse { analysis, rotation, warnings }
}

/// `POST /api/holdings` — analyse the user's REAL holdings (manual JSON, CSV,
/// pasted rows, or the sample set) into their risk picture. Display-only: marks
/// are local EOD / statement closes (flagged not-live), nothing is an order, no
/// advice. The heavy DuckDB reads run on `spawn_blocking`.
async fn holdings_handler(State(state): State<AppState>, Json(req): Json<HoldingsRequest>) -> Response {
    let root = state.root.clone();
    let edges = state.edge_index.clone();
    let now = now_ist_string();

    let resp = tokio::task::spawn_blocking(move || {
        let mut warnings: Vec<String> = Vec::new();
        let source = if req.use_mine {
            HoldingSource::Preset(crate::holdings_analytics::my_portfolio())
        } else if req.use_sample {
            HoldingSource::Preset(crate::holdings_analytics::sample_holdings())
        } else {
            let mut inputs: Vec<HoldingInput> = Vec::new();
            if let Some(csv) = req.csv.as_deref() {
                let (i, w) = crate::holdings_analytics::parse_csv(csv.as_bytes());
                inputs.extend(i);
                warnings.extend(w);
            }
            if let Some(txt) = req.text.as_deref() {
                let (i, w) = crate::holdings_analytics::parse_text(txt);
                inputs.extend(i);
                warnings.extend(w);
            }
            inputs.extend(req.holdings);
            HoldingSource::Raw(inputs)
        };
        build_holdings_response(&root, &edges, source, now, warnings)
    })
    .await;

    match resp {
        Ok(r) => Json(r).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("holdings task panicked: {e}")).into_response(),
    }
}

/// `GET /portfolio` — the dedicated Portfolio Analytics page (upload Excel/CSV or
/// paste rows, or one-click your own book). Read at request time so UI edits show.
async fn portfolio_page_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("portfolio.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not read portfolio.html: {e}")).into_response(),
    }
}

/// `POST /api/portfolio/upload` — a multipart file (Excel / CSV) → parsed holdings
/// → the full risk picture + rotation/growth. Names are resolved company-name →
/// NSE symbol. Display-only ingest of the user's OWN holdings; never an order.
async fn portfolio_upload_handler(State(state): State<AppState>, mut multipart: Multipart) -> Response {
    let root = state.root.clone();
    let edges = state.edge_index.clone();
    let now = now_ist_string();

    // Take the first uploaded file field.
    let mut filename = String::new();
    let mut bytes: Vec<u8> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        if let Some(fname) = field.file_name() {
            filename = fname.to_string();
        }
        if let Ok(b) = field.bytes().await {
            if !b.is_empty() {
                bytes = b.to_vec();
                break;
            }
        }
    }
    if bytes.is_empty() {
        return (StatusCode::BAD_REQUEST, "no file received").into_response();
    }

    let resp = tokio::task::spawn_blocking(move || {
        let imp = crate::portfolio_import::import_bytes(&filename, &bytes);
        build_holdings_response(&root, &edges, HoldingSource::Raw(imp.holdings), now, imp.warnings)
    })
    .await;

    match resp {
        Ok(r) => Json(r).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("upload task panicked: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct CapitalPlanQuery {
    #[serde(default)]
    years: Option<u32>,
    #[serde(default)]
    capital: Option<f64>,
}

/// `GET /api/capital_plan?years=5&capital=200000` — horizon-aware screen of the
/// broad universe into an illustrative ₹ allocation. Display-only, historical
/// evidence only, never a forecast or an order. Heavy DuckDB reads on `spawn_blocking`.
async fn capital_plan_handler(State(state): State<AppState>, Query(q): Query<CapitalPlanQuery>) -> Response {
    let root = state.root.clone();
    let edges = state.edge_index.clone();
    let now = now_ist_string();
    let years = q.years.unwrap_or(5);
    let capital = q.capital.unwrap_or(200_000.0).clamp(10_000.0, 10_000_000.0);

    let resp = tokio::task::spawn_blocking(move || match crate::storage_kernel::open_conn().ok() {
        Some(conn) => crate::capital_planner::build(&conn, &root, &edges, capital, years, now),
        None => crate::capital_planner::empty(capital, years, now),
    })
    .await;

    match resp {
        Ok(plan) => Json(plan).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("capital plan task panicked: {e}")).into_response(),
    }
}

/// `GET /add_stock` — the "add a stock from NSE" page (text box → download).
async fn add_stock_page_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("add_stock.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("could not read add_stock.html: {e}")).into_response(),
    }
}

/// True for a syntactically valid NSE trading code (alphanumeric + `&-`, ≤24
/// chars). The strict whitelist is what lets us hand the symbol to a subprocess
/// arg / archive path with no shell-injection or path-escape risk.
fn valid_nse_symbol(sym: &str) -> bool {
    !sym.is_empty()
        && sym.len() <= 24
        && sym.chars().all(|c| c.is_ascii_alphanumeric() || c == '&' || c == '-')
}

#[derive(serde::Deserialize)]
struct AddStockRequest {
    symbol: String,
}

/// `POST /api/add_stock` {symbol} — download one NSE stock's full history into the
/// parquet archive via `download_stock.py` (Yahoo max daily + Kite intraday →
/// resampled). The symbol is strictly validated (alphanumeric + `&-` only) so it
/// can never inject a shell arg or escape the archive path. Returns the
/// downloader's JSON status. Long-running for a brand-new stock — runs on
/// `spawn_blocking`. Read-only w.r.t. trading: it only fetches market data.
async fn add_stock_handler(State(state): State<AppState>, Json(req): Json<AddStockRequest>) -> Response {
    let sym = req.symbol.trim().to_uppercase();
    if !valid_nse_symbol(&sym) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid symbol — use the NSE trading code, e.g. 63MOONS"})),
        )
            .into_response();
    }
    let root = state.root.clone();
    let script = root.parent().map(|p| p.join("download_stock.py")).unwrap_or_else(|| root.join("download_stock.py"));

    let resp = tokio::task::spawn_blocking(move || {
        if !script.exists() {
            return Err(format!("downloader not found at {}", script.display()));
        }
        let out = std::process::Command::new("python3")
            .arg(&script)
            .arg(&sym)
            .arg("--root")
            .arg(&root)
            .output();
        match out {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let last = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
                match serde_json::from_str::<serde_json::Value>(last) {
                    Ok(v) => Ok(v),
                    Err(_) => {
                        let err_tail: String = String::from_utf8_lossy(&o.stderr).lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                        Err(format!("downloader produced no result. {err_tail}"))
                    }
                }
            }
            Err(e) => Err(format!("could not run the downloader (is python3 installed?): {e}")),
        }
    })
    .await;

    match resp {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("add_stock task panicked: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct OnboardRequest {
    symbol: String,
}

/// Result of onboarding one symbol into the live edge map.
#[derive(Serialize)]
struct OnboardResult {
    symbol: String,
    timeframe: String,
    records_added: usize,
    eligible_added: usize,
    replaced_existing: bool,
    total_records: usize,
    eligible_edges: Vec<crate::types::EligibleEdge>,
    note: String,
}

/// `POST /api/onboard_symbol` {symbol} — backtest ONE already-downloaded symbol
/// on the live timeframe and merge its rows into the edge map, replacing only
/// that symbol (every other symbol stays byte-identical, so the documented
/// anchor edge is untouched). This is the incremental alternative to a ~20-minute
/// full-universe rebuild: a single stock onboards in seconds.
///
/// Honesty/scope: same eligibility gate and cost model as the full pass (zero
/// drift for unchanged symbols). It takes effect in the live Top-10 on the next
/// `serve` restart (the in-memory live universe is fixed at startup); it is
/// reflected immediately in the freshness panel and the per-stock deep-dive.
/// Signals-only — touches market data + the cache, never a broker.
async fn onboard_symbol_handler(State(state): State<AppState>, Json(req): Json<OnboardRequest>) -> Response {
    let sym = req.symbol.trim().to_uppercase();
    if !valid_nse_symbol(&sym) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid symbol — use the NSE trading code, e.g. 63MOONS"})),
        )
            .into_response();
    }
    let root = state.root.clone();
    let tf = state.edge_tf;

    let result = tokio::task::spawn_blocking(move || -> std::result::Result<OnboardResult, String> {
        let conn = crate::storage_kernel::open_conn().map_err(|e| format!("duckdb open failed: {e:#}"))?;
        // backtest_symbol errors only when the candles can't be loaded (not
        // downloaded yet); <100 bars / no signals returns an empty Vec.
        let rows = crate::strategy_engine::backtest_symbol(&conn, &root, &sym, tf).map_err(|e| {
            format!(
                "couldn't load {sym} candles on {} — download it first via /add_stock ({e:#})",
                tf.dir()
            )
        })?;
        let universe = crate::storage_kernel::discover_symbols(&root).map(|v| v.len()).unwrap_or(0);
        let eligible_edges: Vec<crate::types::EligibleEdge> = rows
            .iter()
            .filter(|r| r.eligible)
            .map(|r| crate::types::EligibleEdge {
                strategy: r.strategy.clone(),
                direction: r.direction,
                expectancy_r: r.metrics.expectancy,
                profit_factor: r.metrics.profit_factor,
                win_pct: r.metrics.win_pct,
                n: r.metrics.n,
            })
            .collect();
        let outcome = crate::strategy_engine::merge_edge_records(&sym, rows, tf, universe)
            .map_err(|e| format!("merge into edge map failed: {e:#}"))?;
        let note = if outcome.records_added == 0 {
            format!(
                "{sym}: insufficient history (<100 bars) or no signals on {} — nothing added.",
                tf.dir()
            )
        } else if outcome.eligible_added == 0 {
            format!(
                "Backtested {} config(s) for {sym}; none cleared the eligibility gate (n≥30, PF≥1.2, exp>0). Honest result: no {} edge right now.",
                outcome.records_added,
                tf.dir()
            )
        } else {
            format!(
                "Onboarded {sym} to the {} edge map: {} eligible edge(s). Restart `serve` to include it in the live Top-10; visible now in the freshness panel and deep-dive.",
                tf.dir(),
                outcome.eligible_added
            )
        };
        Ok(OnboardResult {
            symbol: outcome.symbol,
            timeframe: outcome.timeframe,
            records_added: outcome.records_added,
            eligible_added: outcome.eligible_added,
            replaced_existing: outcome.replaced_existing,
            total_records: outcome.total_records,
            eligible_edges,
            note,
        })
    })
    .await;

    match result {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("onboard task panicked: {e}")).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct EnrichRequest {
    symbol: String,
}

/// `POST /api/enrich_symbol` {symbol} — onboard one stock's *details* (not candles)
/// via `enrich_stock.py`: upsert its `symbol_metadata` row (sector/industry/mcap/
/// name/isin from Yahoo), append/upsert its corporate actions, write its
/// split-adjusted `daily_adj` slice, and (only if `INDIANAPI_KEY` is set) pull its
/// fundamentals snapshot. The sibling of `add_stock` (candles) — together they make
/// "add a stock" complete end-to-end.
///
/// Honesty/scope: DISPLAY-ONLY reference data. It never touches the edge map, the
/// eligibility gate, Confidence, or any backtest (the intraday backtest reads the
/// raw resampled candles, not `daily_adj/`), so it cannot move an anchor. Symbol is
/// strictly validated (alphanumeric + `&-`) before it reaches the subprocess arg.
/// Each enrichment step is independent — one failing never aborts the rest, and a
/// missing field is reported honestly (never fabricated). Runs on `spawn_blocking`
/// (Yahoo + optional indianapi network). Touches market data + the cache, never a broker.
async fn enrich_symbol_handler(State(state): State<AppState>, Json(req): Json<EnrichRequest>) -> Response {
    let sym = req.symbol.trim().to_uppercase();
    if !valid_nse_symbol(&sym) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid symbol — use the NSE trading code, e.g. 63MOONS"})),
        )
            .into_response();
    }
    let root = state.root.clone();
    let script = root.parent().map(|p| p.join("enrich_stock.py")).unwrap_or_else(|| root.join("enrich_stock.py"));

    let resp = tokio::task::spawn_blocking(move || {
        if !script.exists() {
            return Err(format!("enricher not found at {}", script.display()));
        }
        let out = std::process::Command::new("python3")
            .arg(&script)
            .arg(&sym)
            .arg("--root")
            .arg(&root)
            .output();
        match out {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let last = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
                match serde_json::from_str::<serde_json::Value>(last) {
                    Ok(v) => Ok(v),
                    Err(_) => {
                        let err_tail: String = String::from_utf8_lossy(&o.stderr).lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                        Err(format!("enricher produced no result. {err_tail}"))
                    }
                }
            }
            Err(e) => Err(format!("could not run the enricher (is python3 installed?): {e}")),
        }
    })
    .await;

    match resp {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("enrich_symbol task panicked: {e}")).into_response(),
    }
}

/// `GET /api/journal` — all journal rows (newest first).
async fn journal_get_handler(State(state): State<AppState>) -> Response {
    let journal = state.journal.clone();
    let entries = tokio::task::spawn_blocking(move || {
        let conn = journal.lock().map_err(|_| ()).ok()?;
        crate::journal_sync::all_entries(&conn).ok()
    })
    .await
    .ok()
    .flatten();
    match entries {
        Some(e) => Json::<Vec<JournalEntry>>(e).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "journal read failed").into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct JournalLog {
    symbol: String,
    #[serde(default)]
    instrument_token: u32,
    direction: String,
    #[serde(default)]
    strategy: String,
    #[serde(default)]
    alpha_trigger: String,
    intended_price: f64,
    #[serde(default)]
    qty: i64,
    #[serde(default)]
    sector: Option<String>,
    state: String,
    #[serde(default)]
    actual_fill_price: Option<f64>,
}

/// `POST /api/journal/log` — create a journal row from a staged signal + the
/// user's Accept/Reject decision (and actual fill price when accepted).
async fn journal_log_handler(State(state): State<AppState>, Json(req): Json<JournalLog>) -> Response {
    let journal = state.journal.clone();
    let now = now_ist_string();
    let result = tokio::task::spawn_blocking(move || {
        let conn = journal.lock().map_err(|_| ()).ok()?;
        let st = crate::types::SignalState::from_str(&req.state);
        let entry = JournalEntry {
            id: 0,
            generated_ist: now.clone(),
            entry_ist: if req.actual_fill_price.is_some() { Some(now.clone()) } else { None },
            exit_ist: None,
            instrument_token: req.instrument_token,
            symbol: req.symbol.clone(),
            direction: req.direction.clone(),
            strategy: if req.strategy.is_empty() { "Intraday Staging".to_string() } else { req.strategy.clone() },
            alpha_trigger: req.alpha_trigger.clone(),
            intended_price: req.intended_price,
            actual_fill_price: req.actual_fill_price,
            exit_price: None,
            qty: req.qty,
            state: st.as_str().to_string(),
            pnl: None,
            slippage: req.actual_fill_price.map(|f| {
                let dir = if req.direction == "BUY" { 1.0 } else { -1.0 };
                (f - req.intended_price) * dir
            }),
            sector: req.sector.clone(),
        };
        let id = crate::journal_sync::insert_entry(&conn, &entry).ok()?;
        Some(JournalEntry { id, ..entry })
    })
    .await
    .ok()
    .flatten();
    match result {
        Some(e) => Json::<JournalEntry>(e).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "journal log failed").into_response(),
    }
}

/// `POST /api/journal/update` — relabel a row or close it with an exit price.
async fn journal_update_handler(State(state): State<AppState>, Json(req): Json<JournalUpdate>) -> Response {
    let journal = state.journal.clone();
    let now = now_ist_string();
    let ok = tokio::task::spawn_blocking(move || {
        let conn = journal.lock().map_err(|_| ()).ok()?;
        let st = crate::types::SignalState::from_str(&req.state);
        crate::journal_sync::update_state(&conn, req.id, st, req.actual_fill_price, req.exit_price, &now).ok()
    })
    .await
    .ok()
    .flatten();
    match ok {
        Some(()) => Json(serde_json::json!({"ok": true})).into_response(),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "journal update failed").into_response(),
    }
}

/// Current IST wall-clock "YYYY-MM-DD HH:MM:SS".
fn now_ist_string() -> String {
    chrono::Utc::now()
        .with_timezone(&crate::config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

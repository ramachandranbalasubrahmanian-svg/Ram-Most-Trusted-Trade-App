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
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use serde::Deserialize;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::config::UserSettings;
use crate::types::{
    FinderResult, FreezeState, JournalEntry, JournalUpdate, PortfolioMetrics, RegimeInfo,
    ScanResult, SignalPacket, StockSuggestion, SwingCatalog,
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
    /// Cached 10-Buy / 10-Sell scanner result (computed lazily on first request).
    pub scanner: Arc<RwLock<Option<crate::types::ScanResult>>>,
    /// Signal-freeze state (manual button or synthetic-drawdown circuit breaker).
    pub freeze: Arc<RwLock<crate::types::FreezeState>>,
    /// Manual-validation journal (file-based DuckDB), behind a Mutex for the
    /// async handlers (each op runs on spawn_blocking).
    pub journal: Arc<std::sync::Mutex<duckdb::Connection>>,
}

/// Run the Axum server until the process exits.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws/live_signals", get(ws_handler))
        .route("/intraday", get(intraday_handler))
        .route("/api/symbols", get(symbols_handler))
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

/// `GET /api/scanner` — the Top-10 Buy / Sell scanner. The ~minute-long scan
/// runs once and is cached in `state.scanner`; subsequent requests serve the
/// cached result. Never holds a sync lock across an `.await`.
async fn scanner_handler(
    State(state): State<AppState>,
    Query(params): Query<SuggestParams>,
) -> Response {
    // Fast path: return the cached scan if present. Clone out of the guard so we
    // never hold the std RwLock across the response.
    if let Ok(guard) = state.scanner.read() {
        if let Some(cached) = guard.clone() {
            return Json(cached).into_response();
        }
    }

    let root = state.root.clone();
    let capital = params.capital.unwrap_or(100000.0);
    let risk = params.risk.unwrap_or(2.5) / 100.0;

    let scan: Result<ScanResult> = tokio::task::spawn_blocking(move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Ok(crate::suggestion_engine::scan_universe(&root, &symbols, capital, risk))
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("scanner task panicked: {e}")));

    match scan {
        Ok(result) => {
            // Store into the cache for subsequent requests.
            if let Ok(mut guard) = state.scanner.write() {
                *guard = Some(result.clone());
            }
            Json(result).into_response()
        }
        Err(e) => {
            warn!("scan_universe failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
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

    let result: Result<FinderResult> = tokio::task::spawn_blocking(move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Ok(crate::suggestion_engine::find_capital_fit(&root, &symbols, capital, risk))
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("finder task panicked: {e}")));

    match result {
        Ok(r) => Json(r).into_response(),
        Err(e) => {
            warn!("find_capital_fit failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
    }
}

/// `GET /api/regime` — NIFTY regime + market breadth (display-only context).
async fn regime_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let regime: Result<RegimeInfo> = tokio::task::spawn_blocking(move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        Ok(crate::suggestion_engine::compute_regime(&root, &symbols))
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("regime task panicked: {e}")));

    match regime {
        Ok(r) => Json(r).into_response(),
        Err(e) => {
            warn!("compute_regime failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
        }
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

    let resp = tokio::task::spawn_blocking(move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        let fit = crate::suggestion_engine::find_capital_fit(&root, &symbols, capital, risk_pct);
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
        StagingResp { top_long: longs, top_short: shorts, capital, risk_tier: tier.as_str().to_string() }
    })
    .await;

    match resp {
        Ok(r) => Json(r).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("staging task panicked: {e}")).into_response(),
    }
}

/// `GET /api/swing` — multi-day Swing Trades Catalog (DuckDB daily scan).
async fn swing_handler(State(state): State<AppState>) -> Response {
    let root = state.root.clone();
    let cat = tokio::task::spawn_blocking(move || {
        let symbols = crate::storage_kernel::discover_symbols(&root).unwrap_or_default();
        crate::swing_analyzer::scan_swing(&root, &symbols)
    })
    .await;
    match cat {
        Ok(c) => Json::<SwingCatalog>(c).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("swing task panicked: {e}")).into_response(),
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

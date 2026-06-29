//! RAM_ISTP — local intraday backtest + signal engine.
//!
//! Signals & alerts ONLY: this binary never places, modifies, or cancels live
//! orders. All position sizing and P&L figures are advisory.
//!
//! Subcommands (more land per phase):
//!   premarket [SYMBOL...]      pre-market historical scan: macro ATR / 52w S-R
//!                              / volume-profile VAH-VAL (full universe if none).
//!   backtest [TF] [SYMBOL...]  backtest the strategy library over history on
//!                              timeframe TF (default 5min), build + cache the
//!                              edge map, and print the top edges.

mod analytics_kernel;
mod cache;
mod capital_planner;
mod circuit_breaker;
mod config;
mod costs;
mod data_quality;
mod execution_staging;
mod holdings_analytics;
mod ingestion_engine;
mod journal_sync;
mod kite_instruments;
mod news_engine;
mod portfolio_analytics;
mod portfolio_import;
mod portfolio_rotation;
mod regime;
mod risk_manager;
mod server;
mod stats;
mod storage_kernel;
mod strategy_engine;
mod suggestion_engine;
mod swing_analyzer;
mod symbol_resolver;
mod tradability;
mod types;
mod validation;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use config::Timeframe;
use storage_kernel::SymbolBaseline;
use strategy_engine::EdgeRecord;

fn main() -> Result<()> {
    init_tracing();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "premarket" => run_premarket(&args[2..]),
        "backtest" => run_backtest(&args[2..]),
        "serve" => run_serve(&args[2..]),
        "live" => run_live_cmd(&args[2..]),
        "suggest" => run_suggest(&args[2..]),
        "instruments" => run_instruments(),
        "" | "help" | "-h" | "--help" => {
            println!("RAM_ISTP — local intraday backtest + signal engine (signals only).");
            println!("usage:");
            println!("  ram_istp premarket [SYMBOL ...]         pre-market historical scan");
            println!("  ram_istp backtest [TF] [SYMBOL ...]     backtest + build edge map");
            println!("  ram_istp serve [TF]                     run replay + dashboard server");
            println!("  ram_istp live [TF]                      LIVE Kite feed + dashboard (needs creds + market hours)");
            println!("  ram_istp suggest SYMBOL                 per-stock intraday suggestion");
            println!("  ram_istp instruments                    refresh NSE token map (pre-market)");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other:?}  (try `premarket`, `backtest`, `serve`, `suggest`)");
            std::process::exit(2);
        }
    }
}

/// Initialise tracing output (honours `RUST_LOG`, defaults to `info`). Without
/// this, every `tracing::info!/warn!` across the codebase — including the live
/// Kite WS connect/subscribe status — is silently dropped. Idempotent.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Current IST wall-clock as "YYYY-MM-DD HH:MM:SS".
fn now_ist_string() -> String {
    chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// If `cache` is stale and no refresh is already in flight, recompute it on a
/// detached thread (single-flight is enforced by the cache). `compute` returns
/// the new value and its IST build-stamp. Runs entirely off the request path —
/// used by the desk scheduler to keep the warm caches fresh during market hours.
fn maybe_refresh<T, F>(cache: &Arc<cache::Cached<T>>, compute: F)
where
    T: Clone + Send + Sync + 'static,
    F: FnOnce() -> (T, String) + Send + 'static,
{
    if cache.lookup().stale && cache.try_begin_refresh() {
        let cache = cache.clone();
        std::thread::spawn(move || {
            let (val, stamp) = compute();
            cache.store(val, stamp);
        });
    }
}

/// Parse a timeframe token (e.g. `5min`, `15min`, `1day`); `None` if not one.
fn parse_timeframe(s: &str) -> Option<Timeframe> {
    Some(match s {
        "minute" | "1min" => Timeframe::Minute,
        "3min" => Timeframe::Min3,
        "5min" => Timeframe::Min5,
        "10min" => Timeframe::Min10,
        "15min" => Timeframe::Min15,
        "30min" => Timeframe::Min30,
        "60min" => Timeframe::Min60,
        "1day" => Timeframe::Daily,
        _ => return None,
    })
}

fn run_premarket(requested: &[String]) -> Result<()> {
    let root = config::data_root();
    if !root.exists() {
        anyhow::bail!(
            "data root {} not found — run from the project directory, or set {}",
            root.display(),
            config::DATA_ROOT_ENV
        );
    }

    let symbols: Vec<String> = if requested.is_empty() {
        let all = storage_kernel::discover_symbols(&root)?;
        eprintln!("scanning full universe: {} symbols", all.len());
        all
    } else {
        eprintln!("scanning {} requested symbol(s)", requested.len());
        requested.to_vec()
    };

    let report = storage_kernel::premarket_scan(&root, &symbols);

    println!(
        "\npre-market scan: {} ok, {} failed in {:.2}s ({:.1} symbols/s)",
        report.baselines.len(),
        report.failures.len(),
        report.elapsed.as_secs_f64(),
        report.baselines.len() as f64 / report.elapsed.as_secs_f64().max(1e-6),
    );

    // Decide which rows to print: requested ones, else a representative sample.
    let show: Vec<&SymbolBaseline> = if requested.is_empty() {
        let sample = ["RELIANCE", "INFY", "TCS", "SBIN", "MARUTI"];
        report
            .baselines
            .iter()
            .filter(|b| sample.contains(&b.symbol.as_str()))
            .collect()
    } else {
        report.baselines.iter().collect()
    };

    if !show.is_empty() {
        println!(
            "\n{:<12} {:>10} {:>9} {:>10} {:>10} {:>10} {:>10} {:>10} {:>7} {:>6}",
            "SYMBOL", "close", "ATR14", "52wHi", "52wLo", "POC", "VAH", "VAL", "vpBars", "src"
        );
        println!("{}", "-".repeat(108));
        for b in show {
            println!(
                "{:<12} {:>10.2} {:>9.4} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>7} {:>6}",
                b.symbol,
                b.last_close,
                b.atr_long,
                b.hi_52w,
                b.lo_52w,
                b.poc,
                b.vah,
                b.val,
                b.vp_bars,
                b.macro_source,
            );
        }
    }

    if !report.failures.is_empty() {
        println!("\nfirst failures:");
        for (sym, err) in report.failures.iter().take(5) {
            println!("  {sym}: {err}");
        }
    }

    Ok(())
}

fn run_backtest(raw: &[String]) -> Result<()> {
    let root = config::data_root();
    if !root.exists() {
        anyhow::bail!(
            "data root {} not found — run from the project directory, or set {}",
            root.display(),
            config::DATA_ROOT_ENV
        );
    }

    // First arg may be a timeframe; the rest are symbols.
    let mut rest = raw;
    let tf = match rest.first().and_then(|s| parse_timeframe(s)) {
        Some(tf) => {
            rest = &rest[1..];
            tf
        }
        None => Timeframe::Min5,
    };

    let symbols: Vec<String> = if rest.is_empty() {
        let all = storage_kernel::discover_symbols(&root)?;
        eprintln!("backtesting {} ({} symbols) — net of {:.2}% round-trip cost",
            tf.dir(), all.len(), config::ROUND_TRIP_COST * 100.0);
        all
    } else {
        eprintln!("backtesting {} ({} requested symbols)", tf.dir(), rest.len());
        rest.to_vec()
    };

    let start = std::time::Instant::now();
    let (records, failures) = strategy_engine::backtest_universe(&root, &symbols, tf);
    let elapsed = start.elapsed();

    strategy_engine::save_edge_map(&records, tf)?;
    // Freshness sidecar (honesty-layer): report the gap against the *full* on-disk
    // universe, even when only a subset of symbols was requested.
    let universe_at_build = storage_kernel::discover_symbols(&root)
        .map(|v| v.len())
        .unwrap_or(symbols.len());
    strategy_engine::save_edge_map_meta(&records, tf, universe_at_build)?;
    let eligible: Vec<&EdgeRecord> = records.iter().filter(|r| r.eligible).collect();

    println!(
        "\nbacktest {}: {} records ({} eligible) from {} symbols, {} failed, in {:.2}s",
        tf.dir(),
        records.len(),
        eligible.len(),
        symbols.len(),
        failures.len(),
        elapsed.as_secs_f64(),
    );
    println!("edge map cached → {}", strategy_engine::edge_map_path(tf).display());

    print_top_edges(&eligible, config::Direction::Long, "TOP BUY EDGES");
    print_top_edges(&eligible, config::Direction::Short, "TOP SELL EDGES");

    if !failures.is_empty() {
        println!("\nfirst failures:");
        for (sym, err) in failures.iter().take(5) {
            println!("  {sym}: {err}");
        }
    }
    Ok(())
}

fn print_top_edges(eligible: &[&EdgeRecord], dir: config::Direction, title: &str) {
    let mut rows: Vec<&&EdgeRecord> = eligible.iter().filter(|r| r.direction == dir).collect();
    rows.sort_by(|a, b| b.metrics.expectancy.partial_cmp(&a.metrics.expectancy).unwrap());
    println!("\n{title} (ranked by expectancy, R per trade)");
    println!(
        "{:<12} {:<20} {:>5} {:>7} {:>6} {:>6} {:>7}",
        "SYMBOL", "STRATEGY", "n", "exp(R)", "win%", "PF", "maxDD"
    );
    println!("{}", "-".repeat(72));
    for r in rows.iter().take(config::TOP_N) {
        let m = &r.metrics;
        println!(
            "{:<12} {:<20} {:>5} {:>7.3} {:>5.1}% {:>6.2} {:>7.2}",
            r.symbol, r.strategy, m.n, m.expectancy, m.win_pct, m.profit_factor, m.max_dd
        );
    }
}

/// `serve [TF]` — load/build the edge map, replay the latest session as a tick
/// stream, rank Top-10 Buy/Sell each second, and serve the dashboard. This is
/// the integration point wiring ingestion → analytics → risk → server.
fn run_serve(raw: &[String]) -> Result<()> {
    serve_pipeline(raw, IngestionSource::Replay)
}

/// Where the tick stream for [`serve_pipeline`] comes from. The entire analytics
/// → risk → server pipeline is identical for both; only the ingestion source and
/// the packet `mode` tag differ.
enum IngestionSource {
    /// Offline replay of the latest session (deterministic; NOT wall-clock gated).
    Replay,
    /// Live Kite WebSocket. Credentials feed the connection URL only (never
    /// logged); the universe is the edge symbols mapped to integer tokens.
    Live {
        api_key: String,
        access_token: String,
        map: kite_instruments::InstrumentMap,
    },
}

fn serve_pipeline(raw: &[String], source: IngestionSource) -> Result<()> {
    let root = config::data_root();
    if !root.exists() {
        anyhow::bail!("data root {} not found", root.display());
    }
    let tf = raw.first().and_then(|s| parse_timeframe(s)).unwrap_or(Timeframe::Min30);
    let mode_str: &'static str = match &source {
        IngestionSource::Replay => "replay",
        IngestionSource::Live { .. } => "live",
    };

    // Edge map: load cache, or build it now.
    let records = match strategy_engine::load_edge_map(tf) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("no cached edge map for {} — building it now…", tf.dir());
            let syms = storage_kernel::discover_symbols(&root)?;
            let (r, _f) = strategy_engine::backtest_universe(&root, &syms, tf);
            strategy_engine::save_edge_map(&r, tf)?;
            strategy_engine::save_edge_map_meta(&r, tf, syms.len())?;
            r
        }
    };
    let edge_index = strategy_engine::build_index(&records);
    let eligible_edges: usize = edge_index.values().map(Vec::len).sum();
    // Shared, read-only copy for the holdings endpoint (cross-reference context).
    let edge_index_arc = Arc::new(edge_index.clone());
    let symbols: Vec<String> = {
        let mut s: Vec<String> = edge_index.keys().cloned().collect();
        s.sort();
        s
    };
    eprintln!(
        "serve @ {}: {} symbols with eligible edges ({} edges)",
        tf.dir(),
        symbols.len(),
        eligible_edges
    );

    // Baselines (ATR / VAH-VAL levels) for the live universe.
    let baselines: HashMap<String, SymbolBaseline> = storage_kernel::premarket_scan(&root, &symbols)
        .baselines
        .into_iter()
        .map(|b| (b.symbol.clone(), b))
        .collect();

    // Tick channel + shared state.
    let (tx, rx) = crossbeam_channel::unbounded::<types::Tick>();
    let stop = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(std::sync::RwLock::new(config::UserSettings::default()));
    let settings0 = *settings.read().unwrap();
    let packet = Arc::new(std::sync::RwLock::new(types::SignalPacket::empty(
        types::SettingsView { budget: settings0.budget, risk_pct: settings0.risk_pct },
        mode_str,
    )));
    let notify = Arc::new(tokio::sync::Notify::new());
    // Trading Desk: signal-freeze state + manual-validation journal (DuckDB file).
    std::fs::create_dir_all("data/journals").ok();
    let journal_conn = journal_sync::open_journal(std::path::Path::new(
        "data/journals/journal_2026.duckdb",
    ))?;
    let freeze = Arc::new(std::sync::RwLock::new(types::FreezeState::active(
        config::CAPITAL_POOL,
        config::DRAWDOWN_FREEZE_PCT,
    )));
    let journal_arc = Arc::new(std::sync::Mutex::new(journal_conn));

    // Warm, stale-while-revalidate caches for the heavy universe scans. TTLs are
    // chosen so a market-hours scheduler keeps them fresh without churning.
    use std::time::Duration;
    let scanner = Arc::new(cache::Cached::<types::ScanResult>::new(Duration::from_secs(180)));
    let regime = Arc::new(cache::Cached::<types::RegimeInfo>::new(Duration::from_secs(120)));
    let swing = Arc::new(cache::Cached::<types::SwingCatalog>::new(Duration::from_secs(300)));
    let finder = Arc::new(cache::KeyedCache::<types::FinderResult>::new(Duration::from_secs(180), 16));
    // Tradability is date-stable (daily turnover + series + caps) — a long TTL is fine.
    let tradability = Arc::new(cache::Cached::<tradability::TradabilityResult>::new(Duration::from_secs(3600)));

    let state = server::AppState {
        packet: packet.clone(),
        settings: settings.clone(),
        notify: notify.clone(),
        static_dir: std::path::PathBuf::from("ui"),
        root: root.clone(),
        scanner: scanner.clone(),
        regime: regime.clone(),
        swing: swing.clone(),
        finder: finder.clone(),
        freeze: freeze.clone(),
        journal: journal_arc.clone(),
        edge_index: edge_index_arc.clone(),
        edge_tf: tf,
        tradability: tradability.clone(),
    };

    // Startup precompute: warm all heavy caches in parallel so the first page
    // open is instant instead of paying a 30–60s universe scan on the request
    // path. Each thread claims the cache's single-flight slot, so a user request
    // arriving mid-warm waits for (rather than duplicates) the scan.
    {
        let (root, sc, rg, sw, fd, td) =
            (root.clone(), scanner.clone(), regime.clone(), swing.clone(), finder.clone(), tradability.clone());
        std::thread::spawn(move || {
            let warm: Vec<std::thread::JoinHandle<()>> = vec![
                {
                    let (root, sc) = (root.clone(), sc.clone());
                    std::thread::spawn(move || {
                        if sc.try_begin_refresh() {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = suggestion_engine::scan_universe(&root, &syms, 100_000.0, 0.025);
                            let stamp = r.built_ist.clone();
                            sc.store(r, stamp);
                            eprintln!("warm: scanner ready");
                        }
                    })
                },
                {
                    let (root, rg) = (root.clone(), rg.clone());
                    std::thread::spawn(move || {
                        if rg.try_begin_refresh() {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = suggestion_engine::compute_regime(&root, &syms);
                            let stamp = r.built_ist.clone();
                            rg.store(r, stamp);
                            eprintln!("warm: regime ready");
                        }
                    })
                },
                {
                    let (root, sw) = (root.clone(), sw.clone());
                    std::thread::spawn(move || {
                        if sw.try_begin_refresh() {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = swing_analyzer::scan_swing(&root, &syms);
                            let stamp = r.built_ist.clone();
                            sw.store(r, stamp);
                            eprintln!("warm: swing ready");
                        }
                    })
                },
                {
                    let (root, fd) = (root.clone(), fd.clone());
                    std::thread::spawn(move || {
                        // Default key = pool capital × Moderate tier; this also
                        // warms the desk's Moderate staging console.
                        let cap = config::CAPITAL_POOL;
                        let risk = config::RiskTier::Moderate.pct();
                        let slot = fd.slot(cache::CapRiskKey::new(cap, risk));
                        if slot.try_begin_refresh() {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = suggestion_engine::find_capital_fit(&root, &syms, cap, risk);
                            let stamp = r.built_ist.clone();
                            slot.store(r, stamp);
                            eprintln!("warm: finder (default key) ready");
                        }
                    })
                },
                {
                    let (root, td) = (root.clone(), td.clone());
                    std::thread::spawn(move || {
                        if td.try_begin_refresh() {
                            let stamp = chrono::Utc::now()
                                .with_timezone(&config::IST)
                                .format("%Y-%m-%d %H:%M:%S")
                                .to_string();
                            match storage_kernel::open_conn() {
                                Ok(conn) => {
                                    let r = tradability::build_index(
                                        &conn,
                                        &root,
                                        std::path::Path::new("cache"),
                                        stamp,
                                    );
                                    let s = r.built_ist.clone();
                                    td.store(r, s);
                                    eprintln!("warm: tradability ready");
                                }
                                Err(_) => td.abort_refresh(),
                            }
                        }
                    })
                },
            ];
            for h in warm {
                let _ = h.join();
            }
            eprintln!("warm: all caches ready");
        });
    }

    // Ingestion thread: replay the latest session, or stream the live Kite feed.
    let ing = match source {
        IngestionSource::Replay => {
            let (root, symbols, stop) = (root.clone(), symbols.clone(), stop.clone());
            std::thread::spawn(move || {
                let opts = ingestion_engine::ReplayOptions { tf, days_back: 1, speed: 0.0 };
                if let Err(e) = ingestion_engine::run_replay(&root, &symbols, &opts, tx, stop) {
                    eprintln!("replay ended: {e:#}");
                }
            })
        }
        IngestionSource::Live { api_key, access_token, map } => {
            // Subscribe only the edge-universe symbols we actually analyse, mapped
            // to integer tokens, capped at the live-universe limit.
            let cap = config::live_universe_max();
            let pairs = kite_instruments::select_universe(&map, &symbols, cap);
            eprintln!(
                "live: subscribing {} of {} edge symbols as instrument_tokens (cap {}) in Full mode",
                pairs.len(),
                symbols.len(),
                cap
            );
            if pairs.is_empty() {
                anyhow::bail!(
                    "live: none of the {} edge symbols resolved to an NSE instrument_token — \
                     run `ram_istp instruments` and check the map",
                    symbols.len()
                );
            }
            let cfg = ingestion_engine::LiveConfig { api_key, access_token, instruments: pairs };
            let stop = stop.clone();
            std::thread::spawn(move || {
                if let Err(e) = ingestion_engine::run_live_blocking(cfg, tx, stop) {
                    eprintln!("live ingestion ended: {e:#}");
                }
            })
        }
    };

    // Analytics + risk thread: fold ticks, emit a ranked packet every second.
    let ana = {
        let (baselines, edge_index, symbols) = (baselines, edge_index, symbols.clone());
        let (settings, packet, notify, stop) =
            (settings.clone(), packet.clone(), notify.clone(), stop.clone());
        let freeze = freeze.clone();
        std::thread::spawn(move || {
            let mut engine =
                analytics_kernel::Engine::new(&symbols, &baselines, &edge_index, eligible_edges);
            let limits = risk_manager::RiskLimits::default();
            let mut last_emit = std::time::Instant::now();
            let mut ticks: u64 = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(t) => {
                        engine.on_tick(&t);
                        ticks += 1;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // Replay finished: keep the last snapshot live for the UI,
                        // but sleep so we re-emit ~1/s instead of busy-spinning.
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                }
                if last_emit.elapsed() >= std::time::Duration::from_secs(1) {
                    let t0 = std::time::Instant::now();
                    let cands = engine.snapshot_candidates();
                    let set = *settings.read().unwrap();
                    let (mut top_buy, mut top_sell) = risk_manager::rank(&cands, &set, &limits);
                    let risk_meter = risk_manager::risk_meter(&top_buy, &top_sell, &set);
                    let mut diagnostics = engine.diagnostics();
                    diagnostics.tick_to_signal_us = t0.elapsed().as_micros() as u64;
                    diagnostics.ticks_per_sec = ticks;
                    ticks = 0;
                    let now = chrono::Utc::now().with_timezone(&config::IST);
                    let mut alerts: Vec<types::Alert> =
                        risk_manager::squareoff_alert(now).into_iter().collect();
                    // Signal Freeze: halt new suggestions + clear the queue, but keep
                    // background logging running. (Manual button or circuit breaker.)
                    let fz = freeze.read().map(|g| g.clone()).ok();
                    if let Some(f) = fz {
                        if f.frozen {
                            top_buy.clear();
                            top_sell.clear();
                            alerts.insert(
                                0,
                                types::Alert {
                                    kind: "freeze".to_string(),
                                    severity: "danger".to_string(),
                                    message: format!(
                                        "SIGNAL FREEZE active — {} (daily P&L ₹{:.0})",
                                        f.reason, f.daily_pnl
                                    ),
                                },
                            );
                        }
                    }
                    *packet.write().unwrap() = types::SignalPacket {
                        ts_ist: now_ist_string(),
                        mode: mode_str.to_string(),
                        settings: types::SettingsView { budget: set.budget, risk_pct: set.risk_pct },
                        top_buy,
                        top_sell,
                        risk_meter,
                        diagnostics,
                        alerts,
                    };
                    notify.notify_waiters();
                    last_emit = std::time::Instant::now();
                }
            }
        })
    };

    // Desk scheduler: circuit breaker + 15:45 CSV export + warm-cache refresh.
    let sched = {
        let (journal, freeze, stop) = (journal_arc.clone(), freeze.clone(), stop.clone());
        let (root, scanner, regime, swing, finder) =
            (root.clone(), scanner.clone(), regime.clone(), swing.clone(), finder.clone());
        std::thread::spawn(move || {
            let mut exported_date = String::new();
            let marks: HashMap<String, f64> = HashMap::new(); // no live marks in replay
            let mut tick: u64 = 0;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
                tick += 1;

                // Circuit breaker: evaluate synthetic MTM of accepted trades. Run
                // every ~15s (not every 3s) — that's plenty for a synthetic MTM and
                // cuts journal-lock pressure ~80% (writers are human-paced anyway).
                if tick % 5 == 0 {
                    if let Ok(conn) = journal.lock() {
                        if let Ok(entries) = journal_sync::all_entries(&conn) {
                            let fs = circuit_breaker::evaluate(
                                &entries,
                                config::CAPITAL_POOL,
                                config::DRAWDOWN_FREEZE_PCT,
                                &marks,
                            );
                            if let Ok(mut g) = freeze.write() {
                                g.daily_pnl = fs.daily_pnl;
                                // Auto-freeze on breach; never auto-clear a freeze
                                // (only the manual Unfreeze button clears it).
                                if fs.frozen && !g.frozen {
                                    g.frozen = true;
                                    g.reason = fs.reason;
                                }
                            }
                        }
                    }
                }

                // Warm-cache refresh: only during/around market hours, so we don't
                // burn cycles re-scanning 1500 symbols overnight. Each refresh is
                // single-flight and TTL-gated, so this 3s poll recomputes a cache
                // at most once per its TTL.
                let now = chrono::Utc::now().with_timezone(&config::IST);
                let t = now.time();
                if t >= config::premarket_start() && t <= config::session_close() {
                    {
                        let (root, syms_root) = (root.clone(), root.clone());
                        maybe_refresh(&scanner, move || {
                            let syms = storage_kernel::discover_symbols(&syms_root).unwrap_or_default();
                            let r = suggestion_engine::scan_universe(&root, &syms, 100_000.0, 0.025);
                            let s = r.built_ist.clone();
                            (r, s)
                        });
                    }
                    {
                        let root = root.clone();
                        maybe_refresh(&regime, move || {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = suggestion_engine::compute_regime(&root, &syms);
                            let s = r.built_ist.clone();
                            (r, s)
                        });
                    }
                    {
                        let root = root.clone();
                        maybe_refresh(&swing, move || {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = swing_analyzer::scan_swing(&root, &syms);
                            let s = r.built_ist.clone();
                            (r, s)
                        });
                    }
                    // Finder: refresh the two well-known keys (default page + the
                    // Moderate staging tier). Ad-hoc user keys just expire and
                    // recompute on demand.
                    for (cap, risk) in [
                        (100_000.0_f64, 0.025_f64),
                        (config::CAPITAL_POOL, config::RiskTier::Moderate.pct()),
                    ] {
                        let slot = finder.slot(cache::CapRiskKey::new(cap, risk));
                        let root = root.clone();
                        maybe_refresh(&slot, move || {
                            let syms = storage_kernel::discover_symbols(&root).unwrap_or_default();
                            let r = suggestion_engine::find_capital_fit(&root, &syms, cap, risk);
                            let s = r.built_ist.clone();
                            (r, s)
                        });
                    }
                }

                // 15:45 IST: export the journal to CSV once per day.
                let now = chrono::Utc::now().with_timezone(&config::IST);
                let today = now.format("%Y-%m-%d").to_string();
                let cutoff = chrono::NaiveTime::from_hms_opt(15, 45, 0).unwrap();
                if now.time() >= cutoff && exported_date != today {
                    if let Ok(conn) = journal.lock() {
                        match journal_sync::export_csv(
                            &conn,
                            std::path::Path::new("data/journals"),
                            &now_ist_string(),
                        ) {
                            Ok(p) => eprintln!("journal exported → {}", p.display()),
                            Err(e) => eprintln!("journal export failed: {e:#}"),
                        }
                    }
                    exported_date = today;
                }
            }
        })
    };

    // Server on the tokio runtime (blocks until exit).
    let rt = tokio::runtime::Runtime::new()?;
    let addr: std::net::SocketAddr = "127.0.0.1:8787".parse().unwrap();
    eprintln!("dashboard:  http://{addr}");
    let res = rt.block_on(async move { server::serve(addr, state).await });

    stop.store(true, Ordering::Relaxed);
    let _ = ing.join();
    let _ = ana.join();
    let _ = sched.join();
    res
}

/// `live [TF]` — the LIVE counterpart of `serve`: stream the authenticated Kite
/// WebSocket (by integer instrument_token, Full mode for L2 depth/OBI) into the
/// same analytics → risk → dashboard pipeline. Reads `KITE_API_KEY` /
/// `KITE_ACCESS_TOKEN` from the environment (.env); they are never logged.
///
/// Signals-only: this consumes market data and stages signals/alerts. It NEVER
/// places, modifies, or cancels an order. Outside 09:15–15:30 IST the socket
/// connects but the market-hours gate drops any ticks (no live signal off-session).
fn run_live_cmd(raw: &[String]) -> Result<()> {
    // Load .env (best-effort) then read creds. Never printed.
    dotenvy::dotenv().ok();
    let api_key = std::env::var("KITE_API_KEY").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let access_token = std::env::var("KITE_ACCESS_TOKEN").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let (Some(api_key), Some(access_token)) = (api_key, access_token) else {
        anyhow::bail!(
            "KITE_API_KEY / KITE_ACCESS_TOKEN not set — add them to .env (see .env.example). \
             Re-auth each morning; the access token expires ~6 AM IST."
        );
    };

    // Build (or load the day's cached) NSE instrument→token map for the universe.
    let ist_date = chrono::Utc::now().with_timezone(&config::IST).format("%Y-%m-%d").to_string();
    let map = {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(kite_instruments::load_or_refresh(std::path::Path::new("cache"), &ist_date))?
    };
    eprintln!("live: NSE instrument map ready ({} symbols, {ist_date})", map.len());

    // Friendly heads-up if the market is closed — the feed still connects.
    let now_ist = chrono::Utc::now().with_timezone(&config::IST);
    if !config::is_regular_session(now_ist.time()) {
        eprintln!(
            "live: NOTE — {} IST is outside the 09:15–15:30 session; the socket will connect but \
             no ticks/signals will flow until the market opens.",
            now_ist.format("%a %H:%M")
        );
    }

    serve_pipeline(raw, IngestionSource::Live { api_key, access_token, map })
}

/// `instruments` — daily pre-market job: refresh the NSE `tradingsymbol →
/// instrument_token` map from Kite's public dump and report the live universe.
/// Read-only / advisory: it fetches a PUBLIC csv (no auth, no secrets) and never
/// places an order. This is the mapping the live Full-mode WebSocket subscribes
/// by (tokens, never string tickers).
fn run_instruments() -> Result<()> {
    let root = config::data_root();
    let ist_date = chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d")
        .to_string();
    let cache_dir = std::path::Path::new("cache");

    let rt = tokio::runtime::Runtime::new()?;
    let map = rt.block_on(kite_instruments::load_or_refresh(cache_dir, &ist_date))?;

    let archive = storage_kernel::discover_symbols(&root).unwrap_or_default();
    let cap = config::live_universe_max();
    let universe = kite_instruments::select_universe(&map, &archive, cap);

    println!(
        "\nNSE instrument map ({ist_date}): {} cash-equity symbols mapped to integer tokens",
        map.len()
    );
    println!(
        "live universe: {} of {} archive symbols map to a token (cap {}) — subscribe these as instrument_tokens in Full mode",
        universe.len(),
        archive.len(),
        cap
    );
    println!("\nsample mappings (symbol → instrument_token):");
    for (sym, tok) in universe.iter().take(8) {
        println!("  {sym:<14} {tok}");
    }
    println!("\ncached → {}", kite_instruments::cache_path(cache_dir, &ist_date).display());
    Ok(())
}

/// `suggest SYMBOL` — print the per-stock intraday suggestion (CLI view of what
/// the `/intraday` page renders).
fn run_suggest(raw: &[String]) -> Result<()> {
    let root = config::data_root();
    if !root.exists() {
        anyhow::bail!("data root {} not found", root.display());
    }
    let symbol = match raw.first() {
        Some(s) => s.to_uppercase(),
        None => anyhow::bail!("usage: ram_istp suggest SYMBOL"),
    };
    // Page convention: ₹1,00,000 capital @ 2.5% risk per trade.
    let s = suggestion_engine::analyze_symbol(&root, &symbol, 100_000.0, 0.025)?;

    println!("\n⚡ Intraday Suggestion — {}", s.symbol);
    println!(
        "intervals: {} · {} trading days · last {} ({}d old) · {} configs tested",
        s.intervals_available.join(", "),
        s.trading_days,
        s.last_date,
        s.days_old,
        s.total_configs
    );
    if let Some(best) = &s.best_overall {
        println!("🏆 Best: {best}");
    }
    for b in &s.blocks {
        println!("\n{} {} — {}", b.emoji, b.name, b.verdict_text);
        if let Some(c) = &b.best {
            println!(
                "   {} {} {} · R:R {} · entry ₹{:.2} SL ₹{:.2} tgt ₹{:.2} · qty {}",
                c.side, c.symbol, c.interval, c.rr_label, c.entry, c.sl, c.target, c.quantity
            );
            println!(
                "   win {:.1}% · PF {:.2} · exp {:+.2}R · n={} · Sharpe {:.2} · Calmar {:.2} · MC P(profit) {:.0}% · DSR {:.0}%",
                c.win_rate, c.profit_factor, c.expectancy_r, c.n_trades, c.sharpe, c.calmar,
                c.mc_prob_profit, c.dsr * 100.0
            );
            // Honesty stats: slippage stress band + same-bar ambiguity share.
            let stress = if c.exp_3x_slip > 0.05 {
                "robust to 3× slip"
            } else if c.exp_3x_slip > 0.0 {
                "thin at 3× slip"
            } else {
                "negative under stress"
            };
            println!(
                "   slippage band: exp {:+.2}R (1×) → {:+.2}R (2×) → {:+.2}R (3×) — {} · ambiguous-bar exits {:.0}%",
                c.expectancy_r, c.exp_2x_slip, c.exp_3x_slip, stress, c.ambiguous_frac * 100.0
            );
            match c.confidence {
                Some(conf) => println!(
                    "   Confidence {}/100 ({}) · t={:.2} p≈{:.3} · Conviction {}/100 ({})",
                    conf, c.confidence_band, c.t_stat, c.p_value, c.conviction, c.conviction_label
                ),
                None => println!("   Confidence — {} · Conviction {}/100", c.confidence_band, c.conviction),
            }
        }
    }
    Ok(())
}

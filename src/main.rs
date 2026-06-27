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
mod circuit_breaker;
mod config;
mod costs;
mod execution_staging;
mod ingestion_engine;
mod journal_sync;
mod news_engine;
mod portfolio_analytics;
mod regime;
mod risk_manager;
mod server;
mod stats;
mod storage_kernel;
mod strategy_engine;
mod suggestion_engine;
mod swing_analyzer;
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
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "premarket" => run_premarket(&args[2..]),
        "backtest" => run_backtest(&args[2..]),
        "serve" => run_serve(&args[2..]),
        "suggest" => run_suggest(&args[2..]),
        "" | "help" | "-h" | "--help" => {
            println!("RAM_ISTP — local intraday backtest + signal engine (signals only).");
            println!("usage:");
            println!("  ram_istp premarket [SYMBOL ...]         pre-market historical scan");
            println!("  ram_istp backtest [TF] [SYMBOL ...]     backtest + build edge map");
            println!("  ram_istp serve [TF]                     run replay + dashboard server");
            println!("  ram_istp suggest SYMBOL                 per-stock intraday suggestion");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other:?}  (try `premarket`, `backtest`, `serve`, `suggest`)");
            std::process::exit(2);
        }
    }
}

/// Current IST wall-clock as "YYYY-MM-DD HH:MM:SS".
fn now_ist_string() -> String {
    chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
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
    let root = config::data_root();
    if !root.exists() {
        anyhow::bail!("data root {} not found", root.display());
    }
    let tf = raw.first().and_then(|s| parse_timeframe(s)).unwrap_or(Timeframe::Min30);

    // Edge map: load cache, or build it now.
    let records = match strategy_engine::load_edge_map(tf) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("no cached edge map for {} — building it now…", tf.dir());
            let syms = storage_kernel::discover_symbols(&root)?;
            let (r, _f) = strategy_engine::backtest_universe(&root, &syms, tf);
            strategy_engine::save_edge_map(&r, tf)?;
            r
        }
    };
    let edge_index = strategy_engine::build_index(&records);
    let eligible_edges: usize = edge_index.values().map(Vec::len).sum();
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
        "replay",
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
    let state = server::AppState {
        packet: packet.clone(),
        settings: settings.clone(),
        notify: notify.clone(),
        static_dir: std::path::PathBuf::from("ui"),
        root: root.clone(),
        scanner: Arc::new(std::sync::RwLock::new(None)),
        freeze: freeze.clone(),
        journal: Arc::new(std::sync::Mutex::new(journal_conn)),
    };

    // Ingestion thread: replay the latest session.
    let ing = {
        let (root, symbols, stop) = (root.clone(), symbols.clone(), stop.clone());
        std::thread::spawn(move || {
            let opts = ingestion_engine::ReplayOptions { tf, days_back: 1, speed: 0.0 };
            if let Err(e) = ingestion_engine::run_replay(&root, &symbols, &opts, tx, stop) {
                eprintln!("replay ended: {e:#}");
            }
        })
    };

    // Analytics + risk thread: fold ticks, emit a ranked packet every second.
    let ana = {
        let (baselines, edge_index, symbols) = (baselines, edge_index, symbols.clone());
        let (settings, packet, notify, stop) =
            (settings.clone(), packet.clone(), notify.clone(), stop.clone());
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
                    let (top_buy, top_sell) = risk_manager::rank(&cands, &set, &limits);
                    let risk_meter = risk_manager::risk_meter(&top_buy, &top_sell, &set);
                    let mut diagnostics = engine.diagnostics();
                    diagnostics.tick_to_signal_us = t0.elapsed().as_micros() as u64;
                    diagnostics.ticks_per_sec = ticks;
                    ticks = 0;
                    let now = chrono::Utc::now().with_timezone(&config::IST);
                    let alerts: Vec<types::Alert> =
                        risk_manager::squareoff_alert(now).into_iter().collect();
                    *packet.write().unwrap() = types::SignalPacket {
                        ts_ist: now_ist_string(),
                        mode: "replay".to_string(),
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

    // Server on the tokio runtime (blocks until exit).
    let rt = tokio::runtime::Runtime::new()?;
    let addr: std::net::SocketAddr = "127.0.0.1:8787".parse().unwrap();
    eprintln!("dashboard:  http://{addr}");
    let res = rt.block_on(async move { server::serve(addr, state).await });

    stop.store(true, Ordering::Relaxed);
    let _ = ing.join();
    let _ = ana.join();
    res
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

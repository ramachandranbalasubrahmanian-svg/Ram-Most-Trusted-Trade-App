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
mod config;
mod ingestion_engine;
mod news_engine;
mod risk_manager;
mod server;
mod storage_kernel;
mod strategy_engine;

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
        "" | "help" | "-h" | "--help" => {
            println!("RAM_ISTP — local intraday backtest + signal engine (signals only).");
            println!("usage:");
            println!("  ram_istp premarket [SYMBOL ...]         pre-market historical scan");
            println!("  ram_istp backtest [TF] [SYMBOL ...]     backtest + build edge map");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other:?}  (try `premarket` or `backtest`)");
            std::process::exit(2);
        }
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

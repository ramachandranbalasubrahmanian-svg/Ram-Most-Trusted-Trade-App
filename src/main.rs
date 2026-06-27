//! RAM_ISTP — local intraday backtest + signal engine.
//!
//! Signals & alerts ONLY: this binary never places, modifies, or cancels live
//! orders. All position sizing and P&L figures are advisory.
//!
//! Subcommands (more land per phase):
//!   premarket [SYMBOL...]   run the pre-market historical scan and print
//!                           baselines (macro ATR / 52w S-R / volume-profile
//!                           VAH-VAL). With no symbols, scans the full universe.

mod analytics_kernel;
mod config;
mod ingestion_engine;
mod news_engine;
mod risk_manager;
mod server;
mod storage_kernel;
mod strategy_engine;

use anyhow::Result;

use storage_kernel::SymbolBaseline;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    match cmd {
        "premarket" => run_premarket(&args[2..]),
        "" | "help" | "-h" | "--help" => {
            println!("RAM_ISTP — local intraday backtest + signal engine (signals only).");
            println!("usage:");
            println!("  ram_istp premarket [SYMBOL ...]   pre-market historical scan");
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other:?}  (try `premarket`)");
            std::process::exit(2);
        }
    }
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

//! Intraday Suggestion engine: the per-stock deep-dive + universe scanner.
//!
//! For a symbol it backtests the four page strategies (VWAP Trend, Opening
//! Range, Prev-Day Breakout, Gap-and-Go) across 6 intervals × 2 sides × 5 R:R
//! configs (~240 combos), holds out the last 30% as out-of-sample, computes the
//! full stat suite (via [`crate::stats`]), and picks the best per strategy. The
//! scanner runs the same across the universe and returns the Top-10 Buy / Sell
//! by Confidence.
//!
//! Honesty-first: every R / P&L is net of round-trip cost; verdicts state plainly
//! when there is no after-cost edge.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use std::path::Path;

use anyhow::Result;

use crate::config::{Timeframe, UserSettings};
use crate::types::{RegimeInfo, ScanResult, StockSuggestion};

/// Intervals scanned per stock (matches the page: 3m/5m/10m/15m/30m/1h).
pub const SUGGEST_INTERVALS: [Timeframe; 6] = [
    Timeframe::Min3,
    Timeframe::Min5,
    Timeframe::Min10,
    Timeframe::Min15,
    Timeframe::Min30,
    Timeframe::Min60,
];

/// The five R:R configurations as (sl_atr_mult, reward:risk).
pub const RR_CONFIGS: [(f64, f64); 5] = [
    (1.0, 1.5),
    (1.0, 2.0),
    (1.0, 3.0),
    (0.75, 2.0),
    (1.25, 2.0),
];

/// Round-trip cost used by the suggestion backtests (~0.16%, matching the
/// Python project's documented cost model).
pub const SUGGEST_COST: f64 = 0.0016;

/// Out-of-sample fraction held out from the tail of history.
pub const OOS_FRACTION: f64 = 0.30;

/// Per-symbol metadata for the picker / header.
#[derive(Debug, Clone, Default)]
pub struct SymbolMeta {
    pub intervals: Vec<String>,
    pub trading_days: usize,
    pub last_date: String,
    pub days_old: i64,
}

/// Which intervals a symbol has on disk, plus day/recency metadata.
pub fn symbol_meta(_root: &Path, _symbol: &str) -> SymbolMeta {
    SymbolMeta::default()
}

/// Full per-stock suggestion: 4 strategy blocks, each with its best setup.
pub fn analyze_symbol(
    _root: &Path,
    symbol: &str,
    _settings: &UserSettings,
) -> Result<StockSuggestion> {
    Ok(StockSuggestion {
        symbol: symbol.to_string(),
        intervals_available: Vec::new(),
        trading_days: 0,
        last_date: String::new(),
        days_old: 0,
        best_overall: None,
        blocks: Vec::new(),
        total_configs: 0,
        disclaimer: DISCLAIMER.to_string(),
    })
}

/// Scan the universe, returning the Top-10 Buy / Sell setups by Confidence.
pub fn scan_universe(_root: &Path, _symbols: &[String], _settings: &UserSettings) -> ScanResult {
    ScanResult {
        top_buy: Vec::new(),
        top_sell: Vec::new(),
        scanned: 0,
        built_ist: String::new(),
    }
}

/// NIFTY regime + market breadth (display-only context).
pub fn compute_regime(_root: &Path, _symbols: &[String]) -> RegimeInfo {
    RegimeInfo {
        nifty_regime: "Flat".to_string(),
        breadth_up: 0,
        breadth_down: 0,
        breadth_label: "neutral".to_string(),
    }
}

/// Standard disclaimer footer shown on the page.
pub const DISCLAIMER: &str = "Research output — not financial advice. Suggestions are derived from \
historical backtests on local data; past performance does not guarantee future results. Entry prices \
are approximate — actual entry requires a live signal after 09:30. Win rate, expectancy and profit \
factor are net of estimated slippage, brokerage and taxes (~0.16% round-trip); circuit breakers and \
corporate events are not modelled. All trading decisions and risk remain with the trader.";

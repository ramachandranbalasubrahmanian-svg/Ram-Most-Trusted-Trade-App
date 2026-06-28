//! Display-only tradability / liquidity / surveillance flags.
//!
//! FIREWALLED: this module imports only `config`, `storage_kernel`, and
//! `kite_instruments`. It NEVER feeds `eligible()`, Confidence, ranking, or
//! position sizing. Its sole job is to render a **non-blocking caption** next to
//! a signal — e.g. "BE — trade-to-trade; MIS likely rejected; verify" or
//! "thin liquidity (~₹0.4 Cr/day)". A caption, never a filter, gate, or order.
//! On a real-money board this is the single highest-consequence detail gap: the
//! engine can show a beautiful edge on a stock you literally cannot trade intraday.
//!
//! Honesty: every flag is computed from data actually on disk. ASM/GSM
//! surveillance lists are NOT present locally, so that field is reported as
//! "not loaded" — never guessed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use duckdb::Connection;
use serde::{Deserialize, Serialize};

use crate::storage_kernel;

// --- thresholds: display heuristics (documented), NOT gates -----------------
const TURNOVER_WINDOW: usize = 60; // trading days for the liquidity estimate
const CR: f64 = 1e7; // ₹1 crore
const DEEP_CR: f64 = 50.0; // ≥ ₹50 Cr/day median turnover
const OK_CR: f64 = 5.0;
const THIN_CR: f64 = 1.0; // < ₹1 Cr/day ⇒ "very thin"
const LOW_PRICE: f64 = 20.0; // < ₹20 ⇒ wider effective spread / penny risk
const MICRO_CAP_CR: f64 = 500.0; // < ₹500 Cr ⇒ micro-cap

/// NSE equity series suffixes (encoded in the Kite tradingsymbol, e.g.
/// `SVLL-BE`). These three are the trade-to-trade / restricted families where
/// MIS/intraday is typically disallowed (delivery only). Anything else (incl.
/// plain `RELIANCE`, or `BAJAJ-AUTO` whose `-AUTO` is part of the name) is EQ.
const T2T_SERIES: &[&str] = &["BE", "BZ", "BL"];

/// One symbol's tradability picture. All fields display-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tradability {
    pub symbol: String,
    /// "EQ" | "BE" | "BZ" | "BL" | "unknown".
    pub series: String,
    /// Series is a trade-to-trade family ⇒ intraday MIS usually rejected.
    pub trade_to_trade: bool,
    pub last_close: f64,
    /// Median daily ₹ turnover (close × volume) over the recent window.
    pub median_turnover_inr: f64,
    pub turnover_days: usize,
    /// "deep" | "ok" | "thin" | "very thin" | "unknown".
    pub liquidity: String,
    pub low_priced: bool,
    pub market_cap_inr: Option<f64>,
    pub micro_cap: bool,
    /// Surveillance status. Always "not loaded" today — never fabricated.
    pub asm_gsm: String,
    /// Caption pieces (warnings only). Empty ⇒ nothing to flag.
    pub flags: Vec<String>,
    /// `flags` joined with " · "; empty when clean.
    pub caption: String,
    /// True when there are no warnings (clean to trade — still verify live).
    pub ok: bool,
}

/// Coverage of a tradability build — what is known vs. not, for honest UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradabilityCoverage {
    pub total: usize,
    pub with_turnover: usize,
    pub series_known: usize,
    pub trade_to_trade: usize,
    pub thin_or_worse: usize,
    /// Honest note on the data we do NOT have.
    pub asm_gsm: String,
}

/// The full tradability index, cached like the other heavy scans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradabilityResult {
    pub built_ist: String,
    pub by_symbol: HashMap<String, Tradability>,
    pub coverage: TradabilityCoverage,
}

/// Liquidity tier from median ₹ turnover. Pure.
fn liquidity_tier(median_turnover_inr: f64, days: usize) -> &'static str {
    if days == 0 || !(median_turnover_inr > 0.0) {
        return "unknown";
    }
    let cr = median_turnover_inr / CR;
    if cr >= DEEP_CR {
        "deep"
    } else if cr >= OK_CR {
        "ok"
    } else if cr >= THIN_CR {
        "thin"
    } else {
        "very thin"
    }
}

/// Assemble one symbol's `Tradability` from its raw inputs. Pure — unit-tested.
pub fn assess(
    symbol: &str,
    series: &str,
    last_close: f64,
    median_turnover_inr: f64,
    turnover_days: usize,
    market_cap_inr: Option<f64>,
) -> Tradability {
    let trade_to_trade = T2T_SERIES.contains(&series);
    let liquidity = liquidity_tier(median_turnover_inr, turnover_days).to_string();
    let low_priced = last_close > 0.0 && last_close < LOW_PRICE;
    let micro_cap = market_cap_inr.map(|m| m > 0.0 && m < MICRO_CAP_CR * CR).unwrap_or(false);

    let mut flags: Vec<String> = Vec::new();
    if trade_to_trade {
        flags.push(format!(
            "{series} series — trade-to-trade; MIS/intraday usually rejected (delivery only). Verify."
        ));
    }
    match liquidity.as_str() {
        "thin" | "very thin" => flags.push(format!(
            "{} liquidity (~₹{:.1} Cr/day median); slippage / partial-fill risk.",
            liquidity,
            median_turnover_inr / CR
        )),
        _ => {}
    }
    if low_priced {
        flags.push(format!("low-priced (₹{last_close:.1}); wider effective spread."));
    }
    if micro_cap {
        if let Some(m) = market_cap_inr {
            flags.push(format!("micro-cap (₹{:.0} Cr); higher volatility / surveillance risk.", m / CR));
        }
    }

    let caption = flags.join(" · ");
    let ok = flags.is_empty();
    Tradability {
        symbol: symbol.to_string(),
        series: series.to_string(),
        trade_to_trade,
        last_close,
        median_turnover_inr,
        turnover_days,
        liquidity,
        low_priced,
        market_cap_inr,
        micro_cap,
        asm_gsm: "not loaded".to_string(),
        flags,
        caption,
        ok,
    }
}

/// Find the most recent `cache/nse_instruments_YYYY-MM-DD.json` (dates sort
/// lexicographically), if any.
fn latest_instruments_cache(cache_dir: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for entry in std::fs::read_dir(cache_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("nse_instruments_") && name.ends_with(".json") {
            let p = entry.path();
            if best.as_ref().map(|b| p > *b).unwrap_or(true) {
                best = Some(p);
            }
        }
    }
    best
}

/// Build a `base/full symbol -> series` map from the cached instruments dump.
/// Best-effort: returns empty when the dump is unavailable (series falls back to
/// "unknown" and only the price/liquidity flags apply).
pub fn load_series_map(cache_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(path) = latest_instruments_cache(cache_dir) else {
        return out;
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return out;
    };
    let Ok(map) = serde_json::from_slice::<crate::kite_instruments::InstrumentMap>(&bytes) else {
        return out;
    };
    for ts in map.by_symbol.keys() {
        if let Some((base, suf)) = ts.rsplit_once('-') {
            if T2T_SERIES.contains(&suf) {
                out.insert(base.to_string(), suf.to_string());
                out.insert(ts.clone(), suf.to_string());
                continue;
            }
        }
        out.entry(ts.clone()).or_insert_with(|| "EQ".to_string());
    }
    out
}

/// Per-symbol market cap (₹) from `symbol_metadata.parquet`. Best-effort.
fn load_market_caps(conn: &Connection, root: &Path) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    let path = root.join("symbol_metadata.parquet");
    if !path.exists() {
        return out;
    }
    let sql = format!(
        "SELECT symbol, market_cap_inr FROM read_parquet({}) WHERE market_cap_inr > 0",
        storage_kernel::quote_path(&path)
    );
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
            for (s, m) in rows.flatten() {
                out.insert(s, m);
            }
        }
    }
    out
}

/// Build the tradability index over every symbol in `nse_daily_all.parquet`.
/// One full scan (cached by the caller); aggregation + the recent-window median
/// are done in Rust (mirrors `capital_planner`, which found a 5M-row DuckDB
/// ORDER BY too slow).
pub fn build_index(conn: &Connection, root: &Path, cache_dir: &Path, built_ist: String) -> TradabilityResult {
    let series_map = load_series_map(cache_dir);
    let caps = load_market_caps(conn, root);

    // (symbol -> [(date_int, close, turnover)]) read unordered.
    let mut raw: HashMap<String, Vec<(i32, f64, f64)>> = HashMap::new();
    let path = root.join("nse_daily_all.parquet");
    if path.exists() {
        let sql = format!(
            "SELECT symbol, (CAST(date AS DATE) - DATE '2000-01-01') AS d, close AS c, \
             close*volume AS turn FROM read_parquet({}) WHERE close > 0",
            storage_kernel::quote_path(&path)
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?))
            }) {
                for (sym, d, c, turn) in rows.flatten() {
                    raw.entry(sym).or_default().push((
                        d,
                        c,
                        if turn.is_finite() && turn > 0.0 { turn } else { 0.0 },
                    ));
                }
            }
        }
    }

    let mut by_symbol: HashMap<String, Tradability> = HashMap::with_capacity(raw.len());
    for (sym, mut rows) in raw {
        rows.sort_by_key(|r| r.0); // ascending by date
        let last_close = rows.last().map(|r| r.1).unwrap_or(0.0);
        // Recent window for the liquidity median.
        let start = rows.len().saturating_sub(TURNOVER_WINDOW);
        let mut turns: Vec<f64> = rows[start..].iter().map(|r| r.2).collect();
        let turnover_days = turns.len();
        let median_turnover = median(&mut turns);
        let series = series_map.get(&sym).cloned().unwrap_or_else(|| "unknown".to_string());
        let cap = caps.get(&sym).copied();
        by_symbol.insert(sym.clone(), assess(&sym, &series, last_close, median_turnover, turnover_days, cap));
    }

    let total = by_symbol.len();
    let with_turnover = by_symbol.values().filter(|t| t.turnover_days > 0).count();
    let series_known = by_symbol.values().filter(|t| t.series != "unknown").count();
    let trade_to_trade = by_symbol.values().filter(|t| t.trade_to_trade).count();
    let thin_or_worse = by_symbol
        .values()
        .filter(|t| t.liquidity == "thin" || t.liquidity == "very thin")
        .count();

    TradabilityResult {
        built_ist,
        by_symbol,
        coverage: TradabilityCoverage {
            total,
            with_turnover,
            series_known,
            trade_to_trade,
            thin_or_worse,
            asm_gsm: "not loaded — verify ASM/GSM surveillance status on NSE before trading".to_string(),
        },
    }
}

/// Median of a slice (mutates: sorts in place). 0.0 for empty.
fn median(xs: &mut [f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquidity_tiers_classify_by_crore_turnover() {
        assert_eq!(liquidity_tier(0.0, 0), "unknown");
        assert_eq!(liquidity_tier(0.0, 10), "unknown");
        assert_eq!(liquidity_tier(60.0 * CR, 60), "deep");
        assert_eq!(liquidity_tier(10.0 * CR, 60), "ok");
        assert_eq!(liquidity_tier(2.0 * CR, 60), "thin");
        assert_eq!(liquidity_tier(0.4 * CR, 60), "very thin");
    }

    #[test]
    fn clean_liquid_eq_stock_has_no_flags() {
        let t = assess("RELIANCE", "EQ", 1400.0, 200.0 * CR, 60, Some(17_000_000.0 * CR));
        assert!(t.ok, "a deep-liquidity EQ large-cap should be clean");
        assert!(t.caption.is_empty());
        assert!(!t.trade_to_trade);
        assert_eq!(t.liquidity, "deep");
    }

    #[test]
    fn trade_to_trade_series_is_flagged_but_never_gated() {
        let t = assess("SVLL", "BE", 120.0, 0.3 * CR, 60, Some(300.0 * CR));
        assert!(t.trade_to_trade);
        assert!(!t.ok);
        assert!(t.caption.contains("trade-to-trade"));
        // T2T + very-thin + micro-cap should all surface as separate warnings.
        assert!(t.flags.len() >= 3, "expected multiple warnings, got {:?}", t.flags);
        assert!(t.caption.contains("very thin"));
        assert!(t.caption.contains("micro-cap"));
        // It remains a CAPTION — there is no boolean that prevents trading.
        assert_eq!(t.asm_gsm, "not loaded");
    }

    #[test]
    fn low_priced_penny_stock_flagged() {
        let t = assess("PENNY", "EQ", 8.5, 3.0 * CR, 60, Some(2000.0 * CR));
        assert!(t.low_priced);
        assert!(t.caption.contains("low-priced"));
        assert!(!t.micro_cap, "₹2000 Cr is not micro-cap");
    }

    #[test]
    fn unknown_series_and_missing_cap_degrade_gracefully() {
        let t = assess("NEWBIE", "unknown", 500.0, 80.0 * CR, 60, None);
        assert_eq!(t.series, "unknown");
        assert!(t.market_cap_inr.is_none());
        assert!(!t.micro_cap);
        assert!(t.ok, "deep liquidity + healthy price ⇒ no warnings even if series unknown");
    }
}

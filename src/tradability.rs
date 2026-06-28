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
    /// Surveillance status for this symbol: the measure from a loaded list
    /// (e.g. "ASM-ST", "GSM"), or "not loaded" when no list is present (never
    /// fabricated).
    pub asm_gsm: String,
    /// Surveillance measure from the loaded list, if any (drives `blocked`).
    pub surveillance: Option<String>,
    /// Overall intraday verdict: "blocked" | "high_risk" | "caution" | "ok".
    pub verdict: String,
    /// One-line reason for the verdict — the stay-away / caution message.
    pub reason: String,
    /// FALSE ⇒ do NOT take this intraday (MIS likely rejected — T2T/surveillance).
    pub intraday_ok: bool,
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
    /// Symbols with verdict == "blocked" (T2T or a loaded surveillance measure).
    pub blocked: usize,
    /// Whether an optional surveillance list was found + loaded.
    pub surveillance_loaded: bool,
    /// Number of names in the loaded surveillance list (0 if none).
    pub surveillance_count: usize,
    pub thin_or_worse: usize,
    /// Honest note on surveillance coverage.
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
/// `surveillance` is the measure (e.g. "GSM") from a loaded surveillance list, or
/// `None` when no list is present (then ASM/GSM is reported as unverified — never
/// assumed clean).
pub fn assess(
    symbol: &str,
    series: &str,
    last_close: f64,
    median_turnover_inr: f64,
    turnover_days: usize,
    market_cap_inr: Option<f64>,
    surveillance: Option<String>,
) -> Tradability {
    let trade_to_trade = T2T_SERIES.contains(&series);
    let liquidity = liquidity_tier(median_turnover_inr, turnover_days).to_string();
    let low_priced = last_close > 0.0 && last_close < LOW_PRICE;
    let micro_cap = market_cap_inr.map(|m| m > 0.0 && m < MICRO_CAP_CR * CR).unwrap_or(false);

    // ---- intraday verdict (the surveillance / tradability gate) ----
    // Precedence: a broker REJECTION (T2T or a surveillance measure) is a hard
    // "stay away"; then liquidity you can't exit; then softer cautions.
    let (verdict, reason, intraday_ok): (&str, String, bool) = if let Some(m) = surveillance.as_deref() {
        (
            "blocked",
            format!("⛔ Under {m} surveillance — intraday/MIS is typically blocked (100% margin / price bands). Stay away even if the edge looks good."),
            false,
        )
    } else if trade_to_trade {
        (
            "blocked",
            format!("⛔ {series} series (trade-to-trade) — MIS/intraday orders are rejected; delivery only. Stay away for intraday."),
            false,
        )
    } else if liquidity == "very thin" {
        (
            "high_risk",
            format!("⚠ Very thin liquidity (~₹{:.1} Cr/day) — you may be unable to exit intraday. High risk.", median_turnover_inr / CR),
            true,
        )
    } else if liquidity == "thin" || low_priced || micro_cap {
        let mut bits: Vec<&str> = Vec::new();
        if liquidity == "thin" { bits.push("thin liquidity"); }
        if low_priced { bits.push("low-priced"); }
        if micro_cap { bits.push("micro-cap"); }
        (
            "caution",
            format!("⚠ {} — elevated intraday risk; size down and use limit orders.", bits.join(" / ")),
            true,
        )
    } else {
        (
            "ok",
            "No local intraday blocker (EQ series, adequate liquidity). ASM/GSM not loaded — verify on NSE.".to_string(),
            true,
        )
    };

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

    // Surface the surveillance measure as a leading flag too (so badges/captions
    // reflect it), then assemble.
    if let Some(m) = surveillance.as_deref() {
        flags.insert(0, format!("under {m} surveillance — intraday likely blocked."));
    }
    let caption = flags.join(" · ");
    let ok = flags.is_empty();
    let asm_gsm = match surveillance.as_deref() {
        Some(m) => m.to_string(),
        None => "not loaded".to_string(),
    };
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
        asm_gsm,
        surveillance,
        verdict: verdict.to_string(),
        reason,
        intraday_ok,
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

/// Load an optional surveillance list so ASM/GSM can be ENFORCED when the user
/// drops it in. We never fetch or fabricate it — if the file is absent, ASM/GSM
/// is reported as unverified. Format: a CSV with a `symbol` column and a measure
/// column (`measure`/`asm`/`gsm`/`category`); header auto-detected. Looked up at
/// `<data_root>/surveillance.csv` then `cache/surveillance.csv`. Returns
/// (symbol_uppercase -> measure).
pub fn load_surveillance(root: &Path, cache_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let candidates = [root.join("surveillance.csv"), cache_dir.join("surveillance.csv")];
    let Some(path) = candidates.iter().find(|p| p.exists()) else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    let mut lines = text.lines();
    // Header: find the symbol + measure column indices.
    let Some(header) = lines.next() else { return out };
    let cols: Vec<String> = header.split(',').map(|c| c.trim().to_lowercase()).collect();
    let sym_idx = cols.iter().position(|c| c == "symbol" || c == "tradingsymbol" || c == "symbol_name");
    let meas_idx = cols
        .iter()
        .position(|c| c == "measure" || c == "category" || c == "asm" || c == "gsm" || c == "stage" || c == "type");
    // If no recognizable header, treat the whole file as `symbol,measure` rows
    // (and re-include the first line as data).
    let (sym_idx, meas_idx, include_first) = match (sym_idx, meas_idx) {
        (Some(s), Some(m)) => (s, m, false),
        _ => (0usize, 1usize, true),
    };
    let mut ingest = |line: &str| {
        let parts: Vec<&str> = line.split(',').map(|p| p.trim()).collect();
        if let Some(sym) = parts.get(sym_idx) {
            if !sym.is_empty() {
                let measure = parts
                    .get(meas_idx)
                    .filter(|m| !m.is_empty())
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "surveillance".to_string());
                out.insert(sym.to_uppercase(), measure);
            }
        }
    };
    if include_first {
        ingest(header);
    }
    for line in lines {
        if !line.trim().is_empty() {
            ingest(line);
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
    let surveillance = load_surveillance(root, cache_dir);
    let surveillance_loaded = !surveillance.is_empty();

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
        let surv = surveillance.get(&sym.to_uppercase()).cloned();
        by_symbol.insert(
            sym.clone(),
            assess(&sym, &series, last_close, median_turnover, turnover_days, cap, surv),
        );
    }

    let total = by_symbol.len();
    let with_turnover = by_symbol.values().filter(|t| t.turnover_days > 0).count();
    let series_known = by_symbol.values().filter(|t| t.series != "unknown").count();
    let trade_to_trade = by_symbol.values().filter(|t| t.trade_to_trade).count();
    let thin_or_worse = by_symbol
        .values()
        .filter(|t| t.liquidity == "thin" || t.liquidity == "very thin")
        .count();
    let blocked = by_symbol.values().filter(|t| t.verdict == "blocked").count();

    TradabilityResult {
        built_ist,
        by_symbol,
        coverage: TradabilityCoverage {
            total,
            with_turnover,
            series_known,
            trade_to_trade,
            blocked,
            surveillance_loaded,
            surveillance_count: surveillance.len(),
            thin_or_worse,
            asm_gsm: if surveillance_loaded {
                format!("surveillance list loaded ({} names)", surveillance.len())
            } else {
                "ASM/GSM list NOT loaded — verify surveillance status on NSE; drop a surveillance.csv (symbol,measure) in the data root to enforce it".to_string()
            },
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
    fn clean_liquid_eq_stock_is_ok_for_intraday() {
        let t = assess("RELIANCE", "EQ", 1400.0, 200.0 * CR, 60, Some(17_000_000.0 * CR), None);
        assert!(t.ok, "a deep-liquidity EQ large-cap should be clean");
        assert!(t.caption.is_empty());
        assert!(!t.trade_to_trade);
        assert_eq!(t.liquidity, "deep");
        assert_eq!(t.verdict, "ok");
        assert!(t.intraday_ok);
        // Honest: we did NOT verify ASM/GSM, so the reason must say so.
        assert!(t.reason.contains("ASM/GSM not loaded"));
        assert_eq!(t.asm_gsm, "not loaded");
    }

    #[test]
    fn trade_to_trade_series_is_blocked_for_intraday() {
        let t = assess("SVLL", "BE", 120.0, 0.3 * CR, 60, Some(300.0 * CR), None);
        assert!(t.trade_to_trade);
        assert_eq!(t.verdict, "blocked", "T2T must block intraday");
        assert!(!t.intraday_ok, "T2T → MIS rejected → not intraday-tradeable");
        assert!(t.reason.contains("trade-to-trade"));
        assert!(t.reason.contains("Stay away"));
    }

    #[test]
    fn loaded_surveillance_blocks_even_a_clean_eq_stock() {
        // An otherwise-pristine deep-liquidity EQ large-cap, but it's on the
        // loaded surveillance list → must be blocked with a stay-away message.
        let t = assess("BIGCAP", "EQ", 1500.0, 300.0 * CR, 60, Some(50_000.0 * CR), Some("GSM".to_string()));
        assert_eq!(t.verdict, "blocked", "a loaded surveillance measure blocks intraday");
        assert!(!t.intraday_ok);
        assert!(t.reason.contains("GSM"));
        assert!(t.reason.contains("Stay away"));
        assert_eq!(t.surveillance.as_deref(), Some("GSM"));
        assert_eq!(t.asm_gsm, "GSM");
    }

    #[test]
    fn very_thin_liquidity_is_high_risk() {
        let t = assess("THINCO", "EQ", 200.0, 0.4 * CR, 60, Some(800.0 * CR), None);
        assert_eq!(t.verdict, "high_risk");
        assert!(t.intraday_ok, "high_risk is tradeable-but-risky, not a hard block");
        assert!(t.reason.contains("exit intraday"));
    }

    #[test]
    fn low_priced_penny_stock_is_caution() {
        let t = assess("PENNY", "EQ", 8.5, 3.0 * CR, 60, Some(2000.0 * CR), None);
        assert!(t.low_priced);
        assert_eq!(t.verdict, "caution");
        assert!(t.intraday_ok);
        assert!(t.reason.contains("low-priced"));
        assert!(!t.micro_cap, "₹2000 Cr is not micro-cap");
    }

    #[test]
    fn unknown_series_and_missing_cap_degrade_gracefully() {
        let t = assess("NEWBIE", "unknown", 500.0, 80.0 * CR, 60, None, None);
        assert_eq!(t.series, "unknown");
        assert!(t.market_cap_inr.is_none());
        assert!(!t.micro_cap);
        assert_eq!(t.verdict, "ok", "deep liquidity + healthy price ⇒ ok even if series unknown");
        assert!(t.intraday_ok);
    }
}

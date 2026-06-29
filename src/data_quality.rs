//! Display-only data-quality guard for a symbol's candle series.
//!
//! FIREWALLED: imports only `config` + `storage_kernel` (+ serde/duckdb). It
//! NEVER feeds `eligible()`, Confidence, ranking, or position sizing. Its sole job
//! is to render a **non-blocking caption / banner** explaining why a name's
//! backtest numbers may be junk — it is a transparency layer, not a gate.
//!
//! Motivation (the 2026-06-28 full-rebuild finding): a handful of symbols
//! (PRIVISCL, KAMOPAINTS, …) produce non-finite / absurd backtest metrics because
//! their RAW intraday series carry bad data — zero / near-zero closes (₹0.00,
//! ₹0.05 placeholder ticks) and the resulting astronomic single-day jumps
//! (378×, 3689×), or an uncorrected corporate-action discontinuity. The
//! eligibility gate already REJECTS these (they never reach Top-10), so this guard
//! does not need to filter — it makes the rejection *visible and explained* on the
//! per-stock deep-dive, where a user can type ANY symbol.
//!
//! Honesty: it flags only what is actually wrong in the data on disk. A clean
//! series (e.g. CUPID, whose splits ARE adjusted and whose worst day-jump is 1.25×)
//! is reported "ok" — we never hard-code a name list or fabricate a problem.

use serde::{Deserialize, Serialize};

use crate::config::Timeframe;
use crate::storage_kernel;

// --- thresholds: documented display heuristics, NOT gates --------------------
/// A single-day close-to-close ratio (symmetric: max(r, 1/r)) at or above this is
/// essentially never a real overnight move for an NSE equity — it is a bad tick or
/// an uncorrected split/bonus. (KAMOPAINTS 378×, PRIVISCL 3689×; CUPID is 1.25×.)
const JUMP_UNRELIABLE: f64 = 3.0;
/// A day-jump in this band is rare but possible on extreme news — flag to verify.
const JUMP_CAUTION: f64 = 1.8;
/// Closes below ₹1 in this large-cap-leaning universe are placeholder/bad ticks.
const LOW_PRICE_FLOOR: f64 = 1.0;
/// How close an observed ex-date drop must be to 1/ratio to call a split "uncorrected".
const SPLIT_TOL: f64 = 0.25;
/// Only cross-check splits at least this large (a 1.2:1 leaves no detectable jump).
const SPLIT_MIN_RATIO: f64 = 1.5;
/// How many recent corporate actions to surface as context tags.
const RECENT_ACTIONS: usize = 3;

/// A corporate-action discontinuity the raw series never corrected for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UncorrectedSplit {
    pub date: String,
    /// The split/bonus ratio (e.g. 5.0 for 5:1).
    pub ratio: f64,
    /// The observed close ratio across the ex-date (≈ 1/ratio when uncorrected).
    pub observed_ratio: f64,
}

/// A recent corporate action, surfaced as a small context tag (display-only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorpActionTag {
    pub date: String,
    /// "split" | "dividend".
    pub kind: String,
    pub value: f64,
}

/// One symbol's data-quality picture at a resolution. All fields display-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DataQualityReport {
    pub symbol: String,
    pub timeframe: String,
    pub bars: usize,
    pub daily_bars: usize,
    /// Bars whose O/H/L/C is non-positive or non-finite (bad ticks).
    pub invalid_price_bars: usize,
    pub min_close: f64,
    /// Worst single-day close ratio, symmetric (1.0 = no jump found).
    pub max_jump: f64,
    pub max_jump_at: Option<String>,
    pub uncorrected_split: Option<UncorrectedSplit>,
    pub recent_actions: Vec<CorpActionTag>,
    /// "ok" | "caution" | "unreliable".
    pub verdict: String,
    /// One-line honest summary.
    pub reason: String,
    /// Warning pieces (empty ⇒ clean).
    pub flags: Vec<String>,
    /// True ⇒ no data-quality concern found (clean series).
    pub ok: bool,
}

/// Pure core: assess a daily (date, close) series + invalid-bar count + the
/// symbol's splits. No I/O — unit-tested directly. Returns
/// (max_jump, max_jump_at, uncorrected_split, verdict, reason, flags).
#[allow(clippy::type_complexity)]
fn assess(
    daily: &[(String, f64)],
    invalid_price_bars: usize,
    min_close: f64,
    splits: &[(String, f64)],
) -> (f64, Option<String>, Option<UncorrectedSplit>, String, String, Vec<String>) {
    // 1) worst day-over-day discontinuity, over positive consecutive closes.
    let mut max_jump = 1.0_f64;
    let mut max_jump_at: Option<String> = None;
    for w in daily.windows(2) {
        let (prev, cur) = (w[0].1, w[1].1);
        if prev > 0.0 && cur > 0.0 && prev.is_finite() && cur.is_finite() {
            let r = cur / prev;
            let j = r.max(1.0 / r);
            if j > max_jump {
                max_jump = j;
                max_jump_at = Some(w[1].0.clone());
            }
        }
    }

    // 2) uncorrected split: at each large split's ex-date, did the raw series drop
    //    by ~the split ratio (≈ 1/ratio) instead of staying continuous (≈ 1.0)?
    let mut uncorrected_split: Option<UncorrectedSplit> = None;
    for (ex_date, ratio) in splits {
        if *ratio < SPLIT_MIN_RATIO || !ratio.is_finite() {
            continue;
        }
        // First daily bar on/after the ex-date.
        if let Some(idx) = daily.iter().position(|(d, _)| d.as_str() >= ex_date.as_str()) {
            if idx >= 1 {
                let (prev, cur) = (daily[idx - 1].1, daily[idx].1);
                if prev > 0.0 && cur > 0.0 {
                    let observed = cur / prev;
                    let target = 1.0 / ratio;
                    // Uncorrected ⇔ observed close to 1/ratio (a real drop), AND a
                    // genuine drop (not ≈1.0 adjusted data).
                    if observed < 0.85 && (observed - target).abs() <= SPLIT_TOL * target {
                        uncorrected_split = Some(UncorrectedSplit {
                            date: ex_date.clone(),
                            ratio: *ratio,
                            observed_ratio: observed,
                        });
                        break;
                    }
                }
            }
        }
    }

    // 3) verdict + honest flags.
    let mut flags: Vec<String> = Vec::new();
    let mut unreliable = false;
    let mut caution = false;

    if invalid_price_bars > 0 {
        flags.push(format!(
            "{invalid_price_bars} bar(s) with non-positive/non-finite prices (bad ticks)"
        ));
        unreliable = true;
    }
    if max_jump >= JUMP_UNRELIABLE {
        flags.push(format!(
            "{:.0}× single-day price discontinuity{} — uncorrected corporate action or bad tick",
            max_jump,
            max_jump_at
                .as_ref()
                .map(|d| format!(" on {d}"))
                .unwrap_or_default()
        ));
        unreliable = true;
    } else if max_jump >= JUMP_CAUTION {
        flags.push(format!("~{max_jump:.1}× single-day jump — verify it is a real move"));
        caution = true;
    }
    if let Some(us) = &uncorrected_split {
        flags.push(format!(
            "uncorrected {:.0}:1 split on {} (price dropped {:.0}% — series not back-adjusted)",
            us.ratio,
            us.date,
            (1.0 - us.observed_ratio) * 100.0
        ));
        unreliable = true;
    }
    if min_close.is_finite() && min_close < LOW_PRICE_FLOOR {
        flags.push(format!("price dips to ₹{min_close:.2} (penny / bad-tick territory)"));
        caution = true;
    }

    let verdict = if unreliable {
        "unreliable"
    } else if caution {
        "caution"
    } else {
        "ok"
    };
    let reason = match verdict {
        "unreliable" => format!(
            "Data quality UNRELIABLE — {}. Backtest metrics for this series may be invalid; verify the price history before trusting any number below.",
            flags.join("; ")
        ),
        "caution" => format!("Data quality caution — {}.", flags.join("; ")),
        _ => "No data-quality concern found in the local series.".to_string(),
    };

    (max_jump, max_jump_at, uncorrected_split, verdict.to_string(), reason, flags)
}

/// Build a daily (date, last-close) series from index-aligned candle closes +
/// dates (both oldest-first). Collapses intraday bars to one close per day.
fn daily_series(closes: &[f64], dates: &[String]) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    let n = closes.len().min(dates.len());
    for i in 0..n {
        let d = &dates[i];
        let c = closes[i];
        match out.last_mut() {
            Some((last_d, last_c)) if last_d == d => *last_c = c, // keep the day's last close
            _ => out.push((d.clone(), c)),
        }
    }
    out
}

/// Load the symbol's splits + recent corporate actions from
/// `corporate_actions_all.parquet`. Best-effort: returns (splits, recent) and
/// empty vectors if the file/symbol is absent (never an error to the caller).
fn load_corp_actions(
    conn: &duckdb::Connection,
    root: &std::path::Path,
    symbol: &str,
) -> (Vec<(String, f64)>, Vec<CorpActionTag>) {
    let path = root.join("corporate_actions_all.parquet");
    if !path.exists() {
        return (Vec::new(), Vec::new());
    }
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS d, type, value \
         FROM read_parquet({}) WHERE upper(symbol) = upper(?) ORDER BY d",
        storage_kernel::quote_path(&path)
    );
    let mut splits: Vec<(String, f64)> = Vec::new();
    let mut all: Vec<CorpActionTag> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([symbol], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        }) {
            for r in rows.flatten() {
                let (d, kind, value) = r;
                if kind == "split" {
                    splits.push((d.clone(), value));
                }
                all.push(CorpActionTag { date: d, kind, value });
            }
        }
    }
    // recent = last N by date (already sorted ascending).
    let recent: Vec<CorpActionTag> = all.iter().rev().take(RECENT_ACTIONS).cloned().collect();
    (splits, recent)
}

/// Assess one symbol's data quality at a resolution. On-demand (single symbol —
/// fast); no warm cache needed. Errors only if the candles can't be loaded.
pub fn check_symbol(
    conn: &duckdb::Connection,
    root: &std::path::Path,
    symbol: &str,
    tf: Timeframe,
) -> anyhow::Result<DataQualityReport> {
    let candles = storage_kernel::load_candles(conn, root, symbol, tf)?;
    let dates = storage_kernel::load_candle_dates(conn, root, symbol, tf)?;

    let bars = candles.len();
    let mut invalid_price_bars = 0usize;
    let mut min_close = f64::INFINITY;
    let mut closes: Vec<f64> = Vec::with_capacity(bars);
    for c in &candles {
        let bad = !c.open.is_finite()
            || !c.high.is_finite()
            || !c.low.is_finite()
            || !c.close.is_finite()
            || c.open <= 0.0
            || c.high <= 0.0
            || c.low <= 0.0
            || c.close <= 0.0;
        if bad {
            invalid_price_bars += 1;
        }
        if c.close.is_finite() && c.close < min_close {
            min_close = c.close;
        }
        closes.push(c.close);
    }
    if !min_close.is_finite() {
        min_close = 0.0;
    }

    let daily = daily_series(&closes, &dates);
    let (splits, recent_actions) = load_corp_actions(conn, root, symbol);

    let (max_jump, max_jump_at, uncorrected_split, verdict, reason, flags) =
        assess(&daily, invalid_price_bars, min_close, &splits);

    let ok = verdict == "ok";
    Ok(DataQualityReport {
        symbol: symbol.to_string(),
        timeframe: tf.dir().to_string(),
        bars,
        daily_bars: daily.len(),
        invalid_price_bars,
        min_close,
        max_jump,
        max_jump_at,
        uncorrected_split,
        recent_actions,
        verdict,
        reason,
        flags,
        ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_series(n: usize) -> Vec<(String, f64)> {
        // A gently trending series, no jumps. Dates yyyy-mm-dd sortable.
        (0..n)
            .map(|i| (format!("2020-01-{:02}", (i % 28) + 1), 100.0 + i as f64 * 0.3))
            .collect()
    }

    #[test]
    fn clean_series_is_ok() {
        let d = clean_series(40);
        let (jump, _at, split, verdict, _reason, flags) = assess(&d, 0, 100.0, &[]);
        assert!(jump < 1.2, "clean jump should be tiny, got {jump}");
        assert!(split.is_none());
        assert_eq!(verdict, "ok");
        assert!(flags.is_empty());
    }

    #[test]
    fn invalid_price_bars_are_unreliable() {
        // PRIVISCL-shape: bad ticks present.
        let d = clean_series(20);
        let (_j, _a, _s, verdict, reason, flags) = assess(&d, 30, 0.0, &[]);
        assert_eq!(verdict, "unreliable");
        assert!(reason.to_lowercase().contains("unreliable"));
        assert!(flags.iter().any(|f| f.contains("bad ticks")));
    }

    #[test]
    fn extreme_jump_is_unreliable() {
        // KAMOPAINTS-shape: a 0.05 bad-tick island jumps to real prices.
        let mut d = vec![("2019-01-01".into(), 0.05), ("2019-01-02".into(), 0.05)];
        d.push(("2019-01-03".into(), 18.0)); // 360× jump
        for i in 0..20 {
            d.push((format!("2019-02-{:02}", i + 1), 18.0 + i as f64));
        }
        let (jump, at, _s, verdict, _r, flags) = assess(&d, 0, 0.05, &[]);
        assert!(jump >= JUMP_UNRELIABLE, "got {jump}");
        assert_eq!(at.as_deref(), Some("2019-01-03"));
        assert_eq!(verdict, "unreliable");
        // both the jump and the sub-₹1 price get flagged.
        assert!(flags.iter().any(|f| f.contains("discontinuity")));
        assert!(flags.iter().any(|f| f.contains("penny")));
    }

    #[test]
    fn uncorrected_split_is_detected_and_unreliable() {
        // Raw (unadjusted) 5:1 split: price drops to 1/5 across the ex-date.
        let mut d: Vec<(String, f64)> = (1..=10)
            .map(|i| (format!("2021-03-{:02}", i), 500.0))
            .collect();
        // ex-date 2021-03-15: 500 -> 100 (exactly 1/5). Gentle after.
        for i in 15..=25 {
            d.push((format!("2021-03-{:02}", i), 100.0));
        }
        let splits = vec![("2021-03-15".to_string(), 5.0)];
        let (_j, _a, split, verdict, _r, flags) = assess(&d, 0, 100.0, &splits);
        let us = split.expect("should detect uncorrected split");
        assert_eq!(us.date, "2021-03-15");
        assert!((us.observed_ratio - 0.2).abs() < 1e-6);
        assert_eq!(verdict, "unreliable");
        assert!(flags.iter().any(|f| f.contains("uncorrected")));
    }

    #[test]
    fn back_adjusted_split_is_not_flagged() {
        // Same 5:1 split but the data IS adjusted (continuous across ex-date):
        // no drop, so it must NOT be flagged as uncorrected.
        let d: Vec<(String, f64)> = (1..=25)
            .map(|i| (format!("2021-03-{:02}", i), 100.0 + i as f64 * 0.2))
            .collect();
        let splits = vec![("2021-03-15".to_string(), 5.0)];
        let (_j, _a, split, verdict, _r, _f) = assess(&d, 0, 100.0, &splits);
        assert!(split.is_none(), "adjusted data must not trip the split check");
        assert_eq!(verdict, "ok");
    }

    #[test]
    fn daily_series_collapses_intraday_to_last_close() {
        let closes = vec![10.0, 11.0, 12.0, 20.0, 21.0];
        let dates = vec![
            "2022-01-01".to_string(),
            "2022-01-01".to_string(),
            "2022-01-01".to_string(),
            "2022-01-02".to_string(),
            "2022-01-02".to_string(),
        ];
        let d = daily_series(&closes, &dates);
        assert_eq!(d, vec![("2022-01-01".into(), 12.0), ("2022-01-02".into(), 21.0)]);
    }
}

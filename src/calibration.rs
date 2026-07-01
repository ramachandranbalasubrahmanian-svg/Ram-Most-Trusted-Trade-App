//! Calibration scorecard — does the engine's claimed win% hold up in YOUR journal?
//!
//! FIREWALLED & PURE: imports only `types` + the edge index. NEVER feeds
//! `eligible()`, Confidence, ranking, or sizing — it is a backward-looking
//! reliability check, the long-run "should I trust this system" anchor.
//!
//! The journal logs realized outcomes (pnl) but not the predicted win% at entry,
//! so we re-derive the prediction by matching each closed trade's
//! (symbol, strategy, direction) to the backtested edge's `win_pct`, then compare
//! predicted vs realized — bucketed into a reliability curve. Honest about small
//! samples: a handful of trades can't calibrate anything, and the panel says so.

use serde::Serialize;

use crate::strategy_engine::EdgeIndex;
use crate::symbol_resolver::SymbolResolver;
use crate::types::JournalEntry;

/// One predicted-win% bucket of the reliability curve.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CalibrationBucket {
    pub label: String,
    /// Average backtested (predicted) win% of the trades in this bucket.
    pub predicted_pct: f64,
    /// Realized win% (share of trades that closed in profit).
    pub realized_pct: f64,
    pub n: usize,
}

/// The full calibration scorecard.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Calibration {
    pub available: bool,
    /// Closed trades in the journal (those with a realized pnl).
    pub n_closed: usize,
    /// Closed trades we could match to a backtested edge (have a prediction).
    pub n_matched: usize,
    /// Realized win% over matched trades.
    pub realized_win_pct: f64,
    /// Average predicted win% over matched trades.
    pub predicted_win_pct: f64,
    /// realized − predicted: negative ⇒ the backtest was OVER-confident live.
    pub gap_pct: f64,
    pub buckets: Vec<CalibrationBucket>,
    pub note: String,
}

/// Look up the backtested win% for a journal trade by (symbol, strategy, dir).
///
/// Two robustness steps so real journals actually match the edge map:
///  1. **Symbol resolution** — imported/broker rows store the full company name
///     ("VIKRAN ENGINEERING LTD"), while the edge map keys on the NSE ticker
///     ("VIKRAN"). Resolve name → ticker via the (display-only) `SymbolResolver`
///     when one is supplied; otherwise fall back to the raw uppercased symbol.
///  2. **Strategy fallback** — imported trades carry `strategy = "Imported"`, which
///     matches no backtested strategy. When there is no exact strategy match, fall
///     back to the strongest eligible edge for this (symbol, direction) by
///     expectancy — i.e. "what the engine's best validated edge on this name+side
///     predicts." Still honest: if the engine has NO edge in that direction (e.g.
///     a long trade on a name with only short edges), it stays unmatched.
fn predicted_win_pct(
    entry: &JournalEntry,
    edges: &EdgeIndex,
    resolver: Option<&SymbolResolver>,
) -> Option<f64> {
    let raw = entry.symbol.trim();
    let sym = match resolver {
        // `resolve` passes a raw ticker straight through and maps a company name to
        // its ticker; on no confident match it returns the uppercased raw name.
        Some(r) => r.resolve(raw, None).symbol,
        None => raw.to_uppercase(),
    };
    let dir = entry.direction.trim().to_uppercase();
    let list = edges.get(&sym)?;
    // 1) Exact (strategy, direction) match — unchanged behaviour for engine-logged trades.
    if let Some(e) = list
        .iter()
        .find(|e| e.strategy == entry.strategy && e.direction.as_str() == dir)
    {
        return Some(e.win_pct);
    }
    // 2) Fallback for discretionary/imported trades with no strategy link: the
    //    strongest eligible edge on this (symbol, direction).
    list.iter()
        .filter(|e| e.direction.as_str() == dir)
        .max_by(|a, b| {
            a.expectancy_r
                .partial_cmp(&b.expectancy_r)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|e| e.win_pct)
}

fn bucket_label(predicted: f64) -> &'static str {
    if predicted < 55.0 {
        "<55%"
    } else if predicted < 65.0 {
        "55–65%"
    } else if predicted < 75.0 {
        "65–75%"
    } else {
        "≥75%"
    }
}

/// Build the calibration scorecard from the journal + the live edge index. Pure.
///
/// `resolver` (optional) maps imported company names → NSE tickers so the journal
/// actually joins the edge map. Pass `None` to match on the raw uppercased symbol.
/// Firewalled: reads only the edge index + name metadata; never writes to scoring.
pub fn build(
    entries: &[JournalEntry],
    edges: &EdgeIndex,
    resolver: Option<&SymbolResolver>,
) -> Calibration {
    // Closed = has a realized pnl.
    let closed: Vec<&JournalEntry> = entries.iter().filter(|e| e.pnl.is_some()).collect();
    let n_closed = closed.len();
    if n_closed == 0 {
        return Calibration {
            available: false,
            note: "No closed trades logged yet — accept signals and record exits in the journal; this scorecard builds as you trade.".to_string(),
            ..Default::default()
        };
    }

    // Match each closed trade to its predicted win% and realized outcome.
    let mut matched: Vec<(f64, bool)> = Vec::new(); // (predicted_pct, realized_win)
    for e in &closed {
        if let Some(pred) = predicted_win_pct(e, edges, resolver) {
            let win = e.pnl.unwrap_or(0.0) > 0.0;
            matched.push((pred, win));
        }
    }
    let n_matched = matched.len();
    if n_matched == 0 {
        return Calibration {
            available: false,
            n_closed,
            note: format!(
                "{n_closed} closed trade(s), but none has an eligible backtested edge in the traded direction — e.g. long trades on names the engine only validates short, or symbols absent from the map. Nothing to compare predicted vs realized against yet."
            ),
            ..Default::default()
        };
    }

    let realized_win_pct = matched.iter().filter(|(_, w)| *w).count() as f64 / n_matched as f64 * 100.0;
    let predicted_win_pct = matched.iter().map(|(p, _)| *p).sum::<f64>() / n_matched as f64;
    let gap_pct = realized_win_pct - predicted_win_pct;

    // Reliability buckets.
    let bins = ["<55%", "55–65%", "65–75%", "≥75%"];
    let mut buckets: Vec<CalibrationBucket> = Vec::new();
    for bin in bins {
        let rows: Vec<&(f64, bool)> = matched.iter().filter(|(p, _)| bucket_label(*p) == bin).collect();
        if rows.is_empty() {
            continue;
        }
        let n = rows.len();
        let predicted = rows.iter().map(|(p, _)| *p).sum::<f64>() / n as f64;
        let realized = rows.iter().filter(|(_, w)| *w).count() as f64 / n as f64 * 100.0;
        buckets.push(CalibrationBucket {
            label: bin.to_string(),
            predicted_pct: predicted,
            realized_pct: realized,
            n,
        });
    }

    let note = if n_matched < 20 {
        format!(
            "Only {n_matched} matched trade(s) — too few to read the curve yet (aim for ~20+). Early read: realized {realized_win_pct:.0}% vs predicted {predicted_win_pct:.0}% ({gap_pct:+.0} pts)."
        )
    } else if gap_pct < -10.0 {
        format!(
            "Realized win {realized_win_pct:.0}% is {:.0} pts BELOW the backtested {predicted_win_pct:.0}% — the edges are over-stating live; size down / trust Confidence less.",
            gap_pct.abs()
        )
    } else if gap_pct > 10.0 {
        format!(
            "Realized win {realized_win_pct:.0}% is running ABOVE the backtested {predicted_win_pct:.0}% — favourable, but don't over-extrapolate a small sample."
        )
    } else {
        format!(
            "Realized {realized_win_pct:.0}% vs predicted {predicted_win_pct:.0}% — broadly calibrated ({gap_pct:+.0} pts) over {n_matched} trades."
        )
    };

    Calibration {
        available: true,
        n_closed,
        n_matched,
        realized_win_pct,
        predicted_win_pct,
        gap_pct,
        buckets,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Direction;
    use crate::types::{EligibleEdge, Robustness};

    fn entry(sym: &str, strat: &str, dir: &str, pnl: Option<f64>) -> JournalEntry {
        JournalEntry {
            id: 0,
            generated_ist: String::new(),
            entry_ist: None,
            exit_ist: None,
            instrument_token: 0,
            symbol: sym.into(),
            direction: dir.into(),
            strategy: strat.into(),
            alpha_trigger: String::new(),
            intended_price: 100.0,
            actual_fill_price: None,
            exit_price: None,
            qty: 1,
            state: "Exited".into(),
            pnl,
            slippage: None,
            sector: None,
        }
    }

    fn edge_index() -> EdgeIndex {
        let mut idx = EdgeIndex::new();
        idx.insert(
            "AAA".to_string(),
            vec![EligibleEdge {
                strategy: "vwap_cross".into(),
                direction: Direction::Long,
                expectancy_r: 0.3,
                profit_factor: 1.6,
                win_pct: 70.0,
                n: 100,
                robustness: Robustness::default(),
            }],
        );
        idx
    }

    #[test]
    fn empty_journal_is_unavailable() {
        let c = build(&[], &edge_index(), None);
        assert!(!c.available);
        assert!(c.note.contains("No closed trades"));
    }

    #[test]
    fn open_trades_are_ignored() {
        // pnl=None ⇒ not closed ⇒ no scorecard.
        let c = build(&[entry("AAA", "vwap_cross", "BUY", None)], &edge_index(), None);
        assert!(!c.available);
    }

    #[test]
    fn matches_prediction_and_computes_realized() {
        // 4 closed AAA/vwap_cross/BUY trades (predicted 70%): 3 wins, 1 loss ⇒ 75% realized.
        let entries = vec![
            entry("AAA", "vwap_cross", "BUY", Some(100.0)),
            entry("AAA", "vwap_cross", "BUY", Some(50.0)),
            entry("AAA", "vwap_cross", "BUY", Some(20.0)),
            entry("AAA", "vwap_cross", "BUY", Some(-80.0)),
        ];
        let c = build(&entries, &edge_index(), None);
        assert!(c.available);
        assert_eq!(c.n_matched, 4);
        assert!((c.predicted_win_pct - 70.0).abs() < 1e-9);
        assert!((c.realized_win_pct - 75.0).abs() < 1e-9);
        assert!((c.gap_pct - 5.0).abs() < 1e-9);
        assert_eq!(c.buckets.len(), 1);
        assert_eq!(c.buckets[0].label, "65–75%");
    }

    fn edge_index_multi() -> EdgeIndex {
        // VIKRAN has two long edges (weaker vwap_cross, stronger gap_and_go) and a short.
        let mut idx = EdgeIndex::new();
        idx.insert(
            "VIKRAN".to_string(),
            vec![
                EligibleEdge { strategy: "vwap_cross".into(), direction: Direction::Long, expectancy_r: 0.10, profit_factor: 1.3, win_pct: 60.0, n: 40, robustness: Robustness::default() },
                EligibleEdge { strategy: "gap_and_go".into(), direction: Direction::Long, expectancy_r: 0.25, profit_factor: 1.5, win_pct: 66.0, n: 39, robustness: Robustness::default() },
                EligibleEdge { strategy: "orb_15m".into(),   direction: Direction::Short, expectancy_r: 0.20, profit_factor: 1.4, win_pct: 58.0, n: 50, robustness: Robustness::default() },
            ],
        );
        idx
    }

    #[test]
    fn resolves_company_name_to_ticker_and_matches() {
        // Journal stores the full company name; the resolver maps it to VIKRAN so
        // the (best-eligible-long) edge is found even with a non-engine strategy.
        let resolver = SymbolResolver::from_pairs(&[("VIKRAN", "Vikran Engineering Limited")]);
        let e = entry("VIKRAN ENGINEERING LTD", "Imported", "BUY", Some(500.0));
        let c = build(&[e], &edge_index_multi(), Some(&resolver));
        assert!(c.available);
        assert_eq!(c.n_matched, 1);
        // Fallback picks the strongest long edge (gap_and_go, 66%), not the weaker one.
        assert!((c.predicted_win_pct - 66.0).abs() < 1e-9);
    }

    #[test]
    fn imported_strategy_falls_back_to_best_direction_edge() {
        // No resolver needed (symbol already a ticker); strategy "Imported" must
        // still match via the direction fallback to the best long edge.
        let e = entry("VIKRAN", "Imported", "BUY", Some(10.0));
        let c = build(&[e], &edge_index_multi(), None);
        assert!(c.available);
        assert_eq!(c.n_matched, 1);
        assert!((c.predicted_win_pct - 66.0).abs() < 1e-9);
    }

    #[test]
    fn long_trade_on_short_only_name_stays_unmatched() {
        // Engine validates only the SHORT side of this name; a LONG trade has no
        // eligible edge in its direction, so it must NOT be fabricated a prediction.
        let mut idx = EdgeIndex::new();
        idx.insert(
            "MARSONS".to_string(),
            vec![EligibleEdge { strategy: "orb_15m".into(), direction: Direction::Short, expectancy_r: 0.2, profit_factor: 1.4, win_pct: 58.0, n: 60, robustness: Robustness::default() }],
        );
        let c = build(&[entry("MARSONS", "Imported", "BUY", Some(-500.0))], &idx, None);
        assert!(!c.available);
        assert_eq!(c.n_matched, 0);
    }

    #[test]
    fn unmatched_symbol_reports_honestly() {
        // A closed trade on a symbol not in the edge index ⇒ matched 0.
        let c = build(&[entry("ZZZ", "vwap_cross", "BUY", Some(10.0))], &edge_index(), None);
        assert!(!c.available);
        assert_eq!(c.n_closed, 1);
        assert!(c.note.contains("none has an eligible"));
    }
}

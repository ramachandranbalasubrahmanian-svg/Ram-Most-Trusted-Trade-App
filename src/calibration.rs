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
fn predicted_win_pct(entry: &JournalEntry, edges: &EdgeIndex) -> Option<f64> {
    let sym = entry.symbol.trim().to_uppercase();
    let dir = entry.direction.trim().to_uppercase();
    edges.get(&sym).and_then(|list| {
        list.iter()
            .find(|e| e.strategy == entry.strategy && e.direction.as_str() == dir)
            .map(|e| e.win_pct)
    })
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
pub fn build(entries: &[JournalEntry], edges: &EdgeIndex) -> Calibration {
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
        if let Some(pred) = predicted_win_pct(e, edges) {
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
                "{n_closed} closed trade(s), but none matched a current backtested edge (symbol/strategy not in the live edge map) — can't compare predicted vs realized yet."
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
        let c = build(&[], &edge_index());
        assert!(!c.available);
        assert!(c.note.contains("No closed trades"));
    }

    #[test]
    fn open_trades_are_ignored() {
        // pnl=None ⇒ not closed ⇒ no scorecard.
        let c = build(&[entry("AAA", "vwap_cross", "BUY", None)], &edge_index());
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
        let c = build(&entries, &edge_index());
        assert!(c.available);
        assert_eq!(c.n_matched, 4);
        assert!((c.predicted_win_pct - 70.0).abs() < 1e-9);
        assert!((c.realized_win_pct - 75.0).abs() < 1e-9);
        assert!((c.gap_pct - 5.0).abs() < 1e-9);
        assert_eq!(c.buckets.len(), 1);
        assert_eq!(c.buckets[0].label, "65–75%");
    }

    #[test]
    fn unmatched_symbol_reports_honestly() {
        // A closed trade on a symbol not in the edge index ⇒ matched 0.
        let c = build(&[entry("ZZZ", "vwap_cross", "BUY", Some(10.0))], &edge_index());
        assert!(!c.available);
        assert_eq!(c.n_closed, 1);
        assert!(c.note.contains("none matched"));
    }
}

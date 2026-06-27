//! Post-trade performance evaluation from the manual journal.
//!
//! Win rate, profit factor, Sharpe, max drawdown, cumulative equity curve, and
//! attribution by strategy and by sector — computed over completed (PnL-bearing)
//! journal trades.
//!
//! Per-trade Sharpe = mean(pnl)/stddev(pnl) (sample std, ddof=1), matching the
//! rest of the system. Everything here is a pure function over the entries — no
//! I/O, no global state, deterministic for a given input.

use std::collections::HashMap;

use crate::types::{AttributionRow, JournalEntry, PortfolioMetrics};

/// Cap on profit factor when there are profits but no losing trades, so a
/// divide-by-zero is reported as a large-but-finite number.
const PROFIT_FACTOR_CAP: f64 = 99.0;

/// Compute the full performance matrix from journal entries.
///
/// Only entries with `pnl = Some(_)` (completed trades) are considered. The
/// equity curve is ordered by `exit_ist` (ascending, `None` last) then `id`
/// ascending; everything else is derived from that completed set. An empty
/// journal (or one with no completed trades) yields all-zero metrics and empty
/// vectors.
pub fn compute(entries: &[JournalEntry]) -> PortfolioMetrics {
    // Keep only completed trades (those with a realized PnL).
    let mut completed: Vec<&JournalEntry> =
        entries.iter().filter(|e| e.pnl.is_some()).collect();

    if completed.is_empty() {
        return PortfolioMetrics {
            trades: 0,
            win_rate: 0.0,
            profit_factor: 0.0,
            sharpe: 0.0,
            max_drawdown: 0.0,
            total_pnl: 0.0,
            equity_curve: Vec::new(),
            by_strategy: Vec::new(),
            by_sector: Vec::new(),
        };
    }

    // Chronological order: by exit_ist (None sorts last), then id ascending.
    // A stable sort keeps insertion order for equal keys as a final tiebreak.
    completed.sort_by(|a, b| {
        let ka = exit_sort_key(a.exit_ist.as_deref());
        let kb = exit_sort_key(b.exit_ist.as_deref());
        ka.cmp(&kb).then(a.id.cmp(&b.id))
    });

    let trades = completed.len();

    // Per-trade pnl series (guaranteed Some by the filter above).
    let pnls: Vec<f64> = completed.iter().map(|e| e.pnl.unwrap_or(0.0)).collect();

    let total_pnl: f64 = pnls.iter().sum();
    let wins = pnls.iter().filter(|&&p| p > 0.0).count();
    let win_rate = wins as f64 / trades as f64 * 100.0;

    let profit_factor = profit_factor_of(&pnls);
    let sharpe = sharpe_of(&pnls);

    // Equity curve + peak-to-trough drawdown in one pass.
    let mut equity_curve: Vec<(String, f64)> = Vec::with_capacity(trades);
    let mut cum = 0.0_f64;
    // Equity starts at 0 (initial capital baseline), so a losing-only run draws
    // down by its full loss — peak must start at 0, not −∞.
    let mut peak = 0.0_f64;
    let mut max_drawdown = 0.0_f64;
    for (idx, e) in completed.iter().enumerate() {
        cum += e.pnl.unwrap_or(0.0);
        // Label: exit_ist if present, else the 1-based index as a string.
        let label = match &e.exit_ist {
            Some(ts) => ts.clone(),
            None => (idx + 1).to_string(),
        };
        equity_curve.push((label, cum));

        if cum > peak {
            peak = cum;
        }
        let dd = peak - cum;
        if dd > max_drawdown {
            max_drawdown = dd;
        }
    }

    let by_strategy = attribute(&completed, |e| e.strategy.clone());
    let by_sector = attribute(&completed, |e| {
        e.sector.clone().unwrap_or_else(|| "Unknown".to_string())
    });

    PortfolioMetrics {
        trades,
        win_rate,
        profit_factor,
        sharpe,
        max_drawdown,
        total_pnl,
        equity_curve,
        by_strategy,
        by_sector,
    }
}

// ---------------------------------------------------------------------------
// helpers (private)
// ---------------------------------------------------------------------------

/// Sort key for `exit_ist`: `Some` entries sort before `None`, and within
/// `Some` they sort lexicographically (ISO-style "YYYY-MM-DD HH:MM:SS" strings
/// sort chronologically). Returns `(is_none, value)` so `false < true`.
fn exit_sort_key(exit: Option<&str>) -> (bool, String) {
    match exit {
        Some(s) => (false, s.to_string()),
        None => (true, String::new()),
    }
}

/// Profit factor = gross profit / gross loss. With profit and no losses, cap at
/// `PROFIT_FACTOR_CAP`; with neither (or only losses summing to 0 profit), 0.0.
fn profit_factor_of(pnls: &[f64]) -> f64 {
    let gross_profit: f64 = pnls.iter().filter(|&&p| p > 0.0).sum();
    let gross_loss: f64 = pnls
        .iter()
        .filter(|&&p| p < 0.0)
        .map(|p| p.abs())
        .sum();

    if gross_loss > 0.0 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        PROFIT_FACTOR_CAP
    } else {
        0.0
    }
}

/// Per-trade Sharpe = mean/stddev (sample std, ddof=1). 0 if <2 trades or std==0.
fn sharpe_of(pnls: &[f64]) -> f64 {
    let n = pnls.len();
    if n < 2 {
        return 0.0;
    }
    let mean = pnls.iter().sum::<f64>() / n as f64;
    let var = pnls.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n as f64 - 1.0);
    let std = var.sqrt();
    if std == 0.0 {
        return 0.0;
    }
    mean / std
}

/// Build attribution rows grouped by `key_of`, one row per distinct key, sorted
/// by pnl descending. Each row's profit factor follows the same cap rules as the
/// portfolio-level figure.
fn attribute<F>(completed: &[&JournalEntry], key_of: F) -> Vec<AttributionRow>
where
    F: Fn(&JournalEntry) -> String,
{
    // Preserve first-seen order for deterministic tiebreaks among equal pnls.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<f64>> = HashMap::new();

    for e in completed {
        let key = key_of(e);
        let pnl = e.pnl.unwrap_or(0.0);
        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                Vec::new()
            })
            .push(pnl);
    }

    let mut rows: Vec<AttributionRow> = order
        .into_iter()
        .map(|key| {
            let pnls = &groups[&key];
            let trades = pnls.len();
            let wins = pnls.iter().filter(|&&p| p > 0.0).count();
            let win_rate = if trades == 0 {
                0.0
            } else {
                wins as f64 / trades as f64 * 100.0
            };
            let pnl: f64 = pnls.iter().sum();
            AttributionRow {
                key,
                trades,
                win_rate,
                profit_factor: profit_factor_of(pnls),
                pnl,
            }
        })
        .collect();

    // Sort by pnl descending; stable sort preserves first-seen order for ties.
    rows.sort_by(|a, b| {
        b.pnl
            .partial_cmp(&a.pnl)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a journal entry with the fields this module reads; the rest are
    /// filled with harmless defaults.
    fn entry(
        id: i64,
        exit_ist: Option<&str>,
        strategy: &str,
        sector: Option<&str>,
        pnl: Option<f64>,
    ) -> JournalEntry {
        JournalEntry {
            id,
            generated_ist: "2026-06-27 09:30:00".to_string(),
            entry_ist: Some("2026-06-27 09:45:00".to_string()),
            exit_ist: exit_ist.map(|s| s.to_string()),
            instrument_token: 100 + id as u32,
            symbol: "TEST".to_string(),
            direction: "BUY".to_string(),
            strategy: strategy.to_string(),
            alpha_trigger: "vwap_cross".to_string(),
            intended_price: 100.0,
            actual_fill_price: Some(100.0),
            exit_price: Some(102.0),
            qty: 10,
            state: "Manually_Accepted".to_string(),
            pnl,
            slippage: Some(0.0),
            sector: sector.map(|s| s.to_string()),
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn empty_journal_is_all_zero() {
        let m = compute(&[]);
        assert_eq!(m.trades, 0);
        assert!(approx(m.win_rate, 0.0));
        assert!(approx(m.profit_factor, 0.0));
        assert!(approx(m.sharpe, 0.0));
        assert!(approx(m.max_drawdown, 0.0));
        assert!(approx(m.total_pnl, 0.0));
        assert!(m.equity_curve.is_empty());
        assert!(m.by_strategy.is_empty());
        assert!(m.by_sector.is_empty());
    }

    #[test]
    fn no_completed_trades_is_all_zero() {
        // Entries exist but none has a realized pnl => treated as empty.
        let entries = vec![
            entry(1, Some("2026-06-27 10:00:00"), "vwap", Some("IT"), None),
            entry(2, Some("2026-06-27 11:00:00"), "orb", Some("Bank"), None),
        ];
        let m = compute(&entries);
        assert_eq!(m.trades, 0);
        assert!(m.equity_curve.is_empty());
        assert!(m.by_strategy.is_empty());
        assert!(m.by_sector.is_empty());
    }

    #[test]
    fn metrics_and_attribution_grouping() {
        // Two wins + a loss across two strategies / two sectors.
        //   vwap / IT   : +200   (exit 10:00)
        //   orb  / Bank : -100   (exit 11:00)
        //   vwap / IT   : +300   (exit 12:00)
        // A pending (None pnl) entry must be ignored entirely.
        let entries = vec![
            entry(1, Some("2026-06-27 10:00:00"), "vwap", Some("IT"), Some(200.0)),
            entry(2, Some("2026-06-27 11:00:00"), "orb", Some("Bank"), Some(-100.0)),
            entry(3, Some("2026-06-27 12:00:00"), "vwap", Some("IT"), Some(300.0)),
            entry(4, Some("2026-06-27 13:00:00"), "vwap", Some("IT"), None),
        ];
        let m = compute(&entries);

        // 3 completed trades.
        assert_eq!(m.trades, 3);
        assert!(approx(m.total_pnl, 400.0));

        // 2 wins of 3 => 66.666...%.
        assert!((m.win_rate - 200.0 / 3.0).abs() < 1e-9);

        // PF = gross profit (500) / gross loss (100) = 5.0.
        assert!(approx(m.profit_factor, 5.0));

        // Equity curve cumulative: 200, 100, 400. Peak 200 -> trough 100 => dd 100.
        assert_eq!(m.equity_curve.len(), 3);
        assert!(approx(m.equity_curve[0].1, 200.0));
        assert!(approx(m.equity_curve[1].1, 100.0));
        assert!(approx(m.equity_curve[2].1, 400.0));
        assert_eq!(m.equity_curve[0].0, "2026-06-27 10:00:00");
        assert!(approx(m.max_drawdown, 100.0));

        // Sharpe: mean=133.333..., sample std of [200,-100,300].
        // var = ((66.667^2)+(-233.333^2)+(166.667^2))/2 = 43333.33..., std≈208.166.
        let mean: f64 = 400.0 / 3.0;
        let var = ((200.0 - mean).powi(2)
            + (-100.0 - mean).powi(2)
            + (300.0 - mean).powi(2))
            / 2.0;
        let expected_sharpe = mean / var.sqrt();
        assert!((m.sharpe - expected_sharpe).abs() < 1e-9, "sharpe {}", m.sharpe);

        // by_strategy: vwap (+500, 2 trades, 100% wr, PF capped 99) then orb (-100).
        assert_eq!(m.by_strategy.len(), 2);
        assert_eq!(m.by_strategy[0].key, "vwap");
        assert_eq!(m.by_strategy[0].trades, 2);
        assert!(approx(m.by_strategy[0].pnl, 500.0));
        assert!(approx(m.by_strategy[0].win_rate, 100.0));
        assert!(approx(m.by_strategy[0].profit_factor, PROFIT_FACTOR_CAP));
        assert_eq!(m.by_strategy[1].key, "orb");
        assert!(approx(m.by_strategy[1].pnl, -100.0));
        assert!(approx(m.by_strategy[1].win_rate, 0.0));
        // Sorted by pnl desc.
        assert!(m.by_strategy[0].pnl >= m.by_strategy[1].pnl);

        // by_sector: IT (+500) then Bank (-100), sorted by pnl desc.
        assert_eq!(m.by_sector.len(), 2);
        assert_eq!(m.by_sector[0].key, "IT");
        assert!(approx(m.by_sector[0].pnl, 500.0));
        assert_eq!(m.by_sector[1].key, "Bank");
        assert!(approx(m.by_sector[1].pnl, -100.0));
    }

    #[test]
    fn missing_sector_is_unknown() {
        let entries = vec![
            entry(1, Some("2026-06-27 10:00:00"), "vwap", None, Some(50.0)),
            entry(2, Some("2026-06-27 11:00:00"), "vwap", Some("IT"), Some(10.0)),
        ];
        let m = compute(&entries);
        let keys: Vec<&str> = m.by_sector.iter().map(|r| r.key.as_str()).collect();
        assert!(keys.contains(&"Unknown"));
        assert!(keys.contains(&"IT"));
    }

    #[test]
    fn ordering_uses_exit_then_id_with_none_last() {
        // Out-of-order input; entry with no exit_ist must land last and use its
        // 1-based curve index as its label.
        let entries = vec![
            entry(3, Some("2026-06-27 12:00:00"), "vwap", Some("IT"), Some(30.0)),
            entry(1, Some("2026-06-27 10:00:00"), "vwap", Some("IT"), Some(10.0)),
            entry(9, None, "vwap", Some("IT"), Some(99.0)),
            entry(2, Some("2026-06-27 11:00:00"), "vwap", Some("IT"), Some(20.0)),
        ];
        let m = compute(&entries);
        // Chronological by exit_ist, None last.
        let labels: Vec<&str> = m.equity_curve.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "2026-06-27 10:00:00",
                "2026-06-27 11:00:00",
                "2026-06-27 12:00:00",
                "4", // the None-exit entry, 4th position => index label "4"
            ]
        );
        // Cumulative: 10, 30, 60, 159.
        assert!(approx(m.equity_curve[3].1, 159.0));
    }

    #[test]
    fn profit_factor_zero_when_only_losses() {
        let entries = vec![
            entry(1, Some("2026-06-27 10:00:00"), "vwap", Some("IT"), Some(-10.0)),
            entry(2, Some("2026-06-27 11:00:00"), "vwap", Some("IT"), Some(-20.0)),
        ];
        let m = compute(&entries);
        // Gross profit 0 => PF 0.0 (not the cap).
        assert!(approx(m.profit_factor, 0.0));
        assert!(approx(m.total_pnl, -30.0));
        // Drawdown is the full peak(0)-to-trough(-30) slide.
        assert!(approx(m.max_drawdown, 30.0));
    }
}

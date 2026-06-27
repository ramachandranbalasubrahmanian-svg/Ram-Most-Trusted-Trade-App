//! Synthetic drawdown monitor → Signal Freeze.
//!
//! Sums the synthetic mark-to-market PnL of manually-accepted (tracked) trades.
//! If today's cumulative paper loss breaches −`threshold_pct` of the capital
//! pool, the returned [`FreezeState`] is `frozen` — the live broadcast halts and
//! the session locks out (manual-discipline protection). No orders are touched.
//!
//! Per-entry P&L rule (ManuallyAccepted entries only; Generated / Rejected /
//! Skipped are ignored):
//!   * If `pnl` is `Some` (a closed trade), use that realized rupee P&L directly.
//!   * Otherwise (an open accepted trade) mark to market:
//!         qty × (mark − entry_basis) × dir
//!     where `entry_basis = actual_fill_price.unwrap_or(intended_price)`,
//!     `dir = +1` for BUY / `−1` for SELL, and `mark = mark_prices[symbol]`.
//!     If no mark price is available for the symbol, the unrealized term is
//!     skipped (contributes 0) — we never fabricate a price.

use std::collections::HashMap;

use crate::types::{FreezeState, JournalEntry, SignalState};

/// Evaluate the freeze condition from the journal. `mark_prices` maps symbol →
/// latest price for marking open accepted positions (empty = realized PnL only).
pub fn evaluate(
    entries: &[JournalEntry],
    capital_pool: f64,
    threshold_pct: f64,
    mark_prices: &HashMap<String, f64>,
) -> FreezeState {
    let mut state = FreezeState::active(capital_pool, threshold_pct);

    let mut daily_pnl = 0.0_f64;
    for e in entries {
        // Only manually-accepted (tracked) trades count toward the drawdown.
        if SignalState::from_str(&e.state) != SignalState::ManuallyAccepted {
            continue;
        }

        match e.pnl {
            // Closed trade: realized rupee P&L is authoritative.
            Some(realized) => {
                daily_pnl += realized;
            }
            // Open trade: mark to market if we have a price; otherwise skip.
            None => {
                if let Some(&mark) = mark_prices.get(&e.symbol) {
                    let dir = match e.direction.as_str() {
                        "SELL" => -1.0,
                        _ => 1.0, // "BUY" (and any default) is long
                    };
                    let entry_basis = e.actual_fill_price.unwrap_or(e.intended_price);
                    daily_pnl += e.qty as f64 * (mark - entry_basis) * dir;
                }
                // No mark price -> contribute nothing (no fabricated mark).
            }
        }
    }

    state.daily_pnl = daily_pnl;
    // threshold is negative (−capital_pool×threshold_pct); breach is <=.
    state.frozen = daily_pnl <= state.threshold;
    if state.frozen {
        state.reason = format!(
            "Synthetic drawdown freeze: daily P&L Rs {:.0} breached -{:.0}% (Rs {:.0})",
            daily_pnl,
            threshold_pct * 100.0,
            state.threshold,
        );
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        id: i64,
        symbol: &str,
        direction: &str,
        state: SignalState,
        intended: f64,
        actual_fill: Option<f64>,
        qty: i64,
        pnl: Option<f64>,
    ) -> JournalEntry {
        JournalEntry {
            id,
            generated_ist: "2026-06-28 09:20:00".to_string(),
            entry_ist: None,
            exit_ist: None,
            instrument_token: 0,
            symbol: symbol.to_string(),
            direction: direction.to_string(),
            strategy: "vwap_trend".to_string(),
            alpha_trigger: String::new(),
            intended_price: intended,
            actual_fill_price: actual_fill,
            exit_price: None,
            qty,
            state: state.as_str().to_string(),
            pnl,
            slippage: None,
            sector: None,
        }
    }

    #[test]
    fn big_accepted_loss_freezes() {
        // Pool 1,000,000 -> threshold = -20,000. A realized -25,000 breaches it.
        let entries = vec![entry(
            1,
            "RELIANCE",
            "BUY",
            SignalState::ManuallyAccepted,
            1300.0,
            Some(1300.0),
            100,
            Some(-25_000.0),
        )];
        let marks = HashMap::new();
        let fs = evaluate(&entries, 1_000_000.0, 0.02, &marks);
        assert!(fs.frozen, "expected freeze, daily_pnl={}", fs.daily_pnl);
        assert!((fs.threshold - -20_000.0).abs() < 1e-6);
        assert!((fs.daily_pnl - -25_000.0).abs() < 1e-6);
        assert!(!fs.reason.is_empty(), "frozen state must carry a reason");
    }

    #[test]
    fn small_loss_does_not_freeze() {
        // Realized -5,000 is well inside the -20,000 threshold.
        let entries = vec![entry(
            1,
            "INFY",
            "SELL",
            SignalState::ManuallyAccepted,
            1500.0,
            None,
            50,
            Some(-5_000.0),
        )];
        let marks = HashMap::new();
        let fs = evaluate(&entries, 1_000_000.0, 0.02, &marks);
        assert!(!fs.frozen, "should not freeze, daily_pnl={}", fs.daily_pnl);
        assert!(fs.reason.is_empty());
        assert!((fs.daily_pnl - -5_000.0).abs() < 1e-6);
    }

    #[test]
    fn rejected_and_generated_entries_are_ignored() {
        // A huge loss on a REJECTED + a GENERATED entry must not count at all.
        let entries = vec![
            entry(
                1,
                "TCS",
                "BUY",
                SignalState::ManuallyRejected,
                3000.0,
                Some(3000.0),
                500,
                Some(-500_000.0),
            ),
            entry(
                2,
                "SBIN",
                "BUY",
                SignalState::Generated,
                800.0,
                None,
                1000,
                Some(-300_000.0),
            ),
            entry(
                3,
                "SBIN",
                "BUY",
                SignalState::Skipped,
                800.0,
                None,
                1000,
                Some(-300_000.0),
            ),
        ];
        let marks = HashMap::new();
        let fs = evaluate(&entries, 1_000_000.0, 0.02, &marks);
        assert!(!fs.frozen, "ignored entries must not freeze");
        assert!((fs.daily_pnl - 0.0).abs() < 1e-6, "daily_pnl={}", fs.daily_pnl);
    }

    #[test]
    fn open_accepted_marked_to_market() {
        // Open BUY 100 @ fill 1300, mark 1100 => unrealized -20,000 => freeze.
        let entries = vec![entry(
            1,
            "RELIANCE",
            "BUY",
            SignalState::ManuallyAccepted,
            1305.0,
            Some(1300.0),
            100,
            None, // open: no realized pnl
        )];
        let mut marks = HashMap::new();
        marks.insert("RELIANCE".to_string(), 1100.0);
        let fs = evaluate(&entries, 1_000_000.0, 0.02, &marks);
        assert!((fs.daily_pnl - -20_000.0).abs() < 1e-6, "daily_pnl={}", fs.daily_pnl);
        assert!(fs.frozen, "open MTM loss should freeze at the threshold");

        // Same trade but SELL: a price drop is a GAIN, so no freeze.
        let sell = vec![entry(
            1,
            "RELIANCE",
            "SELL",
            SignalState::ManuallyAccepted,
            1305.0,
            Some(1300.0),
            100,
            None,
        )];
        let fs2 = evaluate(&sell, 1_000_000.0, 0.02, &marks);
        assert!((fs2.daily_pnl - 20_000.0).abs() < 1e-6, "daily_pnl={}", fs2.daily_pnl);
        assert!(!fs2.frozen);
    }

    #[test]
    fn open_accepted_without_mark_contributes_zero() {
        // No mark price for the symbol => unrealized term skipped (0).
        let entries = vec![entry(
            1,
            "WIPRO",
            "BUY",
            SignalState::ManuallyAccepted,
            500.0,
            Some(500.0),
            100,
            None,
        )];
        let marks = HashMap::new();
        let fs = evaluate(&entries, 1_000_000.0, 0.02, &marks);
        assert!((fs.daily_pnl - 0.0).abs() < 1e-6, "daily_pnl={}", fs.daily_pnl);
        assert!(!fs.frozen);
    }
}
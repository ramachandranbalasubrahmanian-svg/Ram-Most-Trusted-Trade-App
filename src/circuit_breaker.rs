//! Synthetic drawdown monitor → Signal Freeze.
//!
//! Sums the synthetic mark-to-market PnL of manually-accepted (tracked) trades.
//! If today's cumulative paper loss breaches −`threshold_pct` of the capital
//! pool, the returned [`FreezeState`] is `frozen` — the live broadcast halts and
//! the session locks out (manual-discipline protection). No orders are touched.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use std::collections::HashMap;

use crate::types::{FreezeState, JournalEntry};

/// Evaluate the freeze condition from the journal. `mark_prices` maps symbol →
/// latest price for marking open accepted positions (empty = realized PnL only).
pub fn evaluate(
    _entries: &[JournalEntry],
    capital_pool: f64,
    threshold_pct: f64,
    _mark_prices: &HashMap<String, f64>,
) -> FreezeState {
    FreezeState::active(capital_pool, threshold_pct)
}

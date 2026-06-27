//! Post-trade performance evaluation from the manual journal.
//!
//! Win rate, profit factor, Sharpe, max drawdown, cumulative equity curve, and
//! attribution by strategy and by sector — computed over completed (PnL-bearing)
//! journal trades.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use crate::types::{JournalEntry, PortfolioMetrics};

/// Compute the full performance matrix from journal entries.
pub fn compute(_entries: &[JournalEntry]) -> PortfolioMetrics {
    PortfolioMetrics {
        trades: 0,
        win_rate: 0.0,
        profit_factor: 0.0,
        sharpe: 0.0,
        max_drawdown: 0.0,
        total_pnl: 0.0,
        equity_curve: Vec::new(),
        by_strategy: Vec::new(),
        by_sector: Vec::new(),
    }
}

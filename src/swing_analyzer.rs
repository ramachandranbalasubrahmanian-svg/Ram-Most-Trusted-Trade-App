//! Multi-day swing scanner over the long-history daily archive (DuckDB).
//!
//! Two high-conviction setups:
//!   * Volume-delivery breakout — latest volume > 2.5× the 50-day average while
//!     price breaks structure (institutional accumulation).
//!   * Mean-reversion retest — quality names consolidating near the 200-day EMA
//!     or a major multi-year support zone.
//!
//! Produces the pre-market Swing Trades Catalog. (Uses DuckDB rather than Polars
//! — polars was dropped due to an upstream compile bug; DuckDB covers the
//! out-of-core daily scan.)
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use std::path::Path;

use crate::types::SwingCatalog;

/// Volume-breakout threshold: latest volume vs the 50-day average.
pub const VOL_BREAKOUT_MULT: f64 = 2.5;
/// EMA length for the mean-reversion retest.
pub const SWING_EMA_LEN: usize = 200;

/// Scan the daily archive for swing setups and return the catalog.
pub fn scan_swing(_root: &Path, symbols: &[String]) -> SwingCatalog {
    SwingCatalog {
        setups: Vec::new(),
        scanned: symbols.len(),
        built_ist: String::new(),
    }
}

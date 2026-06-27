//! Manual-interaction verification logger → DuckDB `manual_validation_journal_2026`.
//!
//! Every generated signal is logged with a lifecycle state
//! (Generated/Manually_Accepted/Manually_Rejected/Skipped), intended vs actual
//! fill (true manual slippage), and final PnL. A 15:45 IST routine exports the
//! day's rows to `data/journals/`.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use std::path::{Path, PathBuf};

use anyhow::Result;
use duckdb::Connection;

use crate::types::{JournalEntry, SignalState};

/// The DuckDB table name (frozen by spec).
pub const JOURNAL_TABLE: &str = "manual_validation_journal_2026";

/// Open (or create) the file-based journal DB and ensure the table exists.
pub fn open_journal(_path: &Path) -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {JOURNAL_TABLE} (id BIGINT, symbol VARCHAR);"
    ))?;
    Ok(conn)
}

/// Insert a freshly-generated signal; returns its new row id.
pub fn insert_entry(_conn: &Connection, _e: &JournalEntry) -> Result<i64> {
    Ok(0)
}

/// Update a row's lifecycle state and (optionally) the actual fill / exit price,
/// recomputing slippage and PnL when enough fields are present.
pub fn update_state(
    _conn: &Connection,
    _id: i64,
    _state: SignalState,
    _actual_fill: Option<f64>,
    _exit_price: Option<f64>,
    _now_ist: &str,
) -> Result<()> {
    Ok(())
}

/// All journal rows, newest-first.
pub fn all_entries(_conn: &Connection) -> Result<Vec<JournalEntry>> {
    Ok(Vec::new())
}

/// Export the journal to a timestamped CSV under `dir`; returns the path.
pub fn export_csv(_conn: &Connection, dir: &Path, _now_ist: &str) -> Result<PathBuf> {
    Ok(dir.join("manual_validation_journal_2026.csv"))
}

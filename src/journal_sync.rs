//! Manual-interaction verification logger → DuckDB `manual_validation_journal_2026`.
//!
//! Every generated signal is logged with a lifecycle state
//! (Generated/Manually_Accepted/Manually_Rejected/Skipped), intended vs actual
//! fill (true manual slippage), and final PnL. A 15:45 IST routine exports the
//! day's rows to `data/journals/`.
//!
//! Signals only — the journal tracks trades **synthetically**; nothing here ever
//! touches a broker. The file-based DuckDB store is the single source of truth
//! for post-trade portfolio analytics.

use std::path::{Path, PathBuf};

use anyhow::Result;
use duckdb::{Connection, params};

use crate::types::{JournalEntry, SignalState};

/// The DuckDB table name (frozen by spec).
pub const JOURNAL_TABLE: &str = "manual_validation_journal_2026";

/// Open (or create) the file-based journal DB and ensure the table + id sequence
/// exist. The caller is responsible for creating the parent directory.
pub fn open_journal(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {JOURNAL_TABLE} (\
            id BIGINT, \
            generated_ist VARCHAR, \
            entry_ist VARCHAR, \
            exit_ist VARCHAR, \
            instrument_token UBIGINT, \
            symbol VARCHAR, \
            direction VARCHAR, \
            strategy VARCHAR, \
            alpha_trigger VARCHAR, \
            intended_price DOUBLE, \
            actual_fill_price DOUBLE, \
            exit_price DOUBLE, \
            qty BIGINT, \
            state VARCHAR, \
            pnl DOUBLE, \
            slippage DOUBLE, \
            sector VARCHAR\
         ); \
         CREATE SEQUENCE IF NOT EXISTS journal_seq START 1;"
    ))?;
    Ok(conn)
}

/// Insert a freshly-generated signal; returns its new row id (from `journal_seq`).
pub fn insert_entry(conn: &Connection, e: &JournalEntry) -> Result<i64> {
    let id: i64 = conn.query_row("SELECT nextval('journal_seq')", [], |row| row.get(0))?;

    conn.execute(
        &format!(
            "INSERT INTO {JOURNAL_TABLE} (\
                id, generated_ist, entry_ist, exit_ist, instrument_token, symbol, \
                direction, strategy, alpha_trigger, intended_price, actual_fill_price, \
                exit_price, qty, state, pnl, slippage, sector\
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ),
        params![
            id,
            e.generated_ist,
            e.entry_ist,
            e.exit_ist,
            e.instrument_token as u64,
            e.symbol,
            e.direction,
            e.strategy,
            e.alpha_trigger,
            e.intended_price,
            e.actual_fill_price,
            e.exit_price,
            e.qty,
            e.state,
            e.pnl,
            e.slippage,
            e.sector,
        ],
    )?;

    Ok(id)
}

/// Update a row's lifecycle state and (optionally) the actual fill / exit price,
/// recomputing direction-signed slippage and synthetic PnL when enough fields
/// are present.
///
/// * slippage — BUY: `actual − intended`; SELL: `intended − actual`.
/// * pnl — `qty · (exit − entry_basis) · dir`, where
///   `entry_basis = coalesce(actual_fill_price, intended_price)` and
///   `dir = +1` for BUY, `-1` for SELL.
pub fn update_state(
    conn: &Connection,
    id: i64,
    state: SignalState,
    actual_fill: Option<f64>,
    exit_price: Option<f64>,
    now_ist: &str,
) -> Result<()> {
    // Read the fields we need to recompute slippage / pnl.
    let (direction, intended_price, qty, cur_fill): (String, f64, i64, Option<f64>) = conn
        .query_row(
            &format!(
                "SELECT direction, intended_price, qty, actual_fill_price \
                 FROM {JOURNAL_TABLE} WHERE id = ?"
            ),
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<f64>>(3)?,
                ))
            },
        )?;

    let dir = if direction == "SELL" { -1.0 } else { 1.0 };

    // Effective fill after this update (new fill overrides any stored one).
    let eff_fill = actual_fill.or(cur_fill);
    // entry_basis = coalesce(actual_fill_price, intended_price).
    let entry_basis = eff_fill.unwrap_or(intended_price);

    // Compute the new slippage only when a fresh fill is supplied.
    let new_slippage: Option<f64> = actual_fill.map(|fill| {
        if dir < 0.0 {
            intended_price - fill
        } else {
            fill - intended_price
        }
    });

    // Compute pnl only when an exit price is supplied.
    let new_pnl: Option<f64> =
        exit_price.map(|exit| qty as f64 * (exit - entry_basis) * dir);

    // Always update the state. Conditionally update the fill/exit blocks.
    conn.execute(
        &format!("UPDATE {JOURNAL_TABLE} SET state = ? WHERE id = ?"),
        params![state.as_str(), id],
    )?;

    if let Some(fill) = actual_fill {
        conn.execute(
            &format!(
                "UPDATE {JOURNAL_TABLE} \
                 SET actual_fill_price = ?, entry_ist = ?, slippage = ? \
                 WHERE id = ?"
            ),
            params![fill, now_ist, new_slippage, id],
        )?;
    }

    if let Some(exit) = exit_price {
        conn.execute(
            &format!(
                "UPDATE {JOURNAL_TABLE} \
                 SET exit_price = ?, exit_ist = ?, pnl = ? \
                 WHERE id = ?"
            ),
            params![exit, now_ist, new_pnl, id],
        )?;
    }

    Ok(())
}

/// All journal rows, newest-first. NULL DOUBLE/VARCHAR columns map to `None`.
pub fn all_entries(conn: &Connection) -> Result<Vec<JournalEntry>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, generated_ist, entry_ist, exit_ist, instrument_token, symbol, \
                direction, strategy, alpha_trigger, intended_price, actual_fill_price, \
                exit_price, qty, state, pnl, slippage, sector \
         FROM {JOURNAL_TABLE} ORDER BY id DESC"
    ))?;

    let rows = stmt.query_map([], |row| {
        Ok(JournalEntry {
            id: row.get::<_, i64>(0)?,
            generated_ist: row.get::<_, String>(1)?,
            entry_ist: row.get::<_, Option<String>>(2)?,
            exit_ist: row.get::<_, Option<String>>(3)?,
            instrument_token: row.get::<_, u64>(4)? as u32,
            symbol: row.get::<_, String>(5)?,
            direction: row.get::<_, String>(6)?,
            strategy: row.get::<_, String>(7)?,
            alpha_trigger: row.get::<_, String>(8)?,
            intended_price: row.get::<_, f64>(9)?,
            actual_fill_price: row.get::<_, Option<f64>>(10)?,
            exit_price: row.get::<_, Option<f64>>(11)?,
            qty: row.get::<_, i64>(12)?,
            state: row.get::<_, String>(13)?,
            pnl: row.get::<_, Option<f64>>(14)?,
            slippage: row.get::<_, Option<f64>>(15)?,
            sector: row.get::<_, Option<String>>(16)?,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Delete one journal row by id; returns true if a row was removed.
pub fn delete_entry(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute(&format!("DELETE FROM {JOURNAL_TABLE} WHERE id = ?"), params![id])?;
    Ok(n > 0)
}

/// Delete every journal row; returns the number removed. Used by the trade-journal
/// page's "replace / clear" action. Record-only — touches no broker, no signal.
pub fn clear_all(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {JOURNAL_TABLE}"), [], |r| r.get(0))?;
    conn.execute_batch(&format!("DELETE FROM {JOURNAL_TABLE}"))?;
    Ok(n as usize)
}

/// Export the journal to a timestamped CSV under `dir`; returns the file path.
/// The date suffix is the `YYYY-MM-DD` prefix of `now_ist`.
pub fn export_csv(conn: &Connection, dir: &Path, now_ist: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;

    let date = now_ist.get(0..10).unwrap_or(now_ist);
    let path = dir.join(format!("{JOURNAL_TABLE}_{date}.csv"));

    let entries = all_entries(conn)?;

    let mut buf = String::new();
    buf.push_str(
        "id,generated_ist,entry_ist,exit_ist,instrument_token,symbol,direction,\
         strategy,alpha_trigger,intended_price,actual_fill_price,exit_price,qty,\
         state,pnl,slippage,sector\n",
    );

    for e in &entries {
        let fields: [String; 17] = [
            e.id.to_string(),
            csv_quote(&e.generated_ist),
            csv_quote(e.entry_ist.as_deref().unwrap_or("")),
            csv_quote(e.exit_ist.as_deref().unwrap_or("")),
            e.instrument_token.to_string(),
            csv_quote(&e.symbol),
            csv_quote(&e.direction),
            csv_quote(&e.strategy),
            csv_quote(&e.alpha_trigger),
            opt_num(Some(e.intended_price)),
            opt_num(e.actual_fill_price),
            opt_num(e.exit_price),
            e.qty.to_string(),
            csv_quote(&e.state),
            opt_num(e.pnl),
            opt_num(e.slippage),
            csv_quote(e.sector.as_deref().unwrap_or("")),
        ];
        buf.push_str(&fields.join(","));
        buf.push('\n');
    }

    std::fs::write(&path, buf)?;
    Ok(path)
}

/// CSV-escape a VARCHAR field: wrap in double quotes and double any embedded
/// quote, so commas / quotes / newlines round-trip safely.
fn csv_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Render an optional number for CSV: empty cell for `None`.
fn opt_num(v: Option<f64>) -> String {
    match v {
        Some(x) => x.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ram_istp_journal_{tag}_{nanos}.duckdb"))
    }

    fn sample_entry() -> JournalEntry {
        JournalEntry {
            id: 0,
            generated_ist: "2026-06-28 09:20:00".to_string(),
            entry_ist: None,
            exit_ist: None,
            instrument_token: 738561,
            symbol: "RELIANCE".to_string(),
            direction: "BUY".to_string(),
            strategy: "vwap_trend".to_string(),
            alpha_trigger: "VWAP crossover up".to_string(),
            intended_price: 100.0,
            actual_fill_price: None,
            exit_price: None,
            qty: 10,
            state: SignalState::Generated.as_str().to_string(),
            pnl: None,
            slippage: None,
            sector: Some("Energy".to_string()),
        }
    }

    #[test]
    fn lifecycle_accept_fill_then_exit_sets_pnl_and_slippage() {
        let path = temp_db_path("lifecycle");
        let conn = open_journal(&path).expect("open journal");

        let id = insert_entry(&conn, &sample_entry()).expect("insert");
        assert!(id >= 1, "sequence id should start at 1");

        // Accept with an actual fill of 100.5 → BUY slippage = +0.5.
        update_state(
            &conn,
            id,
            SignalState::ManuallyAccepted,
            Some(100.5),
            None,
            "2026-06-28 09:25:00",
        )
        .expect("accept + fill");

        // Close with an exit of 110.0 → pnl = 10 * (110 - 100.5) * 1 = 95.0.
        update_state(
            &conn,
            id,
            SignalState::ManuallyAccepted,
            None,
            Some(110.0),
            "2026-06-28 14:30:00",
        )
        .expect("exit");

        let entries = all_entries(&conn).expect("read all");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];

        assert_eq!(e.id, id);
        assert_eq!(e.state, SignalState::ManuallyAccepted.as_str());
        assert_eq!(e.entry_ist.as_deref(), Some("2026-06-28 09:25:00"));
        assert_eq!(e.exit_ist.as_deref(), Some("2026-06-28 14:30:00"));

        let slippage = e.slippage.expect("slippage non-null");
        assert!((slippage - 0.5).abs() < 1e-9, "slippage={slippage}");

        let pnl = e.pnl.expect("pnl non-null");
        assert!((pnl - 95.0).abs() < 1e-9, "pnl={pnl}");

        // Export should produce a dated, readable CSV.
        let out_dir = std::env::temp_dir().join(format!(
            "ram_istp_journal_csv_{}",
            std::process::id()
        ));
        let csv = export_csv(&conn, &out_dir, "2026-06-28 15:45:00").expect("export csv");
        assert!(csv.exists());
        assert!(
            csv.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("2026-06-28"))
                .unwrap_or(false),
            "csv name should carry the date: {csv:?}"
        );

        drop(conn);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    #[test]
    fn sell_slippage_is_direction_signed() {
        let path = temp_db_path("sell");
        let conn = open_journal(&path).expect("open journal");

        let mut entry = sample_entry();
        entry.direction = "SELL".to_string();
        entry.state = SignalState::Generated.as_str().to_string();
        let id = insert_entry(&conn, &entry).expect("insert");

        // SELL filled below intended (99.0 vs 100.0) → slippage = intended - actual = +1.0.
        update_state(
            &conn,
            id,
            SignalState::ManuallyAccepted,
            Some(99.0),
            None,
            "2026-06-28 09:25:00",
        )
        .expect("accept + fill");

        let e = all_entries(&conn)
            .expect("read")
            .into_iter()
            .find(|e| e.id == id)
            .expect("row");
        let slippage = e.slippage.expect("slippage");
        assert!((slippage - 1.0).abs() < 1e-9, "slippage={slippage}");

        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}

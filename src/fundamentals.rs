//! Display-only fundamentals context for a symbol.
//!
//! FIREWALLED: imports only `storage_kernel` (+ serde/duckdb). These numbers
//! NEVER enter `eligible()`, Confidence, ranking, or position sizing — the edge
//! is price/structure-only. This is purely a "what kind of company is this"
//! context panel next to an edge or a holding.
//!
//! Source: `fundamentals.parquet`, materialized by `build_fundamentals.py` from
//! the indianapi `/stock` snapshots. ~270 covered names today; everything else
//! reports "no fundamentals on file" — never fabricated.

use serde::{Deserialize, Serialize};

use crate::storage_kernel;

/// One symbol's fundamentals snapshot. Every ratio is optional (None ⇒ the source
/// did not carry it — shown as "—", never zero-filled).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Fundamentals {
    pub symbol: String,
    pub company: Option<String>,
    /// P/E (TTM, basic excl. extraordinary).
    pub pe: Option<f64>,
    /// Return on equity %, TTM.
    pub roe: Option<f64>,
    pub debt_to_equity: Option<f64>,
    pub peg: Option<f64>,
    /// Revenue growth %, 5-year.
    pub rev_growth_5y: Option<f64>,
    /// EPS growth %, 5-year.
    pub eps_growth_5y: Option<f64>,
    /// Net profit margin %, TTM.
    pub profit_margin: Option<f64>,
    pub dividend_yield: Option<f64>,
    pub price_to_book: Option<f64>,
    /// Promoter holding %, latest quarter.
    pub promoter_pct: Option<f64>,
    /// Market cap in ₹ crore.
    pub market_cap_cr: Option<f64>,
    /// The snapshot date string from the source (e.g. "25 Jun 2026").
    pub as_of: Option<String>,
}

/// Sanitize an optional float: drop non-finite (NaN/Inf from parquet) to None.
fn fin(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

/// Load one symbol's fundamentals from `fundamentals.parquet`. Returns
/// `Ok(None)` when the file is absent or the symbol is not covered (not an
/// error — most of the universe is uncovered). Errors only on a malformed file.
pub fn load_symbol(
    conn: &duckdb::Connection,
    root: &std::path::Path,
    symbol: &str,
) -> anyhow::Result<Option<Fundamentals>> {
    let path = root.join("fundamentals.parquet");
    if !path.exists() {
        return Ok(None);
    }
    let sql = format!(
        "SELECT symbol, company, pe, roe, debt_to_equity, peg, rev_growth_5y, \
                eps_growth_5y, profit_margin, dividend_yield, price_to_book, \
                promoter_pct, market_cap_cr, as_of \
         FROM read_parquet({}) WHERE upper(symbol) = upper(?) LIMIT 1",
        storage_kernel::quote_path(&path)
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([symbol], |row| {
        Ok(Fundamentals {
            symbol: row.get::<_, String>(0)?,
            company: row.get::<_, Option<String>>(1)?,
            pe: fin(row.get::<_, Option<f64>>(2)?),
            roe: fin(row.get::<_, Option<f64>>(3)?),
            debt_to_equity: fin(row.get::<_, Option<f64>>(4)?),
            peg: fin(row.get::<_, Option<f64>>(5)?),
            rev_growth_5y: fin(row.get::<_, Option<f64>>(6)?),
            eps_growth_5y: fin(row.get::<_, Option<f64>>(7)?),
            profit_margin: fin(row.get::<_, Option<f64>>(8)?),
            dividend_yield: fin(row.get::<_, Option<f64>>(9)?),
            price_to_book: fin(row.get::<_, Option<f64>>(10)?),
            promoter_pct: fin(row.get::<_, Option<f64>>(11)?),
            market_cap_cr: fin(row.get::<_, Option<f64>>(12)?),
            as_of: row.get::<_, Option<String>>(13)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Number of symbols covered by `fundamentals.parquet` (0 if absent). For an
/// honest coverage caption.
pub fn coverage(conn: &duckdb::Connection, root: &std::path::Path) -> usize {
    let path = root.join("fundamentals.parquet");
    if !path.exists() {
        return 0;
    }
    let sql = format!(
        "SELECT COUNT(*) FROM read_parquet({})",
        storage_kernel::quote_path(&path)
    );
    conn.prepare(&sql)
        .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fin_drops_nonfinite() {
        assert_eq!(fin(Some(1.5)), Some(1.5));
        assert_eq!(fin(Some(f64::NAN)), None);
        assert_eq!(fin(Some(f64::INFINITY)), None);
        assert_eq!(fin(None), None);
    }

    #[test]
    fn missing_file_is_none_not_error() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let root = std::path::Path::new("/nonexistent/path/xyz");
        assert!(load_symbol(&conn, root, "RELIANCE").unwrap().is_none());
        assert_eq!(coverage(&conn, root), 0);
    }
}

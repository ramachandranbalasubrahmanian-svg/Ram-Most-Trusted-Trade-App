//! Sector rotational momentum — is a stock leading or lagging ITS OWN sector?
//!
//! FIREWALLED: imports only `storage_kernel` (+ serde/duckdb). It NEVER feeds
//! `eligible()`, Confidence, ranking, or sizing — it is display-only context.
//! Until now relative strength was only ever computed vs the broad NIFTY 50; the
//! per-sector indices (NIFTYBANK / NIFTYIT / …) sat unused on disk. This maps a
//! stock to its sector index and reports `(stock % move) − (sector % move)` — the
//! leader/laggard read the user asked for.
//!
//! Source: `index_daily/<INDEX>.parquet` + `nse_daily_all.parquet`, both EOD
//! daily. So during a live session this reflects the latest CLOSE, not the live
//! intraday tick (the sector indices aren't on the live feed) — every figure is
//! stamped `as_of` and never shown as "now". Sectors without a clean NSE sector
//! index (Industrials, Utilities, …) honestly fall back to NIFTY 50, labelled.

use serde::{Deserialize, Serialize};

use crate::storage_kernel;

/// One symbol's sector-relative momentum. Display-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SectorMomentum {
    pub available: bool,
    pub symbol: String,
    /// GICS-style sector from metadata (e.g. "Financial Services"); "" if unknown.
    pub sector: String,
    /// The benchmark index actually used (e.g. "NIFTYBANK", or "NIFTY50" fallback).
    pub index_name: String,
    /// True when we fell back to the broad index (no clean sector index).
    pub broad_fallback: bool,
    pub as_of: String,
    /// Latest-session % move of the stock and its benchmark, and the difference.
    pub stock_chg_1d: f64,
    pub index_chg_1d: f64,
    pub rs_1d: f64,
    /// 5- and 20-session relative strength (stock return − index return), %.
    pub rs_5d: f64,
    pub rs_20d: f64,
    /// "leader" | "in-line" | "laggard" | "unknown".
    pub verdict: String,
    pub note: String,
}

/// Map a metadata sector to its NSE sector index file (without extension), or
/// `None` ⇒ no clean sector index (caller falls back to the broad NIFTY 50). Pure.
fn sector_to_index(sector: &str) -> Option<&'static str> {
    match sector.trim() {
        "Financial Services" => Some("NIFTYBANK"),
        "Technology" => Some("NIFTYIT"),
        "Energy" => Some("NIFTYENERGY"),
        "Consumer Defensive" => Some("NIFTYFMCG"),
        "Basic Materials" => Some("NIFTYMETAL"),
        "Healthcare" => Some("NIFTYPHARMA"),
        "Real Estate" => Some("NIFTYREALTY"),
        "Consumer Cyclical" => Some("NIFTYAUTO"),
        // Industrials / Utilities / Communication Services / Unknown ⇒ no clean
        // sector index in the archive → broad benchmark.
        _ => None,
    }
}

/// n-session % return (last vs n-back). None when too short / non-positive base. Pure.
fn ret_pct(closes: &[f64], n: usize) -> Option<f64> {
    if n == 0 || closes.len() <= n {
        return None;
    }
    let last = closes[closes.len() - 1];
    let base = closes[closes.len() - 1 - n];
    if base > 0.0 && last.is_finite() && base.is_finite() {
        Some((last / base - 1.0) * 100.0)
    } else {
        None
    }
}

/// Leader/laggard from 5-session relative strength. Pure.
fn verdict(rs_5d: f64) -> &'static str {
    if rs_5d >= 1.5 {
        "leader"
    } else if rs_5d <= -1.5 {
        "laggard"
    } else {
        "in-line"
    }
}

fn load_index_closes(conn: &duckdb::Connection, root: &std::path::Path, index_file: &str) -> Vec<f64> {
    let path = root.join("index_daily").join(format!("{index_file}.parquet"));
    if !path.exists() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT close FROM read_parquet({}) ORDER BY date",
        storage_kernel::quote_path(&path)
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, f64>(0)) {
            out = rows.flatten().collect();
        }
    }
    out
}

fn load_stock_closes(conn: &duckdb::Connection, root: &std::path::Path, symbol: &str) -> Vec<f64> {
    let path = root.join("nse_daily_all.parquet");
    if !path.exists() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT close FROM read_parquet({}) WHERE upper(symbol) = upper(?) ORDER BY date",
        storage_kernel::quote_path(&path)
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([symbol], |r| r.get::<_, f64>(0)) {
            out = rows.flatten().collect();
        }
    }
    out
}

fn sector_of(conn: &duckdb::Connection, root: &std::path::Path, symbol: &str) -> Option<String> {
    let path = root.join("symbol_metadata.parquet");
    if !path.exists() {
        return None;
    }
    let sql = format!(
        "SELECT sector FROM read_parquet({}) WHERE upper(symbol) = upper(?) LIMIT 1",
        storage_kernel::quote_path(&path)
    );
    conn.prepare(&sql)
        .ok()?
        .query_row([symbol], |r| r.get::<_, Option<String>>(0))
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

/// As-of date (latest) from nse_daily_all for the symbol. Best-effort.
fn last_date(conn: &duckdb::Connection, root: &std::path::Path, symbol: &str) -> String {
    let path = root.join("nse_daily_all.parquet");
    let sql = format!(
        "SELECT CAST(MAX(date) AS DATE)::VARCHAR FROM read_parquet({}) WHERE upper(symbol) = upper(?)",
        storage_kernel::quote_path(&path)
    );
    conn.prepare(&sql)
        .ok()
        .and_then(|mut s| s.query_row([symbol], |r| r.get::<_, Option<String>>(0)).ok())
        .flatten()
        .unwrap_or_default()
}

/// Compute one symbol's sector-relative momentum. On-demand (a few small daily
/// reads). Display-only; honest "unavailable" when data is missing.
pub fn compute(conn: &duckdb::Connection, root: &std::path::Path, symbol: &str) -> SectorMomentum {
    let sym = symbol.trim().to_uppercase();
    let sector = sector_of(conn, root, &sym).unwrap_or_default();
    let (index_name, broad_fallback) = match sector_to_index(&sector) {
        Some(ix) => (ix.to_string(), false),
        None => ("NIFTY50".to_string(), true),
    };

    let stock = load_stock_closes(conn, root, &sym);
    let index = load_index_closes(conn, root, &index_name);
    if stock.len() < 2 || index.len() < 2 {
        return SectorMomentum {
            available: false,
            symbol: sym,
            sector,
            index_name,
            broad_fallback,
            verdict: "unknown".to_string(),
            note: "Sector momentum unavailable — missing daily data for the stock or its index.".to_string(),
            ..Default::default()
        };
    }

    let stock_chg_1d = ret_pct(&stock, 1).unwrap_or(0.0);
    let index_chg_1d = ret_pct(&index, 1).unwrap_or(0.0);
    let rs_1d = stock_chg_1d - index_chg_1d;
    let rs_5d = ret_pct(&stock, 5).unwrap_or(0.0) - ret_pct(&index, 5).unwrap_or(0.0);
    let rs_20d = ret_pct(&stock, 20).unwrap_or(0.0) - ret_pct(&index, 20).unwrap_or(0.0);
    let v = verdict(rs_5d);
    let as_of = last_date(conn, root, &sym);

    let bench = if broad_fallback {
        format!("NIFTY50 (no clean sector index for {})", if sector.is_empty() { "this name" } else { sector.as_str() })
    } else {
        format!("{index_name} ({sector})")
    };
    let note = format!(
        "{sym} vs {bench}: latest session {stock_chg_1d:+.2}% vs index {index_chg_1d:+.2}% ({rs_1d:+.2} RS). 5-session RS {rs_5d:+.1}%, 20-session {rs_20d:+.1}% — {v}. EOD daily, not live intraday.",
    );

    SectorMomentum {
        available: true,
        symbol: sym,
        sector,
        index_name,
        broad_fallback,
        as_of,
        stock_chg_1d,
        index_chg_1d,
        rs_1d,
        rs_5d,
        rs_20d,
        verdict: v.to_string(),
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sector_maps_to_clean_index_or_none() {
        assert_eq!(sector_to_index("Financial Services"), Some("NIFTYBANK"));
        assert_eq!(sector_to_index("Technology"), Some("NIFTYIT"));
        assert_eq!(sector_to_index("Healthcare"), Some("NIFTYPHARMA"));
        assert_eq!(sector_to_index("Industrials"), None); // → broad fallback
        assert_eq!(sector_to_index(""), None);
    }

    #[test]
    fn ret_pct_basic() {
        let c = vec![100.0, 101.0, 102.0, 110.0]; // last 110, 1-back 102
        assert!((ret_pct(&c, 1).unwrap() - (110.0 / 102.0 - 1.0) * 100.0).abs() < 1e-9);
        assert!((ret_pct(&c, 3).unwrap() - 10.0).abs() < 1e-9); // 110/100
        assert!(ret_pct(&c, 4).is_none()); // not enough history
    }

    #[test]
    fn verdict_bands() {
        assert_eq!(verdict(3.0), "leader");
        assert_eq!(verdict(-3.0), "laggard");
        assert_eq!(verdict(0.5), "in-line");
    }
}

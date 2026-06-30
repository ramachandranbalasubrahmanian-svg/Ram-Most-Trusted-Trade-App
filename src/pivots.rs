//! Classic floor-trader pivot levels (P / R1·S1 / R2·S2 / R3·S3) for a symbol's
//! NEXT session, from its most recent completed day's High/Low/Close.
//!
//! FIREWALLED: imports only `storage_kernel` (+ serde/duckdb). Display-only —
//! these are context support/resistance levels, NEVER inputs to `eligible()`,
//! Confidence, ranking, or sizing. The backtested `cpr_pivot` strategy already
//! signals on the central pivot P; this just surfaces the full S/R ladder as
//! "where might it go today" context (the spec's R1/S1 targets), which was absent.
//!
//! Source: the latest row of `nse_daily_all.parquet` (EOD daily), stamped
//! `as_of`. Honest "unavailable" when the symbol has no daily data.

use serde::{Deserialize, Serialize};

use crate::storage_kernel;

/// Classic pivot ladder for a session. Display-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PivotLevels {
    pub available: bool,
    pub symbol: String,
    /// Date of the prior session the levels are derived from.
    pub as_of: String,
    pub prior_high: f64,
    pub prior_low: f64,
    pub prior_close: f64,
    pub p: f64,
    pub r1: f64,
    pub s1: f64,
    pub r2: f64,
    pub s2: f64,
    pub r3: f64,
    pub s3: f64,
    pub note: String,
}

/// Classic floor-trader pivots from prior (H, L, C). Pure.
///   P  = (H+L+C)/3
///   R1 = 2P − L      S1 = 2P − H
///   R2 = P + (H−L)   S2 = P − (H−L)
///   R3 = H + 2(P−L)  S3 = L − 2(H−P)
fn classic_pivots(h: f64, l: f64, c: f64) -> (f64, f64, f64, f64, f64, f64, f64) {
    let p = (h + l + c) / 3.0;
    let range = h - l;
    let r1 = 2.0 * p - l;
    let s1 = 2.0 * p - h;
    let r2 = p + range;
    let s2 = p - range;
    let r3 = h + 2.0 * (p - l);
    let s3 = l - 2.0 * (h - p);
    (p, r1, s1, r2, s2, r3, s3)
}

/// Most recent (date, high, low, close) for a symbol from nse_daily_all. Best-effort.
fn last_hlc(
    conn: &duckdb::Connection,
    root: &std::path::Path,
    symbol: &str,
) -> Option<(String, f64, f64, f64)> {
    let path = root.join("nse_daily_all.parquet");
    if !path.exists() {
        return None;
    }
    // Pick the latest row with a USABLE OHLC. Yahoo's most-recent daily bar can
    // land with a NaN/null close (provisional row) while OHL are present — skip
    // those and fall back to the last fully-valid prior session, rather than
    // reporting "unavailable". (NaN is not NULL in DuckDB, so guard with isnan.)
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR, high, low, close FROM read_parquet({}) \
         WHERE upper(symbol) = upper(?) \
           AND high IS NOT NULL AND low IS NOT NULL AND close IS NOT NULL \
           AND NOT isnan(high) AND NOT isnan(low) AND NOT isnan(close) \
           AND close > 0 AND high > 0 AND low > 0 \
         ORDER BY date DESC LIMIT 1",
        storage_kernel::quote_path(&path)
    );
    conn.prepare(&sql).ok()?.query_row([symbol], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, f64>(1)?,
            r.get::<_, f64>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })
    .ok()
}

/// Compute the pivot ladder for one symbol's next session. On-demand; display-only.
pub fn compute(conn: &duckdb::Connection, root: &std::path::Path, symbol: &str) -> PivotLevels {
    let sym = symbol.trim().to_uppercase();
    let (as_of, h, l, c) = match last_hlc(conn, root, &sym) {
        Some(v) if v.1 > 0.0 && v.2 > 0.0 && v.3 > 0.0 && v.1 >= v.2 => v,
        _ => {
            return PivotLevels {
                available: false,
                symbol: sym,
                note: "Pivots unavailable — no usable prior-day OHLC on file.".to_string(),
                ..Default::default()
            };
        }
    };
    let (p, r1, s1, r2, s2, r3, s3) = classic_pivots(h, l, c);
    let note = format!(
        "Pivots for the next session off {as_of} (H {h:.2} / L {l:.2} / C {c:.2}). Above P {p:.2} → bias up toward R1 {r1:.2}; below P → toward S1 {s1:.2}. Display-only context.",
    );
    PivotLevels {
        available: true,
        symbol: sym,
        as_of,
        prior_high: h,
        prior_low: l,
        prior_close: c,
        p,
        r1,
        s1,
        r2,
        s2,
        r3,
        s3,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_pivots_match_formula() {
        // H=110 L=90 C=100 ⇒ P=100, R1=2*100-90=110, S1=2*100-110=90,
        // R2=100+20=120, S2=80, R3=110+2*(100-90)=130, S3=90-2*(110-100)=70.
        let (p, r1, s1, r2, s2, r3, s3) = classic_pivots(110.0, 90.0, 100.0);
        assert!((p - 100.0).abs() < 1e-9);
        assert!((r1 - 110.0).abs() < 1e-9 && (s1 - 90.0).abs() < 1e-9);
        assert!((r2 - 120.0).abs() < 1e-9 && (s2 - 80.0).abs() < 1e-9);
        assert!((r3 - 130.0).abs() < 1e-9 && (s3 - 70.0).abs() < 1e-9);
        // Ladder is ordered S3<S2<S1<P<R1<R2<R3.
        assert!(s3 < s2 && s2 < s1 && s1 < p && p < r1 && r1 < r2 && r2 < r3);
    }
}

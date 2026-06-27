//! NIFTY-regime conditioning for the confidence pipeline.
//!
//! A real edge should survive in BOTH market regimes. We classify each NIFTY-50
//! trading day as up/down (close vs its 20-day SMA), then check whether a
//! setup's trades are profitable in both regimes.
//!
//! Two pieces:
//!   * [`nifty_regime_map`] — reads the index daily parquet and labels every
//!     trading day up/down using a trailing 20-day SMA (with a price-direction
//!     warmup fallback for the first 19 days). Fully resilient: any missing
//!     file, query failure, or parse error yields whatever has been collected so
//!     far (possibly empty). It never panics.
//!   * [`regime_consistency`] — given the R-multiples a setup earned on up-days
//!     and down-days, decides whether the edge holds in both regimes.

use std::collections::HashMap;

/// Trailing SMA window (trading days) used to label the NIFTY regime.
const REGIME_SMA_WINDOW: usize = 20;

/// Map each NIFTY-50 trading day (`"YYYY-MM-DD"`) to an up/down regime flag:
/// `true` = up (close ≥ 20-day SMA), `false` = down. Empty if the index file
/// (`<data_root>/index_daily/NIFTY50.parquet`) is unavailable — never panics.
///
/// Warmup: for the first 19 days there is no full 20-day SMA, so we fall back to
/// raw price direction — `up = close[i] >= close[i-1]` (and `up = true` for the
/// very first day). From day index 19 onward we use the trailing 20-day SMA.
pub fn nifty_regime_map(conn: &duckdb::Connection) -> HashMap<String, bool> {
    let mut map: HashMap<String, bool> = HashMap::new();

    let path = crate::config::data_root()
        .join("index_daily")
        .join("NIFTY50.parquet");
    if !path.exists() {
        return map;
    }

    // Quote the path exactly like `storage_kernel::quote_path` (escape embedded
    // single quotes, wrap in single quotes).
    let qp = crate::storage_kernel::quote_path(&path);
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS day, close \
         FROM read_parquet({qp}) ORDER BY date"
    );

    // Pull (day, close) oldest-first. Any failure → return what we have (empty).
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return map,
    };
    let rows = match stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return map,
    };

    let mut days: Vec<String> = Vec::new();
    let mut closes: Vec<f64> = Vec::new();
    for r in rows {
        match r {
            Ok((day, close)) => {
                days.push(day);
                closes.push(close);
            }
            // A bad row mid-stream: stop and use whatever parsed cleanly so far.
            Err(_) => break,
        }
    }

    // Label each day. Trailing running sum for an O(n) SMA.
    let mut window_sum = 0.0_f64;
    for i in 0..closes.len() {
        window_sum += closes[i];
        if i >= REGIME_SMA_WINDOW {
            window_sum -= closes[i - REGIME_SMA_WINDOW];
        }

        let up = if i >= REGIME_SMA_WINDOW - 1 {
            // Full 20-day SMA available (window currently holds closes[i-19..=i]).
            let sma = window_sum / REGIME_SMA_WINDOW as f64;
            closes[i] >= sma
        } else if i >= 1 {
            // Warmup: raw price direction vs the prior close.
            closes[i] >= closes[i - 1]
        } else {
            // Very first day: default to up.
            true
        };

        map.insert(days[i].clone(), up);
    }

    map
}

/// Does the edge hold in BOTH regimes? Given the R-multiples earned on up-days
/// and on down-days:
///   * `Some(true)`  — both sides have mean R > 0 with ≥ `min_each` trades,
///   * `Some(false)` — one side has ≥ `min_each` trades but mean R ≤ 0,
///   * `None`        — not enough trades in one/both regimes to judge.
pub fn regime_consistency(rs_up: &[f64], rs_down: &[f64], min_each: usize) -> Option<bool> {
    let up_ok = rs_up.len() >= min_each;
    let down_ok = rs_down.len() >= min_each;

    if up_ok && down_ok {
        Some(mean(rs_up) > 0.0 && mean(rs_down) > 0.0)
    } else if up_ok && mean(rs_up) <= 0.0 {
        Some(false)
    } else if down_ok && mean(rs_down) <= 0.0 {
        Some(false)
    } else {
        None
    }
}

/// Arithmetic mean of a slice (0.0 for an empty slice).
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_regimes_positive_is_consistent() {
        let up = [1.0, 0.5, 2.0, -0.5];
        let down = [0.2, 0.1, 0.3];
        // Both means > 0 and both sides have >= min_each trades.
        assert_eq!(regime_consistency(&up, &down, 3), Some(true));
    }

    #[test]
    fn one_regime_nonpositive_with_enough_is_inconsistent() {
        let up = [1.0, 0.5, 2.0];
        let down = [-1.0, -0.5, -0.2]; // mean <= 0, enough trades
        assert_eq!(regime_consistency(&up, &down, 3), Some(false));

        // Symmetric: the up side is the losing regime.
        let up2 = [-1.0, 0.0, -0.5];
        let down2 = [1.0, 1.0, 1.0];
        assert_eq!(regime_consistency(&up2, &down2, 3), Some(false));

        // Exactly-zero mean counts as non-positive (<= 0).
        let up3 = [1.0, -1.0, 0.0];
        let down3 = [2.0, 2.0, 2.0];
        assert_eq!(regime_consistency(&up3, &down3, 3), Some(false));
    }

    #[test]
    fn too_few_trades_is_none() {
        let up = [1.0, 2.0];
        let down = [0.5, 0.5];
        // Neither side reaches min_each = 3.
        assert_eq!(regime_consistency(&up, &down, 3), None);

        // One side short, the other positive with enough → still inconclusive.
        let up2 = [1.0, 1.0, 1.0, 1.0];
        let down2 = [0.5]; // only 1 trade, positive
        assert_eq!(regime_consistency(&up2, &down2, 3), None);

        // Empty both sides.
        let empty: [f64; 0] = [];
        assert_eq!(regime_consistency(&empty, &empty, 1), None);
    }

    #[test]
    fn mean_handles_empty_and_values() {
        let empty: [f64; 0] = [];
        assert_eq!(mean(&empty), 0.0);
        assert!((mean(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-12);
    }
}

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
//! The macro source is the ~30-year Yahoo `daily/` set (`config::data_root()/
//! daily/<SYMBOL>.parquet`); if that file is absent we fall back to the Kite
//! `1day/` set via [`config::parquet_path`]. All heavy numeric reductions
//! (200-EMA seeding, Wilder ATR) run in plain Rust so they are unit-testable.
//!
//! Resilient: any per-symbol read or compute error skips that symbol — a single
//! bad file can never sink the scan or panic.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use duckdb::Connection;
use rayon::prelude::*;

use crate::config::{self, Timeframe};
use crate::storage_kernel::open_conn;
use crate::types::{SwingCatalog, SwingSetup};

/// Volume-breakout threshold: latest volume vs the 50-day average.
pub const VOL_BREAKOUT_MULT: f64 = 2.5;
/// EMA length for the mean-reversion retest.
pub const SWING_EMA_LEN: usize = 200;

/// Minimum daily bars required to compute the full swing baseline (200-EMA seed
/// + a 50-day volume average + structure look-backs).
const MIN_DAILY_BARS: usize = 220;
/// Volume average look-back (days), excluding today's bar.
const VOL_AVG_LEN: usize = 50;
/// Structure look-back for support / resistance (≈ 1 trading year).
const STRUCTURE_LEN: usize = 252;
/// Breakout reference window: prior N-day high (excluding today).
const BREAKOUT_LEN: usize = 20;
/// Wilder ATR period on the daily series.
const ATR_PERIOD: usize = 14;
/// Mean-reversion proximity band around the 200-EMA (±3%).
const MR_BAND_PCT: f64 = 0.03;
/// Mean-reversion quality floor: price must sit at/above 97% of the 200-EMA.
const MR_QUALITY_FLOOR: f64 = 0.97;

// ---------------------------------------------------------------------------
// Pure numeric helpers (unit-tested)
// ---------------------------------------------------------------------------

/// `period`-length EMA of `closes`, seeded with the SMA of the first `period`
/// values, then standard EMA smoothing (`alpha = 2/(period+1)`). Returns the
/// final EMA, or None if there are fewer than `period` closes.
fn ema(closes: &[f64], period: usize) -> Option<f64> {
    if period == 0 || closes.len() < period {
        return None;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut e = closes[..period].iter().sum::<f64>() / period as f64;
    for &c in &closes[period..] {
        e = alpha * c + (1.0 - alpha) * e;
    }
    Some(e)
}

/// Wilder's ATR over the whole series: seed = SMA of the first `period` true
/// ranges, then RMA smoothing. Returns the final ATR, or None if too short.
/// (Same definition as `storage_kernel::wilder_atr`, factored locally so the
/// swing scan stays self-contained and testable.)
fn atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if period == 0 || n <= period {
        return None;
    }
    let mut trs = Vec::with_capacity(n - 1);
    for i in 1..n {
        let tr = (highs[i] - lows[i])
            .max((highs[i] - closes[i - 1]).abs())
            .max((lows[i] - closes[i - 1]).abs());
        trs.push(tr);
    }
    if trs.len() < period {
        return None;
    }
    let mut a = trs[..period].iter().sum::<f64>() / period as f64;
    for &tr in &trs[period..] {
        a = (a * (period as f64 - 1.0) + tr) / period as f64;
    }
    Some(a)
}

/// Mean of a slice (0.0 when empty).
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

// ---------------------------------------------------------------------------
// Macro source resolution + I/O
// ---------------------------------------------------------------------------

/// Pick the daily source for a symbol: prefer the long-history Yahoo `daily/`
/// file under `config::data_root()`, falling back to the Kite `1day/` file at
/// `config::parquet_path(root, sym, Timeframe::Daily)`.
fn daily_source(root: &Path, symbol: &str) -> PathBuf {
    let yahoo = config::parquet_path(&config::data_root(), symbol, Timeframe::DailyLong);
    if yahoo.exists() {
        yahoo
    } else {
        config::parquet_path(root, symbol, Timeframe::Daily)
    }
}

/// Pull (high, low, close, volume) for every daily bar, oldest-first, in a
/// single `read_parquet` SELECT. Volume is CAST to DOUBLE — some files carry a
/// bad volume at/over `i64::MAX` that would otherwise overflow an i64 read.
fn read_daily(
    conn: &Connection,
    path: &Path,
) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> {
    let qp = crate::storage_kernel::quote_path(path);
    let sql = format!(
        "SELECT high, low, close, CAST(volume AS DOUBLE) AS volume \
         FROM read_parquet({qp}) ORDER BY date"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, f64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;
    let (mut h, mut l, mut c, mut v) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for r in rows {
        let (a, b, d, e) = r?;
        h.push(a);
        l.push(b);
        c.push(d);
        v.push(e);
    }
    Ok((h, l, c, v))
}

// ---------------------------------------------------------------------------
// Per-symbol swing evaluation
// ---------------------------------------------------------------------------

/// Evaluate one symbol; returns at most one `SwingSetup` (None when no setup
/// fires or the data is too short). Errors propagate so the caller can skip.
fn eval_symbol(conn: &Connection, root: &Path, symbol: &str) -> Result<Option<SwingSetup>> {
    let path = daily_source(root, symbol);
    let (highs, lows, closes, volumes) = read_daily(conn, &path)?;

    let n = closes.len();
    if n < MIN_DAILY_BARS {
        return Ok(None);
    }

    let last_close = closes[n - 1];
    let last_volume = volumes[n - 1];

    // 50-day average volume EXCLUDING today.
    let vol_window = &volumes[n - 1 - VOL_AVG_LEN..n - 1];
    let sma50_vol = mean(vol_window);
    let vol_ratio = if sma50_vol > 0.0 {
        last_volume / sma50_vol
    } else {
        0.0
    };

    let ema200 =
        ema(&closes, SWING_EMA_LEN).ok_or_else(|| anyhow!("{symbol}: 200-EMA unavailable"))?;
    let atr_val =
        atr(&highs, &lows, &closes, ATR_PERIOD).ok_or_else(|| anyhow!("{symbol}: ATR unavailable"))?;

    // Multi-year structure (saturating start: STRUCTURE_LEN may exceed n when a
    // symbol has just over MIN_DAILY_BARS — use all available history then).
    let struct_start = n.saturating_sub(STRUCTURE_LEN);
    let support = lows[struct_start..]
        .iter()
        .cloned()
        .fold(f64::MAX, f64::min);
    let resistance = highs[struct_start..]
        .iter()
        .cloned()
        .fold(f64::MIN, f64::max);

    // Prior 20-day high, EXCLUDING today.
    let prior20_high = highs[n - 1 - BREAKOUT_LEN..n - 1]
        .iter()
        .cloned()
        .fold(f64::MIN, f64::max);

    // --- Setup A: volume-delivery breakout ---------------------------------
    if vol_ratio >= VOL_BREAKOUT_MULT && last_close > prior20_high {
        let note = format!(
            "Volume surge {vol_ratio:.1}× the 50-day average on a breakout above the \
             prior 20-day high ({prior20_high:.2}) — possible institutional accumulation."
        );
        return Ok(Some(SwingSetup {
            symbol: symbol.to_string(),
            kind: "volume_delivery_breakout".to_string(),
            side: "BUY".to_string(),
            last_close,
            ema200,
            vol_ratio,
            support,
            resistance,
            atr: atr_val,
            note,
            score: vol_ratio,
        }));
    }

    // --- Setup B: mean-reversion retest of the 200-EMA ---------------------
    let dist = (last_close - ema200).abs() / ema200;
    if ema200 > 0.0 && dist <= MR_BAND_PCT && last_close >= ema200 * MR_QUALITY_FLOOR {
        let note = format!(
            "Retesting the 200-day EMA ({ema200:.2}) within {:.1}% — quality name \
             consolidating near its long-term mean.",
            dist * 100.0
        );
        let score = 1.0 / (1.0 + dist);
        return Ok(Some(SwingSetup {
            symbol: symbol.to_string(),
            kind: "mean_reversion_200ema".to_string(),
            side: "BUY".to_string(),
            last_close,
            ema200,
            vol_ratio,
            support,
            resistance,
            atr: atr_val,
            note,
            score,
        }));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Parallel scan
// ---------------------------------------------------------------------------

thread_local! {
    /// One reusable DuckDB connection per rayon worker thread — avoids paying
    /// connection-init cost once per symbol instead of once per core.
    static SWING_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Run `f` with this worker thread's connection, lazily creating it.
fn with_swing_conn<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    SWING_CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(open_conn()?);
        }
        f(slot.as_ref().unwrap())
    })
}

/// Scan the daily archive for swing setups and return the catalog.
///
/// Runs across `symbols` in parallel (rayon), one thread-local DuckDB
/// connection per worker. Each symbol may emit at most one setup; per-symbol
/// read/compute errors are skipped, never fatal. Setups are sorted by `score`
/// descending. `scanned` is the number of symbols inspected.
pub fn scan_swing(root: &Path, symbols: &[String]) -> SwingCatalog {
    let mut setups: Vec<SwingSetup> = symbols
        .par_iter()
        .filter_map(|sym| {
            with_swing_conn(|conn| eval_symbol(conn, root, sym))
                .ok()
                .flatten()
        })
        .collect();

    setups.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let built_ist = chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    SwingCatalog {
        setups,
        scanned: symbols.len(),
        built_ist,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_converges_to_constant() {
        // On a constant series, the SMA seed already equals the constant and
        // every smoothing step is a no-op, so the EMA equals that constant.
        let closes = vec![42.0_f64; 250];
        let e = ema(&closes, SWING_EMA_LEN).unwrap();
        assert!((e - 42.0).abs() < 1e-9, "ema was {e}");
        // Too short => None.
        assert!(ema(&[1.0, 2.0, 3.0], SWING_EMA_LEN).is_none());
    }

    #[test]
    fn ema_tracks_toward_recent_level() {
        // Start flat at 100, then a long run at 200: EMA must move above the
        // seed (100) toward the new level without overshooting it.
        let mut closes = vec![100.0_f64; 200];
        closes.extend(std::iter::repeat(200.0).take(400));
        let e = ema(&closes, SWING_EMA_LEN).unwrap();
        assert!(e > 100.0 && e <= 200.0, "ema was {e}");
    }

    #[test]
    fn atr_constant_range_equals_range() {
        // Constant-range bars: every true range = 2.0, so ATR == 2.0.
        let n = 30;
        let highs: Vec<f64> = (0..n).map(|i| 100.0 + i as f64).collect();
        let lows: Vec<f64> = highs.iter().map(|h| h - 2.0).collect();
        let closes: Vec<f64> = highs.iter().map(|h| h - 1.0).collect();
        let a = atr(&highs, &lows, &closes, ATR_PERIOD).unwrap();
        assert!((a - 2.0).abs() < 1e-9, "atr was {a}");
        // Too short => None.
        assert!(atr(&highs[..5], &lows[..5], &closes[..5], ATR_PERIOD).is_none());
    }

    #[test]
    fn mean_basic_and_empty() {
        assert!((mean(&[1.0, 2.0, 3.0]) - 2.0).abs() < 1e-12);
        assert_eq!(mean(&[]), 0.0);
    }
}

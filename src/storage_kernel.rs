//! Out-of-core historical access via DuckDB over the parquet archive.
//!
//! DuckDB reads the parquet files directly (no full load into RAM): we push
//! filters down so only the row-groups we need are materialised. From that we
//! build a lightweight per-symbol pre-market baseline:
//!
//!   * macro layer — long-term Wilder ATR(14) + 52-week high/low, from the
//!     ~30-year Yahoo `daily/` files (falling back to Kite `1day/` if absent);
//!   * intraday volume profile — POC / VAH / VAL over the last
//!     [`config::VOLUME_PROFILE_DAYS`] trading days of `minute/` bars.
//!
//! All heavy numeric reductions (ATR smoothing, value-area expansion) run in
//! plain Rust so they are unit-testable and byte-reproducible against an
//! external reference (see `tests`).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use duckdb::Connection;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::{self, Timeframe};

/// Lightweight, in-memory baseline state for one symbol, produced by the
/// pre-market scan and consumed by the strategy / analytics layers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolBaseline {
    pub symbol: String,
    /// Most recent daily close (raw, from the macro source).
    pub last_close: f64,
    /// Long-term Wilder ATR(14) on daily bars — systemic volatility scale.
    pub atr_long: f64,
    /// 52-week (252 trading day) high / low — macro resistance / support.
    pub hi_52w: f64,
    pub lo_52w: f64,
    /// Volume-profile Point of Control (most-traded price) over the window.
    pub poc: f64,
    /// Value Area High / Low (bounds of the 70% volume band).
    pub vah: f64,
    pub val: f64,
    /// Number of minute bars that went into the volume profile.
    pub vp_bars: usize,
    /// Which macro source was used (`daily` Yahoo or `1day` Kite fallback).
    pub macro_source: &'static str,
}

// ---------------------------------------------------------------------------
// Symbol discovery
// ---------------------------------------------------------------------------

/// Discover the intraday-tradeable universe = symbols that have a `minute/`
/// file (these all also have `1day/`). Sorted, de-duplicated.
pub fn discover_symbols(root: &Path) -> Result<Vec<String>> {
    let dir = root.join(Timeframe::Minute.dir());
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading minute dir {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sym) = name.strip_suffix(".parquet") {
            if !sym.is_empty() {
                out.insert(sym.to_string());
            }
        }
    }
    Ok(out.into_iter().collect())
}

// ---------------------------------------------------------------------------
// DuckDB helpers
// ---------------------------------------------------------------------------

/// Quote a filesystem path as a single-quoted SQL string literal (escaping any
/// embedded single quote). Paths come from on-disk filenames, not user input.
pub(crate) fn quote_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\'', "''");
    format!("'{s}'")
}

/// Fresh in-memory DuckDB connection, pinned to a single thread (we parallelise
/// across symbols at the rayon level) and to IST so `CAST(ts AS DATE)` yields
/// the correct Indian trading day.
pub(crate) fn open_conn() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("open duckdb in-memory")?;
    // threads=1: we parallelise across symbols at the rayon level.
    // TimeZone=IST: `CAST(ts AS DATE)` yields the correct Indian trading day.
    // memory_limit + temp_directory: a backstop so a single pathological query
    // (e.g. a 25-year 1-minute file, or a full-universe scan) cannot OOM an 18 GB
    // box — past the limit DuckDB spills to disk instead of crashing the process.
    // It is per-connection, and we open ~one connection per rayon worker, so the
    // default is sized to keep the aggregate (≈ cores × limit) well under 18 GB.
    // Tune via RAM_ISTP_DB_MEMORY_LIMIT (e.g. "3GB", "75%").
    let mem = std::env::var("RAM_ISTP_DB_MEMORY_LIMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "2GB".to_string());
    let mem = mem.replace('\'', "");
    let spill = std::env::temp_dir().join("ram_istp_duckdb_spill");
    let _ = std::fs::create_dir_all(&spill);
    conn.execute_batch(&format!(
        "SET threads TO 1; SET TimeZone='Asia/Kolkata'; \
         SET memory_limit='{mem}'; SET temp_directory={spill};",
        spill = quote_path(&spill),
    ))
    .context("configure duckdb session")?;
    Ok(conn)
}

/// Pull (high, low, close) for every daily bar, oldest-first.
fn read_daily_ohlc(conn: &Connection, path: &Path) -> Result<(Vec<f64>, Vec<f64>, Vec<f64>)> {
    let sql = format!(
        "SELECT high, low, close FROM read_parquet({}) ORDER BY date",
        quote_path(path)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, f64>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    let (mut h, mut l, mut c) = (Vec::new(), Vec::new(), Vec::new());
    for r in rows {
        let (a, b, d) = r?;
        h.push(a);
        l.push(b);
        c.push(d);
    }
    Ok((h, l, c))
}

/// Pull (IST-day-string, typical-price, volume) for minute bars in roughly the
/// last 40 calendar days (comfortably ≥ 25 trading days). Day-level trimming to
/// exactly the window happens in `volume_profile`.
fn read_recent_minute(conn: &Connection, path: &Path) -> Result<Vec<(String, f64, f64)>> {
    let qp = quote_path(path);
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS day, (high+low+close)/3.0 AS typ, CAST(volume AS DOUBLE) AS volume \
         FROM read_parquet({qp}) \
         WHERE date >= (SELECT max(date) FROM read_parquet({qp})) - INTERVAL 40 DAY \
         ORDER BY date"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// One OHLCV candle plus an incrementing intraday `day` id (0-based, bumps each
/// time the IST calendar date changes) for fast same-day grouping downstream.
#[derive(Debug, Clone, Copy)]
pub struct Candle {
    pub day: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Load every candle for a symbol at a given resolution, oldest-first, tagging
/// each with a contiguous `day` id. Intended for the backtester (use `5min`/
/// `15min` for tractable history; `minute` works but is ~1M rows/symbol).
pub fn load_candles(conn: &Connection, root: &Path, symbol: &str, tf: Timeframe) -> Result<Vec<Candle>> {
    let path = config::parquet_path(root, symbol, tf);
    // volume is CAST to DOUBLE: a few files carry a volume at/over i64::MAX
    // (bad ticks) that would otherwise overflow an i64 read.
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS day, open, high, low, close, CAST(volume AS DOUBLE) AS volume \
         FROM read_parquet({}) ORDER BY date",
        quote_path(&path)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, f64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, f64>(4)?,
            row.get::<_, f64>(5)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut last_day: Option<String> = None;
    let mut day_id: u32 = 0;
    for r in rows {
        let (day, open, high, low, close, volume) = r?;
        match &last_day {
            Some(d) if *d == day => {}
            Some(_) => {
                day_id += 1;
                last_day = Some(day);
            }
            None => last_day = Some(day),
        }
        out.push(Candle {
            day: day_id,
            open,
            high,
            low,
            close,
            volume,
        });
    }
    Ok(out)
}

/// Load the IST calendar date (`"YYYY-MM-DD"`) of every candle for a symbol at a
/// resolution, oldest-first — index-aligned 1:1 with [`load_candles`]. Used to
/// map a trade's entry bar to a market date for NIFTY-regime conditioning.
pub fn load_candle_dates(
    conn: &Connection,
    root: &Path,
    symbol: &str,
    tf: Timeframe,
) -> Result<Vec<String>> {
    let path = config::parquet_path(root, symbol, tf);
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS day FROM read_parquet({}) ORDER BY date",
        quote_path(&path)
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pure numeric reductions (unit-tested, reference-matched)
// ---------------------------------------------------------------------------

/// Wilder's ATR over the whole series: seed = SMA of the first `period` true
/// ranges, then RMA smoothing. Returns the final ATR, or None if too short.
pub fn wilder_atr(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    let n = closes.len();
    if n <= period {
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
    let mut atr = trs[..period].iter().sum::<f64>() / period as f64;
    for &tr in &trs[period..] {
        atr = (atr * (period as f64 - 1.0) + tr) / period as f64;
    }
    Some(atr)
}

/// 52-week (or all-available) high and low from daily highs/lows.
fn high_low_52w(highs: &[f64], lows: &[f64]) -> (f64, f64) {
    let w = 252.min(highs.len());
    let hi = highs[highs.len() - w..]
        .iter()
        .cloned()
        .fold(f64::MIN, f64::max);
    let lo = lows[lows.len() - w..]
        .iter()
        .cloned()
        .fold(f64::MAX, f64::min);
    (hi, lo)
}

/// Result of a volume-profile reduction.
#[derive(Debug, Clone, Copy)]
pub struct VolumeProfile {
    pub poc: f64,
    pub vah: f64,
    pub val: f64,
    pub bars: usize,
}

/// Build a fixed-bin volume profile over the last `days` trading days and
/// expand a `va_pct` value area outward from the POC.
///
/// `recent` is (IST-day, typical-price, volume), oldest-first. Algorithm is
/// deliberately identical to the Python reference: typical price, `nbins`
/// equal-width bins over the windowed price range, value area grown by always
/// taking the heavier adjacent bin.
pub fn volume_profile(
    recent: &[(String, f64, f64)],
    days: usize,
    nbins: usize,
    va_pct: f64,
) -> Option<VolumeProfile> {
    if recent.is_empty() || nbins == 0 {
        return None;
    }
    // Last `days` distinct trading days.
    let mut distinct: Vec<&str> = recent.iter().map(|(d, _, _)| d.as_str()).collect();
    distinct.dedup();
    let keep: BTreeSet<&str> = distinct.iter().rev().take(days).cloned().collect();

    let bars: Vec<(f64, f64)> = recent
        .iter()
        .filter(|(d, _, _)| keep.contains(d.as_str()))
        .map(|(_, typ, vol)| (*typ, *vol))
        .collect();
    if bars.is_empty() {
        return None;
    }

    let pmin = bars.iter().map(|(t, _)| *t).fold(f64::MAX, f64::min);
    let pmax = bars.iter().map(|(t, _)| *t).fold(f64::MIN, f64::max);
    let binw = (pmax - pmin) / nbins as f64;

    let mut buckets = vec![0.0_f64; nbins];
    for (typ, vol) in &bars {
        let b = if binw > 0.0 {
            (((*typ - pmin) / binw) as usize).min(nbins - 1)
        } else {
            0
        };
        buckets[b] += *vol;
    }
    let total: f64 = buckets.iter().sum();
    if total <= 0.0 {
        return None;
    }

    let poc_bin = buckets
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(i, _)| i)
        .unwrap();

    let target = total * va_pct;
    let (mut lo, mut hi) = (poc_bin, poc_bin);
    let mut acc = buckets[poc_bin];
    while acc < target && (lo > 0 || hi < nbins - 1) {
        let up = if hi < nbins - 1 { buckets[hi + 1] } else { -1.0 };
        let dn = if lo > 0 { buckets[lo - 1] } else { -1.0 };
        if up >= dn {
            hi += 1;
            acc += buckets[hi];
        } else {
            lo -= 1;
            acc += buckets[lo];
        }
    }

    Some(VolumeProfile {
        poc: pmin + (poc_bin as f64 + 0.5) * binw,
        vah: pmin + (hi as f64 + 1.0) * binw,
        val: pmin + lo as f64 * binw,
        bars: bars.len(),
    })
}

// ---------------------------------------------------------------------------
// Per-symbol baseline + parallel scan
// ---------------------------------------------------------------------------

/// Pick the macro daily source: prefer the long-history Yahoo `daily/` file,
/// fall back to the Kite `1day/` file.
fn macro_daily_path(root: &Path, symbol: &str) -> (PathBuf, &'static str) {
    let yahoo = config::parquet_path(root, symbol, Timeframe::DailyLong);
    if yahoo.exists() {
        (yahoo, "daily")
    } else {
        (config::parquet_path(root, symbol, Timeframe::Daily), "1day")
    }
}

/// Compute the full baseline for one symbol (opens its own DuckDB connection).
pub fn compute_baseline(root: &Path, symbol: &str) -> Result<SymbolBaseline> {
    let conn = open_conn()?;
    compute_baseline_on(&conn, root, symbol)
}

/// Compute the full baseline for one symbol using a caller-supplied connection
/// (so the parallel scan can reuse one connection per worker thread).
pub fn compute_baseline_on(conn: &Connection, root: &Path, symbol: &str) -> Result<SymbolBaseline> {
    let (daily_path, macro_source) = macro_daily_path(root, symbol);
    let (highs, lows, closes) = read_daily_ohlc(conn, &daily_path)
        .with_context(|| format!("{symbol}: read daily {}", daily_path.display()))?;
    let atr_long = wilder_atr(&highs, &lows, &closes, config::ATR_PERIOD)
        .ok_or_else(|| anyhow!("{symbol}: not enough daily bars for ATR"))?;
    let (hi_52w, lo_52w) = high_low_52w(&highs, &lows);
    let last_close = *closes.last().unwrap();

    let minute_path = config::parquet_path(root, symbol, Timeframe::Minute);
    let recent = read_recent_minute(conn, &minute_path)
        .with_context(|| format!("{symbol}: read minute {}", minute_path.display()))?;
    let vp = volume_profile(&recent, config::VOLUME_PROFILE_DAYS, 100, config::VALUE_AREA_PCT)
        .ok_or_else(|| anyhow!("{symbol}: empty volume profile"))?;

    Ok(SymbolBaseline {
        symbol: symbol.to_string(),
        last_close,
        atr_long,
        hi_52w,
        lo_52w,
        poc: vp.poc,
        vah: vp.vah,
        val: vp.val,
        vp_bars: vp.bars,
        macro_source,
    })
}

/// Outcome of a full pre-market scan.
pub struct ScanReport {
    pub baselines: Vec<SymbolBaseline>,
    pub failures: Vec<(String, String)>,
    pub elapsed: Duration,
}

thread_local! {
    /// One reusable DuckDB connection per rayon worker thread — avoids paying
    /// connection-init cost once per symbol (541×) instead of once per core.
    static SCAN_CONN: RefCell<Option<Connection>> = const { RefCell::new(None) };
}

/// Run `f` with this worker thread's connection, lazily creating it.
fn with_scan_conn<T>(f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    SCAN_CONN.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(open_conn()?);
        }
        f(slot.as_ref().unwrap())
    })
}

/// Scan all given symbols in parallel (rayon), reusing one DuckDB connection per
/// worker thread. Failures are collected, never fatal — a single bad file can't
/// sink the scan.
pub fn premarket_scan(root: &Path, symbols: &[String]) -> ScanReport {
    let start = Instant::now();
    let results: Vec<Result<SymbolBaseline, (String, String)>> = symbols
        .par_iter()
        .map(|sym| {
            with_scan_conn(|conn| compute_baseline_on(conn, root, sym))
                .map_err(|e| (sym.clone(), format!("{e:#}")))
        })
        .collect();

    let mut baselines = Vec::new();
    let mut failures = Vec::new();
    for r in results {
        match r {
            Ok(b) => baselines.push(b),
            Err(f) => failures.push(f),
        }
    }
    baselines.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    ScanReport {
        baselines,
        failures,
        elapsed: start.elapsed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wilder_atr_matches_known_series() {
        // Constant-range bars: every TR = 2.0, so ATR must converge to 2.0.
        let n = 30;
        let highs: Vec<f64> = (0..n).map(|i| 100.0 + i as f64).collect();
        let lows: Vec<f64> = highs.iter().map(|h| h - 2.0).collect();
        let closes: Vec<f64> = highs.iter().map(|h| h - 1.0).collect();
        let atr = wilder_atr(&highs, &lows, &closes, 14).unwrap();
        assert!((atr - 2.0).abs() < 1e-9, "atr={atr}");
        assert!(wilder_atr(&highs[..5], &lows[..5], &closes[..5], 14).is_none());
    }

    #[test]
    fn volume_profile_single_bin_and_value_area() {
        // One dominant price bucket → POC there, value area collapses to it.
        let recent: Vec<(String, f64, f64)> = vec![
            ("2026-06-25".into(), 100.0, 1000.0),
            ("2026-06-25".into(), 100.1, 10.0),
            ("2026-06-25".into(), 99.9, 10.0),
        ];
        let vp = volume_profile(&recent, 25, 100, 0.70).unwrap();
        assert_eq!(vp.bars, 3);
        assert!(vp.val <= vp.poc && vp.poc <= vp.vah);
        assert!(vp.poc > 99.9 && vp.poc < 100.1);
    }
}

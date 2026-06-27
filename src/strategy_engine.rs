//! Intraday strategy library + backtester (the core edge layer).
//!
//! A `Strategy` is just a function from precomputed `Indicators` to a list of
//! dated entry signals `(bar_index, Direction)`. A single shared `simulate`
//! turns those signals into trades using ATR-scaled stops/targets and an
//! intraday-only exit (square-off at the day's last bar). Results aggregate into
//! per-(symbol, strategy, direction) `EdgeRecord`s — the "edge map" — net of
//! round-trip costs.
//!
//! Everything here is pure and deterministic (no RNG, no time) so a rerun on the
//! same parquet yields byte-identical numbers.

use std::path::Path;

use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::{self, Direction, Timeframe};
use crate::storage_kernel::{self, Candle};

// ===========================================================================
// Indicators
// ===========================================================================

const NAN: f64 = f64::NAN;

/// All indicator series a strategy might read, aligned 1:1 with the bar slice.
/// Undefined (warm-up) values are `NaN`; strategies must guard with `is_finite`.
pub struct Indicators {
    pub atr: Vec<f64>,
    pub ema9: Vec<f64>,
    pub ema21: Vec<f64>,
    pub ema50: Vec<f64>,
    pub rsi: Vec<f64>,
    pub macd: Vec<f64>,
    pub macd_sig: Vec<f64>,
    pub bb_up: Vec<f64>,
    pub bb_lo: Vec<f64>,
    pub st_dir: Vec<f64>,
    pub vwap: Vec<f64>,
    pub band_up: Vec<f64>,
    pub band_lo: Vec<f64>,
    pub donch_up: Vec<f64>,
    pub donch_lo: Vec<f64>,
    pub zscore: Vec<f64>,
    pub rvol: Vec<f64>,
    pub pivot: Vec<f64>,
    pub gap_pct: Vec<f64>,
    pub orh: Vec<f64>,
    pub orl: Vec<f64>,
}

/// Wilder/standard exponential moving average, seeded with an SMA of the first
/// `period` finite values (skips a leading NaN prefix, e.g. for MACD signal).
fn ema(v: &[f64], period: usize) -> Vec<f64> {
    let n = v.len();
    let mut out = vec![NAN; n];
    let start = match v.iter().position(|x| x.is_finite()) {
        Some(s) => s,
        None => return out,
    };
    if n - start < period {
        return out;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut prev = v[start..start + period].iter().sum::<f64>() / period as f64;
    out[start + period - 1] = prev;
    for i in start + period..n {
        prev = v[i] * k + prev * (1.0 - k);
        out[i] = prev;
    }
    out
}

/// Wilder RSI.
fn rsi(c: &[f64], period: usize) -> Vec<f64> {
    let n = c.len();
    let mut out = vec![NAN; n];
    if n <= period {
        return out;
    }
    let (mut gain, mut loss) = (0.0, 0.0);
    for i in 1..=period {
        let d = c[i] - c[i - 1];
        if d >= 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    let (mut ag, mut al) = (gain / period as f64, loss / period as f64);
    out[period] = if al == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + ag / al) };
    for i in period + 1..n {
        let d = c[i] - c[i - 1];
        let (g, l) = (d.max(0.0), (-d).max(0.0));
        ag = (ag * (period as f64 - 1.0) + g) / period as f64;
        al = (al * (period as f64 - 1.0) + l) / period as f64;
        out[i] = if al == 0.0 { 100.0 } else { 100.0 - 100.0 / (1.0 + ag / al) };
    }
    out
}

/// Wilder ATR series (per-bar).
fn atr_series(h: &[f64], l: &[f64], c: &[f64], period: usize) -> Vec<f64> {
    let n = c.len();
    let mut out = vec![NAN; n];
    if n <= period {
        return out;
    }
    let mut tr = vec![0.0; n];
    for i in 1..n {
        tr[i] = (h[i] - l[i])
            .max((h[i] - c[i - 1]).abs())
            .max((l[i] - c[i - 1]).abs());
    }
    let mut prev = tr[1..=period].iter().sum::<f64>() / period as f64;
    out[period] = prev;
    for i in period + 1..n {
        prev = (prev * (period as f64 - 1.0) + tr[i]) / period as f64;
        out[i] = prev;
    }
    out
}

/// Rolling population mean & std-dev over a trailing window.
fn roll_mean_std(c: &[f64], period: usize) -> (Vec<f64>, Vec<f64>) {
    let n = c.len();
    let (mut m, mut s) = (vec![NAN; n], vec![NAN; n]);
    if n < period {
        return (m, s);
    }
    for i in period - 1..n {
        let w = &c[i + 1 - period..=i];
        let mean = w.iter().sum::<f64>() / period as f64;
        let var = w.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / period as f64;
        m[i] = mean;
        s[i] = var.sqrt();
    }
    (m, s)
}

/// Supertrend direction (+1 up-trend, -1 down-trend); NaN during ATR warm-up.
fn supertrend_dir(h: &[f64], l: &[f64], c: &[f64], atr: &[f64], mult: f64) -> Vec<f64> {
    let n = c.len();
    let mut dir = vec![NAN; n];
    let (mut fub, mut flb) = (0.0, 0.0); // final upper / lower band
    let mut trend = 1.0;
    let mut started = false;
    for i in 0..n {
        if !atr[i].is_finite() {
            continue;
        }
        let mid = (h[i] + l[i]) / 2.0;
        let bub = mid + mult * atr[i];
        let blb = mid - mult * atr[i];
        if !started {
            fub = bub;
            flb = blb;
            trend = 1.0;
            dir[i] = trend;
            started = true;
            continue;
        }
        fub = if bub < fub || c[i - 1] > fub { bub } else { fub };
        flb = if blb > flb || c[i - 1] < flb { blb } else { flb };
        if c[i] > fub {
            trend = 1.0;
        } else if c[i] < flb {
            trend = -1.0;
        }
        dir[i] = trend;
    }
    dir
}

/// Intraday VWAP, reset at each new trading day.
fn vwap_daily(bars: &[Candle]) -> Vec<f64> {
    let n = bars.len();
    let mut out = vec![NAN; n];
    let (mut cum_pv, mut cum_v, mut cur_day) = (0.0, 0.0, u32::MAX);
    for i in 0..n {
        if bars[i].day != cur_day {
            cur_day = bars[i].day;
            cum_pv = 0.0;
            cum_v = 0.0;
        }
        let typ = (bars[i].high + bars[i].low + bars[i].close) / 3.0;
        cum_pv += typ * bars[i].volume;
        cum_v += bars[i].volume;
        out[i] = if cum_v > 0.0 { cum_pv / cum_v } else { bars[i].close };
    }
    out
}

/// Donchian channel using the PRIOR `period` bars (excludes the current bar).
fn donchian(h: &[f64], l: &[f64], period: usize) -> (Vec<f64>, Vec<f64>) {
    let n = h.len();
    let (mut up, mut lo) = (vec![NAN; n], vec![NAN; n]);
    for i in period..n {
        up[i] = h[i - period..i].iter().cloned().fold(f64::MIN, f64::max);
        lo[i] = l[i - period..i].iter().cloned().fold(f64::MAX, f64::min);
    }
    (up, lo)
}

/// Relative volume: current bar volume / mean of prior `period` bars.
fn rvol(vol: &[f64], period: usize) -> Vec<f64> {
    let n = vol.len();
    let mut out = vec![NAN; n];
    for i in period..n {
        let avg = vol[i - period..i].iter().sum::<f64>() / period as f64;
        out[i] = if avg > 0.0 { vol[i] / avg } else { NAN };
    }
    out
}

/// Per-day OHLC + bar-index span.
struct DayAgg {
    start: usize,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
}

fn day_aggs(bars: &[Candle]) -> Vec<DayAgg> {
    let mut out: Vec<DayAgg> = Vec::new();
    for (i, b) in bars.iter().enumerate() {
        match out.last_mut() {
            Some(d) if bars[d.start].day == b.day => {
                d.high = d.high.max(b.high);
                d.low = d.low.min(b.low);
                d.close = b.close;
            }
            _ => out.push(DayAgg {
                start: i,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
            }),
        }
    }
    out
}

/// Compute every indicator series for a symbol's bars. `tf_minutes` sizes the
/// opening range (15-minute ORB ⇒ `15 / tf_minutes` bars).
pub fn compute_indicators(bars: &[Candle], tf_minutes: u32) -> Indicators {
    let n = bars.len();
    let h: Vec<f64> = bars.iter().map(|b| b.high).collect();
    let l: Vec<f64> = bars.iter().map(|b| b.low).collect();
    let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
    let vol: Vec<f64> = bars.iter().map(|b| b.volume).collect();

    let atr = atr_series(&h, &l, &c, config::ATR_PERIOD);
    let (macd_fast, macd_slow) = (ema(&c, 12), ema(&c, 26));
    let macd: Vec<f64> = macd_fast
        .iter()
        .zip(&macd_slow)
        .map(|(a, b)| a - b)
        .collect();
    let macd_sig = ema(&macd, 9);

    let (bb_mid, bb_std) = roll_mean_std(&c, 20);
    let bb_up: Vec<f64> = bb_mid.iter().zip(&bb_std).map(|(m, s)| m + 2.0 * s).collect();
    let bb_lo: Vec<f64> = bb_mid.iter().zip(&bb_std).map(|(m, s)| m - 2.0 * s).collect();

    let vwap = vwap_daily(bars);
    let band_up: Vec<f64> = vwap.iter().zip(&bb_std).map(|(v, s)| v + 2.0 * s).collect();
    let band_lo: Vec<f64> = vwap.iter().zip(&bb_std).map(|(v, s)| v - 2.0 * s).collect();

    let (donch_up, donch_lo) = donchian(&h, &l, 20);
    let (zmean, zstd) = roll_mean_std(&c, 20);
    let zscore: Vec<f64> = (0..n)
        .map(|i| {
            if zstd[i].is_finite() && zstd[i] > 0.0 {
                (c[i] - zmean[i]) / zstd[i]
            } else {
                NAN
            }
        })
        .collect();

    // Per-day derived levels: prior-day pivot, gap %, opening range.
    let days = day_aggs(bars);
    let orb_bars = (15 / tf_minutes.max(1)).max(1) as usize;
    let mut pivot = vec![NAN; n];
    let mut gap_pct = vec![NAN; n];
    let mut orh = vec![NAN; n];
    let mut orl = vec![NAN; n];
    for (di, d) in days.iter().enumerate() {
        let end = days.get(di + 1).map(|nx| nx.start).unwrap_or(n);
        // prior-day pivot + gap
        if di > 0 {
            let p = &days[di - 1];
            let piv = (p.high + p.low + p.close) / 3.0;
            let g = (d.open - p.close) / p.close * 100.0;
            for i in d.start..end {
                pivot[i] = piv;
                gap_pct[i] = g;
            }
        }
        // opening range over the first `orb_bars` bars of the day
        let or_end = (d.start + orb_bars).min(end);
        let mut hh = f64::MIN;
        let mut ll = f64::MAX;
        for i in d.start..or_end {
            hh = hh.max(bars[i].high);
            ll = ll.min(bars[i].low);
        }
        for i in or_end..end {
            orh[i] = hh;
            orl[i] = ll;
        }
    }

    Indicators {
        atr,
        ema9: ema(&c, 9),
        ema21: ema(&c, 21),
        ema50: ema(&c, 50),
        rsi: rsi(&c, 14),
        macd,
        macd_sig,
        bb_up,
        bb_lo,
        st_dir: supertrend_dir(&h, &l, &c, &atr_series(&h, &l, &c, 10), 3.0),
        vwap,
        band_up,
        band_lo,
        donch_up,
        donch_lo,
        zscore,
        rvol: rvol(&vol, 20),
        pivot,
        gap_pct,
        orh,
        orl,
    }
}

// ===========================================================================
// Signal helpers
// ===========================================================================

#[inline]
fn finite4(a: f64, b: f64, c: f64, d: f64) -> bool {
    a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite()
}

/// `x` crosses above `y` at bar `i`.
#[inline]
fn cross_up(x: &[f64], y: &[f64], i: usize) -> bool {
    i > 0 && finite4(x[i - 1], y[i - 1], x[i], y[i]) && x[i - 1] <= y[i - 1] && x[i] > y[i]
}
/// `x` crosses below `y` at bar `i`.
#[inline]
fn cross_down(x: &[f64], y: &[f64], i: usize) -> bool {
    i > 0 && finite4(x[i - 1], y[i - 1], x[i], y[i]) && x[i - 1] >= y[i - 1] && x[i] < y[i]
}
/// `x` crosses up through scalar `t`.
#[inline]
fn up_through(x: &[f64], t: f64, i: usize) -> bool {
    i > 0 && x[i - 1].is_finite() && x[i].is_finite() && x[i - 1] <= t && x[i] > t
}
/// `x` crosses down through scalar `t`.
#[inline]
fn down_through(x: &[f64], t: f64, i: usize) -> bool {
    i > 0 && x[i - 1].is_finite() && x[i].is_finite() && x[i - 1] >= t && x[i] < t
}

// ===========================================================================
// Strategy library
// ===========================================================================

/// A backtestable intraday strategy: maps indicators to dated entry signals.
pub trait Strategy: Sync {
    fn name(&self) -> &'static str;
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)>;
}

macro_rules! push_cross {
    ($out:ident, $i:ident, $up:expr, $dn:expr) => {
        if $up {
            $out.push(($i, Direction::Long));
        } else if $dn {
            $out.push(($i, Direction::Short));
        }
    };
}

struct VwapCross;
impl Strategy for VwapCross {
    fn name(&self) -> &'static str {
        "vwap_cross"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.vwap, i), cross_down(&c, &ind.vwap, i));
        }
        o
    }
}

struct EmaCross;
impl Strategy for EmaCross {
    fn name(&self) -> &'static str {
        "ema_cross_9_21"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(
                o,
                i,
                cross_up(&ind.ema9, &ind.ema21, i),
                cross_down(&ind.ema9, &ind.ema21, i)
            );
        }
        o
    }
}

struct RsiReversion;
impl Strategy for RsiReversion {
    fn name(&self) -> &'static str {
        "rsi_reversion"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, up_through(&ind.rsi, 30.0, i), down_through(&ind.rsi, 70.0, i));
        }
        o
    }
}

struct MacdCross;
impl Strategy for MacdCross {
    fn name(&self) -> &'static str {
        "macd_cross"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(
                o,
                i,
                cross_up(&ind.macd, &ind.macd_sig, i),
                cross_down(&ind.macd, &ind.macd_sig, i)
            );
        }
        o
    }
}

struct BollingerReversion;
impl Strategy for BollingerReversion {
    fn name(&self) -> &'static str {
        "bollinger_reversion"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.bb_lo, i), cross_down(&c, &ind.bb_up, i));
        }
        o
    }
}

struct SupertrendFlip;
impl Strategy for SupertrendFlip {
    fn name(&self) -> &'static str {
        "supertrend"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let mut o = Vec::new();
        for i in 1..bars.len() {
            let (p, n) = (ind.st_dir[i - 1], ind.st_dir[i]);
            if p.is_finite() && n.is_finite() {
                if p < 0.0 && n > 0.0 {
                    o.push((i, Direction::Long));
                } else if p > 0.0 && n < 0.0 {
                    o.push((i, Direction::Short));
                }
            }
        }
        o
    }
}

struct OpeningRangeBreakout;
impl Strategy for OpeningRangeBreakout {
    fn name(&self) -> &'static str {
        "orb_15m"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.orh, i), cross_down(&c, &ind.orl, i));
        }
        o
    }
}

struct DonchianBreakout;
impl Strategy for DonchianBreakout {
    fn name(&self) -> &'static str {
        "donchian_breakout"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.donch_up, i), cross_down(&c, &ind.donch_lo, i));
        }
        o
    }
}

struct ZScoreReversion;
impl Strategy for ZScoreReversion {
    fn name(&self) -> &'static str {
        "zscore_reversion"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, up_through(&ind.zscore, -2.0, i), down_through(&ind.zscore, 2.0, i));
        }
        o
    }
}

struct VwapBandReversion;
impl Strategy for VwapBandReversion {
    fn name(&self) -> &'static str {
        "vwap_band_reversion"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.band_lo, i), cross_down(&c, &ind.band_up, i));
        }
        o
    }
}

struct CprPivot;
impl Strategy for CprPivot {
    fn name(&self) -> &'static str {
        "cpr_pivot"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        let c: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let mut o = Vec::new();
        for i in 1..bars.len() {
            push_cross!(o, i, cross_up(&c, &ind.pivot, i), cross_down(&c, &ind.pivot, i));
        }
        o
    }
}

struct GapAndGo;
impl Strategy for GapAndGo {
    fn name(&self) -> &'static str {
        "gap_and_go"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        const THR: f64 = 0.5; // percent
        let mut o = Vec::new();
        let mut last_day = u32::MAX;
        for i in 0..bars.len() {
            if bars[i].day != last_day {
                last_day = bars[i].day;
                let g = ind.gap_pct[i];
                if g.is_finite() {
                    let bullish = bars[i].close >= bars[i].open;
                    if g > THR && bullish {
                        o.push((i, Direction::Long));
                    } else if g < -THR && !bullish {
                        o.push((i, Direction::Short));
                    }
                }
            }
        }
        o
    }
}

struct RvolBreakout;
impl Strategy for RvolBreakout {
    fn name(&self) -> &'static str {
        "rvol_breakout"
    }
    fn signals(&self, bars: &[Candle], ind: &Indicators) -> Vec<(usize, Direction)> {
        const THR: f64 = 2.0;
        let mut o = Vec::new();
        for i in 1..bars.len() {
            if up_through(&ind.rvol, THR, i) {
                if bars[i].close >= bars[i].open {
                    o.push((i, Direction::Long));
                } else {
                    o.push((i, Direction::Short));
                }
            }
        }
        o
    }
}

/// The full, extensible strategy library. Add a struct + one line here to grow.
pub fn registry() -> Vec<Box<dyn Strategy>> {
    vec![
        Box::new(VwapCross),
        Box::new(EmaCross),
        Box::new(RsiReversion),
        Box::new(MacdCross),
        Box::new(BollingerReversion),
        Box::new(SupertrendFlip),
        Box::new(OpeningRangeBreakout),
        Box::new(DonchianBreakout),
        Box::new(ZScoreReversion),
        Box::new(VwapBandReversion),
        Box::new(CprPivot),
        Box::new(GapAndGo),
        Box::new(RvolBreakout),
    ]
}

// ===========================================================================
// Simulator + metrics
// ===========================================================================

/// Simulate one direction's worth of entries with ATR stops/targets and an
/// intraday-only exit. Returns the realised R-multiple of each trade, net of a
/// round-trip cost. One position at a time (later signals while in a trade are
/// ignored), same-day square-off at the day's final bar.
pub fn simulate(
    bars: &[Candle],
    atr: &[f64],
    entries: &[usize],
    dir: Direction,
    k: f64,
    rr: f64,
    cost: f64,
) -> Vec<f64> {
    let n = bars.len();
    let s = dir.sign();
    let mut trades = Vec::new();
    let mut free_from = 0usize;
    for &idx in entries {
        if idx < free_from || idx + 1 >= n {
            continue;
        }
        let a = atr[idx];
        if !a.is_finite() || a <= 0.0 {
            continue;
        }
        let day = bars[idx].day;
        if bars[idx + 1].day != day {
            continue; // no room to manage the trade intraday
        }
        let entry = bars[idx].close;
        let risk = k * a;
        let sl = entry - s * risk;
        let tgt = entry + s * rr * risk;

        let mut exit: Option<f64> = None;
        let mut exit_idx = idx;
        let mut j = idx + 1;
        while j < n && bars[j].day == day {
            let b = &bars[j];
            match dir {
                Direction::Long => {
                    if b.low <= sl {
                        exit = Some(sl);
                        exit_idx = j;
                        break;
                    }
                    if b.high >= tgt {
                        exit = Some(tgt);
                        exit_idx = j;
                        break;
                    }
                }
                Direction::Short => {
                    if b.high >= sl {
                        exit = Some(sl);
                        exit_idx = j;
                        break;
                    }
                    if b.low <= tgt {
                        exit = Some(tgt);
                        exit_idx = j;
                        break;
                    }
                }
            }
            j += 1;
        }
        let (exit_price, eidx) = match exit {
            Some(p) => (p, exit_idx),
            None => {
                let last = j - 1; // last bar of the day
                (bars[last].close, last)
            }
        };
        let gross = s * (exit_price - entry) / risk;
        let cost_r = cost * entry / risk; // round-trip cost expressed in R
        trades.push(gross - cost_r);
        free_from = eidx + 1;
    }
    trades
}

/// Aggregate trade statistics for one (symbol, strategy, direction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub n: usize,
    pub win_pct: f64,
    pub expectancy: f64, // mean R per trade, net of cost
    pub profit_factor: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub max_dd: f64, // max drawdown of the cumulative-R curve
}

impl Metrics {
    pub fn from_rs(rs: &[f64]) -> Metrics {
        let n = rs.len();
        if n == 0 {
            return Metrics {
                n: 0,
                win_pct: 0.0,
                expectancy: 0.0,
                profit_factor: 0.0,
                avg_win: 0.0,
                avg_loss: 0.0,
                max_dd: 0.0,
            };
        }
        let wins: Vec<f64> = rs.iter().cloned().filter(|r| *r > 0.0).collect();
        let losses: Vec<f64> = rs.iter().cloned().filter(|r| *r <= 0.0).collect();
        let sum_pos: f64 = wins.iter().sum();
        let sum_neg: f64 = losses.iter().sum::<f64>().abs();
        let pf = if sum_neg > 0.0 {
            (sum_pos / sum_neg).min(99.0)
        } else if sum_pos > 0.0 {
            99.0
        } else {
            0.0
        };
        let mut peak = 0.0;
        let mut cum = 0.0;
        let mut max_dd = 0.0;
        for r in rs {
            cum += r;
            peak = f64::max(peak, cum);
            max_dd = f64::max(max_dd, peak - cum);
        }
        Metrics {
            n,
            win_pct: wins.len() as f64 / n as f64 * 100.0,
            expectancy: rs.iter().sum::<f64>() / n as f64,
            profit_factor: pf,
            avg_win: if wins.is_empty() { 0.0 } else { sum_pos / wins.len() as f64 },
            avg_loss: if losses.is_empty() { 0.0 } else { -sum_neg / losses.len() as f64 },
            max_dd,
        }
    }
}

/// One row of the edge map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub symbol: String,
    pub strategy: String,
    pub direction: Direction,
    pub timeframe: String,
    pub metrics: Metrics,
    pub eligible: bool,
}

fn eligible(m: &Metrics) -> bool {
    m.n >= config::MIN_BACKTEST_N && m.profit_factor >= config::MIN_PROFIT_FACTOR && m.expectancy > 0.0
}

// ===========================================================================
// Backtest driver + edge-map cache
// ===========================================================================

/// Backtest every strategy × direction for one symbol on `tf`.
pub fn backtest_symbol(
    conn: &duckdb::Connection,
    root: &Path,
    symbol: &str,
    tf: Timeframe,
) -> Result<Vec<EdgeRecord>> {
    let bars = storage_kernel::load_candles(conn, root, symbol, tf)
        .with_context(|| format!("{symbol}: load {} candles", tf.dir()))?;
    if bars.len() < 100 {
        return Ok(Vec::new());
    }
    let ind = compute_indicators(&bars, tf.minutes());
    let (k, rr, cost) = (config::SL_ATR_MULT, config::DEFAULT_RR, config::ROUND_TRIP_COST);

    let mut out = Vec::new();
    for strat in registry() {
        let sigs = strat.signals(&bars, &ind);
        for dir in [Direction::Long, Direction::Short] {
            let entries: Vec<usize> = sigs
                .iter()
                .filter(|(_, d)| *d == dir)
                .map(|(i, _)| *i)
                .collect();
            if entries.is_empty() {
                continue;
            }
            let rs = simulate(&bars, &ind.atr, &entries, dir, k, rr, cost);
            if rs.is_empty() {
                continue;
            }
            let metrics = Metrics::from_rs(&rs);
            let eligible = eligible(&metrics);
            out.push(EdgeRecord {
                symbol: symbol.to_string(),
                strategy: strat.name().to_string(),
                direction: dir,
                timeframe: tf.dir().to_string(),
                metrics,
                eligible,
            });
        }
    }
    Ok(out)
}

/// Backtest the whole universe in parallel (one DuckDB connection per worker).
pub fn backtest_universe(root: &Path, symbols: &[String], tf: Timeframe) -> (Vec<EdgeRecord>, Vec<(String, String)>) {
    use std::cell::RefCell;
    thread_local! {
        static BT_CONN: RefCell<Option<duckdb::Connection>> = const { RefCell::new(None) };
    }
    let nested: Vec<Result<Vec<EdgeRecord>, (String, String)>> = symbols
        .par_iter()
        .map(|sym| {
            BT_CONN.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() {
                    *slot = Some(storage_kernel::open_conn().expect("duckdb conn"));
                }
                backtest_symbol(slot.as_ref().unwrap(), root, sym, tf)
                    .map_err(|e| (sym.clone(), format!("{e:#}")))
            })
        })
        .collect();

    let mut records = Vec::new();
    let mut failures = Vec::new();
    for r in nested {
        match r {
            Ok(mut v) => records.append(&mut v),
            Err(f) => failures.push(f),
        }
    }
    (records, failures)
}

/// Path to the on-disk edge-map cache for a timeframe.
pub fn edge_map_path(tf: Timeframe) -> std::path::PathBuf {
    std::path::PathBuf::from("cache").join(format!("edge_map_{}.json", tf.dir()))
}

/// Persist the edge map as JSON under `cache/`.
pub fn save_edge_map(records: &[EdgeRecord], tf: Timeframe) -> Result<()> {
    std::fs::create_dir_all("cache").context("create cache dir")?;
    let path = edge_map_path(tf);
    let json = serde_json::to_vec_pretty(records)?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Load a previously-saved edge map, if present.
pub fn load_edge_map(tf: Timeframe) -> Result<Vec<EdgeRecord>> {
    let path = edge_map_path(tf);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Fast per-symbol lookup of eligible edges, consumed by the live analytics
/// layer to confirm currently-firing setups.
pub type EdgeIndex = std::collections::HashMap<String, Vec<crate::types::EligibleEdge>>;

/// Project the eligible rows of an edge map into an [`EdgeIndex`].
pub fn build_index(records: &[EdgeRecord]) -> EdgeIndex {
    let mut idx: EdgeIndex = std::collections::HashMap::new();
    for r in records.iter().filter(|r| r.eligible) {
        idx.entry(r.symbol.clone()).or_default().push(crate::types::EligibleEdge {
            strategy: r.strategy.clone(),
            direction: r.direction,
            expectancy_r: r.metrics.expectancy,
            profit_factor: r.metrics.profit_factor,
            win_pct: r.metrics.win_pct,
            n: r.metrics.n,
        });
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(day: u32, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle { day, open: o, high: h, low: l, close: c, volume: 1000.0 }
    }

    #[test]
    fn simulate_hits_target_then_stop() {
        // Day 0: enter long at bar0 close=100, atr=2 ⇒ risk=3 (k=1.5), target=+2R=106.
        // bar1 high 107 hits target ⇒ +2R minus cost.
        let bars = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 107.0, 99.0, 106.0),
            bar(0, 106.0, 106.0, 106.0, 106.0),
        ];
        let atr = vec![2.0, 2.0, 2.0];
        let rs = simulate(&bars, &atr, &[0], Direction::Long, 1.5, 2.0, 0.0);
        assert_eq!(rs.len(), 1);
        assert!((rs[0] - 2.0).abs() < 1e-9, "got {}", rs[0]);

        // Stop case: bar1 low 96 ≤ SL 95.5 ⇒ -1R.
        let bars2 = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 101.0, 96.0, 96.0),
            bar(0, 96.0, 96.0, 96.0, 96.0),
        ];
        let rs2 = simulate(&bars2, &atr, &[0], Direction::Long, 1.5, 2.0, 0.0);
        assert!((rs2[0] + 1.0).abs() < 1e-9, "got {}", rs2[0]);
    }

    #[test]
    fn metrics_basic() {
        let m = Metrics::from_rs(&[2.0, -1.0, 2.0, -1.0]);
        assert_eq!(m.n, 4);
        assert_eq!(m.win_pct, 50.0);
        assert!((m.expectancy - 0.5).abs() < 1e-9);
        assert!((m.profit_factor - 2.0).abs() < 1e-9);
    }
}

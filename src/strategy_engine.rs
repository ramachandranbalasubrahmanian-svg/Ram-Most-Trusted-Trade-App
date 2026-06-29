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

// Display-only robustness params for the edge-map tier. Mirror the deep-dive
// (`suggestion_engine`: OOS 0.30, embargo 0.02, 5 walk-forward folds) so the
// annotation is comparable to the per-stock card. These NEVER feed `eligible()`.
const ROBUST_OOS_FRACTION: f64 = 0.30;
const ROBUST_EMBARGO_FRACTION: f64 = 0.02;
const ROBUST_WF_FOLDS: usize = 5;

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

/// How the simulator resolves a bar whose OHLC range spans BOTH the stop and the
/// target. `PessimisticStopFirst` (the default) reproduces the legacy behaviour
/// exactly — the stop wins on a spanning bar, the worst-case for the trade — so
/// every cached edge map stays byte-identical. `IntrabarResolved` drops to
/// finer-resolution data to learn which level actually printed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AmbiguityPolicy {
    #[default]
    PessimisticStopFirst,
    IntrabarResolved,
}

/// Simulation knobs. `SimConfig::legacy(k, rr, cost)` is byte-for-byte identical
/// to the historical model (pessimistic stop-first, no finer data). `cost` is the
/// *effective* round-trip cost in fraction-of-notional — the slippage stress band
/// re-runs with a scaled `cost`, so this struct never needs a slippage multiplier.
pub struct SimConfig<'a> {
    pub k: f64,
    pub rr: f64,
    pub cost: f64,
    pub ambiguity: AmbiguityPolicy,
    /// Finer-tf bars for the SAME symbol, used only when `ambiguity ==
    /// IntrabarResolved`. `None` ⇒ fall back to pessimistic accounting.
    pub finer: Option<&'a [Candle]>,
}

impl SimConfig<'_> {
    /// The exact legacy model: pessimistic stop-first, no intrabar resolution.
    pub fn legacy(k: f64, rr: f64, cost: f64) -> SimConfig<'static> {
        SimConfig { k, rr, cost, ambiguity: AmbiguityPolicy::PessimisticStopFirst, finer: None }
    }
}

/// Full per-trade detail emitted by the fill core. `simulate`/`simulate_detailed`
/// project the subset they need, so their public signatures are unchanged.
#[derive(Debug, Clone, Copy)]
pub struct TradeOutcome {
    pub entry_idx: usize,
    pub exit_idx: usize,
    /// Realised R-multiple, net of round-trip cost.
    pub r: f64,
    /// True when the exit bar's range spanned BOTH the stop and the target.
    pub ambiguous: bool,
    /// True when finer-resolution data actually decided the order (not assumed).
    pub intrabar_resolved: bool,
    /// Max Adverse Excursion in R: the deepest the trade went AGAINST the position
    /// (entry→worst-held-bar) divided by the 1R risk. ~1.0 for stopped trades; for
    /// WINNERS it shows how much heat they took before working (≈1.0 ⇒ the stop is
    /// too tight). Observational only — never changes `r` or the exit.
    pub mae_r: f64,
}

/// Resolve a same-bar SL/target ambiguity using finer-resolution bars of `day`.
/// Returns the resolved exit price, or `None` when finer data can't disambiguate
/// (caller then falls back to pessimistic accounting). Wired by the `--intrabar`
/// path; with `finer == None` this is never reached.
fn resolve_intrabar(
    finer: &[Candle],
    day: u32,
    dir: Direction,
    sl: f64,
    tgt: f64,
) -> Option<f64> {
    for b in finer.iter().filter(|b| b.day == day) {
        let (hit_sl, hit_tgt) = match dir {
            Direction::Long => (b.low <= sl, b.high >= tgt),
            Direction::Short => (b.high >= sl, b.low <= tgt),
        };
        // A finer bar that still straddles both is itself ambiguous → stop-first.
        if hit_sl {
            return Some(sl);
        }
        if hit_tgt {
            return Some(tgt);
        }
    }
    None
}

/// The single fill core. Simulates one direction's worth of entries with
/// ATR-scaled stops/targets and an intraday-only exit (square-off at the day's
/// last bar), one position at a time. Returns each trade's full [`TradeOutcome`],
/// net of `cfg.cost`. With `SimConfig::legacy(..)` the realised R is byte-for-byte
/// identical to the historical `simulate`.
pub fn run_fill(
    bars: &[Candle],
    atr: &[f64],
    entries: &[usize],
    dir: Direction,
    cfg: &SimConfig,
) -> Vec<TradeOutcome> {
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
        let risk = cfg.k * a;
        let sl = entry - s * risk;
        let tgt = entry + s * cfg.rr * risk;

        let mut exit: Option<f64> = None;
        let mut exit_idx = idx;
        let mut ambiguous = false;
        let mut intrabar_resolved = false;
        // Max adverse excursion (price units), updated read-only as the trade runs.
        let mut worst_adverse = 0.0_f64;
        let mut j = idx + 1;
        while j < n && bars[j].day == day {
            let b = &bars[j];
            // Record how far this bar went AGAINST the position BEFORE deciding the
            // exit, so the exit bar's heat is included. Never affects `r`/exit.
            let adverse = match dir {
                Direction::Long => entry - b.low,
                Direction::Short => b.high - entry,
            };
            if adverse > worst_adverse {
                worst_adverse = adverse;
            }
            let (hit_sl, hit_tgt) = match dir {
                Direction::Long => (b.low <= sl, b.high >= tgt),
                Direction::Short => (b.high >= sl, b.low <= tgt),
            };
            if hit_sl && hit_tgt {
                // Same-bar ambiguity: legacy takes the stop. Intrabar mode tries
                // finer data; either path keeps the stop-first worst case if it
                // can't prove the target printed first.
                ambiguous = true;
                let resolved = match cfg.ambiguity {
                    AmbiguityPolicy::PessimisticStopFirst => None,
                    AmbiguityPolicy::IntrabarResolved => {
                        cfg.finer.and_then(|f| resolve_intrabar(f, day, dir, sl, tgt))
                    }
                };
                match resolved {
                    Some(p) => {
                        exit = Some(p);
                        intrabar_resolved = true;
                    }
                    None => exit = Some(sl),
                }
                exit_idx = j;
                break;
            } else if hit_sl {
                exit = Some(sl);
                exit_idx = j;
                break;
            } else if hit_tgt {
                exit = Some(tgt);
                exit_idx = j;
                break;
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
        let cost_r = cfg.cost * entry / risk; // round-trip cost expressed in R
        trades.push(TradeOutcome {
            entry_idx: idx,
            exit_idx: eidx,
            r: gross - cost_r,
            ambiguous,
            intrabar_resolved,
            mae_r: (worst_adverse / risk).max(0.0),
        });
        free_from = eidx + 1;
    }
    trades
}

/// Simulate one direction's worth of entries with ATR stops/targets and an
/// intraday-only exit. Returns the realised R-multiple of each trade, net of a
/// round-trip cost. Thin shim over [`run_fill`] in the exact legacy model.
pub fn simulate(
    bars: &[Candle],
    atr: &[f64],
    entries: &[usize],
    dir: Direction,
    k: f64,
    rr: f64,
    cost: f64,
) -> Vec<f64> {
    run_fill(bars, atr, entries, dir, &SimConfig::legacy(k, rr, cost))
        .into_iter()
        .map(|o| o.r)
        .collect()
}

/// Like [`simulate`], but returns each trade's `(entry_bar_index, R)` so callers
/// can split trades into in-sample / out-of-sample by position in history. Thin
/// shim over [`run_fill`] in the exact legacy model.
pub fn simulate_detailed(
    bars: &[Candle],
    atr: &[f64],
    entries: &[usize],
    dir: Direction,
    k: f64,
    rr: f64,
    cost: f64,
) -> Vec<(usize, f64)> {
    run_fill(bars, atr, entries, dir, &SimConfig::legacy(k, rr, cost))
        .into_iter()
        .map(|o| (o.entry_idx, o.r))
        .collect()
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
    /// Display-only robustness annotations (OOS / walk-forward / per-symbol DSR).
    /// Computed from the SAME trades as `metrics`; never feeds `eligible()`.
    /// `#[serde(default)]` so a pre-robustness cached edge map still loads.
    #[serde(default)]
    pub robustness: crate::types::Robustness,
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
    let (k, rr, cost) = (
        config::SL_ATR_MULT,
        config::DEFAULT_RR,
        crate::costs::backtest_roundtrip_pct(),
    );

    let total_bars = bars.len();
    let mut out = Vec::new();
    // Per-symbol trial Sharpes (this symbol's strategy×direction set) for DSR.
    let mut trial_sharpes: Vec<f64> = Vec::new();
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
            // Use simulate_detailed (== simulate, both via run_fill/SimConfig::legacy)
            // so `rs` is byte-identical to the legacy path — metrics + the anchor are
            // untouched — while also giving entry indices for OOS / walk-forward.
            let trades = simulate_detailed(&bars, &ind.atr, &entries, dir, k, rr, cost);
            if trades.is_empty() {
                continue;
            }
            let rs: Vec<f64> = trades.iter().map(|(_, r)| *r).collect();
            let metrics = Metrics::from_rs(&rs);
            let eligible = eligible(&metrics);

            // --- display-only robustness (never feeds eligible()) ---
            let (_is_rs, oos_rs) = crate::validation::purged_embargoed_split(
                &trades, total_bars, ROBUST_OOS_FRACTION, ROBUST_EMBARGO_FRACTION,
            );
            let oos_n = oos_rs.len();
            let oos_expectancy = if oos_rs.is_empty() {
                None
            } else {
                Some(crate::stats::mean(&oos_rs))
            };
            let wf_consistency =
                crate::validation::walkforward_consistency(&trades, total_bars, ROBUST_WF_FOLDS);
            let sharpe = crate::stats::sharpe_per_trade(&rs);
            trial_sharpes.push(sharpe);

            out.push(EdgeRecord {
                symbol: symbol.to_string(),
                strategy: strat.name().to_string(),
                direction: dir,
                timeframe: tf.dir().to_string(),
                metrics,
                eligible,
                robustness: crate::types::Robustness {
                    oos_expectancy,
                    oos_n,
                    wf_consistency,
                    dsr: 0.0, // filled below, once the trial set is complete
                },
            });
        }
    }
    // Second pass: per-symbol DSR deflates each record's Sharpe by the multiple
    // testing implied by ITS OWN strategy×direction trial set (≈26 trials).
    // `out` and `trial_sharpes` are pushed together ⇒ index-aligned 1:1.
    for (rec, &sharpe) in out.iter_mut().zip(trial_sharpes.iter()) {
        rec.robustness.dsr =
            crate::stats::deflated_sharpe(sharpe, rec.metrics.n, &trial_sharpes);
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

// ===========================================================================
// Edge-map freshness sidecar (honesty-layer only)
// ===========================================================================
//
// `EdgeMapMeta` records *when* and *over what scope* a map was built, so the
// dashboard can honestly report freshness ("541 of 1,558 symbols backtested;
// 125 new on disk since this map was built") instead of silently presenting a
// stale Top-10 as if it covered everything. It is written as a sibling
// `.meta.json` and never read by the eligibility gate, Confidence, ranking, or
// any decision path — purely a display surface. The map JSON itself is byte-
// identical to before this change, so the 63MOONS anchor is untouched.

/// Build-time scope/freshness metadata for an edge map. Display-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeMapMeta {
    pub timeframe: String,
    /// IST wall-clock at the moment the map was saved.
    pub built_ist: String,
    /// Universe size at build time (symbols with a `minute/` file).
    pub universe_at_build: usize,
    /// Distinct symbols actually carried in the map (had ≥100 bars + signals).
    pub backtested_symbols: usize,
    /// Distinct symbols with ≥1 eligible edge.
    pub eligible_symbols: usize,
    /// Total eligible edges across all symbols.
    pub eligible_edges: usize,
    /// Total records written (eligible + ineligible).
    pub total_records: usize,
    /// Sorted distinct symbols carried in the map — lets the status endpoint
    /// diff against the live universe to surface NEW-since-build names.
    pub symbols: Vec<String>,
}

impl EdgeMapMeta {
    /// Derive the sidecar from the records being saved. `universe_at_build` is
    /// the true on-disk universe (caller-supplied so a partial `backtest FOO`
    /// still reports the gap against the full universe, not just the subset).
    pub fn from_records(records: &[EdgeRecord], tf: Timeframe, universe_at_build: usize) -> Self {
        use std::collections::BTreeSet;
        let mut backtested: BTreeSet<&str> = BTreeSet::new();
        let mut eligible_syms: BTreeSet<&str> = BTreeSet::new();
        let mut eligible_edges = 0usize;
        for r in records {
            backtested.insert(r.symbol.as_str());
            if r.eligible {
                eligible_syms.insert(r.symbol.as_str());
                eligible_edges += 1;
            }
        }
        EdgeMapMeta {
            timeframe: tf.dir().to_string(),
            built_ist: now_ist_string(),
            universe_at_build,
            backtested_symbols: backtested.len(),
            eligible_symbols: eligible_syms.len(),
            eligible_edges,
            total_records: records.len(),
            symbols: backtested.into_iter().map(str::to_string).collect(),
        }
    }
}

/// IST wall-clock as `YYYY-MM-DD HH:MM:SS` (mirrors the suggestion engine's
/// `built_ist` format so timestamps read consistently across the UI).
fn now_ist_string() -> String {
    chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Path to the freshness sidecar for a timeframe (`cache/edge_map_{tf}.meta.json`).
pub fn edge_map_meta_path(tf: Timeframe) -> std::path::PathBuf {
    std::path::PathBuf::from("cache").join(format!("edge_map_{}.meta.json", tf.dir()))
}

/// Write the freshness sidecar alongside an already-saved edge map. A best-effort
/// honesty surface — callers may ignore the result (the map itself is the source
/// of truth; the sidecar is pure decoration).
pub fn save_edge_map_meta(records: &[EdgeRecord], tf: Timeframe, universe_at_build: usize) -> Result<()> {
    std::fs::create_dir_all("cache").context("create cache dir")?;
    let meta = EdgeMapMeta::from_records(records, tf, universe_at_build);
    let path = edge_map_meta_path(tf);
    let json = serde_json::to_vec_pretty(&meta)?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Load a previously-saved freshness sidecar, if present.
pub fn load_edge_map_meta(tf: Timeframe) -> Result<EdgeMapMeta> {
    let path = edge_map_meta_path(tf);
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Outcome of merging one symbol's freshly-backtested rows into an edge map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub symbol: String,
    pub timeframe: String,
    /// New rows written for this symbol (all strategy×direction records).
    pub records_added: usize,
    /// Of those, how many are eligible edges.
    pub eligible_added: usize,
    /// Whether this symbol already had rows in the map (replaced) or is brand new.
    pub replaced_existing: bool,
    /// Total records in the map after the merge.
    pub total_records: usize,
}

/// Merge one symbol's freshly-backtested records into the on-disk edge map for
/// `tf`, replacing only that symbol's rows and leaving every other symbol's
/// records byte-identical (they keep their values and relative order, so they
/// re-serialize unchanged). This is the incremental alternative to a full
/// universe rebuild: onboard a single stock in seconds without re-touching the
/// other ~1,500.
///
/// Anchor-safety: because unchanged symbols are byte-identical, the documented
/// edge-level anchor (e.g. `BAJFINANCE · gap_and_go · Short`) is untouched when
/// a *different* symbol is merged. The whole-file SHA1 does change — that is the
/// expected, intentional cost of adding data, and is why the regression guard is
/// the per-edge test, not a file hash. Persists both the map and its freshness
/// sidecar before returning.
pub fn merge_edge_records(
    symbol: &str,
    new_rows: Vec<EdgeRecord>,
    tf: Timeframe,
    universe_at_build: usize,
) -> Result<MergeOutcome> {
    let mut existing = load_edge_map(tf).unwrap_or_default();
    let records_added = new_rows.len();
    let eligible_added = new_rows.iter().filter(|r| r.eligible).count();
    let replaced_existing = existing.iter().any(|r| r.symbol == symbol);
    let path = edge_map_path(tf);

    // Fast path — onboarding a BRAND-NEW symbol. Splice the new records onto the
    // existing file as raw text so every other symbol's bytes are preserved
    // EXACTLY (not even a 1-ULP float-reserialization nudge). The maps on disk
    // were written by an older float formatter, so a load→save round-trip would
    // otherwise rewrite ~⅓ of all records at the last significant digit and churn
    // the SHA1 anchor. The splice is validated (re-parsed + counted) before any
    // write, falling back to the safe re-serialize path if anything looks off —
    // so it can never emit a malformed map.
    if !replaced_existing && !existing.is_empty() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(spliced) = splice_append_records(&text, &new_rows) {
                let expected = existing.len() + new_rows.len();
                if let Ok(parsed) = serde_json::from_str::<Vec<EdgeRecord>>(&spliced) {
                    if parsed.len() == expected {
                        std::fs::write(&path, &spliced)
                            .with_context(|| format!("write {}", path.display()))?;
                        save_edge_map_meta(&parsed, tf, universe_at_build)?;
                        return Ok(MergeOutcome {
                            symbol: symbol.to_string(),
                            timeframe: tf.dir().to_string(),
                            records_added,
                            eligible_added,
                            replaced_existing: false,
                            total_records: parsed.len(),
                        });
                    }
                }
            }
        }
    }

    // General path — replacing an existing symbol, an empty/absent map, or a
    // splice that failed validation. Parse → merge → save. (For a replace, the
    // re-serialize may ULP-nudge unchanged records; acceptable and rare.)
    merge_into(&mut existing, symbol, new_rows);
    save_edge_map(&existing, tf)?;
    save_edge_map_meta(&existing, tf, universe_at_build)?;

    Ok(MergeOutcome {
        symbol: symbol.to_string(),
        timeframe: tf.dir().to_string(),
        records_added,
        eligible_added,
        replaced_existing,
        total_records: existing.len(),
    })
}

/// Pure in-memory merge: drop `symbol`'s existing rows, append `new_rows` at the
/// end. Returns whether any existing rows were replaced. Every other symbol's
/// records keep their value and relative order, so they re-serialize unchanged.
fn merge_into(existing: &mut Vec<EdgeRecord>, symbol: &str, mut new_rows: Vec<EdgeRecord>) -> bool {
    let before = existing.len();
    existing.retain(|r| r.symbol != symbol);
    let replaced = existing.len() != before;
    existing.append(&mut new_rows);
    replaced
}

/// Append `new_rows` to a pretty-printed edge-map JSON array as raw text,
/// preserving the existing bytes verbatim. Produces output byte-identical to
/// `serde_json::to_vec_pretty` of the concatenated Vec *when the existing file
/// was written by the same formatter* — and, regardless, leaves every existing
/// record's bytes untouched. Returns the spliced document; the caller validates
/// it before writing.
fn splice_append_records(existing: &str, new_rows: &[EdgeRecord]) -> Result<String> {
    if new_rows.is_empty() {
        return Ok(existing.to_string());
    }
    // New records pretty-printed, with the outer array brackets stripped so they
    // sit at the same 2-space indent as the existing records.
    let new_pretty = serde_json::to_string_pretty(new_rows)?;
    let inner = new_pretty
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map(|s| s.trim_matches('\n'))
        .context("unexpected pretty-JSON shape for new records")?;

    let close = existing.rfind(']').context("edge map has no closing ]")?;
    let head = existing[..close].trim_end(); // up to and including the last record's '}'
    if head == "[" {
        // Existing array was empty (`[]` / `[\n]`).
        return Ok(format!("[\n{inner}\n]"));
    }
    Ok(format!("{head},\n{inner}\n]"))
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
            robustness: r.robustness.clone(),
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
    fn run_fill_records_mae_without_changing_r() {
        // Winner: enter long 100, risk=1.5·2=3, target +2R=106. bar1 high 107 hits
        // target but its low 99 ⇒ adverse 1 ⇒ MAE = 1/3 ≈ 0.333R. r is still +2R.
        let bars = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 107.0, 99.0, 106.0),
            bar(0, 106.0, 106.0, 106.0, 106.0),
        ];
        let atr = vec![2.0, 2.0, 2.0];
        let outs = run_fill(&bars, &atr, &[0], Direction::Long, &SimConfig::legacy(1.5, 2.0, 0.0));
        assert_eq!(outs.len(), 1);
        assert!((outs[0].r - 2.0).abs() < 1e-9, "r unchanged, got {}", outs[0].r);
        assert!((outs[0].mae_r - (1.0 / 3.0)).abs() < 1e-9, "MAE got {}", outs[0].mae_r);
        // Stop: bar1 low 96 ⇒ adverse 4 ⇒ MAE = 4/3 (gapped through the −1R stop).
        let bars2 = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 101.0, 96.0, 96.0),
            bar(0, 96.0, 96.0, 96.0, 96.0),
        ];
        let outs2 = run_fill(&bars2, &atr, &[0], Direction::Long, &SimConfig::legacy(1.5, 2.0, 0.0));
        assert!((outs2[0].r + 1.0).abs() < 1e-9);
        assert!((outs2[0].mae_r - (4.0 / 3.0)).abs() < 1e-9, "MAE got {}", outs2[0].mae_r);
    }

    #[test]
    fn metrics_basic() {
        let m = Metrics::from_rs(&[2.0, -1.0, 2.0, -1.0]);
        assert_eq!(m.n, 4);
        assert_eq!(m.win_pct, 50.0);
        assert!((m.expectancy - 0.5).abs() < 1e-9);
        assert!((m.profit_factor - 2.0).abs() < 1e-9);
    }

    /// ANCHOR GUARD: `SimConfig::legacy` must be byte-for-byte identical to the
    /// historical `simulate`. Drive a varied multi-day, multi-entry sequence
    /// through both the shim and a direct `run_fill(legacy)` and assert exact
    /// equality — this is what protects the cached edge maps from drifting.
    #[test]
    fn legacy_simconfig_matches_old_simulate() {
        let bars = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 107.0, 99.0, 106.0), // target
            bar(0, 106.0, 106.0, 96.0, 100.0),
            bar(1, 100.0, 101.0, 95.0, 96.0), // stop next day
            bar(1, 96.0, 99.0, 95.5, 98.0),
            bar(1, 98.0, 98.0, 98.0, 98.0),
            bar(2, 98.0, 98.0, 98.0, 98.0), // square-off (neither hit)
            bar(2, 98.0, 99.0, 97.5, 98.5),
        ];
        let atr = vec![2.0; bars.len()];
        let entries = [0usize, 3, 6];
        for dir in [Direction::Long, Direction::Short] {
            for &(k, rr, cost) in &[(1.5, 2.0, 0.0), (1.0, 3.0, 0.0012), (2.0, 1.0, 0.0006)] {
                let old = simulate(&bars, &atr, &entries, dir, k, rr, cost);
                let new: Vec<f64> = run_fill(&bars, &atr, &entries, dir, &SimConfig::legacy(k, rr, cost))
                    .into_iter()
                    .map(|o| o.r)
                    .collect();
                assert_eq!(old.len(), new.len(), "trade count {dir:?} k{k} rr{rr}");
                for (a, b) in old.iter().zip(&new) {
                    assert_eq!(a.to_bits(), b.to_bits(), "R bits differ {dir:?} k{k} rr{rr}");
                }
            }
        }
    }

    /// A bar whose range spans BOTH the stop and the target is flagged
    /// `ambiguous`, and under the (default) pessimistic policy resolves to the
    /// stop (−1R), with `intrabar_resolved == false`.
    #[test]
    fn ambiguous_bar_flagged_and_pessimistic() {
        // Long entry at 100, atr=2, k=1.5 ⇒ risk 3, SL 95.5, target 106.
        // bar1 low 95 (≤SL) AND high 107 (≥target) ⇒ ambiguous, pessimistic = stop.
        let bars = vec![
            bar(0, 100.0, 100.0, 100.0, 100.0),
            bar(0, 100.0, 107.0, 95.0, 100.0),
            bar(0, 100.0, 100.0, 100.0, 100.0),
        ];
        let atr = vec![2.0; bars.len()];
        let out = run_fill(&bars, &atr, &[0], Direction::Long, &SimConfig::legacy(1.5, 2.0, 0.0));
        assert_eq!(out.len(), 1);
        assert!(out[0].ambiguous, "should flag the spanning bar");
        assert!(!out[0].intrabar_resolved);
        assert!((out[0].r + 1.0).abs() < 1e-9, "pessimistic ⇒ −1R, got {}", out[0].r);
    }

    /// The freshness sidecar counts DISTINCT backtested symbols, distinct
    /// eligible symbols, and total eligible edges — and preserves the supplied
    /// universe size so the dashboard can report the coverage gap honestly.
    #[test]
    fn edge_map_meta_counts_distinct_symbols_and_edges() {
        let rec = |sym: &str, eligible: bool| EdgeRecord {
            symbol: sym.to_string(),
            strategy: "VWAP".to_string(),
            direction: Direction::Long,
            timeframe: "30min".to_string(),
            metrics: Metrics::from_rs(&[]),
            eligible,
            robustness: Default::default(),
        };
        let records = vec![
            rec("AAA", true),
            rec("AAA", false), // same symbol, a second (ineligible) edge
            rec("BBB", true),
            rec("BBB", true), // two eligible edges on BBB
            rec("CCC", false), // backtested but nothing eligible
        ];
        let meta = EdgeMapMeta::from_records(&records, Timeframe::Min30, 1558);
        assert_eq!(meta.universe_at_build, 1558);
        assert_eq!(meta.total_records, 5);
        assert_eq!(meta.backtested_symbols, 3); // AAA, BBB, CCC
        assert_eq!(meta.eligible_symbols, 2); // AAA, BBB
        assert_eq!(meta.eligible_edges, 3); // AAA×1 + BBB×2
        assert_eq!(meta.symbols, vec!["AAA", "BBB", "CCC"]); // sorted, distinct
        assert_eq!(meta.timeframe, "30min");
    }

    /// The per-symbol merge must leave every OTHER symbol byte-identical (this is
    /// the anchor-safety guarantee for onboarding), be idempotent when replacing
    /// a symbol that already exists, and append a brand-new symbol cleanly.
    #[test]
    fn merge_into_preserves_unchanged_symbols_and_is_idempotent() {
        let rec = |sym: &str, strat: &str, eligible: bool| EdgeRecord {
            symbol: sym.to_string(),
            strategy: strat.to_string(),
            direction: Direction::Short,
            timeframe: "15min".to_string(),
            metrics: Metrics::from_rs(&[1.0, -0.5, 1.0]),
            eligible,
            robustness: Default::default(),
        };
        let mut map = vec![
            rec("AAA", "vwap_trend", true),
            rec("BAJFINANCE", "gap_and_go", true), // the anchor edge — must not move
            rec("CCC", "opening_range", false),
        ];
        // Snapshot the anchor row's exact serialization.
        let anchor_before = serde_json::to_string(&map[1]).unwrap();

        // Merge a BRAND-NEW symbol: nothing replaced, anchor untouched.
        let replaced = merge_into(&mut map, "NEWSTK", vec![rec("NEWSTK", "gap_and_go", true)]);
        assert!(!replaced, "new symbol should not replace anything");
        let anchor_after = serde_json::to_string(map.iter().find(|r| r.symbol == "BAJFINANCE").unwrap()).unwrap();
        assert_eq!(anchor_before, anchor_after, "anchor symbol drifted on unrelated merge");
        assert!(map.iter().any(|r| r.symbol == "NEWSTK"), "new symbol added");
        assert_eq!(map.len(), 4);

        // Replace an EXISTING symbol: old rows gone, anchor still untouched.
        let replaced = merge_into(&mut map, "AAA", vec![rec("AAA", "prev_day_breakout", false)]);
        assert!(replaced, "existing symbol should be replaced");
        assert_eq!(map.iter().filter(|r| r.symbol == "AAA").count(), 1, "no duplicate AAA rows");
        assert_eq!(map.iter().find(|r| r.symbol == "AAA").unwrap().strategy, "prev_day_breakout");
        let anchor_after2 = serde_json::to_string(map.iter().find(|r| r.symbol == "BAJFINANCE").unwrap()).unwrap();
        assert_eq!(anchor_before, anchor_after2, "anchor symbol drifted on replace");
    }

    /// The byte-preserving append splice must (a) leave the existing records'
    /// bytes verbatim, (b) round-trip to the right records, and (c) be identical
    /// to a full pretty-serialize when the existing file used the same formatter.
    #[test]
    fn splice_append_preserves_existing_bytes() {
        let rec = |sym: &str, strat: &str, eligible: bool| EdgeRecord {
            symbol: sym.to_string(),
            strategy: strat.to_string(),
            direction: Direction::Short,
            timeframe: "30min".to_string(),
            metrics: Metrics::from_rs(&[0.3, -0.7, 1.1, -0.4]),
            eligible,
            robustness: Default::default(),
        };
        let a = rec("AAA", "vwap_cross", true);
        let b = rec("BAJFINANCE", "gap_and_go", true);
        let c = rec("ZZZNEW", "orb_15m", false);

        let existing = serde_json::to_string_pretty(&vec![a.clone(), b.clone()]).unwrap();
        let spliced = splice_append_records(&existing, std::slice::from_ref(&c)).unwrap();

        // (a) the unchanged prefix (everything up to the last '}') is verbatim.
        let prefix = &existing[..=existing.rfind('}').unwrap()];
        assert!(spliced.starts_with(prefix), "existing bytes were not preserved");

        // (b) parses back to exactly A, B, C.
        let parsed: Vec<EdgeRecord> = serde_json::from_str(&spliced).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].symbol, "AAA");
        assert_eq!(parsed[1].symbol, "BAJFINANCE");
        assert_eq!(parsed[2].symbol, "ZZZNEW");

        // (c) canonical: identical to a full pretty-serialize of [A,B,C].
        let full = serde_json::to_string_pretty(&vec![a, b, c]).unwrap();
        assert_eq!(spliced, full, "splice diverged from canonical serialization");

        // empty-array edge case.
        let from_empty = splice_append_records("[]", std::slice::from_ref(&parsed[2])).unwrap();
        assert_eq!(serde_json::from_str::<Vec<EdgeRecord>>(&from_empty).unwrap().len(), 1);
    }

    /// REGRESSION ANCHOR (edge-map tier) — the Rust project's documented anchor
    /// edge (UPGRADE_PLAN.md §0). RE-COMPUTES BAJFINANCE·gap_and_go·Short on 15min
    /// via the live engine (not the cached JSON), so it guards the actual
    /// COMPUTATION rather than a possibly-stale file. Skips when the archive is
    /// absent (fresh clone / CI).
    ///
    /// RE-BASELINED 2026-06-28 after a full-universe rebuild. The prior anchor
    /// (`exp=0.1433565560483712 / PF=1.2659776591373888`) was a STALE CACHE value
    /// from before the itemized cost model (commit 7ec0a3f): the cached 15min map
    /// had never been rebuilt, so it under-counted cost. Current code — current
    /// itemized cost, SAME n=130 trades, byte-identical fill engine — produces the
    /// values below. The earlier cache-READING version of this test masked the
    /// divergence (it asserted the stale file and passed); re-computing prevents
    /// that. The `63MOONS·15m·n=51` figure was the *Python* project's anchor and
    /// never applied here (deep-dive anchored separately in
    /// `suggestion_engine::tests::anchor_63moons_deep_dive_stable`).
    #[test]
    fn anchor_bajfinance_edge_map_stable() {
        let root = config::data_root();
        let f = config::parquet_path(&root, "BAJFINANCE", Timeframe::Min15);
        if !f.exists() {
            eprintln!("SKIP anchor_bajfinance_edge_map_stable: archive absent ({})", f.display());
            return;
        }
        let conn = storage_kernel::open_conn().expect("duckdb conn");
        let records =
            backtest_symbol(&conn, &root, "BAJFINANCE", Timeframe::Min15).expect("backtest BAJFINANCE 15min");
        let r = records
            .iter()
            .find(|r| r.strategy == "gap_and_go" && r.direction == Direction::Short)
            .expect("BAJFINANCE gap_and_go Short edge present");
        assert_eq!(r.metrics.n, 130, "anchor n drifted");
        assert!(
            (r.metrics.expectancy - 0.13012804335828682).abs() < 1e-12,
            "anchor expectancy drifted: {}",
            r.metrics.expectancy
        );
        assert!(
            (r.metrics.profit_factor - 1.2383992474235814).abs() < 1e-12,
            "anchor PF drifted: {}",
            r.metrics.profit_factor
        );
    }

    #[test]
    fn robustness_annotations_populated_and_in_range() {
        // The display-only robustness columns compute alongside metrics from the
        // SAME trades. Verify they populate and stay in sane ranges — without
        // touching the (anchor-verified) metrics. Skips when the archive is absent.
        let root = config::data_root();
        let f = config::parquet_path(&root, "BAJFINANCE", Timeframe::Min15);
        if !f.exists() {
            eprintln!("SKIP robustness_annotations_populated_and_in_range: archive absent");
            return;
        }
        let conn = storage_kernel::open_conn().expect("duckdb conn");
        let records =
            backtest_symbol(&conn, &root, "BAJFINANCE", Timeframe::Min15).expect("backtest");
        let r = records
            .iter()
            .find(|r| r.strategy == "gap_and_go" && r.direction == Direction::Short)
            .expect("anchor edge present");
        let rob = &r.robustness;
        // OOS held out from a 130-trade edge ⇒ a non-trivial OOS tail.
        assert!(rob.oos_n > 0, "expected some OOS trades, got {}", rob.oos_n);
        assert!(rob.oos_n < r.metrics.n, "OOS must be a subset of all trades");
        assert!((0.0..=1.0).contains(&rob.wf_consistency), "wf out of range: {}", rob.wf_consistency);
        assert!((0.0..=1.0).contains(&rob.dsr), "dsr out of range: {}", rob.dsr);
        // The anchor's metrics MUST be unchanged by the robustness pass.
        assert_eq!(r.metrics.n, 130, "metrics must be byte-identical");
    }
}

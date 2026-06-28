//! Intraday Suggestion engine: the per-stock deep-dive + universe scanner.
//!
//! For a symbol it backtests the four page strategies (VWAP Trend, Opening
//! Range, Prev-Day Breakout, Gap-and-Go) across 6 intervals × 2 sides × 5 R:R
//! configs (~240 combos), holds out the last 30% as out-of-sample, computes the
//! full stat suite (via [`crate::stats`]), and picks the best per strategy. The
//! scanner runs the same across the universe and returns the Top-10 Buy / Sell
//! by Confidence.
//!
//! Honesty-first: every R / P&L is net of round-trip cost; verdicts state plainly
//! when there is no after-cost edge.

use std::cell::RefCell;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use rayon::prelude::*;

use crate::config::{self, Direction, Timeframe};
use crate::stats::{
    self, ConfInput, ConvInput, build_confidence, compute_conviction, deflated_sharpe,
    expectancy_ci, monte_carlo, shrunk_expectancy,
};
#[allow(unused_imports)]
use crate::storage_kernel::{self, Candle, discover_symbols, load_candles, open_conn};
use crate::strategy_engine::{
    Indicators, SimConfig, compute_indicators, run_fill, simulate_detailed,
};
use crate::types::{
    ConvictionDelta, FinderResult, FinderRow, RegimeInfo, ScanResult, ScannerRow, SetupCard,
    StockSuggestion, StrategyBlock, SuggestStrategy,
};

/// Intervals scanned per stock (matches the page: 3m/5m/10m/15m/30m/1h).
pub const SUGGEST_INTERVALS: [Timeframe; 6] = [
    Timeframe::Min3,
    Timeframe::Min5,
    Timeframe::Min10,
    Timeframe::Min15,
    Timeframe::Min30,
    Timeframe::Min60,
];

/// The five R:R configurations as (sl_atr_mult, reward:risk).
pub const RR_CONFIGS: [(f64, f64); 5] = [
    (1.0, 1.5),
    (1.0, 2.0),
    (1.0, 3.0),
    (0.75, 2.0),
    (1.25, 2.0),
];

/// Round-trip cost used by the suggestion backtests (~0.16%, matching the
/// Python project's documented cost model).
pub const SUGGEST_COST: f64 = 0.0016;

/// Out-of-sample fraction held out from the tail of history.
pub const OOS_FRACTION: f64 = 0.30;

/// Minimum trades for a non-provisional Confidence (matches the Python scoring).
#[allow(dead_code)]
const MIN_SAMPLE: usize = 30;
/// Minimum trades to keep a config at all (provisional floor).
const PROVISIONAL_MIN: usize = 20;
/// Confidence is capped here while a config is still provisional (20..30 trades).
#[allow(dead_code)]
const PROVISIONAL_CAP: u32 = 55;
/// Hard ceiling on Confidence under normal conditions.
#[allow(dead_code)]
const HARD_CAP_NORMAL: u32 = 90;

/// Per-symbol metadata for the picker / header.
#[derive(Debug, Clone, Default)]
pub struct SymbolMeta {
    pub intervals: Vec<String>,
    pub trading_days: usize,
    pub last_date: String,
    pub days_old: i64,
}

/// Pretty display name for an interval (e.g. "1 Hour", "5 Minutes").
fn pretty_interval(tf: Timeframe) -> &'static str {
    match tf {
        Timeframe::Min3 => "3 Minutes",
        Timeframe::Min5 => "5 Minutes",
        Timeframe::Min10 => "10 Minutes",
        Timeframe::Min15 => "15 Minutes",
        Timeframe::Min30 => "30 Minutes",
        Timeframe::Min60 => "1 Hour",
        Timeframe::Minute => "1 Minute",
        Timeframe::Daily | Timeframe::DailyLong => "1 Day",
    }
}

/// Which intervals a symbol has on disk, plus day/recency metadata.
pub fn symbol_meta(root: &Path, symbol: &str) -> SymbolMeta {
    let mut meta = SymbolMeta::default();

    // Collect available intervals (pretty names) in SUGGEST_INTERVALS order, and
    // remember the finest available one for counting trading days.
    let mut finest: Option<Timeframe> = None;
    for &tf in SUGGEST_INTERVALS.iter() {
        let path = config::parquet_path(root, symbol, tf);
        if path.exists() {
            meta.intervals.push(pretty_interval(tf).to_string());
            if finest.is_none() {
                finest = Some(tf);
            }
        }
    }

    let Some(tf) = finest else {
        return meta;
    };
    let Ok(conn) = open_conn() else {
        return meta;
    };
    let Ok(bars) = load_candles(&conn, root, symbol, tf) else {
        return meta;
    };
    if bars.is_empty() {
        return meta;
    }

    // Distinct trading days = number of distinct `day` ids (contiguous 0-based).
    meta.trading_days = bars.last().map(|b| b.day as usize + 1).unwrap_or(0);

    // Last date (YYYY-MM-DD) of the last bar — re-query the raw date string so we
    // report the calendar date, since Candle only carries a synthetic day id.
    if let Some(last) = last_bar_date(&conn, root, symbol, tf) {
        let today = chrono::Utc::now().with_timezone(&config::IST).date_naive();
        if let Ok(d) = chrono::NaiveDate::parse_from_str(&last, "%Y-%m-%d") {
            meta.days_old = (today - d).num_days();
        }
        meta.last_date = last;
    }
    meta
}

/// Read the last bar's calendar date (YYYY-MM-DD) directly from the parquet.
fn last_bar_date(conn: &duckdb::Connection, root: &Path, symbol: &str, tf: Timeframe) -> Option<String> {
    let path = config::parquet_path(root, symbol, tf);
    let p = path.to_string_lossy().replace('\'', "''");
    let sql = format!(
        "SELECT CAST(max(date) AS DATE)::VARCHAR FROM read_parquet('{p}')"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let mut rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?;
    rows.next()?.ok()
}

// ---------------------------------------------------------------------------
// Signal helpers (pure, NaN-guarded)
// ---------------------------------------------------------------------------

/// `x[i]` crosses above scalar series `y[i]` at bar `i`. NaN-safe.
#[inline]
fn cross_up(x: &[f64], y: &[f64], i: usize) -> bool {
    i > 0
        && x[i - 1].is_finite()
        && y[i - 1].is_finite()
        && x[i].is_finite()
        && y[i].is_finite()
        && x[i - 1] <= y[i - 1]
        && x[i] > y[i]
}

/// `x[i]` crosses below scalar series `y[i]` at bar `i`. NaN-safe.
#[inline]
fn cross_down(x: &[f64], y: &[f64], i: usize) -> bool {
    i > 0
        && x[i - 1].is_finite()
        && y[i - 1].is_finite()
        && x[i].is_finite()
        && y[i].is_finite()
        && x[i - 1] >= y[i - 1]
        && x[i] < y[i]
}

/// Per-bar prior-day high / low. For each bar, the high/low of the *previous*
/// trading day (NaN for bars on the very first day). Built from the `day` ids.
fn prior_day_high_low(bars: &[Candle]) -> (Vec<f64>, Vec<f64>) {
    let n = bars.len();
    let mut ph = vec![f64::NAN; n];
    let mut pl = vec![f64::NAN; n];
    if n == 0 {
        return (ph, pl);
    }
    // Aggregate each day's high/low keyed by contiguous day id.
    let last_day = bars[n - 1].day as usize;
    let mut day_hi = vec![f64::MIN; last_day + 1];
    let mut day_lo = vec![f64::MAX; last_day + 1];
    for b in bars {
        let d = b.day as usize;
        day_hi[d] = day_hi[d].max(b.high);
        day_lo[d] = day_lo[d].min(b.low);
    }
    for (i, b) in bars.iter().enumerate() {
        let d = b.day as usize;
        if d > 0 {
            ph[i] = day_hi[d - 1];
            pl[i] = day_lo[d - 1];
        }
    }
    (ph, pl)
}

/// Entry signal indices for a (strategy, direction) on a bar series.
fn strategy_signals(
    strat: SuggestStrategy,
    dir: Direction,
    bars: &[Candle],
    ind: &Indicators,
    closes: &[f64],
    prior_hi: &[f64],
    prior_lo: &[f64],
) -> Vec<usize> {
    let n = bars.len();
    let mut out = Vec::new();
    match strat {
        SuggestStrategy::VwapTrend => {
            for i in 1..n {
                match dir {
                    Direction::Long if cross_up(closes, &ind.vwap, i) => out.push(i),
                    Direction::Short if cross_down(closes, &ind.vwap, i) => out.push(i),
                    _ => {}
                }
            }
        }
        SuggestStrategy::OpeningRange => {
            for i in 1..n {
                match dir {
                    Direction::Long if cross_up(closes, &ind.orh, i) => out.push(i),
                    Direction::Short if cross_down(closes, &ind.orl, i) => out.push(i),
                    _ => {}
                }
            }
        }
        SuggestStrategy::GapAndGo => {
            const THR: f64 = 0.5; // percent
            let mut last_day = u32::MAX;
            for i in 0..n {
                if bars[i].day != last_day {
                    last_day = bars[i].day;
                    let g = ind.gap_pct[i];
                    if g.is_finite() {
                        let bullish = bars[i].close >= bars[i].open;
                        match dir {
                            Direction::Long if g > THR && bullish => out.push(i),
                            Direction::Short if g < -THR && !bullish => out.push(i),
                            _ => {}
                        }
                    }
                }
            }
        }
        SuggestStrategy::PrevDayBreakout => {
            for i in 1..n {
                match dir {
                    Direction::Long if cross_up(closes, prior_hi, i) => out.push(i),
                    Direction::Short if cross_down(closes, prior_lo, i) => out.push(i),
                    _ => {}
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Per-config metrics
// ---------------------------------------------------------------------------

/// Everything computed for one (strategy, tf, dir, sl_mult, rr) config. A few
/// fields (`oos_rs`, `wilson_low`) are kept for parity with the Python card model
/// even though the engine recomputes their derived figures directly.
#[allow(dead_code)]
#[derive(Clone)]
struct ConfigStat {
    strat: SuggestStrategy,
    tf: Timeframe,
    dir: Direction,
    sl_mult: f64,
    rr: f64,

    rs: Vec<f64>,
    oos_rs: Vec<f64>,

    n: usize,
    win_rate: f64,
    expectancy: f64,
    profit_factor: f64,
    max_drawdown_r: f64,
    total_r: f64,
    sharpe: f64,
    calmar: f64,
    t_stat: f64,
    recent_20_wr: f64,
    max_loss_streak: usize,
    skew: f64,
    kurt: f64,

    oos_n: usize,
    oos_win_rate: f64,
    oos_expectancy: Option<f64>,

    // last-bar context for the card
    entry: f64,
    atr: f64,

    // v2 reliability inputs
    trades: Vec<(usize, f64)>,
    wf_consistency: f64,
    dsr: f64,
    robustness_pct: f64,
    regime_consistent: Option<bool>,

    // honesty stats (display-only — NEVER enter ConfInput / Confidence)
    // Fraction of trades whose exit bar's range spanned BOTH stop and target
    // (resolved pessimistically as a stop). High ⇒ the edge leans on the fill
    // assumption. Slippage stress band: net expectancy at 2×/3× the slippage
    // allowance — "is it still +EV if fills are worse than modelled?".
    ambiguous_frac: f64,
    exp_2x_slip: f64,
    exp_3x_slip: f64,

    // confidence (computed lazily during selection)
    confidence: Option<u32>,
    confidence_band: String,
    wilson_low: f64,
    p_value: f64,
    provisional: bool,
}

/// Embargo gap (fraction of bars) purged between in-sample and OOS to kill
/// boundary look-ahead leakage.
const EMBARGO_FRACTION: f64 = 0.02;
/// Walk-forward folds used for the consistency check.
const WF_FOLDS: usize = 5;
/// Minimum trades per regime before regime consistency can be judged.
const REGIME_MIN_EACH: usize = 10;

/// Profit factor over rs: sum(+) / |sum(-)|, capped at 99, inf -> 99.
fn profit_factor(rs: &[f64]) -> f64 {
    let mut pos = 0.0;
    let mut neg = 0.0;
    for &r in rs {
        if r > 0.0 {
            pos += r;
        } else {
            neg += r;
        }
    }
    let neg = neg.abs();
    if neg > 0.0 {
        (pos / neg).min(99.0)
    } else if pos > 0.0 {
        99.0
    } else {
        0.0
    }
}

/// Win rate as a percentage (R > 0 counts as a win).
fn win_rate_pct(rs: &[f64]) -> f64 {
    if rs.is_empty() {
        return 0.0;
    }
    let wins = rs.iter().filter(|r| **r > 0.0).count();
    wins as f64 / rs.len() as f64 * 100.0
}

/// Build a `ConfigStat` from a config's detailed trades.
fn build_config_stat(
    strat: SuggestStrategy,
    tf: Timeframe,
    dir: Direction,
    sl_mult: f64,
    rr: f64,
    trades: &[(usize, f64)],
    total_bars: usize,
    entry: f64,
    atr: f64,
) -> ConfigStat {
    let rs: Vec<f64> = trades.iter().map(|(_, r)| *r).collect();
    // Purged + embargoed OOS split (removes boundary look-ahead leakage).
    let (_is_rs, oos_rs) =
        crate::validation::purged_embargoed_split(trades, total_bars, OOS_FRACTION, EMBARGO_FRACTION);
    // Walk-forward fold consistency (cheap; computed for every config).
    let wf_consistency = crate::validation::walkforward_consistency(trades, total_bars, WF_FOLDS);

    let n = rs.len();
    let expectancy = stats::mean(&rs);
    let (skew, kurt) = stats::skew_kurt(&rs);
    let oos_n = oos_rs.len();
    let oos_expectancy = if oos_rs.is_empty() {
        None
    } else {
        Some(stats::mean(&oos_rs))
    };

    ConfigStat {
        strat,
        tf,
        dir,
        sl_mult,
        rr,
        n,
        win_rate: win_rate_pct(&rs),
        expectancy,
        profit_factor: profit_factor(&rs),
        max_drawdown_r: stats::max_drawdown_r(&rs),
        total_r: rs.iter().sum(),
        sharpe: stats::sharpe_per_trade(&rs),
        calmar: stats::calmar(&rs),
        t_stat: stats::t_stat(&rs),
        recent_20_wr: stats::recent_win_rate(&rs, 20),
        max_loss_streak: stats::max_loss_streak(&rs),
        skew,
        kurt,
        oos_n,
        oos_win_rate: win_rate_pct(&oos_rs),
        oos_expectancy,
        entry,
        atr,
        trades: trades.to_vec(),
        wf_consistency,
        dsr: 1.0,            // neutral until the reliability pre-pass fills it
        robustness_pct: 1.0, // neutral until the reliability pre-pass fills it
        regime_consistent: None,
        // Honesty stats filled by the deep-dive loop (which has the bars); default
        // to the baseline expectancy / zero-ambiguity for paths that don't set them.
        ambiguous_frac: 0.0,
        exp_2x_slip: expectancy,
        exp_3x_slip: expectancy,
        confidence: None,
        confidence_band: String::new(),
        wilson_low: 0.0,
        p_value: 1.0,
        provisional: false,
        rs,
        oos_rs,
    }
}

/// Compute (and cache) the statistical Confidence for a config.
fn ensure_confidence(cs: &mut ConfigStat) {
    if cs.confidence_band.is_empty() && cs.confidence.is_none() {
        let inp = ConfInput {
            n_trades: cs.n,
            win_rate_pct: cs.win_rate,
            expectancy_r: cs.expectancy,
            profit_factor: cs.profit_factor,
            max_drawdown_r: cs.max_drawdown_r,
            total_r: cs.total_r,
            recent_20_wr_pct: cs.recent_20_wr,
            oos_win_rate_pct: if cs.oos_n > 0 { Some(cs.oos_win_rate) } else { None },
            oos_expectancy_r: cs.oos_expectancy,
            max_loss_streak: cs.max_loss_streak,
            t_stat: cs.t_stat,
            dsr: cs.dsr,
            wf_consistency: cs.wf_consistency,
            robustness_pct: cs.robustness_pct,
            regime_consistent: cs.regime_consistent,
        };
        let conf = build_confidence(&inp);
        cs.confidence = conf.score;
        cs.confidence_band = conf.band;
        cs.wilson_low = conf.wilson_low;
        cs.p_value = conf.p_value;
        cs.provisional = conf.provisional;
    }
}

/// Confidence selection key: None treated as -1.0, tie-break by expectancy.
fn selection_key(cs: &ConfigStat) -> (f64, f64) {
    let c = cs.confidence.map(|c| c as f64).unwrap_or(-1.0);
    (c, cs.expectancy)
}

/// Last finite ATR value of a series (price units).
fn last_finite_atr(atr: &[f64]) -> f64 {
    atr.iter().rev().copied().find(|a| a.is_finite()).unwrap_or(0.0)
}

/// Fraction of 5 sequential trade-windows with a positive mean R (walk-forward
/// consistency proxy for conviction). Returns a value in [0, 1].
fn wf_consistency(rs: &[f64]) -> f64 {
    if rs.is_empty() {
        return 0.0;
    }
    let k = 5usize;
    let n = rs.len();
    let win = (n / k).max(1);
    let mut positive = 0;
    let mut windows = 0;
    for w in 0..k {
        let start = w * win;
        if start >= n {
            break;
        }
        let end = if w == k - 1 { n } else { ((w + 1) * win).min(n) };
        let slice = &rs[start..end];
        if slice.is_empty() {
            continue;
        }
        windows += 1;
        if stats::mean(slice) > 0.0 {
            positive += 1;
        }
    }
    if windows == 0 {
        0.0
    } else {
        positive as f64 / windows as f64
    }
}

// ---------------------------------------------------------------------------
// Card + block builders
// ---------------------------------------------------------------------------

/// Build the full `SetupCard` for the chosen best config of a strategy.
#[allow(clippy::too_many_arguments)]
fn build_setup_card(
    symbol: &str,
    cs: &ConfigStat,
    capital: f64,
    risk_pct: f64,
    trial_sharpes: &[f64],
    timeframes_agree: u32,
    n_configs_searched: usize,
) -> SetupCard {
    let dsr = deflated_sharpe(cs.sharpe, cs.n, trial_sharpes);
    let mc = monte_carlo(&cs.rs, 5000, 42);
    let ci = expectancy_ci(&cs.rs, 5000, 42);
    // Harder shrink toward 0 for small samples (prior strength 40 ≈ 40 trades).
    let shrunk = shrunk_expectancy(cs.expectancy, cs.n, 0.0, 40.0);

    let (mc_prob_profit, mc_p95_dd_r) = match mc {
        Some(m) => (m.prob_profit, m.p95_maxdd_r),
        None => (0.0, 0.0),
    };
    let (exp_ci_low, exp_ci_high) = match ci {
        Some(c) => (c.p05, c.p95),
        None => (0.0, 0.0),
    };

    let conv_in = ConvInput {
        mtf_agreement: timeframes_agree as f64,
        mc_prob_profit,
        wf_consistency: wf_consistency(&cs.rs),
        dsr,
        oos_exp_r: cs.oos_expectancy,
        oos_n: if cs.oos_n > 0 { Some(cs.oos_n) } else { None },
        skew: Some(cs.skew),
        kurt: Some(cs.kurt),
    };
    let (conviction, conviction_label, conviction_deltas): (u32, String, Vec<ConvictionDelta>) =
        compute_conviction(&conv_in);

    // Sizing + price levels.
    let atr = cs.atr;
    let entry = cs.entry;
    let sl_mult = cs.sl_mult;
    let rr = cs.rr;
    let side = cs.dir.as_str().to_string();
    let sl_dist = sl_mult * atr;

    let (sl, target) = match cs.dir {
        Direction::Long => (entry - sl_dist, entry + rr * sl_dist),
        Direction::Short => (entry + sl_dist, entry - rr * sl_dist),
    };

    let risk_amount = capital * risk_pct;
    let qty_by_risk = if sl_dist > 0.0 {
        (risk_amount / sl_dist).floor()
    } else {
        0.0
    };
    let qty_by_lev = if entry > 0.0 {
        (capital * config::LEVERAGE / entry).floor()
    } else {
        0.0
    };
    let quantity = qty_by_risk.min(qty_by_lev).max(0.0) as i64;
    let max_risk = quantity as f64 * sl_dist;
    let max_reward = quantity as f64 * rr * sl_dist;

    // Net of itemized round-trip costs (broker + STT + exchange/SEBI + GST +
    // stamp + slippage) for this exact position size.
    let long = matches!(cs.dir, Direction::Long);
    let (net_profit, costs_target) = crate::costs::net_pnl(quantity, entry, target, long);
    let (net_loss, _) = crate::costs::net_pnl(quantity, entry, sl, long);

    let prob_score = cs.win_rate;
    let prob_floor = stats::wilson_lower(cs.win_rate / 100.0, cs.n) * 100.0;

    let selection_artifact = if dsr < 0.05 {
        Some(format!(
            "Likely a selection artifact - best of {n} entry setups searched; after the Deflated-Sharpe \
correction only ~{pct:.0}% probability the edge is real. Among the weakest on this measure - skip, or \
trade only minimum size.",
            n = n_configs_searched,
            pct = dsr * 100.0
        ))
    } else {
        None
    };

    SetupCard {
        symbol: symbol.to_string(),
        side,
        interval: pretty_interval(cs.tf).to_string(),
        rr,
        rr_label: format!("1 : {rr:.1}"),
        timeframes_agree,

        entry,
        last_close: entry,
        sl,
        sl_atr_mult: sl_mult,
        target,
        target_atr_mult: rr * sl_mult,
        atr,

        quantity,
        risk_pct,
        max_risk,
        max_reward,
        net_profit,
        net_loss,
        costs: costs_target,

        win_rate: cs.win_rate,
        profit_factor: cs.profit_factor,
        expectancy_r: cs.expectancy,
        n_trades: cs.n,
        sharpe: cs.sharpe,
        calmar: cs.calmar,

        mc_prob_profit,
        mc_p95_dd_r,
        dsr,
        exp_ci_low,
        exp_ci_high,
        exp_shrunk: shrunk,

        ambiguous_frac: cs.ambiguous_frac,
        exp_2x_slip: cs.exp_2x_slip,
        exp_3x_slip: cs.exp_3x_slip,

        prob_score,
        prob_floor,
        confidence: cs.confidence,
        confidence_band: cs.confidence_band.clone(),
        t_stat: cs.t_stat,
        p_value: cs.p_value,
        provisional: cs.provisional,
        conviction,
        conviction_label,
        conviction_deltas,

        selection_artifact,
        // Display-only shortlist gate: Confidence + Wilson floor + DSR. Computed
        // from already-scored values; cannot feed back into Confidence.
        shortlist: stats::is_high_conviction_shortlist(
            cs.confidence,
            prob_floor,
            dsr,
            config::shortlist_min_confidence(),
            config::shortlist_min_prob(),
            config::SHORTLIST_DSR_MIN,
        ),
    }
}

/// Build a strategy block from its configs (already confidence-scored). `configs`
/// are all configs (any n) belonging to this strategy.
fn build_strategy_block(
    strat: SuggestStrategy,
    symbol: &str,
    configs: &mut [ConfigStat],
    capital: f64,
    risk_pct: f64,
    trial_sharpes: &[f64],
    n_configs_searched: usize,
) -> StrategyBlock {
    // Valid setups = positive expectancy & n >= PROVISIONAL_MIN.
    let valid_setups = configs
        .iter()
        .filter(|c| c.n >= PROVISIONAL_MIN && c.expectancy > 0.0)
        .count();

    // timeframes_agree = distinct intervals where this strategy+dir had a
    // positive-expectancy config (any n that we kept, i.e. n >= PROVISIONAL_MIN).
    let mut agree_tfs = std::collections::BTreeSet::new();
    for c in configs.iter() {
        if c.n >= PROVISIONAL_MIN && c.expectancy > 0.0 {
            agree_tfs.insert(c.tf.dir());
        }
    }
    let timeframes_agree = agree_tfs.len() as u32;

    // Pick best by Confidence (None -> -1), tie-break expectancy.
    let best_idx = configs
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            let ka = selection_key(a);
            let kb = selection_key(b);
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i);

    let any_positive = valid_setups > 0;

    // No positive-expectancy setup with enough trades -> honest "no edge".
    if !any_positive {
        return StrategyBlock {
            key: strat.key().to_string(),
            name: strat.name().to_string(),
            emoji: strat.emoji().to_string(),
            description: strat.description().to_string(),
            verdict: "no_edge".to_string(),
            verdict_text: format!(
                "No positive-expectancy setup. This strategy ran enough trades on {symbol} but none beat \
costs + out-of-sample. Honest result: no edge here right now."
            ),
            confidence_note: String::new(),
            headline: None,
            best: None,
            valid_setups: 0,
        };
    }

    let bi = best_idx.expect("any_positive implies a config exists");
    let card = build_setup_card(
        symbol,
        &configs[bi],
        capital,
        risk_pct,
        trial_sharpes,
        timeframes_agree,
        n_configs_searched,
    );

    let c_opt = card.confidence;
    let c = c_opt.unwrap_or(0);
    let wr = card.win_rate;
    let pf = card.profit_factor;
    let exp = card.expectancy_r;
    let wins = wr.round() as i64; // wins out of a notional 100
    let losses = 100 - wins;

    let (verdict, verdict_text, confidence_note, headline) = if c_opt.is_some() && c >= 80 {
        (
            "shortlist".to_string(),
            "STRONG HISTORICAL STATS - but net edge approx 0 after cost; shortlist only, forward-test \
small if it FIRES with conviction 70+"
                .to_string(),
            format!(
                "confidence {c} (strong historical stats, NOT a tradeable edge) - intraday population net \
edge is ~0 after cost"
            ),
            Some(format!(
                "Historically won ~{wr:.0} of 100 ({wins}-{losses}) - winners {pf:.1}x losers - \
{exp:+.2}R/trade after costs"
            )),
        )
    } else if c_opt.is_some() && (60..80).contains(&c) {
        (
            "watch".to_string(),
            "MODERATE - historical edge, unproven net of cost; watch / forward-test small".to_string(),
            format!("confidence {c} - net edge unproven"),
            None,
        )
    } else {
        // C < 60, provisional, or None.
        let mut note = match c_opt {
            Some(c) => format!("confidence {c} below 60 - no validated edge"),
            None => "confidence below 60 - no validated edge".to_string(),
        };
        if card.exp_ci_low < 0.0 {
            note.push_str(" - expected-profit range includes zero");
        }
        (
            "skip".to_string(),
            "SKIP - weak / unreliable edge".to_string(),
            note,
            None,
        )
    };

    StrategyBlock {
        key: strat.key().to_string(),
        name: strat.name().to_string(),
        emoji: strat.emoji().to_string(),
        description: strat.description().to_string(),
        verdict,
        verdict_text,
        confidence_note,
        headline,
        best: Some(card),
        valid_setups,
    }
}

// ---------------------------------------------------------------------------
// analyze_symbol
// ---------------------------------------------------------------------------

/// Full per-stock suggestion: 4 strategy blocks, each with its best setup.
/// `capital` in INR and `risk_pct` as a fraction (e.g. 0.025 = 2.5%); the page
/// allows 0.25%–5%, which is why this takes raw values, not `UserSettings`.
pub fn analyze_symbol(
    root: &Path,
    symbol: &str,
    capital: f64,
    risk_pct: f64,
) -> Result<StockSuggestion> {
    let conn = open_conn()?;
    let meta = symbol_meta(root, symbol);

    // NIFTY-regime map (once) + per-interval bar dates, for regime conditioning.
    let regime_map = crate::regime::nifty_regime_map(&conn);
    let mut tf_dates: std::collections::HashMap<Timeframe, Vec<String>> =
        std::collections::HashMap::new();

    // The per-interval config sweep (6 intervals × 4 strat × 2 dir × 5 R:R = 240
    // configs) is the dominant cost (~4s sequential). Each interval is fully
    // independent — its own candle load, indicators, and simulations — so we run
    // them in parallel, one DuckDB connection per rayon worker (the same
    // thread-local pattern the scanner/finder use). The outputs are merged back in
    // `SUGGEST_INTERVALS` order so `trial_sharpes` and `by_strategy` end up in the
    // exact same sequence as the old sequential loop ⇒ byte-identical results
    // (the regression-anchor invariant). The NIFTY regime map is read once, above,
    // before this region — the parallel tasks never touch the outer `conn`.
    struct IntervalOut {
        tf: Timeframe,
        dates: Option<Vec<String>>,
        by_strategy: [Vec<ConfigStat>; 4],
        sharpes: Vec<f64>,
        used: bool,
    }
    thread_local! {
        static ANALYZE_CONN: std::cell::RefCell<Option<duckdb::Connection>> =
            const { std::cell::RefCell::new(None) };
    }

    let parts: Vec<IntervalOut> = SUGGEST_INTERVALS
        .par_iter()
        .map(|&tf| {
            let mut out = IntervalOut {
                tf,
                dates: None,
                by_strategy: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
                sharpes: Vec::new(),
                used: false,
            };
            ANALYZE_CONN.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() {
                    *slot = open_conn().ok();
                }
                let conn = match slot.as_ref() {
                    Some(c) => c,
                    None => return,
                };
                let path = config::parquet_path(root, symbol, tf);
                if !path.exists() {
                    return;
                }
                let bars = match load_candles(conn, root, symbol, tf) {
                    Ok(b) if b.len() >= 2 => b,
                    _ => return,
                };
                out.used = true;
                if let Ok(d) = storage_kernel::load_candle_dates(conn, root, symbol, tf) {
                    out.dates = Some(d);
                }

                let ind = compute_indicators(&bars, tf.minutes());
                let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
                let (prior_hi, prior_lo) = prior_day_high_low(&bars);
                let last_close = *closes.last().unwrap();
                let last_atr = last_finite_atr(&ind.atr);
                let total_bars = bars.len();

                for (si, strat) in SuggestStrategy::all().into_iter().enumerate() {
                    for dir in [Direction::Long, Direction::Short] {
                        let entries =
                            strategy_signals(strat, dir, &bars, &ind, &closes, &prior_hi, &prior_lo);
                        if entries.is_empty() {
                            continue;
                        }
                        for &(sl_mult, rr) in RR_CONFIGS.iter() {
                            // Run the fill core directly so we can read the same-bar
                            // ambiguity flags (which `simulate_detailed` hides).
                            let cost1 = crate::costs::backtest_roundtrip_pct();
                            let outs = run_fill(
                                &bars, &ind.atr, &entries, dir, &SimConfig::legacy(sl_mult, rr, cost1),
                            );
                            if outs.is_empty() {
                                continue;
                            }
                            let trades: Vec<(usize, f64)> =
                                outs.iter().map(|o| (o.entry_idx, o.r)).collect();
                            let ambiguous_frac =
                                outs.iter().filter(|o| o.ambiguous).count() as f64 / outs.len() as f64;
                            let mut cs = build_config_stat(
                                strat, tf, dir, sl_mult, rr, &trades, total_bars, last_close, last_atr,
                            );
                            cs.ambiguous_frac = ambiguous_frac;
                            // Every config's sharpe feeds the DSR trial set.
                            out.sharpes.push(cs.sharpe);
                            if cs.n >= PROVISIONAL_MIN {
                                // Slippage stress band: re-fill at 2×/3× the slippage
                                // allowance (exit decisions are identical; only the
                                // per-trade cost changes) and record net expectancy.
                                let mean = |o: &[crate::strategy_engine::TradeOutcome]| {
                                    if o.is_empty() { 0.0 } else { o.iter().map(|x| x.r).sum::<f64>() / o.len() as f64 }
                                };
                                let c2 = crate::costs::backtest_roundtrip_pct_scaled(2.0);
                                let c3 = crate::costs::backtest_roundtrip_pct_scaled(3.0);
                                cs.exp_2x_slip = mean(&run_fill(
                                    &bars, &ind.atr, &entries, dir, &SimConfig::legacy(sl_mult, rr, c2),
                                ));
                                cs.exp_3x_slip = mean(&run_fill(
                                    &bars, &ind.atr, &entries, dir, &SimConfig::legacy(sl_mult, rr, c3),
                                ));
                                out.by_strategy[si].push(cs);
                            }
                        }
                    }
                }
            });
            out
        })
        .collect();

    // Deterministic merge (par_iter preserves SUGGEST_INTERVALS order) — this
    // reproduces the sequential push order exactly.
    let mut by_strategy: Vec<Vec<ConfigStat>> = vec![Vec::new(); 4];
    let mut trial_sharpes: Vec<f64> = Vec::new();
    let mut intervals_used = 0usize;
    for part in parts {
        if part.used {
            intervals_used += 1;
        }
        if let Some(d) = part.dates {
            tf_dates.insert(part.tf, d);
        }
        trial_sharpes.extend(part.sharpes);
        for (si, group) in part.by_strategy.into_iter().enumerate() {
            by_strategy[si].extend(group);
        }
    }

    // --- reliability pre-pass: DSR (multiple-testing), parameter robustness
    //     across the R:R configs, and NIFTY-regime conditioning — computed before
    //     confidence scoring so the DSR gate + new penalties can apply. ---
    let mut group_exp: std::collections::HashMap<(usize, Timeframe, Direction), Vec<f64>> =
        std::collections::HashMap::new();
    for (si, group) in by_strategy.iter().enumerate() {
        for cs in group {
            group_exp
                .entry((si, cs.tf, cs.dir))
                .or_default()
                .push(cs.expectancy);
        }
    }
    for (si, group) in by_strategy.iter_mut().enumerate() {
        for cs in group.iter_mut() {
            // DSR over EVERY config searched (the multiple-testing trial set).
            cs.dsr = deflated_sharpe(cs.sharpe, cs.n, &trial_sharpes);
            // Robustness across the R:R configs of this (strategy, tf, dir).
            cs.robustness_pct = crate::validation::parameter_robustness(
                group_exp
                    .get(&(si, cs.tf, cs.dir))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            );
            // Regime conditioning: split this config's trades by NIFTY up/down day.
            if let Some(dates) = tf_dates.get(&cs.tf) {
                let mut up: Vec<f64> = Vec::new();
                let mut down: Vec<f64> = Vec::new();
                for &(idx, r) in &cs.trades {
                    if let Some(day) = dates.get(idx) {
                        match regime_map.get(day) {
                            Some(true) => up.push(r),
                            Some(false) => down.push(r),
                            None => {}
                        }
                    }
                }
                cs.regime_consistent =
                    crate::regime::regime_consistency(&up, &down, REGIME_MIN_EACH);
            }
            ensure_confidence(cs);
        }
    }

    let n_configs_searched = trial_sharpes.len();

    // Build the 4 blocks in canonical order.
    let mut blocks: Vec<StrategyBlock> = Vec::with_capacity(4);
    for (si, strat) in SuggestStrategy::all().into_iter().enumerate() {
        let block = build_strategy_block(
            strat,
            symbol,
            &mut by_strategy[si],
            capital,
            risk_pct,
            &trial_sharpes,
            n_configs_searched,
        );
        blocks.push(block);
    }

    // best_overall = highest-confidence card across blocks. Blocks are in
    // `SuggestStrategy::all()` order, so the index recovers the owning strategy.
    let best_overall = SuggestStrategy::all()
        .into_iter()
        .zip(blocks.iter())
        .filter_map(|(strat, b)| b.best.as_ref().map(|card| (strat, card)))
        .max_by(|(_, a), (_, b)| {
            let ca = a.confidence.map(|c| c as f64).unwrap_or(-1.0);
            let cb = b.confidence.map(|c| c as f64).unwrap_or(-1.0);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(strat, card)| best_overall_string(strat, card));

    let total_configs = 4 * intervals_used * 2 * 5;

    Ok(StockSuggestion {
        symbol: symbol.to_string(),
        intervals_available: meta.intervals,
        trading_days: meta.trading_days,
        last_date: meta.last_date,
        days_old: meta.days_old,
        best_overall,
        blocks,
        total_configs,
        disclaimer: DISCLAIMER.to_string(),
    })
}

// ---------------------------------------------------------------------------
// scan_universe
// ---------------------------------------------------------------------------

/// Light per-config metric bundle for the scanner (no full card).
struct ScanBest {
    strat: SuggestStrategy,
    tf: Timeframe,
    rr: f64,
    confidence: u32,
    expectancy: f64,
    win_rate: f64,
    profit_factor: f64,
    n: usize,
    entry: f64,
    prob_floor: f64,
    shortlist: bool,
}

/// Run the LIGHT search for one symbol: Min15/Min30/Min60 only, all 4 strategies
/// × 2 dirs × RR_CONFIGS. Returns (best_buy, best_sell) by Confidence (>=50).
fn scan_symbol(conn: &duckdb::Connection, root: &Path, symbol: &str) -> (Option<ScanBest>, Option<ScanBest>) {
    const LIGHT_TFS: [Timeframe; 3] = [Timeframe::Min15, Timeframe::Min30, Timeframe::Min60];

    // First pass: collect candidate configs per side AND every config's sharpe
    // (the multiple-testing trial set), exactly as the deep-dive does. The old
    // code scored each config with the neutral `dsr = 1.0`, so the DSR gate never
    // fired in the scanner — that's why a name could read 89 here but 59 in its
    // deep-dive. We now apply the gate over the trial set below.
    let mut buy_cands: Vec<ConfigStat> = Vec::new();
    let mut sell_cands: Vec<ConfigStat> = Vec::new();
    let mut trial_sharpes: Vec<f64> = Vec::new();

    for &tf in LIGHT_TFS.iter() {
        let path = config::parquet_path(root, symbol, tf);
        if !path.exists() {
            continue;
        }
        let bars = match load_candles(conn, root, symbol, tf) {
            Ok(b) if b.len() >= 2 => b,
            _ => continue,
        };
        let ind = compute_indicators(&bars, tf.minutes());
        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let (prior_hi, prior_lo) = prior_day_high_low(&bars);
        let last_close = *closes.last().unwrap();
        let total_bars = bars.len();
        let last_atr = last_finite_atr(&ind.atr);

        for strat in SuggestStrategy::all() {
            for dir in [Direction::Long, Direction::Short] {
                let entries =
                    strategy_signals(strat, dir, &bars, &ind, &closes, &prior_hi, &prior_lo);
                if entries.is_empty() {
                    continue;
                }
                for &(sl_mult, rr) in RR_CONFIGS.iter() {
                    let trades = simulate_detailed(
                        &bars, &ind.atr, &entries, dir, sl_mult, rr, crate::costs::backtest_roundtrip_pct(),
                    );
                    if trades.is_empty() {
                        continue;
                    }
                    let cs = build_config_stat(
                        strat, tf, dir, sl_mult, rr, &trades, total_bars, last_close, last_atr,
                    );
                    // Every config feeds the DSR trial set (matches the deep-dive).
                    trial_sharpes.push(cs.sharpe);
                    if cs.n < PROVISIONAL_MIN {
                        continue;
                    }
                    match dir {
                        Direction::Long => buy_cands.push(cs),
                        Direction::Short => sell_cands.push(cs),
                    }
                }
            }
        }
    }

    // Second pass: deflate each candidate's Sharpe against the full trial set,
    // score it through the same `build_confidence` gate the deep-dive uses, then
    // pick the best per side. The scanner can no longer present an ungated number.
    (gate_and_pick(buy_cands, &trial_sharpes), gate_and_pick(sell_cands, &trial_sharpes))
}

/// Apply the DSR gate over `trial_sharpes` to each candidate, score it, and
/// return the best (by Confidence, then expectancy) that clears Confidence ≥ 50.
/// This is the scanner's counterpart to the deep-dive's reliability pre-pass.
fn gate_and_pick(cands: Vec<ConfigStat>, trial_sharpes: &[f64]) -> Option<ScanBest> {
    let mut best: Option<ScanBest> = None;
    for mut cs in cands {
        cs.dsr = deflated_sharpe(cs.sharpe, cs.n, trial_sharpes);
        ensure_confidence(&mut cs);
        let Some(conf) = cs.confidence else { continue };
        if conf < 50 {
            continue;
        }
        let prob_floor = stats::wilson_lower(cs.win_rate / 100.0, cs.n) * 100.0;
        let shortlist = stats::is_high_conviction_shortlist(
            cs.confidence,
            prob_floor,
            cs.dsr,
            crate::config::shortlist_min_confidence(),
            crate::config::shortlist_min_prob(),
            crate::config::SHORTLIST_DSR_MIN,
        );
        let cand = ScanBest {
            strat: cs.strat,
            tf: cs.tf,
            rr: cs.rr,
            confidence: conf,
            expectancy: cs.expectancy,
            win_rate: cs.win_rate,
            profit_factor: cs.profit_factor,
            n: cs.n,
            entry: cs.entry,
            prob_floor,
            shortlist,
        };
        let better = match &best {
            None => true,
            Some(b) => (conf, cs.expectancy) > (b.confidence, b.expectancy),
        };
        if better {
            best = Some(cand);
        }
    }
    best
}

fn scan_best_to_row(symbol: &str, side: &str, b: &ScanBest) -> ScannerRow {
    ScannerRow {
        symbol: symbol.to_string(),
        side: side.to_string(),
        strategy: b.strat.name().to_string(),
        interval: pretty_interval(b.tf).to_string(),
        rr_label: format!("1 : {:.1}", b.rr),
        confidence: b.confidence,
        expectancy_r: b.expectancy,
        win_rate: b.win_rate,
        profit_factor: b.profit_factor,
        n_trades: b.n,
        entry: b.entry,
        reliability: "scan".to_string(),
        prob_floor: b.prob_floor,
        shortlist: b.shortlist,
    }
}

/// Scan the universe, returning the Top-10 Buy / Sell setups by Confidence.
pub fn scan_universe(root: &Path, symbols: &[String], _capital: f64, _risk_pct: f64) -> ScanResult {
    thread_local! {
        static SCAN_CONN: RefCell<Option<duckdb::Connection>> = const { RefCell::new(None) };
    }

    let rows: Vec<(Option<ScannerRow>, Option<ScannerRow>)> = symbols
        .par_iter()
        .map(|sym| {
            SCAN_CONN.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() {
                    *slot = open_conn().ok();
                }
                let Some(conn) = slot.as_ref() else {
                    return (None, None);
                };
                let (buy, sell) = scan_symbol(conn, root, sym);
                (
                    buy.map(|b| scan_best_to_row(sym, "BUY", &b)),
                    sell.map(|b| scan_best_to_row(sym, "SELL", &b)),
                )
            })
        })
        .collect();

    let mut top_buy: Vec<ScannerRow> = Vec::new();
    let mut top_sell: Vec<ScannerRow> = Vec::new();
    for (b, s) in rows {
        if let Some(r) = b {
            top_buy.push(r);
        }
        if let Some(r) = s {
            top_sell.push(r);
        }
    }
    top_buy.sort_by(|a, b| b.confidence.cmp(&a.confidence));
    top_sell.sort_by(|a, b| b.confidence.cmp(&a.confidence));
    top_buy.truncate(10);
    top_sell.truncate(10);

    let built_ist = chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    ScanResult {
        top_buy,
        top_sell,
        scanned: symbols.len(),
        built_ist,
    }
}

// ---------------------------------------------------------------------------
// find_capital_fit (Capital-Fit ATR finder)
// ---------------------------------------------------------------------------

/// Best single setup for a symbol (any side), carrying the fields sizing needs.
/// Capital/risk-INDEPENDENT — this is the cacheable heavy result; sizing happens
/// later in [`find_capital_fit`].
struct FitBest {
    symbol: String,
    strat: SuggestStrategy,
    tf: Timeframe,
    dir: Direction,
    sl_mult: f64,
    rr: f64,
    confidence: u32,
    expectancy: f64,
    win_rate: f64,
    profit_factor: f64,
    n: usize,
    entry: f64,
    atr: f64,
}

/// Light search: the single best setup (either side) for one symbol by
/// Confidence (>=50), with the ATR / sl_mult / direction needed for sizing.
fn fit_symbol(conn: &duckdb::Connection, root: &Path, symbol: &str) -> Option<FitBest> {
    const LIGHT_TFS: [Timeframe; 3] = [Timeframe::Min15, Timeframe::Min30, Timeframe::Min60];
    let mut best: Option<FitBest> = None;
    for &tf in LIGHT_TFS.iter() {
        let path = config::parquet_path(root, symbol, tf);
        if !path.exists() {
            continue;
        }
        let bars = match load_candles(conn, root, symbol, tf) {
            Ok(b) if b.len() >= 2 => b,
            _ => continue,
        };
        let ind = compute_indicators(&bars, tf.minutes());
        let closes: Vec<f64> = bars.iter().map(|b| b.close).collect();
        let (prior_hi, prior_lo) = prior_day_high_low(&bars);
        let last_close = *closes.last().unwrap();
        let atr = last_finite_atr(&ind.atr);
        let total_bars = bars.len();

        for strat in SuggestStrategy::all() {
            for dir in [Direction::Long, Direction::Short] {
                let entries =
                    strategy_signals(strat, dir, &bars, &ind, &closes, &prior_hi, &prior_lo);
                if entries.is_empty() {
                    continue;
                }
                for &(sl_mult, rr) in RR_CONFIGS.iter() {
                    let trades = simulate_detailed(
                        &bars, &ind.atr, &entries, dir, sl_mult, rr,
                        crate::costs::backtest_roundtrip_pct(),
                    );
                    if trades.is_empty() {
                        continue;
                    }
                    let mut cs = build_config_stat(
                        strat, tf, dir, sl_mult, rr, &trades, total_bars, last_close, atr,
                    );
                    if cs.n < PROVISIONAL_MIN {
                        continue;
                    }
                    ensure_confidence(&mut cs);
                    let Some(conf) = cs.confidence else { continue };
                    if conf < 50 {
                        continue;
                    }
                    let better = match &best {
                        None => true,
                        Some(b) => (conf, cs.expectancy) > (b.confidence, b.expectancy),
                    };
                    if better {
                        best = Some(FitBest {
                            symbol: symbol.to_string(),
                            strat,
                            tf,
                            dir,
                            sl_mult,
                            rr,
                            confidence: conf,
                            expectancy: cs.expectancy,
                            win_rate: cs.win_rate,
                            profit_factor: cs.profit_factor,
                            n: cs.n,
                            entry: last_close,
                            atr,
                        });
                    }
                }
            }
        }
    }
    best
}

/// IST calendar date `YYYY-MM-DD` — the version key for the date-stable fit universe.
fn today_ist_date() -> String {
    chrono::Utc::now().with_timezone(&config::IST).format("%Y-%m-%d").to_string()
}

/// Process-wide cache of the capital/risk-INDEPENDENT fit universe (the heavy
/// per-symbol backtest search). Keyed by `date|symbol-count`, so it rebuilds once
/// a day or when the universe size changes (a stock was onboarded). Mirrors
/// `capital_planner`'s date-keyed cache.
static FIT_UNIVERSE_CACHE: OnceLock<Mutex<Option<(String, Arc<Vec<FitBest>>)>>> = OnceLock::new();

/// Build (or return cached) the per-symbol best edges. This is the expensive part
/// of the finder — 3 timeframes × strategy library × R:R configs per symbol — and
/// it does NOT depend on capital or risk, so it is computed once and reused across
/// every capital/risk the user dials in. The first build (or startup warm) pays
/// the full universe scan; subsequent slider changes only re-run the cheap sizing
/// loop in [`find_capital_fit`].
fn fit_universe(root: &Path, symbols: &[String]) -> Arc<Vec<FitBest>> {
    let version = format!("{}|{}", today_ist_date(), symbols.len());
    let cell = FIT_UNIVERSE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(g) = cell.lock() {
        if let Some((v, data)) = &*g {
            if *v == version {
                return data.clone();
            }
        }
    }

    thread_local! {
        static FIT_CONN: RefCell<Option<duckdb::Connection>> = const { RefCell::new(None) };
    }
    let bests: Vec<FitBest> = symbols
        .par_iter()
        .filter_map(|sym| {
            FIT_CONN.with(|cell| {
                let mut slot = cell.borrow_mut();
                if slot.is_none() {
                    *slot = open_conn().ok();
                }
                let conn = slot.as_ref()?;
                fit_symbol(conn, root, sym)
            })
        })
        .collect();

    let data = Arc::new(bests);
    if let Ok(mut g) = cell.lock() {
        *g = Some((version, data.clone()));
    }
    data
}

/// Capital-Fit ATR finder. For the given `capital` + `risk_pct`, size every
/// symbol's best edge to YOUR account and return ALL that fit (≥ 1 tradeable
/// share within 5× buying power), ranked by fit-adjusted edge
/// (Confidence × deployability). Net P&L is after itemized round-trip costs.
///
/// The heavy backtest search is cached in [`fit_universe`] (capital/risk-
/// independent), so changing the capital/risk sliders only re-runs the cheap
/// sizing loop below — turning a full ~80s rescan into a sub-second resize.
pub fn find_capital_fit(
    root: &Path,
    symbols: &[String],
    capital: f64,
    risk_pct: f64,
) -> FinderResult {
    let universe = fit_universe(root, symbols);

    let risk_amount = capital * risk_pct;
    let leverage = config::LEVERAGE;
    let mut rows: Vec<FinderRow> = Vec::new();

    for b in universe.iter() {
        let sl_dist = b.sl_mult * b.atr;
        if sl_dist <= 0.0 || b.entry <= 0.0 {
            continue;
        }
        let shares_by_risk = (risk_amount / sl_dist).floor().max(0.0);
        let max_affordable = (capital * leverage / b.entry).floor().max(0.0);
        let shares = shares_by_risk.min(max_affordable);
        if shares < 1.0 {
            continue; // can't take even one share within buying power → not a fit
        }
        let shares_i = shares as i64;
        let long = matches!(b.dir, Direction::Long);
        let (sl, target) = if long {
            (b.entry - sl_dist, b.entry + b.rr * sl_dist)
        } else {
            (b.entry + sl_dist, b.entry - b.rr * sl_dist)
        };
        let (net_profit, _) = crate::costs::net_pnl(shares_i, b.entry, target, long);
        let (net_loss, _) = crate::costs::net_pnl(shares_i, b.entry, sl, long);

        let capital_deployed = shares * b.entry;
        let max_notional = capital * leverage;
        let capital_efficiency_pct = if max_notional > 0.0 {
            capital_deployed / max_notional * 100.0
        } else {
            0.0
        };
        let risk_taken = shares * sl_dist;
        let deployability = if shares_by_risk > 0.0 {
            (shares / shares_by_risk).min(1.0)
        } else {
            0.0
        };
        let fit = if shares_by_risk <= max_affordable {
            "ideal"
        } else {
            "leverage_bound"
        };
        let fit_score = b.confidence as f64 * deployability;

        rows.push(FinderRow {
            symbol: b.symbol.clone(),
            strategy: b.strat.name().to_string(),
            side: b.dir.as_str().to_string(),
            interval: pretty_interval(b.tf).to_string(),
            rr_label: format!("1 : {:.1}", b.rr),
            entry: b.entry,
            atr: b.atr,
            sl,
            target,
            shares: shares_i,
            shares_by_risk: shares_by_risk as i64,
            max_affordable: max_affordable as i64,
            fit: fit.to_string(),
            capital_deployed,
            capital_efficiency_pct,
            risk_taken,
            net_profit,
            net_loss,
            confidence: b.confidence,
            expectancy_r: b.expectancy,
            win_rate: b.win_rate,
            profit_factor: b.profit_factor,
            n_trades: b.n,
            fit_score,
        });
    }

    rows.sort_by(|a, b| {
        b.fit_score
            .partial_cmp(&a.fit_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let qualifying = rows.len();
    let built_ist = chrono::Utc::now()
        .with_timezone(&config::IST)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    FinderResult {
        capital,
        risk_pct,
        rows,
        qualifying,
        scanned: symbols.len(),
        built_ist,
    }
}

// ---------------------------------------------------------------------------
// compute_regime
// ---------------------------------------------------------------------------

/// Read NIFTY50 daily closes (oldest-first) from `index_daily/NIFTY50.parquet`.
/// The index set lives alongside the per-symbol archive under the data root; we
/// honour the spec's `config::data_root()/index_daily/NIFTY50.parquet` location.
fn read_nifty_closes(conn: &duckdb::Connection, _root: &Path) -> Option<Vec<f64>> {
    let path = config::data_root().join("index_daily").join("NIFTY50.parquet");
    if !path.exists() {
        return None;
    }
    let p = path.to_string_lossy().replace('\'', "''");
    let sql = format!("SELECT close FROM read_parquet('{p}') ORDER BY date");
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt.query_map([], |row| row.get::<_, f64>(0)).ok()?;
    let out: Vec<f64> = rows.flatten().collect();
    if out.is_empty() { None } else { Some(out) }
}

/// Last close vs prev close for one symbol's `1day` parquet (for breadth).
fn last_two_daily_closes(conn: &duckdb::Connection, root: &Path, symbol: &str) -> Option<(f64, f64)> {
    let path = config::parquet_path(root, symbol, Timeframe::Daily);
    if !path.exists() {
        return None;
    }
    let p = path.to_string_lossy().replace('\'', "''");
    let sql = format!(
        "SELECT close FROM read_parquet('{p}') ORDER BY date DESC LIMIT 2"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt.query_map([], |row| row.get::<_, f64>(0)).ok()?;
    let vals: Vec<f64> = rows.flatten().collect();
    if vals.len() == 2 {
        // DESC: vals[0] = last, vals[1] = prev.
        Some((vals[0], vals[1]))
    } else {
        None
    }
}

/// NIFTY regime + market breadth (display-only context). Resilient: any read
/// error falls back to sensible defaults and never panics.
pub fn compute_regime(root: &Path, symbols: &[String]) -> RegimeInfo {
    let mut info = RegimeInfo {
        nifty_regime: "Flat".to_string(),
        breadth_up: 0,
        breadth_down: 0,
        breadth_label: "neutral".to_string(),
        built_ist: chrono::Utc::now()
            .with_timezone(&crate::config::IST)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
    };

    let conn = match open_conn() {
        Ok(c) => c,
        Err(_) => return info,
    };

    // NIFTY regime: last close vs trailing 20-bar SMA.
    if let Some(closes) = read_nifty_closes(&conn, root) {
        let n = closes.len();
        if n >= 20 {
            let last = closes[n - 1];
            let sma: f64 = closes[n - 20..n].iter().sum::<f64>() / 20.0;
            if sma > 0.0 {
                let dev = (last - sma) / sma;
                info.nifty_regime = if dev.abs() <= 0.001 {
                    "Flat".to_string()
                } else if last > sma {
                    "Up".to_string()
                } else {
                    "Down".to_string()
                };
            }
        }
    }

    // Breadth: first ~207 symbols, last vs prev daily close.
    let take = symbols.len().min(207);
    let mut up = 0usize;
    let mut down = 0usize;
    for sym in symbols.iter().take(take) {
        if let Some((last, prev)) = last_two_daily_closes(&conn, root, sym) {
            if last > prev {
                up += 1;
            } else if last < prev {
                down += 1;
            }
        }
    }
    info.breadth_up = up;
    info.breadth_down = down;
    let total = up + down;
    info.breadth_label = if total == 0 {
        "neutral".to_string()
    } else if (up.max(down) as f64) / (total as f64) > 0.60 {
        "narrow".to_string()
    } else {
        "broad".to_string()
    };

    info
}

/// Standard disclaimer footer shown on the page.
pub const DISCLAIMER: &str = "Research output — not financial advice. Suggestions are derived from \
historical backtests on local data; past performance does not guarantee future results. Entry prices \
are approximate — actual entry requires a live signal after 09:30. Win rate, expectancy and profit \
factor are net of estimated slippage, brokerage and taxes (~0.16% round-trip); circuit breakers and \
corporate events are not modelled. All trading decisions and risk remain with the trader.";

/// Format the `best_overall` line given the owning strategy and its card. Kept
/// as a free fn so the test module can exercise the exact formatting.
fn best_overall_string(strat: SuggestStrategy, card: &SetupCard) -> String {
    format!(
        "{} - {} on {} (R:R 1:{:.1}) - Confidence {}/100",
        strat.name(),
        card.side,
        card.interval,
        card.rr,
        card.confidence.unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_kernel::Candle;

    fn bar(day: u32, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle { day, open: o, high: h, low: l, close: c, volume: 1000.0 }
    }

    #[test]
    fn cross_up_detects_upward_cross_and_guards_nan() {
        let x = [1.0, 2.0, 3.0];
        let y = [2.0, 2.0, 2.0];
        // i=1: 1<=2 and 2>2? no (equal). i=2: 2<=2 and 3>2 -> yes.
        assert!(!cross_up(&x, &y, 1));
        assert!(cross_up(&x, &y, 2));
        // NaN guard.
        let xn = [f64::NAN, 3.0];
        let yn = [2.0, 2.0];
        assert!(!cross_up(&xn, &yn, 1));
        // i==0 never crosses.
        assert!(!cross_up(&x, &y, 0));
    }

    /// P4 reconciliation: the scanner now applies the DSR gate (it used to score
    /// with the neutral `dsr = 1.0`, so a name could read a higher *pre-gate*
    /// Confidence in the scanner than in its own 6-interval deep-dive). A config
    /// that scores well in isolation must be capped to ≤59 once its Sharpe is
    /// unexceptional against a high-dispersion trial set.
    #[test]
    fn scanner_gate_and_pick_applies_dsr_cap() {
        let strat = SuggestStrategy::all().into_iter().next().unwrap();
        // 51 trades, clean positive edge, no long loss streak (W W L pattern).
        let trades: Vec<(usize, f64)> =
            (0..51).map(|i| (i, if i % 3 == 2 { -1.0 } else { 1.0 })).collect();
        let mk = || {
            build_config_stat(
                strat, Timeframe::Min30, Direction::Long, 1.0, 2.0, &trades, 500, 100.0, 2.0,
            )
        };

        // Light trial set (one trial) ⇒ this config looks unique ⇒ high DSR ⇒ ungated.
        let weak = gate_and_pick(vec![mk()], &[mk().sharpe]).expect("weak scores");
        // High-dispersion trial set ⇒ a strong Sharpe was easy to find by luck ⇒
        // DSR < 0.5 ⇒ build_confidence caps the Confidence at 59.
        let strong_trials: Vec<f64> = (0..240).map(|i| -0.5 + (i as f64) / 240.0 * 1.5).collect();
        let strong = gate_and_pick(vec![mk()], &strong_trials).expect("strong still clears ≥50");

        assert!(
            strong.confidence <= 59,
            "DSR gate must cap scanner Confidence ≤59, got {}",
            strong.confidence
        );
        assert!(
            strong.confidence < weak.confidence,
            "gate must lower it: ungated {} vs gated {}",
            weak.confidence,
            strong.confidence
        );
    }

    #[test]
    fn cross_down_detects_downward_cross() {
        let x = [3.0, 2.0, 1.0];
        let y = [2.0, 2.0, 2.0];
        // i=1: 3>=2 and 2<2? no. i=2: 2>=2 and 1<2 -> yes.
        assert!(!cross_down(&x, &y, 1));
        assert!(cross_down(&x, &y, 2));
    }

    #[test]
    fn prior_day_high_low_uses_previous_day() {
        // Day 0: H 110, L 90. Day 1: two bars. Bars on day 1 should see prior H/L.
        let bars = vec![
            bar(0, 100.0, 110.0, 90.0, 105.0),
            bar(0, 105.0, 108.0, 95.0, 100.0),
            bar(1, 100.0, 120.0, 99.0, 115.0),
            bar(1, 115.0, 118.0, 112.0, 116.0),
        ];
        let (ph, pl) = prior_day_high_low(&bars);
        // Day 0 bars have no prior day.
        assert!(ph[0].is_nan() && pl[0].is_nan());
        assert!(ph[1].is_nan() && pl[1].is_nan());
        // Day 1 bars see day 0's high (110) and low (90).
        assert!((ph[2] - 110.0).abs() < 1e-9);
        assert!((pl[2] - 90.0).abs() < 1e-9);
        assert!((ph[3] - 110.0).abs() < 1e-9);
        assert!((pl[3] - 90.0).abs() < 1e-9);
    }

    #[test]
    fn sizing_caps_by_leverage_and_risk() {
        // Check the sizing arithmetic the card uses.
        // entry=100, atr=2, sl_mult=1.0 -> sl_dist=2. capital=100_000, risk 2.5%.
        let entry: f64 = 100.0;
        let atr: f64 = 2.0;
        let sl_mult: f64 = 1.0;
        let sl_dist = sl_mult * atr;
        let capital: f64 = 100_000.0;
        let risk_pct: f64 = 0.025;
        let risk_amount = capital * risk_pct; // 2500
        let qty_by_risk = (risk_amount / sl_dist).floor(); // 1250
        let qty_by_lev = (capital * config::LEVERAGE / entry).floor(); // 5000
        let quantity = qty_by_risk.min(qty_by_lev) as i64;
        assert_eq!(quantity, 1250);

        // Leverage-bound case: tiny stop, huge risk budget.
        let entry2 = 100.0;
        let sl_dist2 = 0.01;
        let qbr = ((capital * 0.05) / sl_dist2).floor(); // enormous
        let qbl = (capital * config::LEVERAGE / entry2).floor(); // 5000
        let q2 = qbr.min(qbl) as i64;
        assert_eq!(q2, 5000);
    }

    #[test]
    fn profit_factor_caps_and_handles_no_losses() {
        assert!((profit_factor(&[2.0, -1.0, 2.0, -1.0]) - 2.0).abs() < 1e-9);
        assert_eq!(profit_factor(&[1.0, 2.0]), 99.0); // no losses
        assert_eq!(profit_factor(&[-1.0, -2.0]), 0.0); // no wins
    }

    #[test]
    fn best_overall_string_formats() {
        let mut card = sample_card();
        card.confidence = Some(86);
        let s = best_overall_string(SuggestStrategy::GapAndGo, &card);
        assert_eq!(s, "Gap-and-Go - SELL on 1 Hour (R:R 1:3.0) - Confidence 86/100");
    }

    fn sample_card() -> SetupCard {
        SetupCard {
            symbol: "TEST".into(),
            side: "SELL".into(),
            interval: "1 Hour".into(),
            rr: 3.0,
            rr_label: "1 : 3.0".into(),
            timeframes_agree: 0,
            entry: 100.0,
            last_close: 100.0,
            sl: 102.0,
            sl_atr_mult: 1.0,
            target: 94.0,
            target_atr_mult: 3.0,
            atr: 2.0,
            quantity: 0,
            risk_pct: 0.025,
            max_risk: 0.0,
            max_reward: 0.0,
            net_profit: 0.0,
            net_loss: 0.0,
            costs: crate::types::CostBreakdown::default(),
            win_rate: 0.0,
            profit_factor: 0.0,
            expectancy_r: 0.0,
            n_trades: 0,
            sharpe: 0.0,
            calmar: 0.0,
            mc_prob_profit: 0.0,
            mc_p95_dd_r: 0.0,
            dsr: 0.0,
            exp_ci_low: 0.0,
            exp_ci_high: 0.0,
            exp_shrunk: 0.0,
            ambiguous_frac: 0.0,
            exp_2x_slip: 0.0,
            exp_3x_slip: 0.0,
            prob_score: 0.0,
            prob_floor: 0.0,
            confidence: None,
            confidence_band: String::new(),
            t_stat: 0.0,
            p_value: 1.0,
            provisional: false,
            conviction: 0,
            conviction_label: String::new(),
            conviction_deltas: Vec::new(),
            selection_artifact: None,
            shortlist: false,
        }
    }

    /// REGRESSION ANCHOR — re-baselined 2026-06-28 against the current archive
    /// (2,776 trading days, last bar 2026-06-25). The legacy
    /// `VWAP·SELL·15m·n=51·+0.494R·conf=72` anchor is RETIRED: the 63MOONS
    /// history has grown ~50× since it was first set, so those numbers no longer
    /// exist on disk. This freezes the *current* canonical deep-dive so any
    /// future code change that silently moves the numbers fails loudly.
    ///
    /// The deep-dive needs the ~14 GB archive, so the test SKIPS cleanly when it
    /// is absent (CI / a fresh clone) — it is a live guard on the owner's box.
    /// NOTE: a data refresh that adds 63MOONS bars will move `n_trades` and is
    /// expected to require a conscious re-baseline (update the constants here).
    #[test]
    fn anchor_63moons_deep_dive_stable() {
        let root = config::data_root();
        let minute = config::parquet_path(&root, "63MOONS", Timeframe::Minute);
        if !minute.exists() {
            eprintln!(
                "SKIP anchor_63moons_deep_dive_stable: archive absent ({})",
                minute.display()
            );
            return;
        }
        // Capital/risk match the CLI `suggest` defaults; they affect sizing only,
        // never the backtest statistics asserted below.
        let s = analyze_symbol(&root, "63MOONS", 100_000.0, 0.025).expect("analyze 63MOONS");

        // Headline pick.
        let best = s.best_overall.as_deref().unwrap_or("");
        assert!(best.contains("Prev-Day Breakout"), "best_overall drifted: {best}");
        assert!(best.contains("SELL on 30 Minutes"), "best_overall drifted: {best}");
        assert!(best.contains("Confidence 59"), "best_overall drifted: {best}");

        // Canonical anchor block: VWAP Trend → SELL · 30 Minutes · R:R 1 : 2.0.
        let vwap = s
            .blocks
            .iter()
            .find(|b| b.key == crate::types::SuggestStrategy::VwapTrend.key())
            .expect("VWAP block present");
        let c = vwap.best.as_ref().expect("VWAP best setup present");

        let approx = |got: f64, want: f64, tol: f64, what: &str| {
            assert!(
                (got - want).abs() <= tol,
                "anchor {what} drifted: got {got}, expected {want} ± {tol}"
            );
        };

        assert_eq!(c.side, "SELL", "anchor side drifted");
        assert_eq!(c.interval, "30 Minutes", "anchor interval drifted");
        approx(c.rr, 2.0, 1e-9, "rr");
        assert_eq!(c.n_trades, 2603, "anchor n_trades drifted (data refresh? re-baseline)");
        assert_eq!(c.confidence, Some(59), "anchor confidence drifted");
        approx(c.win_rate, 51.4, 0.1, "win_rate");
        approx(c.profit_factor, 1.18, 0.01, "profit_factor");
        approx(c.expectancy_r, 0.07, 0.01, "expectancy_r");
        approx(c.sharpe, 0.07, 0.01, "sharpe");
        approx(c.t_stat, 3.46, 0.01, "t_stat");
    }
}

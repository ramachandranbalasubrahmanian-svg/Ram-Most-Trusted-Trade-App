//! Statistics for the Intraday Suggestion page — ported faithfully from the
//! Python project's `core/scoring.py` + `core/backtest.py` so the numbers match
//! the user's mental model.
//!
//! Confidence = 50 + clamp(t·12, 0, 45) − behavioural penalties (banded).
//! Conviction = clipped linear sum of 6 structural factors.
//! Everything is deterministic (seeded PRNG for Monte-Carlo / bootstrap).
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use crate::types::ConvictionDelta;

// --- basic per-trade stats -------------------------------------------------

/// Sample mean of the R-multiples.
pub fn mean(rs: &[f64]) -> f64 {
    if rs.is_empty() { 0.0 } else { rs.iter().sum::<f64>() / rs.len() as f64 }
}

/// Sample standard deviation (ddof=1, matching pandas `.std()`).
pub fn std_dev(_rs: &[f64]) -> f64 {
    0.0
}

/// Edge t-statistic: mean / (std / sqrt(n)). 0 when undefined.
pub fn t_stat(_rs: &[f64]) -> f64 {
    0.0
}

/// Per-trade Sharpe: mean / std. 0 when std==0.
pub fn sharpe_per_trade(_rs: &[f64]) -> f64 {
    0.0
}

/// Max drawdown (in R) of the cumulative-R equity curve.
pub fn max_drawdown_r(_rs: &[f64]) -> f64 {
    0.0
}

/// Calmar: total R / max drawdown R (0 when max dd <= 0).
pub fn calmar(_rs: &[f64]) -> f64 {
    0.0
}

/// Longest run of consecutive losing trades (R <= 0).
pub fn max_loss_streak(_rs: &[f64]) -> usize {
    0
}

/// Win rate (%) over the most recent `k` trades.
pub fn recent_win_rate(_rs: &[f64], _k: usize) -> f64 {
    0.0
}

/// Sample skewness and (non-excess) kurtosis of the R distribution.
pub fn skew_kurt(_rs: &[f64]) -> (f64, f64) {
    (0.0, 0.0)
}

// --- proportion / significance helpers -------------------------------------

/// Wilson score lower bound for a proportion `p` over `n` trials (z=1.96).
pub fn wilson_lower(_p: f64, _n: usize) -> f64 {
    0.0
}

/// One-sided p-value from a t-statistic: 0.5·erfc(t/√2).
pub fn t_to_p_onesided(_t: f64) -> f64 {
    1.0
}

// --- Monte-Carlo + bootstrap (seeded, deterministic) -----------------------

/// Monte-Carlo bootstrap of the trade sequence.
#[derive(Debug, Clone, Copy)]
pub struct MonteCarlo {
    pub prob_profit: f64, // % of resampled paths ending > 0
    pub p95_maxdd_r: f64, // 95th-pct max drawdown (R)
    pub p05_final_r: f64, // 5th-pct final equity (R)
}

/// Resample R outcomes with replacement `n_sims` times; None if too few trades.
pub fn monte_carlo(_rs: &[f64], _n_sims: usize, _seed: u64) -> Option<MonteCarlo> {
    None
}

/// Bootstrap CI on expectancy (mean R).
#[derive(Debug, Clone, Copy)]
pub struct ExpectancyCi {
    pub p05: f64,
    pub p50: f64,
    pub p95: f64,
}

/// 90% bootstrap CI of the mean R; None when n < 20.
pub fn expectancy_ci(_rs: &[f64], _n_sims: usize, _seed: u64) -> Option<ExpectancyCi> {
    None
}

/// James–Stein shrinkage of expectancy toward `prior_r`:
/// (n·exp + strength·prior_r) / (n + strength).
pub fn shrunk_expectancy(exp: f64, _n: usize, _prior_r: f64, _strength: f64) -> f64 {
    exp
}

/// Deflated Sharpe Ratio (Bailey/López de Prado): probability in [0,1] that the
/// selected combo's Sharpe is real, given how many trials were searched.
pub fn deflated_sharpe(_sharpe_obs: f64, _n_trades: usize, _trial_sharpes: &[f64]) -> f64 {
    0.0
}

// --- Confidence (statistical) ----------------------------------------------

/// Inputs the confidence penalties read (assembled by the engine).
#[derive(Debug, Clone)]
pub struct ConfInput {
    pub n_trades: usize,
    pub win_rate_pct: f64,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub max_drawdown_r: f64,
    pub total_r: f64,
    pub recent_20_wr_pct: f64,
    pub oos_win_rate_pct: Option<f64>,
    pub oos_expectancy_r: Option<f64>,
    pub max_loss_streak: usize,
    pub t_stat: f64,
}

/// Confidence result mirroring the Python `build_confidence` contract.
#[derive(Debug, Clone)]
pub struct ConfidenceResult {
    pub score: Option<u32>,
    pub band: String,
    pub t_stat: f64,
    pub p_value: f64,
    pub wilson_low: f64,
    pub provisional: bool,
    /// (label, points, explanation)
    pub penalties: Vec<(String, u32, String)>,
}

/// Compute the statistical Confidence (0-100) from a setup's metrics.
pub fn build_confidence(inp: &ConfInput) -> ConfidenceResult {
    ConfidenceResult {
        score: None,
        band: "Insufficient data".to_string(),
        t_stat: inp.t_stat,
        p_value: t_to_p_onesided(inp.t_stat),
        wilson_low: 0.0,
        provisional: false,
        penalties: Vec::new(),
    }
}

// --- Conviction (structural, display-only) ---------------------------------

/// Inputs for the structural conviction score.
#[derive(Debug, Clone, Default)]
pub struct ConvInput {
    pub mtf_agreement: f64,
    pub mc_prob_profit: f64, // percentage 0-100
    pub wf_consistency: f64, // 0-1
    pub dsr: f64,            // raw deflated sharpe 0-1
    pub oos_exp_r: Option<f64>,
    pub oos_n: Option<usize>,
    pub skew: Option<f64>,
    pub kurt: Option<f64>,
}

/// Returns (score 0-100, label, non-zero deltas). Structural only (no live).
pub fn compute_conviction(_inp: &ConvInput) -> (u32, String, Vec<ConvictionDelta>) {
    (0, "Stand down (structural only)".to_string(), Vec::new())
}

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

// Reference scoring constants (must match the Python `core/scoring.py`).
const MIN_SAMPLE: usize = 30;
const PROVISIONAL_MIN: usize = 20;
const PROVISIONAL_CAP: f64 = 55.0;
const HARD_CAP_NORMAL: f64 = 90.0;

// --- basic per-trade stats -------------------------------------------------

/// Sample mean of the R-multiples.
pub fn mean(rs: &[f64]) -> f64 {
    if rs.is_empty() { 0.0 } else { rs.iter().sum::<f64>() / rs.len() as f64 }
}

/// Sample standard deviation (ddof=1, matching pandas `.std()`).
pub fn std_dev(rs: &[f64]) -> f64 {
    let n = rs.len();
    if n < 2 {
        return 0.0;
    }
    let m = mean(rs);
    let var = rs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / (n as f64 - 1.0);
    var.sqrt()
}

/// Edge t-statistic: mean / (std / sqrt(n)). 0 when undefined.
pub fn t_stat(rs: &[f64]) -> f64 {
    let n = rs.len();
    if n < 2 {
        return 0.0;
    }
    let s = std_dev(rs);
    if s <= 0.0 {
        return 0.0;
    }
    mean(rs) / (s / (n as f64).sqrt())
}

/// Per-trade Sharpe: mean / std. 0 when std==0.
pub fn sharpe_per_trade(rs: &[f64]) -> f64 {
    let s = std_dev(rs);
    if s == 0.0 {
        return 0.0;
    }
    mean(rs) / s
}

/// Max drawdown (in R) of the cumulative-R equity curve.
pub fn max_drawdown_r(rs: &[f64]) -> f64 {
    if rs.is_empty() {
        return 0.0;
    }
    let mut cum = 0.0;
    let mut peak = 0.0;
    let mut max_dd = 0.0;
    for &r in rs {
        cum += r;
        if cum > peak {
            peak = cum;
        }
        let dd = peak - cum;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    max_dd
}

/// Calmar: total R / max drawdown R (0 when max dd <= 0).
pub fn calmar(rs: &[f64]) -> f64 {
    let dd = max_drawdown_r(rs);
    if dd <= 0.0 {
        return 0.0;
    }
    let total: f64 = rs.iter().sum();
    total / dd
}

/// Longest run of consecutive losing trades (R <= 0).
pub fn max_loss_streak(rs: &[f64]) -> usize {
    let mut max_run = 0usize;
    let mut run = 0usize;
    for &r in rs {
        if r <= 0.0 {
            run += 1;
            if run > max_run {
                max_run = run;
            }
        } else {
            run = 0;
        }
    }
    max_run
}

/// Win rate (%) over the most recent `k` trades.
pub fn recent_win_rate(rs: &[f64], k: usize) -> f64 {
    let n = rs.len();
    if n == 0 || k == 0 {
        return 0.0;
    }
    let take = k.min(n);
    let slice = &rs[n - take..];
    let wins = slice.iter().filter(|&&r| r > 0.0).count();
    wins as f64 / take as f64 * 100.0
}

/// Sample skewness and (non-excess) kurtosis of the R distribution.
pub fn skew_kurt(rs: &[f64]) -> (f64, f64) {
    let n = rs.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let m = mean(rs);
    let nf = n as f64;
    let m2 = rs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / nf;
    let m3 = rs.iter().map(|x| (x - m).powi(3)).sum::<f64>() / nf;
    let m4 = rs.iter().map(|x| (x - m).powi(4)).sum::<f64>() / nf;
    if m2 <= 0.0 {
        return (0.0, 3.0);
    }
    let skew = m3 / m2.powf(1.5);
    let kurt = m4 / (m2 * m2);
    (skew, kurt)
}

// --- proportion / significance helpers -------------------------------------

/// Wilson score lower bound for a proportion `p` over `n` trials (z=1.96).
pub fn wilson_lower(p: f64, n: usize) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let z = 1.96_f64;
    let nf = n as f64;
    let centre = p + z * z / (2.0 * nf);
    let margin = z * (p * (1.0 - p) / nf + z * z / (4.0 * nf * nf)).sqrt();
    let denom = 1.0 + z * z / nf;
    let lower = (centre - margin) / denom;
    if lower < 0.0 { 0.0 } else { lower }
}

/// One-sided p-value from a t-statistic: 0.5·erfc(t/√2).
pub fn t_to_p_onesided(t: f64) -> f64 {
    0.5 * erfc(t / std::f64::consts::SQRT_2)
}

// --- numerical special functions (private) ---------------------------------

/// Complementary error function via Abramowitz–Stegun 7.1.26 (max abs err ~1.5e-7).
fn erfc(x: f64) -> f64 {
    1.0 - erf(x)
}

/// Error function via Abramowitz–Stegun 7.1.26.
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let ax = x.abs();
    let t = 1.0 / (1.0 + 0.3275911 * ax);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
            + 0.254829592)
            * t
            * (-ax * ax).exp();
    sign * y
}

/// Standard normal CDF: Phi(x) = 0.5·erfc(-x/√2).
fn phi(x: f64) -> f64 {
    0.5 * erfc(-x / std::f64::consts::SQRT_2)
}

/// Inverse standard normal CDF via the Acklam / Beasley–Springer rational
/// approximation (relative error < ~1.15e-9 across the interior).
fn phi_inv(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }

    // Coefficients.
    const A: [f64; 6] = [
        -3.969683028665376e+01,
        2.209460984245205e+02,
        -2.759285104469687e+02,
        1.383577518672690e+02,
        -3.066479806614716e+01,
        2.506628277459239e+00,
    ];
    const B: [f64; 5] = [
        -5.447609879822406e+01,
        1.615858368580409e+02,
        -1.556989798598866e+02,
        6.680131188771972e+01,
        -1.328068155288572e+01,
    ];
    const C: [f64; 6] = [
        -7.784894002430293e-03,
        -3.223964580411365e-01,
        -2.400758277161838e+00,
        -2.549732539343734e+00,
        4.374664141464968e+00,
        2.938163982698783e+00,
    ];
    const D: [f64; 4] = [
        7.784695709041462e-03,
        3.224671290700398e-01,
        2.445134137142996e+00,
        3.754408661907416e+00,
    ];

    let p_low = 0.02425;
    let p_high = 1.0 - p_low;

    let x = if p < p_low {
        // Lower region.
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= p_high {
        // Central region.
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        // Upper region.
        let q = (-2.0 * (1.0 - p).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    };

    // One Halley refinement step for full double precision.
    let e = phi(x) - p;
    let u = e * (2.0 * std::f64::consts::PI).sqrt() * (x * x / 2.0).exp();
    x - u / (1.0 + x * u / 2.0)
}

// --- deterministic PRNG (private) ------------------------------------------

/// SplitMix64 — a tiny, fast, reproducible 64-bit generator. Seeding the same
/// value yields the same sequence, so Monte-Carlo / bootstrap are deterministic.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform float in [0, 1).
    fn next_f64(&mut self) -> f64 {
        // Use the top 53 bits for a uniform double in [0,1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform index in [0, n).
    fn next_index(&mut self, n: usize) -> usize {
        ((self.next_f64() * n as f64) as usize).min(n - 1)
    }
}

/// Percentile of a slice via linear interpolation between closest ranks
/// (NumPy default). `pct` in [0, 100]. `values` need not be pre-sorted.
fn percentile(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 1 {
        return v[0];
    }
    let rank = (pct / 100.0) * (n as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return v[lo];
    }
    let frac = rank - lo as f64;
    v[lo] + (v[hi] - v[lo]) * frac
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
pub fn monte_carlo(rs: &[f64], n_sims: usize, seed: u64) -> Option<MonteCarlo> {
    let n = rs.len();
    if n == 0 {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let mut finals: Vec<f64> = Vec::with_capacity(n_sims);
    let mut maxdds: Vec<f64> = Vec::with_capacity(n_sims);
    let mut positive = 0usize;

    for _ in 0..n_sims {
        let mut cum = 0.0;
        let mut peak = 0.0;
        let mut max_dd = 0.0;
        for _ in 0..n {
            let idx = rng.next_index(n);
            cum += rs[idx];
            if cum > peak {
                peak = cum;
            }
            let dd = peak - cum;
            if dd > max_dd {
                max_dd = dd;
            }
        }
        if cum > 0.0 {
            positive += 1;
        }
        finals.push(cum);
        maxdds.push(max_dd);
    }

    let prob_profit = if n_sims == 0 {
        0.0
    } else {
        positive as f64 / n_sims as f64 * 100.0
    };

    Some(MonteCarlo {
        prob_profit,
        p95_maxdd_r: percentile(&maxdds, 95.0),
        p05_final_r: percentile(&finals, 5.0),
    })
}

/// Bootstrap CI on expectancy (mean R).
#[derive(Debug, Clone, Copy)]
pub struct ExpectancyCi {
    pub p05: f64,
    pub p50: f64,
    pub p95: f64,
}

/// 90% bootstrap CI of the mean R; None when n < 20.
pub fn expectancy_ci(rs: &[f64], n_sims: usize, seed: u64) -> Option<ExpectancyCi> {
    let n = rs.len();
    if n < PROVISIONAL_MIN {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let mut means: Vec<f64> = Vec::with_capacity(n_sims);
    for _ in 0..n_sims {
        let mut sum = 0.0;
        for _ in 0..n {
            let idx = rng.next_index(n);
            sum += rs[idx];
        }
        means.push(sum / n as f64);
    }
    Some(ExpectancyCi {
        p05: percentile(&means, 5.0),
        p50: percentile(&means, 50.0),
        p95: percentile(&means, 95.0),
    })
}

/// James–Stein shrinkage of expectancy toward `prior_r`:
/// (n·exp + strength·prior_r) / (n + strength).
pub fn shrunk_expectancy(exp: f64, n: usize, prior_r: f64, strength: f64) -> f64 {
    let nf = n as f64;
    let denom = nf + strength;
    if denom == 0.0 {
        return exp;
    }
    (nf * exp + strength * prior_r) / denom
}

/// Deflated Sharpe Ratio (Bailey/López de Prado): probability in [0,1] that the
/// selected combo's Sharpe is real, given how many trials were searched.
pub fn deflated_sharpe(sharpe_obs: f64, n_trades: usize, trial_sharpes: &[f64]) -> f64 {
    let big_n = trial_sharpes.len().max(1);
    let nt = (n_trades as f64 - 1.0).max(1.0);

    if big_n == 1 {
        return phi(sharpe_obs * nt.sqrt()).clamp(0.0, 1.0);
    }

    // Dispersion of the trial Sharpe ratios.
    let mut sigma_sr = std_dev(trial_sharpes);
    if sigma_sr <= 0.0 {
        sigma_sr = 1e-9;
    }

    let gamma = 0.5772156649_f64; // Euler–Mascheroni
    let e = std::f64::consts::E;
    let nf = big_n as f64;

    let expected_max =
        sigma_sr * ((1.0 - gamma) * phi_inv(1.0 - 1.0 / nf) + gamma * phi_inv(1.0 - 1.0 / (nf * e)));

    // Skew / kurtosis of the trial Sharpe distribution (fallbacks: 0, 3).
    let (mut skew_sr, mut kurt_sr) = skew_kurt(trial_sharpes);
    if !skew_sr.is_finite() {
        skew_sr = 0.0;
    }
    if !kurt_sr.is_finite() {
        kurt_sr = 3.0;
    }

    let denom_inner =
        1.0 - skew_sr * sharpe_obs + ((kurt_sr - 1.0) / 4.0) * sharpe_obs * sharpe_obs;
    let denom = denom_inner.max(1e-9).sqrt();

    let dsr = phi((sharpe_obs - expected_max) * nt.sqrt() / denom);
    dsr.clamp(0.0, 1.0)
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
    let n = inp.n_trades;
    let p_value = t_to_p_onesided(inp.t_stat);

    // Early return: insufficient data.
    if n < PROVISIONAL_MIN {
        return ConfidenceResult {
            score: None,
            band: "Insufficient data".to_string(),
            t_stat: inp.t_stat,
            p_value,
            wilson_low: 0.0,
            provisional: false,
            penalties: Vec::new(),
        };
    }

    // Early return: no edge.
    if inp.expectancy_r <= 0.0 {
        return ConfidenceResult {
            score: None,
            band: "No edge".to_string(),
            t_stat: inp.t_stat,
            p_value,
            wilson_low: 0.0,
            provisional: false,
            penalties: Vec::new(),
        };
    }

    let provisional = n < MIN_SAMPLE;
    let wilson_low = wilson_lower(inp.win_rate_pct / 100.0, n) * 100.0;

    let base = 50.0 + (inp.t_stat * 12.0).clamp(0.0, 45.0);

    let mut penalties: Vec<(String, u32, String)> = Vec::new();

    // --- drawdown penalty ---
    if inp.total_r > 0.0 {
        let dd_ratio = inp.max_drawdown_r / inp.total_r.max(0.01);
        if dd_ratio > 2.0 {
            penalties.push((
                "high_drawdown".to_string(),
                12,
                "Drawdown exceeds 2× total return — equity curve is very choppy.".to_string(),
            ));
        } else if dd_ratio > 1.0 {
            penalties.push((
                "moderate_drawdown".to_string(),
                6,
                "Drawdown exceeds total return — meaningful equity dips.".to_string(),
            ));
        }
    } else {
        penalties.push((
            "negative_expectancy".to_string(),
            20,
            "Total return is non-positive — no realized edge.".to_string(),
        ));
    }

    // --- recent vs overall performance ---
    if n >= 20 && inp.win_rate_pct > 0.0 {
        let ratio = inp.recent_20_wr_pct / inp.win_rate_pct;
        if ratio < 0.70 {
            penalties.push((
                "recent_underperformance".to_string(),
                12,
                "Recent 20-trade win rate is far below the overall — edge may be fading."
                    .to_string(),
            ));
        } else if ratio < 0.85 {
            penalties.push((
                "slight_recent_underperformance".to_string(),
                6,
                "Recent win rate is moderately below the overall.".to_string(),
            ));
        }
    }

    // --- out-of-sample degradation ---
    if let (Some(oos_wr), Some(oos_exp)) = (inp.oos_win_rate_pct, inp.oos_expectancy_r) {
        if oos_wr < inp.win_rate_pct * 0.75 {
            penalties.push((
                "oos_degradation".to_string(),
                12,
                "Out-of-sample win rate is much weaker than in-sample — possible overfit."
                    .to_string(),
            ));
        } else if oos_wr < inp.win_rate_pct * 0.90 {
            penalties.push((
                "mild_oos_degradation".to_string(),
                5,
                "Out-of-sample win rate is mildly weaker than in-sample.".to_string(),
            ));
        }
        if oos_exp < 0.0 {
            penalties.push((
                "negative_oos_expectancy".to_string(),
                15,
                "Out-of-sample expectancy is negative — edge does not hold out-of-sample."
                    .to_string(),
            ));
        }
    }

    // --- profit factor ---
    if inp.profit_factor.is_finite() {
        if inp.profit_factor < 1.1 {
            penalties.push((
                "low_profit_factor".to_string(),
                10,
                "Profit factor below 1.1 — gross wins barely exceed gross losses.".to_string(),
            ));
        } else if inp.profit_factor < 1.3 {
            penalties.push((
                "marginal_profit_factor".to_string(),
                4,
                "Profit factor is only marginal (<1.3).".to_string(),
            ));
        }
    }

    // --- loss streak ---
    if inp.max_loss_streak >= 8 {
        penalties.push((
            "long_losing_streak".to_string(),
            8,
            "A run of 8+ consecutive losers — painful to sit through.".to_string(),
        ));
    } else if inp.max_loss_streak >= 5 {
        penalties.push((
            "moderate_losing_streak".to_string(),
            3,
            "A run of 5+ consecutive losers observed.".to_string(),
        ));
    }

    let sum_penalties: f64 = penalties.iter().map(|(_, pts, _)| *pts as f64).sum();
    let mut adjusted = (base - sum_penalties).clamp(0.0, 95.0);

    // --- hard cap (unless an exceptional, well-validated sample) ---
    let oos_ok = match inp.oos_win_rate_pct {
        Some(oos_wr) => oos_wr >= inp.win_rate_pct * 0.90,
        None => false,
    };
    let pf_ok = inp.profit_factor.is_finite() && inp.profit_factor >= 2.0;
    if adjusted > HARD_CAP_NORMAL && !(n >= 300 && oos_ok && pf_ok) {
        adjusted = HARD_CAP_NORMAL - 1.0;
    }

    // --- provisional cap ---
    if provisional {
        adjusted = adjusted.min(PROVISIONAL_CAP);
    }

    let score = adjusted as u32; // truncates like Python int()

    let band = if provisional {
        "Provisional (small sample, 20-29 trades)".to_string()
    } else if adjusted < 50.0 {
        "Weak - NO TRADE".to_string()
    } else if adjusted < 60.0 {
        "Low confidence".to_string()
    } else if adjusted < 70.0 {
        "Moderate confidence".to_string()
    } else if adjusted < 80.0 {
        "Good confidence".to_string()
    } else if adjusted < 90.0 {
        "Strong confidence".to_string()
    } else {
        "Very strong confidence".to_string()
    };

    ConfidenceResult {
        score: Some(score),
        band,
        t_stat: inp.t_stat,
        p_value,
        wilson_low,
        provisional,
        penalties,
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
pub fn compute_conviction(inp: &ConvInput) -> (u32, String, Vec<ConvictionDelta>) {
    let mut deltas: Vec<ConvictionDelta> = Vec::new();
    let mut total = 0.0_f64;

    let mut push = |name: &str, points: f64, deltas: &mut Vec<ConvictionDelta>, total: &mut f64| {
        if points != 0.0 {
            deltas.push(ConvictionDelta {
                name: name.to_string(),
                points,
            });
            *total += points;
        }
    };

    // mtf: min(mtf_agreement, 5) * 3.
    let mtf = inp.mtf_agreement.min(5.0) * 3.0;
    push("mtf", mtf, &mut deltas, &mut total);

    // mc_prob_profit (%) banded.
    let mc = if inp.mc_prob_profit >= 95.0 {
        15.0
    } else if inp.mc_prob_profit >= 90.0 {
        10.0
    } else if inp.mc_prob_profit >= 80.0 {
        5.0
    } else {
        0.0
    };
    push("mc_prob_profit", mc, &mut deltas, &mut total);

    // wf_consistency (0-1): round(wf * 12).
    let wf = (inp.wf_consistency * 12.0).round();
    push("wf_consistency", wf, &mut deltas, &mut total);

    // dsr (raw 0-1) banded.
    let dsr = if inp.dsr >= 0.20 {
        12.0
    } else if inp.dsr >= 0.10 {
        6.0
    } else {
        0.0
    };
    push("dsr", dsr, &mut deltas, &mut total);

    // oos: requires positive oos expectancy AND an oos sample count.
    if let (Some(oexp), Some(on)) = (inp.oos_exp_r, inp.oos_n) {
        if oexp > 0.0 {
            let oos = if on >= 12 {
                8.0
            } else if on >= 6 {
                4.0
            } else {
                0.0
            };
            push("oos", oos, &mut deltas, &mut total);
        }
    }

    // tail: shape of the R distribution.
    if let (Some(skew), Some(kurt)) = (inp.skew, inp.kurt) {
        let tail = if skew >= 0.0 && kurt < 6.0 {
            8.0
        } else if skew >= -0.3 && kurt < 8.0 {
            4.0
        } else {
            0.0
        };
        push("tail", tail, &mut deltas, &mut total);
    }

    let score = (total.round().clamp(0.0, 100.0)) as u32;

    let label = if score >= 70 {
        "Strong conviction (structural only)".to_string()
    } else if score >= 50 {
        "Moderate conviction (structural only)".to_string()
    } else if score >= 30 {
        "Weak conviction (structural only)".to_string()
    } else {
        "Stand down (structural only)".to_string()
    };

    (score, label, deltas)
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_dev_sample_ddof1() {
        // [2,4,4,4,5,5,7,9] has population std 2.0 and sample std ~2.138.
        let rs = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let s = std_dev(&rs);
        assert!((s - 2.13809).abs() < 1e-4, "std_dev was {s}");
        assert_eq!(std_dev(&[1.0]), 0.0);
        assert_eq!(std_dev(&[]), 0.0);
    }

    #[test]
    fn max_drawdown_and_streak() {
        let rs = [1.0, -2.0, -1.0, 3.0, -1.0];
        // cum: 1, -1, -2, 1, 0; peak 1 -> trough -2 => dd 3.
        assert!((max_drawdown_r(&rs) - 3.0).abs() < 1e-9);
        assert_eq!(max_loss_streak(&rs), 2);
    }

    #[test]
    fn wilson_lower_sanity() {
        // Lower bound is below the point estimate and within [0,1].
        let p = 0.6;
        let n = 100;
        let lo = wilson_lower(p, n);
        assert!(lo > 0.0 && lo < p, "wilson lower {lo} not below p");
        // Larger sample => tighter (higher) lower bound for the same p.
        let lo_big = wilson_lower(p, 1000);
        assert!(lo_big > lo, "expected tighter bound for larger n");
        // n == 0 => 0.
        assert_eq!(wilson_lower(0.6, 0), 0.0);
    }

    #[test]
    fn t_to_p_monotonic() {
        // Larger t => smaller one-sided p-value.
        let p_lo = t_to_p_onesided(0.0);
        let p_mid = t_to_p_onesided(1.0);
        let p_hi = t_to_p_onesided(3.0);
        assert!((p_lo - 0.5).abs() < 1e-6, "p(0) should be 0.5, got {p_lo}");
        assert!(p_mid < p_lo, "p should decrease with t");
        assert!(p_hi < p_mid, "p should decrease with t");
        assert!(p_hi > 0.0 && p_hi < 0.01);
    }

    #[test]
    fn phi_inv_round_trip() {
        for &p in &[0.01_f64, 0.1, 0.25, 0.5, 0.75, 0.9, 0.99] {
            let x = phi_inv(p);
            let back = phi(x);
            assert!((back - p).abs() < 1e-6, "phi(phi_inv({p}))={back}");
        }
        assert!((phi_inv(0.5)).abs() < 1e-6);
    }

    #[test]
    fn monte_carlo_determinism() {
        let rs = [0.8, -1.0, 1.2, -1.0, 0.5, 2.0, -1.0, 0.9, -1.0, 1.5];
        let a = monte_carlo(&rs, 2000, 42).unwrap();
        let b = monte_carlo(&rs, 2000, 42).unwrap();
        assert_eq!(a.prob_profit, b.prob_profit);
        assert_eq!(a.p95_maxdd_r, b.p95_maxdd_r);
        assert_eq!(a.p05_final_r, b.p05_final_r);
        // Different seed should (almost surely) differ somewhere.
        let c = monte_carlo(&rs, 2000, 7).unwrap();
        assert!(a.prob_profit != c.prob_profit || a.p05_final_r != c.p05_final_r);
        // Empty => None.
        assert!(monte_carlo(&[], 100, 1).is_none());
    }

    #[test]
    fn expectancy_ci_threshold_and_order() {
        let short: Vec<f64> = vec![1.0; 19];
        assert!(expectancy_ci(&short, 1000, 1).is_none());
        let rs: Vec<f64> = (0..40)
            .map(|i| if i % 2 == 0 { 1.0 } else { -0.5 })
            .collect();
        let ci = expectancy_ci(&rs, 3000, 42).unwrap();
        assert!(ci.p05 <= ci.p50 && ci.p50 <= ci.p95);
    }

    #[test]
    fn shrunk_expectancy_pulls_toward_prior() {
        // With small n, the prior (0) dominates a high raw expectancy.
        let s = shrunk_expectancy(1.0, 5, 0.0, 20.0);
        assert!(s < 0.3, "expected heavy shrinkage, got {s}");
        // With large n, it stays near the raw value.
        let s2 = shrunk_expectancy(1.0, 1000, 0.0, 20.0);
        assert!(s2 > 0.95, "expected little shrinkage, got {s2}");
    }

    #[test]
    fn deflated_sharpe_in_range() {
        let trials = [0.1, 0.2, 0.15, 0.05, 0.25, 0.18, 0.12, 0.3];
        let d = deflated_sharpe(0.4, 300, &trials);
        assert!((0.0..=1.0).contains(&d));
        // Single-trial branch.
        let d1 = deflated_sharpe(0.5, 100, &[0.5]);
        assert!((0.0..=1.0).contains(&d1));
        assert!(d1 > 0.99, "high positive sharpe should give high DSR, got {d1}");
    }

    #[test]
    fn confidence_strong_when_edge_real() {
        // Construct a set: t≈3 via win-loss mix, pf=2, n=300, oos matches.
        let inp = ConfInput {
            n_trades: 300,
            win_rate_pct: 60.0,
            expectancy_r: 0.3,
            profit_factor: 2.0,
            max_drawdown_r: 5.0,
            total_r: 90.0,
            recent_20_wr_pct: 60.0,
            oos_win_rate_pct: Some(58.0),
            oos_expectancy_r: Some(0.28),
            max_loss_streak: 3,
            t_stat: 3.0,
        };
        let r = build_confidence(&inp);
        let score = r.score.expect("score present");
        // base = 50 + clamp(36,0,45) = 86; no penalties => 86 => "Strong confidence".
        assert!(score >= 80 && score < 90, "score was {score}");
        assert_eq!(r.band, "Strong confidence");
        assert!(!r.provisional);
        assert!(r.p_value < 0.01);
    }

    #[test]
    fn confidence_none_when_no_edge() {
        let inp = ConfInput {
            n_trades: 100,
            win_rate_pct: 40.0,
            expectancy_r: -0.1, // <= 0
            profit_factor: 0.8,
            max_drawdown_r: 10.0,
            total_r: -5.0,
            recent_20_wr_pct: 40.0,
            oos_win_rate_pct: None,
            oos_expectancy_r: None,
            max_loss_streak: 6,
            t_stat: -1.0,
        };
        let r = build_confidence(&inp);
        assert!(r.score.is_none());
        assert_eq!(r.band, "No edge");

        // Insufficient data path.
        let small = ConfInput {
            n_trades: 10,
            expectancy_r: 0.5,
            ..inp.clone()
        };
        let rs = build_confidence(&small);
        assert!(rs.score.is_none());
        assert_eq!(rs.band, "Insufficient data");
    }

    #[test]
    fn confidence_provisional_capped() {
        let inp = ConfInput {
            n_trades: 25, // 20-29 => provisional
            win_rate_pct: 70.0,
            expectancy_r: 0.5,
            profit_factor: 3.0,
            max_drawdown_r: 1.0,
            total_r: 12.0,
            recent_20_wr_pct: 70.0,
            oos_win_rate_pct: None,
            oos_expectancy_r: None,
            max_loss_streak: 1,
            t_stat: 4.0,
        };
        let r = build_confidence(&inp);
        let score = r.score.unwrap();
        assert!(score <= PROVISIONAL_CAP as u32, "score {score} exceeds provisional cap");
        assert_eq!(r.band, "Provisional (small sample, 20-29 trades)");
        assert!(r.provisional);
    }

    #[test]
    fn conviction_sum_example() {
        let inp = ConvInput {
            mtf_agreement: 5.0,      // min(5,5)*3 = 15
            mc_prob_profit: 96.0,    // 15
            wf_consistency: 1.0,     // round(12) = 12
            dsr: 0.25,               // 12
            oos_exp_r: Some(0.3),    // oos_n>=12 => 8
            oos_n: Some(20),
            skew: Some(0.1),         // skew>=0 && kurt<6 => 8
            kurt: Some(4.0),
        };
        let (score, label, deltas) = compute_conviction(&inp);
        // 15+15+12+12+8+8 = 70.
        assert_eq!(score, 70);
        assert_eq!(label, "Strong conviction (structural only)");
        assert_eq!(deltas.len(), 6);
        assert!(deltas.iter().all(|d| d.points != 0.0));

        // Empty / weak input -> stand down, no deltas.
        let (s0, l0, d0) = compute_conviction(&ConvInput::default());
        assert_eq!(s0, 0);
        assert_eq!(l0, "Stand down (structural only)");
        assert!(d0.is_empty());
    }
}

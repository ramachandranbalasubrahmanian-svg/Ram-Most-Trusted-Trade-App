//! Combinatorially Symmetric Cross-Validation → Probability of Backtest
//! Overfitting (Bailey, Borwein, López de Prado & Zhu, 2014).
//!
//! FIREWALLED & PURE: no imports beyond `std`/serde. This is a DISPLAY-ONLY
//! robustness statistic — it NEVER enters `eligible()`, Confidence, ranking, or
//! sizing. It answers one question on the deep-dive: "across all the configs we
//! tried for this name, how likely is it that the best in-sample one is no better
//! than median out-of-sample?" — i.e. how much of the headline edge is plausibly
//! data-snooping.
//!
//! Method (CSCV): split the common timeline into S equal blocks; for every way to
//! choose S/2 blocks as in-sample (the rest out-of-sample), pick the config that
//! looked best IN-sample, then read its OUT-of-sample rank. PBO = the fraction of
//! splits where that IS-best config lands below the OOS median (logit ≤ 0). Ties
//! are handled with midranks so a flat field can't inflate or deflate the rank.

use serde::{Deserialize, Serialize};

/// Minimum configs for a meaningful ranking distribution.
const MIN_CONFIGS: usize = 8;
/// Keep the logit finite at the extremes.
const EPS: f64 = 1e-6;

/// A PBO estimate over a configs×blocks performance matrix. Display-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PboResult {
    /// Probability of Backtest Overfitting in [0, 1] — higher = more snooping.
    pub pbo: f64,
    pub n_configs: usize,
    pub n_blocks: usize,
    /// Number of combinatorial IS/OOS splits evaluated (C(S, S/2)).
    pub n_splits: usize,
    /// Median logit of the IS-best config's OOS rank (≤0 ⇒ typically overfit).
    pub median_logit: f64,
}

/// Per-block mean return for one config's trades on a `[0, total_bars)` grid.
/// Block `b` covers bars `[b·total/S, (b+1)·total/S)`. An empty block → 0.0
/// (no trades that period contributes no edge). Pure.
pub fn block_means(trades: &[(usize, f64)], total_bars: usize, n_blocks: usize) -> Vec<f64> {
    let nb = n_blocks.max(1);
    let mut sum = vec![0.0f64; nb];
    let mut cnt = vec![0usize; nb];
    if total_bars == 0 {
        return sum;
    }
    for &(idx, r) in trades {
        if !r.is_finite() {
            continue;
        }
        let b = (idx.min(total_bars - 1) * nb / total_bars).min(nb - 1);
        sum[b] += r;
        cnt[b] += 1;
    }
    (0..nb)
        .map(|b| if cnt[b] > 0 { sum[b] / cnt[b] as f64 } else { 0.0 })
        .collect()
}

/// Mean of selected columns of a row vector.
fn mean_over(vals: &[f64], idxs: &[usize]) -> f64 {
    if idxs.is_empty() {
        return 0.0;
    }
    idxs.iter().map(|&i| vals[i]).sum::<f64>() / idxs.len() as f64
}

/// All combinations of `k` of `0..n` (k small; n ≤ ~12). Pure, allocation-simple.
fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut idx: Vec<usize> = (0..k).collect();
    if k == 0 || k > n {
        return out;
    }
    loop {
        out.push(idx.clone());
        // advance like an odometer (lexicographic combinations)
        let mut i = k;
        while i > 0 {
            i -= 1;
            if idx[i] != i + n - k {
                idx[i] += 1;
                for j in (i + 1)..k {
                    idx[j] = idx[j - 1] + 1;
                }
                break;
            }
            if i == 0 {
                return out;
            }
        }
    }
}

/// Midrank (1..=N) of `value` within `all`, averaging ties. 1 = worst, N = best.
fn midrank(all: &[f64], value: f64) -> f64 {
    let below = all.iter().filter(|&&v| v < value).count();
    let equal = all.iter().filter(|&&v| v == value).count().max(1);
    below as f64 + (equal as f64 + 1.0) / 2.0
}

/// CSCV PBO over `m[config][block]` (per-block performance). Returns `None` when
/// there are too few configs, an odd/too-small block count, or a non-rectangular
/// matrix. Pure.
pub fn cscv_pbo(m: &[Vec<f64>], n_blocks: usize) -> Option<PboResult> {
    let n = m.len();
    if n < MIN_CONFIGS || n_blocks < 4 || n_blocks % 2 != 0 {
        return None;
    }
    if m.iter().any(|row| row.len() != n_blocks) {
        return None;
    }
    let all_blocks: Vec<usize> = (0..n_blocks).collect();
    let is_combos = combinations(n_blocks, n_blocks / 2);
    if is_combos.is_empty() {
        return None;
    }
    let mut logits: Vec<f64> = Vec::with_capacity(is_combos.len());
    for is_blocks in &is_combos {
        let oos_blocks: Vec<usize> = all_blocks
            .iter()
            .copied()
            .filter(|b| !is_blocks.contains(b))
            .collect();
        // IS performance per config; pick the best in-sample.
        let mut best_c = 0usize;
        let mut best_is = f64::NEG_INFINITY;
        for (c, row) in m.iter().enumerate() {
            let is_perf = mean_over(row, is_blocks);
            if is_perf > best_is {
                best_is = is_perf;
                best_c = c;
            }
        }
        // OOS performance of every config; rank the IS-best one.
        let oos_perf: Vec<f64> = m.iter().map(|row| mean_over(row, &oos_blocks)).collect();
        let r = midrank(&oos_perf, oos_perf[best_c]);
        let omega = (r / (n as f64 + 1.0)).clamp(EPS, 1.0 - EPS);
        logits.push((omega / (1.0 - omega)).ln());
    }
    let n_splits = logits.len();
    let pbo = logits.iter().filter(|&&l| l <= 0.0).count() as f64 / n_splits as f64;
    let mut sorted = logits.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_logit = sorted[sorted.len() / 2];
    Some(PboResult {
        pbo,
        n_configs: n,
        n_blocks,
        n_splits,
        median_logit,
    })
}

/// Convenience: build the matrix from each config's trades on a shared grid and
/// run CSCV. All configs MUST share `total_bars` (same symbol+timeframe). Pure.
pub fn pbo_for_configs(
    configs: &[Vec<(usize, f64)>],
    total_bars: usize,
    n_blocks: usize,
) -> Option<PboResult> {
    if configs.len() < MIN_CONFIGS {
        return None;
    }
    let m: Vec<Vec<f64>> = configs
        .iter()
        .map(|t| block_means(t, total_bars, n_blocks))
        .collect();
    cscv_pbo(&m, n_blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_means_buckets_by_entry_index() {
        // 8 bars, 2 blocks → block 0 = bars [0,4), block 1 = [4,8).
        let trades = vec![(0usize, 1.0), (3, 3.0), (4, -2.0), (7, -4.0)];
        let bm = block_means(&trades, 8, 2);
        assert_eq!(bm, vec![2.0, -3.0]); // means per block
        // empty block → 0.0
        assert_eq!(block_means(&[(0, 5.0)], 8, 2), vec![5.0, 0.0]);
    }

    #[test]
    fn rejects_degenerate_inputs() {
        assert!(cscv_pbo(&vec![vec![0.0; 8]; 4], 8).is_none()); // <MIN_CONFIGS
        assert!(cscv_pbo(&vec![vec![0.0; 7]; 10], 7).is_none()); // odd blocks
        assert!(cscv_pbo(&vec![vec![0.0; 3]; 10], 8).is_none()); // ragged
    }

    #[test]
    fn dominant_config_has_low_pbo() {
        // One config is strictly best in EVERY block ⇒ it is always the IS pick
        // AND the OOS best ⇒ never below the OOS median ⇒ PBO ≈ 0.
        let n_blocks = 8;
        let mut m: Vec<Vec<f64>> = Vec::new();
        m.push(vec![1.0; n_blocks]); // the dominant config
        for c in 0..10 {
            // distinct, clearly-worse configs (no ties with the winner)
            m.push((0..n_blocks).map(|b| -0.5 + 0.001 * (c * n_blocks + b) as f64).collect());
        }
        let r = cscv_pbo(&m, n_blocks).expect("pbo");
        assert!(r.pbo < 0.05, "dominant config should be ~not overfit, got {}", r.pbo);
        assert!((0.0..=1.0).contains(&r.pbo));
        assert_eq!(r.n_splits, 70); // C(8,4)
    }

    #[test]
    fn specialist_field_is_overfit() {
        // Each config is excellent on exactly ONE block and poor elsewhere (with
        // tiny distinct jitter to break ties). The IS-best is always whoever's
        // star block sits in the IS half — and that config is poor OOS ⇒ high PBO.
        let n_blocks = 8;
        let n_cfg = 12;
        let mut m: Vec<Vec<f64>> = Vec::new();
        for c in 0..n_cfg {
            let star = c % n_blocks;
            let row: Vec<f64> = (0..n_blocks)
                .map(|b| {
                    let jitter = 0.0001 * ((c * 7 + b * 3) % 11) as f64;
                    if b == star { 1.0 + jitter } else { -0.1 + jitter }
                })
                .collect();
            m.push(row);
        }
        let r = cscv_pbo(&m, n_blocks).expect("pbo");
        assert!(r.pbo > 0.6, "specialist field should be overfit, got {}", r.pbo);
        assert!(r.median_logit <= 0.0, "median logit should be ≤0, got {}", r.median_logit);
    }

    #[test]
    fn pbo_for_configs_threshold() {
        // < MIN_CONFIGS configs → None.
        let few = vec![vec![(0usize, 1.0)]; 4];
        assert!(pbo_for_configs(&few, 100, 8).is_none());
    }
}

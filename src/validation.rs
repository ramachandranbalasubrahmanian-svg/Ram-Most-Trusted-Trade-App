//! Out-of-sample / robustness validation helpers for the confidence pipeline.
//!
//! Pure functions over per-trade `(entry_bar_index, R)` sequences:
//!   * purged + embargoed in-sample / out-of-sample split (no boundary leakage),
//!   * walk-forward k-fold consistency,
//!   * parameter robustness across R:R configs.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

/// Split trades into (in-sample R, out-of-sample R), holding out the most
/// recent `oos_frac` of bars as OOS and PURGING an `embargo_frac` band of bars
/// just before the OOS boundary so a trade straddling the split can't leak.
/// Trades are `(entry_bar_index, R)`.
///
/// * `oos_start = floor(total_bars * (1 - oos_frac))`,
/// * `embargo   = floor(total_bars * embargo_frac)`,
/// * in-sample  = R of trades with `entry_idx < oos_start.saturating_sub(embargo)`,
/// * oos        = R of trades with `entry_idx >= oos_start`,
/// * the band `[oos_start - embargo, oos_start)` is PURGED (dropped from both).
///
/// Robust to empty input and to `oos_frac` / `embargo_frac` at 0 or 1.
pub fn purged_embargoed_split(
    trades: &[(usize, f64)],
    total_bars: usize,
    oos_frac: f64,
    embargo_frac: f64,
) -> (Vec<f64>, Vec<f64>) {
    // Clamp the fractions into [0, 1] so out-of-range inputs degrade gracefully.
    let oos_frac = oos_frac.clamp(0.0, 1.0);
    let embargo_frac = embargo_frac.clamp(0.0, 1.0);

    let tb = total_bars as f64;
    // floor(total_bars * (1 - oos_frac)).
    let oos_start = (tb * (1.0 - oos_frac)).floor() as usize;
    // floor(total_bars * embargo_frac).
    let embargo = (tb * embargo_frac).floor() as usize;

    // The in-sample boundary: trades with entry_idx strictly below this are kept.
    // saturating_sub guards the embargo >= oos_start case.
    let in_sample_end = oos_start.saturating_sub(embargo);

    let mut in_sample_r: Vec<f64> = Vec::new();
    let mut oos_r: Vec<f64> = Vec::new();

    for &(entry_idx, r) in trades {
        if entry_idx >= oos_start {
            // Out-of-sample.
            oos_r.push(r);
        } else if entry_idx < in_sample_end {
            // In-sample.
            in_sample_r.push(r);
        }
        // else: entry_idx in [in_sample_end, oos_start) -> embargo band, PURGED.
    }

    (in_sample_r, oos_r)
}

/// Walk-forward consistency in [0,1]: partition the bar timeline into `k`
/// sequential folds, and return the fraction of non-empty folds whose mean R is
/// positive. Low values flag an edge that only worked in part of history.
///
/// * fold width = `ceil(total_bars / k)`; `fold(idx) = min(idx / width, k - 1)`,
/// * `consistency = (#folds with mean > 0) / (#non-empty folds)`,
/// * fewer than 2 non-empty folds (cannot judge) -> `1.0` (neutral),
/// * `k <= 1` -> `1.0`.
pub fn walkforward_consistency(trades: &[(usize, f64)], total_bars: usize, k: usize) -> f64 {
    if k <= 1 {
        return 1.0;
    }

    // ceil(total_bars / k); guard a zero-width timeline.
    let width = total_bars.div_ceil(k).max(1);

    // Per-fold accumulators: running sum of R and count of trades.
    let mut sums: Vec<f64> = vec![0.0; k];
    let mut counts: Vec<usize> = vec![0usize; k];

    for &(entry_idx, r) in trades {
        let fold = (entry_idx / width).min(k - 1);
        sums[fold] += r;
        counts[fold] += 1;
    }

    let mut non_empty = 0usize;
    let mut positive = 0usize;
    for f in 0..k {
        if counts[f] >= 1 {
            non_empty += 1;
            let mean = sums[f] / counts[f] as f64;
            if mean > 0.0 {
                positive += 1;
            }
        }
    }

    // Cannot judge with fewer than 2 populated folds — stay neutral.
    if non_empty < 2 {
        return 1.0;
    }

    positive as f64 / non_empty as f64
}

/// Parameter robustness in [0,1]: fraction of the supplied config expectancies
/// that are strictly positive. Guards against best-of-N selection (an edge that
/// only the single luckiest R:R config shows is fragile).
///
/// Empty slice -> `1.0` (neutral).
pub fn parameter_robustness(expectancies: &[f64]) -> f64 {
    if expectancies.is_empty() {
        return 1.0;
    }
    let positive = expectancies.iter().filter(|&&e| e > 0.0).count();
    positive as f64 / expectancies.len() as f64
}

// --- tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_drops_embargo_band() {
        // total_bars=100, oos_frac=0.20 -> oos_start = floor(100*0.8) = 80.
        // embargo_frac=0.10 -> embargo = floor(100*0.10) = 10.
        // in_sample_end = 80 - 10 = 70.
        //   in-sample: entry_idx < 70.
        //   embargo (purged): 70 <= entry_idx < 80.
        //   oos: entry_idx >= 80.
        let trades = [
            (10usize, 1.0), // in-sample
            (69, 0.5),      // in-sample (last before embargo)
            (70, -2.0),     // BOUNDARY: start of embargo -> purged from BOTH
            (75, 9.9),      // embargo -> purged
            (79, 9.9),      // embargo -> purged (last before oos)
            (80, 1.5),      // oos (boundary into oos)
            (95, -0.5),     // oos
        ];
        let (is_r, oos_r) = purged_embargoed_split(&trades, 100, 0.20, 0.10);

        // In-sample holds exactly the two pre-embargo trades.
        assert_eq!(is_r, vec![1.0, 0.5]);
        // OOS holds exactly the two post-boundary trades.
        assert_eq!(oos_r, vec![1.5, -0.5]);

        // The boundary trade at idx 70 must appear in NEITHER set.
        assert!(!is_r.contains(&-2.0), "boundary trade leaked into in-sample");
        assert!(!oos_r.contains(&-2.0), "boundary trade leaked into oos");
        // And the rest of the embargo band (75, 79) is gone too.
        assert!(!is_r.contains(&9.9) && !oos_r.contains(&9.9));
    }

    #[test]
    fn purge_robust_to_extremes_and_empty() {
        // Empty input -> two empty vecs.
        let (is_e, oos_e) = purged_embargoed_split(&[], 100, 0.2, 0.1);
        assert!(is_e.is_empty() && oos_e.is_empty());

        // total_bars = 0 -> oos_start = 0, everything is oos (entry_idx >= 0).
        let trades = [(0usize, 1.0), (1, -1.0)];
        let (is0, oos0) = purged_embargoed_split(&trades, 0, 0.2, 0.1);
        assert!(is0.is_empty());
        assert_eq!(oos0, vec![1.0, -1.0]);

        // oos_frac = 1.0 -> oos_start = 0 -> all trades are oos.
        let (is_all_oos, oos_all) = purged_embargoed_split(&trades, 100, 1.0, 0.1);
        assert!(is_all_oos.is_empty());
        assert_eq!(oos_all, vec![1.0, -1.0]);

        // oos_frac = 0.0 -> oos_start = 100; nothing is oos. embargo_frac=1.0 ->
        // embargo=100, in_sample_end = 100.saturating_sub(100) = 0 -> nothing in-sample either.
        let (is_none, oos_none) = purged_embargoed_split(&trades, 100, 0.0, 1.0);
        assert!(is_none.is_empty() && oos_none.is_empty());

        // oos_frac = 0.0, embargo = 0.0 -> oos_start=100, in_sample_end=100; all in-sample.
        let (is_all, oos_z) = purged_embargoed_split(&trades, 100, 0.0, 0.0);
        assert_eq!(is_all, vec![1.0, -1.0]);
        assert!(oos_z.is_empty());
    }

    #[test]
    fn walkforward_all_positive_is_one() {
        // 4 folds of width ceil(100/4)=25: [0,25),[25,50),[50,75),[75,100).
        // One positive-mean trade in each fold => consistency 1.0.
        let trades = [(5usize, 1.0), (30, 2.0), (60, 0.5), (90, 3.0)];
        let c = walkforward_consistency(&trades, 100, 4);
        assert!((c - 1.0).abs() < 1e-9, "expected 1.0, got {c}");
    }

    #[test]
    fn walkforward_half_positive_is_half() {
        // 4 folds; two folds positive-mean, two negative-mean => 0.5.
        let trades = [
            (5usize, 1.0),  // fold 0: +
            (30, -1.0),     // fold 1: -
            (60, 2.0),      // fold 2: +
            (90, -3.0),     // fold 3: -
        ];
        let c = walkforward_consistency(&trades, 100, 4);
        assert!((c - 0.5).abs() < 1e-9, "expected ~0.5, got {c}");
    }

    #[test]
    fn walkforward_neutral_paths() {
        // k <= 1 -> neutral 1.0.
        let trades = [(5usize, -1.0), (10, -2.0)];
        assert_eq!(walkforward_consistency(&trades, 100, 1), 1.0);
        assert_eq!(walkforward_consistency(&trades, 100, 0), 1.0);

        // Fewer than 2 non-empty folds -> neutral 1.0 even if that fold is negative.
        // All trades land in fold 0 here, so only one fold is populated.
        let clustered = [(1usize, -1.0), (2, -2.0), (3, -3.0)];
        let c = walkforward_consistency(&clustered, 100, 4);
        assert_eq!(c, 1.0, "single non-empty fold should stay neutral");

        // Empty input -> 0 non-empty folds -> neutral 1.0.
        assert_eq!(walkforward_consistency(&[], 100, 4), 1.0);
    }

    #[test]
    fn parameter_robustness_counts_positives() {
        // 2 of 4 strictly positive -> 0.5. Zero is NOT positive.
        let exps = [0.3, -0.1, 0.0, 0.2];
        let r = parameter_robustness(&exps);
        assert!((r - 0.5).abs() < 1e-9, "expected 0.5, got {r}");

        // All positive -> 1.0.
        assert!((parameter_robustness(&[0.1, 0.2, 0.3]) - 1.0).abs() < 1e-9);
        // None positive -> 0.0.
        assert!((parameter_robustness(&[-0.1, 0.0, -0.2]) - 0.0).abs() < 1e-9);
        // Empty -> neutral 1.0.
        assert_eq!(parameter_robustness(&[]), 1.0);
    }
}

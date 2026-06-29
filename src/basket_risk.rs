//! Live Trade Plan basket risk — true diversification (ENB) + a Monte-Carlo
//! "bad day" distribution. Display-only.
//!
//! FIREWALLED & PURE: imports only `types` (+ serde). No I/O — the caller passes a
//! recent-returns map (loaded once by `storage_kernel::load_returns_map`). It
//! NEVER feeds `eligible()`, Confidence, ranking, or sizing.
//!
//! Two questions the Live Trade Plan can't answer on its own:
//!   1. Are the basket's N positions really N independent bets, or do they move
//!      together? — pairwise correlation → Effective Number of Bets (ENB). A
//!      5-name basket with ENB 1.5 is barely diversified.
//!   2. What does a realistic bad day look like? — a seeded Monte-Carlo over each
//!      position's win%/RR gives the loss distribution (P(losing day), 5th-pctile
//!      day). Run UNDER INDEPENDENCE and labelled as such, with the ENB stated so
//!      the user knows the true tail is fatter when ENB << N.

use serde::{Deserialize, Serialize};

use crate::types::{BasketRisk, PlanPosition};

/// Monte-Carlo paths. Deterministic (seeded) so the figure is reproducible.
const MC_SIMS: usize = 4000;
const MC_SEED: u64 = 42;

/// Tiny seeded PRNG (SplitMix64) — keeps this module self-contained + deterministic.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (pct / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// Pearson correlation of two equal-length series. None if degenerate. Pure.
fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 3 {
        return None;
    }
    let (a, b) = (&a[a.len() - n..], &b[b.len() - n..]);
    let ma = a.iter().sum::<f64>() / n as f64;
    let mb = b.iter().sum::<f64>() / n as f64;
    let mut cov = 0.0;
    let mut va = 0.0;
    let mut vb = 0.0;
    for i in 0..n {
        let da = a[i] - ma;
        let db = b[i] - mb;
        cov += da * db;
        va += da * da;
        vb += db * db;
    }
    if va <= 0.0 || vb <= 0.0 {
        return None;
    }
    Some(cov / (va.sqrt() * vb.sqrt()))
}

/// Average pairwise correlation across return series. None when <2 usable. Pure.
fn avg_pairwise_corr(series: &[Vec<f64>]) -> Option<f64> {
    let mut sum = 0.0;
    let mut k = 0usize;
    for i in 0..series.len() {
        for j in (i + 1)..series.len() {
            if let Some(c) = pearson(&series[i], &series[j]) {
                sum += c;
                k += 1;
            }
        }
    }
    if k == 0 {
        None
    } else {
        Some(sum / k as f64)
    }
}

/// Effective Number of Bets for an equal-risk basket of `n` names with average
/// pairwise correlation `rho`: `n / (1 + (n-1)·rho)` (rho clamped to [0,1]). Pure.
/// rho=0 ⇒ ENB=n (fully independent); rho=1 ⇒ ENB=1 (one bet).
fn enb(n: usize, rho: f64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    let rho = rho.clamp(0.0, 1.0);
    n as f64 / (1.0 + (n as f64 - 1.0) * rho)
}

fn enb_label(enb: f64, n: usize) -> &'static str {
    if n == 0 {
        return "—";
    }
    let ratio = enb / n as f64;
    if ratio >= 0.8 {
        "well diversified"
    } else if ratio >= 0.5 {
        "moderately correlated"
    } else {
        "concentrated — they move together"
    }
}

/// Monte-Carlo the basket's one-day P&L under INDEPENDENCE. Each position wins
/// (prob `win`) → +rr·risk, else → −risk. Returns (expected, p05, p95,
/// p_losing_day%, prob_big_loss%). Deterministic. Pure.
fn mc_basket(positions: &[(f64, f64, f64)]) -> (f64, f64, f64, f64, f64) {
    if positions.is_empty() {
        return (0.0, 0.0, 0.0, 0.0, 0.0);
    }
    let total_risk: f64 = positions.iter().map(|(_, _, risk)| *risk).sum();
    let big_loss_threshold = -0.5 * total_risk; // "a big loss" = >half the risk budget
    let mut rng = Rng(MC_SEED);
    let mut pnls: Vec<f64> = Vec::with_capacity(MC_SIMS);
    let mut losing = 0usize;
    let mut big = 0usize;
    for _ in 0..MC_SIMS {
        let mut day = 0.0;
        for (win, rr, risk) in positions {
            day += if rng.next_f64() < *win { *rr * *risk } else { -*risk };
        }
        if day < 0.0 {
            losing += 1;
        }
        if day <= big_loss_threshold {
            big += 1;
        }
        pnls.push(day);
    }
    pnls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let expected = pnls.iter().sum::<f64>() / pnls.len() as f64;
    (
        expected,
        percentile(&pnls, 5.0),
        percentile(&pnls, 95.0),
        losing as f64 / MC_SIMS as f64 * 100.0,
        big as f64 / MC_SIMS as f64 * 100.0,
    )
}

/// Compute the basket risk for the Live Trade Plan positions. `returns` maps
/// SYMBOL → recent daily returns (best-effort; correlation uses only names found).
/// `win_rr` carries each position's (win_prob 0..1, rr) aligned with `positions`.
/// Pure.
pub fn compute(
    positions: &[PlanPosition],
    win_rr: &[(f64, f64)],
    returns: &std::collections::HashMap<String, Vec<f64>>,
) -> BasketRisk {
    let n = positions.len();
    if n == 0 {
        return BasketRisk {
            available: false,
            note: "No basket to assess.".to_string(),
            ..Default::default()
        };
    }

    // Correlation / ENB from whatever return series we have.
    let series: Vec<Vec<f64>> = positions
        .iter()
        .filter_map(|p| returns.get(&p.symbol).cloned())
        .filter(|s| s.len() >= 3)
        .collect();
    let names_used = series.len();
    let avg_corr = avg_pairwise_corr(&series);
    // When correlation is unknown (too few series), fall back to a conservative
    // 0.5 prior for ENB so we never overstate diversification.
    let rho = avg_corr.unwrap_or(0.5);
    let enb_v = enb(n, rho);

    // Monte-Carlo loss distribution.
    let mc_positions: Vec<(f64, f64, f64)> = positions
        .iter()
        .zip(win_rr.iter())
        .map(|(p, (win, rr))| (*win, *rr, p.risk_inr))
        .collect();
    let (expected, p05, p95, p_losing, prob_big) = mc_basket(&mc_positions);

    let elabel = enb_label(enb_v, n);
    let corr_str = avg_corr
        .map(|c| format!("avg corr {c:+.2}"))
        .unwrap_or_else(|| "correlation unknown".to_string());
    let note = format!(
        "{n} names but ≈{enb_v:.1} independent bets ({elabel}; {corr_str}). A typical bad day (5th pctile) ≈ ₹{p05:.0}; ~{p_losing:.0}% of days are losers. MC assumes independence — with ENB {enb_v:.1} the true tail is somewhat worse.",
    );

    BasketRisk {
        available: true,
        n,
        names_used,
        avg_corr: avg_corr.unwrap_or(f64::NAN),
        enb: enb_v,
        enb_label: elabel.to_string(),
        expected_inr: expected,
        var5_inr: p05,
        p95_inr: p95,
        p_losing_day: p_losing,
        prob_big_loss: prob_big,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pearson_basic() {
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let b = vec![2.0, 4.0, 6.0, 8.0];
        assert!((pearson(&a, &b).unwrap() - 1.0).abs() < 1e-9);
        let c = vec![4.0, 3.0, 2.0, 1.0];
        assert!((pearson(&a, &c).unwrap() + 1.0).abs() < 1e-9);
    }

    #[test]
    fn enb_spans_independence_to_one_bet() {
        assert!((enb(5, 0.0) - 5.0).abs() < 1e-9, "rho 0 ⇒ ENB n");
        assert!((enb(5, 1.0) - 1.0).abs() < 1e-9, "rho 1 ⇒ ENB 1");
        assert!(enb(5, 0.5) > 1.0 && enb(5, 0.5) < 5.0);
        assert_eq!(enb_label(5.0, 5), "well diversified");
        assert_eq!(enb_label(1.4, 5), "concentrated — they move together");
    }

    #[test]
    fn mc_is_deterministic_and_sane() {
        // 3 positions, 50% win, RR 2, risk 1000 each ⇒ +EV.
        let pos = vec![(0.5, 2.0, 1000.0); 3];
        let (e1, p5_1, _p95, losing, big) = mc_basket(&pos);
        let (e2, p5_2, _, _, _) = mc_basket(&pos); // reproducible
        assert!((e1 - e2).abs() < 1e-9 && (p5_1 - p5_2).abs() < 1e-9);
        assert!(e1 > 0.0, "50%/RR2 is +EV, expected {e1}");
        assert!(p5_1 < 0.0, "a bad day is a loss");
        assert!((0.0..=100.0).contains(&losing) && (0.0..=100.0).contains(&big));
    }

    #[test]
    fn empty_basket_is_unavailable() {
        let br = compute(&[], &[], &std::collections::HashMap::new());
        assert!(!br.available);
    }
}

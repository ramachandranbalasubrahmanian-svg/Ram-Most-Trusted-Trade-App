//! Break-even-vs-edge gauge — FIREWALLED, PURE, display-only.
//!
//! The backtest expectancy is already NET of the full round-trip cost, but that
//! truth is buried in a table. This puts the blunt version on the decision line:
//! how much of one R you pay just to break even, vs how much net edge is actually
//! left. When the net edge is smaller than its own cost floor, the trade is
//! fragile (a small slippage increase flips it negative) → STAND ASIDE.
//!
//! It NEVER feeds Confidence, scoring, ranking, the gate, or the edge map — it only
//! reads numbers already computed and states the honest conclusion. No naked call:
//! "stand aside vs cost floor" is an edge+caveat, never "BUY/SELL X".

use serde::Serialize;

/// Cost drag expressed in R for a setup: the fraction of one R eaten by the
/// round-trip cost. `cost_pct` is the representative round-trip cost as a fraction
/// of one-leg notional (see `costs::backtest_roundtrip_pct`). Returns 0 for a
/// degenerate setup (no risk distance) rather than dividing by zero.
pub fn break_even_r(entry: f64, sl: f64, cost_pct: f64) -> f64 {
    let risk = (entry - sl).abs();
    if risk <= 0.0 || entry <= 0.0 || !cost_pct.is_finite() {
        return 0.0;
    }
    cost_pct * entry / risk
}

/// The gauge shown on a card / the live plan.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CostFloor {
    /// R you pay in round-trip cost just to break even.
    pub break_even_r: f64,
    /// Net (already cost-deducted) expected R per trade.
    pub net_edge_r: f64,
    /// True when the net edge is smaller than the cost floor — fragile, stand aside.
    pub stand_aside: bool,
    /// One-line honest verdict for the decision line.
    pub note: String,
}

/// Assess a setup's net edge against its own cost floor. `expectancy_r` is the
/// backtest's cost-net expectancy; `cost_pct` from `costs::backtest_roundtrip_pct`.
pub fn assess(entry: f64, sl: f64, expectancy_r: f64, cost_pct: f64) -> CostFloor {
    let be = break_even_r(entry, sl, cost_pct);
    let stand_aside = expectancy_r < be;
    let note = if be <= 0.0 {
        format!("Net edge {expectancy_r:+.2}R (cost floor unavailable).")
    } else if stand_aside {
        format!(
            "Net edge {expectancy_r:+.2}R is below the +{be:.2}R you pay in round-trip cost — the edge is smaller than its own cost drag. STAND ASIDE."
        )
    } else {
        format!(
            "Net edge {expectancy_r:+.2}R clears the +{be:.2}R cost floor."
        )
    };
    CostFloor { break_even_r: be, net_edge_r: expectancy_r, stand_aside, note }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn break_even_scales_with_cost_and_inverse_risk() {
        // entry 100, sl 98 → risk 2. cost 0.13% → be = 0.0013*100/2 = 0.065R.
        let be = break_even_r(100.0, 98.0, 0.0013);
        assert!((be - 0.065).abs() < 1e-9, "be={be}");
        // Wider stop (more risk) → smaller cost-in-R.
        assert!(break_even_r(100.0, 90.0, 0.0013) < be);
    }

    #[test]
    fn degenerate_setup_is_zero_not_nan() {
        assert_eq!(break_even_r(100.0, 100.0, 0.0013), 0.0); // no risk distance
        assert_eq!(break_even_r(0.0, 0.0, 0.0013), 0.0);
    }

    #[test]
    fn thin_edge_below_its_cost_floor_stands_aside() {
        // 63MOONS-like: entry 667.90 / sl 674.71 (short) → risk 6.81; net +0.07R.
        // cost floor ≈ 0.0013*667.9/6.81 ≈ 0.127R → 0.07 < 0.127 → stand aside.
        let c = assess(667.90, 674.71, 0.07, 0.0013);
        assert!(c.stand_aside);
        assert!(c.break_even_r > 0.10 && c.break_even_r < 0.15, "be={}", c.break_even_r);
        assert!(c.note.contains("STAND ASIDE"));
    }

    #[test]
    fn healthy_edge_clears_the_floor() {
        // Net +0.40R with a tight cost floor → clears.
        let c = assess(100.0, 95.0, 0.40, 0.0013);
        assert!(!c.stand_aside);
        assert!(c.note.contains("clears"));
    }
}

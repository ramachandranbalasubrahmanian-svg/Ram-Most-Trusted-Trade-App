//! Style validation — does the book the owner ACTUALLY trades have an edge?
//! FIREWALLED, PURE, display-only.
//!
//! The whole expensive stack ranks a universe/direction the owner never trades. His
//! real book is LONG small-cap/SME/IPO momentum — and on those exact names the
//! validated edges are mostly SHORT. This runs the trusted, already-computed edge
//! map through his actual lens: LONG × the momentum/breakout strategy family ×
//! the small-cap universe → how many names carry an eligible (cost-net) edge, the
//! aggregate stats, and the brutal verdict: real edge, or luck.
//!
//! It NEVER feeds Confidence/scoring/ranking/the edge map, and emits no buy signal —
//! it reports on a STYLE, with a mandatory survivorship caveat.

use std::collections::HashMap;

use serde::Serialize;

use crate::config::Direction;
use crate::strategy_engine::EdgeIndex;

/// The breakout/momentum strategy family — the owner's actual style.
pub const MOMENTUM_FAMILY: &[&str] = &["gap_and_go", "rvol_breakout", "donchian_breakout", "orb_15m"];

/// One eligible long-momentum edge on a small-cap name.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StyleEdge {
    pub symbol: String,
    pub strategy: String,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub win_pct: f64,
    pub n: usize,
}

/// The verdict on the owner's actual style.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StyleReport {
    pub universe_label: String,
    pub direction: String,
    pub strategies: Vec<String>,
    /// # small-cap symbols with market-cap data (the tested denominator).
    pub smallcap_universe: usize,
    /// # of those with ≥1 eligible long-momentum edge.
    pub names_with_edge: usize,
    /// total eligible long-momentum edges across the universe.
    pub eligible_edges: usize,
    pub median_expectancy_r: f64,
    pub median_pf: f64,
    pub median_win: f64,
    /// strongest eligible edges by expectancy (up to 8).
    pub top: Vec<StyleEdge>,
    pub verdict: String,
    pub note: String,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Build the style verdict. `mcap_of` maps SYMBOL → market cap (₹); only symbols
/// with a positive cap ≤ `smallcap_max_mcap` form the tested universe. Pure.
pub fn build(edges: &EdgeIndex, mcap_of: &HashMap<String, f64>, smallcap_max_mcap: f64) -> StyleReport {
    let is_momentum = |s: &str| MOMENTUM_FAMILY.contains(&s);

    let smallcaps: Vec<&String> = mcap_of
        .iter()
        .filter(|(_, m)| **m > 0.0 && **m <= smallcap_max_mcap)
        .map(|(s, _)| s)
        .collect();
    let smallcap_universe = smallcaps.len();

    let mut all_edges: Vec<StyleEdge> = Vec::new();
    let mut names_with_edge = 0usize;
    for sym in &smallcaps {
        if let Some(list) = edges.get(*sym) {
            let mut has = false;
            for e in list
                .iter()
                .filter(|e| e.direction == Direction::Long && is_momentum(&e.strategy))
            {
                has = true;
                all_edges.push(StyleEdge {
                    symbol: (*sym).clone(),
                    strategy: e.strategy.clone(),
                    expectancy_r: e.expectancy_r,
                    profit_factor: e.profit_factor,
                    win_pct: e.win_pct,
                    n: e.n,
                });
            }
            if has {
                names_with_edge += 1;
            }
        }
    }

    let eligible_edges = all_edges.len();
    let median_expectancy_r = median(all_edges.iter().map(|e| e.expectancy_r).collect());
    let median_pf = median(all_edges.iter().map(|e| e.profit_factor).collect());
    let median_win = median(all_edges.iter().map(|e| e.win_pct).collect());

    let mut top = all_edges.clone();
    top.sort_by(|a, b| b.expectancy_r.partial_cmp(&a.expectancy_r).unwrap_or(std::cmp::Ordering::Equal));
    top.truncate(8);

    let coverage = if smallcap_universe > 0 {
        names_with_edge as f64 / smallcap_universe as f64
    } else {
        0.0
    };
    let verdict = if smallcap_universe == 0 {
        "Small-cap universe unavailable — market-cap data isn't loaded (needs symbol_metadata.parquet). Can't validate the style yet.".to_string()
    } else if eligible_edges == 0 {
        "NO validated long-momentum edge in the small-cap universe. The engine finds ZERO eligible long edges in this family (net of cost). Long small-cap/SME/IPO momentum is not a repeatable edge here — wins are regime/luck, not proof.".to_string()
    } else if coverage < 0.05 {
        format!(
            "THIN, NAME-SPECIFIC edge only: {names_with_edge} of {smallcap_universe} small-caps ({:.1}%) carry an eligible long-momentum edge (median exp {median_expectancy_r:+.2}R). NOT a broad validated style — don't assume your NEXT pick has it; verify it's one of these names.",
            coverage * 100.0
        )
    } else {
        format!(
            "There IS a validated long-momentum edge in small-caps: {names_with_edge} of {smallcap_universe} names ({:.1}%), median exp {median_expectancy_r:+.2}R / PF {median_pf:.2}. Trade only names on this list, in this direction.",
            coverage * 100.0
        )
    };
    let note = "Eligible = n≥30, PF≥1.2, cost-net expectancy>0. SURVIVORSHIP CAVEAT: the backtest only contains names that survived to today; small-caps that blew up were delisted and don't appear, so even a positive read is optimistic. Momentum is regime-dependent. Display-only — never a buy signal.".to_string();

    StyleReport {
        universe_label: format!("small/mid-cap (< ₹{:.0} Cr)", smallcap_max_mcap / 1e7),
        direction: "LONG".to_string(),
        strategies: MOMENTUM_FAMILY.iter().map(|s| s.to_string()).collect(),
        smallcap_universe,
        names_with_edge,
        eligible_edges,
        median_expectancy_r,
        median_pf,
        median_win,
        top,
        verdict,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EligibleEdge, Robustness};

    fn edge(strategy: &str, dir: Direction, exp: f64) -> EligibleEdge {
        EligibleEdge {
            strategy: strategy.into(),
            direction: dir,
            expectancy_r: exp,
            profit_factor: 1.4,
            win_pct: 55.0,
            n: 40,
            robustness: Robustness::default(),
        }
    }

    fn mcaps() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("SMALLA".into(), 1_000.0 * 1e7); // ₹1,000 Cr — small
        m.insert("SMALLB".into(), 3_000.0 * 1e7); // ₹3,000 Cr — small
        m.insert("BIGCAP".into(), 500_000.0 * 1e7); // ₹5,00,000 Cr — large, excluded
        m
    }

    const SMALL_MAX: f64 = 20_000.0 * 1e7; // ₹20,000 Cr

    #[test]
    fn no_long_momentum_edge_is_brutal() {
        // SMALLA has only a SHORT momentum edge + a long NON-momentum edge → none qualify.
        let mut edges = EdgeIndex::new();
        edges.insert("SMALLA".into(), vec![edge("gap_and_go", Direction::Short, 0.3), edge("vwap_cross", Direction::Long, 0.3)]);
        let r = build(&edges, &mcaps(), SMALL_MAX);
        assert_eq!(r.eligible_edges, 0);
        assert_eq!(r.names_with_edge, 0);
        assert!(r.verdict.contains("NO validated"));
        assert_eq!(r.smallcap_universe, 2); // BIGCAP excluded
    }

    #[test]
    fn counts_eligible_long_momentum_edges() {
        let mut edges = EdgeIndex::new();
        edges.insert("SMALLA".into(), vec![edge("rvol_breakout", Direction::Long, 0.20), edge("orb_15m", Direction::Long, 0.30)]);
        edges.insert("SMALLB".into(), vec![edge("gap_and_go", Direction::Long, 0.10)]);
        // BIGCAP has a long-momentum edge but is out of the small-cap universe.
        edges.insert("BIGCAP".into(), vec![edge("gap_and_go", Direction::Long, 0.99)]);
        let r = build(&edges, &mcaps(), SMALL_MAX);
        assert_eq!(r.eligible_edges, 3); // 2 on SMALLA + 1 on SMALLB
        assert_eq!(r.names_with_edge, 2);
        assert!((r.median_expectancy_r - 0.20).abs() < 1e-9); // median of [0.10,0.20,0.30]
        assert_eq!(r.top[0].symbol, "SMALLA"); // strongest (0.30) first
        assert!(!r.top.iter().any(|e| e.symbol == "BIGCAP"));
    }

    #[test]
    fn large_cap_universe_is_excluded() {
        let mut edges = EdgeIndex::new();
        edges.insert("BIGCAP".into(), vec![edge("gap_and_go", Direction::Long, 0.5)]);
        let r = build(&edges, &mcaps(), SMALL_MAX);
        assert_eq!(r.smallcap_universe, 2);
        assert_eq!(r.eligible_edges, 0);
    }
}

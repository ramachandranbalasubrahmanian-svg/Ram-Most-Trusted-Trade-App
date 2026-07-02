//! Simple picks — top-5 buy / top-5 sell for a capital + risk%, plus an HONEST
//! target-feasibility readout. FIREWALLED, PURE, display-only.
//!
//! Re-presents the Capital-Fit Finder's ranked, cost-net edges — DSR-gated since
//! 2026-07-02 (fit_symbol deflates each candidate's Sharpe against its full
//! 120-config trial set, same gate as the scanner/deep-dive) — as the smallest
//! actionable card the owner asked for: side, qty, entry, target, stop, and the
//! backtested win rate as "X/10". The finder already picks the best backtested
//! strategy per name (ranked by Confidence × deployability); here we split by side,
//! dedupe to one row per stock, drop names that are `blocked` (T2T/ASM/GSM) or whose
//! net edge sits inside its own cost floor, and INLINE-FLAG thin `high_risk`/`caution`
//! names (may be unable to exit intraday) so no illiquid name ever appears un-annotated.
//!
//! The target ("grow ₹X → ₹Y") NEVER inflates position size — sizing stays at the
//! chosen risk%. It only drives a blunt feasibility line, because sizing up to chase
//! an aspirational target is exactly how leveraged intraday accounts blow up. Never a
//! naked call, never feeds Confidence/scoring/the edge map.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::types::FinderRow;

/// Per-symbol tradability flag passed into `build` (verdict + one-line reason).
#[derive(Debug, Clone, PartialEq)]
pub struct TradInfo {
    pub verdict: String, // "blocked" | "high_risk" | "caution" | "ok"
    pub reason: String,
}

/// One actionable pick — deliberately minimal.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Pick {
    pub symbol: String,
    pub side: String, // "BUY" | "SELL"
    pub strategy: String,
    pub interval: String,
    pub qty: i64,
    pub entry: f64,
    pub target: f64,
    pub stop_loss: f64,
    /// Backtested win rate (%) and the "X/10" form the owner asked for.
    pub win_pct: f64,
    pub win_out_of_10: u32,
    pub n_trades: usize,
    pub net_profit: f64, // ₹ if target hits (after cost)
    pub net_loss: f64,   // ₹ if stop hits (after cost)
    /// Backtest expectancy (R, net) — used for the target readout, not shown.
    pub expectancy_r: f64,
    /// Tradability verdict ("ok" | "high_risk" | "caution" | "unverified").
    pub tradability: String,
    /// The tradability caption when the name is thin/elevated-risk (else None).
    pub tradability_note: Option<String>,
}

/// Honest feasibility of a growth target — informational, never a sizing input.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TargetPlan {
    pub from: f64,
    pub to: f64,
    pub multiple: f64,
    /// Expected fractional capital gain per trade at the shown edge × risk% (%).
    pub per_trade_growth_pct: f64,
    /// Rough count of winning-EXPECTATION trades to reach it (0 if not growable).
    pub est_trades_needed: u64,
    /// "reasonable" | "aggressive" | "unrealistic".
    pub realism: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PicksResult {
    pub capital: f64,
    pub risk_pct: f64,
    pub top_buy: Vec<Pick>,
    pub top_sell: Vec<Pick>,
    pub target: Option<TargetPlan>,
    /// False when the tradability cache was empty (cold) — the UI must warn that
    /// T2T/ASM/GSM + liquidity were NOT checked and the trader must verify manually.
    pub tradability_verified: bool,
    pub caveat: String,
}

/// "X/10" from the WILSON LOWER BOUND of the backtested win rate — the honest
/// statistical floor, not the raw in-sample average. The raw % is a
/// best-of-many-configs pick and systematically optimistic, especially on
/// small n (65.9% over 59 trades used to round UP to 7/10; the floor says
/// ~6/10). The raw win_pct is still shown alongside; this is the number the
/// owner acts on, so it gets the conservative treatment (same wilson_lower
/// the scanner's prob_floor already uses).
fn win_10(win_rate_pct: f64, n: usize) -> u32 {
    (crate::stats::wilson_lower(win_rate_pct / 100.0, n) * 10.0)
        .round()
        .clamp(0.0, 10.0) as u32
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Pick the best usable rows for one side (already fit-ranked), one per symbol,
/// attaching the tradability flag. Excludes `blocked` names and sub-cost-floor edges.
fn top_side(
    rows: &[FinderRow],
    side: &str,
    trad_of: &HashMap<String, TradInfo>,
    cost_pct: f64,
) -> Vec<Pick> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Pick> = Vec::new();
    for r in rows.iter().filter(|r| r.side == side) {
        let info = trad_of.get(&r.symbol);
        let verdict = info.map(|t| t.verdict.as_str()).unwrap_or("unverified");
        if verdict == "blocked" {
            continue;
        }
        let usable = r.expectancy_r > 0.0
            && r.shares > 0
            && !crate::cost_floor::assess(r.entry, r.sl, r.expectancy_r, cost_pct).stand_aside;
        if !usable {
            continue;
        }
        if !seen.insert(r.symbol.clone()) {
            continue;
        }
        let note = match verdict {
            "high_risk" | "caution" => info.map(|t| t.reason.clone()),
            "unverified" => Some("tradability not verified — check T2T/ASM/GSM + liquidity before trading".to_string()),
            _ => None,
        };
        out.push(Pick {
            symbol: r.symbol.clone(),
            side: r.side.clone(),
            strategy: r.strategy.clone(),
            interval: r.interval.clone(),
            qty: r.shares,
            entry: r.entry,
            target: r.target,
            stop_loss: r.sl,
            win_pct: r.win_rate,
            win_out_of_10: win_10(r.win_rate, r.n_trades),
            n_trades: r.n_trades,
            net_profit: r.net_profit,
            net_loss: r.net_loss,
            expectancy_r: r.expectancy_r,
            tradability: verdict.to_string(),
            tradability_note: note,
        });
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// Build the picks from the finder's ranked rows. `trad_of` = per-symbol tradability
/// (empty ⇒ cold cache ⇒ `tradability_verified=false`). `cost_pct` = round-trip cost
/// fraction (drops sub-cost-floor edges). Pure; sanitises non-finite inputs.
pub fn build(
    rows: &[FinderRow],
    capital: f64,
    risk_pct: f64,
    target_from: f64,
    target_to: f64,
    trad_of: &HashMap<String, TradInfo>,
    cost_pct: f64,
) -> PicksResult {
    let top_buy = top_side(rows, "BUY", trad_of, cost_pct);
    let top_sell = top_side(rows, "SELL", trad_of, cost_pct);

    // Honest target feasibility — median expectancy of the shown picks, compounded
    // at the chosen risk%. Never changes position size. Non-finite inputs → no plan.
    let target = if target_from.is_finite()
        && target_to.is_finite()
        && target_from > 0.0
        && target_to > target_from
    {
        let multiple = target_to / target_from;
        let exps: Vec<f64> = top_buy.iter().chain(top_sell.iter()).map(|p| p.expectancy_r).collect();
        let median_exp = median(exps);
        let g = risk_pct * median_exp; // expected fractional capital gain per trade
        let per_trade_growth_pct = g * 100.0;
        let (est_trades_needed, realism, note) = if g > 0.0 && multiple.is_finite() {
            // Cap the estimate so a tiny edge can't overflow into u64::MAX.
            let n = (multiple.ln() / (1.0 + g).ln()).ceil().clamp(0.0, 1_000_000.0) as u64;
            if multiple <= 1.5 {
                (n, "reasonable".to_string(), format!(
                    "~{n} winning-expectation trades at ~{per_trade_growth_pct:.2}%/trade. Compounding is bumpy — expect drawdowns; position size stays at your {:.2}% risk, never scaled up to chase the target.",
                    risk_pct * 100.0
                ))
            } else if multiple <= 3.0 {
                (n, "aggressive".to_string(), format!(
                    "{multiple:.1}× is aggressive: ~{n} trades at the current edge, with a MEANINGFUL risk of ruin along the way. Do NOT size up to get there faster — that is how leveraged accounts blow up."
                ))
            } else {
                (n, "unrealistic".to_string(), format!(
                    "{multiple:.0}× is UNREALISTIC for intraday: it needs ~{n} trades at the current thin edge, or luck / ruinous leverage. The honest expectation is far slower — treat the target as a direction, not a plan."
                ))
            }
        } else {
            (0, "unrealistic".to_string(),
             "The shown picks have no positive net expectation to compound — there is no honest path to this target from here today.".to_string())
        };
        Some(TargetPlan { from: target_from, to: target_to, multiple, per_trade_growth_pct, est_trades_needed, realism, note })
    } else {
        None
    };

    let caveat = "Backtested win% is the historical average over n trades — NOT a promise, and it says nothing about the NEXT trade. Entry/target/stop/qty are net of estimated cost. Signals only — not advice, no orders are placed. Verify tradability + your live fill before acting; MIS squares off ~15:20 IST.".to_string();

    PicksResult {
        capital,
        risk_pct,
        top_buy,
        top_sell,
        target,
        tradability_verified: !trad_of.is_empty(),
        caveat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sym: &str, side: &str, win: f64, exp: f64, entry: f64, sl: f64) -> FinderRow {
        FinderRow {
            symbol: sym.into(),
            strategy: "gap_and_go".into(),
            side: side.into(),
            interval: "30 Minutes".into(),
            rr_label: "1 : 2.0".into(),
            entry,
            atr: (entry - sl).abs() / 1.5,
            sl,
            target: entry + (entry - sl) * 2.0,
            shares: 100,
            shares_by_risk: 100,
            max_affordable: 500,
            fit: "ideal".into(),
            capital_deployed: entry * 100.0,
            capital_efficiency_pct: 50.0,
            risk_taken: 5000.0,
            net_profit: 9000.0,
            net_loss: -5000.0,
            confidence: 60,
            expectancy_r: exp,
            win_rate: win,
            profit_factor: 1.4,
            n_trades: 80,
            fit_score: win,
        }
    }

    fn trad(pairs: &[(&str, &str)]) -> HashMap<String, TradInfo> {
        pairs.iter().map(|(s, v)| ((*s).to_string(), TradInfo { verdict: (*v).to_string(), reason: format!("{v} caption") })).collect()
    }

    #[test]
    fn win_pct_becomes_out_of_ten_via_wilson_floor() {
        // n=80 (from row()): Wilson lower of 66% is ~55.1% → 6/10 (the raw
        // rounding said 7/10 — the exact overstatement W4 removes); 51.4% →
        // ~40.6% → 4/10. The raw win_pct stays available alongside.
        let rows = vec![row("A", "BUY", 66.0, 0.30, 100.0, 95.0), row("B", "BUY", 51.4, 0.30, 100.0, 95.0)];
        let r = build(&rows, 500_000.0, 0.02, 0.0, 0.0, &trad(&[("A", "ok"), ("B", "ok")]), 0.0013);
        assert_eq!(r.top_buy[0].win_out_of_10, 6);
        assert_eq!(r.top_buy[1].win_out_of_10, 4);
        assert_eq!(r.top_buy[0].win_pct, 66.0, "raw % still reported");
        // The floor tightens toward the raw value as n grows.
        assert_eq!(super::win_10(66.0, 10_000), 7);
        assert_eq!(super::win_10(66.0, 0), 0, "no trades → no claim");
    }

    #[test]
    fn splits_sides_and_caps_at_five_each() {
        let mut rows = Vec::new();
        let mut t = Vec::new();
        for i in 0..8 {
            rows.push(row(&format!("B{i}"), "BUY", 60.0, 0.3, 100.0, 95.0));
            rows.push(row(&format!("S{i}"), "SELL", 60.0, 0.3, 100.0, 95.0));
        }
        for i in 0..8 { t.push((format!("B{i}"), "ok".to_string())); t.push((format!("S{i}"), "ok".to_string())); }
        let tm: HashMap<String, TradInfo> = t.iter().map(|(s, v)| (s.clone(), TradInfo { verdict: v.clone(), reason: String::new() })).collect();
        let r = build(&rows, 500_000.0, 0.02, 0.0, 0.0, &tm, 0.0013);
        assert_eq!(r.top_buy.len(), 5);
        assert_eq!(r.top_sell.len(), 5);
        assert!(r.top_buy.iter().all(|p| p.side == "BUY"));
    }

    #[test]
    fn blocked_excluded_thin_flagged_dupes_dropped() {
        let rows = vec![
            row("GOOD", "BUY", 60.0, 0.30, 100.0, 95.0),
            row("GOOD", "BUY", 58.0, 0.30, 100.0, 95.0),    // dupe symbol → dropped
            row("BLOCKED", "BUY", 70.0, 0.30, 100.0, 95.0), // T2T → excluded
            row("THINNAME", "BUY", 65.0, 0.30, 100.0, 95.0),// high_risk → INCLUDED but flagged
            row("SUBCOST", "BUY", 55.0, 0.001, 100.0, 99.9),// net edge inside cost floor → dropped
        ];
        let tm = trad(&[("GOOD", "ok"), ("BLOCKED", "blocked"), ("THINNAME", "high_risk"), ("SUBCOST", "ok")]);
        let r = build(&rows, 500_000.0, 0.02, 0.0, 0.0, &tm, 0.0013);
        let syms: Vec<&str> = r.top_buy.iter().map(|p| p.symbol.as_str()).collect();
        assert_eq!(syms, vec!["GOOD", "THINNAME"]);
        let thin = r.top_buy.iter().find(|p| p.symbol == "THINNAME").unwrap();
        assert_eq!(thin.tradability, "high_risk");
        assert!(thin.tradability_note.is_some());
        assert!(r.top_buy.iter().find(|p| p.symbol == "GOOD").unwrap().tradability_note.is_none());
    }

    #[test]
    fn cold_cache_marks_unverified_and_flags_rows() {
        let rows = vec![row("A", "BUY", 60.0, 0.30, 100.0, 95.0)];
        let r = build(&rows, 500_000.0, 0.02, 0.0, 0.0, &HashMap::new(), 0.0013);
        assert!(!r.tradability_verified);
        assert_eq!(r.top_buy[0].tradability, "unverified");
        assert!(r.top_buy[0].tradability_note.is_some());
    }

    #[test]
    fn huge_target_reads_unrealistic_and_never_promises() {
        let rows = vec![row("A", "BUY", 60.0, 0.20, 100.0, 95.0)];
        let r = build(&rows, 5_000.0, 0.02, 5_000.0, 200_000.0, &trad(&[("A", "ok")]), 0.0013);
        let t = r.target.unwrap();
        assert_eq!(t.realism, "unrealistic");
        assert!((t.multiple - 40.0).abs() < 1e-9);
        assert!(t.est_trades_needed > 100 && t.est_trades_needed <= 1_000_000);
    }

    #[test]
    fn non_finite_target_is_rejected_no_overflow() {
        let rows = vec![row("A", "BUY", 60.0, 0.20, 100.0, 95.0)];
        let r = build(&rows, 5_000.0, 0.02, 5_000.0, f64::INFINITY, &trad(&[("A", "ok")]), 0.0013);
        assert!(r.target.is_none()); // inf target → no plan, no u64::MAX
    }

    #[test]
    fn no_target_when_range_absent() {
        let rows = vec![row("A", "BUY", 60.0, 0.20, 100.0, 95.0)];
        let r = build(&rows, 500_000.0, 0.02, 0.0, 0.0, &trad(&[("A", "ok")]), 0.0013);
        assert!(r.target.is_none());
    }
}

//! Live Trade Plan — portfolio-aware budget/risk/ATR sizing (display-only).
//!
//! FIREWALLED: imports only `config` + `types`. It NEVER places, modifies, or
//! cancels an order, and NEVER feeds `eligible()`, Confidence, ranking, or the
//! edge map. It only ANSWERS the question the per-signal sizing can't: "given my
//! budget and risk rate, which of these Top-10 ideas can I actually take TOGETHER,
//! and what is my real aggregate risk?".
//!
//! The per-signal sizing (`risk_manager::size`) already does the hard part —
//! risk-first, ATR-based shares, leverage-capped. The risk meter then SUMS all 20
//! ideas, which over-counts wildly (e.g. 431% exposure) because you can't take
//! them all. This planner instead SELECTS a realistic basket:
//!
//!   * highest-score ideas first, across both sides;
//!   * each keeps the user's exact risk% (drop-to-fit — never silently scaled);
//!   * stop when the next idea would breach the leverage notional cap
//!     (`budget × LEVERAGE`), the portfolio total-risk ceiling
//!     (`max(one-trade risk, budget × MAX_PORTFOLIO_RISK_PCT)`), the max-concurrent
//!     count, or the per-sector cap.
//!
//! Every skip is counted + explained, so the user understands why only N of the
//! Top-10 made the basket.

use std::collections::HashMap;

use crate::config::{self, UserSettings};
use crate::types::{PlanPosition, PlanTotals, RankedSignal, TradePlan};

/// Liquidity verdict from a participation rate (planned qty / ADV, in %). Pure.
/// `None` ADV ⇒ "unknown" (never assumed fillable).
fn liquidity_verdict(participation_pct: Option<f64>) -> &'static str {
    match participation_pct {
        None => "unknown",
        Some(p) if p < 1.0 => "ok",
        Some(p) if p < 5.0 => "caution",
        Some(p) if p < 20.0 => "heavy",
        Some(_) => "illiquid",
    }
}

/// Build the Live Trade Plan from the ranked Top-10 lists. `sectors` maps SYMBOL
/// → sector (best-effort; empty disables the per-sector cap). `adv` maps SYMBOL →
/// average daily share volume (best-effort; empty disables the liquidity flag).
/// Pure.
pub fn build_plan(
    buy: &[RankedSignal],
    sell: &[RankedSignal],
    settings: &UserSettings,
    sectors: &HashMap<String, String>,
    adv: &HashMap<String, f64>,
    returns: &HashMap<String, Vec<f64>>,
) -> TradePlan {
    let budget = settings.budget;
    let max_notional = settings.max_notional();
    let per_trade_risk = settings.risk_amount();
    // Portfolio risk ceiling: the configured fraction, but never below a single
    // trade at the user's risk% (so one trade is always allowed).
    let risk_cap = (budget * config::MAX_PORTFOLIO_RISK_PCT).max(per_trade_risk);
    let max_concurrent = config::PLAN_MAX_CONCURRENT;
    let max_per_sector = config::PLAN_MAX_PER_SECTOR;

    // Merge both sides and order by score (desc); both inputs are already
    // per-side-sorted, so a stable score sort gives the global ranking.
    let mut pool: Vec<&RankedSignal> = buy.iter().chain(sell.iter()).collect();
    pool.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let considered = pool.len();

    let mut positions: Vec<PlanPosition> = Vec::new();
    // (win_prob 0..1, rr) per accepted position — for the basket Monte-Carlo.
    let mut win_rr: Vec<(f64, f64)> = Vec::new();
    let mut sector_count: HashMap<String, usize> = HashMap::new();
    let mut deployed = 0.0_f64;
    let mut total_risk = 0.0_f64;
    let (mut skip_lev, mut skip_risk, mut skip_conc, mut skip_sector) = (0usize, 0usize, 0usize, 0usize);

    for r in pool {
        if r.shares <= 0 {
            continue;
        }
        // Concurrency cap is terminal once hit (every remaining idea is also blocked).
        if positions.len() >= max_concurrent {
            skip_conc += 1;
            continue;
        }
        let risk_inr = r.proj_loss.abs();
        let sector = sectors
            .get(r.symbol.as_str())
            .cloned()
            .unwrap_or_default();

        // Sector cap (best-effort — only when the sector is known).
        if !sector.is_empty() {
            if let Some(c) = sector_count.get(&sector) {
                if *c >= max_per_sector {
                    skip_sector += 1;
                    continue;
                }
            }
        }
        // Leverage notional headroom.
        if deployed + r.notional > max_notional + f64::EPSILON {
            skip_lev += 1;
            continue;
        }
        // Portfolio total-risk ceiling.
        if total_risk + risk_inr > risk_cap + f64::EPSILON {
            skip_risk += 1;
            continue;
        }

        // Accept.
        deployed += r.notional;
        total_risk += risk_inr;
        if !sector.is_empty() {
            *sector_count.entry(sector.clone()).or_insert(0) += 1;
        }
        let sl_dist = (r.entry - r.sl).abs();
        let atr = if config::SL_ATR_MULT > 0.0 { sl_dist / config::SL_ATR_MULT } else { 0.0 };
        let atr_pct = if r.entry > 0.0 { sl_dist / r.entry * 100.0 } else { 0.0 };
        // Liquidity-at-size: planned qty vs average daily volume.
        let adv_v = adv.get(r.symbol.as_str()).copied().unwrap_or(0.0);
        let participation_pct = if adv_v > 0.0 { r.shares as f64 / adv_v * 100.0 } else { 0.0 };
        let liquidity = liquidity_verdict(if adv_v > 0.0 { Some(participation_pct) } else { None });
        let max_safe_qty = (adv_v * config::LIQUIDITY_PARTICIPATION_CAP).floor() as i64;
        positions.push(PlanPosition {
            symbol: r.symbol.clone(),
            side: r.side.clone(),
            strategy: r.strategy.clone(),
            shares: r.shares,
            entry: r.entry,
            sl: r.sl,
            target: r.target,
            risk_inr,
            notional: r.notional,
            atr,
            atr_pct,
            proj_profit: r.proj_profit,
            proj_loss: r.proj_loss,
            exp_pnl: r.exp_pnl,
            sector,
            adv: adv_v,
            participation_pct,
            liquidity: liquidity.to_string(),
            max_safe_qty,
        });
        // win% and reward:risk for the basket Monte-Carlo.
        let rr_ratio = if sl_dist > 0.0 { (r.target - r.entry).abs() / sl_dist } else { 0.0 };
        win_rr.push(((r.win_pct / 100.0).clamp(0.0, 1.0), rr_ratio));
    }

    let totals = compute_totals(&positions, budget, max_notional);
    let basket_risk = crate::basket_risk::compute(&positions, &win_rr, returns);
    let notes = build_notes(&totals, skip_lev, skip_risk, skip_conc, skip_sector, risk_cap, budget);

    TradePlan {
        positions,
        totals,
        basket_risk,
        considered,
        skipped_leverage: skip_lev,
        skipped_risk_cap: skip_risk,
        skipped_concurrent: skip_conc,
        skipped_sector: skip_sector,
        notes,
    }
}

fn compute_totals(positions: &[PlanPosition], budget: f64, max_notional: f64) -> PlanTotals {
    let mut t = PlanTotals {
        budget,
        max_notional,
        ..Default::default()
    };
    for p in positions {
        t.n_positions += 1;
        let long = p.side.eq_ignore_ascii_case("BUY") || p.side.eq_ignore_ascii_case("LONG");
        if long {
            t.n_long += 1;
            t.long_notional += p.notional;
        } else {
            t.n_short += 1;
            t.short_notional += p.notional;
        }
        t.deployed += p.notional;
        t.total_risk_inr += p.risk_inr;
        t.exp_pnl += p.exp_pnl;
        t.best_case += p.proj_profit;
        t.worst_case += p.proj_loss;
        if p.liquidity == "heavy" || p.liquidity == "illiquid" {
            t.n_illiquid += 1;
        }
    }
    t.deployed_pct = if max_notional > 0.0 { t.deployed / max_notional * 100.0 } else { 0.0 };
    t.free_margin = max_notional - t.deployed;
    t.total_risk_pct = if budget > 0.0 { t.total_risk_inr / budget * 100.0 } else { 0.0 };
    t.color = if t.deployed_pct < 60.0 {
        "green"
    } else if t.deployed_pct < 90.0 {
        "amber"
    } else {
        "red"
    }
    .to_string();
    t
}

fn build_notes(
    t: &PlanTotals,
    skip_lev: usize,
    skip_risk: usize,
    skip_conc: usize,
    skip_sector: usize,
    risk_cap: f64,
    budget: f64,
) -> Vec<String> {
    let mut notes = Vec::new();
    if t.n_positions == 0 {
        notes.push(
            "No tradable basket — no Top-10 idea fit your budget/risk yet (or signals are empty)."
                .to_string(),
        );
        return notes;
    }
    notes.push(format!(
        "Take {} of {} ideas: ₹{:.0} deployed ({:.0}% of your ₹{:.0} max notional), ₹{:.0} at risk ({:.1}% of budget if every stop hits).",
        t.n_positions,
        t.n_positions + skip_lev + skip_risk + skip_conc + skip_sector,
        t.deployed,
        t.deployed_pct,
        t.max_notional,
        t.total_risk_inr,
        t.total_risk_pct,
    ));
    if skip_lev > 0 {
        notes.push(format!(
            "{skip_lev} more idea(s) didn't fit your 5× leverage headroom (₹{:.0} max notional).",
            t.max_notional
        ));
    }
    if skip_risk > 0 {
        notes.push(format!(
            "{skip_risk} idea(s) skipped to keep total risk under ₹{risk_cap:.0} ({:.0}% of budget).",
            risk_cap / budget * 100.0
        ));
    }
    if skip_conc > 0 {
        notes.push(format!(
            "{skip_conc} idea(s) beyond the {}-position concurrency cap.",
            config::PLAN_MAX_CONCURRENT
        ));
    }
    if skip_sector > 0 {
        notes.push(format!(
            "{skip_sector} idea(s) skipped by the ≤{}-per-sector diversification cap.",
            config::PLAN_MAX_PER_SECTOR
        ));
    }
    // Liquidity-at-size warning — the order may be too big for the stock.
    if t.n_illiquid > 0 {
        notes.push(format!(
            "⚠ {} position(s) exceed a safe fill size (>5% of daily volume) — the SL/target prices are unrealistic at that qty; reduce to the 'max fill' shown or skip.",
            t.n_illiquid
        ));
    }
    // Directional bias.
    if t.n_long > 0 && t.n_short == 0 {
        notes.push("Basket is ALL LONG — fully exposed to a market-wide down move.".to_string());
    } else if t.n_short > 0 && t.n_long == 0 {
        notes.push("Basket is ALL SHORT — fully exposed to a market-wide up move.".to_string());
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Robustness;

    fn sig(sym: &str, side: &str, score: f64, entry: f64, shares: i64, sl_dist: f64) -> RankedSignal {
        // proj_loss = -shares*sl_dist (a long); notional = shares*entry.
        RankedSignal {
            symbol: sym.into(),
            strategy: "s".into(),
            side: side.into(),
            entry,
            sl: entry - sl_dist,
            target: entry + 2.0 * sl_dist,
            shares,
            notional: shares as f64 * entry,
            proj_profit: shares as f64 * 2.0 * sl_dist,
            proj_loss: -(shares as f64) * sl_dist,
            exp_pnl: shares as f64 * sl_dist * 0.2,
            expectancy_r: 0.2,
            shrunk_expectancy_r: 0.15,
            win_pct: 55.0,
            profit_factor: 1.6,
            n: 100,
            robustness: Robustness::default(),
            score,
            obi: 0.0,
            rvol: 1.0,
            vwap_dev_pct: 0.0,
            rsi: 50.0,
            note: String::new(),
        }
    }

    #[test]
    fn selects_by_score_and_caps_at_leverage() {
        // budget 100k, 1% risk = 1000/trade. max_notional = 500k.
        let s = UserSettings::new(100_000.0, 0.01);
        // Three ideas, each notional 200k ⇒ TWO fit (400k ≤ 500k), the third
        // (600k) breaches the leverage cap.
        let buy = vec![
            sig("AAA", "BUY", 3.0, 1000.0, 200, 3.3),
            sig("BBB", "BUY", 2.0, 1000.0, 200, 3.3),
            sig("CCC", "BUY", 1.0, 1000.0, 200, 3.3),
        ];
        let plan = build_plan(&buy, &[], &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions.len(), 2, "two fit, third over leverage");
        assert_eq!(plan.positions[0].symbol, "AAA", "highest score first");
        assert_eq!(plan.skipped_leverage, 1);
        assert!(plan.totals.deployed <= s.max_notional() + 1e-6);
        assert!(plan.totals.total_risk_inr > 0.0);
    }

    #[test]
    fn concurrency_cap_is_terminal() {
        let s = UserSettings::new(10_000_000.0, 0.01); // huge budget ⇒ leverage never binds
        // 7 small ideas; cap is PLAN_MAX_CONCURRENT (5).
        let buy: Vec<RankedSignal> = (0..7)
            .map(|i| sig(&format!("S{i}"), "BUY", 10.0 - i as f64, 100.0, 10, 1.0))
            .collect();
        let plan = build_plan(&buy, &[], &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions.len(), config::PLAN_MAX_CONCURRENT);
        assert_eq!(plan.skipped_concurrent, 7 - config::PLAN_MAX_CONCURRENT);
    }

    #[test]
    fn sector_cap_limits_per_sector() {
        let s = UserSettings::new(10_000_000.0, 0.02);
        let buy: Vec<RankedSignal> = (0..4)
            .map(|i| sig(&format!("B{i}"), "BUY", 10.0 - i as f64, 100.0, 10, 1.0))
            .collect();
        let mut sectors = HashMap::new();
        for i in 0..4 {
            sectors.insert(format!("B{i}"), "Banking".to_string());
        }
        let plan = build_plan(&buy, &[], &s, &sectors, &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions.len(), config::PLAN_MAX_PER_SECTOR, "≤2 per sector");
        assert_eq!(plan.skipped_sector, 4 - config::PLAN_MAX_PER_SECTOR);
    }

    #[test]
    fn portfolio_risk_ceiling_caps_total() {
        // budget 100k, 6% cap = 6000. Each trade risks 2000 (2% × 100k) ⇒ 3 fit.
        let s = UserSettings::new(100_000.0, 0.02);
        let buy: Vec<RankedSignal> = (0..5)
            .map(|i| sig(&format!("R{i}"), "BUY", 10.0 - i as f64, 10.0, 1000, 2.0)) // risk=1000*2=2000, notional=10k
            .collect();
        let plan = build_plan(&buy, &[], &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        // risk_cap = max(2000, 6000) = 6000 ⇒ 3 trades (6000), 4th skipped-risk.
        assert_eq!(plan.positions.len(), 3);
        assert_eq!(plan.skipped_risk_cap, 2);
        assert!(plan.totals.total_risk_inr <= 6000.0 + 1e-6);
    }

    #[test]
    fn liquidity_flags_oversized_orders() {
        let s = UserSettings::new(10_000_000.0, 0.01);
        // One name with 1000 planned shares; ADV says only 5,000/day trade ⇒ 20%
        // participation ⇒ "illiquid"; max safe = floor(1% × 5000) = 50.
        let buy = vec![sig("THIN", "BUY", 5.0, 100.0, 1000, 1.0)];
        let mut adv = HashMap::new();
        adv.insert("THIN".to_string(), 5_000.0);
        let plan = build_plan(&buy, &[], &s, &HashMap::new(), &adv, &HashMap::new());
        let p = &plan.positions[0];
        assert!((p.participation_pct - 20.0).abs() < 1e-6);
        assert_eq!(p.liquidity, "illiquid");
        assert_eq!(p.max_safe_qty, 50);
        assert_eq!(plan.totals.n_illiquid, 1);
        assert!(plan.notes.iter().any(|n| n.contains("safe fill size")));
    }

    #[test]
    fn liquidity_unknown_when_no_adv() {
        let s = UserSettings::new(10_000_000.0, 0.01);
        let buy = vec![sig("X", "BUY", 5.0, 100.0, 10, 1.0)];
        let plan = build_plan(&buy, &[], &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions[0].liquidity, "unknown");
        assert_eq!(plan.totals.n_illiquid, 0);
    }

    #[test]
    fn empty_signals_give_empty_plan() {
        let s = UserSettings::new(500_000.0, 0.01);
        let plan = build_plan(&[], &[], &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions.len(), 0);
        assert!(plan.notes[0].contains("No tradable basket"));
    }

    #[test]
    fn mixes_long_and_short_by_score() {
        let s = UserSettings::new(10_000_000.0, 0.01);
        let buy = vec![sig("L1", "BUY", 5.0, 100.0, 10, 1.0)];
        let sell = vec![sig("S1", "SELL", 9.0, 100.0, 10, 1.0)];
        let plan = build_plan(&buy, &sell, &s, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(plan.positions[0].symbol, "S1", "higher-score short ranks first");
        assert_eq!(plan.totals.n_long, 1);
        assert_eq!(plan.totals.n_short, 1);
    }
}

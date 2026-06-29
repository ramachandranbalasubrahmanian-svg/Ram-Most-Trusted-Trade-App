//! Position sizing & projected P&L (advisory only — never places orders).
//!
//! `risk_amount = budget × risk%`; `SL_dist = k·ATR`;
//! `shares = floor(risk_amount / SL_dist)` capped by `budget × leverage`.
//! Emits SL, target, projected profit (target hit), projected loss (SL hit), and
//! an expectancy-weighted P&L estimate. Enforces max-concurrent and sector
//! correlation limits; the 15:15 IST square-off is an ALERT, not an order.
//!
//! Everything here is display-only: nothing in this module is ever sent to a
//! broker. Projected P&L are scenarios net of cost, never promises.

use chrono::DateTime;
use chrono_tz::Tz;

use crate::config::{self, Direction, UserSettings};
use crate::types::{Alert, Candidate, RankedSignal, RiskMeter, Sizing};

/// Portfolio-level guardrails applied while ranking.
pub struct RiskLimits {
    pub max_concurrent: usize,
    pub max_per_sector: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        RiskLimits {
            max_concurrent: 5,
            max_per_sector: 2,
        }
    }
}

/// Size a single position and project its P&L under the current settings.
///
/// Sizing is risk-first: `shares = floor(risk_amount / (k·ATR))`, then capped by
/// the leverage headroom `floor(max_notional / entry)`. A degenerate input
/// (non-positive entry or stop distance) yields a zero-share `Sizing` that still
/// carries the entry so the UI can show context.
pub fn size(
    settings: &UserSettings,
    entry: f64,
    atr: f64,
    dir: Direction,
    expectancy_r: f64,
) -> Sizing {
    let risk_amount = settings.risk_amount();
    let k = config::SL_ATR_MULT;
    let rr = config::DEFAULT_RR;
    let sl_dist = k * atr;

    // Degenerate inputs: nothing tradable, but echo the entry for context.
    if sl_dist <= 0.0 || entry <= 0.0 {
        return Sizing {
            entry,
            ..Default::default()
        };
    }

    // Risk-first share count, then cap by leverage headroom.
    let mut shares = (risk_amount / sl_dist).floor() as i64;
    let lev_cap = (settings.max_notional() / entry).floor() as i64;
    if lev_cap < shares {
        shares = lev_cap;
    }

    let sign = dir.sign();
    let sl = entry - sign * sl_dist;
    let target = entry + sign * rr * sl_dist;

    // Zero (or negative) shares: still fill entry/sl/target, leave P&L at 0.
    if shares <= 0 {
        return Sizing {
            shares: 0,
            entry,
            sl,
            target,
            risk_per_share: sl_dist,
            notional: 0.0,
            proj_profit: 0.0,
            proj_loss: 0.0,
            exp_pnl: 0.0,
        };
    }

    let shares_f = shares as f64;
    // sign·(target-entry) = +rr·sl_dist (>=0); sign·(sl-entry) = -sl_dist (<=0).
    let proj_profit = shares_f * sign * (target - entry);
    let proj_loss = shares_f * sign * (sl - entry);
    let exp_pnl = shares_f * sl_dist * expectancy_r;
    let notional = shares_f * entry;

    Sizing {
        shares,
        entry,
        sl,
        target,
        risk_per_share: sl_dist,
        notional,
        proj_profit,
        proj_loss,
        exp_pnl,
    }
}

/// Build the honest caveat string for a row: short, comma-joined flags.
fn build_note(c: &Candidate) -> String {
    let mut flags: Vec<String> = Vec::new();
    if c.n < 50 {
        flags.push("low n".to_string());
    }
    if c.features.spread_pct > 0.2 {
        flags.push("wide spread".to_string());
    }
    if c.features.obi.abs() < 0.05 {
        flags.push("thin OBI".to_string());
    }
    // Display-only robustness caveats from the edge map (never gate the row). We
    // flag the two ACTIONABLE ones — a non-positive out-of-sample tail (the edge
    // fails when held out) and weak walk-forward consistency. DSR is shown in the
    // row annotation but not flagged here: deflated against a symbol's 26 sibling
    // trials it is near-zero for almost every edge-map edge, so a flag would be
    // pure noise rather than a signal.
    let rob = &c.robustness;
    if let Some(oos) = rob.oos_expectancy {
        if oos <= 0.0 {
            flags.push(format!("OOS {oos:+.2}R (fails out-of-sample)"));
        }
    }
    if rob.oos_n > 0 && rob.wf_consistency < 0.5 {
        flags.push(format!("WF {:.0}% (inconsistent)", rob.wf_consistency * 100.0));
    }
    flags.join(", ")
}

/// Convert a sized candidate into a fully-formed ranked row.
fn to_ranked(c: &Candidate, sizing: &Sizing) -> RankedSignal {
    RankedSignal {
        symbol: c.symbol.clone(),
        strategy: c.strategy.clone(),
        side: c.direction.as_str().to_string(),
        entry: sizing.entry,
        sl: sizing.sl,
        target: sizing.target,
        shares: sizing.shares,
        notional: sizing.notional,
        proj_profit: sizing.proj_profit,
        proj_loss: sizing.proj_loss,
        exp_pnl: sizing.exp_pnl,
        expectancy_r: c.expectancy_r,
        shrunk_expectancy_r: c.shrunk_expectancy_r,
        win_pct: c.win_pct,
        profit_factor: c.profit_factor,
        n: c.n,
        robustness: c.robustness.clone(),
        score: c.score,
        obi: c.features.obi,
        rvol: c.features.rvol,
        vwap_dev_pct: c.features.vwap_dev_pct,
        rsi: c.features.rsi,
        block_mult: c.features.block_mult,
        tick_sweep: c.features.tick_sweep,
        spread_widening: c.features.spread_widening,
        adv: 0.0, // annotated post-rank from the ADV map (for the liquidity filter)
        note: build_note(c),
    }
}

/// Rank candidates into Top-N Buy / Top-N Sell, sized and guardrailed.
///
/// One signal survives per `(symbol, side)` — the highest-scoring strategy.
/// Zero-share rows are dropped. Each side is sorted by score descending and
/// truncated to `config::TOP_N`, independently — both lists are shown in full.
/// `limits.max_concurrent` / `max_per_sector` are portfolio guardrails surfaced
/// as advisories elsewhere; they do NOT prune these display lists.
pub fn rank(
    candidates: &[Candidate],
    settings: &UserSettings,
    limits: &RiskLimits,
) -> (Vec<RankedSignal>, Vec<RankedSignal>) {
    // Collapse to one best row per (symbol, side), dropping zero-share rows.
    // best[i] holds the chosen ranked signal; we track score for comparison.
    let mut best: Vec<RankedSignal> = Vec::new();

    for c in candidates {
        let sizing = size(settings, c.last_price, c.atr, c.direction, c.expectancy_r);
        if sizing.shares == 0 {
            continue;
        }
        let side = c.direction.as_str();
        match best
            .iter_mut()
            .find(|r| r.symbol == c.symbol && r.side == side)
        {
            Some(existing) => {
                if c.score > existing.score {
                    *existing = to_ranked(c, &sizing);
                }
            }
            None => best.push(to_ranked(c, &sizing)),
        }
    }

    // NOTE: `max_concurrent` is a PORTFOLIO guardrail (how many positions you
    // would actually hold at once), surfaced as an advisory in the risk meter —
    // it must NOT prune the displayed idea lists. The user wants a full Top-10
    // per side shown, so both lists stay independent and complete here.
    let _ = limits;

    // Split by side, sort each by score desc, truncate to TOP_N.
    let mut buy: Vec<RankedSignal> = best
        .iter()
        .filter(|r| r.side == Direction::Long.as_str())
        .cloned()
        .collect();
    let mut sell: Vec<RankedSignal> = best
        .iter()
        .filter(|r| r.side == Direction::Short.as_str())
        .cloned()
        .collect();

    let by_score_desc = |a: &RankedSignal, b: &RankedSignal| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
    };
    buy.sort_by(by_score_desc);
    sell.sort_by(by_score_desc);

    buy.truncate(config::TOP_N);
    sell.truncate(config::TOP_N);

    (buy, sell)
}

/// Compute the exposure gauge from the ranked lists.
pub fn risk_meter(
    buy: &[RankedSignal],
    sell: &[RankedSignal],
    settings: &UserSettings,
) -> RiskMeter {
    let deployed_notional: f64 = buy
        .iter()
        .chain(sell.iter())
        .map(|r| r.notional)
        .sum();

    let max_notional = settings.max_notional();
    let exposure_pct = if max_notional > 0.0 {
        deployed_notional / max_notional * 100.0
    } else {
        0.0
    };
    let free_margin = max_notional - deployed_notional;

    let color = if exposure_pct < 60.0 {
        "green"
    } else if exposure_pct < 90.0 {
        "amber"
    } else {
        "red"
    }
    .to_string();

    RiskMeter {
        budget: settings.budget,
        max_notional,
        deployed_notional,
        free_margin,
        exposure_pct,
        color,
    }
}

/// 15:15 IST square-off reminder (alert only — never an order).
///
/// `[15:15, 15:20)` → danger ("exit now"); `[15:00, 15:15)` → warn (heads-up);
/// otherwise `None`.
pub fn squareoff_alert(now_ist: DateTime<Tz>) -> Option<Alert> {
    let now = now_ist.time();
    let squareoff = config::squareoff_alert(); // 15:15
    let _close = config::session_close(); // 15:30 (reference)

    // Boundaries derived from the square-off alert time.
    let warn_start = chrono::NaiveTime::from_hms_opt(15, 0, 0).unwrap();
    let danger_end = chrono::NaiveTime::from_hms_opt(15, 20, 0).unwrap();

    if now >= squareoff && now < danger_end {
        Some(Alert {
            kind: "squareoff".to_string(),
            severity: "danger".to_string(),
            message:
                "MIS square-off window — exit intraday positions by ~15:20 IST (this is an alert, not an order)."
                    .to_string(),
        })
    } else if now >= warn_start && now < squareoff {
        Some(Alert {
            kind: "squareoff".to_string(),
            severity: "warn".to_string(),
            message:
                "Heads-up: MIS square-off approaches at 15:15 IST — plan your exits (alert only, no orders placed)."
                    .to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UserSettings;

    #[test]
    fn long_size_arithmetic() {
        // budget 500k, 2% risk => risk_amount 10,000.
        let settings = UserSettings::new(500_000.0, 0.02);
        let entry = 1000.0;
        let atr = 10.0;
        let expectancy_r = 0.3;

        let s = size(&settings, entry, atr, Direction::Long, expectancy_r);

        // sl_dist = k·atr = 1.5·10 = 15. shares = floor(10000/15) = 666.
        // leverage cap = floor(2_500_000 / 1000) = 2500 -> not binding.
        let sl_dist = config::SL_ATR_MULT * atr; // 15.0
        assert_eq!(s.shares, 666);
        assert_eq!(s.risk_per_share, sl_dist);

        // sl = entry - 1·sl_dist = 985; target = entry + rr·sl_dist = 1030.
        assert!((s.sl - 985.0).abs() < 1e-9);
        assert!((s.target - 1030.0).abs() < 1e-9);

        // proj_profit = 666 · (+1) · (1030 - 1000) = 666·30 = 19980.
        assert!((s.proj_profit - 19_980.0).abs() < 1e-6);
        // proj_loss = 666 · (+1) · (985 - 1000) = 666·(-15) = -9990 (<=0).
        assert!((s.proj_loss + 9_990.0).abs() < 1e-6);
        assert!(s.proj_loss <= 0.0);
        // exp_pnl = 666 · 15 · 0.3 = 2997.
        assert!((s.exp_pnl - 2_997.0).abs() < 1e-6);
        // notional = 666 · 1000 = 666000.
        assert!((s.notional - 666_000.0).abs() < 1e-6);
    }

    #[test]
    fn short_size_sign_check() {
        let settings = UserSettings::new(500_000.0, 0.02);
        let entry = 1000.0;
        let atr = 10.0;
        let s = size(&settings, entry, atr, Direction::Short, 0.3);

        let sl_dist = config::SL_ATR_MULT * atr; // 15.0
        // Short: sl above entry, target below.
        assert!((s.sl - (entry + sl_dist)).abs() < 1e-9); // 1015
        assert!((s.target - (entry - config::DEFAULT_RR * sl_dist)).abs() < 1e-9); // 970

        // Projected profit must still be >= 0 and loss <= 0 regardless of side.
        assert!(s.proj_profit >= 0.0);
        assert!(s.proj_loss <= 0.0);
        assert!((s.proj_profit - 666.0 * config::DEFAULT_RR * sl_dist).abs() < 1e-6);
        assert!((s.proj_loss + 666.0 * sl_dist).abs() < 1e-6);
    }

    #[test]
    fn degenerate_inputs_yield_zero_shares() {
        let settings = UserSettings::default();
        // zero atr -> zero stop distance.
        let s = size(&settings, 1000.0, 0.0, Direction::Long, 0.3);
        assert_eq!(s.shares, 0);
        assert_eq!(s.entry, 1000.0);
        // zero entry.
        let s2 = size(&settings, 0.0, 10.0, Direction::Long, 0.3);
        assert_eq!(s2.shares, 0);
    }

    #[test]
    fn leverage_cap_binds_on_expensive_name() {
        // Small budget, expensive share: leverage headroom binds before risk.
        let settings = UserSettings::new(50_000.0, 0.07); // risk_amount = 3500
        let entry = 60_000.0; // very expensive
        let atr = 10.0;
        let s = size(&settings, entry, atr, Direction::Long, 0.1);
        // risk-first shares = floor(3500 / 15) = 233.
        // lev cap = floor(250_000 / 60_000) = 4 -> binds.
        assert_eq!(s.shares, 4);
        assert!((s.notional - 4.0 * entry).abs() < 1e-6);
    }

    #[test]
    fn risk_meter_color_bands() {
        let settings = UserSettings::new(500_000.0, 0.01); // max_notional 2.5M
        let mk = |notional: f64| RankedSignal {
            symbol: "X".into(),
            strategy: "s".into(),
            side: "BUY".into(),
            entry: 100.0,
            sl: 90.0,
            target: 120.0,
            shares: 1,
            notional,
            proj_profit: 0.0,
            proj_loss: 0.0,
            exp_pnl: 0.0,
            expectancy_r: 0.0,
            shrunk_expectancy_r: 0.0,
            win_pct: 0.0,
            profit_factor: 0.0,
            robustness: Default::default(),
            n: 100,
            score: 1.0,
            obi: 0.0,
            rvol: 0.0,
            vwap_dev_pct: 0.0,
            rsi: 50.0,
            block_mult: 0.0,
            tick_sweep: 0,
            spread_widening: false,
            adv: 0.0,
            note: String::new(),
        };
        // 50% -> green.
        let m = risk_meter(&[mk(1_250_000.0)], &[], &settings);
        assert_eq!(m.color, "green");
        assert!((m.exposure_pct - 50.0).abs() < 1e-9);
        // 75% -> amber.
        let m = risk_meter(&[mk(1_875_000.0)], &[], &settings);
        assert_eq!(m.color, "amber");
        // 95% -> red.
        let m = risk_meter(&[mk(2_375_000.0)], &[], &settings);
        assert_eq!(m.color, "red");
        assert!((m.free_margin - (2_500_000.0 - 2_375_000.0)).abs() < 1e-6);
    }

    #[test]
    fn squareoff_alert_windows() {
        use chrono::TimeZone;
        let mk = |h: u32, m: u32| config::IST.with_ymd_and_hms(2026, 6, 27, h, m, 0).unwrap();

        // Before 15:00 -> None.
        assert!(squareoff_alert(mk(14, 59)).is_none());
        // [15:00, 15:15) -> warn.
        let a = squareoff_alert(mk(15, 5)).unwrap();
        assert_eq!(a.severity, "warn");
        assert_eq!(a.kind, "squareoff");
        // [15:15, 15:20) -> danger.
        let a = squareoff_alert(mk(15, 17)).unwrap();
        assert_eq!(a.severity, "danger");
        // >= 15:20 -> None.
        assert!(squareoff_alert(mk(15, 25)).is_none());
    }

    #[test]
    fn rank_dedups_and_truncates() {
        let settings = UserSettings::default();
        let limits = RiskLimits {
            max_concurrent: 100,
            max_per_sector: 2,
        };
        let mk = |sym: &str, dir: Direction, score: f64| Candidate {
            symbol: sym.to_string(),
            strategy: format!("strat-{score}"),
            direction: dir,
            expectancy_r: 0.3,
            shrunk_expectancy_r: 0.3,
            profit_factor: 1.5,
            win_pct: 50.0,
            n: 100,
            robustness: Default::default(),
            last_price: 1000.0,
            atr: 10.0,
            features: crate::types::LiveFeatures::default(),
            live_score: 1.0,
            score,
        };
        // Two strategies on same (symbol, side): the higher score must win.
        let cands = vec![
            mk("AAA", Direction::Long, 1.0),
            mk("AAA", Direction::Long, 5.0),
            mk("BBB", Direction::Short, 2.0),
        ];
        let (buy, sell) = rank(&cands, &settings, &limits);
        assert_eq!(buy.len(), 1);
        assert_eq!(buy[0].symbol, "AAA");
        assert!((buy[0].score - 5.0).abs() < 1e-9);
        assert_eq!(sell.len(), 1);
        assert_eq!(sell[0].symbol, "BBB");
    }
}

//! Session governor — the behavioral kill-switch. FIREWALLED, PURE, advisory.
//!
//! At 5× MIS on a thin edge the account-killer is almost never one bad signal —
//! it's tilt: revenge-trading a red morning into a 15% day. `circuit_breaker.rs`
//! already auto-freezes on the daily *loss* cap; this adds the two missing
//! behavioral dimensions — **trades/day** and **consecutive-loss cool-off** — and
//! rolls all three into one honest desk verdict: open / caution / closed.
//!
//! Reads today's journal only. It NEVER places, modifies, or cancels an order, and
//! NEVER feeds Confidence/scoring/ranking/the edge map. "Desk closed" is a strong
//! recommendation the trader (and the UI banner) can act on, not an auto-exec.

use serde::Serialize;

use crate::types::{JournalEntry, SignalState};

/// The three session limits. Defaults live in `config`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GovernorLimits {
    pub max_trades: usize,
    pub max_consecutive_losses: usize,
    /// Daily loss cap as a POSITIVE fraction of capital (e.g. 0.02 = 2%).
    pub daily_loss_cap_pct: f64,
}

/// The governor's read on today's session.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GovernorState {
    pub trades_today: usize,
    pub wins_today: usize,
    pub losses_today: usize,
    /// Trailing run of losses among today's closed trades (chronological).
    pub consecutive_losses: usize,
    /// Today's realized ₹ P&L over tracked (manually-accepted) closed trades.
    pub daily_pnl: f64,
    /// The loss cap in ₹ (negative).
    pub daily_loss_cap: f64,
    /// "open" | "caution" | "closed".
    pub verdict: String,
    /// Human-readable reasons (what tripped, or "within limits").
    pub reasons: Vec<String>,
    pub limits: GovernorLimits,
}

fn date10(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|v| v.chars().take(10).collect::<String>()).filter(|d| d.len() == 10)
}

/// Evaluate the session governor from the journal. `today` is the IST date
/// ("YYYY-MM-DD"); a trade counts toward today if it was opened or closed today.
/// Pure — no I/O.
pub fn evaluate(
    entries: &[JournalEntry],
    capital: f64,
    today: &str,
    limits: &GovernorLimits,
) -> GovernorState {
    let today10: String = today.chars().take(10).collect();

    // Today's TRACKED (manually-accepted) trades, chronological by id.
    let mut todays: Vec<&JournalEntry> = entries
        .iter()
        .filter(|e| SignalState::from_str(&e.state) == SignalState::ManuallyAccepted)
        .filter(|e| {
            date10(&e.entry_ist).as_deref() == Some(&today10)
                || date10(&e.exit_ist).as_deref() == Some(&today10)
        })
        .collect();
    todays.sort_by_key(|e| e.id);

    let trades_today = todays.len();
    let closed: Vec<&&JournalEntry> = todays.iter().filter(|e| e.pnl.is_some()).collect();
    let wins_today = closed.iter().filter(|e| e.pnl.unwrap_or(0.0) > 0.0).count();
    let losses_today = closed.iter().filter(|e| e.pnl.unwrap_or(0.0) < 0.0).count();
    let daily_pnl: f64 = closed.iter().map(|e| e.pnl.unwrap_or(0.0)).sum();

    // Trailing consecutive losses among today's CLOSED trades (chronological).
    let mut consecutive_losses = 0usize;
    for e in closed.iter().rev() {
        if e.pnl.unwrap_or(0.0) < 0.0 {
            consecutive_losses += 1;
        } else {
            break;
        }
    }

    let daily_loss_cap = -(capital.max(0.0) * limits.daily_loss_cap_pct.max(0.0));

    // Evaluate the three limits into a single verdict. "closed" wins over
    // "caution" wins over "open". Each triggered limit contributes a reason.
    let mut reasons: Vec<String> = Vec::new();
    let mut closed_flag = false;
    let mut caution_flag = false;

    // 1) daily loss cap
    if daily_pnl <= daily_loss_cap && daily_loss_cap < 0.0 {
        closed_flag = true;
        reasons.push(format!(
            "Daily loss cap hit: {daily_pnl:.0} ≤ {daily_loss_cap:.0} ({:.0}% of capital). Stop for the day.",
            limits.daily_loss_cap_pct * 100.0
        ));
    } else if daily_loss_cap < 0.0 && daily_pnl <= 0.7 * daily_loss_cap {
        caution_flag = true;
        reasons.push(format!(
            "Approaching the daily loss cap ({daily_pnl:.0} vs {daily_loss_cap:.0}) — trade smaller / be selective."
        ));
    }

    // 2) trades-per-day
    if limits.max_trades > 0 && trades_today >= limits.max_trades {
        closed_flag = true;
        reasons.push(format!(
            "Trade count {trades_today} ≥ max {} for the day — over-trading risk; desk closed.",
            limits.max_trades
        ));
    } else if limits.max_trades > 0 && trades_today + 1 >= limits.max_trades {
        caution_flag = true;
        reasons.push(format!(
            "One trade from the daily max ({trades_today}/{}) — make it count.",
            limits.max_trades
        ));
    }

    // 3) consecutive-loss cool-off
    if limits.max_consecutive_losses > 0 && consecutive_losses >= limits.max_consecutive_losses {
        closed_flag = true;
        reasons.push(format!(
            "{consecutive_losses} losses in a row (≥ {}) — cool off; step away before revenge-trading.",
            limits.max_consecutive_losses
        ));
    } else if limits.max_consecutive_losses > 0 && consecutive_losses + 1 >= limits.max_consecutive_losses && consecutive_losses > 0 {
        caution_flag = true;
        reasons.push(format!(
            "{consecutive_losses} losses in a row — one more triggers a cool-off."
        ));
    }

    let verdict = if closed_flag {
        "closed"
    } else if caution_flag {
        "caution"
    } else {
        reasons.push("Within limits — trade your plan.".to_string());
        "open"
    }
    .to_string();

    GovernorState {
        trades_today,
        wins_today,
        losses_today,
        consecutive_losses,
        daily_pnl,
        daily_loss_cap,
        verdict,
        reasons,
        limits: limits.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(id: i64, date: &str, pnl: Option<f64>) -> JournalEntry {
        JournalEntry {
            id,
            generated_ist: format!("{date} 10:00:00"),
            entry_ist: Some(format!("{date} 10:00:00")),
            exit_ist: Some(format!("{date} 12:00:00")),
            instrument_token: 0,
            symbol: "X".into(),
            direction: "BUY".into(),
            strategy: "Imported".into(),
            alpha_trigger: String::new(),
            intended_price: 100.0,
            actual_fill_price: Some(100.0),
            exit_price: Some(100.0),
            qty: 1,
            state: "Manually_Accepted".into(),
            pnl,
            slippage: None,
            sector: None,
        }
    }

    fn limits() -> GovernorLimits {
        GovernorLimits { max_trades: 5, max_consecutive_losses: 3, daily_loss_cap_pct: 0.02 }
    }

    #[test]
    fn open_when_within_all_limits() {
        let es = vec![trade(1, "2026-07-01", Some(500.0)), trade(2, "2026-07-01", Some(-100.0))];
        let g = evaluate(&es, 1_000_000.0, "2026-07-01", &limits());
        assert_eq!(g.verdict, "open");
        assert_eq!(g.trades_today, 2);
        assert_eq!(g.consecutive_losses, 1);
    }

    #[test]
    fn closes_on_daily_loss_cap() {
        // Capital 5L, cap 2% = -10,000. A -12,000 day → closed.
        let es = vec![trade(1, "2026-07-01", Some(-12_000.0))];
        let g = evaluate(&es, 500_000.0, "2026-07-01", &limits());
        assert_eq!(g.verdict, "closed");
        assert!(g.reasons.iter().any(|r| r.contains("Daily loss cap")));
    }

    #[test]
    fn closes_on_trade_count() {
        let es: Vec<_> = (1..=5).map(|i| trade(i, "2026-07-01", Some(10.0))).collect();
        let g = evaluate(&es, 1_000_000.0, "2026-07-01", &limits());
        assert_eq!(g.verdict, "closed");
        assert_eq!(g.trades_today, 5);
    }

    #[test]
    fn closes_on_consecutive_losses() {
        let es = vec![
            trade(1, "2026-07-01", Some(50.0)),
            trade(2, "2026-07-01", Some(-10.0)),
            trade(3, "2026-07-01", Some(-10.0)),
            trade(4, "2026-07-01", Some(-10.0)),
        ];
        let g = evaluate(&es, 1_000_000.0, "2026-07-01", &limits());
        assert_eq!(g.consecutive_losses, 3);
        assert_eq!(g.verdict, "closed");
    }

    #[test]
    fn a_win_resets_the_streak() {
        let es = vec![
            trade(1, "2026-07-01", Some(-10.0)),
            trade(2, "2026-07-01", Some(-10.0)),
            trade(3, "2026-07-01", Some(50.0)), // win breaks the run
        ];
        let g = evaluate(&es, 1_000_000.0, "2026-07-01", &limits());
        assert_eq!(g.consecutive_losses, 0);
        assert_ne!(g.verdict, "closed");
    }

    #[test]
    fn yesterdays_trades_are_ignored() {
        let es = vec![trade(1, "2026-06-30", Some(-99_000.0))];
        let g = evaluate(&es, 500_000.0, "2026-07-01", &limits());
        assert_eq!(g.trades_today, 0);
        assert_eq!(g.verdict, "open");
    }
}

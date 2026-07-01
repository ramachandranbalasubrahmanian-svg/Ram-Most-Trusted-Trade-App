//! Exit-Reachability Guard — FIREWALLED, PURE, display/advisory only.
//!
//! Before a position is sized or staged, verify the protective stop can actually be
//! FILLED, not merely placed:
//!   • the stop must sit INSIDE the live circuit band — a stop beyond the band never
//!     fills, because when price reaches the circuit the stock LOCKS and you're
//!     trapped in an unbounded loss;
//!   • the name must accept intraday / MIS (not T2T / ASM / GSM surveillance);
//!   • the exit size must be small vs the name's daily turnover, else it can't clear
//!     in one bar and the "stop" is a fantasy.
//!
//! It NEVER places, modifies, or cancels an order, and NEVER feeds Confidence,
//! scoring, ranking, or the edge map. It emits edges + statistics + caveats, never a
//! naked "BUY/SELL X" — the trigger-pull stays with the human. All inputs are
//! primitives so the logic is fully unit-testable with zero I/O; the caller supplies
//! `None` for any live value it couldn't fetch, and the guard degrades to an honest
//! "verify" rather than a false all-clear.

use serde::Serialize;

/// The guard's verdict on a proposed position.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ExitVerdict {
    /// "blocked" | "unreachable_stop" | "caution" | "unknown" | "ok".
    pub verdict: String,
    /// "info" | "warn" | "danger" — for UI colouring / alert routing.
    pub severity: String,
    /// Human-readable reason — the stay-away / verify / all-clear message.
    pub reason: String,
    /// Distance entry → adverse circuit band, as % of entry (None if band unknown).
    pub circuit_room_pct: Option<f64>,
    /// true = stop is inside the band (fillable); false = beyond it (unfillable);
    /// None = no live circuit data to judge.
    pub stop_inside_band: Option<bool>,
    /// Exit notional as % of the name's daily turnover (None if turnover unknown).
    pub participation_pct: Option<f64>,
}

/// Assess whether the protective stop for a proposed position can actually be filled.
///
/// * `side` — "BUY"/"SELL" (anything not "SELL" is treated as a long).
/// * `entry`, `stop_loss` — planned prices (₹/share).
/// * `notional` — order value in ₹ (qty × entry), for the participation check.
/// * `lower_circuit` / `upper_circuit` — today's band from a LIVE quote; `None` when
///   no live quote is available.
/// * `tradability_verdict` — the engine's tradability verdict for this name:
///   "blocked" (T2T / ASM / GSM ⇒ MIS may be rejected) | "high_risk" | "caution" |
///   "ok"; `None` when unknown. `tradability_reason` is its one-line explanation.
/// * `turnover_inr` — the name's median daily turnover in ₹; `None` if unknown.
/// * `participation_cap_pct` — max exit as % of daily turnover before it's a caution.
///
/// Verdict precedence (highest-consequence first): blocked → unreachable_stop →
/// caution (participation) → caution (elevated-risk name) → unknown (no circuit
/// data) → ok. A name the engine flags high_risk/caution NEVER returns a clean "ok".
#[allow(clippy::too_many_arguments)]
pub fn assess_exit(
    side: &str,
    entry: f64,
    stop_loss: f64,
    notional: f64,
    lower_circuit: Option<f64>,
    upper_circuit: Option<f64>,
    tradability_verdict: Option<&str>,
    tradability_reason: Option<&str>,
    turnover_inr: Option<f64>,
    participation_cap_pct: f64,
) -> ExitVerdict {
    let is_long = !side.eq_ignore_ascii_case("SELL");

    // Participation is independent of the circuit check; it may attach as a caution.
    let participation_pct = match turnover_inr {
        Some(t) if t > 0.0 && notional > 0.0 => Some(notional / t * 100.0),
        _ => None,
    };

    // Adverse band = the one the stop is protecting against: lower for a long,
    // upper for a short. Reachability = is the stop on the tradeable side of it.
    let adverse_band = if is_long { lower_circuit } else { upper_circuit };
    let (circuit_room_pct, stop_inside_band) = match adverse_band {
        Some(band) if band > 0.0 && entry > 0.0 => {
            let room = (entry - band).abs() / entry * 100.0;
            let inside = if is_long { stop_loss > band } else { stop_loss < band };
            (Some(room), Some(inside))
        }
        _ => (None, None),
    };

    let mk = |verdict: &str, severity: &str, reason: String| ExitVerdict {
        verdict: verdict.to_string(),
        severity: severity.to_string(),
        reason,
        circuit_room_pct,
        stop_inside_band,
        participation_pct,
    };

    // 1) Hard block — intraday/MIS not allowed on this name (T2T / surveillance).
    if tradability_verdict == Some("blocked") {
        return mk(
            "blocked",
            "danger",
            tradability_reason.map(str::to_string).unwrap_or_else(|| {
                "Intraday/MIS may be rejected (T2T / ASM / GSM surveillance) — verify on NSE before sizing; this may be delivery-only.".to_string()
            }),
        );
    }

    // 2) Unreachable stop — the protective stop sits at/beyond the circuit band, so
    //    a circuit-lock traps the position and the stop can never fill.
    if stop_inside_band == Some(false) {
        let band = adverse_band.unwrap_or(0.0);
        let side_word = if is_long { "lower" } else { "upper" };
        return mk(
            "unreachable_stop",
            "danger",
            format!(
                "Stop ₹{stop_loss:.2} is beyond the {side_word} circuit ₹{band:.2} — if price reaches the band the stock locks and your stop can't fill (unbounded loss). Move the stop inside the band or skip."
            ),
        );
    }

    // 3) Participation caution — the exit is large vs the name's daily turnover.
    if let Some(p) = participation_pct {
        if p > participation_cap_pct {
            return mk(
                "caution",
                "warn",
                format!(
                    "Your exit is {p:.1}% of the name's daily turnover (cap {participation_cap_pct:.1}%) — it may not clear in one bar; size down for a reliable exit."
                ),
            );
        }
    }

    // 3b) Elevated-risk name — the engine flags it high_risk/caution (very thin
    //     liquidity, micro-cap, low-priced) even though MIS is allowed. Never emit a
    //     clean "ok" over such a name; surface the tradability reason.
    if matches!(tradability_verdict, Some("high_risk") | Some("caution")) {
        return mk(
            "caution",
            "warn",
            tradability_reason.map(str::to_string).unwrap_or_else(|| {
                "The engine flags this name as elevated-risk (thin liquidity / micro-cap / low-priced) — intraday exits can be unreliable; size down and verify depth.".to_string()
            }),
        );
    }

    // 4) Unknown — no live circuit data, so reachability can't be confirmed.
    if stop_inside_band.is_none() {
        return mk(
            "unknown",
            "warn",
            "Live circuit limits unavailable (no live quote) — verify the stop sits inside today's circuit band before entry.".to_string(),
        );
    }

    // 5) All clear.
    mk(
        "ok",
        "info",
        "Stop sits inside the circuit band, the name is intraday-eligible, and the exit size is within daily turnover.".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // A comfortable long: entry 100, stop 98, band at 90 (stop well inside), thin-free.
    #[test]
    fn long_stop_inside_band_is_ok() {
        let v = assess_exit("BUY", 100.0, 98.0, 10_000.0, Some(90.0), Some(110.0), Some("ok"), None, Some(1_000_000.0), 1.0);
        assert_eq!(v.verdict, "ok");
        assert_eq!(v.stop_inside_band, Some(true));
        assert!(v.circuit_room_pct.is_some());
    }

    #[test]
    fn long_stop_beyond_lower_circuit_is_unreachable() {
        // Stop 88 is BELOW the lower circuit 90 → if it falls to 90 it locks first.
        let v = assess_exit("BUY", 100.0, 88.0, 10_000.0, Some(90.0), Some(110.0), Some("ok"), None, Some(1_000_000.0), 1.0);
        assert_eq!(v.verdict, "unreachable_stop");
        assert_eq!(v.severity, "danger");
        assert_eq!(v.stop_inside_band, Some(false));
    }

    #[test]
    fn short_stop_beyond_upper_circuit_is_unreachable() {
        // Short: stop 112 is ABOVE the upper circuit 110 → unreachable on a lock.
        let v = assess_exit("SELL", 100.0, 112.0, 10_000.0, Some(90.0), Some(110.0), Some("ok"), None, Some(1_000_000.0), 1.0);
        assert_eq!(v.verdict, "unreachable_stop");
        assert_eq!(v.stop_inside_band, Some(false));
    }

    #[test]
    fn blocked_takes_precedence_over_unreachable_stop() {
        // Not intraday-eligible AND an unreachable stop → block wins (most severe).
        let v = assess_exit("BUY", 100.0, 88.0, 10_000.0, Some(90.0), Some(110.0), Some("blocked"), Some("T2T — delivery only"), Some(1_000_000.0), 1.0);
        assert_eq!(v.verdict, "blocked");
        assert!(v.reason.contains("T2T"));
    }

    #[test]
    fn high_participation_is_a_caution() {
        // Exit ₹50k on a name doing ₹1L/day = 50% participation, cap 1%.
        let v = assess_exit("BUY", 100.0, 98.0, 50_000.0, Some(90.0), Some(110.0), Some("ok"), None, Some(100_000.0), 1.0);
        assert_eq!(v.verdict, "caution");
        assert!((v.participation_pct.unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn high_risk_name_never_returns_clean_ok() {
        // Stop inside band, tiny participation, live circuit present — but the engine
        // flags the name high_risk (very thin liquidity). Must downgrade ok → caution.
        let v = assess_exit("BUY", 100.0, 98.0, 5_000.0, Some(90.0), Some(110.0), Some("high_risk"), Some("Very thin liquidity — you may be unable to exit intraday."), Some(10_000_000.0), 1.0);
        assert_eq!(v.verdict, "caution");
        assert!(v.reason.contains("thin"));
    }

    #[test]
    fn unknown_when_no_live_circuit() {
        let v = assess_exit("BUY", 100.0, 98.0, 10_000.0, None, None, Some("ok"), None, Some(1_000_000.0), 1.0);
        assert_eq!(v.verdict, "unknown");
        assert_eq!(v.stop_inside_band, None);
    }

    #[test]
    fn unreachable_beats_participation() {
        // Both an unreachable stop and heavy participation → the stop (danger) wins.
        let v = assess_exit("BUY", 100.0, 88.0, 50_000.0, Some(90.0), Some(110.0), Some("ok"), None, Some(100_000.0), 1.0);
        assert_eq!(v.verdict, "unreachable_stop");
    }
}

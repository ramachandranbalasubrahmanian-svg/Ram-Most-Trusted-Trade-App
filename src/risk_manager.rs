//! Position sizing & projected P&L (advisory only — never places orders).
//!
//! `risk_amount = budget × risk%`; `SL_dist = k·ATR`;
//! `shares = floor(risk_amount / SL_dist)` capped by `budget × leverage`.
//! Emits SL, target, projected profit (target hit), projected loss (SL hit), and
//! an expectancy-weighted P&L estimate. Enforces max-concurrent and sector
//! correlation limits; the 15:15 IST square-off is an ALERT, not an order.
//!
//! CONTRACT STUB — public signatures are frozen; bodies are filled in Phase 3.

use chrono::DateTime;
use chrono_tz::Tz;

use crate::config::{Direction, UserSettings};
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
pub fn size(
    _settings: &UserSettings,
    entry: f64,
    _atr: f64,
    _dir: Direction,
    _expectancy_r: f64,
) -> Sizing {
    Sizing {
        entry,
        ..Default::default()
    }
}

/// Rank candidates into Top-N Buy / Top-N Sell, sized and guardrailed.
pub fn rank(
    _candidates: &[Candidate],
    _settings: &UserSettings,
    _limits: &RiskLimits,
) -> (Vec<RankedSignal>, Vec<RankedSignal>) {
    (Vec::new(), Vec::new())
}

/// Compute the exposure gauge from the ranked lists.
pub fn risk_meter(
    _buy: &[RankedSignal],
    _sell: &[RankedSignal],
    settings: &UserSettings,
) -> RiskMeter {
    RiskMeter {
        budget: settings.budget,
        max_notional: settings.max_notional(),
        free_margin: settings.max_notional(),
        color: "green".to_string(),
        ..Default::default()
    }
}

/// 15:15 IST square-off reminder (alert only — never an order).
pub fn squareoff_alert(_now_ist: DateTime<Tz>) -> Option<Alert> {
    None
}

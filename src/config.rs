//! System-wide configuration: data paths, dual parquet schemas, IST session
//! windows, budget/risk matrix, leverage cap, and the live sliding-window size.
//!
//! Pure constants + small helpers — no I/O, no heavy deps — so every other
//! module can depend on it freely.

use std::path::{Path, PathBuf};

use chrono::NaiveTime;
use chrono_tz::Asia::Kolkata;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Time / market session (all Asia/Kolkata, IST)
// ---------------------------------------------------------------------------

/// The one true timezone for every market operation in this system.
pub const IST: Tz = Kolkata;

/// Pre-market analysis window start (09:00 IST).
pub fn premarket_start() -> NaiveTime {
    NaiveTime::from_hms_opt(9, 0, 0).unwrap()
}
/// Pre-market analysis window end (09:08 IST).
pub fn premarket_end() -> NaiveTime {
    NaiveTime::from_hms_opt(9, 8, 0).unwrap()
}
/// Continuous session open (09:15 IST).
pub fn session_open() -> NaiveTime {
    NaiveTime::from_hms_opt(9, 15, 0).unwrap()
}
/// Continuous session close (15:30 IST).
pub fn session_close() -> NaiveTime {
    NaiveTime::from_hms_opt(15, 30, 0).unwrap()
}
/// Intraday (MIS) square-off ALERT time (15:15 IST). This system raises an
/// alert here — it never auto-fires orders.
pub fn squareoff_alert() -> NaiveTime {
    NaiveTime::from_hms_opt(15, 15, 0).unwrap()
}
/// Bars in a full NSE continuous session (09:15–15:29 inclusive = 375 minutes).
pub const BARS_PER_SESSION: usize = 375;

// ---------------------------------------------------------------------------
// Data archive layout
// ---------------------------------------------------------------------------

/// Env var that overrides the parquet archive root.
pub const DATA_ROOT_ENV: &str = "RAM_ISTP_DATA_ROOT";
/// Default archive root, relative to the project working directory.
pub const DEFAULT_DATA_ROOT: &str = "1500-Stocks-Parquest";

/// Resolve the parquet archive root (env override → default).
pub fn data_root() -> PathBuf {
    std::env::var(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DATA_ROOT))
}

/// The resolutions available in the archive. `Daily` is the Kite-sourced
/// `1day/` set (IST tz); `DailyLong` is the Yahoo `daily/` set (~30 yrs,
/// tz-naive, has an extra `adj close` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    Minute,
    Min3,
    Min5,
    Min10,
    Min15,
    Min30,
    Min60,
    Daily,
    DailyLong,
}

impl Timeframe {
    /// Sub-directory name within the archive root.
    pub fn dir(self) -> &'static str {
        match self {
            Timeframe::Minute => "minute",
            Timeframe::Min3 => "3min",
            Timeframe::Min5 => "5min",
            Timeframe::Min10 => "10min",
            Timeframe::Min15 => "15min",
            Timeframe::Min30 => "30min",
            Timeframe::Min60 => "60min",
            Timeframe::Daily => "1day",
            Timeframe::DailyLong => "daily",
        }
    }

    /// Approximate bar length in minutes (0 for daily resolutions).
    pub fn minutes(self) -> u32 {
        match self {
            Timeframe::Minute => 1,
            Timeframe::Min3 => 3,
            Timeframe::Min5 => 5,
            Timeframe::Min10 => 10,
            Timeframe::Min15 => 15,
            Timeframe::Min30 => 30,
            Timeframe::Min60 => 60,
            Timeframe::Daily | Timeframe::DailyLong => 0,
        }
    }

    /// True for the Yahoo long-history set, whose `date` column is tz-naive and
    /// which carries an extra `adj close` column.
    pub fn is_tz_naive(self) -> bool {
        matches!(self, Timeframe::DailyLong)
    }
}

/// Absolute path to a symbol's parquet file at a given resolution, e.g.
/// `1500-Stocks-Parquest/minute/RELIANCE.parquet`.
pub fn parquet_path(root: &Path, symbol: &str, tf: Timeframe) -> PathBuf {
    root.join(tf.dir()).join(format!("{symbol}.parquet"))
}

// ---------------------------------------------------------------------------
// Capital, budget slider, and risk meter
// ---------------------------------------------------------------------------

/// Default budget when the dashboard loads (₹5,00,000).
pub const BUDGET_DEFAULT: f64 = 500_000.0;
/// Each slider click adjusts the budget by this amount (₹50,000).
pub const BUDGET_STEP: f64 = 50_000.0;
/// Floor for the budget slider.
pub const BUDGET_MIN: f64 = 50_000.0;
/// Practical ceiling for the slider (15× the default — generous headroom).
pub const BUDGET_MAX: f64 = 7_500_000.0;

/// Risk-meter bounds: 1%–7% of budget risked per trade.
pub const RISK_MIN_PCT: f64 = 0.01;
pub const RISK_MAX_PCT: f64 = 0.07;
pub const RISK_DEFAULT_PCT: f64 = 0.01;

/// MIS intraday leverage cap used for the budget → max-notional guard.
pub const LEVERAGE: f64 = 5.0;

/// Live UI-controlled settings. Always clamped to safe bounds on construction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UserSettings {
    /// Total capital pool in INR.
    pub budget: f64,
    /// Fraction of budget risked per trade, in [RISK_MIN_PCT, RISK_MAX_PCT].
    pub risk_pct: f64,
}

impl Default for UserSettings {
    fn default() -> Self {
        UserSettings {
            budget: BUDGET_DEFAULT,
            risk_pct: RISK_DEFAULT_PCT,
        }
    }
}

impl UserSettings {
    /// Build from raw UI values, clamping both into legal range.
    pub fn new(budget: f64, risk_pct: f64) -> Self {
        UserSettings {
            budget: budget.clamp(BUDGET_MIN, BUDGET_MAX),
            risk_pct: risk_pct.clamp(RISK_MIN_PCT, RISK_MAX_PCT),
        }
    }

    /// Rupees risked per trade (`budget × risk_pct`).
    pub fn risk_amount(&self) -> f64 {
        self.budget * self.risk_pct
    }

    /// Maximum deployable notional under MIS leverage (`budget × LEVERAGE`).
    pub fn max_notional(&self) -> f64 {
        self.budget * LEVERAGE
    }
}

// ---------------------------------------------------------------------------
// Strategy / backtest defaults
// ---------------------------------------------------------------------------

/// ATR lookback used for stops, targets, and volatility sizing.
pub const ATR_PERIOD: usize = 14;
/// Default stop distance in ATR multiples (`SL = entry ± k·ATR`).
pub const SL_ATR_MULT: f64 = 1.5;
/// Default reward:risk (`target = entry ± RR·ATR`... see note in risk_manager).
pub const DEFAULT_RR: f64 = 2.0;

/// Round-trip transaction cost as a fraction of notional (~0.12%): brokerage +
/// STT + exchange/SEBI/GST + a slippage allowance. Subtracted in the backtest
/// and in projected P&L so figures are net, not gross.
pub const ROUND_TRIP_COST: f64 = 0.0012;

/// A (symbol, strategy, direction) combo must clear these to be eligible for the
/// Top-10 lists — keeps statistical noise out of the rankings.
pub const MIN_BACKTEST_N: usize = 30;
pub const MIN_PROFIT_FACTOR: f64 = 1.2;

/// Number of names shown in each of the Buy / Sell lists.
pub const TOP_N: usize = 10;

// ---------------------------------------------------------------------------
// Live engine
// ---------------------------------------------------------------------------

/// Per-symbol sliding window of the most recent ticks held in memory.
pub const LIVE_WINDOW: usize = 1000;
/// Trailing trading days used to build the intraday volume profile (VAH/VAL).
pub const VOLUME_PROFILE_DAYS: usize = 25;
/// Fraction of volume that defines the "value area" (standard 70%).
pub const VALUE_AREA_PCT: f64 = 0.70;

// ---------------------------------------------------------------------------
// Shared primitives
// ---------------------------------------------------------------------------

/// Trade side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Long => "BUY",
            Direction::Short => "SELL",
        }
    }
    /// +1 for long, -1 for short — handy for signed P&L math.
    pub fn sign(self) -> f64 {
        match self {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_clamp_into_legal_range() {
        let s = UserSettings::new(10.0, 0.5); // both out of range
        assert_eq!(s.budget, BUDGET_MIN);
        assert_eq!(s.risk_pct, RISK_MAX_PCT);
        let s2 = UserSettings::new(500_000.0, 0.02);
        assert_eq!(s2.risk_amount(), 10_000.0);
        assert_eq!(s2.max_notional(), 2_500_000.0);
    }

    #[test]
    fn parquet_path_uses_symbol_dot_parquet() {
        let p = parquet_path(Path::new("/data"), "RELIANCE", Timeframe::Minute);
        assert_eq!(p, PathBuf::from("/data/minute/RELIANCE.parquet"));
        assert!(Timeframe::DailyLong.is_tz_naive());
        assert!(!Timeframe::Daily.is_tz_naive());
    }

    #[test]
    fn direction_sign_and_label() {
        assert_eq!(Direction::Long.sign(), 1.0);
        assert_eq!(Direction::Short.as_str(), "SELL");
    }
}

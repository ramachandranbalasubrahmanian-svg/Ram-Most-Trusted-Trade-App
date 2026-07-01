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

/// True if `t` (IST wall-clock time) is inside the continuous trading session
/// (09:15:00–15:30:00). Live tick ingestion / signal generation must be gated to
/// this window — outside it (incl. the 09:00–09:15 pre-open call auction, where
/// VWAP and continuous indicators are unreliable) no live signal should fire.
/// Replay/backtest are an offline simulator and are intentionally NOT gated.
#[allow(dead_code)] // called from the live tick path (wired when live ingestion lands) + tests
pub fn is_regular_session(t: NaiveTime) -> bool {
    t >= session_open() && t <= session_close()
}

/// True if `t` (IST) is inside the pre-market gap-analysis window (09:00–09:08).
/// Gap math (prior-close vs open) is auction-safe — it computes no continuous
/// indicators — so it is fine to run in this pre-open window.
#[allow(dead_code)] // available for an opt-in timed pre-market trigger + tests
pub fn is_premarket_gap_window(t: NaiveTime) -> bool {
    t >= premarket_start() && t <= premarket_end()
}

// ---------------------------------------------------------------------------
// Data archive layout
// ---------------------------------------------------------------------------

/// Env var that overrides the parquet archive root.
pub const DATA_ROOT_ENV: &str = "RAM_ISTP_DATA_ROOT";
/// Default archive root, relative to the project working directory.
pub const DEFAULT_DATA_ROOT: &str = "1500-Stocks-Parquest";

/// Resolve the parquet archive root (env override → default).
pub fn data_root() -> PathBuf {
    // Treat an empty value (e.g. a blank `RAM_ISTP_DATA_ROOT=` line in .env that
    // dotenv loads as "") as unset, so we fall back to the default rather than an
    // empty path.
    std::env::var(DATA_ROOT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_ROOT))
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

/// Live Trade Plan: portfolio total-risk ceiling as a fraction of budget. The
/// per-trade risk% is the user's; the plan additionally caps the SUM of risk
/// across the selected basket here, so e.g. five 1% trades (5%) are allowed but a
/// high per-trade risk% can't silently stack into reckless aggregate risk. The
/// effective cap is `max(one trade's risk, budget × this)` — a single trade at
/// the user's risk% is always allowed even if it alone exceeds this.
pub const MAX_PORTFOLIO_RISK_PCT: f64 = 0.06;
/// Live Trade Plan: most positions held concurrently (also the RiskLimits default).
pub const PLAN_MAX_CONCURRENT: usize = 5;
/// Live Trade Plan: most positions per sector (diversification guard).
pub const PLAN_MAX_PER_SECTOR: usize = 2;
/// Live Trade Plan: "safe" intraday fill ceiling as a fraction of average daily
/// volume. An order above ~1% of ADV starts moving the price against you on entry
/// AND exit; `max_safe_qty = floor(this × ADV)` is shown as a fillable guide.
pub const LIQUIDITY_PARTICIPATION_CAP: f64 = 0.01;
/// ADV lookback (trading days) for the participation-rate liquidity flag.
pub const ADV_WINDOW_DAYS: usize = 20;
/// Daily-returns lookback for the basket correlation / ENB.
pub const BASKET_CORR_DAYS: usize = 60;

// --- Trading Desk: capital pool, risk tiers, circuit breaker ---------------

/// Liquid trading capital pool for the Trading Desk (₹10,00,000).
pub const CAPITAL_POOL: f64 = 1_000_000.0;
/// Scalable ceiling for the capital pool (₹15,00,000).
pub const CAPITAL_POOL_MAX: f64 = 1_500_000.0;

/// Three selectable manual risk tiers (fraction of capital risked per signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RiskTier {
    Conservative,
    Moderate,
    Aggressive,
}

impl RiskTier {
    pub fn pct(self) -> f64 {
        match self {
            RiskTier::Conservative => 0.005,
            RiskTier::Moderate => 0.01,
            RiskTier::Aggressive => 0.02,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            RiskTier::Conservative => "Conservative",
            RiskTier::Moderate => "Moderate",
            RiskTier::Aggressive => "Aggressive",
        }
    }
}

/// Daily synthetic-drawdown limit: a paper loss beyond −2% of the capital pool
/// triggers a system-wide Signal Freeze for the rest of the session.
pub const DRAWDOWN_FREEZE_PCT: f64 = 0.02;

/// Session governor (behavioral kill-switch) limits — advisory defaults, applied
/// on top of the daily-loss freeze above. Over-trading, not one bad signal, is the
/// empirical top cause of leveraged intraday ruin.
pub const MAX_TRADES_PER_DAY: usize = 5;
pub const MAX_CONSECUTIVE_LOSSES: usize = 3;

/// ATR multiple for the SEBI-compliance limit buffer: `limit = LTP ± ATR×0.1`.
pub const STAGING_LIMIT_ATR_MULT: f64 = 0.1;

// ---------------------------------------------------------------------------
// Portfolio correlation (the "real independent bets" measure on /portfolio, /desk)
// ---------------------------------------------------------------------------

/// Max common daily sessions used to estimate the holdings return-correlation
/// matrix (≈2 trading years). The actual window is `min(this, common overlap)`;
/// a recently-listed name naturally caps it. Reported as `corr_sessions`.
pub const CORR_LOOKBACK_SESSIONS: usize = 504;

/// Below this many aligned sessions the correlation estimate is too thin to
/// trust, so the correlation block is omitted (the page falls back to the
/// honest weight-based figure rather than print a noisy number).
pub const CORR_MIN_SESSIONS: usize = 60;

/// Daily-return correlation at/above which two names are linked into the same
/// "moves together" cluster (single-linkage). 0.60 ≈ a clearly shared driver.
pub const CORR_CLUSTER_THRESHOLD: f64 = 0.60;

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
/// Maximum number of NSE-equity instruments to map → `instrument_token` and
/// subscribe on the live WebSocket. A configurable knob, NOT a hardcoded
/// universe: the actual tokens always come from the live Kite instruments dump
/// (`kite_instruments`), never from literals. Stays under Kite's ~3000
/// tokens/connection cap. Override via `RAM_ISTP_LIVE_UNIVERSE_MAX`.
pub const LIVE_UNIVERSE_MAX: usize = 1600;

/// Max news-sentiment API requests per IST trading day. Kept under the provider's
/// 100/day free-tier limit with headroom. Sentiment is fetched on-demand only for
/// Top-10 names that cross the volatility + VWAP-extension triggers.
pub const NEWS_DAILY_CAP: u32 = 90;

// --- High-conviction shortlist thresholds (DISPLAY-ONLY) ------------------
// The honest version of a ">60%" filter: a setup is shortlisted only if its
// Confidence AND its 95% Wilson win-FLOOR AND the DSR reliability gate all clear
// these bars. It is a derived LABEL over already-scored output — it never feeds
// back into Confidence. `SHORTLIST_DSR_MIN` is pinned to the same 0.50 the
// Confidence gate uses, so the shortlist can never disagree with the gate.
pub const SHORTLIST_MIN_CONFIDENCE: u32 = 70;
pub const SHORTLIST_MIN_PROB: f64 = 60.0;
pub const SHORTLIST_DSR_MIN: f64 = 0.50;

// James–Stein shrinkage applied to a candidate's full-sample expectancy ONLY for
// the live Top-10 RANKING score — never for Confidence, the eligibility gate, or
// the displayed raw expectancy. With prior 0.0 and a pseudo-count of 40, a small-n
// "lucky" edge (n≈30) is pulled roughly halfway toward zero while a robust edge
// (n≳300) keeps ~88% of its raw expectancy, so small-sample flukes stop topping
// the list (67 eligible edges today carry n<50). Display-only re-ordering.
pub const SHRINK_PRIOR_R: f64 = 0.0;
pub const SHRINK_STRENGTH: f64 = 40.0;

/// Shortlist min-Confidence, honouring `RAM_ISTP_SHORTLIST_MIN_CONFIDENCE`.
pub fn shortlist_min_confidence() -> u32 {
    std::env::var("RAM_ISTP_SHORTLIST_MIN_CONFIDENCE")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(SHORTLIST_MIN_CONFIDENCE)
}
/// Shortlist min win-floor %, honouring `RAM_ISTP_SHORTLIST_MIN_PROB`.
pub fn shortlist_min_prob() -> f64 {
    std::env::var("RAM_ISTP_SHORTLIST_MIN_PROB")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| *n > 0.0)
        .unwrap_or(SHORTLIST_MIN_PROB)
}

/// Resolve the live-universe cap, honouring `RAM_ISTP_LIVE_UNIVERSE_MAX`.
pub fn live_universe_max() -> usize {
    std::env::var("RAM_ISTP_LIVE_UNIVERSE_MAX")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(LIVE_UNIVERSE_MAX)
}
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

    #[test]
    fn shortlist_min_conf_above_dsr_gate_cap() {
        // The DSR gate caps a flagged name's Confidence at 59; the shortlist needs
        // >= 70. So a DSR-gated name can NEVER be shortlisted — the two can't
        // disagree. Accessors fall back to the consts when env is unset.
        assert!(SHORTLIST_MIN_CONFIDENCE > 59);
        assert_eq!(shortlist_min_confidence(), SHORTLIST_MIN_CONFIDENCE);
        assert_eq!(shortlist_min_prob(), SHORTLIST_MIN_PROB);
        assert!((SHORTLIST_DSR_MIN - 0.50).abs() < 1e-9);
    }

    #[test]
    fn session_and_premarket_windows() {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        // Regular session 09:15–15:30 inclusive.
        assert!(!is_regular_session(t(9, 14)), "pre-open auction is not the session");
        assert!(is_regular_session(t(9, 15)));
        assert!(is_regular_session(t(15, 30)));
        assert!(!is_regular_session(t(15, 31)), "after close");
        assert!(!is_regular_session(t(8, 59)));
        // Pre-market gap window 09:00–09:08.
        assert!(is_premarket_gap_window(t(9, 0)));
        assert!(is_premarket_gap_window(t(9, 8)));
        assert!(!is_premarket_gap_window(t(9, 9)));
        assert!(!is_premarket_gap_window(t(8, 59)));
        // The pre-open auction (09:09–09:14) is in neither window.
        assert!(!is_regular_session(t(9, 10)) && !is_premarket_gap_window(t(9, 10)));
    }
}

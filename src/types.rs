//! Shared contracts that cross module boundaries.
//!
//! Ingestion → analytics → risk → server → UI all speak in these types. They
//! depend only on `config` (for `Direction`/`UserSettings`) so there are no
//! dependency cycles. Anything serialized to the dashboard lives here.

use serde::{Deserialize, Serialize};

use crate::config::Direction;

// ---------------------------------------------------------------------------
// Market data (ingestion → analytics)
// ---------------------------------------------------------------------------

/// One level of the order book.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DepthLevel {
    pub price: f64,
    pub qty: i64,
    pub orders: i64,
}

/// Top-5 bid/ask market depth (Kite "full" mode).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MarketDepth {
    pub bids: [DepthLevel; 5],
    pub asks: [DepthLevel; 5],
}

/// A single normalized tick, the unit that flows over the crossbeam channel.
/// Synthetic (replay) and live (Kite) ticks are indistinguishable downstream.
#[derive(Debug, Clone)]
pub struct Tick {
    pub symbol: String,
    pub instrument_token: u32,
    /// Last traded price.
    pub ltp: f64,
    /// Cumulative volume for the day, as reported by the feed.
    pub volume_day: i64,
    /// Exchange tick timestamp, epoch microseconds IST. 0 if unavailable.
    pub ts_exchange_us: i64,
    /// Local receive timestamp, epoch microseconds.
    pub ts_recv_us: i64,
    /// `ts_recv_us - ts_exchange_us` in microseconds (informational; can be <0
    /// in replay). Computed at ingestion.
    pub latency_us: i64,
    /// Full-mode depth, if present.
    pub depth: Option<MarketDepth>,
}

// ---------------------------------------------------------------------------
// Edge map projection (strategy_engine → analytics)
// ---------------------------------------------------------------------------

/// A single eligible backtested edge, flattened for fast live lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EligibleEdge {
    pub strategy: String,
    pub direction: Direction,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub win_pct: f64,
    pub n: usize,
}

// ---------------------------------------------------------------------------
// Live analytics features (analytics internal → candidate scoring)
// ---------------------------------------------------------------------------

/// Microstructure features computed from a symbol's live sliding window.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct LiveFeatures {
    /// Order-book imbalance in [-1, 1]; positive = bid-heavy.
    pub obi: f64,
    /// Session VWAP.
    pub vwap: f64,
    /// (last - vwap)/vwap * 100, signed.
    pub vwap_dev_pct: f64,
    /// Rolling z-score of price over the live window.
    pub zscore: f64,
    /// Relative volume vs the window's running average.
    pub rvol: f64,
    /// Best bid/ask spread as a percent of mid (0 if no depth).
    pub spread_pct: f64,
    pub last_price: f64,
}

// ---------------------------------------------------------------------------
// Ranked candidate (analytics → risk)
// ---------------------------------------------------------------------------

/// A live-firing setup with its backtested edge and live confirmation, before
/// position sizing. `score` drives ranking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub symbol: String,
    pub strategy: String,
    pub direction: Direction,
    // backtested edge
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub win_pct: f64,
    pub n: usize,
    // live
    pub last_price: f64,
    /// ATR (price units) to size stops/targets from.
    pub atr: f64,
    pub features: LiveFeatures,
    /// Live confirmation multiplier in roughly [0, 2]; 1.0 = neutral.
    pub live_score: f64,
    /// Final ranking score = f(edge, live_score).
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Sizing + ranked signal (risk → server → UI)
// ---------------------------------------------------------------------------

/// Position sizing + projected P&L for one candidate under current settings.
/// Advisory only — nothing here is ever sent to a broker.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Sizing {
    pub shares: i64,
    pub entry: f64,
    pub sl: f64,
    pub target: f64,
    pub risk_per_share: f64,
    pub notional: f64,
    /// Rupees if the target is hit.
    pub proj_profit: f64,
    /// Rupees if the stop is hit (negative).
    pub proj_loss: f64,
    /// Expectancy-weighted rupee P&L = shares·risk_per_share·expectancy_r.
    pub exp_pnl: f64,
}

/// One fully-formed row in a Top-10 list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedSignal {
    pub symbol: String,
    pub strategy: String,
    /// "BUY" or "SELL".
    pub side: String,
    pub entry: f64,
    pub sl: f64,
    pub target: f64,
    pub shares: i64,
    pub notional: f64,
    pub proj_profit: f64,
    pub proj_loss: f64,
    pub exp_pnl: f64,
    pub expectancy_r: f64,
    pub win_pct: f64,
    pub profit_factor: f64,
    pub n: usize,
    pub score: f64,
    pub obi: f64,
    pub rvol: f64,
    /// Honest caveat / context shown in the row (e.g. low sample, wide spread).
    pub note: String,
}

// ---------------------------------------------------------------------------
// Risk meter, diagnostics, alerts, and the full packet (→ UI)
// ---------------------------------------------------------------------------

/// Exposure gauge: deployed notional vs leverage headroom.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RiskMeter {
    pub budget: f64,
    pub max_notional: f64,
    pub deployed_notional: f64,
    pub free_margin: f64,
    /// deployed / max_notional * 100.
    pub exposure_pct: f64,
    /// "green" | "amber" | "red".
    pub color: String,
}

/// System diagnostics shown in the dashboard footer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    /// Local tick-to-signal processing time (µs) — the genuine sub-ms figure.
    pub tick_to_signal_us: u64,
    /// Median exchange→local tick latency (µs); informational.
    pub ingest_latency_us: i64,
    pub ticks_per_sec: u64,
    pub threads: usize,
    /// Approximate replay/feed I/O throughput (MB/s), best-effort.
    pub io_mbps: f64,
    pub universe: usize,
    pub eligible_edges: usize,
}

/// A non-actionable alert (e.g. 15:15 square-off reminder, wide-spread warning).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// "squareoff" | "spread" | "circuit" | "info".
    pub kind: String,
    pub message: String,
    /// "info" | "warn" | "danger".
    pub severity: String,
}

/// Compact view of the user-controlled settings, echoed back to the UI.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SettingsView {
    pub budget: f64,
    pub risk_pct: f64,
}

/// The complete payload pushed over `/ws/live_signals` on every update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalPacket {
    /// IST wall-clock of this snapshot, "YYYY-MM-DD HH:MM:SS".
    pub ts_ist: String,
    /// "replay" | "live".
    pub mode: String,
    pub settings: SettingsView,
    pub top_buy: Vec<RankedSignal>,
    pub top_sell: Vec<RankedSignal>,
    pub risk_meter: RiskMeter,
    pub diagnostics: Diagnostics,
    pub alerts: Vec<Alert>,
}

impl SignalPacket {
    /// An empty packet for app startup before the first tick arrives.
    pub fn empty(settings: SettingsView, mode: &str) -> Self {
        SignalPacket {
            ts_ist: String::new(),
            mode: mode.to_string(),
            settings,
            top_buy: Vec::new(),
            top_sell: Vec::new(),
            risk_meter: RiskMeter::default(),
            diagnostics: Diagnostics::default(),
            alerts: Vec::new(),
        }
    }
}

/// Inbound message from the UI: budget / risk-meter changes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SettingsMsg {
    pub budget: f64,
    pub risk_pct: f64,
}

// ===========================================================================
// Intraday Suggestion page (per-stock deep-dive + scanner)
// ===========================================================================

/// One conviction sub-score component, e.g. ("mc_prob_profit", 15.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvictionDelta {
    pub name: String,
    pub points: f64,
}

/// The full statistics + sizing for a single (symbol, strategy, interval, side,
/// R:R) setup — everything one strategy card renders. Honesty-first: all R / P&L
/// figures are net of round-trip cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupCard {
    pub symbol: String,
    pub side: String,         // "BUY" | "SELL"
    pub interval: String,     // "1 Hour", "30 Minutes", ...
    pub rr: f64,              // reward:risk (target_mult / sl_mult)
    pub rr_label: String,     // "1 : 3.0"
    pub timeframes_agree: u32, // "✓ N timeframes"

    // entry / stop / target
    pub entry: f64,           // last close (approximate, pre-market)
    pub last_close: f64,
    pub sl: f64,
    pub sl_atr_mult: f64,
    pub target: f64,
    pub target_atr_mult: f64,
    pub atr: f64,

    // sizing (under current capital + risk%)
    pub quantity: i64,
    pub risk_pct: f64,
    pub max_risk: f64,    // gross ₹ if stop hits (≈ risk budget)
    pub max_reward: f64,  // gross ₹ if target hits
    // net of itemized round-trip costs (broker + taxes + slippage)
    pub net_profit: f64,  // target hit, after all costs
    pub net_loss: f64,    // stop hit, after all costs (negative)
    pub costs: CostBreakdown, // itemized costs for the target-hit scenario

    // core backtest stats (net of cost)
    pub win_rate: f64,
    pub profit_factor: f64,
    pub expectancy_r: f64,
    pub n_trades: usize,
    pub sharpe: f64,
    pub calmar: f64,

    // robustness
    pub mc_prob_profit: f64,  // % of bootstrap paths ending positive
    pub mc_p95_dd_r: f64,     // 95th-pct max drawdown (R)
    pub dsr: f64,             // deflated Sharpe (overfit-adj.), 0..1
    pub exp_ci_low: f64,      // 90% bootstrap CI on expectancy
    pub exp_ci_high: f64,
    pub exp_shrunk: f64,      // Bayesian-shrunk expectancy

    // probability / confidence / conviction
    pub prob_score: f64,      // win chance 0-100 (historical win rate)
    pub prob_floor: f64,      // 95% Wilson lower bound on win rate
    pub confidence: Option<u32>,
    pub confidence_band: String,
    pub t_stat: f64,
    pub p_value: f64,
    pub provisional: bool,
    pub conviction: u32,
    pub conviction_label: String,
    pub conviction_deltas: Vec<ConvictionDelta>,

    // honest caveats
    pub selection_artifact: Option<String>, // DSR-based overfit warning
}

/// The four page strategies, each rendered as a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestStrategy {
    VwapTrend,
    OpeningRange,
    PrevDayBreakout,
    GapAndGo,
}

impl SuggestStrategy {
    pub fn key(self) -> &'static str {
        match self {
            SuggestStrategy::VwapTrend => "vwap_trend",
            SuggestStrategy::OpeningRange => "opening_range",
            SuggestStrategy::PrevDayBreakout => "prev_day_breakout",
            SuggestStrategy::GapAndGo => "gap_and_go",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            SuggestStrategy::VwapTrend => "VWAP Trend",
            SuggestStrategy::OpeningRange => "Opening Range",
            SuggestStrategy::PrevDayBreakout => "Prev-Day Breakout",
            SuggestStrategy::GapAndGo => "Gap-and-Go",
        }
    }
    pub fn emoji(self) -> &'static str {
        match self {
            SuggestStrategy::VwapTrend => "🟦",
            SuggestStrategy::OpeningRange => "🟧",
            SuggestStrategy::PrevDayBreakout => "🟪",
            SuggestStrategy::GapAndGo => "🟩",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            SuggestStrategy::VwapTrend => "Enter on a fresh VWAP crossover in the trend direction (mean-anchor continuation).",
            SuggestStrategy::OpeningRange => "Break of the first-30-min high/low — classic opening-range breakout.",
            SuggestStrategy::PrevDayBreakout => "Break of the previous day's high/low on volume — momentum continuation.",
            SuggestStrategy::GapAndGo => "After a gap, ride the gap direction while price holds above/below open & VWAP.",
        }
    }
    pub fn all() -> [SuggestStrategy; 4] {
        [
            SuggestStrategy::VwapTrend,
            SuggestStrategy::OpeningRange,
            SuggestStrategy::PrevDayBreakout,
            SuggestStrategy::GapAndGo,
        ]
    }
}

/// One strategy block on the page: either its best setup + verdict, or an
/// honest "no edge" result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyBlock {
    pub key: String,
    pub name: String,
    pub emoji: String,
    pub description: String,
    /// "skip" | "shortlist" | "no_edge" | "tradeable" — drives the badge color.
    pub verdict: String,
    pub verdict_text: String,   // the badge line, e.g. "⛔ SKIP — weak / unreliable edge"
    pub confidence_note: String,
    pub headline: Option<String>, // "📊 Historically won ~64 of 100 ..."
    pub best: Option<SetupCard>,
    pub valid_setups: usize,      // "All N valid {strategy} setups"
}

/// The full per-stock suggestion payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StockSuggestion {
    pub symbol: String,
    pub intervals_available: Vec<String>,
    pub trading_days: usize,
    pub last_date: String,
    pub days_old: i64,
    /// "Gap-and-Go — SELL on 1 Hour (R:R 1:3.0) · Confidence 86/100", or None.
    pub best_overall: Option<String>,
    pub blocks: Vec<StrategyBlock>,
    pub total_configs: usize,
    pub disclaimer: String,
}

/// One row of the 10-Buy / 10-Sell scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScannerRow {
    pub symbol: String,
    pub side: String,
    pub strategy: String,
    pub interval: String,
    pub rr_label: String,
    pub confidence: u32,
    pub expectancy_r: f64,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub n_trades: usize,
    pub entry: f64,
}

/// The scanner result: top-N best Buy and Sell setups across the universe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub top_buy: Vec<ScannerRow>,
    pub top_sell: Vec<ScannerRow>,
    pub scanned: usize,
    pub built_ist: String,
}

/// Itemized round-trip transaction cost for one intraday equity trade (INR).
/// Mirrors the Zerodha-style charge stack so net P&L is exact, not blended.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub brokerage: f64,
    pub stt: f64,
    pub exchange_txn: f64,
    pub sebi: f64,
    pub gst: f64,
    pub stamp: f64,
    pub slippage: f64,
    pub total: f64,
}

/// One row of the Capital-Fit ATR finder: a stock sized to YOUR capital + risk,
/// with a "fit" verdict and net-of-cost projected P&L.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinderRow {
    pub symbol: String,
    pub strategy: String,
    pub side: String,
    pub interval: String,
    pub rr_label: String,
    pub entry: f64,
    pub atr: f64,
    pub sl: f64,
    pub target: f64,
    /// Shares actually tradeable = min(risk-based, leverage-affordable).
    pub shares: i64,
    /// Shares the risk budget alone would buy (before the leverage cap).
    pub shares_by_risk: i64,
    /// Shares the 5× buying power can afford at this price.
    pub max_affordable: i64,
    /// "ideal" (risk fully deployable) | "leverage_bound" (too pricey for full risk).
    pub fit: String,
    pub capital_deployed: f64,
    pub capital_efficiency_pct: f64,
    /// Rupees actually at risk if the stop hits (≈ risk budget when ideal).
    pub risk_taken: f64,
    /// Net of itemized costs.
    pub net_profit: f64,
    pub net_loss: f64,
    pub confidence: u32,
    pub expectancy_r: f64,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub n_trades: usize,
    /// Ranking score = Confidence × deployability.
    pub fit_score: f64,
}

/// Result of the Capital-Fit finder: every qualifying stock for the given
/// capital + risk, ranked by fit-adjusted edge (not a fixed Top-N).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinderResult {
    pub capital: f64,
    pub risk_pct: f64,
    pub rows: Vec<FinderRow>,
    pub qualifying: usize,
    pub scanned: usize,
    pub built_ist: String,
}

/// NIFTY regime + market breadth context (display-only; never changes a score).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeInfo {
    pub nifty_regime: String, // "Up" | "Down" | "Flat"
    pub breadth_up: usize,
    pub breadth_down: usize,
    pub breadth_label: String, // "narrow" | "broad" | "neutral"
}

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

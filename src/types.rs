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

/// Display-only robustness annotations for an edge-map / Top-10 edge. The
/// edge-map tier ranks on `eligible()` only; these are the deep-tier robustness
/// stats (purged+embargoed OOS, walk-forward consistency, per-symbol deflated
/// Sharpe) carried alongside so the Top-10 can be SHOWN them. They NEVER change
/// `eligible()`, Confidence, ranking, or sizing — annotation only. All default
/// to "not computed" so an OLD cached edge map still deserializes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Robustness {
    /// Mean R on the held-out (purged+embargoed) out-of-sample tail; None when
    /// there were too few OOS trades to estimate.
    pub oos_expectancy: Option<f64>,
    pub oos_n: usize,
    /// Walk-forward fold consistency in [0, 1] (share of folds that stayed +EV).
    pub wf_consistency: f64,
    /// Deflated Sharpe over this symbol's own strategy×direction trial set [0, 1].
    pub dsr: f64,
}

/// A single eligible backtested edge, flattened for fast live lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EligibleEdge {
    pub strategy: String,
    pub direction: Direction,
    pub expectancy_r: f64,
    pub profit_factor: f64,
    pub win_pct: f64,
    pub n: usize,
    /// Display-only robustness annotation (see `Robustness`).
    #[serde(default)]
    pub robustness: Robustness,
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
    /// James–Stein-shrunk expectancy (toward 0 by sample size) — the value the
    /// Top-10 RANKS on, so small-n flukes don't top the list. Display/ranking
    /// only; never Confidence. Approaches `expectancy_r` for large n.
    pub shrunk_expectancy_r: f64,
    pub profit_factor: f64,
    pub win_pct: f64,
    pub n: usize,
    /// Display-only robustness annotation carried from the edge map.
    #[serde(default)]
    pub robustness: Robustness,
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
    /// Shrunk expectancy used for ranking (display-only; see `Candidate`).
    pub shrunk_expectancy_r: f64,
    pub win_pct: f64,
    pub profit_factor: f64,
    pub n: usize,
    /// Display-only robustness annotation (OOS / walk-forward / DSR).
    #[serde(default)]
    pub robustness: Robustness,
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

/// One position in the Live Trade Plan — a Top-10 idea that fits the budget/risk
/// basket. Display-only (no orders). Carried from a `RankedSignal`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanPosition {
    pub symbol: String,
    pub side: String,
    pub strategy: String,
    pub shares: i64,
    pub entry: f64,
    pub sl: f64,
    pub target: f64,
    /// ₹ actually at risk if the stop hits (= |proj_loss|).
    pub risk_inr: f64,
    pub notional: f64,
    /// ATR (price units) implied by the stop distance.
    pub atr: f64,
    /// Stop distance as % of entry — "how far the SL sits".
    pub atr_pct: f64,
    pub proj_profit: f64,
    pub proj_loss: f64,
    pub exp_pnl: f64,
    /// Sector (best-effort; empty when unknown).
    pub sector: String,
    // --- liquidity-at-size (participation rate) ---
    /// Average daily share volume (recent window); 0 when unknown.
    pub adv: f64,
    /// Planned qty as % of ADV — market-impact proxy.
    pub participation_pct: f64,
    /// "ok" | "caution" | "heavy" | "illiquid" | "unknown".
    pub liquidity: String,
    /// Shares you could fill within the safe participation cap (≈1% of ADV).
    pub max_safe_qty: i64,
}

/// Portfolio aggregates for the Live Trade Plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanTotals {
    pub n_positions: usize,
    pub n_long: usize,
    pub n_short: usize,
    pub budget: f64,
    pub max_notional: f64,
    pub deployed: f64,
    /// deployed / max_notional × 100.
    pub deployed_pct: f64,
    pub free_margin: f64,
    /// Sum of per-position ₹ risk.
    pub total_risk_inr: f64,
    /// total_risk_inr / budget × 100 — the real aggregate risk if all stops hit.
    pub total_risk_pct: f64,
    /// Expectancy-weighted basket P&L.
    pub exp_pnl: f64,
    /// All targets hit.
    pub best_case: f64,
    /// All stops hit (≤ 0).
    pub worst_case: f64,
    pub long_notional: f64,
    pub short_notional: f64,
    /// Positions whose planned qty exceeds the safe fill size (heavy/illiquid).
    pub n_illiquid: usize,
    /// "green" | "amber" | "red" exposure colour for the basket.
    pub color: String,
}

/// The Live Trade Plan: a budget/risk/ATR-aware basket selected from the Top-10.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TradePlan {
    pub positions: Vec<PlanPosition>,
    pub totals: PlanTotals,
    /// How many ranked ideas were considered.
    pub considered: usize,
    /// Ideas dropped because they didn't fit (leverage / risk cap / count / sector).
    pub skipped_leverage: usize,
    pub skipped_risk_cap: usize,
    pub skipped_concurrent: usize,
    pub skipped_sector: usize,
    /// Honest, plain-English notes (skip reasons, directional bias, …).
    pub notes: Vec<String>,
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
    /// Budget/risk/ATR-aware actionable basket (display-only).
    #[serde(default)]
    pub trade_plan: TradePlan,
    /// Daily market-regime + breadth context (display-only; same each second).
    #[serde(default)]
    pub market_regime: crate::market_regime::MarketRegime,
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
            trade_plan: TradePlan::default(),
            market_regime: crate::market_regime::MarketRegime::default(),
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

    // honesty stats (display-only; never feed Confidence)
    #[serde(default)]
    pub ambiguous_frac: f64,  // frac of exits on bars that spanned BOTH stop+target
    #[serde(default)]
    pub exp_2x_slip: f64,     // net expectancy (R) if slippage is 2× the model
    #[serde(default)]
    pub exp_3x_slip: f64,     // net expectancy (R) if slippage is 3× the model

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

    /// Display-only: passes the high-conviction shortlist gate (Confidence +
    /// Wilson win-floor + DSR). A shortlist, NOT a "sure shot"; never gates score.
    #[serde(default)]
    pub shortlist: bool,
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

/// One timeframe's Probability-of-Backtest-Overfitting (CSCV/PBO) — display-only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PboRow {
    pub timeframe: String,
    /// PBO % (0–100): higher ⇒ the best in-sample config is more likely a fluke.
    pub pbo_pct: f64,
    pub n_configs: usize,
    pub n_blocks: usize,
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
    /// CSCV Probability of Backtest Overfitting per timeframe (display-only;
    /// never feeds Confidence/the gate). Empty when too few configs to estimate.
    #[serde(default)]
    pub pbo_by_tf: Vec<PboRow>,
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
    /// Stop / target (entry ± sl_mult·ATR, ± rr·sl_mult·ATR) — capital-independent.
    #[serde(default)]
    pub sl: f64,
    #[serde(default)]
    pub target: f64,
    #[serde(default)]
    pub atr: f64,
    /// Sizing for the user's capital + risk% (filled per-request by the handler;
    /// net P&L is after itemized round-trip cost). 0 when un-sized / unaffordable.
    #[serde(default)]
    pub shares: i64,
    #[serde(default)]
    pub net_profit: f64,
    #[serde(default)]
    pub net_loss: f64,
    /// Reliability provenance of this row's Confidence. The scanner gates over a
    /// lighter 3-interval (15m/30m/60m) DSR trial set; the per-stock deep-dive
    /// searches 6 intervals and is the stricter, authoritative number. "scan"
    /// here tells the UI to footnote "open the deep-dive for the final score".
    #[serde(default)]
    pub reliability: String,
    /// 95% Wilson lower bound on win rate (the honest probability floor).
    #[serde(default)]
    pub prob_floor: f64,
    /// Display-only high-conviction shortlist flag (Confidence + floor + DSR).
    #[serde(default)]
    pub shortlist: bool,
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
    /// Largest ATR (₹/share) across all backtested stocks today — the upper bound
    /// for the UI's "Max ATR" slider.
    #[serde(default)]
    pub max_atr_universe: f64,
    pub built_ist: String,
}

// ===========================================================================
// Trading Desk: signal staging, journal, circuit breaker, swing, portfolio
// ===========================================================================

/// A synthetic, SEBI-2026-compliant Bracket Order payload — generated for manual
/// copy/paste, NEVER sent to a broker. All prices are LIMIT (no naked market).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BracketOrder {
    pub symbol: String,
    pub instrument_token: u32,
    pub side: String, // "BUY" | "SELL"
    pub qty: i64,
    /// LTP ± ATR×0.1 (compliance buffer): the staged limit entry.
    pub limit_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    /// Trailing distance in price (ATR-scaled).
    pub trailing: f64,
    pub variety: String, // "BO"
}

/// One row of the Intraday Staging Console: a ready-to-copy staged signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedSignal {
    pub symbol: String,
    pub instrument_token: u32,
    pub side: String,
    pub ltp: f64,
    pub atr: f64,
    pub limit_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub qty: i64,
    pub notional: f64,
    pub bracket: BracketOrder,
    /// One-line copy/paste execution text.
    pub copy_text: String,
}

/// Lifecycle state of a generated signal in the manual journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalState {
    Generated,
    ManuallyAccepted,
    ManuallyRejected,
    Skipped,
}

impl SignalState {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalState::Generated => "Generated",
            SignalState::ManuallyAccepted => "Manually_Accepted",
            SignalState::ManuallyRejected => "Manually_Rejected",
            SignalState::Skipped => "Skipped",
        }
    }
    pub fn from_str(s: &str) -> SignalState {
        match s {
            "Manually_Accepted" => SignalState::ManuallyAccepted,
            "Manually_Rejected" => SignalState::ManuallyRejected,
            "Skipped" => SignalState::Skipped,
            _ => SignalState::Generated,
        }
    }
}

/// One row of `manual_validation_journal_2026`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: i64,
    pub generated_ist: String,
    pub entry_ist: Option<String>,
    pub exit_ist: Option<String>,
    pub instrument_token: u32,
    pub symbol: String,
    pub direction: String,
    pub strategy: String,
    pub alpha_trigger: String,
    pub intended_price: f64,
    /// User-entered actual fill (to compute true manual slippage).
    pub actual_fill_price: Option<f64>,
    pub exit_price: Option<f64>,
    pub qty: i64,
    pub state: String,
    pub pnl: Option<f64>,
    /// actual_fill − intended (signed by direction), in ₹/share.
    pub slippage: Option<f64>,
    pub sector: Option<String>,
}

/// Inbound payload to log/update a signal from the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalUpdate {
    pub id: i64,
    pub state: String,
    pub actual_fill_price: Option<f64>,
    pub exit_price: Option<f64>,
}

/// Synthetic drawdown / signal-freeze state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreezeState {
    pub frozen: bool,
    pub reason: String,
    /// Today's synthetic mark-to-market PnL across accepted trades (₹).
    pub daily_pnl: f64,
    /// The -2% capital threshold in ₹ (negative).
    pub threshold: f64,
    pub capital_pool: f64,
}

impl FreezeState {
    pub fn active(capital_pool: f64, threshold_pct: f64) -> Self {
        FreezeState {
            frozen: false,
            reason: String::new(),
            daily_pnl: 0.0,
            threshold: -capital_pool * threshold_pct,
            capital_pool,
        }
    }
}

/// One multi-day swing setup in the daily catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwingSetup {
    pub symbol: String,
    /// "volume_delivery_breakout" | "mean_reversion_200ema"
    pub kind: String,
    pub side: String,
    pub last_close: f64,
    pub ema200: f64,
    /// Latest volume / 50-day average volume.
    pub vol_ratio: f64,
    pub support: f64,
    pub resistance: f64,
    pub atr: f64,
    pub note: String,
    pub score: f64,
}

/// The pre-market Swing Trades Catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwingCatalog {
    pub setups: Vec<SwingSetup>,
    pub scanned: usize,
    pub built_ist: String,
}

/// Per-group performance attribution row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributionRow {
    pub key: String,
    pub trades: usize,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub pnl: f64,
}

/// Post-trade portfolio analytics from the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioMetrics {
    pub trades: usize,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub sharpe: f64,
    pub max_drawdown: f64,
    pub total_pnl: f64,
    /// Cumulative equity curve points: (label, cumulative_pnl).
    pub equity_curve: Vec<(String, f64)>,
    pub by_strategy: Vec<AttributionRow>,
    pub by_sector: Vec<AttributionRow>,
}

// ===========================================================================
// Holdings analytics (the user's REAL external portfolio across brokers).
// Display-only: shows the user THEIR risk picture. NEVER advice, NEVER an order.
// Distinct from PortfolioMetrics above (which analyses CLOSED simulated trades).
// ===========================================================================

/// A normalized holding (after ingest from CSV / manual / sample).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holding {
    pub symbol: String,
    pub qty: f64,
    pub avg_cost: f64,
    pub broker: String,
    pub sector: Option<String>,
    /// A statement-provided last/closing price (an EOD mark from the broker
    /// report). When present it is used as the mark, so off-archive names
    /// (recent demergers/IPOs) still value correctly instead of falling back to
    /// cost. `None` ⇒ the mark comes from the local archive's last close.
    #[serde(default)]
    pub last_price: Option<f64>,
}

/// Raw inbound holding from a POST body / CSV / pasted text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldingInput {
    pub symbol: String,
    pub qty: f64,
    pub avg_cost: f64,
    #[serde(default)]
    pub broker: Option<String>,
    #[serde(default)]
    pub sector: Option<String>,
    /// ISIN from a broker statement, if present — the most reliable key for
    /// resolving the row to an NSE trading symbol.
    #[serde(default)]
    pub isin: Option<String>,
    /// Closing/last price from a broker statement, if present — carried through
    /// as the holding's mark.
    #[serde(default)]
    pub last_price: Option<f64>,
}

/// Per-holding analysis row. `flag`/`flag_reason` state a WHY (concentration /
/// deep loss / sector over-weight / no eligible edge) — never a directive.
/// `kelly_band_*` is an ADVISORY sizing band (half-Kelly, clamped); [0,0] for
/// names with no eligible edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldingAnalysis {
    pub symbol: String,
    pub qty: f64,
    pub avg_cost: f64,
    pub broker: String,
    pub sector: Option<String>,
    pub last_price: Option<f64>,
    pub mark_is_live: bool,
    pub market_value: f64,
    pub cost_basis: f64,
    pub unrealized_pnl: f64,
    pub unrealized_pct: f64,
    pub weight_pct: f64,
    pub drawdown_vs_cost_pct: f64,
    pub edge_eligible: bool,
    pub edge_note: String,
    pub flag: String,
    pub flag_reason: String,
    pub kelly_band_low_pct: f64,
    pub kelly_band_high_pct: f64,
}

/// Exposure / heat for a sector or broker bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExposureRow {
    pub key: String,
    pub names: usize,
    pub value: f64,
    pub weight_pct: f64,
    pub unrealized_pnl: f64,
    pub heat: String, // "high" | "elevated" | "normal"
}

/// A cluster of names that move together. `basis` states the rule honestly
/// (e.g. "same sector (no price-correlation data)").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationCluster {
    pub label: String,
    pub members: Vec<String>,
    pub combined_weight_pct: f64,
    pub basis: String,
}

/// The full holdings risk picture. Display-only; carries a disclaimer + build
/// timestamp; `mark_is_live`/`marks_live` make staleness explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioAnalysis {
    pub total_cost: f64,
    pub total_value: f64,
    pub total_unrealized_pnl: f64,
    pub total_unrealized_pct: f64,
    pub holdings: Vec<HoldingAnalysis>,
    pub top1_weight_pct: f64,
    pub top3_weight_pct: f64,
    pub top5_weight_pct: f64,
    pub hhi: f64,
    pub hhi_label: String,
    /// Weight-based effective names (`1/Σwᵢ²`) — diversification by *position size*
    /// only. It does NOT see that two names move together, so it overstates real
    /// diversification. Kept as honest context next to `corr_effective_bets`.
    pub effective_names: f64,
    /// Correlation-based effective number of independent bets over the names that
    /// have enough daily history: the participation ratio `N²/Σᵢⱼρᵢⱼ²` of the
    /// daily-return correlation matrix. `None` when fewer than two names have
    /// `≥ CORR_MIN_SESSIONS` aligned sessions. This is the honest "real bets".
    #[serde(default)]
    pub corr_effective_bets: Option<f64>,
    /// Mean off-diagonal daily-return correlation across the names used (context
    /// for the effective-bets number; higher ⇒ more overlap ⇒ fewer real bets).
    #[serde(default)]
    pub corr_avg_pairwise: Option<f64>,
    /// How many names entered the correlation estimate (had enough history).
    #[serde(default)]
    pub corr_names_used: usize,
    /// Names excluded from the correlation estimate (no/short daily archive
    /// history). Surfaced so the figure is never silently partial.
    #[serde(default)]
    pub corr_names_dropped: Vec<String>,
    /// Aligned common sessions the correlation matrix was estimated over.
    #[serde(default)]
    pub corr_sessions: usize,
    /// Honest one-line statement of how the correlation figure was derived.
    #[serde(default)]
    pub corr_basis: String,
    pub by_sector: Vec<ExposureRow>,
    pub by_broker: Vec<ExposureRow>,
    pub clusters: Vec<CorrelationCluster>,
    pub names_with_edge: usize,
    pub names_total: usize,
    pub marks_live: usize,
    pub marks_total: usize,
    pub disclaimer: String,
    #[serde(default)]
    pub built_ist: String,
}

// ---------------------------------------------------------------------------
// Rotation & growth (Page-equivalent for the Desk: leaders / laggards / buys).
// Display-only contracts — descriptive evidence, never advice or an order.
// ---------------------------------------------------------------------------

/// One holding's rotation read. `action` is a descriptive bucket
/// (Leader / Hold / Trim / Rotate out / Hold*), never a directive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationRow {
    pub symbol: String,
    pub weight_pct: f64,
    pub vs_dma200: Option<f64>,
    pub rs_12m: Option<f64>,
    pub off_high_pct: Option<f64>,
    pub trend: String,
    pub edge_eligible: bool,
    pub action: String,
    pub reason: String,
}

/// A screened buy candidate: an eligible-edge name ALSO in a price uptrend that
/// is beating NIFTY. CAGRs are historical/descriptive, not forecasts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuyCandidate {
    pub symbol: String,
    pub last: f64,
    pub vs_dma200: f64,
    pub rs_6m: f64,
    pub rs_12m: f64,
    pub off_high_pct: f64,
    pub cagr_3y: Option<f64>,
    pub cagr_5y: Option<f64>,
    pub edge_strategy: String,
    pub edge_expectancy_r: f64,
    pub edge_profit_factor: f64,
    pub edge_win_pct: f64,
    pub edge_n: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalanceSell {
    pub symbol: String,
    pub action: String,
    pub frac: f64,
    /// Whole shares to sell to realise `frac` of the position (descriptive).
    pub shares: i64,
    pub cash: f64,
    pub realized_gain: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalanceBuy {
    pub symbol: String,
    pub amount: f64,
    pub shares: i64,
    pub edge_strategy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalanceProfile {
    pub uptrend_pct: f64,
    pub sectors: usize,
    pub theme_pct: f64,
    pub top2_pct: f64,
    pub hhi: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebalancePlan {
    pub sells: Vec<RebalanceSell>,
    pub buys: Vec<RebalanceBuy>,
    pub cash_raised: f64,
    pub realized_gain: f64,
    pub ltcg_tax_est: f64,
    pub to_redeploy: f64,
    pub before: RebalanceProfile,
    pub after: RebalanceProfile,
}

/// Portfolio-level forward scenario band — a RANGE, never a point forecast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrowthScenario {
    pub name: String,
    pub cagr_low: f64,
    pub cagr_high: f64,
    pub assumes: String,
}

/// The full rotation/growth payload attached to the holdings response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationAnalysis {
    pub holdings: Vec<RotationRow>,
    pub buy_candidates: Vec<BuyCandidate>,
    pub plan: Option<RebalancePlan>,
    pub scenarios: Vec<GrowthScenario>,
    pub decade_note: String,
    pub disclaimer: String,
    #[serde(default)]
    pub built_ist: String,
}

// ---------------------------------------------------------------------------
// Capital horizon planner — "₹X for N years → which names fit"
// Every figure is HISTORICAL/descriptive, never a forecast. Display-only.
// ---------------------------------------------------------------------------

/// One screened candidate for the capital plan, with its backtest-grounded
/// evidence and a suggested (illustrative) ₹ allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapitalPick {
    pub symbol: String,
    pub sector: String,
    pub last: f64,
    pub alloc_rupees: f64,
    pub shares: i64,
    pub weight_pct: f64,
    /// Trailing annualised return over the horizon — PAST performance, not a forecast.
    pub cagr_pct: f64,
    pub max_dd_pct: f64,
    pub rs_vs_nifty_pct: f64,
    pub consistency_pct: f64,
    pub mcap_cr: f64,
    pub edge_backed: bool,
    pub edge_note: String,
    /// Trailing CAGR is unusually high (likely not repeatable) — flagged honestly.
    pub high_cagr_flag: bool,
    pub note: String,
}

/// A horizon-aware capital deployment screen. NOT advice, NOT a forecast, never
/// an order — candidates + evidence + an illustrative allocation; the user decides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapitalPlan {
    pub horizon_years: u32,
    pub capital: f64,
    pub picks: Vec<CapitalPick>,
    pub deployed: f64,
    pub leftover_cash: f64,
    pub universe_scanned: usize,
    /// False when NIFTY history was unavailable, so relative-strength was omitted
    /// (the per-pick `rs_vs_nifty_pct` are all 0 and must render as "—", not "+0%").
    #[serde(default = "default_true")]
    pub rs_available: bool,
    pub methodology: String,
    pub disclaimer: String,
    #[serde(default)]
    pub built_ist: String,
}

fn default_true() -> bool {
    true
}

/// NIFTY regime + market breadth context (display-only; never changes a score).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeInfo {
    pub nifty_regime: String, // "Up" | "Down" | "Flat"
    pub breadth_up: usize,
    pub breadth_down: usize,
    pub breadth_label: String, // "narrow" | "broad" | "neutral"
    /// When this snapshot was computed (IST "YYYY-MM-DD HH:MM:SS"). Display-only,
    /// so a cached/stale regime is never presented as live. Defaults empty for
    /// back-compat with any older serialized payloads.
    #[serde(default)]
    pub built_ist: String,
}

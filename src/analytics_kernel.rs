//! Per-second microstructure analytics + universe ranking.
//!
//! Maintains a per-symbol live sliding window (1000-tick ring buffer), computes
//! OBI / VWAP-vs-VAH-VAL / rolling z-score / RVOL, detects which eligible
//! strategies are firing, scores each as (backtested edge × live confirmation),
//! and emits ranked candidates for the risk layer to size.
//!
//! Pure, deterministic, allocation-light. Holds no time and no RNG, so the same
//! tick stream always produces the same features and candidates.

use std::collections::HashMap;
use std::collections::VecDeque;

use crate::config::{self, Direction};
use crate::storage_kernel::SymbolBaseline;
use crate::strategy_engine::EdgeIndex;
use crate::types::{Candidate, Diagnostics, EligibleEdge, LiveFeatures, MarketDepth, Tick};

/// Cap on retained ingest-latency samples used for the median diagnostic.
const LATENCY_CAP: usize = 256;
/// Cap on retained per-tick volume deltas used for the RVOL average.
const RVOL_HISTORY_CAP: usize = 256;
/// Depth of the per-tick LTQ / spread history for the microstructure detectors.
const MICRO_HISTORY_CAP: usize = 64;
/// A single tick's traded qty ≥ this multiple of the recent average ⇒ block deal.
const BLOCK_DEAL_MULT: f64 = 5.0;
/// Current spread ≥ this multiple of the recent median ⇒ "widening".
const SPREAD_WIDEN_MULT: f64 = 2.5;
/// Ticks the price must move monotonically (with rising qty) for a sweep.
const SWEEP_TICKS: usize = 6;
/// Floor for the intraday ATR proxy, as a fraction of last price, so stops are
/// never zero even on a flat/illiquid window.
const ATR_FLOOR_PCT: f64 = 0.002;

/// Per-symbol live state: the sliding window plus the running accumulators that
/// feed [`LiveFeatures`]. Private — only the [`Engine`] touches it.
struct LiveState {
    /// Most recent prices, capped at [`config::LIVE_WINDOW`].
    prices: VecDeque<f64>,
    /// Session VWAP numerator: Σ ltp · vol_delta.
    vwap_pv: f64,
    /// Session VWAP denominator: Σ vol_delta.
    vwap_vol: f64,
    /// Last seen cumulative day volume, to derive per-tick volume deltas.
    prev_volume_day: i64,
    /// True once we have an anchor for `prev_volume_day` (first tick seen).
    has_prev_volume: bool,
    /// Recent per-tick volume deltas for RVOL.
    vol_hist: VecDeque<f64>,
    /// Latest full-mode depth, if any.
    depth: Option<MarketDepth>,
    /// Most recent traded price.
    last_price: f64,
    /// True once at least one tick has set `last_price`.
    has_price: bool,
    /// Recent per-tick last-traded-QTY (live only; empty in replay). Block-deal +
    /// tick-sweep detectors.
    ltq_hist: VecDeque<f64>,
    /// Recent best bid/ask spread% snapshots — for the spread-widening detector.
    spread_hist: VecDeque<f64>,
    /// Recent ingest latency samples (µs).
    latency: VecDeque<i64>,
}

impl LiveState {
    fn new() -> LiveState {
        LiveState {
            prices: VecDeque::with_capacity(config::LIVE_WINDOW),
            vwap_pv: 0.0,
            vwap_vol: 0.0,
            prev_volume_day: 0,
            has_prev_volume: false,
            vol_hist: VecDeque::with_capacity(RVOL_HISTORY_CAP),
            depth: None,
            last_price: 0.0,
            has_price: false,
            ltq_hist: VecDeque::with_capacity(MICRO_HISTORY_CAP),
            spread_hist: VecDeque::with_capacity(MICRO_HISTORY_CAP),
            latency: VecDeque::with_capacity(LATENCY_CAP),
        }
    }

    /// Fold one tick into this symbol's state.
    fn update(&mut self, tick: &Tick) {
        // --- per-tick volume delta (non-negative; resets on feed restart) ----
        let vol_delta = if self.has_prev_volume {
            (tick.volume_day - self.prev_volume_day).max(0) as f64
        } else {
            0.0
        };
        self.prev_volume_day = tick.volume_day;
        self.has_prev_volume = true;

        // --- session VWAP accumulators --------------------------------------
        if vol_delta > 0.0 {
            self.vwap_pv += tick.ltp * vol_delta;
            self.vwap_vol += vol_delta;
        }

        // --- RVOL history (only meaningful, non-zero deltas) ----------------
        if vol_delta > 0.0 {
            if self.vol_hist.len() == RVOL_HISTORY_CAP {
                self.vol_hist.pop_front();
            }
            self.vol_hist.push_back(vol_delta);
        }

        // --- price ring buffer ----------------------------------------------
        if self.prices.len() == config::LIVE_WINDOW {
            self.prices.pop_front();
        }
        self.prices.push_back(tick.ltp);
        self.last_price = tick.ltp;
        self.has_price = true;

        // --- last-traded-qty history (live only; replay sends ltq=0) --------
        if tick.ltq > 0 {
            if self.ltq_hist.len() == MICRO_HISTORY_CAP {
                self.ltq_hist.pop_front();
            }
            self.ltq_hist.push_back(tick.ltq as f64);
        }

        // --- depth ----------------------------------------------------------
        if let Some(d) = tick.depth {
            self.depth = Some(d);
        }
        // --- spread history (after depth update) ----------------------------
        let sp = self.spread_pct();
        if sp > 0.0 {
            if self.spread_hist.len() == MICRO_HISTORY_CAP {
                self.spread_hist.pop_front();
            }
            self.spread_hist.push_back(sp);
        }

        // --- latency sample -------------------------------------------------
        if self.latency.len() == LATENCY_CAP {
            self.latency.pop_front();
        }
        self.latency.push_back(tick.latency_us);
    }

    /// Population mean and std-dev of the current price window.
    fn mean_std(&self) -> (f64, f64) {
        let n = self.prices.len();
        if n == 0 {
            return (0.0, 0.0);
        }
        let mean = self.prices.iter().sum::<f64>() / n as f64;
        let var = self
            .prices
            .iter()
            .map(|p| (p - mean).powi(2))
            .sum::<f64>()
            / n as f64;
        (mean, var.sqrt())
    }

    /// Order-book imbalance over the (up to) 5 visible levels, in [-1, 1].
    /// Positive = bid-heavy. 0 when there is no depth or no resting size.
    fn obi(&self) -> f64 {
        match &self.depth {
            Some(d) => {
                let bid: f64 = d.bids.iter().map(|l| l.qty.max(0) as f64).sum();
                let ask: f64 = d.asks.iter().map(|l| l.qty.max(0) as f64).sum();
                let denom = bid + ask;
                if denom > 0.0 {
                    (bid - ask) / denom
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    }

    /// Best bid/ask spread as a percent of mid (0 if no usable depth).
    fn spread_pct(&self) -> f64 {
        match &self.depth {
            Some(d) => {
                let bid = d.bids[0].price;
                let ask = d.asks[0].price;
                let mid = (bid + ask) / 2.0;
                if bid > 0.0 && ask > 0.0 && mid > 0.0 {
                    (ask - bid) / mid * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    }

    /// Relative volume: latest volume delta vs the running average of history.
    /// 1.0 when there is no usable history.
    fn rvol(&self) -> f64 {
        let n = self.vol_hist.len();
        if n == 0 {
            return 1.0;
        }
        let last = *self.vol_hist.back().unwrap();
        let mean = self.vol_hist.iter().sum::<f64>() / n as f64;
        if mean > 0.0 {
            last / mean
        } else {
            1.0
        }
    }

    /// Compute the live microstructure feature bundle for this symbol.
    fn features(&self) -> LiveFeatures {
        let last = self.last_price;
        let vwap = if self.vwap_vol > 0.0 {
            self.vwap_pv / self.vwap_vol
        } else {
            last
        };
        let vwap_dev_pct = if vwap != 0.0 {
            (last - vwap) / vwap * 100.0
        } else {
            0.0
        };
        let (mean, std) = self.mean_std();
        let zscore = if std > 0.0 { (last - mean) / std } else { 0.0 };

        LiveFeatures {
            obi: self.obi(),
            vwap,
            vwap_dev_pct,
            zscore,
            rvol: self.rvol(),
            spread_pct: self.spread_pct(),
            rsi: self.rsi(),
            block_mult: self.block_mult(),
            tick_sweep: self.tick_sweep(),
            spread_widening: self.spread_widening(),
            last_price: last,
        }
    }

    /// Wilder RSI(14) over the live tick price series — a live momentum read.
    /// Returns 50 (neutral) until there are enough ticks. Display-only.
    fn rsi(&self) -> f64 {
        const PERIOD: usize = 14;
        let n = self.prices.len();
        if n < PERIOD + 1 {
            return 50.0;
        }
        let p: Vec<f64> = self.prices.iter().copied().collect();
        let (mut avg_gain, mut avg_loss) = (0.0, 0.0);
        for i in 1..=PERIOD {
            let d = p[i] - p[i - 1];
            if d >= 0.0 {
                avg_gain += d;
            } else {
                avg_loss -= d;
            }
        }
        avg_gain /= PERIOD as f64;
        avg_loss /= PERIOD as f64;
        for i in (PERIOD + 1)..n {
            let d = p[i] - p[i - 1];
            let (g, l) = if d >= 0.0 { (d, 0.0) } else { (0.0, -d) };
            avg_gain = (avg_gain * (PERIOD as f64 - 1.0) + g) / PERIOD as f64;
            avg_loss = (avg_loss * (PERIOD as f64 - 1.0) + l) / PERIOD as f64;
        }
        if avg_loss <= 0.0 {
            return if avg_gain > 0.0 { 100.0 } else { 50.0 };
        }
        let rs = avg_gain / avg_loss;
        100.0 - 100.0 / (1.0 + rs)
    }

    /// Block-deal multiple: the latest tick's traded qty ÷ the recent average
    /// LTQ. 0 when there's no live per-trade qty (replay / quote mode). ≥ ~5×
    /// signals an aggressive block clearing the book. Display-only.
    fn block_mult(&self) -> f64 {
        let n = self.ltq_hist.len();
        if n < 5 {
            return 0.0;
        }
        let last = *self.ltq_hist.back().unwrap();
        // Average of the prior history (exclude the latest tick).
        let prior: f64 = self.ltq_hist.iter().take(n - 1).sum::<f64>() / (n - 1) as f64;
        if prior > 0.0 {
            last / prior
        } else {
            0.0
        }
    }

    /// Tick-sweep: +1 when the last `SWEEP_TICKS` prices rose monotonically AND the
    /// latest traded qty is above the recent average (an institutional buy sweep);
    /// −1 for the mirror sell sweep; 0 otherwise / no live qty. Display-only.
    fn tick_sweep(&self) -> i8 {
        let np = self.prices.len();
        if np <= SWEEP_TICKS || self.ltq_hist.len() < 5 {
            return 0;
        }
        let tail: Vec<f64> = self.prices.iter().rev().take(SWEEP_TICKS + 1).copied().collect();
        // tail[0] = newest. Rising = newest > older at each step.
        let mut all_up = true;
        let mut all_down = true;
        for w in tail.windows(2) {
            if !(w[0] > w[1]) {
                all_up = false;
            }
            if !(w[0] < w[1]) {
                all_down = false;
            }
        }
        let qty_hot = self.block_mult() > 1.0;
        if all_up && qty_hot {
            1
        } else if all_down && qty_hot {
            -1
        } else {
            0
        }
    }

    /// Spread widening: true when the current spread% is ≥ `SPREAD_WIDEN_MULT`× the
    /// recent median (liquidity thinning → violent moves). Needs depth history.
    fn spread_widening(&self) -> bool {
        let n = self.spread_hist.len();
        if n < 8 {
            return false;
        }
        let cur = *self.spread_hist.back().unwrap();
        let mut prior: Vec<f64> = self.spread_hist.iter().take(n - 1).copied().collect();
        prior.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = prior[prior.len() / 2];
        median > 0.0 && cur >= SPREAD_WIDEN_MULT * median
    }

    /// Intraday volatility proxy used as the candidate's ATR: population
    /// std-dev of the window, floored at a small fraction of last price so a
    /// stop distance is never zero.
    fn atr_proxy(&self) -> f64 {
        let (_, std) = self.mean_std();
        let floor = self.last_price.abs() * ATR_FLOOR_PCT;
        std.max(floor)
    }

    /// Median of the retained latency samples (0 when empty).
    fn median_latency(&self) -> i64 {
        if self.latency.is_empty() {
            return 0;
        }
        let mut v: Vec<i64> = self.latency.iter().cloned().collect();
        v.sort_unstable();
        let mid = v.len() / 2;
        if v.len() % 2 == 1 {
            v[mid]
        } else {
            // Even count: mean of the two central samples (integer floor).
            (v[mid - 1] + v[mid]) / 2
        }
    }
}

/// The live analytics engine: owns all per-symbol state and the edge index.
pub struct Engine {
    /// Per-symbol live windows, keyed by symbol.
    states: HashMap<String, LiveState>,
    /// Eligible (positive-expectancy) edges per symbol.
    edges: EdgeIndex,
    universe: usize,
    eligible_edges: usize,
}

impl Engine {
    /// Build the engine over the symbols-with-edges universe.
    pub fn new(
        symbols: &[String],
        _baselines: &HashMap<String, SymbolBaseline>,
        edges: &EdgeIndex,
        eligible_edges: usize,
    ) -> Engine {
        let mut states = HashMap::with_capacity(symbols.len());
        for sym in symbols {
            states.entry(sym.clone()).or_insert_with(LiveState::new);
        }
        Engine {
            states,
            edges: edges.clone(),
            universe: symbols.len(),
            eligible_edges,
        }
    }

    /// Fold one tick into the relevant symbol's live window. Unknown symbols
    /// (not in the tracked universe) are ignored.
    pub fn on_tick(&mut self, tick: &Tick) {
        if let Some(state) = self.states.get_mut(&tick.symbol) {
            state.update(tick);
        }
    }

    /// Transparent live-confirmation multiplier in [0, 2]. Neutral is 1.0;
    /// agreement between order flow / VWAP position and the edge's direction
    /// pushes it up, disagreement pushes it down.
    fn live_score(dir: Direction, f: &LiveFeatures) -> f64 {
        let obi = f.obi.clamp(-1.0, 1.0);
        let dev = (f.vwap_dev_pct / 2.0).clamp(-0.5, 0.5);
        let raw = match dir {
            Direction::Long => 1.0 + obi * 0.5 + dev,
            Direction::Short => 1.0 + (-obi) * 0.5 + (-dev),
        };
        raw.clamp(0.0, 2.0)
    }

    /// Snapshot the currently edge-confirmed candidates (unsized). One candidate
    /// per (symbol, eligible edge) where the symbol has live state and a price.
    pub fn snapshot_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for (symbol, edge_list) in self.edges.iter() {
            let state = match self.states.get(symbol) {
                Some(s) if s.has_price => s,
                _ => continue,
            };
            let features = state.features();
            let atr = state.atr_proxy();
            let last_price = state.last_price;
            for edge in edge_list {
                let EligibleEdge {
                    strategy,
                    direction,
                    expectancy_r,
                    profit_factor,
                    win_pct,
                    n,
                    robustness,
                } = edge;
                let live_score = Self::live_score(*direction, &features);
                // Rank on the James–Stein-shrunk expectancy so small-n lucky edges
                // don't top the list. Raw `expectancy_r` is still carried for
                // display; this only changes ordering, never Confidence/the gate.
                let shrunk_expectancy_r = crate::stats::shrunk_expectancy(
                    *expectancy_r,
                    *n,
                    crate::config::SHRINK_PRIOR_R,
                    crate::config::SHRINK_STRENGTH,
                );
                let score = shrunk_expectancy_r * live_score;
                out.push(Candidate {
                    symbol: symbol.clone(),
                    strategy: strategy.clone(),
                    direction: *direction,
                    expectancy_r: *expectancy_r,
                    shrunk_expectancy_r,
                    profit_factor: *profit_factor,
                    win_pct: *win_pct,
                    n: *n,
                    robustness: robustness.clone(),
                    last_price,
                    atr,
                    features,
                    live_score,
                    score,
                });
            }
        }
        out
    }

    /// Current engine diagnostics. `ingest_latency_us` is the median over the
    /// retained per-symbol latency samples; `tick_to_signal_us`/`ticks_per_sec`
    /// are filled by the caller (the loop that owns wall-clock).
    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            universe: self.universe,
            eligible_edges: self.eligible_edges,
            threads: rayon::current_num_threads(),
            ingest_latency_us: self.median_ingest_latency(),
            io_mbps: 0.0,
            ..Default::default()
        }
    }

    /// Median of all retained latency samples across every tracked symbol.
    fn median_ingest_latency(&self) -> i64 {
        let mut all: Vec<i64> = Vec::new();
        for s in self.states.values() {
            all.extend(s.latency.iter().cloned());
        }
        if all.is_empty() {
            return 0;
        }
        all.sort_unstable();
        let mid = all.len() / 2;
        if all.len() % 2 == 1 {
            all[mid]
        } else {
            (all[mid - 1] + all[mid]) / 2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DepthLevel, MarketDepth};

    fn depth(bid_qtys: [i64; 5], ask_qtys: [i64; 5], best_bid: f64, best_ask: f64) -> MarketDepth {
        let mut d = MarketDepth::default();
        for i in 0..5 {
            d.bids[i] = DepthLevel {
                price: if i == 0 { best_bid } else { best_bid - i as f64 },
                qty: bid_qtys[i],
                orders: 1,
            };
            d.asks[i] = DepthLevel {
                price: if i == 0 { best_ask } else { best_ask + i as f64 },
                qty: ask_qtys[i],
                orders: 1,
            };
        }
        d
    }

    fn tick(symbol: &str, ltp: f64, volume_day: i64, depth: Option<MarketDepth>) -> Tick {
        Tick {
            symbol: symbol.to_string(),
            instrument_token: 1,
            ltp,
            ltq: 0,
            volume_day,
            ts_exchange_us: 0,
            ts_recv_us: 0,
            latency_us: 100,
            depth,
        }
    }

    #[test]
    fn obi_from_handbuilt_depth() {
        let mut st = LiveState::new();
        // bids sum = 300, asks sum = 100 ⇒ obi = (300-100)/400 = 0.5.
        let d = depth([100, 100, 50, 30, 20], [40, 30, 20, 5, 5], 99.0, 100.0);
        st.update(&tick("X", 99.5, 0, Some(d)));
        let obi = st.obi();
        assert!((obi - 0.5).abs() < 1e-9, "obi={obi}");

        // spread = (100-99)/99.5*100.
        let sp = st.spread_pct();
        assert!((sp - (1.0 / 99.5 * 100.0)).abs() < 1e-9, "spread={sp}");

        // No depth ⇒ obi/spread fall to 0.
        let empty = LiveState::new();
        assert_eq!(empty.obi(), 0.0);
        assert_eq!(empty.spread_pct(), 0.0);
    }

    #[test]
    fn vwap_from_volume_deltas() {
        let mut st = LiveState::new();
        // First tick anchors prev_volume_day; vol_delta = 0, so VWAP not yet set.
        st.update(&tick("X", 100.0, 0, None));
        // +100 vol @ 100, then +100 vol @ 102 ⇒ VWAP = (100*100 + 102*100)/200 = 101.
        st.update(&tick("X", 100.0, 100, None));
        st.update(&tick("X", 102.0, 200, None));
        let f = st.features();
        assert!((f.vwap - 101.0).abs() < 1e-9, "vwap={}", f.vwap);
        // last=102, vwap=101 ⇒ dev = (102-101)/101*100.
        let expect_dev = (102.0 - 101.0) / 101.0 * 100.0;
        assert!(
            (f.vwap_dev_pct - expect_dev).abs() < 1e-9,
            "dev={}",
            f.vwap_dev_pct
        );
        // Volume going backwards (feed restart) yields a clamped 0 delta, so the
        // VWAP accumulators are untouched.
        st.update(&tick("X", 99.0, 50, None));
        let f2 = st.features();
        assert!((f2.vwap - 101.0).abs() < 1e-9, "vwap_after_reset={}", f2.vwap);
    }

    #[test]
    fn micro_detectors_block_sweep_and_inert_in_replay() {
        let mk = |p: f64, q: i64, vol: i64| Tick {
            symbol: "X".into(),
            instrument_token: 1,
            ltp: p,
            ltq: q,
            volume_day: vol,
            ts_exchange_us: 0,
            ts_recv_us: 0,
            latency_us: 0,
            depth: None,
        };
        // Live: 8 steady ticks (rising price, qty 100) then one 10× block.
        let mut st = LiveState::new();
        let mut vol = 0;
        for i in 0..8 {
            vol += 100;
            st.update(&mk(100.0 + i as f64, 100, vol));
        }
        assert!((st.block_mult() - 1.0).abs() < 1e-6, "steady ⇒ ~1×, got {}", st.block_mult());
        vol += 100;
        st.update(&mk(108.0, 1000, vol)); // 10× block on a rising print
        assert!(st.block_mult() >= BLOCK_DEAL_MULT, "block_mult {}", st.block_mult());
        assert_eq!(st.tick_sweep(), 1, "rising prices + hot qty ⇒ buy sweep");

        // Replay: ltq=0 throughout ⇒ detectors inert (no fabricated microstructure).
        let mut st2 = LiveState::new();
        for i in 0..10 {
            st2.update(&mk(100.0 + i as f64, 0, (i + 1) * 100));
        }
        assert_eq!(st2.block_mult(), 0.0);
        assert_eq!(st2.tick_sweep(), 0);
    }

    #[test]
    fn live_score_direction_agreement() {
        // Bid-heavy + price above VWAP should reward Long and penalise Short.
        let f = LiveFeatures {
            obi: 1.0,
            vwap: 100.0,
            vwap_dev_pct: 1.0,
            zscore: 0.0,
            rvol: 1.0,
            spread_pct: 0.0,
            rsi: 50.0,
            block_mult: 0.0,
            tick_sweep: 0,
            spread_widening: false,
            last_price: 101.0,
        };
        let long = Engine::live_score(Direction::Long, &f);
        let short = Engine::live_score(Direction::Short, &f);
        // long = 1 + 0.5 + 0.5 = 2.0 (clamped); short = 1 - 0.5 - 0.5 = 0.0.
        assert!((long - 2.0).abs() < 1e-9, "long={long}");
        assert!((short - 0.0).abs() < 1e-9, "short={short}");
    }

    #[test]
    fn atr_proxy_is_floored_and_unknown_symbols_ignored() {
        let mut st = LiveState::new();
        st.update(&tick("X", 200.0, 0, None));
        // Single price ⇒ std 0 ⇒ ATR floored at 0.2% of 200 = 0.4.
        assert!((st.atr_proxy() - 0.4).abs() < 1e-9, "atr={}", st.atr_proxy());

        let symbols = vec!["X".to_string()];
        let mut eng = Engine::new(&symbols, &HashMap::new(), &EdgeIndex::new(), 0);
        // Unknown symbol must be ignored (no panic, no state created).
        eng.on_tick(&tick("UNKNOWN", 50.0, 10, None));
        assert!(eng.snapshot_candidates().is_empty());
        let diag = eng.diagnostics();
        assert_eq!(diag.universe, 1);
    }

    #[test]
    fn snapshot_emits_candidate_for_eligible_edge() {
        let symbols = vec!["X".to_string()];
        let mut edges = EdgeIndex::new();
        edges.insert(
            "X".to_string(),
            vec![EligibleEdge {
                strategy: "vwap_cross".to_string(),
                direction: Direction::Long,
                expectancy_r: 0.5,
                profit_factor: 2.0,
                win_pct: 55.0,
                n: 40,
                robustness: Default::default(),
            }],
        );
        let mut eng = Engine::new(&symbols, &HashMap::new(), &edges, 1);
        // No price yet ⇒ no candidates.
        assert!(eng.snapshot_candidates().is_empty());
        eng.on_tick(&tick("X", 100.0, 0, None));
        let cands = eng.snapshot_candidates();
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.symbol, "X");
        assert_eq!(c.strategy, "vwap_cross");
        assert_eq!(c.direction, Direction::Long);
        // Raw expectancy is preserved for display…
        assert!((c.expectancy_r - 0.5).abs() < 1e-9, "raw exp={}", c.expectancy_r);
        // …but the board ranks on the sample-size-shrunk expectancy: with n=40 and
        // strength 40, shrunk = (40·0.5 + 40·0)/(40+40) = 0.25.
        assert!(
            (c.shrunk_expectancy_r - 0.25).abs() < 1e-9,
            "shrunk exp={}",
            c.shrunk_expectancy_r
        );
        // Neutral features (no depth, last==vwap) ⇒ live_score 1.0, so the ranking
        // score == shrunk expectancy.
        assert!((c.live_score - 1.0).abs() < 1e-9, "live_score={}", c.live_score);
        assert!((c.score - 0.25).abs() < 1e-9, "score={}", c.score);
        assert!(c.atr > 0.0);
    }
}

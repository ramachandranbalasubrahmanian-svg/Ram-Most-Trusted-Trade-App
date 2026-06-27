//! Tick ingestion.
//!
//! (a) Replay simulator over `minute/` parquet (default; no credentials).
//! (b) Live Kite Connect WebSocket client: zero-copy binary tick parser
//!     (LTP / volume / 5-level depth), instruments-dump fetch + cache, and a
//!     local↔exchange latency tracker. Dispatches across threads via crossbeam.
//!
//! CONTRACT STUB — public signatures are frozen; bodies are filled in Phase 3/5.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use crossbeam_channel::Sender;

use crate::config::Timeframe;
use crate::types::Tick;

/// How the replay simulator should run.
pub struct ReplayOptions {
    /// Resolution to stream (e.g. `Min5`/`Min30`).
    pub tf: Timeframe,
    /// How many of the most recent trading days to replay (1 = last session).
    pub days_back: usize,
    /// Pacing: 0.0 = as fast as possible; >0 multiplies real-time speed.
    pub speed: f64,
}

/// Summary of a replay run.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayStats {
    pub ticks: u64,
    pub bars: u64,
    pub elapsed_ms: u128,
}

/// Replay historical bars from the archive as a synthetic tick stream onto `tx`.
/// Stops early when `stop` is set.
pub fn run_replay(
    _root: &Path,
    _symbols: &[String],
    _opts: &ReplayOptions,
    _tx: Sender<Tick>,
    _stop: Arc<AtomicBool>,
) -> Result<ReplayStats> {
    Ok(ReplayStats::default())
}

/// Credentials + subscription list for the live Kite WebSocket.
pub struct LiveConfig {
    pub api_key: String,
    pub access_token: String,
    /// (symbol, instrument_token) pairs to subscribe.
    pub instruments: Vec<(String, u32)>,
}

/// Connect to the Kite WebSocket and stream live ticks onto `tx` until `stop`.
pub async fn run_live(_cfg: LiveConfig, _tx: Sender<Tick>, _stop: Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

/// Decode one Kite binary WebSocket frame into ticks (full/quote/ltp modes).
/// Pure + testable against golden byte fixtures.
pub fn parse_binary_frame(_payload: &[u8]) -> Vec<Tick> {
    Vec::new()
}

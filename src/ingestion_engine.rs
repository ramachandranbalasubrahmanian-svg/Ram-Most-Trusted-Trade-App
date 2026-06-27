//! Tick ingestion.
//!
//! (a) Replay simulator over the parquet archive (default; no credentials). It
//!     interleaves bars across the whole universe slot-by-slot, synthesising a
//!     direction-aware order book per tick so downstream OBI / VWAP / RVOL math
//!     has something realistic to chew on, and paces emission so a session
//!     streams out over ~20–40 s (the analytics layer snapshots once a second).
//! (b) Live Kite Connect WebSocket client: a zero-copy binary tick parser
//!     (LTP / quote / full 5-level depth), a token→symbol map, and a
//!     local↔exchange latency tracker. Reconnects with backoff and honours the
//!     shared stop flag. Secrets are never logged.
//!
//! Everything emits the single normalized [`crate::types::Tick`] onto a
//! crossbeam channel; replay and live ticks are indistinguishable downstream.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use byteorder::{BigEndian, ByteOrder};
use crossbeam_channel::Sender;
// The live client uses tungstenite's *blocking* WebSocket (re-exported through
// tokio-tungstenite) driven on a `spawn_blocking` thread. This keeps the whole
// crate free of the `futures` ecosystem — only `tokio` + `tokio-tungstenite` +
// `std` — while still satisfying the async `run_live` signature.
use tokio_tungstenite::tungstenite::stream::MaybeTlsStream;
use tokio_tungstenite::tungstenite::{self, Error as WsError, Message};

use crate::config::Timeframe;
use crate::storage_kernel::{self, Candle};
use crate::types::{DepthLevel, MarketDepth, Tick};

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// How the replay simulator should run.
pub struct ReplayOptions {
    /// Resolution to stream (e.g. `Min5`/`Min30`).
    pub tf: Timeframe,
    /// How many of the most recent trading days to replay (1 = last session).
    pub days_back: usize,
    /// Pacing: 0.0 = synthetic ~20–40 s/session; >0 multiplies real-time speed.
    pub speed: f64,
}

/// Summary of a replay run.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplayStats {
    pub ticks: u64,
    pub bars: u64,
    pub elapsed_ms: u128,
}

/// A symbol's window of candles plus a synthetic instrument token (derived from
/// the symbol name so replay tokens are stable and collision-resistant enough
/// for a local simulation).
struct ReplaySeries {
    symbol: String,
    token: u32,
    candles: Vec<Candle>,
    /// Running cumulative volume within the current session, indexed lock-step
    /// with `candles` consumption.
    cum_volume: i64,
    /// `day` id of the session the cumulative counter is currently tracking.
    cur_day: Option<u32>,
}

/// Deterministic synthetic instrument token from a symbol name (FNV-1a 32-bit,
/// forced non-zero). Replay never talks to a broker, so any stable id works.
fn synthetic_token(symbol: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in symbol.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash | 1
}

/// Build a direction-aware synthetic top-5 order book around `close`.
///
/// If the bar closed up (`close >= open`) we make the bids heavier than the
/// asks so OBI reads positive; a down bar tilts the book the other way. Levels
/// step ~0.02% away from the close and quantities are scaled from the bar's
/// volume so RVOL-style comparisons stay meaningful.
fn synth_depth(open: f64, close: f64, volume: f64) -> MarketDepth {
    let step = (close.abs() * 0.0002).max(0.01);
    // Base quantity per level: a small slice of the bar's traded volume, floored
    // so even thin bars carry a non-trivial book.
    let base = ((volume / 20.0).round() as i64).max(1);
    let bull = close >= open;
    // Heavier side gets 1.6×, lighter side 0.6× — a clear, bounded imbalance.
    let (bid_w, ask_w) = if bull { (1.6_f64, 0.6_f64) } else { (0.6_f64, 1.6_f64) };

    let mut bids = [DepthLevel::default(); 5];
    let mut asks = [DepthLevel::default(); 5];
    for i in 0..5 {
        let dist = (i as f64) + 1.0;
        // Nearer levels are deeper; taper outward.
        let taper = 1.0 / dist;
        let bid_qty = ((base as f64) * bid_w * taper).round() as i64;
        let ask_qty = ((base as f64) * ask_w * taper).round() as i64;
        bids[i] = DepthLevel {
            price: close - step * dist,
            qty: bid_qty.max(1),
            orders: (dist as i64).max(1),
        };
        asks[i] = DepthLevel {
            price: close + step * dist,
            qty: ask_qty.max(1),
            orders: (dist as i64).max(1),
        };
    }
    MarketDepth { bids, asks }
}

/// Replay historical bars from the archive as a synthetic tick stream onto `tx`.
///
/// Bars are interleaved across the universe: for each successive bar-slot in the
/// recent-days window we sweep every symbol that has a bar at that slot and emit
/// one tick apiece, so the Top-10 rankings evolve as a real session would.
/// Stops early when `stop` is set.
pub fn run_replay(
    root: &Path,
    symbols: &[String],
    opts: &ReplayOptions,
    tx: Sender<Tick>,
    stop: Arc<AtomicBool>,
) -> Result<ReplayStats> {
    let start = Instant::now();
    let conn = storage_kernel::open_conn().context("replay: open duckdb")?;

    // Load + window each symbol's candles to the last `days_back` trading days.
    let days_back = opts.days_back.max(1);
    let mut series: Vec<ReplaySeries> = Vec::with_capacity(symbols.len());
    for sym in symbols {
        let candles = match storage_kernel::load_candles(&conn, root, sym, opts.tf) {
            Ok(c) if !c.is_empty() => c,
            // A missing/empty file must not sink the whole replay.
            _ => continue,
        };
        let max_day = candles.iter().map(|c| c.day).max().unwrap_or(0);
        let lo_day = max_day.saturating_sub((days_back - 1) as u32);
        let windowed: Vec<Candle> = candles.into_iter().filter(|c| c.day >= lo_day).collect();
        if windowed.is_empty() {
            continue;
        }
        series.push(ReplaySeries {
            symbol: sym.clone(),
            token: synthetic_token(sym),
            candles: windowed,
            cum_volume: 0,
            cur_day: None,
        });
    }

    if series.is_empty() {
        return Ok(ReplayStats {
            ticks: 0,
            bars: 0,
            elapsed_ms: start.elapsed().as_millis(),
        });
    }

    // Per-bar synthetic duration (used when speed > 0 for real-time-ish pacing).
    let bar_minutes = opts.tf.minutes().max(1) as u64;
    let real_bar = Duration::from_secs(bar_minutes * 60);

    // Synthetic monotonic IST clock. Anchor at an arbitrary fixed epoch-us so the
    // stream is deterministic; bump by the bar duration each slot so timestamps
    // advance like a real session.
    let mut synth_ts_us: i64 = 1_700_000_000_000_000; // ~2023-11, IST-agnostic anchor
    let bar_us: i64 = (bar_minutes as i64) * 60 * 1_000_000;

    let max_slots = series.iter().map(|s| s.candles.len()).max().unwrap_or(0);
    let mut stats = ReplayStats::default();

    for slot in 0..max_slots {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Sweep the whole universe for this slot.
        for s in series.iter_mut() {
            let Some(bar) = s.candles.get(slot).copied() else {
                continue;
            };

            // Reset the cumulative volume at each new session (day) boundary.
            match s.cur_day {
                Some(d) if d == bar.day => {}
                _ => {
                    s.cum_volume = 0;
                    s.cur_day = Some(bar.day);
                }
            }
            s.cum_volume = s.cum_volume.saturating_add(bar.volume.max(0.0) as i64);

            let ts_exchange_us = synth_ts_us;
            // Small synthetic transport delay (250 µs) so latency is a small,
            // non-negative figure as it would be on a healthy live feed.
            let ts_recv_us = ts_exchange_us + 250;
            let latency_us = ts_recv_us - ts_exchange_us;

            let depth = synth_depth(bar.open, bar.close, bar.volume);

            let tick = Tick {
                symbol: s.symbol.clone(),
                instrument_token: s.token,
                ltp: bar.close,
                volume_day: s.cum_volume,
                ts_exchange_us,
                ts_recv_us,
                latency_us,
                depth: Some(depth),
            };

            // Receiver gone → nothing left to do; end cleanly.
            if tx.send(tick).is_err() {
                stats.elapsed_ms = start.elapsed().as_millis();
                return Ok(stats);
            }
            stats.ticks += 1;
            stats.bars += 1;
        }

        synth_ts_us += bar_us;

        // Pacing.
        if opts.speed == 0.0 {
            // ~15–25 ms per slot. Vary deterministically with the slot index so
            // we don't peg a single sleep value, yielding ~20–40 s per session.
            let jitter = 15 + (slot as u64 % 11); // 15..=25
            std::thread::sleep(Duration::from_millis(jitter));
        } else {
            let scaled = real_bar.as_secs_f64() / opts.speed;
            if scaled > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(scaled));
            }
        }
    }

    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Live Kite WebSocket
// ---------------------------------------------------------------------------

/// Credentials + subscription list for the live Kite WebSocket.
pub struct LiveConfig {
    pub api_key: String,
    pub access_token: String,
    /// (symbol, instrument_token) pairs to subscribe.
    pub instruments: Vec<(String, u32)>,
}

/// Current epoch in microseconds (local wall clock).
fn now_epoch_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Connect to the Kite WebSocket and stream live ticks onto `tx` until `stop`.
///
/// Reconnects with capped exponential backoff on any transport error; returns
/// `Ok(())` on a clean stop. The API key and access token are used only to build
/// the connection URL and the subscribe frames — they are never logged.
///
/// The blocking WebSocket I/O runs on a `spawn_blocking` thread so the rest of
/// the runtime stays async; we communicate state purely through the shared
/// `stop` flag and the crossbeam `tx`, both of which are `Send`.
pub async fn run_live(cfg: LiveConfig, tx: Sender<Tick>, stop: Arc<AtomicBool>) -> Result<()> {
    let handle = tokio::task::spawn_blocking(move || live_blocking(cfg, tx, stop));
    // Propagate the inner result; a join error (panic) becomes an anyhow error.
    match handle.await {
        Ok(inner) => inner,
        Err(join_err) => Err(anyhow::anyhow!("live ingestion task failed: {join_err}")),
    }
}

/// Map a [`tungstenite::Error`] to "treat as idle" (true) vs "real disconnect"
/// (false). A read timeout we deliberately set on the socket surfaces as a
/// `WouldBlock`/`TimedOut` IO error and just means "no data this interval".
fn is_idle_timeout(err: &WsError) -> bool {
    if let WsError::Io(io) = err {
        matches!(
            io.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

/// Apply a short read timeout to the underlying TCP socket so blocking `read()`
/// calls return periodically, letting us poll the stop flag on an idle feed.
fn set_read_timeout(stream: &mut MaybeTlsStream<std::net::TcpStream>, dur: Duration) {
    // `MaybeTlsStream` is `#[non_exhaustive]`; with no TLS feature enabled only
    // the `Plain` arm exists, but the wildcard keeps this robust if a TLS
    // variant is ever compiled in.
    #[allow(unreachable_patterns)]
    match stream {
        MaybeTlsStream::Plain(tcp) => {
            let _ = tcp.set_read_timeout(Some(dur));
        }
        _ => {}
    }
}

/// The synchronous connect → subscribe → read loop. Runs on a blocking thread.
fn live_blocking(cfg: LiveConfig, tx: Sender<Tick>, stop: Arc<AtomicBool>) -> Result<()> {
    let tokens: Vec<u32> = cfg.instruments.iter().map(|(_, t)| *t).collect();
    let token_to_symbol: HashMap<u32, String> =
        cfg.instruments.iter().map(|(s, t)| (*t, s.clone())).collect();

    // Subscribe + full-mode frames (built once; tokens are stable per session).
    let subscribe_msg = format!(r#"{{"a":"subscribe","v":{}}}"#, json_u32_array(&tokens));
    let mode_msg = format!(r#"{{"a":"mode","v":["full",{}]}}"#, json_u32_array(&tokens));

    // Secret-bearing connection URL — built here, never logged.
    let url = format!(
        "wss://ws.kite.trade?api_key={}&access_token={}",
        cfg.api_key, cfg.access_token
    );

    let mut backoff_ms: u64 = 500;
    const BACKOFF_MAX_MS: u64 = 30_000;
    const READ_TIMEOUT: Duration = Duration::from_millis(500);

    while !stop.load(Ordering::Relaxed) {
        tracing::info!(instruments = tokens.len(), "kite ws: connecting");

        match tungstenite::connect(&url) {
            Ok((mut ws, _resp)) => {
                tracing::info!("kite ws: connected; subscribing");
                backoff_ms = 500; // reset on a successful connect
                set_read_timeout(ws.get_mut(), READ_TIMEOUT);

                // Subscribe, then switch the subscribed tokens to full mode.
                let subscribed = ws
                    .send(Message::text(subscribe_msg.clone()))
                    .and_then(|_| ws.send(Message::text(mode_msg.clone())));
                if let Err(e) = subscribed {
                    tracing::warn!(error = %e, "kite ws: subscribe failed");
                    // fall through to backoff + reconnect
                } else {
                    // Read loop.
                    loop {
                        if stop.load(Ordering::Relaxed) {
                            tracing::info!("kite ws: stop requested; closing");
                            let _ = ws.close(None);
                            return Ok(());
                        }

                        match ws.read() {
                            Ok(Message::Binary(buf)) => {
                                let recv_us = now_epoch_us();
                                let mut ticks = parse_binary_frame(buf.as_ref());
                                for t in ticks.iter_mut() {
                                    if let Some(sym) = token_to_symbol.get(&t.instrument_token) {
                                        t.symbol = sym.clone();
                                    }
                                    t.ts_recv_us = recv_us;
                                    t.latency_us = if t.ts_exchange_us > 0 {
                                        recv_us - t.ts_exchange_us
                                    } else {
                                        0
                                    };
                                    if tx.send(t.clone()).is_err() {
                                        // Receiver gone — nothing more to do.
                                        let _ = ws.close(None);
                                        return Ok(());
                                    }
                                }
                            }
                            // Kite heartbeats arrive as 1-byte messages; keep the
                            // socket alive by answering pings.
                            Ok(Message::Ping(payload)) => {
                                let _ = ws.send(Message::Pong(payload));
                            }
                            Ok(Message::Pong(_)) | Ok(Message::Text(_)) => {
                                // Heartbeat / JSON status — nothing to emit.
                            }
                            Ok(Message::Close(_)) => {
                                tracing::info!("kite ws: server closed connection");
                                break;
                            }
                            Ok(Message::Frame(_)) => { /* raw frame — never surfaced */ }
                            Err(e) if is_idle_timeout(&e) => {
                                // No data this interval — loop back to re-check stop.
                                continue;
                            }
                            Err(WsError::ConnectionClosed) | Err(WsError::AlreadyClosed) => {
                                tracing::info!("kite ws: connection closed");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "kite ws: read error");
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "kite ws: connect failed");
            }
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Backoff before reconnecting, staying responsive to the stop flag.
        tracing::info!(backoff_ms, "kite ws: reconnecting after backoff");
        let mut waited = 0_u64;
        while waited < backoff_ms {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let chunk = (backoff_ms - waited).min(200);
            std::thread::sleep(Duration::from_millis(chunk));
            waited += chunk;
        }
        backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
    }

    Ok(())
}

/// Render a `&[u32]` as a compact JSON array string, e.g. `[123,456]`.
fn json_u32_array(tokens: &[u32]) -> String {
    let mut s = String::with_capacity(2 + tokens.len() * 8);
    s.push('[');
    for (i, t) in tokens.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&t.to_string());
    }
    s.push(']');
    s
}

// ---------------------------------------------------------------------------
// Binary frame parser (Kite Connect protocol)
// ---------------------------------------------------------------------------

/// NSE equity prices arrive as paise-scaled integers; divide by 100 for rupees.
const PRICE_DIVISOR: f64 = 100.0;

/// Decode one Kite binary WebSocket frame into ticks (full / quote / ltp modes).
///
/// Frame layout (all integers big-endian):
///   * `u16` number of packets, then for each packet a `u16` length followed by
///     that many bytes.
///
/// Packet shapes are distinguished by length:
///   * 8   → LTP mode:   `i32 token`, `i32 ltp`.
///   * 44  → quote mode: token, ltp, last_qty, avg_price, volume, buy_qty,
///                       sell_qty, open, high, low, close (all `i32`).
///   * ≥184 → full mode: the quote fields, then at byte offset 60 a `u32`
///                       exchange timestamp (epoch seconds), then market depth:
///                       5 bids then 5 asks, each entry 12 bytes =
///                       `i32 qty`, `i32 price`, `u16 orders`, `u16 pad`.
///
/// Pure + robust: a truncated buffer yields whatever whole packets could be read
/// and silently skips the rest. `symbol` is left empty for the caller to map.
pub fn parse_binary_frame(payload: &[u8]) -> Vec<Tick> {
    let mut out = Vec::new();
    if payload.len() < 2 {
        return out;
    }

    let num_packets = BigEndian::read_u16(&payload[0..2]) as usize;
    let mut off = 2usize;

    for _ in 0..num_packets {
        // Need at least the 2-byte length prefix.
        if off + 2 > payload.len() {
            break;
        }
        let plen = BigEndian::read_u16(&payload[off..off + 2]) as usize;
        off += 2;

        // Truncated packet — stop; partial bytes are unusable.
        if off + plen > payload.len() {
            break;
        }
        let pkt = &payload[off..off + plen];
        off += plen;

        if let Some(tick) = parse_packet(pkt) {
            out.push(tick);
        }
    }

    out
}

/// Parse a single packet body (without its length prefix) into a [`Tick`].
/// Returns `None` for shapes too short to carry even an LTP.
fn parse_packet(pkt: &[u8]) -> Option<Tick> {
    let len = pkt.len();
    if len < 8 {
        return None;
    }

    let token = BigEndian::read_i32(&pkt[0..4]) as u32;
    let ltp_raw = BigEndian::read_i32(&pkt[4..8]);
    let ltp = ltp_raw as f64 / PRICE_DIVISOR;

    // Defaults for LTP-only packets.
    let mut volume_day: i64 = 0;
    let mut ts_exchange_us: i64 = 0;
    let mut depth: Option<MarketDepth> = None;

    // Quote/full carry cumulative volume at offset 16..20.
    if len >= 44 {
        let volume = BigEndian::read_i32(&pkt[16..20]);
        volume_day = volume as i64;
    }

    // Full mode: exchange timestamp + 5+5 depth levels.
    if len >= 184 {
        // u32 epoch-seconds at offset 60.
        let ts_secs = BigEndian::read_u32(&pkt[60..64]) as i64;
        if ts_secs > 0 {
            ts_exchange_us = ts_secs * 1_000_000;
        }

        // Depth begins right after the timestamp at offset 64.
        // 10 entries × 12 bytes = 120 bytes → ends at 184.
        let mut bids = [DepthLevel::default(); 5];
        let mut asks = [DepthLevel::default(); 5];
        let depth_start = 64usize;
        let mut entry_off = depth_start;
        let mut ok = true;
        for i in 0..10 {
            if entry_off + 12 > len {
                ok = false;
                break;
            }
            let qty = BigEndian::read_i32(&pkt[entry_off..entry_off + 4]) as i64;
            let price_raw = BigEndian::read_i32(&pkt[entry_off + 4..entry_off + 8]);
            let orders = BigEndian::read_u16(&pkt[entry_off + 8..entry_off + 10]) as i64;
            // bytes [10..12] are padding.
            let level = DepthLevel {
                price: price_raw as f64 / PRICE_DIVISOR,
                qty,
                orders,
            };
            if i < 5 {
                bids[i] = level;
            } else {
                asks[i - 5] = level;
            }
            entry_off += 12;
        }
        if ok {
            depth = Some(MarketDepth { bids, asks });
        }
    }

    Some(Tick {
        symbol: String::new(),
        instrument_token: token,
        ltp,
        volume_day,
        ts_exchange_us,
        ts_recv_us: 0,
        latency_us: 0,
        depth,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::WriteBytesExt;
    use std::io::Write;

    /// Append a 12-byte depth entry (qty, price-paise, orders, pad) big-endian.
    fn push_depth_entry(buf: &mut Vec<u8>, qty: i32, price_paise: i32, orders: u16) {
        buf.write_i32::<BigEndian>(qty).unwrap();
        buf.write_i32::<BigEndian>(price_paise).unwrap();
        buf.write_u16::<BigEndian>(orders).unwrap();
        buf.write_u16::<BigEndian>(0).unwrap(); // pad
    }

    /// Build one full-mode packet body (184 bytes) with known fields.
    fn build_full_packet() -> Vec<u8> {
        let mut p = Vec::with_capacity(184);
        p.write_i32::<BigEndian>(408065).unwrap(); // token  (offset 0)
        p.write_i32::<BigEndian>(150_25).unwrap(); // ltp = 150.25 (offset 4)
        p.write_i32::<BigEndian>(5).unwrap(); // last_qty   (8)
        p.write_i32::<BigEndian>(150_00).unwrap(); // avg_price (12)
        p.write_i32::<BigEndian>(123_456).unwrap(); // volume   (16)
        p.write_i32::<BigEndian>(1000).unwrap(); // buy_qty     (20)
        p.write_i32::<BigEndian>(900).unwrap(); // sell_qty     (24)
        p.write_i32::<BigEndian>(149_00).unwrap(); // open      (28)
        p.write_i32::<BigEndian>(151_00).unwrap(); // high      (32)
        p.write_i32::<BigEndian>(148_50).unwrap(); // low       (36)
        p.write_i32::<BigEndian>(149_50).unwrap(); // close     (40)
        // pad out to offset 60 (timestamp). Currently at 44.
        while p.len() < 60 {
            p.write_u8(0).unwrap();
        }
        p.write_u32::<BigEndian>(1_700_000_000).unwrap(); // ts secs (60)
        // Depth: 5 bids (descending price), 5 asks (ascending price).
        push_depth_entry(&mut p, 100, 150_20, 3); // best bid
        push_depth_entry(&mut p, 90, 150_15, 2);
        push_depth_entry(&mut p, 80, 150_10, 2);
        push_depth_entry(&mut p, 70, 150_05, 1);
        push_depth_entry(&mut p, 60, 150_00, 1);
        push_depth_entry(&mut p, 110, 150_30, 4); // best ask
        push_depth_entry(&mut p, 95, 150_35, 2);
        push_depth_entry(&mut p, 85, 150_40, 2);
        push_depth_entry(&mut p, 75, 150_45, 1);
        push_depth_entry(&mut p, 65, 150_50, 1);
        assert_eq!(p.len(), 184, "full packet must be exactly 184 bytes");
        p
    }

    #[test]
    fn parse_full_frame_golden() {
        let body = build_full_packet();
        // Wrap in a frame: u16 num_packets=1, then u16 len, then body.
        let mut frame = Vec::new();
        frame.write_u16::<BigEndian>(1).unwrap();
        frame.write_u16::<BigEndian>(body.len() as u16).unwrap();
        frame.write_all(&body).unwrap();

        let ticks = parse_binary_frame(&frame);
        assert_eq!(ticks.len(), 1, "exactly one tick expected");
        let t = &ticks[0];

        assert_eq!(t.instrument_token, 408065);
        assert!((t.ltp - 150.25).abs() < 1e-9, "ltp={}", t.ltp);
        assert_eq!(t.volume_day, 123_456);
        assert_eq!(t.ts_exchange_us, 1_700_000_000 * 1_000_000);
        assert!(t.symbol.is_empty(), "symbol mapped by caller");

        let depth = t.depth.as_ref().expect("full mode carries depth");
        // Top-of-book bid.
        assert!((depth.bids[0].price - 150.20).abs() < 1e-9);
        assert_eq!(depth.bids[0].qty, 100);
        assert_eq!(depth.bids[0].orders, 3);
        // Top-of-book ask.
        assert!((depth.asks[0].price - 150.30).abs() < 1e-9);
        assert_eq!(depth.asks[0].qty, 110);
        assert_eq!(depth.asks[0].orders, 4);
        // Deepest levels parsed too.
        assert!((depth.bids[4].price - 150.00).abs() < 1e-9);
        assert!((depth.asks[4].price - 150.50).abs() < 1e-9);
    }

    #[test]
    fn parse_ltp_and_quote_packets() {
        // LTP-only packet (8 bytes).
        let mut ltp = Vec::new();
        ltp.write_i32::<BigEndian>(111).unwrap();
        ltp.write_i32::<BigEndian>(99_99).unwrap();
        let mut frame = Vec::new();
        frame.write_u16::<BigEndian>(1).unwrap();
        frame.write_u16::<BigEndian>(ltp.len() as u16).unwrap();
        frame.write_all(&ltp).unwrap();
        let ticks = parse_binary_frame(&frame);
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].instrument_token, 111);
        assert!((ticks[0].ltp - 99.99).abs() < 1e-9);
        assert!(ticks[0].depth.is_none());
        assert_eq!(ticks[0].volume_day, 0);
    }

    #[test]
    fn truncated_buffers_are_safe() {
        // Claims 2 packets but only carries one short one.
        let mut frame = Vec::new();
        frame.write_u16::<BigEndian>(2).unwrap();
        frame.write_u16::<BigEndian>(8).unwrap();
        frame.write_i32::<BigEndian>(7).unwrap();
        frame.write_i32::<BigEndian>(100_00).unwrap();
        // Second packet declared length 44 but no bytes follow.
        frame.write_u16::<BigEndian>(44).unwrap();
        let ticks = parse_binary_frame(&frame);
        assert_eq!(ticks.len(), 1, "only the complete packet is decoded");
        assert_eq!(ticks[0].instrument_token, 7);

        // Empty / sub-header buffers never panic.
        assert!(parse_binary_frame(&[]).is_empty());
        assert!(parse_binary_frame(&[0x00]).is_empty());
    }

    #[test]
    fn synthetic_token_is_stable_and_nonzero() {
        assert_ne!(synthetic_token("RELIANCE"), 0);
        assert_eq!(synthetic_token("INFY"), synthetic_token("INFY"));
        assert_ne!(synthetic_token("INFY"), synthetic_token("TCS"));
    }

    #[test]
    fn synth_depth_reflects_bar_direction() {
        // Bullish bar → bids heavier than asks (positive OBI).
        let up = synth_depth(100.0, 101.0, 10_000.0);
        let bid_sum: i64 = up.bids.iter().map(|d| d.qty).sum();
        let ask_sum: i64 = up.asks.iter().map(|d| d.qty).sum();
        assert!(bid_sum > ask_sum, "bull bar should be bid-heavy");

        // Bearish bar → asks heavier (negative OBI).
        let down = synth_depth(100.0, 99.0, 10_000.0);
        let bid_sum2: i64 = down.bids.iter().map(|d| d.qty).sum();
        let ask_sum2: i64 = down.asks.iter().map(|d| d.qty).sum();
        assert!(ask_sum2 > bid_sum2, "bear bar should be ask-heavy");
    }
}

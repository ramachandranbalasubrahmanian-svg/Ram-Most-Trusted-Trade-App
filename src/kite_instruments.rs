//! Zerodha Kite instrument-mapping (NSE cash equity).
//!
//! The live WebSocket feed subscribes by **integer `instrument_token`**, never by
//! string ticker. This module owns the daily pre-market job that fetches Kite's
//! public instruments dump (`https://api.kite.trade/instruments` — no auth, no
//! secrets), filters it to NSE cash equity, and builds a `tradingsymbol ↔
//! instrument_token` map. The selected universe is capped at
//! [`config::live_universe_max`] (default 1600) and intersected with the local
//! parquet archive, so we only ever subscribe to names we actually analyse.
//!
//! Honesty / safety:
//! - The instruments dump is **public** — this module never touches the API key,
//!   secret, or access token, and never logs them.
//! - It is **advisory only**: it produces a mapping. It never places an order.
//! - Token counts stay under Kite's ~3000-tokens-per-connection cap via the
//!   universe cap, so a Full-mode subscription won't be silently truncated.
//! - Volatile values (tokens, lot sizes) come from the live dump — never
//!   hardcoded (the project's no-hardcode invariant).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Public Kite instruments master dump (CSV, no authentication required).
pub const INSTRUMENTS_URL: &str = "https://api.kite.trade/instruments";

/// One NSE cash-equity instrument, projected from the Kite dump.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Instrument {
    pub instrument_token: u32,
    pub tradingsymbol: String,
    pub name: String,
    pub lot_size: u32,
    pub tick_size: f64,
}

/// Bidirectional symbol ↔ token map plus per-token metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstrumentMap {
    pub by_symbol: HashMap<String, u32>,
    pub by_token: HashMap<u32, String>,
    pub meta: HashMap<u32, Instrument>,
}

impl InstrumentMap {
    pub fn len(&self) -> usize {
        self.by_symbol.len()
    }
    #[allow(dead_code)] // public API; exercised by tests
    pub fn is_empty(&self) -> bool {
        self.by_symbol.is_empty()
    }
    pub fn token_of(&self, symbol: &str) -> Option<u32> {
        self.by_symbol.get(symbol).copied()
    }
    #[allow(dead_code)] // public API (token→symbol for live decode); exercised by tests
    pub fn symbol_of(&self, token: u32) -> Option<&str> {
        self.by_token.get(&token).map(String::as_str)
    }
}

/// Parse the Kite instruments CSV, keeping only **NSE cash-equity** rows
/// (`exchange == "NSE"` and `instrument_type == "EQ"`). This excludes indices,
/// futures, options, and other segments. Robust to commas inside the `name`
/// field (proper CSV parsing, not a naive split).
pub fn parse_nse_equity(csv_text: &str) -> Result<Vec<Instrument>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(csv_text.as_bytes());

    // Resolve columns by header name so we don't depend on column order.
    let headers = rdr.headers().context("instruments CSV has no header")?.clone();
    let col = |name: &str| headers.iter().position(|h| h.eq_ignore_ascii_case(name));
    let (c_tok, c_sym, c_exch, c_type) = (
        col("instrument_token").context("missing instrument_token column")?,
        col("tradingsymbol").context("missing tradingsymbol column")?,
        col("exchange").context("missing exchange column")?,
        col("instrument_type").context("missing instrument_type column")?,
    );
    let c_name = col("name");
    let c_lot = col("lot_size");
    let c_tick = col("tick_size");

    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => continue, // skip a malformed line rather than abort the whole dump
        };
        let get = |i: usize| rec.get(i).unwrap_or("").trim();
        if get(c_exch) != "NSE" || get(c_type) != "EQ" {
            continue;
        }
        let token: u32 = match get(c_tok).parse() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let sym = get(c_sym).to_string();
        if sym.is_empty() {
            continue;
        }
        out.push(Instrument {
            instrument_token: token,
            tradingsymbol: sym,
            name: c_name.map(|i| get(i).to_string()).unwrap_or_default(),
            lot_size: c_lot.and_then(|i| get(i).parse().ok()).unwrap_or(1),
            tick_size: c_tick.and_then(|i| get(i).parse().ok()).unwrap_or(0.05),
        });
    }
    Ok(out)
}

/// Build the bidirectional map from parsed instruments. On a duplicate trading
/// symbol (rare), the first occurrence wins (deterministic).
pub fn build_map(instruments: Vec<Instrument>) -> InstrumentMap {
    let mut m = InstrumentMap::default();
    for ins in instruments {
        m.by_symbol.entry(ins.tradingsymbol.clone()).or_insert(ins.instrument_token);
        m.by_token.entry(ins.instrument_token).or_insert_with(|| ins.tradingsymbol.clone());
        m.meta.entry(ins.instrument_token).or_insert(ins);
    }
    m
}

/// Select the live-subscription universe: NSE-equity symbols that ALSO exist in
/// the local parquet archive (so every subscribed token is one we can analyse),
/// sorted for determinism and capped at `cap`. Returns `(tradingsymbol, token)`
/// pairs ready for a Full-mode subscription.
///
/// Intersecting with the archive is the safe default membership rule; a
/// liquidity-ranked Top-N (by turnover/volume) is a follow-up the owner can opt
/// into without changing this signature.
pub fn select_universe(map: &InstrumentMap, archive_symbols: &[String], cap: usize) -> Vec<(String, u32)> {
    let mut pairs: Vec<(String, u32)> = archive_symbols
        .iter()
        .filter_map(|s| map.token_of(s).map(|t| (s.clone(), t)))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs.truncate(cap);
    pairs
}

/// Disk cache path for the parsed map, keyed by IST trading date so it refreshes
/// once per day. `ist_date` is "YYYY-MM-DD".
pub fn cache_path(cache_dir: &Path, ist_date: &str) -> PathBuf {
    cache_dir.join(format!("nse_instruments_{ist_date}.json"))
}

/// Load a same-day cached map if present.
pub fn load_cached(cache_dir: &Path, ist_date: &str) -> Option<InstrumentMap> {
    let bytes = std::fs::read(cache_path(cache_dir, ist_date)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the map for the day.
pub fn save_cached(cache_dir: &Path, ist_date: &str, map: &InstrumentMap) -> Result<()> {
    std::fs::create_dir_all(cache_dir).ok();
    let json = serde_json::to_vec(map)?;
    std::fs::write(cache_path(cache_dir, ist_date), json)
        .with_context(|| format!("write instruments cache for {ist_date}"))?;
    Ok(())
}

/// Fetch the public Kite instruments dump (CSV). No auth; no secrets touched.
pub async fn fetch_instruments_csv() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build http client")?;
    let resp = client
        .get(INSTRUMENTS_URL)
        .send()
        .await
        .context("GET instruments dump")?;
    if !resp.status().is_success() {
        anyhow::bail!("instruments dump HTTP {}", resp.status());
    }
    resp.text().await.context("read instruments body")
}

/// Daily pre-market entry point: same-day cache → else fetch, parse, map, cache.
/// `ist_date` is "YYYY-MM-DD". The network fetch only runs on a cache miss.
pub async fn load_or_refresh(cache_dir: &Path, ist_date: &str) -> Result<InstrumentMap> {
    if let Some(m) = load_cached(cache_dir, ist_date) {
        return Ok(m);
    }
    let csv_text = fetch_instruments_csv().await?;
    let instruments = parse_nse_equity(&csv_text)?;
    let map = build_map(instruments);
    let _ = save_cached(cache_dir, ist_date, &map);
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small fixture mirroring the Kite dump header + a few rows, including:
    // a normal NSE-EQ row, an NSE-EQ row whose `name` CONTAINS A COMMA (the case
    // a naive split would corrupt), an NSE index (instrument_type empty → drop),
    // and a BSE-EQ row (wrong exchange → drop).
    const FIXTURE: &str = "instrument_token,exchange_token,tradingsymbol,name,last_price,expiry,strike,tick_size,lot_size,instrument_type,segment,exchange\n\
738561,2885,RELIANCE,RELIANCE INDUSTRIES,0,,0,0.05,1,EQ,NSE,NSE\n\
408065,1594,INFY,\"INFOSYS LTD, INDIA\",0,,0,0.05,1,EQ,NSE,NSE\n\
256265,1001,NIFTY 50,NIFTY 50,0,,0,0.05,0,,INDICES,NSE\n\
136442884,533155,RELIANCE,RELIANCE INDUSTRIES,0,,0,0.05,1,EQ,BSE,BSE\n";

    #[test]
    fn parse_keeps_only_nse_equity_and_handles_commas_in_name() {
        let ins = parse_nse_equity(FIXTURE).unwrap();
        // Only the two NSE-EQ rows survive (index + BSE dropped).
        assert_eq!(ins.len(), 2);
        let infy = ins.iter().find(|i| i.tradingsymbol == "INFY").unwrap();
        assert_eq!(infy.instrument_token, 408065);
        // The comma inside the quoted name must be preserved, not split.
        assert_eq!(infy.name, "INFOSYS LTD, INDIA");
        let rel = ins.iter().find(|i| i.tradingsymbol == "RELIANCE").unwrap();
        assert_eq!(rel.instrument_token, 738561); // the NSE token, not the BSE one
    }

    #[test]
    fn build_map_is_bidirectional() {
        let map = build_map(parse_nse_equity(FIXTURE).unwrap());
        assert_eq!(map.token_of("RELIANCE"), Some(738561));
        assert_eq!(map.symbol_of(408065), Some("INFY"));
        assert_eq!(map.len(), 2);
        assert!(map.token_of("NIFTY 50").is_none()); // index excluded
    }

    #[test]
    fn select_universe_intersects_archive_sorts_and_caps() {
        let map = build_map(parse_nse_equity(FIXTURE).unwrap());
        // Archive has RELIANCE + a name not in the dump; only the intersection maps.
        let archive = vec!["RELIANCE".to_string(), "INFY".to_string(), "NOTLISTED".to_string()];
        let uni = select_universe(&map, &archive, 100);
        assert_eq!(uni, vec![("INFY".to_string(), 408065), ("RELIANCE".to_string(), 738561)]);
        // Cap is honoured.
        let capped = select_universe(&map, &archive, 1);
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].0, "INFY"); // first after sort
    }
}

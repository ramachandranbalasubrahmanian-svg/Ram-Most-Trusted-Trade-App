//! On-demand Kite REST quote (last price + day OHLCV + 5-level market depth) for
//! the Live Integration page. SEPARATE from the websocket tick feed
//! (`ingestion_engine`) — this is a per-symbol pull a dashboard page polls every
//! few seconds, never the hot tick path.
//!
//! HONESTY / FIREWALL:
//!   - Display/advisory only. NEVER feeds Confidence, the edge map, sizing, or any
//!     order. It is a read-only market-data fetch.
//!   - The api_key + access_token are read from `.kite_token.json` (mode 600) and
//!     used ONLY to build the Authorization header. They are never logged or
//!     returned to the browser.
//!   - Kite REST `/quote` returns prices in RUPEES (floats), unlike the websocket
//!     binary protocol which uses paise — so NO ×100 conversion here.
//!   - Market depth is populated only during the continuous session (09:15–15:30
//!     IST). Outside it, Kite returns zeroed levels → `depth = None`,
//!     `depth_available = false`, and the page shows an honest "market closed".

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::types::{DepthLevel, MarketDepth};

const QUOTE_URL: &str = "https://api.kite.trade/quote";

/// A flattened, display-ready live quote for one NSE symbol.
#[derive(Debug, Clone, Serialize)]
pub struct KiteQuote {
    pub symbol: String,
    pub last_price: f64,
    pub volume: i64,
    /// Total pending BUY quantity across the WHOLE NSE order book (every broker
    /// routes here), not just the visible 5 levels. From Kite's `buy_quantity`.
    pub total_buy_qty: i64,
    /// Total pending SELL quantity across the whole book (all brokers).
    pub total_sell_qty: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    /// Previous session close (Kite's `ohlc.close`).
    pub prev_close: f64,
    pub net_change: f64,
    pub lower_circuit: f64,
    pub upper_circuit: f64,
    /// Exchange-reported timestamps (strings as Kite sends them), if present.
    pub exchange_timestamp: Option<String>,
    pub last_trade_time: Option<String>,
    /// 5×5 depth — `None` when the book is empty (outside the live session).
    pub depth: Option<MarketDepth>,
    pub depth_available: bool,
}

// --- Kite JSON response shapes (deserialize only the fields we use) -----------

#[derive(serde::Deserialize)]
struct QuoteResp {
    data: std::collections::HashMap<String, QuoteData>,
}

#[derive(serde::Deserialize)]
struct QuoteData {
    #[serde(default)]
    last_price: f64,
    #[serde(default)]
    volume: i64,
    #[serde(default)]
    buy_quantity: i64,
    #[serde(default)]
    sell_quantity: i64,
    #[serde(default)]
    net_change: f64,
    #[serde(default)]
    lower_circuit_limit: f64,
    #[serde(default)]
    upper_circuit_limit: f64,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    last_trade_time: Option<String>,
    #[serde(default)]
    ohlc: Ohlc,
    #[serde(default)]
    depth: DepthResp,
}

#[derive(serde::Deserialize, Default)]
struct Ohlc {
    #[serde(default)]
    open: f64,
    #[serde(default)]
    high: f64,
    #[serde(default)]
    low: f64,
    #[serde(default)]
    close: f64,
}

#[derive(serde::Deserialize, Default)]
struct DepthResp {
    #[serde(default)]
    buy: Vec<DepthEntry>,
    #[serde(default)]
    sell: Vec<DepthEntry>,
}

#[derive(serde::Deserialize, Default, Clone, Copy)]
struct DepthEntry {
    #[serde(default)]
    price: f64,
    #[serde(default)]
    quantity: i64,
    #[serde(default)]
    orders: i64,
}

/// Read `api_key` + `access_token` (+ token date) from `.kite_token.json` under
/// `root`. Never logged. Errors with a user-facing message when the token is
/// missing/blank so the page can say "log in on /kite".
pub fn read_token(root: &Path) -> Result<(String, String, String)> {
    let path = root.join(".kite_token.json");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read kite token at {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse .kite_token.json")?;
    let api_key = v.get("api_key").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let access_token = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let date = v.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if api_key.is_empty() || access_token.is_empty() {
        anyhow::bail!("kite token not configured — log in on the /kite page");
    }
    Ok((api_key, access_token, date))
}

/// Fetch a live quote + depth for one NSE symbol via Kite REST. `nse_symbol` must
/// already be validated (caller uses the server's `valid_nse_symbol`).
pub async fn fetch_quote(
    api_key: &str,
    access_token: &str,
    nse_symbol: &str,
    timeout_secs: u64,
) -> Result<KiteQuote> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("build http client")?;
    let inst = format!("NSE:{nse_symbol}");
    let resp = client
        .get(QUOTE_URL)
        .query(&[("i", inst.as_str())])
        .header("X-Kite-Version", "3")
        // Kite's auth is token-scheme (NOT OAuth bearer): "token api_key:access_token".
        .header("Authorization", format!("token {api_key}:{access_token}"))
        .send()
        .await
        .context("GET kite quote")?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("kite auth failed (HTTP {}) — token likely expired; re-login on /kite", status.as_u16());
    }
    if !status.is_success() {
        anyhow::bail!("kite quote HTTP {}", status.as_u16());
    }
    let parsed: QuoteResp = resp.json().await.context("parse kite quote json")?;
    let d = parsed
        .data
        .get(&inst)
        .ok_or_else(|| anyhow::anyhow!("no quote data for {nse_symbol}"))?;

    let depth = build_depth(&d.depth);
    Ok(KiteQuote {
        symbol: nse_symbol.to_string(),
        last_price: d.last_price,
        volume: d.volume,
        total_buy_qty: d.buy_quantity,
        total_sell_qty: d.sell_quantity,
        open: d.ohlc.open,
        high: d.ohlc.high,
        low: d.ohlc.low,
        prev_close: d.ohlc.close,
        net_change: d.net_change,
        lower_circuit: d.lower_circuit_limit,
        upper_circuit: d.upper_circuit_limit,
        exchange_timestamp: d.timestamp.clone(),
        last_trade_time: d.last_trade_time.clone(),
        depth_available: depth.is_some(),
        depth,
    })
}

/// Convert Kite's buy/sell arrays into `MarketDepth`. Returns `None` when the book
/// is empty (all-zero levels) — i.e. outside the live session — so the UI can show
/// an honest "no live depth" rather than a fake all-zero ladder.
fn build_depth(dp: &DepthResp) -> Option<MarketDepth> {
    let has_book = dp
        .buy
        .iter()
        .chain(dp.sell.iter())
        .any(|e| e.price > 0.0 && e.quantity > 0);
    if !has_book {
        return None;
    }
    let mut md = MarketDepth::default();
    for (i, e) in dp.buy.iter().take(5).enumerate() {
        md.bids[i] = DepthLevel { price: e.price, qty: e.quantity, orders: e.orders };
    }
    for (i, e) in dp.sell.iter().take(5).enumerate() {
        md.asks[i] = DepthLevel { price: e.price, qty: e.quantity, orders: e.orders };
    }
    Some(md)
}

// ---------------------------------------------------------------------------
// The user's OWN pending orders + GTTs (the only place a real stoploss/target
// exists — they are private to the trader, never in the public exchange book).
// ---------------------------------------------------------------------------

/// One pending order or GTT leg for the user's own account, flattened for display.
#[derive(Debug, Clone, Serialize)]
pub struct MyOrder {
    /// "order" | "gtt".
    pub kind: String,
    /// "BUY" | "SELL".
    pub side: String,
    pub qty: i64,
    pub order_type: String,
    /// Limit/entry price (0 ⇒ not applicable, e.g. SL-M / market).
    pub price: f64,
    /// Trigger / stoploss / target price (0 ⇒ none).
    pub trigger: f64,
    pub status: String,
    /// What this row is — "pending order" / "SL leg" / "target leg" / "GTT trigger".
    pub note: String,
}

fn jstr(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn jf64(v: &serde_json::Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
fn ji64(v: &serde_json::Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

async fn kite_get_json(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    access_token: &str,
) -> Result<serde_json::Value> {
    let resp = client
        .get(url)
        .header("X-Kite-Version", "3")
        .header("Authorization", format!("token {api_key}:{access_token}"))
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("kite auth failed (HTTP {}) — re-login on /kite", status.as_u16());
    }
    if !status.is_success() {
        anyhow::bail!("kite HTTP {}", status.as_u16());
    }
    resp.json::<serde_json::Value>().await.context("parse json")
}

/// Fetch the user's OWN pending regular orders + active GTTs for one symbol. Pending
/// (not-yet-executed) only. Read-only — this NEVER places, modifies, or cancels.
pub async fn fetch_my_orders(
    api_key: &str,
    access_token: &str,
    nse_symbol: &str,
    timeout_secs: u64,
) -> Result<Vec<MyOrder>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .context("build http client")?;
    let sym = nse_symbol.to_uppercase();
    let mut out: Vec<MyOrder> = Vec::new();

    // Regular orders that are still working (not COMPLETE/CANCELLED/REJECTED).
    const PENDING: &[&str] = &[
        "OPEN", "TRIGGER PENDING", "AMO REQ RECEIVED", "OPEN PENDING",
        "VALIDATION PENDING", "PUT ORDER REQ RECEIVED", "MODIFY PENDING",
    ];
    if let Ok(v) = kite_get_json(&client, "https://api.kite.trade/orders", api_key, access_token).await {
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for o in arr {
                if jstr(o, "tradingsymbol").to_uppercase() != sym {
                    continue;
                }
                let st = jstr(o, "status");
                if !PENDING.iter().any(|p| st.eq_ignore_ascii_case(p)) {
                    continue;
                }
                out.push(MyOrder {
                    kind: "order".into(),
                    side: jstr(o, "transaction_type"),
                    qty: ji64(o, "quantity"),
                    order_type: jstr(o, "order_type"),
                    price: jf64(o, "price"),
                    trigger: jf64(o, "trigger_price"),
                    status: st,
                    note: "pending order".into(),
                });
            }
        }
    }

    // Active GTTs — where a real SL/target trigger lives (single or OCO two-leg).
    if let Ok(v) = kite_get_json(&client, "https://api.kite.trade/gtt/triggers", api_key, access_token).await {
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for g in arr {
                let cond = g.get("condition").cloned().unwrap_or_default();
                if jstr(&cond, "tradingsymbol").to_uppercase() != sym {
                    continue;
                }
                if !jstr(g, "status").eq_ignore_ascii_case("active") {
                    continue;
                }
                let trigs = cond.get("trigger_values").and_then(|t| t.as_array()).cloned().unwrap_or_default();
                let legs = g.get("orders").and_then(|o| o.as_array()).cloned().unwrap_or_default();
                let two_leg = jstr(g, "type").eq_ignore_ascii_case("two-leg");
                for (i, leg) in legs.iter().enumerate() {
                    let trig = trigs.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let note = if two_leg {
                        if i == 0 { "GTT lower trigger (stoploss)" } else { "GTT upper trigger (target)" }
                    } else {
                        "GTT trigger"
                    };
                    out.push(MyOrder {
                        kind: "gtt".into(),
                        side: jstr(leg, "transaction_type"),
                        qty: ji64(leg, "quantity"),
                        order_type: if two_leg { "GTT-OCO".into() } else { "GTT".into() },
                        price: jf64(leg, "price"),
                        trigger: trig,
                        status: "active".into(),
                        note: note.into(),
                    });
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed real-shaped Kite /quote response: prices in RUPEES, 5 depth levels
    // each side, with circuit limits and OHLC.
    const FIXTURE: &str = r#"{
      "status":"success",
      "data":{"NSE:RELIANCE":{
        "last_price":1492.5,"volume":1234567,"net_change":-7.5,
        "lower_circuit_limit":1350.0,"upper_circuit_limit":1640.0,
        "timestamp":"2026-06-30 10:44:00","last_trade_time":"2026-06-30 10:43:59",
        "ohlc":{"open":1500.0,"high":1505.0,"low":1488.0,"close":1500.0},
        "depth":{
          "buy":[{"price":1492.4,"quantity":120,"orders":3},{"price":1492.3,"quantity":80,"orders":2},{"price":1492.2,"quantity":50,"orders":1},{"price":1492.1,"quantity":40,"orders":1},{"price":1492.0,"quantity":30,"orders":1}],
          "sell":[{"price":1492.6,"quantity":90,"orders":2},{"price":1492.7,"quantity":70,"orders":2},{"price":1492.8,"quantity":60,"orders":1},{"price":1492.9,"quantity":45,"orders":1},{"price":1493.0,"quantity":35,"orders":1}]
        }
      }}
    }"#;

    #[test]
    fn parses_quote_in_rupees_with_depth() {
        let parsed: QuoteResp = serde_json::from_str(FIXTURE).unwrap();
        let d = parsed.data.get("NSE:RELIANCE").unwrap();
        assert_eq!(d.last_price, 1492.5); // rupees, not paise
        assert_eq!(d.volume, 1234567);
        assert_eq!(d.ohlc.close, 1500.0);
        let md = build_depth(&d.depth).expect("depth present");
        assert_eq!(md.bids[0].price, 1492.4);
        assert_eq!(md.bids[0].qty, 120);
        assert_eq!(md.asks[0].price, 1492.6);
        assert_eq!(md.asks[4].orders, 1);
    }

    #[test]
    fn empty_book_yields_none() {
        let dp = DepthResp {
            buy: vec![DepthEntry::default(); 5],
            sell: vec![DepthEntry::default(); 5],
        };
        assert!(build_depth(&dp).is_none());
    }
}

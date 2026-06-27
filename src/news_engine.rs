//! Reactive, metered news sentiment.
//!
//! Fires an async request only when a symbol enters the ranked Top 10 (never a
//! continuous pull over the whole universe). Marketaux/EODHD with
//! `&countries=in&exchanges=NSE`. Feature-flagged OFF by default with a mock
//! provider; a real API key flips it on.
//!
//! CONTRACT STUB — public signatures are frozen; bodies are filled in Phase 5.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// News provider configuration (loaded from env). Disabled by default.
#[derive(Debug, Clone)]
pub struct NewsConfig {
    pub enabled: bool,
    /// "marketaux" | "eodhd" | "mock".
    pub provider: String,
    pub api_key: Option<String>,
}

impl Default for NewsConfig {
    fn default() -> Self {
        NewsConfig {
            enabled: false,
            provider: "mock".to_string(),
            api_key: None,
        }
    }
}

impl NewsConfig {
    /// Build from environment (`NEWS_PROVIDER`, `NEWS_API_KEY`); stays disabled
    /// unless a key is present.
    pub fn from_env() -> Self {
        NewsConfig::default()
    }
}

/// A sentiment reading for one symbol.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sentiment {
    /// [-1, 1], negative = bearish.
    pub score: f64,
    pub headline: String,
    pub source: String,
}

/// Fetch sentiment for one symbol. Returns `Ok(None)` when disabled.
pub async fn fetch_sentiment(_cfg: &NewsConfig, _symbol: &str) -> Result<Option<Sentiment>> {
    Ok(None)
}

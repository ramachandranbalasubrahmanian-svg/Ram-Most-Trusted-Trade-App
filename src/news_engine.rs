//! Reactive, metered news sentiment.
//!
//! Fires an async request only when a symbol enters the ranked Top 10 (never a
//! continuous pull over the whole universe). Marketaux/EODHD with
//! `&countries=in&exchanges=NSE`. Feature-flagged OFF by default with a mock
//! provider; a real API key flips it on.
//!
//! Failure-tolerant by design: any transport or parse error degrades to
//! `Ok(None)` so the live loop never stalls on a flaky news provider. The API
//! key is read from the environment and is NEVER logged.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// HTTP timeout for a single news fetch. Kept short so a slow provider can't
/// back up the per-symbol live pipeline.
const NEWS_TIMEOUT: Duration = Duration::from_secs(4);

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
    /// unless a non-empty key is present AND the provider is a real, supported
    /// backend (`marketaux` or `eodhd`). The mock provider (the default) is
    /// always disabled, so out of the box no network calls are ever made.
    pub fn from_env() -> Self {
        // Best-effort .env load; harmless if absent or already loaded.
        dotenvy::dotenv().ok();

        let provider = std::env::var("NEWS_PROVIDER")
            .ok()
            .map(|p| p.trim().to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "mock".to_string());

        let api_key = std::env::var("NEWS_API_KEY")
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());

        let provider_supported = provider == "marketaux" || provider == "eodhd";
        let enabled = api_key.is_some() && provider_supported;

        NewsConfig {
            enabled,
            provider,
            api_key,
        }
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

/// Fetch sentiment for one symbol. Returns `Ok(None)` when disabled or on any
/// transport/parse failure (never hard-fails the caller).
pub async fn fetch_sentiment(cfg: &NewsConfig, symbol: &str) -> Result<Option<Sentiment>> {
    if !cfg.enabled {
        return Ok(None);
    }

    // `enabled` already implies a present, non-empty key, but guard defensively.
    let key = match cfg.api_key.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => return Ok(None),
    };

    let sym = symbol.trim().to_ascii_uppercase();
    if sym.is_empty() {
        return Ok(None);
    }

    let client = match reqwest::Client::builder().timeout(NEWS_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    match cfg.provider.as_str() {
        "marketaux" => Ok(fetch_marketaux(&client, &sym, key).await),
        "eodhd" => Ok(fetch_eodhd(&client, &sym, key).await),
        _ => Ok(None),
    }
}

/// Marketaux: `/v1/news/all`, India + NSE scoped, average per-entity
/// `sentiment_score` across the returned articles.
async fn fetch_marketaux(client: &reqwest::Client, sym: &str, key: &str) -> Option<Sentiment> {
    let symbols = format!("{sym}.NS");
    let resp = client
        .get("https://api.marketaux.com/v1/news/all")
        .query(&[
            ("symbols", symbols.as_str()),
            ("filter_entities", "true"),
            ("countries", "in"),
            ("exchanges", "NSE"),
            ("language", "en"),
            ("limit", "3"),
            ("api_token", key),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let v: serde_json::Value = resp.json().await.ok()?;
    let data = v.get("data")?.as_array()?;
    if data.is_empty() {
        return None;
    }

    // Average every entity sentiment_score we can find, clamped to [-1, 1].
    let mut sum = 0.0_f64;
    let mut count = 0usize;
    for article in data {
        if let Some(entities) = article.get("entities").and_then(|e| e.as_array()) {
            for ent in entities {
                if let Some(s) = ent.get("sentiment_score").and_then(json_to_f64) {
                    sum += s;
                    count += 1;
                }
            }
        }
    }
    let score = if count > 0 {
        (sum / count as f64).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    // First headline/title + source.
    let first = &data[0];
    let headline = first
        .get("title")
        .and_then(|t| t.as_str())
        .or_else(|| first.get("description").and_then(|d| d.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    let source = first
        .get("source")
        .and_then(|s| s.as_str())
        .unwrap_or("marketaux")
        .trim()
        .to_string();

    Some(Sentiment {
        score,
        headline,
        source,
    })
}

/// EODHD: `/api/news`, average `sentiment.polarity` across the returned items.
async fn fetch_eodhd(client: &reqwest::Client, sym: &str, key: &str) -> Option<Sentiment> {
    let s = format!("{sym}.NSE");
    let resp = client
        .get("https://eodhd.com/api/news")
        .query(&[
            ("s", s.as_str()),
            ("limit", "3"),
            ("api_token", key),
            ("fmt", "json"),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let v: serde_json::Value = resp.json().await.ok()?;
    let items = v.as_array()?;
    if items.is_empty() {
        return None;
    }

    let mut sum = 0.0_f64;
    let mut count = 0usize;
    for item in items {
        if let Some(p) = item
            .get("sentiment")
            .and_then(|s| s.get("polarity"))
            .and_then(json_to_f64)
        {
            sum += p;
            count += 1;
        }
    }
    let score = if count > 0 {
        (sum / count as f64).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    let first = &items[0];
    let headline = first
        .get("title")
        .and_then(|t| t.as_str())
        .or_else(|| first.get("content").and_then(|c| c.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    let source = first
        .get("source")
        .and_then(|s| s.as_str())
        .unwrap_or("eodhd")
        .trim()
        .to_string();

    Some(Sentiment {
        score,
        headline,
        source,
    })
}

/// Lenient numeric coercion: accepts JSON numbers and numeric strings.
fn json_to_f64(v: &serde_json::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_mock() {
        let cfg = NewsConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.provider, "mock");
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn from_env_without_key_stays_disabled() {
        // Ensure no key is set for this process; a real provider name alone must
        // not enable the engine without a key.
        unsafe {
            std::env::remove_var("NEWS_API_KEY");
            std::env::set_var("NEWS_PROVIDER", "marketaux");
        }
        let cfg = NewsConfig::from_env();
        assert!(
            !cfg.enabled,
            "no API key present → engine must remain disabled"
        );
        unsafe {
            std::env::remove_var("NEWS_PROVIDER");
        }
    }

    #[tokio::test]
    async fn fetch_on_disabled_cfg_is_none() {
        let cfg = NewsConfig::default();
        let out = fetch_sentiment(&cfg, "RELIANCE").await.unwrap();
        assert!(out.is_none(), "disabled config must yield Ok(None)");

        // Also: a config that looks enabled but uses the mock provider path
        // produces nothing (mock has no real backend).
        let mock = NewsConfig {
            enabled: true,
            provider: "mock".to_string(),
            api_key: Some("dummy".to_string()),
        };
        assert!(fetch_sentiment(&mock, "RELIANCE").await.unwrap().is_none());
    }

    #[test]
    fn json_to_f64_accepts_numbers_and_strings() {
        assert_eq!(json_to_f64(&serde_json::json!(0.5)), Some(0.5));
        assert_eq!(json_to_f64(&serde_json::json!("-0.25")), Some(-0.25));
        assert_eq!(json_to_f64(&serde_json::json!("abc")), None);
        assert_eq!(json_to_f64(&serde_json::json!(null)), None);
    }
}

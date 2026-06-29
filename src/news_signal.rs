//! Display-only news / momentum caution layer for the live Top-10 picks.
//!
//! FIREWALLED: imports only `reqwest`/`serde`/std. It NEVER feeds `eligible()`,
//! Confidence, ranking, sizing, or the edge map. Its sole job is a **caution
//! caption** on a pick: when the news / today's move CONTRADICTS the signal
//! direction, say so.
//!
//! Source: IndianAPI `GET /stock?name=SYM` (`recentNews` headlines + today's
//! `percentChange`). recentNews carries NO sentiment field, so sentiment is a
//! TRANSPARENT headline-keyword heuristic — always labelled as such, never as
//! fact — combined with the objective intraday % move.
//!
//! The rule the owner asked for:
//!   * SELL signal + positive news / up move  → ⚠ CAUTIOUS SELL
//!   * BUY  signal + negative news / down move → ⚠ CAUTIOUS BUY
//!   * otherwise → "news supports" / neutral.
//!
//! Needs `INDIANAPI_KEY` in `.env`. Absent ⇒ an honest "news unavailable" — never
//! fabricated. Budget-capped + cached per day so a paid endpoint isn't hammered.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const INDIANAPI_BASE: &str = "https://stock.indianapi.in";
const NEWS_TIMEOUT: Duration = Duration::from_secs(8);
/// |% move| at/above which today's price action is itself a directional signal.
const MOVE_PCT: f64 = 2.0;
/// Max distinct symbols fetched per day (a hard backstop on the paid API).
const NEWS_DAILY_CAP: u32 = 60;
/// Headlines surfaced per pick.
const MAX_HEADLINES: usize = 3;

/// Positive / negative keyword lexicon for Indian-market headlines. Lowercased,
/// matched as whitespace/punctuation-delimited tokens (so "up" ≠ "upset").
const POSITIVE: &[&str] = &[
    "surge", "surges", "surged", "jump", "jumps", "jumped", "gain", "gains", "gained", "rally",
    "rallies", "rallied", "soar", "soars", "soared", "rise", "rises", "rose", "record", "high",
    "profit", "profits", "beat", "beats", "upgrade", "upgrades", "upgraded", "bonus", "dividend",
    "order", "orders", "win", "wins", "won", "approval", "approved", "expansion", "expands",
    "acquire", "acquires", "acquisition", "strong", "growth", "outperform", "bullish", "boost",
    "boosts", "hike", "hikes", "raises", "raised", "jumps", "multibagger", "buyback",
];
const NEGATIVE: &[&str] = &[
    "fall", "falls", "fell", "drop", "drops", "dropped", "plunge", "plunges", "plunged", "slump",
    "slumps", "slumped", "decline", "declines", "declined", "low", "loss", "losses", "miss",
    "misses", "missed", "downgrade", "downgrades", "downgraded", "cut", "cuts", "probe", "fraud",
    "scam", "default", "defaults", "resign", "resigns", "resigned", "weak", "slowdown", "lawsuit",
    "penalty", "fine", "fined", "ban", "bans", "recall", "bearish", "warning", "warns", "concern",
    "concerns", "debt", "crash", "crashes", "tumble", "tumbles", "tumbled", "selloff", "raid",
];

/// One pick's news / momentum caution signal. All fields display-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NewsSignal {
    pub symbol: String,
    /// The signal side this was judged against ("BUY" | "SELL").
    pub side: String,
    /// True ⇒ we actually have data; false ⇒ key missing / fetch failed / budget.
    pub available: bool,
    /// "positive" | "negative" | "neutral" (headline-keyword heuristic).
    pub sentiment: String,
    /// Net headline score in [-1, 1] (heuristic).
    pub sentiment_score: f64,
    /// Today's % move from the snapshot (None when absent).
    pub pct_change: Option<f64>,
    /// Up to three recent headlines (verbatim — read them yourself).
    pub headlines: Vec<String>,
    /// "cautious" | "aligned" | "neutral" | "unavailable".
    pub verdict: String,
    /// Plain-English explanation of the verdict.
    pub reason: String,
    /// Snapshot date from the source, if any.
    pub as_of: Option<String>,
}

impl NewsSignal {
    fn unavailable(symbol: &str, side: &str, reason: &str) -> Self {
        NewsSignal {
            symbol: symbol.to_string(),
            side: side.to_string(),
            available: false,
            sentiment: "neutral".to_string(),
            sentiment_score: 0.0,
            pct_change: None,
            headlines: Vec::new(),
            verdict: "unavailable".to_string(),
            reason: reason.to_string(),
            as_of: None,
        }
    }
}

/// Strip simple HTML tags from a headline (IndianAPI embeds e.g.
/// `<span class='webrupee'>₹</span>`). Keeps the inner text. Pure.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Score headlines by the keyword lexicon. Pure. Returns (score∈[-1,1], label).
fn score_headlines(headlines: &[String]) -> (f64, &'static str) {
    let mut pos = 0i32;
    let mut neg = 0i32;
    for h in headlines {
        for tok in h
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
        {
            if POSITIVE.contains(&tok) {
                pos += 1;
            } else if NEGATIVE.contains(&tok) {
                neg += 1;
            }
        }
    }
    let total = pos + neg;
    if total == 0 {
        return (0.0, "neutral");
    }
    let score = (pos - neg) as f64 / total as f64;
    let label = if score > 0.15 {
        "positive"
    } else if score < -0.15 {
        "negative"
    } else {
        "neutral"
    };
    (score, label)
}

/// The owner's contradiction rule. Pure — unit-tested.
/// `side` is "BUY"/"SELL"; `sentiment` is the headline label; `pct_change` is today's %.
fn verdict(side: &str, sentiment: &str, pct_change: Option<f64>) -> (String, String) {
    let up = pct_change.map(|p| p >= MOVE_PCT).unwrap_or(false);
    let down = pct_change.map(|p| p <= -MOVE_PCT).unwrap_or(false);
    let bullish = sentiment == "positive" || up;
    let bearish = sentiment == "negative" || down;

    // Describe the news/move evidence compactly.
    let mv = pct_change
        .map(|p| format!("{p:+.1}% today"))
        .unwrap_or_else(|| "no intraday move data".to_string());
    let buy = side.eq_ignore_ascii_case("BUY");

    if buy && bearish {
        let why = if sentiment == "negative" && down {
            format!("negative headlines AND a down move ({mv})")
        } else if sentiment == "negative" {
            "negative headlines".to_string()
        } else {
            format!("a down move ({mv})")
        };
        return (
            "cautious".to_string(),
            format!("⚠ CAUTIOUS BUY — the long is contradicted by {why}. The edge says buy; the news/tape disagrees. Size down or wait."),
        );
    }
    if !buy && bullish {
        let why = if sentiment == "positive" && up {
            format!("positive headlines AND an up move ({mv})")
        } else if sentiment == "positive" {
            "positive headlines".to_string()
        } else {
            format!("an up move ({mv})")
        };
        return (
            "cautious".to_string(),
            format!("⚠ CAUTIOUS SELL — the short is contradicted by {why}. The edge says sell; the news/tape disagrees. Size down or wait."),
        );
    }
    // Supportive: the evidence points the same way as the signal.
    if (buy && bullish) || (!buy && bearish) {
        return (
            "aligned".to_string(),
            format!("News/tape support the {}: {sentiment} headlines, {mv}.", side.to_uppercase()),
        );
    }
    (
        "neutral".to_string(),
        format!("No strong news/move signal either way ({sentiment} headlines, {mv})."),
    )
}

// --- raw fetch + per-day cache -----------------------------------------------

#[derive(Clone)]
struct CachedNews {
    sentiment: String,
    sentiment_score: f64,
    pct_change: Option<f64>,
    headlines: Vec<String>,
    as_of: Option<String>,
}

struct NewsCache {
    day: String,
    used: u32,
    /// `Some` = fetched OK; `None` = fetched today but failed / not on IndianAPI
    /// (a NEGATIVE cache so we never re-spend budget on the same dud symbol).
    by_symbol: HashMap<String, Option<CachedNews>>,
}

fn cache() -> &'static tokio::sync::Mutex<NewsCache> {
    static CACHE: OnceLock<tokio::sync::Mutex<NewsCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        tokio::sync::Mutex::new(NewsCache {
            day: String::new(),
            used: 0,
            by_symbol: HashMap::new(),
        })
    })
}

/// Read `INDIANAPI_KEY` from the environment (.env). Never logged.
fn api_key() -> Option<String> {
    dotenvy::dotenv().ok();
    std::env::var("INDIANAPI_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Pull recent headlines + today's % move from the IndianAPI snapshot.
async fn fetch_indianapi(
    client: &reqwest::Client,
    key: &str,
    symbol: &str,
) -> Option<CachedNews> {
    let resp = client
        .get(format!("{INDIANAPI_BASE}/stock"))
        .header("X-API-Key", key)
        .query(&[("name", symbol)])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;

    let headlines: Vec<String> = v
        .get("recentNews")
        .and_then(|n| n.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|it| it.get("headline").and_then(|h| h.as_str()))
                .map(strip_tags)
                .filter(|s| !s.is_empty())
                .take(MAX_HEADLINES)
                .collect()
        })
        .unwrap_or_default();

    // percentChange can come as a number or a string.
    let pct_change = v.get("percentChange").and_then(|p| {
        p.as_f64()
            .or_else(|| p.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
    });

    let as_of = v
        .get("stockDetailsReusableData")
        .and_then(|d| d.get("date"))
        .and_then(|d| d.as_str())
        .map(|s| s.to_string());

    let (sentiment_score, label) = score_headlines(&headlines);
    Some(CachedNews {
        sentiment: label.to_string(),
        sentiment_score,
        pct_change,
        headlines,
        as_of,
    })
}

/// Build the display-only news caution signal for one (symbol, side). Honest at
/// every failure: missing key, budget exhausted, or fetch error ⇒ `unavailable`,
/// never a fabricated headline or sentiment.
pub async fn build_signal(symbol: &str, side: &str, today: &str) -> NewsSignal {
    let key = match api_key() {
        Some(k) => k,
        None => {
            return NewsSignal::unavailable(
                symbol,
                side,
                "News unavailable — set INDIANAPI_KEY in .env to enable the IndianAPI news layer.",
            )
        }
    };

    // Cache + budget (per process, per day).
    {
        let mut c = cache().lock().await;
        if c.day != today {
            c.day = today.to_string();
            c.used = 0;
            c.by_symbol.clear();
        }
        if let Some(hit) = c.by_symbol.get(symbol) {
            return match hit.clone() {
                Some(news) => finalize(symbol, side, news),
                None => NewsSignal::unavailable(
                    symbol,
                    side,
                    "News unavailable for this symbol today (not on IndianAPI or an earlier fetch failed) — verify on a news site.",
                ),
            };
        }
        if c.used >= NEWS_DAILY_CAP {
            return NewsSignal::unavailable(
                symbol,
                side,
                "News budget for today reached — try again tomorrow (paid-API cap).",
            );
        }
        c.used += 1; // reserve a slot before the await
    }

    let client = match reqwest::Client::builder().timeout(NEWS_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return NewsSignal::unavailable(symbol, side, "News client init failed."),
    };
    match fetch_indianapi(&client, &key, symbol).await {
        Some(news) => {
            cache()
                .lock()
                .await
                .by_symbol
                .insert(symbol.to_string(), Some(news.clone()));
            finalize(symbol, side, news)
        }
        None => {
            // Negative-cache the miss so we don't re-spend budget on it today.
            cache().lock().await.by_symbol.insert(symbol.to_string(), None);
            NewsSignal::unavailable(
                symbol,
                side,
                "News fetch failed (symbol not found on IndianAPI, rate-limited, or network) — verify on a news site.",
            )
        }
    }
}

/// Assemble the final signal (verdict from the cached raw news + the side).
fn finalize(symbol: &str, side: &str, news: CachedNews) -> NewsSignal {
    let (v, reason) = verdict(side, &news.sentiment, news.pct_change);
    NewsSignal {
        symbol: symbol.to_string(),
        side: side.to_string(),
        available: true,
        sentiment: news.sentiment,
        sentiment_score: news.sentiment_score,
        pct_change: news.pct_change,
        headlines: news.headlines,
        verdict: v,
        reason,
        as_of: news.as_of,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_tags_removes_html_keeps_text() {
        assert_eq!(
            strip_tags("3M India special dividend of <span class='webrupee'>₹</span>506/share"),
            "3M India special dividend of ₹506/share"
        );
        assert_eq!(strip_tags("plain headline"), "plain headline");
    }

    #[test]
    fn scores_positive_and_negative_headlines() {
        let (s, l) = score_headlines(&["Reliance profit surges, stock hits record high".to_string()]);
        assert!(s > 0.15, "score {s}");
        assert_eq!(l, "positive");
        let (s2, l2) = score_headlines(&["Company under fraud probe; shares plunge on downgrade".to_string()]);
        assert!(s2 < -0.15, "score {s2}");
        assert_eq!(l2, "negative");
        let (_s3, l3) = score_headlines(&["Board meeting scheduled for next week".to_string()]);
        assert_eq!(l3, "neutral");
    }

    #[test]
    fn token_match_is_word_bounded() {
        // "upset" must not match "up"; "supportive" must not match "port".
        let (_s, l) = score_headlines(&["Investors upset as guidance stays flat".to_string()]);
        assert_eq!(l, "neutral", "substring should not trigger");
    }

    #[test]
    fn sell_with_positive_news_is_cautious() {
        let (v, why) = verdict("SELL", "positive", Some(3.5));
        assert_eq!(v, "cautious");
        assert!(why.contains("CAUTIOUS SELL"));
    }

    #[test]
    fn buy_with_negative_news_is_cautious() {
        let (v, why) = verdict("BUY", "negative", Some(-1.0));
        assert_eq!(v, "cautious");
        assert!(why.contains("CAUTIOUS BUY"));
    }

    #[test]
    fn buy_with_big_down_move_alone_is_cautious() {
        // Even neutral headlines: a -2%+ move contradicts a long.
        let (v, _why) = verdict("BUY", "neutral", Some(-2.5));
        assert_eq!(v, "cautious");
    }

    #[test]
    fn aligned_and_neutral_paths() {
        assert_eq!(verdict("BUY", "positive", Some(2.5)).0, "aligned");
        assert_eq!(verdict("SELL", "negative", Some(-2.5)).0, "aligned");
        assert_eq!(verdict("BUY", "neutral", Some(0.3)).0, "neutral");
        assert_eq!(verdict("SELL", "neutral", None).0, "neutral");
    }
}

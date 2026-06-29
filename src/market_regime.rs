//! Display-only market-regime + breadth snapshot for the live page.
//!
//! FIREWALLED: imports only `config` + `storage_kernel` (+ serde/duckdb). It
//! NEVER feeds `eligible()`, Confidence, ranking, or sizing — it is context for
//! the long-vs-short decision: is the broad tape with you or against you today?
//! Especially relevant when the Live Trade Plan comes out all-long or all-short.
//!
//! Source: `index_daily/NIFTY50.parquet` (trend), `index_daily/INDIAVIX.parquet`
//! (fear gauge), and `nse_daily_all.parquet` (advance/decline breadth). This is
//! EOD daily data, so during a live session it reflects the latest CLOSE, not the
//! live intraday move — every figure is stamped `as_of` so it is never shown as
//! "now". Absent data ⇒ an honest "unavailable", never fabricated.

use serde::{Deserialize, Serialize};

use crate::storage_kernel;

/// A daily market-state snapshot. All fields display-only.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MarketRegime {
    pub available: bool,
    /// Latest index date the figures are drawn from (e.g. "2026-06-29").
    pub as_of: String,
    pub nifty_close: f64,
    /// NIFTY % change on the latest session.
    pub nifty_pct: f64,
    /// "up" | "down" | "flat" — trend vs the 20-day SMA + last-session sign.
    pub nifty_trend: String,
    pub nifty_above_sma20: bool,
    pub vix: f64,
    /// "calm" | "normal" | "elevated" | "high".
    pub vix_label: String,
    pub advances: i64,
    pub declines: i64,
    /// advances / (advances + declines) × 100.
    pub breadth_pct: f64,
    /// "broad up" | "mixed" | "broad down" | "—".
    pub breadth_label: String,
    /// "longs favored" | "shorts favored" | "neutral / choppy".
    pub bias: String,
    /// One-line plain-English read tying it together.
    pub note: String,
}

fn vix_label(v: f64) -> &'static str {
    if v <= 0.0 {
        "—"
    } else if v < 13.0 {
        "calm"
    } else if v < 18.0 {
        "normal"
    } else if v < 25.0 {
        "elevated"
    } else {
        "high"
    }
}

fn breadth_label(pct: f64, total: i64) -> &'static str {
    if total == 0 {
        "—"
    } else if pct >= 60.0 {
        "broad up"
    } else if pct <= 40.0 {
        "broad down"
    } else {
        "mixed"
    }
}

/// NIFTY trend from (last close, 20-day SMA, last-session %). Pure.
fn trend_label(last: f64, sma20: f64, pct: f64) -> (&'static str, bool) {
    let above = sma20 > 0.0 && last >= sma20;
    let trend = if above && pct >= 0.0 {
        "up"
    } else if !above && pct < 0.0 {
        "down"
    } else {
        "flat"
    };
    (trend, above)
}

/// Directional bias from the trend + breadth. Pure. Returns (bias, note-fragment).
fn bias_from(trend: &str, breadth_pct: f64, breadth_total: i64, vix_label: &str) -> (String, String) {
    let breadth_known = breadth_total > 0;
    let broad_up = breadth_known && breadth_pct >= 60.0;
    let broad_down = breadth_known && breadth_pct <= 40.0;
    // Divergence: the index points one way, the broad market the other. That's a
    // low-conviction tape — call it mixed, not "longs/shorts favored".
    let divergence = (trend == "up" && broad_down) || (trend == "down" && broad_up);
    let (bias, lean) = if divergence {
        (
            "mixed / divergence",
            if trend == "up" {
                "the index is up but most stocks are falling — narrow, low-conviction"
            } else {
                "the index is down but most stocks are rising — narrow, low-conviction"
            },
        )
    } else {
        match trend {
            "up" => ("longs favored", "the tape is rising"),
            "down" => ("shorts favored", "the tape is falling"),
            _ => ("neutral / choppy", "no clear trend"),
        }
    };
    let breadth_note = if divergence {
        ""
    } else if broad_up {
        " with broad participation"
    } else if broad_down {
        " with broad weakness"
    } else if breadth_known {
        " but breadth is mixed"
    } else {
        ""
    };
    let vix_note = match vix_label {
        "high" => " — VIX is HIGH, expect violent two-way moves",
        "elevated" => " — VIX elevated, wider stops",
        _ => "",
    };
    (
        bias.to_string(),
        format!("{lean}{breadth_note}{vix_note}."),
    )
}

fn unavailable() -> MarketRegime {
    MarketRegime {
        available: false,
        bias: "unknown".to_string(),
        note: "Market regime unavailable — index_daily/ data not found.".to_string(),
        ..Default::default()
    }
}

/// Latest (date, close) tail of an index parquet, oldest-first. Best-effort.
fn load_index_tail(
    conn: &duckdb::Connection,
    root: &std::path::Path,
    file: &str,
    limit: usize,
) -> Vec<(String, f64)> {
    let path = root.join("index_daily").join(file);
    if !path.exists() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT CAST(date AS DATE)::VARCHAR AS d, close FROM read_parquet({}) \
         ORDER BY date DESC LIMIT {}",
        storage_kernel::quote_path(&path),
        limit.max(1),
    );
    let mut out: Vec<(String, f64)> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))) {
            out = rows.flatten().collect();
        }
    }
    out.reverse(); // oldest-first
    out
}

/// Latest-session advance/decline across the cash universe. Best-effort → (adv, dec).
fn load_breadth(conn: &duckdb::Connection, root: &std::path::Path) -> (i64, i64) {
    let path = root.join("nse_daily_all.parquet");
    if !path.exists() {
        return (0, 0);
    }
    let sql = format!(
        "WITH r AS ( \
            SELECT symbol, close, row_number() OVER (PARTITION BY symbol ORDER BY date DESC) rn \
            FROM read_parquet({}) ) \
         SELECT \
            SUM(CASE WHEN t.close > p.close THEN 1 ELSE 0 END) AS adv, \
            SUM(CASE WHEN t.close < p.close THEN 1 ELSE 0 END) AS dec \
         FROM (SELECT symbol, close FROM r WHERE rn = 1) t \
         JOIN (SELECT symbol, close FROM r WHERE rn = 2) p USING (symbol)",
        storage_kernel::quote_path(&path),
    );
    conn.prepare(&sql)
        .and_then(|mut s| {
            s.query_row([], |row| {
                Ok((row.get::<_, Option<i64>>(0)?.unwrap_or(0), row.get::<_, Option<i64>>(1)?.unwrap_or(0)))
            })
        })
        .unwrap_or((0, 0))
}

/// Compute the market-regime snapshot. On-demand (a handful of small daily reads).
pub fn compute(conn: &duckdb::Connection, root: &std::path::Path) -> MarketRegime {
    let nifty = load_index_tail(conn, root, "NIFTY50.parquet", 25);
    if nifty.len() < 2 {
        return unavailable();
    }
    let closes: Vec<f64> = nifty.iter().map(|(_, c)| *c).collect();
    let last = *closes.last().unwrap();
    let prev = closes[closes.len() - 2];
    let pct = if prev > 0.0 { (last / prev - 1.0) * 100.0 } else { 0.0 };
    let sma_n = closes.len().min(20);
    let sma20 = closes[closes.len() - sma_n..].iter().sum::<f64>() / sma_n as f64;
    let (trend, above) = trend_label(last, sma20, pct);
    let as_of = nifty.last().unwrap().0.clone();

    let vix = load_index_tail(conn, root, "INDIAVIX.parquet", 1)
        .last()
        .map(|(_, c)| *c)
        .unwrap_or(0.0);
    let vlabel = vix_label(vix);

    let (advances, declines) = load_breadth(conn, root);
    let total = advances + declines;
    let breadth_pct = if total > 0 { advances as f64 / total as f64 * 100.0 } else { 0.0 };
    let blabel = breadth_label(breadth_pct, total);

    let (bias, note_frag) = bias_from(trend, breadth_pct, total, vlabel);
    let note = format!(
        "NIFTY {pct:+.2}% ({trend}{}), VIX {vix:.1} ({vlabel}), breadth {advances}↑/{declines}↓ — {bias}: {note_frag}",
        if above { ", above 20-DMA" } else { ", below 20-DMA" },
    );

    MarketRegime {
        available: true,
        as_of,
        nifty_close: last,
        nifty_pct: pct,
        nifty_trend: trend.to_string(),
        nifty_above_sma20: above,
        vix,
        vix_label: vlabel.to_string(),
        advances,
        declines,
        breadth_pct,
        breadth_label: blabel.to_string(),
        bias,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vix_labels_by_band() {
        assert_eq!(vix_label(11.0), "calm");
        assert_eq!(vix_label(15.0), "normal");
        assert_eq!(vix_label(20.0), "elevated");
        assert_eq!(vix_label(30.0), "high");
        assert_eq!(vix_label(0.0), "—");
    }

    #[test]
    fn breadth_labels_by_band() {
        assert_eq!(breadth_label(70.0, 500), "broad up");
        assert_eq!(breadth_label(30.0, 500), "broad down");
        assert_eq!(breadth_label(50.0, 500), "mixed");
        assert_eq!(breadth_label(50.0, 0), "—");
    }

    #[test]
    fn trend_up_when_above_sma_and_positive() {
        let (t, above) = trend_label(100.0, 95.0, 0.5);
        assert_eq!(t, "up");
        assert!(above);
        let (t2, above2) = trend_label(90.0, 95.0, -0.5);
        assert_eq!(t2, "down");
        assert!(!above2);
        // above SMA but down on the day ⇒ flat (mixed).
        assert_eq!(trend_label(100.0, 95.0, -0.3).0, "flat");
    }

    #[test]
    fn bias_follows_trend_and_breadth() {
        assert_eq!(bias_from("up", 70.0, 500, "calm").0, "longs favored");
        assert_eq!(bias_from("down", 30.0, 500, "normal").0, "shorts favored");
        assert_eq!(bias_from("flat", 50.0, 500, "calm").0, "neutral / choppy");
        // high VIX surfaces a warning in the note.
        assert!(bias_from("up", 65.0, 500, "high").1.contains("VIX is HIGH"));
        // Divergence: index up but breadth broad-down ⇒ mixed, not "longs favored".
        assert_eq!(bias_from("up", 31.0, 1775, "normal").0, "mixed / divergence");
        assert_eq!(bias_from("down", 70.0, 500, "normal").0, "mixed / divergence");
    }
}

//! Holdings analytics — the user's REAL external portfolio (across brokers).
//!
//! Ingests holdings (manual/CSV, best-effort pasted text, or a sample fixture)
//! and shows the user THEIR risk picture: concentration (HHI / effective names),
//! sector & broker exposure + heat, same-sector clusters, per-name unrealized
//! P&L, an honest edge cross-reference against our edge map, a WHY flag for weak
//! names, and an ADVISORY half-Kelly sizing band.
//!
//! Honesty firewall (mirrors `conviction.py`'s one-way rule): this module imports
//! ONLY `types` + `strategy_engine::EdgeIndex` — never `stats`/confidence. Every
//! output is display-only and structurally cannot gate or inflate Confidence.
//! Flags carry a reason (a WHY), never a directive; the Kelly band is labelled
//! advisory and is [0,0] for names with no eligible edge. Nothing here places,
//! modifies, or cancels a broker order. `built_ist` + `mark_is_live` keep stale
//! marks from ever reading as live.

use std::collections::HashMap;
use std::path::Path;

use crate::strategy_engine::EdgeIndex;
use crate::types::{
    CorrelationCluster, ExposureRow, Holding, HoldingAnalysis, HoldingInput, PortfolioAnalysis,
};

/// The non-advice disclaimer shipped on every analysis payload.
pub const DISCLAIMER: &str =
    "Your holdings, your risk picture — analysis only, not advice. We never place, modify, or cancel any broker order.";

const DEEP_LOSS_PCT: f64 = -25.0;
const CONCENTRATION_WEIGHT_PCT: f64 = 25.0;
const SECTOR_OVERWEIGHT_PCT: f64 = 40.0;
const KELLY_CLAMP_PCT: f64 = 5.0;

/// Normalize a raw input into a `Holding` (uppercased symbol, broker default).
pub fn normalize(input: &HoldingInput) -> Holding {
    Holding {
        symbol: input.symbol.trim().to_uppercase(),
        qty: input.qty,
        avg_cost: input.avg_cost,
        broker: input
            .broker
            .as_deref()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| "Unknown".to_string()),
        sector: input
            .sector
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

fn sector_label(s: &Option<String>) -> String {
    s.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "Unknown".to_string())
}

fn heat_for(weight_pct: f64) -> String {
    if weight_pct >= 40.0 {
        "high"
    } else if weight_pct >= 20.0 {
        "elevated"
    } else {
        "normal"
    }
    .to_string()
}

/// Half-Kelly advisory band (percent of capital) from an eligible edge's win%
/// and profit factor. Full Kelly f = W·(1 − 1/PF); we return half of that as the
/// band high, clamped to [0, 5]%, and half-of-that as the low. A losing/edgeless
/// input yields [0, 0] — no size, no directive.
fn kelly_band(win_pct: f64, profit_factor: f64) -> (f64, f64) {
    let w = (win_pct / 100.0).clamp(0.0, 1.0);
    if profit_factor <= 0.0 {
        return (0.0, 0.0);
    }
    let full_kelly_pct = w * (1.0 - 1.0 / profit_factor) * 100.0;
    if full_kelly_pct <= 0.0 {
        return (0.0, 0.0);
    }
    let high = (0.5 * full_kelly_pct).clamp(0.0, KELLY_CLAMP_PCT);
    let low = (0.25 * full_kelly_pct).clamp(0.0, KELLY_CLAMP_PCT);
    (low, high)
}

/// Pure, deterministic holdings analysis. `marks` maps SYMBOL → last price (an
/// EOD/last-close mark from the local archive); a missing mark falls back to
/// avg_cost and is flagged `mark_is_live = false`. `edges` is the eligible-edge
/// index. No I/O; never imports scoring.
pub fn analyze(
    holdings: &[Holding],
    marks: &HashMap<String, f64>,
    edges: &EdgeIndex,
    built_ist: String,
) -> PortfolioAnalysis {
    // First pass: per-holding value math.
    struct Row {
        h: Holding,
        last_price: Option<f64>,
        mark_is_live: bool,
        market_value: f64,
        cost_basis: f64,
        unrealized_pnl: f64,
        unrealized_pct: f64,
    }
    let rows: Vec<Row> = holdings
        .iter()
        .map(|h| {
            let mark = marks.get(&h.symbol).copied();
            let eff = mark.unwrap_or(h.avg_cost);
            let market_value = h.qty * eff;
            let cost_basis = h.qty * h.avg_cost;
            let unrealized_pnl = market_value - cost_basis;
            let unrealized_pct = if cost_basis.abs() > f64::EPSILON {
                unrealized_pnl / cost_basis * 100.0
            } else {
                0.0
            };
            Row {
                h: h.clone(),
                last_price: mark,
                mark_is_live: mark.is_some(),
                market_value,
                cost_basis,
                unrealized_pnl,
                unrealized_pct,
            }
        })
        .collect();

    let total_value: f64 = rows.iter().map(|r| r.market_value).sum();
    let total_cost: f64 = rows.iter().map(|r| r.cost_basis).sum();
    let total_unrealized_pnl = total_value - total_cost;
    let total_unrealized_pct = if total_cost.abs() > f64::EPSILON {
        total_unrealized_pnl / total_cost * 100.0
    } else {
        0.0
    };
    let weight = |mv: f64| if total_value.abs() > f64::EPSILON { mv / total_value * 100.0 } else { 0.0 };

    // Sector exposure (needed before per-name flags for sector_overweight).
    let mut sector_value: HashMap<String, f64> = HashMap::new();
    for r in &rows {
        *sector_value.entry(sector_label(&r.h.sector)).or_default() += r.market_value;
    }

    // Per-holding analysis.
    let mut analyses: Vec<HoldingAnalysis> = Vec::with_capacity(rows.len());
    for r in &rows {
        let weight_pct = weight(r.market_value);
        let sec = sector_label(&r.h.sector);
        let sec_weight = weight(*sector_value.get(&sec).unwrap_or(&0.0));

        // Edge cross-reference (honest context, never a buy/sell call).
        let best_edge = edges.get(&r.h.symbol).and_then(|v| {
            v.iter().max_by(|a, b| a.expectancy_r.partial_cmp(&b.expectancy_r).unwrap_or(std::cmp::Ordering::Equal))
        });
        let edge_eligible = best_edge.is_some();
        let (edge_note, kelly_low, kelly_high) = match best_edge {
            Some(e) => {
                let (lo, hi) = kelly_band(e.win_pct, e.profit_factor);
                (
                    format!(
                        "eligible {} edge in our map (n={}, PF={:.2}) — context only, not a trade recommendation",
                        e.strategy, e.n, e.profit_factor
                    ),
                    lo,
                    hi,
                )
            }
            None => (
                "no eligible edge in our map — not a verdict on the stock".to_string(),
                0.0,
                0.0,
            ),
        };

        // Flag precedence: deep_loss > concentration > sector_overweight > no_edge.
        let (flag, flag_reason) = if r.unrealized_pct <= DEEP_LOSS_PCT {
            ("deep_loss".to_string(), format!("{:.0}% below cost — a large unrealized loss", r.unrealized_pct))
        } else if weight_pct >= CONCENTRATION_WEIGHT_PCT {
            ("concentration".to_string(), format!("{:.0}% of the book — one shock dominates your P&L", weight_pct))
        } else if sec_weight >= SECTOR_OVERWEIGHT_PCT {
            ("sector_overweight".to_string(), format!("{} is {:.0}% of the book", sec, sec_weight))
        } else if !edge_eligible {
            ("no_edge".to_string(), "no eligible edge in our map (not a verdict on the stock)".to_string())
        } else {
            (String::new(), String::new())
        };

        analyses.push(HoldingAnalysis {
            symbol: r.h.symbol.clone(),
            qty: r.h.qty,
            avg_cost: r.h.avg_cost,
            broker: r.h.broker.clone(),
            sector: r.h.sector.clone(),
            last_price: r.last_price,
            mark_is_live: r.mark_is_live,
            market_value: r.market_value,
            cost_basis: r.cost_basis,
            unrealized_pnl: r.unrealized_pnl,
            unrealized_pct: r.unrealized_pct,
            weight_pct,
            drawdown_vs_cost_pct: r.unrealized_pct.min(0.0),
            edge_eligible,
            edge_note,
            flag,
            flag_reason,
            kelly_band_low_pct: kelly_low,
            kelly_band_high_pct: kelly_high,
        });
    }

    // Concentration: HHI over weight fractions, effective names = 1/Σw².
    let hhi: f64 = analyses.iter().map(|a| (a.weight_pct / 100.0).powi(2)).sum::<f64>() * 10_000.0;
    let effective_names = if hhi > f64::EPSILON { 10_000.0 / hhi } else { 0.0 };
    let hhi_label = if hhi < 1500.0 {
        "diversified"
    } else if hhi < 2500.0 {
        "moderate"
    } else {
        "concentrated"
    }
    .to_string();

    // Top-N weights.
    let mut weights: Vec<f64> = analyses.iter().map(|a| a.weight_pct).collect();
    weights.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let top_k = |k: usize| weights.iter().take(k).sum::<f64>();

    // Sector & broker exposure rows.
    let exposure = |key_of: &dyn Fn(&HoldingAnalysis) -> String| -> Vec<ExposureRow> {
        let mut map: HashMap<String, (usize, f64, f64)> = HashMap::new();
        for a in &analyses {
            let e = map.entry(key_of(a)).or_insert((0, 0.0, 0.0));
            e.0 += 1;
            e.1 += a.market_value;
            e.2 += a.unrealized_pnl;
        }
        let mut rows: Vec<ExposureRow> = map
            .into_iter()
            .map(|(key, (names, value, pnl))| {
                let weight_pct = weight(value);
                ExposureRow { key, names, value, weight_pct, unrealized_pnl: pnl, heat: heat_for(weight_pct) }
            })
            .collect();
        rows.sort_by(|a, b| b.value.partial_cmp(&a.value).unwrap_or(std::cmp::Ordering::Equal));
        rows
    };
    let by_sector = exposure(&|a| sector_label(&a.sector));
    let by_broker = exposure(&|a| a.broker.clone());

    // Same-sector clusters (>=2 names AND combined weight >= concentration cut).
    let mut clusters: Vec<CorrelationCluster> = by_sector
        .iter()
        .filter(|row| row.names >= 2 && row.weight_pct >= CONCENTRATION_WEIGHT_PCT && row.key != "Unknown")
        .map(|row| CorrelationCluster {
            label: row.key.clone(),
            members: analyses
                .iter()
                .filter(|a| sector_label(&a.sector) == row.key)
                .map(|a| a.symbol.clone())
                .collect(),
            combined_weight_pct: row.weight_pct,
            basis: "same sector (no price-correlation data)".to_string(),
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.combined_weight_pct.partial_cmp(&a.combined_weight_pct).unwrap_or(std::cmp::Ordering::Equal)
    });

    let names_with_edge = analyses.iter().filter(|a| a.edge_eligible).count();
    let marks_live = analyses.iter().filter(|a| a.mark_is_live).count();
    let names_total = analyses.len();

    PortfolioAnalysis {
        total_cost,
        total_value,
        total_unrealized_pnl,
        total_unrealized_pct,
        holdings: analyses,
        top1_weight_pct: top_k(1),
        top3_weight_pct: top_k(3),
        top5_weight_pct: top_k(5),
        hhi,
        hhi_label,
        effective_names,
        by_sector,
        by_broker,
        clusters,
        names_with_edge,
        names_total,
        marks_live,
        marks_total: names_total,
        disclaimer: DISCLAIMER.to_string(),
        built_ist,
    }
}

// ---------------------------------------------------------------------------
// Ingest: CSV / pasted text / sample
// ---------------------------------------------------------------------------

/// Strip currency symbols, commas and spaces, then parse as f64.
fn parse_num(s: &str) -> Option<f64> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
    if cleaned.is_empty() {
        None
    } else {
        cleaned.parse::<f64>().ok()
    }
}

/// Parse a holdings CSV with broker-agnostic header aliasing (Zerodha/Groww/
/// INDmoney). Good rows return as `HoldingInput`; malformed rows are collected
/// into `warnings` (never panics, never fabricates).
pub fn parse_csv<R: std::io::Read>(reader: R) -> (Vec<HoldingInput>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut out = Vec::new();
    let mut rdr = csv::ReaderBuilder::new().has_headers(true).flexible(true).trim(csv::Trim::All).from_reader(reader);

    let headers = match rdr.headers() {
        Ok(h) => h.clone(),
        Err(e) => {
            warnings.push(format!("could not read CSV header: {e}"));
            return (out, warnings);
        }
    };
    let find = |aliases: &[&str]| -> Option<usize> {
        headers.iter().position(|h| {
            let h = h.to_ascii_lowercase();
            aliases.iter().any(|a| h == *a || h.contains(a))
        })
    };
    let c_sym = find(&["tradingsymbol", "symbol", "instrument", "scrip", "stock", "name"]);
    let c_qty = find(&["quantity", "qty", "units", "shares", "holding"]);
    let c_avg = find(&["average price", "avg price", "avg cost", "avg. cost", "avg", "buy price", "cost", "price"]);
    let c_broker = find(&["broker", "account", "exchange"]);
    let c_sector = find(&["sector", "industry"]);

    let (Some(c_sym), Some(c_qty), Some(c_avg)) = (c_sym, c_qty, c_avg) else {
        warnings.push("CSV missing required columns (need symbol, quantity, average price)".to_string());
        return (out, warnings);
    };

    for (i, rec) in rdr.records().enumerate() {
        let rec = match rec {
            Ok(r) => r,
            Err(_) => {
                warnings.push(format!("row {}: malformed CSV record — skipped", i + 2));
                continue;
            }
        };
        let get = |idx: usize| rec.get(idx).unwrap_or("").trim();
        let sym = get(c_sym).to_string();
        let qty = parse_num(get(c_qty));
        let avg = parse_num(get(c_avg));
        match (sym.is_empty(), qty, avg) {
            (false, Some(qty), Some(avg)) if qty > 0.0 => out.push(HoldingInput {
                symbol: sym,
                qty,
                avg_cost: avg,
                broker: c_broker.map(|i| get(i).to_string()).filter(|s| !s.is_empty()),
                sector: c_sector.map(|i| get(i).to_string()).filter(|s| !s.is_empty()),
            }),
            _ => warnings.push(format!("row {}: could not parse (symbol/qty/avg) — skipped", i + 2)),
        }
    }
    (out, warnings)
}

/// Best-effort parse of pasted/PDF-extracted text. Each line is scanned for a
/// symbol-like token plus the first two numbers (qty, avg cost). Lines that
/// don't yield a clean triple go to `warnings`. Deliberately conservative.
pub fn parse_text(raw: &str) -> (Vec<HoldingInput>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let sym = toks
            .iter()
            .find(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '&' || c == '-') && t.chars().any(|c| c.is_ascii_alphabetic()));
        let nums: Vec<f64> = toks.iter().filter_map(|t| parse_num(t)).collect();
        match (sym, nums.first(), nums.get(1)) {
            (Some(sym), Some(&qty), Some(&avg)) if qty > 0.0 && avg > 0.0 => out.push(HoldingInput {
                symbol: sym.to_string(),
                qty,
                avg_cost: avg,
                broker: None,
                sector: None,
            }),
            _ => warnings.push(format!("line {}: could not parse a (symbol, qty, avg) triple — skipped", i + 1)),
        }
    }
    (out, warnings)
}

/// A fixed, deterministic multi-broker / multi-sector sample portfolio for the
/// "Load sample" path (includes one no-edge name to demonstrate the flag).
pub fn sample_holdings() -> Vec<Holding> {
    let h = |symbol: &str, qty: f64, avg_cost: f64, broker: &str, sector: &str| Holding {
        symbol: symbol.to_string(),
        qty,
        avg_cost,
        broker: broker.to_string(),
        sector: Some(sector.to_string()),
    };
    vec![
        h("RELIANCE", 50.0, 1300.0, "Zerodha", "Energy"),
        h("HDFCBANK", 40.0, 1600.0, "Zerodha", "Banking"),
        h("ICICIBANK", 60.0, 1100.0, "Groww", "Banking"),
        h("SBIN", 100.0, 820.0, "Zerodha", "Banking"),
        h("TCS", 20.0, 3800.0, "Groww", "IT"),
        h("INFY", 30.0, 1500.0, "INDmoney", "IT"),
        h("TATASTEEL", 200.0, 150.0, "Zerodha", "Metals"),
        h("SMALLCAPXYZ", 500.0, 90.0, "Groww", "Unknown"),
    ]
}

// ---------------------------------------------------------------------------
// Mark source (I/O — called by the endpoint, NOT by analyze())
// ---------------------------------------------------------------------------

/// Latest daily close for a symbol from the local parquet archive, as an EOD
/// mark. `None` when there's no daily file (caller falls back to avg_cost and
/// flags the mark not-live). Signals-only: a read, never an order.
pub fn latest_daily_close(conn: &duckdb::Connection, root: &Path, symbol: &str) -> Option<f64> {
    let path = crate::config::parquet_path(root, symbol, crate::config::Timeframe::Daily);
    if !path.exists() {
        return None;
    }
    let sql = format!(
        "SELECT close FROM read_parquet({}) ORDER BY date DESC LIMIT 1",
        crate::storage_kernel::quote_path(&path)
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let mut rows = stmt.query_map([], |row| row.get::<_, f64>(0)).ok()?;
    rows.next()?.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Direction;
    use crate::types::EligibleEdge;

    fn hold(symbol: &str, qty: f64, avg: f64, broker: &str, sector: Option<&str>) -> Holding {
        Holding { symbol: symbol.into(), qty, avg_cost: avg, broker: broker.into(), sector: sector.map(|s| s.into()) }
    }
    fn edge(win_pct: f64, pf: f64, n: usize) -> EligibleEdge {
        EligibleEdge { strategy: "vwap_cross".into(), direction: Direction::Long, expectancy_r: 0.2, profit_factor: pf, win_pct, n }
    }
    fn edges_with(symbols: &[(&str, EligibleEdge)]) -> EdgeIndex {
        let mut m: EdgeIndex = EdgeIndex::new();
        for (s, e) in symbols {
            m.insert((*s).to_string(), vec![e.clone()]);
        }
        m
    }

    #[test]
    fn empty_holdings_is_all_zero() {
        let a = analyze(&[], &HashMap::new(), &EdgeIndex::new(), "2026-06-28 09:00:00".into());
        assert_eq!(a.total_value, 0.0);
        assert_eq!(a.total_cost, 0.0);
        assert!(a.holdings.is_empty() && a.by_sector.is_empty() && a.by_broker.is_empty() && a.clusters.is_empty());
        assert_eq!(a.hhi, 0.0);
        assert!(!a.disclaimer.is_empty());
        assert!(!a.built_ist.is_empty());
    }

    #[test]
    fn unrealized_pnl_and_weight_math() {
        let hs = vec![hold("RELIANCE", 10.0, 100.0, "Z", Some("Energy")), hold("TCS", 5.0, 200.0, "Z", Some("IT"))];
        let marks = HashMap::from([("RELIANCE".to_string(), 120.0), ("TCS".to_string(), 180.0)]);
        let a = analyze(&hs, &marks, &EdgeIndex::new(), "t".into());
        let r = &a.holdings[0];
        assert!((r.market_value - 1200.0).abs() < 1e-9);
        assert!((r.cost_basis - 1000.0).abs() < 1e-9);
        assert!((r.unrealized_pnl - 200.0).abs() < 1e-9);
        assert!((r.unrealized_pct - 20.0).abs() < 1e-9);
        let wsum: f64 = a.holdings.iter().map(|h| h.weight_pct).sum();
        assert!((wsum - 100.0).abs() < 1e-9, "weights sum {wsum}");
    }

    #[test]
    fn missing_mark_falls_back_to_cost_and_flags_not_live() {
        let hs = vec![hold("NOPRICE", 10.0, 50.0, "Z", Some("IT"))];
        let a = analyze(&hs, &HashMap::new(), &edges_with(&[("NOPRICE", edge(55.0, 1.5, 80))]), "t".into());
        let r = &a.holdings[0];
        assert!(!r.mark_is_live);
        assert!(r.last_price.is_none());
        assert!((r.market_value - r.cost_basis).abs() < 1e-9, "falls back to cost ⇒ 0 unrealized");
        assert_eq!(a.marks_total, 1);
        assert_eq!(a.marks_live, 0);
    }

    #[test]
    fn hhi_and_effective_names() {
        // 4 equal-weight names (same mark=cost ⇒ equal value).
        let hs = vec![
            hold("A", 1.0, 100.0, "Z", Some("IT")),
            hold("B", 1.0, 100.0, "Z", Some("Energy")),
            hold("C", 1.0, 100.0, "Z", Some("Metals")),
            hold("D", 1.0, 100.0, "Z", Some("Auto")),
        ];
        let a = analyze(&hs, &HashMap::new(), &EdgeIndex::new(), "t".into());
        assert!((a.hhi - 2500.0).abs() < 1.0, "hhi {}", a.hhi);
        assert!((a.effective_names - 4.0).abs() < 0.05, "eff {}", a.effective_names);

        // One 60% name (+ one 40%) ⇒ concentrated, effective_names < 2.
        let hs2 = vec![hold("BIG", 60.0, 100.0, "Z", Some("IT")), hold("SM", 40.0, 100.0, "Z", Some("Auto"))];
        let a2 = analyze(&hs2, &HashMap::new(), &EdgeIndex::new(), "t".into());
        assert_eq!(a2.hhi_label, "concentrated");
        assert!(a2.effective_names < 2.0, "eff {}", a2.effective_names);
    }

    #[test]
    fn missing_sector_is_unknown() {
        let hs = vec![hold("X", 1.0, 100.0, "Z", None)];
        let a = analyze(&hs, &HashMap::new(), &EdgeIndex::new(), "t".into());
        assert_eq!(a.by_sector[0].key, "Unknown");
    }

    #[test]
    fn broker_and_sector_exposure_sorted_desc() {
        let hs = vec![
            hold("A", 100.0, 100.0, "Zerodha", Some("Banking")),
            hold("B", 10.0, 100.0, "Groww", Some("IT")),
        ];
        let a = analyze(&hs, &HashMap::new(), &EdgeIndex::new(), "t".into());
        assert!(a.by_broker[0].value >= a.by_broker[1].value, "sorted desc by value");
        assert_eq!(a.by_broker[0].key, "Zerodha");
        assert!(a.by_broker[0].names == 1);
        assert_eq!(a.by_broker[0].heat, "high"); // ~91% weight
    }

    #[test]
    fn flag_deep_loss_precedence() {
        // -30% AND 100% weight: deep_loss must win over concentration.
        let hs = vec![hold("LOSER", 10.0, 100.0, "Z", Some("IT"))];
        let marks = HashMap::from([("LOSER".to_string(), 70.0)]);
        let a = analyze(&hs, &marks, &edges_with(&[("LOSER", edge(55.0, 1.5, 80))]), "t".into());
        assert_eq!(a.holdings[0].flag, "deep_loss");
        assert!(!a.holdings[0].flag_reason.is_empty());
    }

    #[test]
    fn flag_concentration_when_overweight() {
        // Two names, one 30% weight, no deep loss, both have edges ⇒ concentration.
        let hs = vec![hold("BIG", 30.0, 100.0, "Z", Some("IT")), hold("REST", 70.0, 100.0, "Z", Some("Energy"))];
        let edges = edges_with(&[("BIG", edge(55.0, 1.5, 80)), ("REST", edge(55.0, 1.5, 80))]);
        let a = analyze(&hs, &HashMap::new(), &edges, "t".into());
        let big = a.holdings.iter().find(|h| h.symbol == "BIG").unwrap();
        // BIG is 30% weight; REST is 70% (also flagged). BIG's flag = concentration.
        assert_eq!(big.flag, "concentration");
        assert!(big.flag_reason.contains("%"));
    }

    #[test]
    fn flag_no_edge_and_zero_kelly_band() {
        // NOEDGE is a SMALL weight (no deep loss, not concentrated, sector not
        // overweight) and ABSENT from EdgeIndex ⇒ flag falls through to no_edge
        // with a [0,0] Kelly band.
        let hs = vec![
            hold("HASEDGE", 100.0, 100.0, "Z", Some("IT")),
            hold("FILL", 100.0, 100.0, "Z", Some("Energy")),
            hold("NOEDGE", 10.0, 100.0, "Z", Some("Auto")),
        ];
        let edges = edges_with(&[("HASEDGE", edge(55.0, 1.5, 80)), ("FILL", edge(55.0, 1.5, 80))]);
        let a = analyze(&hs, &HashMap::new(), &edges, "t".into());
        let ne = a.holdings.iter().find(|h| h.symbol == "NOEDGE").unwrap();
        assert!(!ne.edge_eligible);
        assert!(ne.edge_note.contains("not a verdict"));
        assert_eq!(ne.flag, "no_edge");
        assert_eq!(ne.kelly_band_low_pct, 0.0);
        assert_eq!(ne.kelly_band_high_pct, 0.0);
    }

    #[test]
    fn kelly_band_is_half_clamped_and_a_band() {
        let hs = vec![hold("E", 10.0, 100.0, "Z", Some("IT"))];
        let a = analyze(&hs, &HashMap::new(), &edges_with(&[("E", edge(60.0, 2.0, 200))]), "t".into());
        let r = &a.holdings[0];
        // full Kelly = 0.6*(1-1/2)*100 = 30%; half = 15% → clamped to 5.
        assert!(r.kelly_band_high_pct <= 5.0);
        assert!(r.kelly_band_low_pct <= r.kelly_band_high_pct);
        assert!(r.kelly_band_low_pct >= 0.0);
        let full = 0.6 * (1.0 - 1.0 / 2.0) * 100.0;
        assert!(r.kelly_band_high_pct < full, "half-kelly must be below full");
    }

    #[test]
    fn edge_cross_reference_honest_context() {
        let hs = vec![hold("E", 10.0, 100.0, "Z", Some("IT"))];
        let a = analyze(&hs, &HashMap::new(), &edges_with(&[("E", edge(55.0, 1.6, 120))]), "t".into());
        let r = &a.holdings[0];
        assert!(r.edge_eligible);
        assert!(r.edge_note.contains("n=120") && r.edge_note.contains("PF"));
        assert!(!r.edge_note.to_uppercase().contains("BUY") && !r.edge_note.to_uppercase().contains("SELL"));
    }

    #[test]
    fn parse_csv_aliases_and_warns() {
        let csv = "tradingsymbol,quantity,average price,sector\nRELIANCE,10,1300,Energy\nBADROW,notanumber,xx,IT\n";
        let (rows, warnings) = parse_csv(csv.as_bytes());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert!((rows[0].qty - 10.0).abs() < 1e-9);
        assert_eq!(warnings.len(), 1, "the bad row must warn, not panic");
    }

    #[test]
    fn parse_text_best_effort() {
        let txt = "RELIANCE 10 1300\nsome noise line without numbers\nTCS 5 3800\n";
        let (rows, warnings) = parse_text(txt);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert!(!warnings.is_empty(), "the noise line goes to warnings");
    }

    #[test]
    fn sample_holdings_deterministic() {
        let a = sample_holdings();
        let b = sample_holdings();
        assert_eq!(a.len(), b.len());
        assert_eq!(a.first().unwrap().symbol, "RELIANCE");
        assert_eq!(a.last().unwrap().symbol, "SMALLCAPXYZ");
    }

    #[test]
    fn disclaimer_and_built_ist_present() {
        let a = analyze(&sample_holdings(), &HashMap::new(), &EdgeIndex::new(), "2026-06-28 10:00:00".into());
        assert_eq!(a.disclaimer, DISCLAIMER);
        assert_eq!(a.built_ist, "2026-06-28 10:00:00");
        assert!(a.disclaimer.to_lowercase().contains("not advice"));
    }
}

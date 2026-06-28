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
use std::io::Read;
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
        last_price: input.last_price.filter(|p| *p > 0.0),
    }
}

/// Merge holdings that share a symbol into ONE row (the same stock held across
/// two brokers, or two statement lines, is one position). Quantities sum;
/// `avg_cost` becomes the quantity-weighted average; brokers are joined; the
/// first non-empty sector and any provided `last_price` are kept. Order is
/// preserved by first appearance. This keeps `names_total` over DISTINCT names,
/// so the correlation coverage ("X of N names") and cluster weights reconcile —
/// the root fix for the duplicate-symbol accounting gap.
pub fn merge_holdings(holdings: Vec<Holding>) -> Vec<Holding> {
    let mut order: Vec<String> = Vec::new();
    let mut acc: HashMap<String, Holding> = HashMap::new();
    let mut brokers: HashMap<String, Vec<String>> = HashMap::new();
    for h in holdings {
        match acc.get_mut(&h.symbol) {
            None => {
                order.push(h.symbol.clone());
                brokers.insert(h.symbol.clone(), vec![h.broker.clone()]);
                acc.insert(h.symbol.clone(), h);
            }
            Some(existing) => {
                let qty_sum = existing.qty + h.qty;
                // Quantity-weighted average cost (guard non-positive qty).
                existing.avg_cost = if qty_sum > 0.0 {
                    (existing.avg_cost * existing.qty + h.avg_cost * h.qty) / qty_sum
                } else {
                    existing.avg_cost
                };
                existing.qty = qty_sum;
                if existing.sector.is_none() {
                    existing.sector = h.sector.clone();
                }
                if existing.last_price.is_none() {
                    existing.last_price = h.last_price;
                }
                let bs = brokers.entry(h.symbol.clone()).or_default();
                if !bs.contains(&h.broker) {
                    bs.push(h.broker.clone());
                }
            }
        }
    }
    order
        .into_iter()
        .map(|sym| {
            let mut h = acc.remove(&sym).unwrap();
            if let Some(bs) = brokers.get(&sym) {
                if bs.len() > 1 {
                    h.broker = bs.join("/");
                }
            }
            h
        })
        .collect()
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
            // Prefer a statement-provided close (values off-archive names
            // correctly); else the archive's last close; else fall back to cost.
            let mark = h.last_price.filter(|p| *p > 0.0).or_else(|| marks.get(&h.symbol).copied());
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
        // Correlation block is filled by `attach_correlation` (needs daily I/O).
        // Left empty here so `analyze` stays pure; `corr_basis` states the default.
        corr_effective_bets: None,
        corr_avg_pairwise: None,
        corr_names_used: 0,
        corr_names_dropped: Vec::new(),
        corr_sessions: 0,
        corr_basis: "weight-based only (no return-correlation computed)".to_string(),
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

/// Resolved column indices for a holdings table.
struct ColMap {
    sym: usize,
    qty: usize,
    /// Per-share average/buy price column. Optional: a value-only export (only a
    /// "Buy value" total) is still valid — avg cost is then derived from buyval/qty.
    avg: Option<usize>,
    buyval: Option<usize>,
    broker: Option<usize>,
    sector: Option<usize>,
    isin: Option<usize>,
    last: Option<usize>,
}

/// Resolve a column by header alias, EXACT match first (across all columns), then
/// a substring `contains` fallback. Two-pass so a precise header ("Quantity",
/// "Stock Name") always wins over a loose substring hit on an earlier column
/// ("Holding Period", "Account Name") — the column-major contains-match bug.
fn find_col(rec: &csv::StringRecord, aliases: &[&str]) -> Option<usize> {
    let lc: Vec<String> = rec.iter().map(|h| h.trim().to_ascii_lowercase()).collect();
    if let Some(i) = lc.iter().position(|h| aliases.iter().any(|a| h == a)) {
        return Some(i);
    }
    lc.iter().position(|h| !h.is_empty() && aliases.iter().any(|a| h.contains(a)))
}

/// Treat a record as a header row and resolve columns; `Some` only if it carries
/// a symbol/stock column AND a quantity column AND a PRICE source — either a
/// per-share avg-price column OR a "buy value" total (avg derived = total/qty).
/// So a statement's preamble lines never match, and a value-only export is read
/// correctly instead of dropping to positional mode and 10×-corrupting cost basis.
/// `avg`/`qty` exclude bare loose tokens ("cost", "holding"); `find_col` prefers
/// exact matches anyway.
fn header_map(rec: &csv::StringRecord) -> Option<ColMap> {
    let sym = find_col(rec, &["tradingsymbol", "stock name", "scrip name", "security name", "symbol", "instrument", "scrip", "security", "company", "stock", "name"]);
    let qty = find_col(rec, &["quantity", "qty", "units", "shares", "no. of shares", "no of shares"]);
    let avg = find_col(rec, &["average buy price", "avg buy price", "average price", "avg price", "avg. price", "average cost", "avg cost", "avg. cost", "buy price", "cost basis", "buy avg", "avg rate", "rate", "avg"]);
    let buyval = find_col(rec, &["buy value", "invested value", "investment value", "invested amount", "cost value"]);
    let (sym, qty) = (sym?, qty?);
    if sym == qty {
        return None;
    }
    // a price source that is a distinct column from sym/qty
    let avg = avg.filter(|&a| a != sym && a != qty);
    let has_buyval = buyval.map_or(false, |b| b != sym && b != qty);
    if avg.is_none() && !has_buyval {
        return None;
    }
    Some(ColMap {
        sym,
        qty,
        avg,
        buyval: if has_buyval { buyval } else { None },
        broker: find_col(rec, &["broker", "account"]),
        sector: find_col(rec, &["sector", "industry"]),
        isin: find_col(rec, &["isin"]),
        last: find_col(rec, &["closing price", "close price", "last price", "ltp", "current price", "market price", "cmp"]),
    })
}

impl ColMap {
    /// How many columns this header resolved (2 required + optionals) — used to
    /// prefer the detailed equity-holdings header over a sparser summary table.
    fn resolved(&self) -> usize {
        2 + [self.avg, self.buyval, self.broker, self.sector, self.isin, self.last].iter().filter(|o| o.is_some()).count()
    }
}

/// Sniff the delimiter (tab vs comma) by which one yields the most rectangular
/// rows — counting lines that split into ≥3 fields — NOT raw character counts, so
/// a comma-heavy preamble can't fool a tab-delimited statement. (No EU semicolon
/// branch: comma-as-decimal would silently 100× the numbers.)
fn sniff_delimiter(bytes: &[u8]) -> u8 {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().take(40).collect();
    let multi = |d: char| lines.iter().filter(|l| l.matches(d).count() >= 2).count();
    let tabs = multi('\t');
    let commas = multi(',');
    if tabs >= commas && tabs > 0 {
        b'\t'
    } else {
        b','
    }
}

/// Parse one data record into a `HoldingInput`. `Ok` = a good row; `Err(Some)` =
/// a skip-with-warning (non-empty symbol but unreadable numbers); `Err(None)` =
/// a blank/structural row to ignore silently.
fn parse_row(rec: &csv::StringRecord, cm: &ColMap, rownum: usize) -> Result<HoldingInput, Option<String>> {
    let get = |idx: usize| rec.get(idx).unwrap_or("").trim();
    let sym = get(cm.sym).to_string();
    let qty = parse_num(get(cm.qty));
    // Average cost: prefer the EXACT "buy value" total / qty (the printed avg price
    // is rounded, so this reconciles with the statement); else the stated avg-price
    // column. A "buy value" column is a TOTAL, never a per-share price.
    let avg = qty.filter(|q| *q > 0.0).and_then(|q| {
        cm.buyval
            .and_then(|i| parse_num(get(i)))
            .filter(|bv| *bv > 0.0)
            .map(|bv| bv / q)
            .or_else(|| cm.avg.and_then(|i| parse_num(get(i))).filter(|a| *a > 0.0))
    });
    match (sym.is_empty(), qty, avg) {
        (false, Some(qty), Some(avg)) if qty > 0.0 && avg > 0.0 => Ok(HoldingInput {
            symbol: sym,
            qty,
            avg_cost: avg,
            broker: cm.broker.map(|i| get(i).to_string()).filter(|s| !s.is_empty()),
            sector: cm.sector.map(|i| get(i).to_string()).filter(|s| !s.is_empty()),
            isin: cm.isin.map(|i| get(i).to_string()).filter(|s| !s.is_empty()),
            last_price: cm.last.and_then(|i| parse_num(get(i))).filter(|p| *p > 0.0),
        }),
        (false, _, _) => Err(Some(format!("row {}: couldn't read quantity/price for '{}' — skipped", rownum, sym))),
        _ => Err(None),
    }
}

/// Parse a holdings table (CSV/TSV bytes) with broker-agnostic header aliasing
/// AND header-row DETECTION — so a statement with preamble rows above the table
/// (Groww-style: name, client code, summary, then the header) parses correctly.
/// When no header is found, falls back to positional `symbol, qty, avg [,broker
/// ,sector]` (the documented paste format). Good rows return as `HoldingInput`;
/// unreadable rows go to `warnings`. Never panics, never fabricates.
pub fn parse_csv<R: Read>(mut reader: R) -> (Vec<HoldingInput>, Vec<String>) {
    let mut buf = Vec::new();
    if reader.read_to_end(&mut buf).is_err() {
        return (Vec::new(), vec!["could not read the input".to_string()]);
    }
    parse_holdings_bytes(&buf)
}

/// Byte-level entry point (used by the Excel importer + paste path).
pub fn parse_holdings_bytes(bytes: &[u8]) -> (Vec<HoldingInput>, Vec<String>) {
    let delim = sniff_delimiter(bytes);
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .delimiter(delim)
        .from_reader(bytes);
    let records: Vec<csv::StringRecord> = rdr.records().filter_map(|r| r.ok()).collect();

    let mut out = Vec::new();
    let mut warnings = Vec::new();

    // Locate the header row: among all header-shaped records, prefer the one that
    // resolves the MOST columns (the detailed equity table, not a sparse summary
    // section printed above it); ties keep the earliest.
    let header_at = records
        .iter()
        .enumerate()
        .filter_map(|(i, rec)| header_map(rec).map(|cm| (i, cm)))
        .fold(None::<(usize, ColMap)>, |best, (i, cm)| match best {
            Some((_, ref b)) if b.resolved() >= cm.resolved() => best,
            _ => Some((i, cm)),
        });

    match header_at {
        Some((hidx, cm)) => {
            for (i, rec) in records.iter().enumerate().skip(hidx + 1) {
                if rec.iter().all(|f| f.trim().is_empty()) {
                    continue;
                }
                match parse_row(rec, &cm, i + 1) {
                    Ok(h) => out.push(h),
                    Err(Some(w)) => warnings.push(w),
                    Err(None) => {}
                }
            }
        }
        None => {
            // Headerless positional fallback: symbol, qty, avg [, broker, sector].
            // ONLY enter this if the first non-empty row looks like DATA (its qty &
            // avg cells are numeric). Otherwise it's an unrecognised header row —
            // refuse to guess (which would fabricate a phantom holding from the
            // header) and return an honest "missing columns" warning instead.
            let first_is_data = records
                .iter()
                .find(|r| !r.iter().all(|f| f.trim().is_empty()))
                .map_or(false, |r| {
                    r.len() >= 3 && parse_num(r.get(1).unwrap_or("")).is_some() && parse_num(r.get(2).unwrap_or("")).is_some()
                });
            if !first_is_data {
                warnings.push(
                    "no recognised header — needs columns for stock/symbol, quantity and average price (or buy value)".to_string(),
                );
            } else {
                let cm = ColMap { sym: 0, qty: 1, avg: Some(2), buyval: None, broker: Some(3), sector: Some(4), isin: None, last: None };
                for (i, rec) in records.iter().enumerate() {
                    if rec.len() < 3 || rec.iter().all(|f| f.trim().is_empty()) {
                        continue;
                    }
                    match parse_row(rec, &cm, i + 1) {
                        Ok(h) => out.push(h),
                        Err(Some(w)) => warnings.push(w),
                        Err(None) => {}
                    }
                }
            }
        }
    }

    if out.is_empty() && warnings.is_empty() {
        warnings.push("no holdings rows recognised — needs columns for stock/symbol, quantity and average price".to_string());
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
                isin: None,
                last_price: None,
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
        last_price: None,
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

/// The owner's REAL consolidated book, as a one-click preset for the Portfolio
/// page — mirrors the 26-06-2026 holdings statement. Sectors are factual
/// classifications; `last` is the statement's closing price (carried as the mark
/// so off-archive names — 63MOONS, the post-demerger Tata entities — still value
/// correctly instead of falling back to cost). Display-only — never an order.
pub fn my_portfolio() -> Vec<Holding> {
    let h = |symbol: &str, qty: f64, avg_cost: f64, last: f64, sector: &str| Holding {
        symbol: symbol.to_string(),
        qty,
        avg_cost,
        broker: "Groww".to_string(),
        sector: Some(sector.to_string()),
        last_price: Some(last),
    };
    vec![
        h("IDFCFIRSTB", 7676.0, 60.11, 79.22, "Banking"),
        h("TATAPOWER", 701.0, 250.70, 388.95, "Power"),
        h("63MOONS", 481.0, 375.16, 669.50, "Technology"),
        h("TMPV", 970.0, 306.75, 353.20, "Automobiles"),
        h("TMCV", 970.0, 138.78, 431.90, "Automobiles"),
        h("RVNL", 650.0, 173.83, 240.85, "PSU Railways"),
        h("SUZLON", 2200.0, 29.17, 57.14, "Renewables"),
        h("IRFC", 1400.0, 40.88, 91.77, "PSU Finance"),
        h("IOC", 852.0, 135.56, 143.89, "PSU Energy"),
        h("WAAREEENER", 8.0, 3147.35, 3009.40, "Renewables"),
        h("GMRAIRPORT", 350.0, 82.68, 108.44, "Infrastructure"),
        h("HUDCO", 177.0, 101.94, 208.27, "PSU Finance"),
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

// ---------------------------------------------------------------------------
// Correlation diversification — the "real independent bets" measure
// ---------------------------------------------------------------------------
//
// The weight-based `effective_names = 1/Σwᵢ²` only sees position *size*: it
// happily reports a 13-name book as ≈7 "independent bets" even when most of
// those names are the same PSU/infra/renewables trade in different wrappers.
// This block measures the diversification that actually matters — how much the
// holdings *move together* — from daily-return correlation. Still display-only,
// still firewalled (no scoring import), still no order, no advice.

/// Aligned daily-return correlation for the names that had enough common
/// history. `symbols[i]` ↔ row/col `i` of `matrix`. `dropped` lists names left
/// out (no/short archive history) so the figure is never silently partial.
/// Internal transport between the I/O loader and the pure math — not serialized.
pub struct CorrelationContext {
    pub symbols: Vec<String>,
    pub matrix: Vec<Vec<f64>>, // N×N Pearson correlation, diagonal = 1.0
    pub sessions: usize,       // aligned return observations per name
    pub dropped: Vec<String>,
}

/// Pearson correlation matrix of equal-length return series. A zero-variance
/// series correlates 0.0 with everything (never a spurious 1.0); the diagonal
/// is forced to 1.0. Pure and deterministic. Empty input ⇒ empty matrix.
pub fn pearson_matrix(series: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let n = series.len();
    let mut out = vec![vec![0.0; n]; n];
    // Pre-compute mean and (population) std for each series.
    let stats: Vec<(f64, f64)> = series
        .iter()
        .map(|s| {
            let m = if s.is_empty() { 0.0 } else { s.iter().sum::<f64>() / s.len() as f64 };
            let var = if s.is_empty() { 0.0 } else { s.iter().map(|x| (x - m).powi(2)).sum::<f64>() / s.len() as f64 };
            (m, var.sqrt())
        })
        .collect();
    for i in 0..n {
        out[i][i] = 1.0;
        for j in (i + 1)..n {
            let (mi, si) = stats[i];
            let (mj, sj) = stats[j];
            let rho = if si <= f64::EPSILON || sj <= f64::EPSILON || series[i].len() != series[j].len() || series[i].is_empty() {
                0.0
            } else {
                let cov = series[i].iter().zip(&series[j]).map(|(a, b)| (a - mi) * (b - mj)).sum::<f64>()
                    / series[i].len() as f64;
                (cov / (si * sj)).clamp(-1.0, 1.0)
            };
            out[i][j] = rho;
            out[j][i] = rho;
        }
    }
    out
}

/// Correlation-based effective number of independent bets: the participation
/// ratio `N²/Σᵢⱼ ρᵢⱼ²` (= N²/‖C‖²_F). Equals N when names are mutually
/// uncorrelated and → 1 as they all move as one. `None` for an empty matrix.
pub fn effective_bets(corr: &[Vec<f64>]) -> Option<f64> {
    let n = corr.len();
    if n == 0 {
        return None;
    }
    let frob_sq: f64 = corr.iter().flat_map(|row| row.iter()).map(|c| c * c).sum();
    if !frob_sq.is_finite() || frob_sq <= f64::EPSILON {
        return None;
    }
    let pr = (n * n) as f64 / frob_sq;
    pr.is_finite().then_some(pr)
}

/// Mean of the off-diagonal correlation entries (signed). `None` for N < 2.
pub fn avg_pairwise(corr: &[Vec<f64>]) -> Option<f64> {
    let n = corr.len();
    if n < 2 {
        return None;
    }
    let mut sum = 0.0;
    for i in 0..n {
        for j in 0..n {
            if i != j {
                sum += corr[i][j];
            }
        }
    }
    let v = sum / (n * (n - 1)) as f64;
    v.is_finite().then_some(v)
}

/// Single-linkage clusters of names whose pairwise correlation ≥ `threshold`.
/// Each returned cluster has ≥ 2 members; `combined_weight_pct` sums members'
/// book weights from `weight_of` (0 if absent). Sorted by combined weight desc.
/// `basis` states the rule + window honestly. Pure.
pub fn correlation_clusters(
    symbols: &[String],
    corr: &[Vec<f64>],
    weight_of: &HashMap<String, f64>,
    threshold: f64,
    sessions: usize,
) -> Vec<CorrelationCluster> {
    let n = symbols.len();
    // Union-find over indices linked by a strong-enough correlation.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if corr.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0) >= threshold {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let basis = format!("daily-return correlation ≥ {:.2} over {} sessions", threshold, sessions);
    let mut clusters: Vec<CorrelationCluster> = groups
        .into_values()
        .filter(|idxs| idxs.len() >= 2)
        .map(|mut idxs| {
            idxs.sort_by(|&a, &b| {
                let wa = weight_of.get(&symbols[a]).copied().unwrap_or(0.0);
                let wb = weight_of.get(&symbols[b]).copied().unwrap_or(0.0);
                wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
            });
            let members: Vec<String> = idxs.iter().map(|&i| symbols[i].clone()).collect();
            let combined_weight_pct = members.iter().map(|m| weight_of.get(m).copied().unwrap_or(0.0)).sum();
            CorrelationCluster { label: members.join(" · "), members, combined_weight_pct, basis: basis.clone() }
        })
        .collect();
    clusters.sort_by(|a, b| {
        b.combined_weight_pct.partial_cmp(&a.combined_weight_pct).unwrap_or(std::cmp::Ordering::Equal)
    });
    clusters
}

/// Fill the correlation block of an analysis from a computed context, and
/// REPLACE the (sector-fallback) clusters with real price-correlation clusters.
/// Pure; reads book weights from `analysis.holdings`.
pub fn attach_correlation(analysis: &mut PortfolioAnalysis, ctx: &CorrelationContext) {
    let weight_of: HashMap<String, f64> =
        analysis.holdings.iter().map(|h| (h.symbol.clone(), h.weight_pct)).collect();
    analysis.corr_effective_bets = effective_bets(&ctx.matrix);
    analysis.corr_avg_pairwise = avg_pairwise(&ctx.matrix);
    analysis.corr_names_used = ctx.symbols.len();
    analysis.corr_names_dropped = ctx.dropped.clone();
    analysis.corr_sessions = ctx.sessions;
    analysis.corr_basis = format!(
        "daily-return correlation over {} common sessions, {} of {} names with history",
        ctx.sessions, ctx.symbols.len(), analysis.names_total
    );
    analysis.clusters =
        correlation_clusters(&ctx.symbols, &ctx.matrix, &weight_of, crate::config::CORR_CLUSTER_THRESHOLD, ctx.sessions);
}

/// Load up to `lookback` most-recent `(date, close)` daily bars for a symbol,
/// ascending by date. `date` is normalised to a `YYYY-MM-DD` string key (the
/// archive stores `TIMESTAMP WITH TIME ZONE`). `None` when there's no daily file
/// or it's empty. I/O — a read, never an order.
pub fn load_daily_series(
    conn: &duckdb::Connection,
    root: &Path,
    symbol: &str,
    lookback: usize,
) -> Option<Vec<(String, f64)>> {
    let path = crate::config::parquet_path(root, symbol, crate::config::Timeframe::Daily);
    if !path.exists() {
        return None;
    }
    let sql = format!(
        "SELECT CAST(CAST(date AS DATE) AS VARCHAR) AS d, close FROM read_parquet({}) ORDER BY date DESC LIMIT {}",
        crate::storage_kernel::quote_path(&path),
        lookback
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))).ok()?;
    let mut v: Vec<(String, f64)> = rows.filter_map(|r| r.ok()).collect();
    v.reverse(); // archive query is DESC; we want ascending by date
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Pure alignment + return + correlation step (no I/O, unit-testable): take
/// per-name `(date, close)` series, keep names with ≥ `min_sessions`+1 points,
/// align on the common trading dates, build simple daily returns, and correlate.
/// `None` when fewer than two names survive or the common window is too short.
pub fn build_correlation(
    loaded: Vec<(String, Vec<(String, f64)>)>,
    mut dropped: Vec<String>,
    min_sessions: usize,
) -> Option<CorrelationContext> {
    use std::collections::{BTreeMap, BTreeSet};
    let present: Vec<(String, BTreeMap<String, f64>)> = loaded
        .into_iter()
        .filter_map(|(s, v)| {
            if v.len() >= min_sessions + 1 {
                Some((s, v.into_iter().collect()))
            } else {
                dropped.push(s);
                None
            }
        })
        .collect();
    if present.len() < 2 {
        return None;
    }
    // Intersection of trading dates across all present names.
    let mut common: BTreeSet<String> = present[0].1.keys().cloned().collect();
    for (_, m) in &present[1..] {
        let keys: BTreeSet<String> = m.keys().cloned().collect();
        common = common.intersection(&keys).cloned().collect();
    }
    let dates: Vec<String> = common.into_iter().collect(); // BTreeSet ⇒ ascending
    if dates.len() < min_sessions + 1 {
        return None;
    }
    let mut symbols = Vec::with_capacity(present.len());
    let mut series = Vec::with_capacity(present.len());
    for (sym, m) in &present {
        let closes: Vec<f64> = dates.iter().map(|d| *m.get(d).unwrap_or(&0.0)).collect();
        let rets: Vec<f64> = closes
            .windows(2)
            .map(|w| if w[0].abs() > f64::EPSILON { (w[1] - w[0]) / w[0] } else { 0.0 })
            .collect();
        symbols.push(sym.clone());
        series.push(rets);
    }
    let matrix = pearson_matrix(&series);
    Some(CorrelationContext { symbols, matrix, sessions: dates.len() - 1, dropped })
}

/// Load + align + correlate the holdings' daily returns from the archive.
/// `None` when fewer than two names have enough history. I/O orchestration over
/// the pure `build_correlation`; reuses the caller's connection.
pub fn compute_correlation(
    conn: &duckdb::Connection,
    root: &Path,
    symbols: &[String],
    lookback: usize,
    min_sessions: usize,
) -> Option<CorrelationContext> {
    let mut loaded: Vec<(String, Vec<(String, f64)>)> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for s in symbols {
        let sym = s.to_uppercase();
        if !seen.insert(sym.clone()) {
            continue; // de-dup
        }
        match load_daily_series(conn, root, &sym, lookback) {
            Some(v) if v.len() >= min_sessions + 1 => loaded.push((sym, v)),
            _ => dropped.push(sym),
        }
    }
    build_correlation(loaded, dropped, min_sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Direction;
    use crate::types::EligibleEdge;

    fn hold(symbol: &str, qty: f64, avg: f64, broker: &str, sector: Option<&str>) -> Holding {
        Holding { symbol: symbol.into(), qty, avg_cost: avg, broker: broker.into(), sector: sector.map(|s| s.into()), last_price: None }
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
    fn parse_statement_skips_preamble_and_captures_isin_and_close() {
        // A Groww-style statement: preamble rows, then the table header on a later
        // row, then data with company names + ISIN + closing price.
        let csv = "Name,Ramachandran\n\
                   Unique Client Code,4754577622\n\
                   \n\
                   Summary\n\
                   Invested Value,1671932.47\n\
                   \n\
                   Stock Name,ISIN,Quantity,Average buy price,Buy value,Closing price,Closing value,Unrealised P&L\n\
                   63 MOONS TECHNOLOGIES LTD,INE111B01023,481,375.16,180451.96,669.5,322029.5,141577.54\n\
                   IDFC FIRST BANK LIMITED,INE092T01019,7676,60.11,461404.36,79.22,608092.72,146688.36\n";
        let (rows, _w) = parse_csv(csv.as_bytes());
        assert_eq!(rows.len(), 2, "only the two data rows, preamble skipped");
        let moons = &rows[0];
        assert_eq!(moons.symbol, "63 MOONS TECHNOLOGIES LTD");
        assert!((moons.qty - 481.0).abs() < 1e-9);
        assert!((moons.avg_cost - 375.16).abs() < 1e-9, "buy price, not closing price");
        assert_eq!(moons.isin.as_deref(), Some("INE111B01023"));
        assert!((moons.last_price.unwrap() - 669.5).abs() < 1e-9, "closing price captured as mark");
    }

    #[test]
    fn header_matching_is_exact_first_not_loose_substring() {
        // "Account Name" must NOT win sym over "Stock Name"; a "Holding Period"
        // column must NOT win qty over "Quantity"; an explicit avg price column wins.
        let csv = "Account Name,Stock Name,Holding Period,Quantity,Average buy price,Closing price\n\
                   Ramachandran,RELIANCE,2y,10,1300,1450\n";
        let (rows, _w) = parse_csv(csv.as_bytes());
        assert_eq!(rows.len(), 1, "the data row parses");
        assert_eq!(rows[0].symbol, "RELIANCE", "sym from Stock Name, not Account Name");
        assert!((rows[0].qty - 10.0).abs() < 1e-9, "qty from Quantity, not Holding Period");
        assert!((rows[0].avg_cost - 1300.0).abs() < 1e-9);
        assert!((rows[0].last_price.unwrap() - 1450.0).abs() < 1e-9);
    }

    #[test]
    fn best_header_wins_over_a_sparse_summary_section_above() {
        // A summary table prints first, then the detailed equity header (more cols).
        let csv = "Summary\n\
                   Name,Quantity,Average Cost\n\
                   Equity,100,500\n\
                   \n\
                   Stock Name,ISIN,Quantity,Average buy price,Buy value,Closing price\n\
                   TATA POWER CO LTD,INE245A01021,701,250.70,175740.70,388.95\n";
        let (rows, _w) = parse_csv(csv.as_bytes());
        // The detailed header (6 resolved cols) beats the 3-col summary, so we read
        // the real holding, not the summary's "Equity 100 @ 500".
        assert!(rows.iter().any(|r| r.symbol == "TATA POWER CO LTD" && r.isin.as_deref() == Some("INE245A01021")),
            "parsed the detailed equity row, got {:?}", rows.iter().map(|r| &r.symbol).collect::<Vec<_>>());
    }

    #[test]
    fn value_only_header_derives_avg_cost_not_10x() {
        // A statement with a "Buy value" TOTAL but no per-share avg-price column:
        // avg cost must be derived as total/qty (1500), never read as the total.
        let csv = "Stock Name,Quantity,Buy value\nINFY,10,15000\n";
        let (rows, _w) = parse_csv(csv.as_bytes());
        assert_eq!(rows.len(), 1, "the value-only header is recognised, header not eaten");
        assert_eq!(rows[0].symbol, "INFY");
        assert!((rows[0].avg_cost - 1500.0).abs() < 1e-9, "avg = buy value / qty, got {}", rows[0].avg_cost);
    }

    #[test]
    fn unrecognised_header_warns_not_fabricates() {
        // sym + qty recognised but NO price source ("Foobar" is neither avg nor
        // buy value): refuse to fabricate a holding from the header row.
        let csv = "Scrip,Units,Foobar\nINFY,10,1500\n";
        let (rows, warnings) = parse_csv(csv.as_bytes());
        assert!(rows.is_empty(), "no fabricated rows, got {:?}", rows.iter().map(|r| &r.symbol).collect::<Vec<_>>());
        assert!(warnings.iter().any(|w| w.contains("no recognised header")));
    }

    #[test]
    fn parse_tsv_paste_with_header_detection() {
        // Tab-separated paste (as copied from Excel), header present.
        let tsv = "Stock Name\tQuantity\tAverage buy price\tClosing price\n\
                   SUZLON ENERGY LIMITED\t2200\t29.17\t57.14\n";
        let (rows, _w) = parse_csv(tsv.as_bytes());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol, "SUZLON ENERGY LIMITED");
        assert!((rows[0].last_price.unwrap() - 57.14).abs() < 1e-9);
    }

    #[test]
    fn parse_headerless_positional_does_not_eat_first_row() {
        // The documented paste format with no header — the first row must NOT be
        // consumed as a header (the old has_headers(true) bug).
        let (rows, _w) = parse_csv("RELIANCE,10,1300\nTCS,5,3800\n".as_bytes());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].symbol, "RELIANCE");
        assert_eq!(rows[1].symbol, "TCS");
    }

    #[test]
    fn merge_holdings_sums_qty_weights_cost_and_joins_brokers() {
        let hs = vec![
            Holding { symbol: "IDFCFIRSTB".into(), qty: 100.0, avg_cost: 60.0, broker: "Groww".into(), sector: Some("Banking".into()), last_price: Some(79.0) },
            Holding { symbol: "IDFCFIRSTB".into(), qty: 100.0, avg_cost: 80.0, broker: "Zerodha".into(), sector: None, last_price: None },
            Holding { symbol: "IOC".into(), qty: 10.0, avg_cost: 130.0, broker: "Groww".into(), sector: Some("Energy".into()), last_price: None },
        ];
        let merged = merge_holdings(hs);
        assert_eq!(merged.len(), 2, "duplicate symbol collapses to one row");
        let idfc = merged.iter().find(|h| h.symbol == "IDFCFIRSTB").unwrap();
        assert!((idfc.qty - 200.0).abs() < 1e-9);
        assert!((idfc.avg_cost - 70.0).abs() < 1e-9, "qty-weighted avg (60*100+80*100)/200");
        assert_eq!(idfc.broker, "Groww/Zerodha");
        assert_eq!(idfc.sector.as_deref(), Some("Banking"), "first non-empty sector kept");
        assert_eq!(idfc.last_price, Some(79.0), "first available last_price kept");
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

    // --- Correlation diversification --------------------------------------

    #[test]
    fn pearson_perfect_anti_and_diag() {
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let scaled = vec![2.0, 4.0, 6.0, 8.0]; // perfectly correlated
        let anti = vec![4.0, 3.0, 2.0, 1.0]; // perfectly anti-correlated
        let m = pearson_matrix(&[a, scaled, anti]);
        assert!((m[0][0] - 1.0).abs() < 1e-9 && (m[1][1] - 1.0).abs() < 1e-9, "diagonal is 1");
        assert!((m[0][1] - 1.0).abs() < 1e-9, "perfect +corr = 1, got {}", m[0][1]);
        assert!((m[0][2] + 1.0).abs() < 1e-9, "perfect -corr = -1, got {}", m[0][2]);
        assert!((m[0][1] - m[1][0]).abs() < 1e-12, "symmetric");
    }

    #[test]
    fn pearson_zero_variance_is_zero_not_nan() {
        let flat = vec![5.0, 5.0, 5.0, 5.0]; // zero variance
        let m = pearson_matrix(&[flat, vec![1.0, 2.0, 3.0, 4.0]]);
        assert_eq!(m[0][1], 0.0, "flat series correlates 0, never NaN/1");
        assert!(!m[0][1].is_nan());
    }

    #[test]
    fn effective_bets_spans_one_to_n() {
        // Identity (mutually uncorrelated) ⇒ exactly N.
        let ident = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]];
        assert!((effective_bets(&ident).unwrap() - 3.0).abs() < 1e-9);
        // All-ones (everything moves as one) ⇒ exactly 1.
        let ones = vec![vec![1.0, 1.0, 1.0], vec![1.0, 1.0, 1.0], vec![1.0, 1.0, 1.0]];
        assert!((effective_bets(&ones).unwrap() - 1.0).abs() < 1e-9);
        // Empty ⇒ None.
        assert!(effective_bets(&[]).is_none());
    }

    #[test]
    fn avg_pairwise_known_and_guards() {
        let m = vec![vec![1.0, 0.5], vec![0.5, 1.0]];
        assert!((avg_pairwise(&m).unwrap() - 0.5).abs() < 1e-9);
        assert!(avg_pairwise(&[vec![1.0]]).is_none(), "N<2 ⇒ None");
    }

    #[test]
    fn correlation_clusters_links_only_strong_pairs() {
        let syms: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        // A~B strong (0.8), C weak to both (0.1).
        let m = vec![vec![1.0, 0.8, 0.1], vec![0.8, 1.0, 0.1], vec![0.1, 0.1, 1.0]];
        let w = HashMap::from([("A".to_string(), 30.0), ("B".to_string(), 20.0), ("C".to_string(), 50.0)]);
        let cl = correlation_clusters(&syms, &m, &w, 0.60, 250);
        assert_eq!(cl.len(), 1, "exactly one cluster (A,B); C excluded");
        assert_eq!(cl[0].members.len(), 2);
        assert!(cl[0].members.contains(&"A".to_string()) && cl[0].members.contains(&"B".to_string()));
        assert!((cl[0].combined_weight_pct - 50.0).abs() < 1e-9, "30+20");
        assert!(cl[0].basis.contains("0.60") && cl[0].basis.contains("250"));
    }

    #[test]
    fn build_correlation_aligns_drops_short_and_returns_perfect() {
        let dates = ["2025-01-01", "2025-01-02", "2025-01-03", "2025-01-04", "2025-01-05"];
        let mk = |closes: &[f64]| -> Vec<(String, f64)> {
            dates.iter().zip(closes).map(|(d, c)| (d.to_string(), *c)).collect()
        };
        let a = ("AAA".to_string(), mk(&[100.0, 101.0, 102.0, 103.0, 104.0]));
        let b = ("BBB".to_string(), mk(&[200.0, 202.0, 204.0, 206.0, 208.0])); // same % moves ⇒ corr 1
        let short = ("CCC".to_string(), vec![("2025-01-01".to_string(), 10.0), ("2025-01-02".to_string(), 11.0)]);
        let ctx = build_correlation(vec![a, b, short], vec![], 2).expect("two good names");
        assert_eq!(ctx.symbols, vec!["AAA".to_string(), "BBB".to_string()]);
        assert_eq!(ctx.dropped, vec!["CCC".to_string()], "the 2-point name is dropped");
        assert_eq!(ctx.sessions, 4, "5 common dates ⇒ 4 returns");
        assert!((ctx.matrix[0][1] - 1.0).abs() < 1e-9, "AAA,BBB perfectly correlated");
        // Two perfectly-correlated names ⇒ ~1 effective bet.
        assert!((effective_bets(&ctx.matrix).unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn build_correlation_none_when_too_few_or_too_short() {
        // Only one usable name ⇒ None.
        let one = ("AAA".to_string(), (0..5).map(|i| (format!("d{i}"), 100.0 + i as f64)).collect());
        assert!(build_correlation(vec![one], vec![], 2).is_none());
        // Two names but common window shorter than min_sessions+1 ⇒ None.
        let a = ("A".to_string(), vec![("d1".into(), 1.0), ("d2".into(), 2.0), ("d3".into(), 3.0)]);
        let b = ("B".to_string(), vec![("d1".into(), 1.0), ("d2".into(), 2.0), ("d3".into(), 3.0)]);
        assert!(build_correlation(vec![a, b], vec![], 5).is_none(), "3 dates < 5+1");
    }

    #[test]
    fn attach_correlation_sets_block_and_replaces_clusters() {
        let hs = vec![hold("A", 1.0, 100.0, "Z", Some("IT")), hold("B", 1.0, 100.0, "Z", Some("Energy"))];
        let mut a = analyze(&hs, &HashMap::new(), &EdgeIndex::new(), "t".into());
        assert!(a.corr_effective_bets.is_none(), "starts unset (pure analyze)");
        let ctx = CorrelationContext {
            symbols: vec!["A".to_string(), "B".to_string()],
            matrix: vec![vec![1.0, 0.9], vec![0.9, 1.0]],
            sessions: 120,
            dropped: vec!["C".to_string()],
        };
        attach_correlation(&mut a, &ctx);
        assert!(a.corr_effective_bets.unwrap() < 1.2, "highly correlated ⇒ ~1 bet");
        assert!((a.corr_avg_pairwise.unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(a.corr_names_used, 2);
        assert_eq!(a.corr_sessions, 120);
        assert_eq!(a.corr_names_dropped, vec!["C".to_string()]);
        assert_eq!(a.clusters.len(), 1, "0.9 ≥ 0.60 ⇒ one correlation cluster");
        assert!(a.clusters[0].basis.contains("correlation"));
    }
}

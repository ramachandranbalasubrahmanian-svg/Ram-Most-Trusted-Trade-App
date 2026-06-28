//! Capital horizon planner — "if I have ₹X to invest for N years, which names fit?"
//!
//! Scans the broad NSE universe (the archive's adjusted daily history) and ranks
//! candidates with a HORIZON-AWARE, backtest-grounded score: trailing CAGR,
//! relative strength vs NIFTY, trend, max-drawdown, a full-history consistency
//! record, and size/quality. The weighting shifts with the horizon — short = more
//! momentum/relative-strength; long = more compounding + low-drawdown + quality.
//! A name that ALSO has an eligible edge in our backtested edge map is tagged.
//!
//! Honesty firewall (mirrors `holdings_analytics`/`portfolio_rotation`): imports
//! ONLY `types`, `config`, `storage_kernel`, and `strategy_engine::EdgeIndex` —
//! never `stats`/scoring/confidence. EVERY figure is HISTORICAL/descriptive, never
//! a forecast; the output is a screen + evidence, NOT advice and NEVER an order.
//! Hard quality/liquidity/sanity floors keep micro-cap pumps off the list.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use duckdb::Connection;

use crate::storage_kernel;
use crate::strategy_engine::EdgeIndex;
use crate::types::{CapitalPick, CapitalPlan};
use crate::config::Direction;

const TD: usize = 252; // trading days per year
const CAGR_CLIP: f64 = 0.60; // winsorize trailing CAGR in SCORING (a parabolic pop can't dominate)
const HIGH_CAGR_FLAG: f64 = 80.0; // trailing CAGR above this is flagged "unusually high"
const STALE_DAYS_MAX: i32 = 30; // a name whose last bar is older than this is skipped (delisted/halted)
const TOP_N: usize = 8;
const MAX_PER_SECTOR: usize = 2;
const MAX_WEIGHT: f64 = 0.25;

pub const PLANNER_DISCLAIMER: &str = "Historical screen — NOT advice, NOT a forecast, and never an order. \
Every figure (CAGR, drawdown, relative strength, consistency) is PAST performance and typically does NOT repeat. \
It also screens only stocks still listed today, so it can't see companies that failed or delisted — the record shown is \
survivor-biased and flatters buy-and-hold. These are candidates that fit the horizon on the evidence; you decide what \
(if anything) to buy and size it yourself.";

/// The valid horizons offered in the UI.
pub fn valid_years(y: u32) -> u32 {
    match y {
        1 | 2 | 3 | 5 | 10 => y,
        _ => 5,
    }
}

#[derive(Clone, Copy)]
struct Weights {
    cagr: f64,
    rs: f64,
    trend: f64,
    dd: f64,
    consistency: f64,
    quality: f64,
}

fn weights_for(years: u32) -> Weights {
    match years {
        1 => Weights { cagr: 0.20, rs: 0.35, trend: 0.25, dd: 0.05, consistency: 0.05, quality: 0.10 },
        2 => Weights { cagr: 0.25, rs: 0.30, trend: 0.20, dd: 0.05, consistency: 0.10, quality: 0.10 },
        3 => Weights { cagr: 0.30, rs: 0.20, trend: 0.15, dd: 0.10, consistency: 0.15, quality: 0.10 },
        5 => Weights { cagr: 0.30, rs: 0.10, trend: 0.05, dd: 0.20, consistency: 0.20, quality: 0.15 },
        _ => Weights { cagr: 0.28, rs: 0.05, trend: 0.02, dd: 0.25, consistency: 0.20, quality: 0.20 },
    }
}

// floors scale with the horizon (₹cr market cap, ₹cr median daily turnover, rolling-1y win-rate)
fn mcap_floor_cr(y: u32) -> f64 {
    match y {
        1 | 2 => 3000.0,
        3 => 5000.0,
        5 => 10000.0,
        _ => 20000.0,
    }
}
fn liq_floor_cr(y: u32) -> f64 {
    if y <= 3 {
        5.0
    } else {
        3.0
    }
}
fn consistency_floor(y: u32) -> f64 {
    match y {
        1 | 2 => 0.45,
        3 => 0.50,
        5 => 0.55,
        _ => 0.60,
    }
}

// --- price-series primitives (descriptive, never a forecast) ----------------

fn sma_tail(c: &[f64], n: usize) -> f64 {
    let k = n.min(c.len());
    if k == 0 {
        return 0.0;
    }
    c[c.len() - k..].iter().sum::<f64>() / k as f64
}

/// Annualised return over `years` (index-based, 252 td/yr). None if too short.
fn cagr(c: &[f64], years: u32) -> Option<f64> {
    let n = years as usize * TD;
    if c.len() <= n {
        return None;
    }
    let base = c[c.len() - 1 - n];
    let last = c[c.len() - 1];
    if base > 0.0 {
        Some(((last / base).powf(1.0 / years as f64) - 1.0) * 100.0)
    } else {
        None
    }
}

fn ret_window(c: &[f64], n: usize) -> Option<f64> {
    if c.len() > n {
        let base = c[c.len() - 1 - n];
        if base > 0.0 {
            return Some((c[c.len() - 1] / base - 1.0) * 100.0);
        }
    }
    None
}

/// Max drawdown (negative %) over the last `n` bars.
fn max_drawdown(c: &[f64], n: usize) -> f64 {
    let start = c.len().saturating_sub(n);
    let mut peak = f64::MIN;
    let mut mdd = 0.0;
    for &x in &c[start..] {
        if x > peak {
            peak = x;
        }
        if peak > 0.0 {
            let dd = x / peak - 1.0;
            if dd < mdd {
                mdd = dd;
            }
        }
    }
    mdd * 100.0
}

/// Annualised volatility (%) of daily returns over the last `n` bars.
fn volatility(c: &[f64], n: usize) -> f64 {
    let start = c.len().saturating_sub(n + 1);
    let rets: Vec<f64> = c[start..].windows(2).filter(|w| w[0] > 0.0).map(|w| w[1] / w[0] - 1.0).collect();
    if rets.len() < 2 {
        return 0.0;
    }
    let m = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - m).powi(2)).sum::<f64>() / rets.len() as f64;
    var.sqrt() * (TD as f64).sqrt() * 100.0
}

/// Fraction of rolling 1-year holds (sampled monthly over the FULL history) that
/// were positive — a stable "does it compound" record, horizon-independent.
fn consistency(c: &[f64]) -> f64 {
    if c.len() <= TD {
        return 0.0;
    }
    let (mut hits, mut total) = (0usize, 0usize);
    let mut i = TD;
    while i < c.len() {
        let base = c[i - TD];
        if base > 0.0 {
            total += 1;
            if c[i] > base {
                hits += 1;
            }
        }
        i += 21;
    }
    if total == 0 {
        0.0
    } else {
        hits as f64 / total as f64
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

struct Series {
    closes: Vec<f64>,
    turn: Vec<f64>,
    last_day: i32,
}

struct Feat {
    sym: String,
    sector: String,
    last: f64,
    vs200: f64,
    cagr: f64,
    max_dd: f64,
    vol: f64,
    rs: f64,
    consistency: f64,
    mcap_cr: f64,
    liq_cr: f64,
    edge: Option<(String, f64, f64, f64, usize)>, // strategy, expectancy_r, pf, win%, n
}

fn z(vals: &[f64]) -> Vec<f64> {
    let n = vals.len();
    if n == 0 {
        return vec![];
    }
    let m = vals.iter().sum::<f64>() / n as f64;
    let var = vals.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n as f64;
    let sd = var.sqrt();
    if sd <= 1e-9 {
        return vec![0.0; n];
    }
    vals.iter().map(|x| (x - m) / sd).collect()
}

// --- I/O loaders (read-only) -------------------------------------------------

/// Load every symbol's adjusted daily close + ₹ turnover from the bulk archive.
/// Read UNORDERED (cheap) and sort each symbol's series by date in Rust — far
/// faster than a 5M-row `ORDER BY` in DuckDB.
fn load_universe(conn: &Connection, root: &Path) -> (HashMap<String, Series>, i32) {
    let path = root.join("nse_daily_all.parquet");
    let mut raw: HashMap<String, Vec<(i32, f64, f64)>> = HashMap::new();
    let mut global_last = i32::MIN;
    if !path.exists() {
        return (HashMap::new(), 0);
    }
    let sql = format!(
        "SELECT symbol, (CAST(date AS DATE) - DATE '2000-01-01') AS d, \"adj close\" AS c, close*volume AS turn \
         FROM read_parquet({}) WHERE \"adj close\" > 0",
        storage_kernel::quote_path(&path)
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return (HashMap::new(), 0),
    };
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?))
    });
    if let Ok(rows) = rows {
        for (sym, d, c, turn) in rows.flatten() {
            if d > global_last {
                global_last = d;
            }
            raw.entry(sym).or_default().push((d, c, if turn.is_finite() && turn > 0.0 { turn } else { 0.0 }));
        }
    }
    let mut out: HashMap<String, Series> = HashMap::with_capacity(raw.len());
    for (sym, mut rows) in raw {
        rows.sort_by_key(|r| r.0);
        let last_day = rows.last().map(|r| r.0).unwrap_or(i32::MIN);
        let closes = rows.iter().map(|r| r.1).collect();
        let turn = rows.iter().map(|r| r.2).collect();
        out.insert(sym, Series { closes, turn, last_day });
    }
    (out, global_last)
}

/// The heavy, date-stable inputs to the planner, cached so only the FIRST request
/// of the day pays the archive read; later horizon/capital changes are instant.
struct PlannerData {
    universe: HashMap<String, Series>,
    nifty: Vec<f64>,
    meta: HashMap<String, (f64, String)>,
    global_last: i32,
}

static CACHE: OnceLock<Mutex<Option<(String, Arc<PlannerData>)>>> = OnceLock::new();

/// Get (or build + cache) the planner inputs for the given IST date.
fn planner_data(conn: &Connection, root: &Path, day: &str) -> Arc<PlannerData> {
    let cell = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(g) = cell.lock() {
        if let Some((d, data)) = &*g {
            if d == day {
                return data.clone();
            }
        }
    }
    let (universe, global_last) = load_universe(conn, root);
    let nifty = load_nifty(conn, root);
    let meta = load_meta(conn, root);
    let data = Arc::new(PlannerData { universe, nifty, meta, global_last });
    if let Ok(mut g) = cell.lock() {
        *g = Some((day.to_string(), data.clone()));
    }
    data
}

/// NIFTY50 adjusted closes (for relative strength).
fn load_nifty(conn: &Connection, root: &Path) -> Vec<f64> {
    let path = root.join("index_daily_all.parquet");
    if !path.exists() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT \"adj close\" FROM read_parquet({}) WHERE index_name='NIFTY50' AND \"adj close\">0 ORDER BY date",
        storage_kernel::quote_path(&path)
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, f64>(0)) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Market cap (₹cr) + sector per symbol, from the metadata parquet.
fn load_meta(conn: &Connection, root: &Path) -> HashMap<String, (f64, String)> {
    let path = root.join("symbol_metadata.parquet");
    let mut out = HashMap::new();
    if !path.exists() {
        return out;
    }
    let sql = format!(
        "SELECT symbol, COALESCE(market_cap_inr,0), COALESCE(sector,'Unknown') FROM read_parquet({})",
        storage_kernel::quote_path(&path)
    );
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?))
        }) {
            for (sym, mc, sec) in rows.flatten() {
                out.insert(sym.to_uppercase(), (mc / 1e7, sec));
            }
        }
    }
    out
}

// --- the planner --------------------------------------------------------------

/// Build the horizon plan. `capital` is split across the top picks (inverse-vol,
/// capped). Read-only; descriptive; never an order.
pub fn build(conn: &Connection, root: &Path, edges: &EdgeIndex, capital: f64, years_in: u32, built_ist: String) -> CapitalPlan {
    let years = valid_years(years_in);
    let day = built_ist.get(0..10).unwrap_or("").to_string();
    let data = planner_data(conn, root, &day);
    let (universe, nifty, meta, global_last) = (&data.universe, &data.nifty, &data.meta, data.global_last);
    let nr = ret_window(nifty, years as usize * TD);

    // Require ≥2 years of history regardless of horizon, so the consistency
    // record is built from many rolling windows, not one degenerate sample.
    let need = (years as usize * TD).max(2 * TD) + 5;
    let rs_available = nr.is_some();
    let mcap_floor = mcap_floor_cr(years);
    let liq_floor = liq_floor_cr(years);
    let cons_floor = consistency_floor(years);

    let mut feats: Vec<Feat> = Vec::new();
    for (sym, s) in universe {
        // recency: skip delisted/halted names
        if global_last - s.last_day > STALE_DAYS_MAX {
            continue;
        }
        if s.closes.len() < need {
            continue;
        }
        let last = *s.closes.last().unwrap();
        if last <= 0.0 {
            continue;
        }
        let cg = match cagr(&s.closes, years) {
            Some(v) => v,
            None => continue,
        };
        let vs200 = (last / sma_tail(&s.closes, 200) - 1.0) * 100.0;
        // gates: positive long-run trend + positive horizon CAGR
        if cg <= 0.0 || vs200 <= 0.0 {
            continue;
        }
        let (mcap_cr, sector) = meta.get(sym).cloned().unwrap_or((0.0, "Unknown".to_string()));
        if mcap_cr < mcap_floor {
            continue;
        }
        let mut tail: Vec<f64> = s.turn[s.turn.len().saturating_sub(60)..].to_vec();
        let liq_cr = median(&mut tail) / 1e7;
        if liq_cr < liq_floor {
            continue;
        }
        let cons = consistency(&s.closes);
        if cons < cons_floor {
            continue;
        }
        let sret = ret_window(&s.closes, years as usize * TD);
        let rs = match (sret, nr) {
            (Some(a), Some(b)) => a - b,
            _ => 0.0,
        };
        let edge = edges.get(sym).and_then(|v| {
            v.iter()
                .filter(|e| matches!(e.direction, Direction::Long))
                .max_by(|a, b| a.expectancy_r.partial_cmp(&b.expectancy_r).unwrap_or(std::cmp::Ordering::Equal))
                .map(|e| (e.strategy.clone(), e.expectancy_r, e.profit_factor, e.win_pct, e.n))
        });
        feats.push(Feat {
            sym: sym.clone(),
            sector,
            last,
            vs200,
            cagr: cg,
            max_dd: max_drawdown(&s.closes, years as usize * TD),
            vol: volatility(&s.closes, years as usize * TD),
            rs,
            consistency: cons * 100.0,
            mcap_cr,
            liq_cr,
            edge,
        });
    }

    let scanned = feats.len();
    if feats.is_empty() {
        return CapitalPlan {
            horizon_years: years,
            capital,
            picks: Vec::new(),
            deployed: 0.0,
            leftover_cash: capital,
            universe_scanned: 0,
            rs_available,
            methodology: methodology(years, rs_available),
            disclaimer: PLANNER_DISCLAIMER.to_string(),
            built_ist,
        };
    }

    // z-score each factor across the eligible set, then a horizon-weighted blend.
    let w = weights_for(years);
    let zc = z(&feats.iter().map(|f| f.cagr.min(CAGR_CLIP * 100.0)).collect::<Vec<_>>());
    let zr = z(&feats.iter().map(|f| f.rs).collect::<Vec<_>>());
    let zt = z(&feats.iter().map(|f| f.vs200).collect::<Vec<_>>());
    let zd = z(&feats.iter().map(|f| f.max_dd).collect::<Vec<_>>()); // higher (less negative) is better
    let zk = z(&feats.iter().map(|f| f.consistency).collect::<Vec<_>>());
    let zq = z(&feats.iter().map(|f| (f.mcap_cr.max(0.0) + 1.0).ln()).collect::<Vec<_>>());

    let mut scored: Vec<(f64, usize)> = (0..feats.len())
        .map(|i| {
            let mut s = w.cagr * zc[i] + w.rs * zr[i] + w.trend * zt[i] + w.dd * zd[i] + w.consistency * zk[i] + w.quality * zq[i];
            if feats[i].edge.is_some() {
                s += 0.25; // a backtested edge is a tie-breaking bonus, not a gate
            }
            (s, i)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // diversify: at most MAX_PER_SECTOR per sector, take TOP_N.
    let mut chosen: Vec<usize> = Vec::new();
    let mut per_sector: HashMap<String, usize> = HashMap::new();
    for (_, i) in &scored {
        let sec = feats[*i].sector.clone();
        let cnt = per_sector.entry(sec).or_insert(0);
        if *cnt >= MAX_PER_SECTOR {
            continue;
        }
        *cnt += 1;
        chosen.push(*i);
        if chosen.len() >= TOP_N {
            break;
        }
    }

    // allocate inverse-vol with a per-name cap, via water-filling: freeze any name
    // that hits the cap at the cap and redistribute the remainder proportionally to
    // the rest, repeating until stable — so the cap genuinely holds (a single
    // clamp-then-renormalise would re-inflate capped weights back over the cap).
    // The cap loosens to equal-weight when there are few names, so capital still
    // fully allocates.
    let n_pick = chosen.len();
    let cap = MAX_WEIGHT.max(1.0 / n_pick.max(1) as f64);
    let inv: Vec<f64> = chosen.iter().map(|&i| 1.0 / feats[i].vol.max(5.0)).collect();
    let mut wts = vec![0.0; n_pick];
    let mut capped = vec![false; n_pick];
    loop {
        let capped_sum: f64 = (0..n_pick).filter(|&i| capped[i]).map(|i| wts[i]).sum();
        let free_inv: f64 = (0..n_pick).filter(|&i| !capped[i]).map(|i| inv[i]).sum();
        let remaining = (1.0 - capped_sum).max(0.0);
        if free_inv <= 0.0 {
            break;
        }
        let mut newly = false;
        for i in 0..n_pick {
            if capped[i] {
                continue;
            }
            wts[i] = remaining * inv[i] / free_inv;
            if wts[i] > cap + 1e-9 {
                wts[i] = cap;
                capped[i] = true;
                newly = true;
            }
        }
        if !newly {
            break;
        }
    }

    // Whole-share counts from each target slice, then a greedy top-up: spend
    // leftover on the most-under-funded affordable name, so a high-priced name
    // rounding to 0 doesn't strand a big chunk of the capital.
    let mut shares: Vec<i64> = chosen
        .iter()
        .enumerate()
        .map(|(k, &i)| if feats[i].last > 0.0 { (capital * wts[k] / feats[i].last).floor() as i64 } else { 0 })
        .collect();
    let mut leftover = capital - chosen.iter().enumerate().map(|(k, &i)| shares[k] as f64 * feats[i].last).sum::<f64>();
    loop {
        // pick the affordable name furthest below its target ₹ allocation.
        let mut best: Option<(usize, f64)> = None;
        for (k, &i) in chosen.iter().enumerate() {
            let price = feats[i].last;
            if price <= 0.0 || price > leftover {
                continue;
            }
            let target = capital * wts[k];
            let current = shares[k] as f64 * price;
            let deficit = target - current;
            if best.map_or(true, |(_, d)| deficit > d) {
                best = Some((k, deficit));
            }
        }
        match best {
            Some((k, _)) => {
                shares[k] += 1;
                leftover -= feats[chosen[k]].last;
            }
            None => break,
        }
    }

    let mut picks: Vec<CapitalPick> = Vec::new();
    let mut deployed = 0.0;
    for (k, &i) in chosen.iter().enumerate() {
        let f = &feats[i];
        let alloc = capital * wts[k];
        let shares = shares[k];
        let spent = shares as f64 * f.last;
        deployed += spent;
        let (edge_backed, edge_note) = match &f.edge {
            Some((strat, _exp, pf, win, n)) => (
                true,
                format!("backtested edge: {} (PF {:.2}, {:.0}% win, n={})", strat, pf, win, n),
            ),
            None => (false, "no backtested edge in our map (still screened on price evidence)".to_string()),
        };
        let note = if shares == 0 {
            format!("1 share = ₹{:.0} exceeds this slice — give it a bigger share or skip", f.last)
        } else {
            String::new()
        };
        picks.push(CapitalPick {
            symbol: f.sym.clone(),
            sector: f.sector.clone(),
            last: (f.last * 100.0).round() / 100.0,
            alloc_rupees: alloc.round(),
            shares,
            weight_pct: (wts[k] * 1000.0).round() / 10.0,
            cagr_pct: f.cagr.round(),
            max_dd_pct: f.max_dd.round(),
            rs_vs_nifty_pct: f.rs.round(),
            consistency_pct: f.consistency.round(),
            mcap_cr: f.mcap_cr.round(),
            edge_backed,
            edge_note,
            high_cagr_flag: f.cagr > HIGH_CAGR_FLAG,
            note,
        });
    }

    CapitalPlan {
        horizon_years: years,
        capital,
        picks,
        deployed: deployed.round(),
        leftover_cash: (capital - deployed).round(),
        universe_scanned: scanned,
        rs_available,
        methodology: methodology(years, rs_available),
        disclaimer: PLANNER_DISCLAIMER.to_string(),
        built_ist,
    }
}

/// Empty plan (no DB connection) — still ships methodology + disclaimer so the UI
/// renders consistently.
pub fn empty(capital: f64, years_in: u32, built_ist: String) -> CapitalPlan {
    let years = valid_years(years_in);
    CapitalPlan {
        horizon_years: years,
        capital,
        picks: Vec::new(),
        deployed: 0.0,
        leftover_cash: capital,
        universe_scanned: 0,
        rs_available: true,
        methodology: methodology(years, true),
        disclaimer: PLANNER_DISCLAIMER.to_string(),
        built_ist,
    }
}

fn methodology(years: u32, rs_available: bool) -> String {
    let tilt = if years <= 2 {
        "weighted toward relative strength + trend (momentum factor)"
    } else if years <= 3 {
        "a balance of growth, trend and consistency"
    } else {
        "weighted toward long-run compounding, low drawdown, consistency and size/quality"
    };
    let rs_note = if rs_available {
        ""
    } else {
        " (relative strength was omitted this run — NIFTY history was unavailable)"
    };
    format!(
        "Scanned the broad NSE universe over adjusted daily history. Kept only liquid (≥₹{:.0}cr/day), \
         sized (≥₹{:.0}cr market cap) names with ≥2 years of history, in an uptrend with a positive {}-year record, \
         then ranked {} — all on PAST data{}. Allocation is inverse-volatility, capped at {:.0}% per name, \
         diversified ≤{} per sector.",
        liq_floor_cr(years), mcap_floor_cr(years), years, tilt, rs_note, MAX_WEIGHT * 100.0, MAX_PER_SECTOR
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize, start: f64, step: f64) -> Vec<f64> {
        (0..n).map(|i| start + step * i as f64).collect()
    }

    #[test]
    fn cagr_and_drawdown_basic() {
        // doubling over exactly one year (252 td) ⇒ ~100% CAGR.
        let mut c = vec![100.0];
        for _ in 0..TD {
            c.push(c.last().unwrap() * (2.0f64).powf(1.0 / TD as f64));
        }
        let cg = cagr(&c, 1).unwrap();
        assert!((cg - 100.0).abs() < 2.0, "cagr {cg}");
        // a monotonic ramp has ~0 drawdown.
        assert!(max_drawdown(&ramp(300, 100.0, 1.0), 252).abs() < 1e-6);
    }

    #[test]
    fn drawdown_detects_a_dip() {
        let mut c = ramp(100, 100.0, 1.0); // up to ~199
        c.extend(ramp(20, 199.0, -5.0)); // fall to ~99 → ~-50% from peak
        let dd = max_drawdown(&c, 200);
        assert!(dd < -40.0 && dd > -60.0, "dd {dd}");
    }

    #[test]
    fn consistency_all_up_is_one() {
        let c = ramp(TD * 4, 100.0, 1.0); // always higher 1y later
        assert!((consistency(&c) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn zscore_centers_and_scales() {
        let zz = z(&[1.0, 2.0, 3.0]);
        assert!((zz[1]).abs() < 1e-9, "middle is ~0");
        assert!(zz[0] < 0.0 && zz[2] > 0.0);
    }

    #[test]
    fn valid_years_clamps() {
        assert_eq!(valid_years(7), 5);
        assert_eq!(valid_years(10), 10);
        assert_eq!(valid_years(1), 1);
    }
}

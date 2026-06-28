//! Portfolio rotation & growth — the Desk's "leaders to keep / laggards to rotate /
//! edge-backed uptrend buys" layer, on top of the holdings risk picture.
//!
//! Honesty firewall (mirrors `holdings_analytics`): this module imports ONLY
//! `types`, `config`, `storage_kernel` (read-only candle access) and
//! `strategy_engine::EdgeIndex` — never `stats`/confidence/scoring. Every output
//! is display-only and structurally cannot gate or inflate Confidence.
//!
//! What it does NOT do, deliberately: it never places/modifies/cancels an order;
//! `action` buckets are descriptive (what price IS doing + edge presence), never a
//! buy/sell directive; "growth" is shown as portfolio scenario RANGES, never a
//! per-stock forecast; and demerger-distorted tickers (Tata Motors TMLCV/TMPV,
//! whose price history is split by the 2025 corporate action) are NEVER auto-flagged
//! for rotation — their trend signal is an artifact, judged by hand.
//!
//! Since this archive carries no analyst price targets, the quality filter for a
//! "buy candidate" is the platform's own edge map: a name must have an eligible,
//! DSR-gated LONG edge AND be in a price uptrend beating NIFTY.

use std::collections::HashSet;
use std::path::Path;

use duckdb::Connection;

use crate::config::{Direction, Timeframe};
use crate::storage_kernel;
use crate::strategy_engine::EdgeIndex;
use crate::types::{
    BuyCandidate, GrowthScenario, HoldingAnalysis, RebalanceBuy, RebalancePlan, RebalanceProfile,
    RebalanceSell, RotationAnalysis, RotationRow,
};

pub const ROTATION_DISCLAIMER: &str = "Rotation view — descriptive evidence, NOT advice or an order. \
Trend & relative-strength describe what price IS doing; buy candidates are edge-backed names in an \
uptrend, not recommendations; 'growth' is shown as scenario ranges, never a per-stock forecast.";

pub const DECADE_NOTE: &str = "Indian large-cap has historically compounded ~11-13% nominal over rolling \
decades — with 30-50% drawdowns along the way. Single-stock 10-year forecasts are not knowable; \
diversification and rotation discipline matter more.";

const LTCG_RATE: f64 = 0.125;
const LTCG_EXEMPTION: f64 = 125_000.0;
const TRIM_WEIGHT_PCT: f64 = 12.0;
const ROTATE_RS_CUT: f64 = -15.0;
const OFF_HIGH_MIN: f64 = -18.0;
const MIN_HISTORY: usize = 60;
const BUY_CANDIDATES_MAX: usize = 12;
const REDEPLOY_NAMES: usize = 5;

fn is_demerger_distorted(sym: &str) -> bool {
    // The 2025 Tata Motors demerger split the price history across these tickers
    // (TMCV = the commercial-vehicle entity, TMPV = passenger vehicles), so their
    // trend signal is an artifact — never auto-flag them for rotation.
    matches!(sym, "TMPV" | "TMLCV" | "TMCV" | "TATAMOTORS")
}

/// Best-effort theme tag from a (user-provided) sector label, for the before/after
/// concentration read. Universe buys have no sector here, so they read as non-theme.
fn is_theme(sector: &str) -> bool {
    let s = sector.to_ascii_lowercase();
    s.contains("psu") || s.contains("power") || s.contains("energy") || s.contains("renew")
}

fn r0(x: f64) -> f64 {
    x.round()
}
fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// --- price-series primitives (no external helpers exist for these) ----------

/// % return from the last close vs the close `n` bars earlier. None if too short.
fn ret_n(closes: &[f64], n: usize) -> Option<f64> {
    if closes.len() > n {
        let base = closes[closes.len() - 1 - n];
        if base > 0.0 {
            return Some((closes[closes.len() - 1] / base - 1.0) * 100.0);
        }
    }
    None
}

fn sma(closes: &[f64], n: usize) -> f64 {
    let len = closes.len();
    let k = n.min(len);
    if k == 0 {
        return 0.0;
    }
    closes[len - k..].iter().sum::<f64>() / k as f64
}

/// Annualised return over `years` (252 trading days/yr). Descriptive, not a forecast.
fn cagr(closes: &[f64], years: usize) -> Option<f64> {
    let n = years * 252;
    if closes.len() > n {
        let base = closes[closes.len() - 1 - n];
        let last = closes[closes.len() - 1];
        if base > 0.0 {
            return Some(((last / base).powf(1.0 / years as f64) - 1.0) * 100.0);
        }
    }
    None
}

#[derive(Clone, Copy)]
struct Signal {
    last: f64,
    vs50: f64,
    vs200: f64,
    rs6: Option<f64>,
    rs12: Option<f64>,
    off_high: f64,
    mom3: f64,
    ret12: f64,
    cagr3: Option<f64>,
    cagr5: Option<f64>,
}

fn signal_from(closes: &[f64], nr6: Option<f64>, nr12: Option<f64>) -> Option<Signal> {
    if closes.len() < MIN_HISTORY {
        return None;
    }
    let last = *closes.last()?;
    if last <= 0.0 {
        return None;
    }
    let vs50 = (last / sma(closes, 50) - 1.0) * 100.0;
    let vs200 = (last / sma(closes, 200) - 1.0) * 100.0;
    let w = closes.len().min(252);
    let hi = closes[closes.len() - w..].iter().cloned().fold(f64::MIN, f64::max);
    let off_high = if hi > 0.0 { (last / hi - 1.0) * 100.0 } else { 0.0 };
    let rs6 = match (ret_n(closes, 126), nr6) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };
    let rs12 = match (ret_n(closes, 252), nr12) {
        (Some(a), Some(b)) => Some(a - b),
        _ => None,
    };
    Some(Signal {
        last,
        vs50,
        vs200,
        rs6,
        rs12,
        off_high,
        mom3: ret_n(closes, 63).unwrap_or(0.0),
        ret12: ret_n(closes, 252).unwrap_or(0.0),
        cagr3: cagr(closes, 3),
        cagr5: cagr(closes, 5),
    })
}

fn trend_label(s: &Signal) -> &'static str {
    if s.vs200 > 0.0 && s.mom3 > 0.0 {
        "Uptrend"
    } else if s.vs200 < 0.0 && s.ret12 < 0.0 && s.mom3 < 0.0 {
        "Downtrend"
    } else {
        "Sideways"
    }
}

/// Descriptive bucket (NOT advice). Demerger-distorted and no-data names first so
/// a corporate-action artifact is never mistaken for a sell signal.
fn classify(symbol: &str, sig: Option<&Signal>, weight: f64, _edge_eligible: bool) -> (String, String) {
    if is_demerger_distorted(symbol) {
        return (
            "Hold*".into(),
            "trend distorted by the 2025 Tata Motors demerger — judge the COMBINED position".into(),
        );
    }
    let s = match sig {
        Some(s) => s,
        None => return ("Hold".into(), "no candle history — trend can't be read".into()),
    };
    let trend = trend_label(s);
    if trend == "Downtrend" && s.rs12.map_or(false, |x| x < ROTATE_RS_CUT) {
        return (
            "Rotate out".into(),
            format!("downtrend + lagging NIFTY by {:.0}% over 12m", s.rs12.unwrap_or(0.0)),
        );
    }
    if trend == "Uptrend" && s.rs12.map_or(true, |x| x > 0.0) {
        return ("Leader".into(), "uptrend, beating the market".into());
    }
    if weight >= TRIM_WEIGHT_PCT && s.vs200 < 0.0 && s.rs12.map_or(false, |x| x < 0.0) {
        return ("Trim".into(), format!("oversized ({weight:.0}%) and lagging the market"));
    }
    ("Hold".into(), "mixed / consolidating".into())
}

// --- I/O: candle + NIFTY loaders (read-only; signals-only) -------------------

fn daily_closes(conn: &Connection, root: &Path, symbol: &str) -> Vec<f64> {
    match storage_kernel::load_candles(conn, root, symbol, Timeframe::Daily) {
        Ok(c) => c.iter().map(|x| x.close).collect(),
        Err(_) => Vec::new(),
    }
}

/// Raw NIFTY50 daily closes (no public helper exists; mirrors `regime`'s query).
pub fn load_nifty_closes(conn: &Connection, root: &Path) -> Vec<f64> {
    let path = root.join("index_daily").join("NIFTY50.parquet");
    if !path.exists() {
        return Vec::new();
    }
    let sql = format!(
        "SELECT close FROM read_parquet({}) ORDER BY date",
        storage_kernel::quote_path(&path)
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, f64>(0)) {
            for v in rows.flatten() {
                out.push(v);
            }
        }
    }
    out
}

// --- buy screen: edge-backed AND in a price uptrend beating NIFTY -----------

fn screen_buy_candidates(
    conn: &Connection,
    root: &Path,
    edges: &EdgeIndex,
    held: &HashSet<String>,
    nr6: Option<f64>,
    nr12: Option<f64>,
) -> Vec<BuyCandidate> {
    let mut cands: Vec<BuyCandidate> = Vec::new();
    for (sym, eds) in edges.iter() {
        if held.contains(sym) {
            continue;
        }
        // Best LONG edge by expectancy (a "buy" idea must be a long edge).
        let best = eds
            .iter()
            .filter(|e| matches!(e.direction, Direction::Long))
            .max_by(|a, b| a.expectancy_r.partial_cmp(&b.expectancy_r).unwrap_or(std::cmp::Ordering::Equal));
        let best = match best {
            Some(e) => e,
            None => continue,
        };
        let closes = daily_closes(conn, root, sym);
        let sig = match signal_from(&closes, nr6, nr12) {
            Some(s) => s,
            None => continue,
        };
        if !(sig.vs50 > 0.0 && sig.vs200 > 0.0) {
            continue;
        }
        let (rs6, rs12) = match (sig.rs6, sig.rs12) {
            (Some(a), Some(b)) if a > 0.0 && b > 0.0 => (a, b),
            _ => continue,
        };
        if sig.off_high < OFF_HIGH_MIN {
            continue;
        }
        cands.push(BuyCandidate {
            symbol: sym.clone(),
            last: r2(sig.last),
            vs_dma200: r1(sig.vs200),
            rs_6m: r1(rs6),
            rs_12m: r1(rs12),
            off_high_pct: r1(sig.off_high),
            cagr_3y: sig.cagr3.map(r1),
            cagr_5y: sig.cagr5.map(r1),
            edge_strategy: best.strategy.clone(),
            edge_expectancy_r: r2(best.expectancy_r),
            edge_profit_factor: r2(best.profit_factor),
            edge_win_pct: r1(best.win_pct),
            edge_n: best.n,
        });
    }
    // Rank by combined relative strength (momentum leadership).
    cands.sort_by(|a, b| {
        (b.rs_12m + b.rs_6m).partial_cmp(&(a.rs_12m + a.rs_6m)).unwrap_or(std::cmp::Ordering::Equal)
    });
    cands.truncate(BUY_CANDIDATES_MAX);
    cands
}

// --- illustrative rebalance --------------------------------------------------

struct PlanRow {
    symbol: String,
    qty: f64,
    value: f64,
    cost: f64,
    vs200: Option<f64>,
    sector: String,
    action: String,
}

fn frac_for(action: &str) -> f64 {
    match action {
        "Rotate out" => 1.0,
        "Trim" => 0.6,
        _ => 0.0,
    }
}

fn profile(items: &[(f64, Option<f64>, String)]) -> RebalanceProfile {
    let total: f64 = items.iter().map(|x| x.0).sum::<f64>().max(1.0);
    let uptrend = items.iter().filter(|x| x.1.map_or(false, |v| v > 0.0)).map(|x| x.0).sum::<f64>() / total * 100.0;
    let theme = items.iter().filter(|x| is_theme(&x.2)).map(|x| x.0).sum::<f64>() / total * 100.0;
    let mut sectors: HashSet<String> = HashSet::new();
    for x in items {
        if !x.2.is_empty() {
            sectors.insert(x.2.clone());
        }
    }
    let mut ws: Vec<f64> = items.iter().map(|x| x.0 / total * 100.0).collect();
    ws.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    RebalanceProfile {
        uptrend_pct: r0(uptrend),
        sectors: sectors.len(),
        theme_pct: r0(theme),
        top2_pct: r0(ws.iter().take(2).sum::<f64>()),
        hhi: r0(ws.iter().map(|w| w * w).sum::<f64>()),
    }
}

fn build_plan(rows: &[PlanRow], candidates: &[BuyCandidate]) -> Option<RebalancePlan> {
    let mut sells: Vec<RebalanceSell> = Vec::new();
    for r in rows {
        let frac = frac_for(&r.action);
        if frac <= 0.0 {
            continue;
        }
        let reason = if r.action == "Rotate out" {
            "confirmed downtrend, lagging the market".to_string()
        } else {
            "oversized & lagging the market".to_string()
        };
        sells.push(RebalanceSell {
            symbol: r.symbol.clone(),
            action: r.action.clone(),
            frac,
            shares: (r.qty * frac).round() as i64,
            cash: r0(r.value * frac),
            realized_gain: r0((r.value - r.cost) * frac),
            reason,
        });
    }
    if sells.is_empty() {
        return None;
    }
    let cash_raised: f64 = sells.iter().map(|s| s.cash).sum();
    let realized_gain: f64 = sells.iter().map(|s| s.realized_gain).sum();
    let tax = (realized_gain - LTCG_EXEMPTION).max(0.0) * LTCG_RATE;
    let deploy = (cash_raised - tax).max(0.0);

    let mut buys: Vec<RebalanceBuy> = Vec::new();
    let n = candidates.len().min(REDEPLOY_NAMES);
    if n > 0 && deploy > 0.0 {
        let per = deploy / n as f64;
        for c in candidates.iter().take(n) {
            buys.push(RebalanceBuy {
                symbol: c.symbol.clone(),
                amount: r0(per),
                shares: if c.last > 0.0 { (per / c.last) as i64 } else { 0 },
                edge_strategy: c.edge_strategy.clone(),
            });
        }
    }

    let before: Vec<(f64, Option<f64>, String)> =
        rows.iter().map(|r| (r.value, r.vs200, r.sector.clone())).collect();
    let mut after: Vec<(f64, Option<f64>, String)> = Vec::new();
    for r in rows {
        let v = r.value * (1.0 - frac_for(&r.action));
        if v > 0.0 {
            after.push((v, r.vs200, r.sector.clone()));
        }
    }
    for b in &buys {
        after.push((b.amount, Some(1.0), "New".into()));
    }

    Some(RebalancePlan {
        sells,
        buys,
        cash_raised: r0(cash_raised),
        realized_gain: r0(realized_gain),
        ltcg_tax_est: r0(tax),
        to_redeploy: r0(deploy),
        before: profile(&before),
        after: profile(&after),
    })
}

fn scenarios() -> Vec<GrowthScenario> {
    vec![
        GrowthScenario { name: "Bear".into(), cagr_low: 5.0, cagr_high: 7.0, assumes: "theme stays out of favour; multiples compress".into() },
        GrowthScenario { name: "Base".into(), cagr_low: 10.0, cagr_high: 12.0, assumes: "in line with India nominal GDP / NIFTY long-run".into() },
        GrowthScenario { name: "Bull".into(), cagr_low: 14.0, cagr_high: 16.0, assumes: "leaders compound AND laggards rotated into quality".into() },
    ]
}

/// Empty payload (when no DuckDB connection is available) — still ships the
/// scenarios + disclaimers so the UI renders consistently.
pub fn empty(built_ist: String) -> RotationAnalysis {
    RotationAnalysis {
        holdings: Vec::new(),
        buy_candidates: Vec::new(),
        plan: None,
        scenarios: scenarios(),
        decade_note: DECADE_NOTE.into(),
        disclaimer: ROTATION_DISCLAIMER.into(),
        built_ist,
    }
}

/// Build the full rotation/growth payload from the already-computed holdings
/// analyses + the edge map. Reads daily candles + NIFTY for trend/relative
/// strength. Pure of scoring; never an order.
pub fn build(
    conn: &Connection,
    root: &Path,
    holdings: &[HoldingAnalysis],
    edges: &EdgeIndex,
    built_ist: String,
) -> RotationAnalysis {
    let nifty = load_nifty_closes(conn, root);
    let nr6 = ret_n(&nifty, 126);
    let nr12 = ret_n(&nifty, 252);

    let mut rows: Vec<RotationRow> = Vec::with_capacity(holdings.len());
    let mut plan_rows: Vec<PlanRow> = Vec::with_capacity(holdings.len());
    for h in holdings {
        let closes = daily_closes(conn, root, &h.symbol);
        let sig = signal_from(&closes, nr6, nr12);
        let (action, reason) = classify(&h.symbol, sig.as_ref(), h.weight_pct, h.edge_eligible);
        let trend = if is_demerger_distorted(&h.symbol) {
            "Demerger".to_string()
        } else {
            sig.map(|s| trend_label(&s).to_string()).unwrap_or_else(|| "No data".to_string())
        };
        rows.push(RotationRow {
            symbol: h.symbol.clone(),
            weight_pct: r1(h.weight_pct),
            vs_dma200: sig.map(|s| r1(s.vs200)),
            rs_12m: sig.and_then(|s| s.rs12).map(r1),
            off_high_pct: sig.map(|s| r1(s.off_high)),
            trend,
            edge_eligible: h.edge_eligible,
            action: action.clone(),
            reason,
        });
        plan_rows.push(PlanRow {
            symbol: h.symbol.clone(),
            qty: h.qty,
            value: h.market_value,
            cost: h.cost_basis,
            vs200: sig.map(|s| s.vs200),
            sector: h.sector.clone().unwrap_or_default(),
            action,
        });
    }

    let held: HashSet<String> = holdings.iter().map(|h| h.symbol.clone()).collect();
    let candidates = screen_buy_candidates(conn, root, edges, &held, nr6, nr12);
    let plan = build_plan(&plan_rows, &candidates);

    RotationAnalysis {
        holdings: rows,
        buy_candidates: candidates,
        plan,
        scenarios: scenarios(),
        decade_note: DECADE_NOTE.into(),
        disclaimer: ROTATION_DISCLAIMER.into(),
        built_ist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(vs200: f64, mom3: f64, ret12: f64, rs12: Option<f64>) -> Signal {
        Signal { last: 100.0, vs50: vs200, vs200, rs6: rs12, rs12, off_high: -5.0, mom3, ret12, cagr3: Some(12.0), cagr5: Some(10.0) }
    }

    #[test]
    fn demerger_tickers_are_never_rotated() {
        for s in ["TMPV", "TMLCV", "TATAMOTORS"] {
            let (a, _) = classify(s, Some(&sig(-20.0, -10.0, -40.0, Some(-44.0))), 14.0, false);
            assert_eq!(a, "Hold*", "{s} must be Hold*, never a sell");
        }
    }

    #[test]
    fn no_data_is_hold_not_rotate() {
        let (a, _) = classify("ANY", None, 30.0, false);
        assert_eq!(a, "Hold");
    }

    #[test]
    fn downtrend_lagging_is_rotate_out() {
        let (a, _) = classify("RVNL", Some(&sig(-21.0, -7.0, -38.0, Some(-35.0))), 6.0, true);
        assert_eq!(a, "Rotate out");
    }

    #[test]
    fn uptrend_beating_is_leader() {
        let (a, _) = classify("GMRAIRPORT", Some(&sig(12.0, 22.0, 34.0, Some(37.0))), 2.0, true);
        assert_eq!(a, "Leader");
    }

    #[test]
    fn oversized_lagging_is_trim() {
        // below 200-DMA, lagging, 13% weight, NOT a confirmed downtrend (ret12 positive).
        let (a, _) = classify("BIG", Some(&sig(-5.0, 30.0, 5.0, Some(-19.0))), 13.0, false);
        assert_eq!(a, "Trim");
    }

    #[test]
    fn plan_math_tax_and_before_after() {
        let rows = vec![
            PlanRow { symbol: "RVNL".into(), qty: 1000.0, value: 180_000.0, cost: 130_000.0, vs200: Some(-21.0), sector: "PSU".into(), action: "Rotate out".into() },
            PlanRow { symbol: "MOON".into(), qty: 500.0, value: 350_000.0, cost: 195_000.0, vs200: Some(-5.0), sector: "Tech".into(), action: "Trim".into() },
            PlanRow { symbol: "BANK".into(), qty: 200.0, value: 600_000.0, cost: 460_000.0, vs200: Some(5.0), sector: "Bank".into(), action: "Leader".into() },
        ];
        let cands = vec![BuyCandidate {
            symbol: "X".into(), last: 100.0, vs_dma200: 5.0, rs_6m: 10.0, rs_12m: 20.0, off_high_pct: -3.0,
            cagr_3y: Some(15.0), cagr_5y: Some(12.0), edge_strategy: "vwap_cross".into(),
            edge_expectancy_r: 0.2, edge_profit_factor: 1.6, edge_win_pct: 55.0, edge_n: 100,
        }];
        let p = build_plan(&rows, &cands).expect("a plan");
        // Rotate out 100% of 180k + Trim 60% of 350k = 180k + 210k = 390k cash.
        assert!((p.cash_raised - 390_000.0).abs() < 1.0, "cash {}", p.cash_raised);
        // gain = (180k-130k) + 0.6*(350k-195k) = 50k + 93k = 143k.
        assert!((p.realized_gain - 143_000.0).abs() < 1.0, "gain {}", p.realized_gain);
        // tax = (143k - 125k)*12.5% = 2250.
        assert!((p.ltcg_tax_est - 2250.0).abs() < 1.0, "tax {}", p.ltcg_tax_est);
        assert!(!p.buys.is_empty());
        // After should have MORE of the book in uptrend than before (laggards out, buy in).
        assert!(p.after.uptrend_pct >= p.before.uptrend_pct, "after {} before {}", p.after.uptrend_pct, p.before.uptrend_pct);
    }

    #[test]
    fn no_laggards_means_no_plan() {
        let rows = vec![
            PlanRow { symbol: "A".into(), qty: 100.0, value: 100_000.0, cost: 80_000.0, vs200: Some(5.0), sector: "Bank".into(), action: "Leader".into() },
            PlanRow { symbol: "B".into(), qty: 100.0, value: 100_000.0, cost: 90_000.0, vs200: Some(2.0), sector: "IT".into(), action: "Hold".into() },
        ];
        assert!(build_plan(&rows, &[]).is_none());
    }

    #[test]
    fn empty_payload_still_ships_scenarios_and_disclaimer() {
        let e = empty("2026-06-28 12:00:00".into());
        assert_eq!(e.scenarios.len(), 3);
        assert!(e.disclaimer.to_lowercase().contains("not advice"));
        assert!(e.plan.is_none() && e.holdings.is_empty());
    }
}

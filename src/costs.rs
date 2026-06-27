//! Itemized Indian intraday-equity transaction costs (Zerodha-style), so net
//! P&L is exact rather than a blended percentage.
//!
//! Round-trip = buy leg + sell leg. Rates below are the standard NSE
//! equity-intraday charges (approximate, configurable). Slippage is NOT a
//! statutory charge — it is an explicit modelling assumption.
//!
//! Two consumers:
//!   * [`round_trip`] — EXACT charges for a known (buy_val, sell_val); used for
//!     the per-card net profit / loss and the cost breakdown shown to the user.
//!   * [`backtest_roundtrip_pct`] — a representative round-trip cost as a
//!     fraction of one-leg notional, used inside the per-share backtest (which
//!     does not know position size). Derived from the same rates at a reference
//!     notional so it stays consistent with the exact model.

use crate::types::CostBreakdown;

// --- statutory + broker rates (NSE equity, intraday) -----------------------

/// Brokerage: 0.03% of leg turnover, capped at ₹20 per executed order.
pub const BROKERAGE_PCT: f64 = 0.0003;
pub const BROKERAGE_CAP: f64 = 20.0;
/// Securities Transaction Tax: 0.025% on the SELL leg only (intraday equity).
pub const STT_PCT_SELL: f64 = 0.00025;
/// NSE exchange transaction charge: 0.00297% of turnover.
pub const EXCHANGE_TXN_PCT: f64 = 0.0000297;
/// SEBI turnover fee: ₹10 per crore = 0.0001% of turnover.
pub const SEBI_PCT: f64 = 0.000001;
/// GST: 18% on (brokerage + exchange txn + SEBI).
pub const GST_PCT: f64 = 0.18;
/// Stamp duty: 0.003% on the BUY leg only.
pub const STAMP_PCT_BUY: f64 = 0.00003;
/// Slippage allowance per leg (each-way), as a fraction of that leg's value.
/// Modelling assumption (spread/impact), not a statutory charge.
pub const SLIPPAGE_PCT_EACH_WAY: f64 = 0.00025;

/// Reference per-leg notional used to derive the representative backtest cost
/// percentage (so the ₹20 brokerage cap is applied at a realistic position size).
pub const REF_LEG_NOTIONAL: f64 = 100_000.0;

/// Exact itemized round-trip cost (INR) for a trade given its buy-leg and
/// sell-leg rupee values. Direction-agnostic: for a short, `sell_val` is the
/// entry leg and `buy_val` the cover leg — the statutory split (STT on sell,
/// stamp on buy) is applied to the correct leg either way.
pub fn round_trip(buy_val: f64, sell_val: f64) -> CostBreakdown {
    let buy_val = buy_val.max(0.0);
    let sell_val = sell_val.max(0.0);
    let turnover = buy_val + sell_val;

    let brokerage =
        (BROKERAGE_PCT * buy_val).min(BROKERAGE_CAP) + (BROKERAGE_PCT * sell_val).min(BROKERAGE_CAP);
    let stt = STT_PCT_SELL * sell_val;
    let exchange_txn = EXCHANGE_TXN_PCT * turnover;
    let sebi = SEBI_PCT * turnover;
    let gst = GST_PCT * (brokerage + exchange_txn + sebi);
    let stamp = STAMP_PCT_BUY * buy_val;
    let slippage = SLIPPAGE_PCT_EACH_WAY * turnover;

    let total = brokerage + stt + exchange_txn + sebi + gst + stamp + slippage;
    CostBreakdown {
        brokerage,
        stt,
        exchange_txn,
        sebi,
        gst,
        stamp,
        slippage,
        total,
    }
}

/// Representative round-trip cost as a fraction of ONE-leg notional, evaluated
/// at [`REF_LEG_NOTIONAL`]. This is what the per-share backtest deducts
/// (`cost × entry / risk` per trade). ~0.13% with the default rates.
pub fn backtest_roundtrip_pct() -> f64 {
    let c = round_trip(REF_LEG_NOTIONAL, REF_LEG_NOTIONAL);
    c.total / REF_LEG_NOTIONAL
}

/// Round-trip cost fraction with the **slippage** allowance scaled by `mult`
/// (1×/2×/3×) while the statutory charges (brokerage/STT/exchange/SEBI/GST/stamp)
/// stay fixed. Powers the backtest's slippage stress band: "is the edge still
/// positive if fills slip 2–3× worse than assumed?". `mult == 1.0` returns
/// exactly [`backtest_roundtrip_pct`].
pub fn backtest_roundtrip_pct_scaled(mult: f64) -> f64 {
    let base = round_trip(REF_LEG_NOTIONAL, REF_LEG_NOTIONAL).total;
    let turnover = REF_LEG_NOTIONAL + REF_LEG_NOTIONAL;
    let extra_slip = SLIPPAGE_PCT_EACH_WAY * turnover * (mult - 1.0);
    (base + extra_slip) / REF_LEG_NOTIONAL
}

/// Net P&L (INR) for a sized position at the given fill prices, after exact
/// round-trip costs. `qty` shares, `entry`/`exit` prices, `long` direction.
/// Returns `(net_pnl, cost_breakdown)`.
pub fn net_pnl(qty: i64, entry: f64, exit: f64, long: bool) -> (f64, CostBreakdown) {
    let q = qty.max(0) as f64;
    let entry_val = q * entry;
    let exit_val = q * exit;
    // Map to buy/sell legs for the statutory split.
    let (buy_val, sell_val) = if long {
        (entry_val, exit_val) // buy at entry, sell at exit
    } else {
        (exit_val, entry_val) // short: sell at entry, buy back at exit
    };
    let costs = round_trip(buy_val, sell_val);
    let gross = if long {
        exit_val - entry_val
    } else {
        entry_val - exit_val
    };
    (gross - costs.total, costs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtest_pct_is_in_sane_range() {
        let p = backtest_roundtrip_pct();
        // Itemized round-trip should land near ~0.13% of one-leg notional.
        assert!(p > 0.0009 && p < 0.0020, "pct={p}");
    }

    #[test]
    fn slippage_band_is_monotone_and_1x_equals_baseline() {
        let base = backtest_roundtrip_pct();
        // 1× must be byte-identical to the unscaled cost (preserves the anchor).
        assert_eq!(backtest_roundtrip_pct_scaled(1.0).to_bits(), base.to_bits());
        // More slippage only ever costs more.
        let p2 = backtest_roundtrip_pct_scaled(2.0);
        let p3 = backtest_roundtrip_pct_scaled(3.0);
        assert!(p2 > base && p3 > p2, "base={base} 2x={p2} 3x={p3}");
    }

    #[test]
    fn stt_only_on_sell_and_stamp_only_on_buy() {
        // Pure-buy value vs pure-sell value: stamp tracks buy, STT tracks sell.
        let only_buy = round_trip(100_000.0, 0.0);
        assert!(only_buy.stamp > 0.0 && only_buy.stt == 0.0);
        let only_sell = round_trip(0.0, 100_000.0);
        assert!(only_sell.stt > 0.0 && only_sell.stamp == 0.0);
    }

    #[test]
    fn net_pnl_long_deducts_costs() {
        // 100 sh, entry 1000 -> exit 1010: gross +1000, costs > 0, net < 1000.
        let (net, c) = net_pnl(100, 1000.0, 1010.0, true);
        assert!(net < 1000.0 && net > 1000.0 - 600.0, "net={net}");
        assert!(c.total > 0.0);
        // Short symmetric: entry 1010 -> exit 1000 also profits ~+1000 gross.
        let (net_s, _) = net_pnl(100, 1010.0, 1000.0, false);
        assert!(net_s > 0.0);
    }
}

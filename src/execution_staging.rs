//! 2026-compliant signal staging — converts a trade intent into a synthetic,
//! manual-execution Bracket Order. NEVER sends to a broker.
//!
//! SEBI-2026 buffer: all entries are LIMIT (no naked market). The staged limit
//! is `LTP ± ATR × config::STAGING_LIMIT_ATR_MULT`; SL/target are ATR-scaled.
//!
//! Limit-side choice (documented): we stage a *passive* limit just inside the
//! current price so the manual operator is filled on a small pullback rather
//! than chasing the print.
//!   * BUY  → `limit = LTP − ATR×0.1` (bid slightly below the LTP).
//!   * SELL → `limit = LTP + ATR×0.1` (offer slightly above the LTP).
//! Stops and targets are then measured from that staged entry, not the raw LTP,
//! so the bracket's risk/reward geometry is internally consistent:
//!   * BUY : SL = entry − sl·ATR (below), TP = entry + tp·ATR (above).
//!   * SELL: SL = entry + sl·ATR (above), TP = entry − tp·ATR (below).
//! `trailing` is the ATR-scaled trail distance (`sl_atr_mult × ATR`), variety is
//! always "BO". Nothing here is ever transmitted — `copy_text` is the only
//! consumer-facing artifact, a one-line paste-ready string.

use crate::config::{self, Direction};
use crate::types::{BracketOrder, StagedSignal};

/// Build a synthetic Bracket Order (limit entry + protective SL/TP, ATR-scaled).
///
/// See the module docs for the side/limit/stop/target geometry. All `f64` price
/// fields keep full precision; rounding to 2dp happens only in the human-facing
/// `copy_text` produced by [`stage_signal`].
pub fn stage_bracket(
    symbol: &str,
    token: u32,
    side: Direction,
    ltp: f64,
    atr: f64,
    qty: i64,
    sl_atr_mult: f64,
    tp_atr_mult: f64,
) -> BracketOrder {
    let buffer = atr * config::STAGING_LIMIT_ATR_MULT;
    let dir = side.sign(); // +1 BUY, -1 SELL

    // Passive limit just inside the LTP: BUY bids below, SELL offers above.
    let limit_price = ltp - dir * buffer;

    // SL/TP measured from the staged entry, on the protective side.
    let stop_loss = limit_price - dir * sl_atr_mult * atr;
    let take_profit = limit_price + dir * tp_atr_mult * atr;

    // Trailing distance is ATR-scaled by the stop multiple (sign-free distance).
    let trailing = sl_atr_mult * atr;

    BracketOrder {
        symbol: symbol.to_string(),
        instrument_token: token,
        side: side.as_str().to_string(),
        qty,
        limit_price,
        stop_loss,
        take_profit,
        trailing,
        variety: "BO".to_string(),
    }
}

/// Build a full staging-console row (bracket + copy/paste text).
///
/// `copy_text` example:
/// `BUY 120 RELIANCE @ LMT 1316.50 | SL 1278.40 | TGT 1392.70 | BO (manual)`.
pub fn stage_signal(
    symbol: &str,
    token: u32,
    side: Direction,
    ltp: f64,
    atr: f64,
    qty: i64,
    sl_atr_mult: f64,
    tp_atr_mult: f64,
) -> StagedSignal {
    let bracket = stage_bracket(symbol, token, side, ltp, atr, qty, sl_atr_mult, tp_atr_mult);

    let copy_text = format!(
        "{} {} {} @ LMT {:.2} | SL {:.2} | TGT {:.2} | {} (manual)",
        bracket.side,
        qty,
        symbol,
        bracket.limit_price,
        bracket.stop_loss,
        bracket.take_profit,
        bracket.variety,
    );

    StagedSignal {
        symbol: symbol.to_string(),
        instrument_token: token,
        side: side.as_str().to_string(),
        ltp,
        atr,
        limit_price: bracket.limit_price,
        stop_loss: bracket.stop_loss,
        take_profit: bracket.take_profit,
        qty,
        notional: qty as f64 * bracket.limit_price,
        copy_text,
        bracket,
        tradability_note: None, // callers annotate; staging itself never verifies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    #[test]
    fn buy_bracket_sides_and_fields() {
        // LTP 1320, ATR 10, sl 1.5, tp 3.0.
        let ltp = 1320.0;
        let atr = 10.0;
        let b = stage_bracket("RELIANCE", 738561, Direction::Long, ltp, atr, 120, 1.5, 3.0);

        // BUY: passive limit BELOW the LTP by ATR×0.1 = 1.0 => 1319.0.
        assert!((b.limit_price - 1319.0).abs() < EPS, "limit {}", b.limit_price);
        assert!(b.limit_price < ltp, "BUY limit must be below LTP");

        // SL below entry, TP above entry.
        assert!((b.stop_loss - (1319.0 - 15.0)).abs() < EPS, "sl {}", b.stop_loss);
        assert!((b.take_profit - (1319.0 + 30.0)).abs() < EPS, "tp {}", b.take_profit);
        assert!(b.stop_loss < b.limit_price, "BUY SL must be below entry");
        assert!(b.take_profit > b.limit_price, "BUY TP must be above entry");

        // BO fields.
        assert_eq!(b.side, "BUY");
        assert_eq!(b.variety, "BO");
        assert_eq!(b.qty, 120);
        assert_eq!(b.instrument_token, 738561);
        assert!((b.trailing - 15.0).abs() < EPS, "trailing {}", b.trailing);
    }

    #[test]
    fn sell_bracket_sides_and_fields() {
        let ltp = 1320.0;
        let atr = 10.0;
        let b = stage_bracket("RELIANCE", 738561, Direction::Short, ltp, atr, 80, 1.5, 3.0);

        // SELL: passive limit ABOVE the LTP by ATR×0.1 = 1.0 => 1321.0.
        assert!((b.limit_price - 1321.0).abs() < EPS, "limit {}", b.limit_price);
        assert!(b.limit_price > ltp, "SELL limit must be above LTP");

        // SL above entry, TP below entry.
        assert!((b.stop_loss - (1321.0 + 15.0)).abs() < EPS, "sl {}", b.stop_loss);
        assert!((b.take_profit - (1321.0 - 30.0)).abs() < EPS, "tp {}", b.take_profit);
        assert!(b.stop_loss > b.limit_price, "SELL SL must be above entry");
        assert!(b.take_profit < b.limit_price, "SELL TP must be below entry");

        assert_eq!(b.side, "SELL");
        assert_eq!(b.variety, "BO");
        assert_eq!(b.qty, 80);
    }

    #[test]
    fn signal_copy_text_and_notional() {
        let s = stage_signal("RELIANCE", 738561, Direction::Long, 1320.0, 10.0, 120, 1.5, 3.0);
        assert_eq!(
            s.copy_text,
            "BUY 120 RELIANCE @ LMT 1319.00 | SL 1304.00 | TGT 1349.00 | BO (manual)"
        );
        assert!((s.notional - 120.0 * 1319.0).abs() < EPS, "notional {}", s.notional);
        // Mirrored fields match the bracket.
        assert!((s.limit_price - s.bracket.limit_price).abs() < EPS);
        assert!((s.stop_loss - s.bracket.stop_loss).abs() < EPS);
        assert!((s.take_profit - s.bracket.take_profit).abs() < EPS);
    }
}

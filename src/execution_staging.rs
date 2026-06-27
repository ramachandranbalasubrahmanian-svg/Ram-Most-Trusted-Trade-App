//! 2026-compliant signal staging — converts a trade intent into a synthetic,
//! manual-execution Bracket Order. NEVER sends to a broker.
//!
//! SEBI-2026 buffer: all entries are LIMIT (no naked market). The staged limit
//! is `LTP ± ATR × config::STAGING_LIMIT_ATR_MULT`; SL/target are ATR-scaled.
//!
//! CONTRACT STUB — public signatures are frozen; bodies filled by the workflow.

use crate::config::Direction;
use crate::types::{BracketOrder, StagedSignal};

/// Build a synthetic Bracket Order (limit entry + protective SL/TP, ATR-scaled).
pub fn stage_bracket(
    symbol: &str,
    token: u32,
    side: Direction,
    ltp: f64,
    atr: f64,
    qty: i64,
    _sl_atr_mult: f64,
    _tp_atr_mult: f64,
) -> BracketOrder {
    BracketOrder {
        symbol: symbol.to_string(),
        instrument_token: token,
        side: side.as_str().to_string(),
        qty,
        limit_price: ltp,
        stop_loss: ltp,
        take_profit: ltp,
        trailing: atr,
        variety: "BO".to_string(),
    }
}

/// Build a full staging-console row (bracket + copy/paste text).
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
        copy_text: String::new(),
        bracket,
    }
}

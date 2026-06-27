//! Per-second microstructure analytics + universe ranking.
//!
//! Detects which strategies are currently firing per symbol, confirms with live
//! OBI / VWAP-vs-VAH-VAL / rolling z-score, scores each candidate as
//! (backtested edge × live confirmation), and ranks the universe into
//! Top 10 Buy and Top 10 Sell.
//!
//! Filled in Phase 3.

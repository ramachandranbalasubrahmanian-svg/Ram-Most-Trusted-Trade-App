//! Position sizing & projected P&L (advisory only — never places orders).
//!
//! `risk_amount = budget × risk%`; `SL_dist = k·ATR`;
//! `shares = floor(risk_amount / SL_dist)` capped by `budget × leverage`.
//! Emits SL, target, projected profit (target hit), projected loss (SL hit), and
//! an expectancy-weighted P&L estimate. Enforces max-concurrent and sector
//! correlation limits; the 15:15 IST square-off is an ALERT, not an order.
//!
//! Filled in Phase 3.

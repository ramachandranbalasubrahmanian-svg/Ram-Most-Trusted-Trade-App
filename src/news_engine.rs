//! Reactive, metered news sentiment.
//!
//! Fires an async request only when a symbol enters the ranked Top 10 (never a
//! continuous pull over the whole universe). Marketaux/EODHD with
//! `&countries=in&exchanges=NSE`. Feature-flagged OFF by default with a mock
//! provider; a real API key flips it on.
//!
//! Filled in Phase 5 (stub/flag wired alongside Phase 3 ranking).

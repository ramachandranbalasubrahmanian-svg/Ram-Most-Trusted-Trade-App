//! Local Axum web server + `/ws/live_signals` WebSocket.
//!
//! Streams ranked Top 10 Buy/Sell signals + risk packets to the dashboard, and
//! receives budget/risk-meter changes from the UI to re-size positions without
//! recomputing the backtest.
//!
//! Filled in Phase 4.

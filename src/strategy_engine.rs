//! Intraday strategy library + backtester.
//!
//! A `Strategy` trait with a comprehensive, extensible library (VWAP cross, ORB,
//! EMA/MACD/Supertrend, RSI/Bollinger/VWAP-band/Value-Area reversion, CPR pivots,
//! Donchian/ATR breakout, gap-and-go, RVOL, z-score, OBI). The backtester runs
//! each strategy × symbol × direction over history (net of costs) and writes a
//! cached "edge map" of expectancy / PF / win% / sample-n.
//!
//! Filled in Phase 2.

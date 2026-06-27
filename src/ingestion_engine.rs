//! Tick ingestion.
//!
//! (a) Replay simulator over `minute/` parquet (default; no credentials).
//! (b) Live Kite Connect WebSocket client: zero-copy binary tick parser
//!     (LTP / volume / 5-level depth), instruments-dump fetch + cache, and a
//!     local↔exchange latency tracker. Dispatches across threads via crossbeam.
//!
//! Replay built in Phase 3; live WebSocket in Phase 5.

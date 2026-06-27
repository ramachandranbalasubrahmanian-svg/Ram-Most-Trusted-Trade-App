//! RAM_ISTP — local intraday backtest + signal engine.
//!
//! Signals & alerts ONLY: this binary never places, modifies, or cancels live
//! orders. All position sizing and P&L figures are advisory.
//!
//! Phase 0 skeleton: module scaffold + dependency compile check. Real logic is
//! filled in per-phase (see plan: config/storage → strategies → ingestion →
//! analytics → risk → server/UI → live).

mod analytics_kernel;
mod config;
mod ingestion_engine;
mod news_engine;
mod risk_manager;
mod server;
mod storage_kernel;
mod strategy_engine;

fn main() {
    println!("RAM_ISTP skeleton OK — modules linked, dependencies compiled.");
}

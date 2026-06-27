//! Local Axum web server + `/ws/live_signals` WebSocket.
//!
//! Serves the dashboard, streams the latest `SignalPacket` on every update
//! (woken by `notify`), and applies inbound budget/risk-meter changes to
//! `settings` so the analytics loop re-sizes on its next tick.
//!
//! CONTRACT STUB — public signatures are frozen; bodies are filled in Phase 4.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use tokio::sync::Notify;

use crate::config::UserSettings;
use crate::types::SignalPacket;

/// Shared application state handed to the server and updated by the analytics
/// loop. Cheaply cloneable (all fields are `Arc`).
#[derive(Clone)]
pub struct AppState {
    /// Latest signal packet to push to clients.
    pub packet: Arc<RwLock<SignalPacket>>,
    /// User-controlled budget/risk, mutated by the UI.
    pub settings: Arc<RwLock<UserSettings>>,
    /// Fired whenever `packet` is refreshed, to wake WS pushers.
    pub notify: Arc<Notify>,
    /// Directory containing `index.html`.
    pub static_dir: PathBuf,
}

/// Run the Axum server until the process exits.
pub async fn serve(_addr: SocketAddr, _state: AppState) -> Result<()> {
    Ok(())
}

//! Local Axum web server + `/ws/live_signals` WebSocket.
//!
//! Serves the dashboard, streams the latest `SignalPacket` on every update
//! (woken by `notify`), and applies inbound budget/risk-meter changes to
//! `settings` so the analytics loop re-sizes on its next tick.
//!
//! Signals-only: nothing here ever reaches a broker. The server merely renders
//! advisory rankings and echoes user settings back to the UI.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::sink::SinkExt;
use futures_util::stream::StreamExt;
use tokio::sync::Notify;
use tracing::{info, warn};

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
    /// Directory containing `index.html` / `intraday.html`.
    pub static_dir: PathBuf,
    /// Parquet archive root, for on-demand suggestion analysis.
    pub root: PathBuf,
    /// Cached 10-Buy / 10-Sell scanner result (computed lazily on first request).
    pub scanner: Arc<RwLock<Option<crate::types::ScanResult>>>,
}

/// Run the Axum server until the process exits.
pub async fn serve(addr: SocketAddr, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws/live_signals", get(ws_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    info!("RAM_ISTP dashboard listening on http://{addr}");

    axum::serve(listener, app)
        .await
        .context("axum server error")?;

    Ok(())
}

/// `GET /` — read `{static_dir}/index.html` at request time so UI edits show on
/// refresh without a rebuild.
async fn index_handler(State(state): State<AppState>) -> Response {
    let path = state.static_dir.join("index.html");
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            warn!("failed to read {}: {e:#}", path.display());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read {}: {e}", path.display()),
            )
                .into_response()
        }
    }
}

/// `GET /ws/live_signals` — upgrade to a WebSocket that streams packets and
/// accepts settings updates.
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Serialize the current packet to JSON, cloning out of the guard so we never
/// hold a sync lock across an `.await`.
fn current_packet_json(state: &AppState) -> Option<String> {
    let packet = {
        let guard = state.packet.read().ok()?;
        guard.clone()
    };
    serde_json::to_string(&packet).ok()
}

/// Drive one WebSocket connection: push the latest packet on every notify and
/// apply inbound `SettingsMsg` updates.
async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();

    // Send the current packet immediately so a fresh client renders at once.
    if let Some(json) = current_packet_json(&state) {
        if sink.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            // (a) Push loop: wake on a packet refresh, send the latest snapshot.
            _ = state.notify.notified() => {
                if let Some(json) = current_packet_json(&state) {
                    if sink.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            // (b) Recv loop: apply settings updates; echo packet to confirm.
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let raw = text.as_str();
                        if let Ok(update) =
                            serde_json::from_str::<crate::types::SettingsMsg>(raw)
                        {
                            {
                                let mut guard = match state.settings.write() {
                                    Ok(g) => g,
                                    Err(_) => break,
                                };
                                *guard = UserSettings::new(update.budget, update.risk_pct);
                            }
                            // Confirm the applied settings by echoing the packet.
                            if let Some(json) = current_packet_json(&state) {
                                if sink.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        // Ignore parse errors; keep the connection open.
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Ping(_)))
                    | Some(Ok(Message::Pong(_)))
                    | Some(Ok(Message::Binary(_))) => {
                        // Non-text frames carry no settings; ignore.
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
}

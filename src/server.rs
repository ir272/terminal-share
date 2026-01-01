//! WebSocket server for terminal sharing
//!
//! This module provides:
//! - A WebSocket endpoint for viewers to connect
//! - Broadcasting terminal output to all connected viewers
//! - Receiving input from viewers (with permission)

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// Messages sent from server to viewer
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Terminal output data
    Output { data: String },
    /// Terminal was resized
    Resize { cols: u16, rows: u16 },
    /// Session info
    Info { session_id: String, viewers: usize },
}

/// Messages sent from viewer to server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Viewer wants to send input (if allowed)
    Input { data: String },
    /// Viewer requesting control
    RequestControl,
}

/// Maximum size of terminal buffer (64KB)
const MAX_BUFFER_SIZE: usize = 64 * 1024;

/// Shared state for the server
pub struct ServerState {
    /// Session ID for this sharing session
    pub session_id: String,
    /// Broadcast channel for terminal output
    pub output_tx: broadcast::Sender<ServerMessage>,
    /// Current terminal size
    pub terminal_size: RwLock<(u16, u16)>,
    /// Number of connected viewers
    pub viewer_count: RwLock<usize>,
    /// Channel to send input from viewers to the PTY
    pub input_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Buffer storing recent terminal output for new viewers
    pub terminal_buffer: RwLock<Vec<u8>>,
}

impl ServerState {
    pub fn new(input_tx: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        let (output_tx, _) = broadcast::channel(1024);
        let session_id = generate_session_id();

        Self {
            session_id,
            output_tx,
            terminal_size: RwLock::new((80, 24)),
            viewer_count: RwLock::new(0),
            input_tx,
            terminal_buffer: RwLock::new(Vec::with_capacity(MAX_BUFFER_SIZE)),
        }
    }

    /// Broadcast terminal output to all viewers and store in buffer
    pub async fn broadcast_output(&self, data: &[u8]) {
        // Store in buffer for new viewers
        {
            let mut buffer = self.terminal_buffer.write().await;
            buffer.extend_from_slice(data);

            // Trim buffer if it exceeds max size (keep the most recent data)
            if buffer.len() > MAX_BUFFER_SIZE {
                let excess = buffer.len() - MAX_BUFFER_SIZE;
                buffer.drain(0..excess);
            }
        }

        // Convert to base64 for safe JSON transmission of binary data
        let encoded = base64_encode(data);
        let _ = self.output_tx.send(ServerMessage::Output { data: encoded });
    }

    /// Get the current terminal buffer contents (for new viewers)
    pub async fn get_buffer(&self) -> Vec<u8> {
        self.terminal_buffer.read().await.clone()
    }

    /// Broadcast terminal resize to all viewers
    pub async fn broadcast_resize(&self, cols: u16, rows: u16) {
        *self.terminal_size.write().await = (cols, rows);
        let _ = self.output_tx.send(ServerMessage::Resize { cols, rows });
    }
}

/// Generate a short, memorable session ID
fn generate_session_id() -> String {
    // Use first 8 chars of UUID for a shorter, shareable ID
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// Base64 encode bytes for safe JSON transmission
fn base64_encode(data: &[u8]) -> String {
    use std::io::Write;
    let mut buf = Vec::new();
    let mut encoder = Base64Encoder::new(&mut buf);
    encoder.write_all(data).unwrap();
    drop(encoder);
    String::from_utf8(buf).unwrap()
}

/// Simple base64 encoder (avoiding extra dependency)
struct Base64Encoder<W: std::io::Write> {
    writer: W,
}

impl<W: std::io::Write> Base64Encoder<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: std::io::Write> std::io::Write for Base64Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        for chunk in buf.chunks(3) {
            let b0 = chunk[0] as usize;
            let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
            let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

            let c0 = CHARS[b0 >> 2];
            let c1 = CHARS[((b0 & 0x03) << 4) | (b1 >> 4)];
            let c2 = if chunk.len() > 1 {
                CHARS[((b1 & 0x0f) << 2) | (b2 >> 6)]
            } else {
                b'='
            };
            let c3 = if chunk.len() > 2 {
                CHARS[b2 & 0x3f]
            } else {
                b'='
            };

            self.writer.write_all(&[c0, c1, c2, c3])?;
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// Create the web server router
pub fn create_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(websocket_handler))
        .with_state(state)
}

/// Serve the viewer HTML page
async fn index_handler() -> impl IntoResponse {
    Html(include_str!("viewer.html"))
}

/// Handle WebSocket connections
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Handle an individual WebSocket connection
async fn handle_socket(socket: WebSocket, state: Arc<ServerState>) {
    // Increment viewer count
    {
        let mut count = state.viewer_count.write().await;
        *count += 1;
        tracing::info!("Viewer connected. Total viewers: {}", *count);
    }

    // Subscribe to terminal output broadcasts
    let mut rx = state.output_tx.subscribe();

    // Split socket into sender and receiver
    let (mut sender, mut receiver) = socket.split();

    // Send initial session info
    let (cols, rows) = *state.terminal_size.read().await;
    let viewer_count = *state.viewer_count.read().await;

    let info_msg = ServerMessage::Info {
        session_id: state.session_id.clone(),
        viewers: viewer_count,
    };
    let _ = sender.send(Message::Text(serde_json::to_string(&info_msg).unwrap().into())).await;

    // Send current terminal size
    let resize_msg = ServerMessage::Resize { cols, rows };
    let _ = sender.send(Message::Text(serde_json::to_string(&resize_msg).unwrap().into())).await;

    // Send buffered terminal content so viewer can see existing state
    let buffer = state.get_buffer().await;
    if !buffer.is_empty() {
        let encoded = base64_encode(&buffer);
        let buffer_msg = ServerMessage::Output { data: encoded };
        let _ = sender.send(Message::Text(serde_json::to_string(&buffer_msg).unwrap().into())).await;
    }

    // Clone state for the receiver task
    let state_clone = state.clone();

    // Task to forward broadcasts to this viewer
    let send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            let text = serde_json::to_string(&msg).unwrap();
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Task to receive input from this viewer
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                    match client_msg {
                        ClientMessage::Input { data } => {
                            // Decode base64 and send to PTY
                            if let Ok(bytes) = base64_decode(&data) {
                                let _ = state_clone.input_tx.send(bytes).await;
                            }
                        }
                        ClientMessage::RequestControl => {
                            // TODO: Implement control request/grant
                            tracing::info!("Viewer requested control");
                        }
                    }
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // Decrement viewer count
    {
        let mut count = state.viewer_count.write().await;
        *count -= 1;
        tracing::info!("Viewer disconnected. Total viewers: {}", *count);
    }
}

/// Decode base64 string to bytes
fn base64_decode(data: &str) -> Result<Vec<u8>, ()> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn char_to_val(c: u8) -> Option<u8> {
        CHARS.iter().position(|&x| x == c).map(|p| p as u8)
    }

    let bytes: Vec<u8> = data.bytes().filter(|&b| b != b'=').collect();
    let mut result = Vec::new();

    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            break;
        }

        let v0 = char_to_val(chunk[0]).ok_or(())?;
        let v1 = char_to_val(chunk[1]).ok_or(())?;
        result.push((v0 << 2) | (v1 >> 4));

        if chunk.len() > 2 {
            let v2 = char_to_val(chunk[2]).ok_or(())?;
            result.push(((v1 & 0x0f) << 4) | (v2 >> 2));

            if chunk.len() > 3 {
                let v3 = char_to_val(chunk[3]).ok_or(())?;
                result.push(((v2 & 0x03) << 6) | v3);
            }
        }
    }

    Ok(result)
}

/// Start the web server
pub async fn start_server(state: Arc<ServerState>, port: u16) -> anyhow::Result<()> {
    let app = create_router(state);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("Server listening on http://{}", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

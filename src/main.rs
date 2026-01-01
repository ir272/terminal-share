//! TermShare - Share your terminal with others in real-time
//!
//! Run `termshare` to start a terminal session that can be viewed
//! by others in their web browser.

mod pty;
mod server;
mod terminal;

use anyhow::Result;
use std::io::Read;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::pty::PtySession;
use crate::server::ServerState;
use crate::terminal::{read_event, write_stdout, RawModeGuard, TerminalEvent};

const DEFAULT_PORT: u16 = 3001;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("termshare=info")
        .with_writer(std::io::stderr)
        .init();

    // Create channel for viewer input
    let (viewer_input_tx, viewer_input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(100);

    // Create server state
    let state = Arc::new(ServerState::new(viewer_input_tx));
    let session_id = state.session_id.clone();

    // Print startup banner
    println!("TermShare v0.1.0 - Terminal Sharing Tool");
    println!("=========================================");
    println!();
    println!("Share URL: http://localhost:{}", DEFAULT_PORT);
    println!("Session ID: {}", session_id);
    println!();
    println!("Starting shell session...");
    println!("Press Ctrl+Q to exit");
    println!();

    // Start web server in background
    let server_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = server::start_server(server_state, DEFAULT_PORT).await {
            tracing::error!("Server error: {}", e);
        }
    });

    // Small delay to let server start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Run the main terminal session
    run_session(state, viewer_input_rx).await?;

    println!();
    println!("Session ended. Goodbye!");
    Ok(())
}

/// Run the main terminal session with network sharing
async fn run_session(
    state: Arc<ServerState>,
    mut viewer_input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    // Create the PTY session (spawns a shell)
    let mut pty = PtySession::new()?;

    // Enable raw mode
    let _raw_guard = RawModeGuard::new()?;

    // Create channel for PTY output
    let (pty_output_tx, pty_output_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Clone PTY reader for the reader thread
    let mut pty_reader = pty.try_clone_reader()?;

    // Spawn thread to read PTY output
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_output_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("PTY read error: {}", e);
                    break;
                }
            }
        }
    });

    // Main event loop
    loop {
        // Check for PTY output (non-blocking)
        while let Ok(output) = pty_output_rx.try_recv() {
            // Display locally
            write_stdout(&output)?;

            // Broadcast to viewers (and store in buffer)
            state.broadcast_output(&output).await;
        }

        // Check for viewer input (non-blocking)
        while let Ok(input) = viewer_input_rx.try_recv() {
            // Send viewer input to PTY
            pty.write(&input)?;
        }

        // Check for local keyboard input
        if let Some(event) = read_event(Duration::from_millis(10))? {
            match event {
                TerminalEvent::Key(bytes) => {
                    pty.write(&bytes)?;
                }
                TerminalEvent::Quit => {
                    break;
                }
                TerminalEvent::Resize { cols, rows } => {
                    pty.resize(cols, rows)?;
                    // Broadcast resize to viewers
                    state.broadcast_resize(cols, rows).await;
                }
            }
        }

        // Check if PTY reader finished (shell exited)
        if reader_handle.is_finished() {
            break;
        }

        // Small yield to prevent busy loop
        tokio::task::yield_now().await;
    }

    let _ = reader_handle.join();
    Ok(())
}

mod cider;
mod lyrics;
mod overlay;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;
use tracing::{info, warn};

// Global visibility flag, toggled by SIGUSR1
pub static VISIBLE: AtomicBool = AtomicBool::new(true);

fn setup_signal_handler() {
    std::thread::spawn(|| {
        use std::os::unix::net::UnixListener;
        use std::fs;

        let socket_path = "/tmp/lyrics-overlay.sock";
        let _ = fs::remove_file(socket_path);

        if let Ok(listener) = UnixListener::bind(socket_path) {
            for stream in listener.incoming() {
                if stream.is_ok() {
                    let was_visible = VISIBLE.fetch_xor(true, Ordering::SeqCst);
                    info!("Toggled visibility: {} -> {}", was_visible, !was_visible);
                }
            }
        }
    });

    // Also handle SIGUSR1
    std::thread::spawn(|| {
        unsafe {
            libc::signal(libc::SIGUSR1, toggle_handler as usize);
        }
    });
}

extern "C" fn toggle_handler(_: i32) {
    let was_visible = VISIBLE.fetch_xor(true, Ordering::SeqCst);
    // Can't use tracing in signal handler, just toggle
    let _ = was_visible;
}

#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub song_name: String,
    pub artist_name: String,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub is_playing: bool,
    pub track_id: String,
}

#[derive(Debug, Clone)]
pub struct LyricLine {
    pub time_ms: u64,
    pub text: String,
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("lyrics_overlay=debug".parse()?)
        )
        .init();

    info!("Starting lyrics overlay");

    // Set up SIGUSR1 handler for toggle
    setup_signal_handler();
    info!("Send SIGUSR1 to toggle visibility (pkill -USR1 lyrics-overlay)");

    // Create tokio runtime
    let rt = tokio::runtime::Runtime::new()?;

    // Channel for playback state updates
    let (playback_tx, playback_rx) = watch::channel(PlaybackState::default());

    // Channel for current lyric line
    let (lyric_tx, lyric_rx) = watch::channel(String::new());

    // Spawn Cider poller in background
    let playback_tx_clone = playback_tx.clone();
    rt.spawn(async move {
        if let Err(e) = cider::poll_cider(playback_tx_clone).await {
            warn!("Cider poller error: {}", e);
        }
    });

    // Spawn lyrics synchronizer
    let lyric_tx_clone = lyric_tx.clone();
    let playback_rx_clone = playback_rx.clone();
    rt.spawn(async move {
        lyrics::sync_lyrics(playback_rx_clone, lyric_tx_clone).await;
    });

    // Run the Wayland overlay on the main thread
    overlay::run_overlay(lyric_rx)?;

    Ok(())
}

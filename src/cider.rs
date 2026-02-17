//! Cider 2 REST API client
//!
//! Polls the Cider API at localhost:10767 for playback state.
//!
//! Requires API token from Cider Settings > Connectivity.
//! Set the token via CIDER_API_TOKEN environment variable.

use crate::PlaybackState;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

const CIDER_API_BASE: &str = "http://localhost:10767";
const POLL_INTERVAL: Duration = Duration::from_millis(500);

fn get_api_token() -> Option<String> {
    env::var("CIDER_API_TOKEN").ok()
}

#[derive(Debug, Deserialize)]
struct NowPlayingResponse {
    info: Option<SongInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SongInfo {
    name: Option<String>,
    artist_name: Option<String>,
    #[serde(default)]
    current_playback_time: f64,
    #[serde(default)]
    duration_in_millis: u64,
    play_params: Option<PlayParams>,
}

#[derive(Debug, Deserialize)]
struct PlayParams {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IsPlayingResponse {
    #[allow(dead_code)]
    status: Option<String>,
    is_playing: bool,
}

pub async fn poll_cider(tx: watch::Sender<PlaybackState>) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let api_token = get_api_token();
    if api_token.is_none() {
        warn!("CIDER_API_TOKEN not set - API calls may fail");
        warn!("Get token from Cider Settings > Connectivity > API Token");
    }

    info!("Starting Cider poller, connecting to {}", CIDER_API_BASE);

    let mut consecutive_errors = 0;

    loop {
        match fetch_playback_state(&client, api_token.as_deref()).await {
            Ok(state) => {
                consecutive_errors = 0;
                debug!(
                    "Playback: {} - {} @ {:.1}s (playing: {})",
                    state.artist_name, state.song_name, state.position_secs, state.is_playing
                );
                let _ = tx.send(state);
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors <= 3 {
                    warn!("Failed to fetch playback state: {}", e);
                } else if consecutive_errors == 4 {
                    warn!("Cider connection failed, will retry silently...");
                }
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn fetch_playback_state(
    client: &reqwest::Client,
    api_token: Option<&str>,
) -> Result<PlaybackState> {
    // Build request with optional token
    let mut now_playing_req = client.get(format!("{}/api/v1/playback/now-playing", CIDER_API_BASE));
    let mut is_playing_req = client.get(format!("{}/api/v1/playback/is-playing", CIDER_API_BASE));

    if let Some(token) = api_token {
        now_playing_req = now_playing_req.header("apptoken", token);
        is_playing_req = is_playing_req.header("apptoken", token);
    }

    // Fetch now playing info
    let now_playing: NowPlayingResponse = now_playing_req
        .send()
        .await
        .context("Failed to connect to Cider")?
        .json()
        .await
        .context("Failed to parse now-playing response")?;

    // Fetch is-playing status
    let is_playing: IsPlayingResponse = is_playing_req
        .send()
        .await
        .context("Failed to fetch is-playing")?
        .json()
        .await
        .context("Failed to parse is-playing response")?;

    let info = now_playing.info.unwrap_or(SongInfo {
        name: None,
        artist_name: None,
        current_playback_time: 0.0,
        duration_in_millis: 0,
        play_params: None,
    });

    Ok(PlaybackState {
        song_name: info.name.unwrap_or_default(),
        artist_name: info.artist_name.unwrap_or_default(),
        position_secs: info.current_playback_time,
        duration_secs: info.duration_in_millis as f64 / 1000.0,
        is_playing: is_playing.is_playing,
        track_id: info.play_params.and_then(|p| p.id).unwrap_or_default(),
    })
}

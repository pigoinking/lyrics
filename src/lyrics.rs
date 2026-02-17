//! LRCLIB lyrics fetcher and LRC parser
//!
//! Fetches synchronized lyrics from lrclib.net and parses LRC format.

use crate::{LyricLine, PlaybackState};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::sync::Mutex;
use tokio::sync::watch;
use tracing::{debug, info, warn};

const LRCLIB_API: &str = "https://lrclib.net/api";

/// Time offset in milliseconds to advance lyrics display.
/// Compensates for system latency. Can be set via LYRICS_OFFSET_MS env var.
fn get_lyrics_offset_ms() -> i64 {
    env::var("LYRICS_OFFSET_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300) // Default 300ms advance
}

/// Whether to allow lyrics from a different artist (timing will likely be wrong).
/// Set LYRICS_ALLOW_ARTIST_MISMATCH=1 to enable.
fn allow_artist_mismatch() -> bool {
    env::var("LYRICS_ALLOW_ARTIST_MISMATCH")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false) // Default: don't use mismatched artists
}

// Simple in-memory cache
static LYRICS_CACHE: Mutex<Option<HashMap<String, Vec<LyricLine>>>> = Mutex::new(None);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    #[allow(dead_code)]
    plain_lyrics: Option<String>,
    instrumental: Option<bool>,
}

/// Parse LRC format into timestamped lines
/// Format: [MM:SS.cs] Text
fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();

    for line in lrc.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('[') {
            continue;
        }

        // Find the closing bracket
        if let Some(bracket_end) = line.find(']') {
            let timestamp = &line[1..bracket_end];
            let text = line[bracket_end + 1..].trim().to_string();

            // Skip empty lines or metadata tags
            if text.is_empty() || timestamp.contains(':') == false {
                continue;
            }

            // Parse timestamp: MM:SS.cs or MM:SS
            if let Some(time_ms) = parse_timestamp(timestamp) {
                lines.push(LyricLine { time_ms, text });
            }
        }
    }

    // Sort by time
    lines.sort_by_key(|l| l.time_ms);
    lines
}

fn parse_timestamp(ts: &str) -> Option<u64> {
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 2 {
        return None;
    }

    let minutes: u64 = parts[0].parse().ok()?;

    // Handle seconds with or without centiseconds
    let secs_parts: Vec<&str> = parts[1].split('.').collect();
    let seconds: u64 = secs_parts[0].parse().ok()?;
    let centisecs: u64 = if secs_parts.len() > 1 {
        // Pad or truncate to 2 digits
        let cs_str = secs_parts[1];
        let cs: u64 = cs_str.parse().ok()?;
        if cs_str.len() == 1 {
            cs * 10
        } else if cs_str.len() == 2 {
            cs
        } else {
            cs / 10u64.pow(cs_str.len() as u32 - 2)
        }
    } else {
        0
    };

    Some(minutes * 60_000 + seconds * 1000 + centisecs * 10)
}

/// Search results from LRCLIB
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibSearchResult {
    id: i64,
    track_name: String,
    artist_name: String,
    synced_lyrics: Option<String>,
    instrumental: Option<bool>,
}

async fn fetch_lyrics(artist: &str, track: &str) -> Result<Vec<LyricLine>> {
    let cache_key = format!("{}:{}", artist.to_lowercase(), track.to_lowercase());

    // Check cache
    {
        let cache = LYRICS_CACHE.lock().unwrap();
        if let Some(ref map) = *cache {
            if let Some(lyrics) = map.get(&cache_key) {
                debug!("Cache hit for '{} - {}'", artist, track);
                return Ok(lyrics.clone());
            }
        }
    }

    info!("Fetching lyrics for '{} - {}'", artist, track);

    let client = reqwest::Client::new();

    // Try exact match first
    let url = format!(
        "{}/get?artist_name={}&track_name={}",
        LRCLIB_API,
        urlencoding::encode(artist),
        urlencoding::encode(track)
    );

    let response = client
        .get(&url)
        .header("User-Agent", "lyrics-overlay/0.1.0")
        .send()
        .await
        .context("Failed to fetch from LRCLIB")?;

    let lyrics = if response.status().is_success() {
        let data: LrclibResponse = response.json().await.context("Failed to parse LRCLIB response")?;
        if data.instrumental == Some(true) {
            info!("Track is instrumental: '{} - {}'", artist, track);
            vec![]
        } else if let Some(synced) = data.synced_lyrics {
            parse_lrc(&synced)
        } else {
            vec![]
        }
    } else {
        // Fallback: use search endpoint with fuzzy matching
        debug!("Exact match failed, trying search for '{} {}'", track, artist);

        // Try multiple search strategies
        let search_queries = vec![
            format!("{} {}", track, artist),
            track.to_string(),
        ];

        let mut found_lyrics = vec![];

        for query in search_queries {
            let search_url = format!(
                "{}/search?q={}",
                LRCLIB_API,
                urlencoding::encode(&query)
            );

            if let Ok(resp) = client
                .get(&search_url)
                .header("User-Agent", "lyrics-overlay/0.1.0")
                .send()
                .await
            {
                if resp.status().is_success() {
                    if let Ok(results) = resp.json::<Vec<LrclibSearchResult>>().await {
                        // Find best match - prioritize artist match
                        let artist_lower = artist.to_lowercase();
                        let track_lower = track.to_lowercase();

                        // First pass: require both track AND artist match
                        for result in &results {
                            if result.synced_lyrics.is_some() && result.instrumental != Some(true) {
                                let result_artist = result.artist_name.to_lowercase();
                                let result_track = result.track_name.to_lowercase();

                                let track_match = result_track.contains(&track_lower)
                                    || track_lower.contains(&result_track);
                                let artist_match = result_artist.contains(&artist_lower)
                                    || artist_lower.contains(&result_artist);

                                if track_match && artist_match {
                                    info!(
                                        "Found lyrics via search (artist match): '{}' by '{}'",
                                        result.track_name, result.artist_name
                                    );
                                    found_lyrics = parse_lrc(result.synced_lyrics.as_ref().unwrap());
                                    break;
                                }
                            }
                        }

                        // Second pass: only if no artist match AND user allows mismatch
                        if found_lyrics.is_empty() && allow_artist_mismatch() {
                            for result in &results {
                                if result.synced_lyrics.is_some() && result.instrumental != Some(true) {
                                    let result_track = result.track_name.to_lowercase();
                                    let track_match = result_track.contains(&track_lower)
                                        || track_lower.contains(&result_track);

                                    if track_match {
                                        warn!(
                                            "Using lyrics with artist mismatch: '{}' by '{}' (wanted '{}')",
                                            result.track_name, result.artist_name, artist
                                        );
                                        found_lyrics = parse_lrc(result.synced_lyrics.as_ref().unwrap());
                                        break;
                                    }
                                }
                            }
                        } else if found_lyrics.is_empty() {
                            // Log what we could have found but skipped
                            for result in &results {
                                if result.synced_lyrics.is_some() && result.instrumental != Some(true) {
                                    let result_track = result.track_name.to_lowercase();
                                    let track_match = result_track.contains(&track_lower)
                                        || track_lower.contains(&result_track);

                                    if track_match {
                                        info!(
                                            "Skipped lyrics with wrong artist: '{}' by '{}' (wanted '{}') - set LYRICS_ALLOW_ARTIST_MISMATCH=1 to use",
                                            result.track_name, result.artist_name, artist
                                        );
                                        break;
                                    }
                                }
                            }
                        }

                        if !found_lyrics.is_empty() {
                            break;
                        }
                    }
                }
            }
        }

        if found_lyrics.is_empty() {
            info!("No lyrics found for '{} - {}'", artist, track);
        }
        found_lyrics
    };

    // Cache the result
    {
        let mut cache = LYRICS_CACHE.lock().unwrap();
        if cache.is_none() {
            *cache = Some(HashMap::new());
        }
        cache.as_mut().unwrap().insert(cache_key, lyrics.clone());
    }

    Ok(lyrics)
}

/// Find the current lyric line based on playback position
fn find_current_lyric(lyrics: &[LyricLine], position_ms: u64) -> Option<&LyricLine> {
    if lyrics.is_empty() {
        return None;
    }

    // Binary search for the line just before or at current position
    let idx = lyrics.partition_point(|l| l.time_ms <= position_ms);

    if idx == 0 {
        // Before first lyric - show nothing or first line
        if position_ms + 2000 >= lyrics[0].time_ms {
            // Within 2 seconds of first line, show it
            Some(&lyrics[0])
        } else {
            None
        }
    } else {
        Some(&lyrics[idx - 1])
    }
}

pub async fn sync_lyrics(
    playback_rx: watch::Receiver<PlaybackState>,
    lyric_tx: watch::Sender<String>,
) {
    let mut current_track_id = String::new();
    let mut current_lyrics: Vec<LyricLine> = vec![];
    let mut last_lyric_text = String::new();

    let offset_ms = get_lyrics_offset_ms();
    info!("Lyrics time offset: {}ms", offset_ms);

    loop {
        let state = playback_rx.borrow().clone();

        // Check if track changed
        if state.track_id != current_track_id && !state.track_id.is_empty() {
            current_track_id = state.track_id.clone();
            info!(
                "Track changed: {} - {}",
                state.artist_name, state.song_name
            );

            // Fetch new lyrics
            match fetch_lyrics(&state.artist_name, &state.song_name).await {
                Ok(lyrics) => {
                    info!("Loaded {} lyric lines", lyrics.len());
                    current_lyrics = lyrics;
                }
                Err(e) => {
                    warn!("Failed to fetch lyrics: {}", e);
                    current_lyrics = vec![];
                }
            }
            last_lyric_text.clear();
        }

        // Find current lyric (with time offset to compensate for latency)
        if state.is_playing && !current_lyrics.is_empty() {
            let position_ms = (state.position_secs * 1000.0) as i64 + offset_ms;
            let position_ms = position_ms.max(0) as u64;

            if let Some(lyric) = find_current_lyric(&current_lyrics, position_ms) {
                if lyric.text != last_lyric_text {
                    last_lyric_text = lyric.text.clone();
                    let _ = lyric_tx.send(lyric.text.clone());
                }
            }
        } else if !state.is_playing {
            // Paused - keep showing current lyric
        } else if current_lyrics.is_empty() && !state.song_name.is_empty() {
            // No lyrics available - show message
            let no_lyrics_msg = "♪ No lyrics found".to_string();
            if last_lyric_text != no_lyrics_msg {
                last_lyric_text = no_lyrics_msg.clone();
                let _ = lyric_tx.send(no_lyrics_msg);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timestamp() {
        assert_eq!(parse_timestamp("00:27.93"), Some(27930));
        assert_eq!(parse_timestamp("01:30.00"), Some(90000));
        assert_eq!(parse_timestamp("00:05"), Some(5000));
        assert_eq!(parse_timestamp("02:45.5"), Some(165500));
    }

    #[test]
    fn test_parse_lrc() {
        let lrc = r#"
[00:27.93] Listen to the wind blow
[00:30.88] Watch the sun rise
[00:35.00] Running in the shadows
"#;
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 27930);
        assert_eq!(lines[0].text, "Listen to the wind blow");
        assert_eq!(lines[1].time_ms, 30880);
        assert_eq!(lines[2].time_ms, 35000);
    }
}

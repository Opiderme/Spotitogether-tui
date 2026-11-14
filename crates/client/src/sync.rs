//! Mirrors the room playback on the local user's Spotify account.

use std::time::{Duration, Instant};

use protocol::{Playback, TrackInfo};
use spotify::models::PlaybackState;
use spotify::{Error as SpotifyError, SpotifyClient};

use crate::clock::Clock;

/// Correct the local player when it drifts further than this from the room.
pub const MAX_DRIFT_MS: i64 = 1_500;
/// Initial guess of the delay between a play command and audible playback.
const DEFAULT_LATENCY_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq)]
pub enum SyncEvent {
    /// The room state was applied on Spotify.
    Applied { track: Option<String>, position_ms: u64, playing: bool },
    /// The local player drifted and was re-aligned.
    Corrected { drift_ms: i64 },
    /// The local player was not playing the room's track; it is restarted.
    Resynced { reason: String },
    /// Playback stopped on this account (no longer allowed to listen).
    Stopped,
    /// A device was picked because none was active.
    DeviceSelected { name: String },
    Error(String),
}

pub struct Syncer {
    device_id: Option<String>,
    /// Exponential moving average of Spotify command latency (ms).
    latency_ms: u64,
    /// URI and play state last pushed to Spotify.
    applied: Option<(String, bool)>,
}

impl Syncer {
    pub fn new(device_id: Option<String>) -> Self {
        Self { device_id, latency_ms: DEFAULT_LATENCY_MS, applied: None }
    }

    /// Picks a device when none is active; returns an event to show.
    pub async fn choose_device(&mut self, spotify: &SpotifyClient) -> Option<SyncEvent> {
        if self.device_id.is_some() {
            return None;
        }
        let devices = spotify.devices().await.ok()?;
        if devices.iter().any(|d| d.is_active) {
            return None;
        }
        let d = spotify::models::pick_idle_device(&devices)?;
        self.device_id = d.id.clone();
        Some(SyncEvent::DeviceSelected { name: d.name.clone() })
    }

    /// Pushes the room state to Spotify.
    pub async fn apply(&mut self, spotify: &SpotifyClient, clock: &Clock, playback: &Playback) -> Vec<SyncEvent> {
        let mut events = Vec::new();
        let result = self.try_apply(spotify, clock, playback).await;
        let result = match result {
            // The device went idle in the meantime: pick one and retry once.
            Err(SpotifyError::NoActiveDevice) => {
                self.device_id = None;
                events.extend(self.choose_device(spotify).await);
                self.try_apply(spotify, clock, playback).await
            }
            other => other,
        };
        match result {
            Ok(position_ms) => events.push(SyncEvent::Applied {
                track: playback.current.as_ref().map(|i| format!("{} — {}", i.track.name, i.track.artists)),
                position_ms,
                playing: playback.is_playing,
            }),
            Err(err) => events.push(SyncEvent::Error(err.to_string())),
        }
        events
    }

    async fn try_apply(&mut self, spotify: &SpotifyClient, clock: &Clock, playback: &Playback) -> spotify::Result<u64> {
        let device = self.device_id.as_deref();
        let Some(item) = &playback.current else {
            if matches!(self.applied, Some((_, true))) {
                spotify.pause(device).await?;
            }
            self.applied = None;
            return Ok(0);
        };

        if !playback.is_playing {
            if matches!(&self.applied, Some((uri, true)) if *uri == item.track.uri) {
                spotify.pause(device).await?;
            }
            self.applied = Some((item.track.uri.clone(), false));
            return Ok(playback.position_at(clock.server_now()));
        }

        // Aim slightly ahead to compensate for the command latency.
        let target = (playback.position_at(clock.server_now()) + self.latency_ms).min(item.track.duration_ms);
        let started = Instant::now();
        spotify
            .play_track(device, &item.track.uri, item.track.album_uri.as_deref(), target)
            .await?;
        self.observe_latency(started.elapsed());
        self.applied = Some((item.track.uri.clone(), true));
        Ok(target)
    }

    /// Stops mirroring the room (the `Listen` permission was withdrawn).
    pub async fn stop(&mut self, spotify: &SpotifyClient) -> Option<SyncEvent> {
        let was_playing = matches!(self.applied, Some((_, true)));
        self.applied = None;
        if !was_playing {
            return None;
        }
        match spotify.pause(self.device_id.as_deref()).await {
            Ok(()) => Some(SyncEvent::Stopped),
            Err(e) => Some(SyncEvent::Error(e.to_string())),
        }
    }

    /// Compares the local player with the room and corrects drift.
    pub async fn check(&mut self, spotify: &SpotifyClient, clock: &Clock, playback: &Playback) -> Vec<SyncEvent> {
        let Some(item) = playback.current.as_ref().filter(|_| playback.is_playing) else {
            return vec![];
        };
        let started = Instant::now();
        let state = match spotify.playback().await {
            Ok(s) => s,
            Err(err) => return vec![SyncEvent::Error(err.to_string())],
        };
        // Spotify sampled its position roughly half-way through the request.
        let half_rtt = started.elapsed().as_millis() as u64 / 2;
        let expected = playback.position_at(clock.server_now().saturating_sub(half_rtt)) as i64;

        if let Some(reason) = mismatch(state.as_ref(), &item.track) {
            // Paused, other track, or nothing playing: re-apply the room state.
            let mut events = vec![SyncEvent::Resynced { reason }];
            events.extend(self.apply(spotify, clock, playback).await);
            return events;
        }

        let actual = state.and_then(|s| s.progress_ms).unwrap_or(0) as i64;
        let drift = actual - expected;
        if drift.abs() <= MAX_DRIFT_MS {
            return vec![];
        }
        let target = (playback.position_at(clock.server_now()) + self.latency_ms).min(item.track.duration_ms);
        match spotify.seek(self.device_id.as_deref(), target).await {
            Ok(()) => vec![SyncEvent::Corrected { drift_ms: drift }],
            Err(err) => vec![SyncEvent::Error(err.to_string())],
        }
    }

    fn observe_latency(&mut self, elapsed: Duration) {
        // Half the command round trip, smoothed (EMA, alpha = 1/4).
        let sample = (elapsed.as_millis() as u64 / 2).min(2_000);
        self.latency_ms = (self.latency_ms * 3 + sample) / 4;
    }
}

/// Why the local player does not match the room track, if it does not.
/// Spotify may play a relinked copy of a track under another URI (and no
/// longer reports `linked_from` since 2026), so the same title counts as a
/// match.
fn mismatch(state: Option<&PlaybackState>, expected: &TrackInfo) -> Option<String> {
    let Some(state) = state else {
        return Some("rien en lecture sur ton Spotify".into());
    };
    let Some(item) = &state.item else {
        return Some("rien en lecture sur ton Spotify".into());
    };
    let same_track = item.uri == expected.uri || item.name.eq_ignore_ascii_case(&expected.name);
    if !same_track {
        return Some(format!("ton Spotify jouait « {} »", item.name));
    }
    if !state.is_playing {
        return Some("ton Spotify était en pause".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use spotify::models::Track;

    fn expected() -> TrackInfo {
        TrackInfo {
            uri: "spotify:track:a".into(),
            name: "Aerodynamic".into(),
            artists: "Daft Punk".into(),
            duration_ms: 1,
            album_uri: None,
            cover_url: None,
        }
    }

    fn state(uri: &str, name: &str, playing: bool) -> PlaybackState {
        PlaybackState {
            device: None,
            is_playing: playing,
            progress_ms: Some(0),
            item: Some(Track { uri: uri.into(), name: name.into(), ..Default::default() }),
        }
    }

    #[test]
    fn detects_mismatches() {
        assert_eq!(mismatch(Some(&state("spotify:track:a", "Aerodynamic", true)), &expected()), None);
        // Relinked copy: other URI, same title.
        assert_eq!(mismatch(Some(&state("spotify:track:b", "aerodynamic", true)), &expected()), None);
        assert!(mismatch(Some(&state("spotify:track:c", "One More Time", true)), &expected()).unwrap().contains("One More Time"));
        assert!(mismatch(Some(&state("spotify:track:a", "Aerodynamic", false)), &expected()).unwrap().contains("pause"));
        assert!(mismatch(None, &expected()).is_some());
    }
}

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
/// Close to the end of a track, the local player legitimately runs ahead
/// or stops: no drift or mismatch correction there (it would jump back
/// into the ending track).
const END_WINDOW_MS: u64 = 4_000;
/// How long before the room track ends the local player is looked at, to
/// stop it right at its own end.
const END_PROBE_BEFORE_MS: u64 = 6_000;
/// The pause lands this long before the local track's end, so Spotify never
/// starts the next track of the album (the playback context).
const END_MARGIN_MS: u64 = 150;

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
    /// A mismatch seen once: Spotify can lag in reporting a new track, so
    /// it is only corrected when the next check confirms it.
    suspect: bool,
}

impl Syncer {
    pub fn new(device_id: Option<String>) -> Self {
        Self { device_id, latency_ms: DEFAULT_LATENCY_MS, applied: None, suspect: false }
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
        self.suspect = false;
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
        if in_end_window(clock, playback) {
            return vec![];
        }
        let started = Instant::now();
        let state = match spotify.playback().await {
            Ok(s) => s,
            Err(err) => return vec![SyncEvent::Error(err.to_string())],
        };
        // Spotify sampled its position roughly half-way through the request.
        let half_rtt = started.elapsed().as_millis() as u64 / 2;
        let expected = playback.position_at(clock.server_now().saturating_sub(half_rtt)) as i64;

        if let Some(reason) = mismatch(state.as_ref(), &item.track) {
            // Seen once: wait for the next check (soon, see `wants_recheck`).
            if !self.suspect {
                self.suspect = true;
                return vec![];
            }
            // Confirmed: paused, other track, or nothing playing: re-apply.
            let mut events = vec![SyncEvent::Resynced { reason }];
            events.extend(self.apply(spotify, clock, playback).await);
            return events;
        }

        self.suspect = false;
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

    /// A mismatch waits for confirmation: check again shortly.
    pub fn wants_recheck(&self) -> bool {
        self.suspect
    }

    /// When to call [`Self::time_to_end`] for the current room track.
    pub fn end_probe_in(&self, clock: &Clock, playback: &Playback) -> Option<Duration> {
        let item = playback.current.as_ref().filter(|_| playback.is_playing)?;
        let remaining = item.track.duration_ms.saturating_sub(playback.position_at(clock.server_now()));
        Some(Duration::from_millis(remaining.saturating_sub(END_PROBE_BEFORE_MS)))
    }

    /// Looks at the local player near the end of the room track and returns
    /// when to pause it, just before its own end: the room's next track
    /// then starts from silence instead of the album's next track.
    pub async fn time_to_end(&mut self, spotify: &SpotifyClient, playback: &Playback) -> Option<Duration> {
        let item = playback.current.as_ref().filter(|_| playback.is_playing)?;
        let started = Instant::now();
        let state = spotify.playback().await.ok()??;
        let half_rtt = started.elapsed().as_millis() as u64 / 2;
        if mismatch(Some(&state), &item.track).is_some() {
            return None;
        }
        let local = state.item.as_ref()?;
        // The copy Spotify plays may last longer or shorter than the room's.
        let duration = if local.duration_ms > 0 { local.duration_ms } else { item.track.duration_ms };
        let progress = state.progress_ms? + half_rtt;
        let remaining = duration.saturating_sub(progress);
        Some(Duration::from_millis(remaining.saturating_sub(self.latency_ms + END_MARGIN_MS)))
    }

    /// Pauses the local player at the end of the room track (see
    /// [`Self::time_to_end`]). The next room state plays as usual.
    pub async fn hold_at_end(&mut self, spotify: &SpotifyClient) -> Option<SyncEvent> {
        if !matches!(self.applied, Some((_, true))) {
            return None;
        }
        match spotify.pause(self.device_id.as_deref()).await {
            Ok(()) => {
                if let Some((_, playing)) = &mut self.applied {
                    *playing = false;
                }
                None
            }
            Err(e) => Some(SyncEvent::Error(e.to_string())),
        }
    }

    fn observe_latency(&mut self, elapsed: Duration) {
        // Half the command round trip, smoothed (EMA, alpha = 1/4).
        let sample = (elapsed.as_millis() as u64 / 2).min(2_000);
        self.latency_ms = (self.latency_ms * 3 + sample) / 4;
    }
}

/// The room track is about to end (or the server is late moving on).
fn in_end_window(clock: &Clock, playback: &Playback) -> bool {
    playback.current.as_ref().is_some_and(|item| {
        item.track.duration_ms.saturating_sub(playback.position_at(clock.server_now())) < END_WINDOW_MS
    })
}

/// Title without its version suffix: "Song - 2011 Remaster" and
/// "Song (Remastered)" both give "song".
fn base_title(name: &str) -> String {
    let lower = name.to_lowercase();
    let cut = [" - ", " ("].iter().filter_map(|sep| lower.find(sep)).min().unwrap_or(lower.len());
    lower[..cut].trim().to_owned()
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
    let same_track = item.uri == expected.uri
        || item.name.eq_ignore_ascii_case(&expected.name)
        || (!base_title(&item.name).is_empty() && base_title(&item.name) == base_title(&expected.name));
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
            album_uri: None, album_name: None,
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
        // Relinked copy under a remaster title.
        assert_eq!(mismatch(Some(&state("spotify:track:d", "Aerodynamic - 2001 Remaster", true)), &expected()), None);
        assert_eq!(mismatch(Some(&state("spotify:track:e", "Aerodynamic (Live)", true)), &expected()), None);
    }

    #[test]
    fn base_title_drops_version_suffixes() {
        assert_eq!(base_title("One More Time - Radio Edit"), "one more time");
        assert_eq!(base_title("Harder (feat. X) - Remix"), "harder");
        assert_eq!(base_title("Plain"), "plain");
    }
}

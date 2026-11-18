use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, RETRY_AFTER};
pub use reqwest::Method;
use reqwest::{Response, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{self, Token};
use crate::models::{Device, DevicesResponse, Paging, PlaybackState, PlaylistEntry, PlaylistSummary, SearchResponse, Track, User};
use crate::{Error, Result};

const API: &str = "https://api.spotify.com/v1";
/// Development Mode caps `limit` on /search at 10 since February 2026.
pub const SEARCH_MAX_LIMIT: u32 = 10;
/// URIs per /me/library request.
const LIBRARY_BATCH: usize = 40;
const MAX_RATE_LIMIT_RETRIES: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

/// Called with the new token every time it is refreshed, so the caller can
/// persist it (keyring, file...).
pub type TokenSink = Box<dyn Fn(&Token) + Send + Sync>;

pub struct SpotifyClient {
    http: reqwest::Client,
    client_id: String,
    token: Mutex<Token>,
    on_refresh: Option<TokenSink>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    message: String,
    #[serde(default)]
    reason: Option<String>,
}

impl SpotifyClient {
    pub fn new(client_id: impl Into<String>, token: Token) -> Self {
        Self {
            http: Self::http_client(),
            client_id: client_id.into(),
            token: Mutex::new(token),
            on_refresh: None,
        }
    }

    pub fn with_token_sink(mut self, sink: TokenSink) -> Self {
        self.on_refresh = Some(sink);
        self
    }

    /// HTTP client shared by the auth flow and the API client.
    pub fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent(concat!("spotitogether/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
            .expect("valid reqwest configuration")
    }

    // ---------------------------------------------------------------- user

    pub async fn me(&self) -> Result<User> {
        self.get_json("/me", &[]).await
    }

    // -------------------------------------------------------------- player

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let r: DevicesResponse = self.get_json("/me/player/devices", &[]).await?;
        Ok(r.devices)
    }

    /// Current playback, or `None` when nothing is playing on any device.
    pub async fn playback(&self) -> Result<Option<PlaybackState>> {
        let resp = self.request(Method::GET, "/me/player", &[], None).await?;
        if resp.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let body = resp.text().await?;
        if body.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&body)?))
    }

    /// Starts `track_uri` at `position_ms` on `device_id` (or the active device).
    ///
    /// With `album_uri` the track is started inside its album context
    /// (`context_uri` + `offset`). As of November 2026 the desktop client
    /// ignores the `uris` form and restarts the previous track at 0:00, while
    /// the album form works; `uris` is only the fallback.
    pub async fn play_track(
        &self,
        device_id: Option<&str>,
        track_uri: &str,
        album_uri: Option<&str>,
        position_ms: u64,
    ) -> Result<()> {
        let body = play_body(track_uri, album_uri, position_ms);
        self.request(Method::PUT, "/me/player/play", &device_query(device_id), Some(&body))
            .await?;
        Ok(())
    }

    /// Resumes whatever was paused.
    pub async fn resume(&self, device_id: Option<&str>) -> Result<()> {
        self.request(Method::PUT, "/me/player/play", &device_query(device_id), None)
            .await?;
        Ok(())
    }

    pub async fn pause(&self, device_id: Option<&str>) -> Result<()> {
        self.request(Method::PUT, "/me/player/pause", &device_query(device_id), None)
            .await?;
        Ok(())
    }

    pub async fn seek(&self, device_id: Option<&str>, position_ms: u64) -> Result<()> {
        let mut query = device_query(device_id);
        query.push(("position_ms", position_ms.to_string()));
        self.request(Method::PUT, "/me/player/seek", &query, None).await?;
        Ok(())
    }

    /// Volume of this account's player (0–100). Only this member hears it.
    pub async fn set_volume(&self, device_id: Option<&str>, percent: u8) -> Result<()> {
        let mut query = device_query(device_id);
        query.push(("volume_percent", percent.min(100).to_string()));
        self.request(Method::PUT, "/me/player/volume", &query, None).await?;
        Ok(())
    }

    /// Current volume of the active device, if it reports one.
    pub async fn volume(&self) -> Result<Option<u8>> {
        let devices = self.devices().await?;
        Ok(devices.iter().find(|d| d.is_active).or(devices.first()).filter(|d| d.supports_volume).and_then(|d| d.volume_percent))
    }

    /// Moves playback to `device_id`.
    pub async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        let body = json!({ "device_ids": [device_id], "play": play });
        self.request(Method::PUT, "/me/player", &[], Some(&body)).await?;
        Ok(())
    }

    // -------------------------------------------------------------- catalog

    /// Track search; `limit` is clamped to the Development Mode maximum (10).
    pub async fn search_tracks(&self, query: &str, limit: u32, offset: u32) -> Result<Paging<Track>> {
        let limit = limit.clamp(1, SEARCH_MAX_LIMIT);
        let r: SearchResponse = self
            .get_json(
                "/search",
                &[
                    ("q", query.to_owned()),
                    ("type", "track".to_owned()),
                    ("limit", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        Ok(r.tracks)
    }

    pub async fn track(&self, id: &str) -> Result<Track> {
        self.get_json(&format!("/tracks/{id}"), &[]).await
    }

    // -------------------------------------------------------------- library

    /// Which of `uris` are in the user's library (liked), in order.
    /// Spotify accepts 40 URIs per request; longer lists are split.
    pub async fn library_contains(&self, uris: &[String]) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(uris.len());
        for chunk in uris.chunks(LIBRARY_BATCH) {
            let flags: Vec<bool> = self.get_json("/me/library/contains", &[("uris", chunk.join(","))]).await?;
            out.extend(flags);
        }
        Ok(out)
    }

    /// Adds tracks to the user's liked songs (or removes them).
    pub async fn set_liked(&self, uris: &[String], liked: bool) -> Result<()> {
        let method = if liked { Method::PUT } else { Method::DELETE };
        for chunk in uris.chunks(LIBRARY_BATCH) {
            self.request(method.clone(), "/me/library", &[("uris", chunk.join(","))], None).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ playlists

    /// The user's playlists (first 50).
    pub async fn my_playlists(&self) -> Result<Vec<PlaylistSummary>> {
        let page: Paging<PlaylistSummary> = self.get_json("/me/playlists", &[("limit", "50".to_owned())]).await?;
        Ok(page.items)
    }

    /// Tracks of a playlist, up to `max` (episodes and local files skipped).
    /// Since 2026, Development Mode only returns the items of playlists the
    /// user owns or collaborates on.
    pub async fn playlist_tracks(&self, id: &str, max: usize) -> Result<Vec<Track>> {
        match self.collect_tracks(&format!("/playlists/{id}/items"), max).await {
            Err(Error::Api { status: 403, .. }) => Err(Error::PlaylistNotOwned),
            other => other,
        }
    }

    /// The user's liked songs, most recent first, up to `max`.
    pub async fn saved_tracks(&self, max: usize) -> Result<Vec<Track>> {
        self.collect_tracks("/me/tracks", max).await
    }

    /// Pages through a list of `{item|track}` entries, keeping playable tracks.
    async fn collect_tracks(&self, path: &str, max: usize) -> Result<Vec<Track>> {
        const PAGE: usize = 50;
        let mut tracks = Vec::new();
        let mut offset = 0;
        loop {
            // Entries are decoded one by one: an odd entry (local file,
            // episode, unavailable track) is skipped instead of failing the
            // whole list.
            let page: Paging<Value> = self
                .get_json(path, &[("limit", PAGE.to_string()), ("offset", offset.to_string())])
                .await?;
            let count = page.items.len();
            tracks.extend(page.items.into_iter().filter_map(playable_track));
            offset += count;
            if count < PAGE || tracks.len() >= max || offset >= page.total as usize {
                break;
            }
        }
        tracks.truncate(max);
        Ok(tracks)
    }

    /// Raw request for debugging (`spotitogether spotify raw`).
    /// `path` may contain a query string.
    pub async fn raw(&self, method: Method, path: &str, body: Option<&Value>) -> Result<(u16, String)> {
        let resp = self.request(method, path, &[], body).await?;
        let status = resp.status().as_u16();
        Ok((status, resp.text().await?))
    }

    // ------------------------------------------------------------ plumbing

    async fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let resp = self.request(Method::GET, path, query, None).await?;
        let body = resp.text().await?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Returns a valid access token, refreshing it when close to expiry
    /// (or unconditionally with `force`, after a 401).
    async fn access_token(&self, force: bool) -> Result<String> {
        let mut token = self.token.lock().await;
        if force || token.needs_refresh() {
            let fresh = auth::refresh(&self.http, &self.client_id, &token.refresh_token).await?;
            if let Some(sink) = &self.on_refresh {
                sink(&fresh);
            }
            *token = fresh;
        }
        Ok(token.access_token.clone())
    }

    /// Sends a request with automatic token refresh (once, on 401) and
    /// rate-limit handling (honours `Retry-After`, up to 30 s, 3 times).
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Response> {
        let mut refreshed = false;
        let mut rate_limited = 0;
        loop {
            let token = self.access_token(false).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{API}{path}"))
                .bearer_auth(&token)
                .query(query);
            req = match body {
                Some(b) => req.json(b),
                // Spotify answers 411 to body-less PUT/POST without Content-Length.
                None if method != Method::GET => req.header(CONTENT_LENGTH, "0"),
                None => req,
            };

            let resp = req.send().await?;
            match resp.status() {
                s if s.is_success() => return Ok(resp),
                StatusCode::UNAUTHORIZED if !refreshed => {
                    refreshed = true;
                    self.access_token(true).await?;
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let wait = Duration::from_secs(
                        resp.headers()
                            .get(RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(1),
                    );
                    if rate_limited >= MAX_RATE_LIMIT_RETRIES || wait > MAX_RETRY_WAIT {
                        return Err(Error::RateLimited(wait));
                    }
                    rate_limited += 1;
                    tracing::warn!(?wait, "rate limited by Spotify, retrying");
                    tokio::time::sleep(wait).await;
                }
                _ => return Err(api_error(resp).await),
            }
        }
    }
}

/// The track of a playlist / liked-songs entry, if it can be played.
fn playable_track(entry: Value) -> Option<Track> {
    match serde_json::from_value::<PlaylistEntry>(entry) {
        Ok(e) => e.item.filter(|t| t.uri.starts_with("spotify:track:") && t.duration_ms > 0),
        Err(e) => {
            tracing::warn!(%e, "skipping unreadable playlist entry");
            None
        }
    }
}

fn play_body(track_uri: &str, album_uri: Option<&str>, position_ms: u64) -> Value {
    match album_uri.filter(|a| a.starts_with("spotify:album:")) {
        Some(album) => json!({ "context_uri": album, "offset": { "uri": track_uri }, "position_ms": position_ms }),
        None => json!({ "uris": [track_uri], "position_ms": position_ms }),
    }
}

fn device_query(device_id: Option<&str>) -> Vec<(&'static str, String)> {
    device_id
        .map(|id| vec![("device_id", id.to_owned())])
        .unwrap_or_default()
}

async fn api_error(resp: Response) -> Error {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let (message, reason) = match serde_json::from_str::<ApiErrorBody>(&body) {
        Ok(b) => (b.error.message, b.error.reason),
        Err(_) => (body, None),
    };
    classify(status, message, reason)
}

fn classify(status: u16, message: String, reason: Option<String>) -> Error {
    let lower = message.to_lowercase();
    match reason.as_deref() {
        Some("PREMIUM_REQUIRED") => return Error::PremiumRequired,
        Some("NO_ACTIVE_DEVICE") => return Error::NoActiveDevice,
        _ => {}
    }
    if status == 403 && lower.contains("premium") {
        Error::PremiumRequired
    } else if status == 403 && lower.contains("scope") {
        Error::MissingScope
    } else if status == 404 && lower.contains("no active device") {
        Error::NoActiveDevice
    } else {
        Error::Api { status, message, reason }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_unplayable_or_unreadable_entries() {
        let ok = json!({"item": {"uri": "spotify:track:a", "name": "A", "duration_ms": 1}});
        assert_eq!(playable_track(ok).unwrap().uri, "spotify:track:a");
        assert!(playable_track(json!({"item": {"uri": "spotify:local:x", "name": "L", "duration_ms": 1}})).is_none());
        assert!(playable_track(json!({"item": {"uri": "spotify:track:b", "name": "B", "duration_ms": 0}})).is_none());
        assert!(playable_track(json!({"item": {"uri": "spotify:track:c", "duration_ms": "oops"}})).is_none());
        assert!(playable_track(json!({"item": null})).is_none());
    }

    #[test]
    fn play_body_prefers_album_context() {
        assert_eq!(
            play_body("spotify:track:t", Some("spotify:album:a"), 5),
            json!({"context_uri": "spotify:album:a", "offset": {"uri": "spotify:track:t"}, "position_ms": 5})
        );
        assert_eq!(play_body("spotify:track:t", None, 5), json!({"uris": ["spotify:track:t"], "position_ms": 5}));
        assert_eq!(play_body("spotify:track:t", Some(""), 0), json!({"uris": ["spotify:track:t"], "position_ms": 0}));
    }

    #[test]
    fn classifies_player_errors() {
        assert!(matches!(
            classify(403, "Player command failed: Premium required".into(), Some("PREMIUM_REQUIRED".into())),
            Error::PremiumRequired
        ));
        assert!(matches!(
            classify(404, "Player command failed: No active device found".into(), None),
            Error::NoActiveDevice
        ));
        assert!(matches!(classify(500, "boom".into(), None), Error::Api { status: 500, .. }));
    }

    #[test]
    fn parses_playback_with_episode_like_item() {
        let json = r#"{"is_playing":true,"progress_ms":1000,"device":{"id":"d","name":"PC","type":"Computer","is_active":true},
                      "item":{"uri":"spotify:episode:1","name":"Ep"}}"#;
        let p: PlaybackState = serde_json::from_str(json).unwrap();
        assert_eq!(p.item.unwrap().artist_names(), "");
    }
}

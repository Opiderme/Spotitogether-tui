//! Subset of the Spotify Web API objects used by the app.
//! Fields are lenient (`default`) because Spotify removes fields over time
//! and the player may report podcast episodes instead of tracks.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_restricted: bool,
    #[serde(default)]
    pub volume_percent: Option<u8>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Artist {
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Image {
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Album {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Track {
    #[serde(default)]
    pub id: Option<String>,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub album: Album,
}

impl Track {
    /// "Artist A, Artist B"
    pub fn artist_names(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Largest cover image, if any (used later for the ASCII cover).
    pub fn cover_url(&self) -> Option<&str> {
        self.album
            .images
            .iter()
            .max_by_key(|i| i.width.unwrap_or(0))
            .map(|i| i.url.as_str())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlaybackState {
    #[serde(default)]
    pub device: Option<Device>,
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub progress_ms: Option<u64>,
    #[serde(default)]
    pub item: Option<Track>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DevicesResponse {
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Paging<T> {
    pub items: Vec<T>,
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub offset: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SearchResponse {
    pub tracks: Paging<Track>,
}

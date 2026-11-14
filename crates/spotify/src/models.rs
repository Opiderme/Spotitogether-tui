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
    pub uri: String,
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

/// First controllable device, preferring a computer over phones/speakers.
/// Used when no device is active: Spotify would otherwise answer
/// NO_ACTIVE_DEVICE.
pub fn pick_idle_device(devices: &[Device]) -> Option<&Device> {
    let usable = || devices.iter().filter(|d| d.id.is_some() && !d.is_restricted);
    usable().find(|d| d.kind == "Computer").or_else(|| usable().next())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, kind: &str, restricted: bool) -> Device {
        Device {
            id: Some(name.to_owned()),
            name: name.to_owned(),
            kind: kind.to_owned(),
            is_active: false,
            is_restricted: restricted,
            volume_percent: None,
        }
    }

    #[test]
    fn picks_computer_then_any_controllable_device() {
        let devices = [dev("tel", "Smartphone", false), dev("pc", "Computer", false)];
        assert_eq!(pick_idle_device(&devices).unwrap().name, "pc");
        let devices = [dev("enceinte", "Speaker", true), dev("tel", "Smartphone", false)];
        assert_eq!(pick_idle_device(&devices).unwrap().name, "tel");
        assert!(pick_idle_device(&[dev("x", "Speaker", true)]).is_none());
    }
}

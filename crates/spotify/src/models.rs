//! Subset of the Spotify Web API objects used by the app.
//! Fields are lenient (`default`) because Spotify removes fields over time
//! and the player may report podcast episodes instead of tracks.

use serde::{Deserialize, Deserializer, Serialize};

/// `default` only covers a missing field; Spotify also sends `null`
/// (local files have `"uri": null` in their album, some images a null URL).
fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
    #[serde(default = "yes")]
    pub supports_volume: bool,
}

fn nullable_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(d)?.unwrap_or_default())
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Artist {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Image {
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Album {
    #[serde(default, deserialize_with = "nullable")]
    pub uri: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub images: Vec<Image>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Track {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub uri: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub duration_ms: u64,
    #[serde(default, deserialize_with = "nullable")]
    pub artists: Vec<Artist>,
    #[serde(default, deserialize_with = "nullable")]
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
            .filter(|i| !i.url.is_empty())
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
    #[serde(default = "Vec::new", deserialize_with = "nullable_vec", bound(deserialize = "T: Deserialize<'de>"))]
    pub items: Vec<T>,
    #[serde(default, deserialize_with = "nullable")]
    pub total: u32,
    #[serde(default, deserialize_with = "nullable")]
    pub offset: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemCount {
    #[serde(default)]
    pub total: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlaylistSummary {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// `items` since February 2026, `tracks` before.
    #[serde(default, alias = "tracks", deserialize_with = "nullable")]
    pub items: ItemCount,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlaylistEntry {
    /// `item` since February 2026, `track` before; null for unavailable
    /// tracks. Episodes parse too and are filtered out by URI.
    #[serde(default, alias = "track")]
    pub item: Option<Track>,
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
            supports_volume: true,
        }
    }

    #[test]
    fn parses_playlists_in_both_formats() {
        let new: Paging<PlaylistSummary> =
            serde_json::from_str(r#"{"items":[{"id":"p","name":"Soirée","items":{"total":42}}],"total":1}"#).unwrap();
        assert_eq!(new.items[0].items.total, 42);
        let old: PlaylistSummary = serde_json::from_str(r#"{"id":"p","name":"S","tracks":{"total":7}}"#).unwrap();
        assert_eq!(old.items.total, 7);

        let entries: Paging<PlaylistEntry> = serde_json::from_str(
            r#"{"items":[{"item":{"uri":"spotify:track:a","name":"A","duration_ms":1}},
                         {"track":{"uri":"spotify:track:b","name":"B","duration_ms":1}},
                         {"item":null}],"total":3}"#,
        )
        .unwrap();
        let uris: Vec<_> = entries.items.iter().filter_map(|e| e.item.as_ref().map(|t| t.uri.as_str())).collect();
        assert_eq!(uris, ["spotify:track:a", "spotify:track:b"]);
    }

    #[test]
    fn tolerates_nulls_from_local_files_and_odd_images() {
        // A local file in a playlist: album with null URI, null name, no id.
        let local = r#"{"item":{"id":null,"uri":"spotify:local:A:B:C:180","name":"Mon mp3","duration_ms":180000,
            "artists":[{"name":null}],"album":{"uri":null,"name":null,"images":null}}}"#;
        let e: PlaylistEntry = serde_json::from_str(local).unwrap();
        assert_eq!(e.item.unwrap().album.uri, "");
        // Null image URLs are skipped when picking a cover.
        let t: Track = serde_json::from_str(
            r#"{"uri":"spotify:track:a","name":"A","album":{"images":[{"url":null,"width":640},{"url":"https://i/x","width":300}]}}"#,
        )
        .unwrap();
        assert_eq!(t.cover_url(), Some("https://i/x"));
        let p: Paging<PlaylistEntry> = serde_json::from_str(r#"{"items":null,"total":null,"offset":null}"#).unwrap();
        assert!(p.items.is_empty());
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

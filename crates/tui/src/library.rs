//! The member's Spotify library (`l`): liked songs and playlists, to queue
//! tracks one by one or play a whole list, with a local filter (`/`).

use ratatui::widgets::{ListState, TableState};
use spotify::models::{PlaylistSummary, Track};

/// Tracks loaded per list: the room queue holds 500 at most.
pub const MAX_TRACKS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Liked,
    Playlist { id: String },
}

impl Source {
    /// Identifies a request, to drop answers that arrive too late.
    pub fn key(&self) -> &str {
        match self {
            Source::Liked => "liked",
            Source::Playlist { id } => id,
        }
    }
}

/// Filter typed after `/`: case and accent insensitive, every word must
/// appear.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub text: String,
    pub editing: bool,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        !self.text.trim().is_empty()
    }

    fn words(&self) -> Vec<String> {
        fold(&self.text).split_whitespace().map(str::to_owned).collect()
    }

    pub fn matches(&self, haystack: &str) -> bool {
        let haystack = fold(haystack);
        self.words().iter().all(|w| haystack.contains(w.as_str()))
    }
}

/// Lowercase without diacritics: "Télépopmusik" -> "telepopmusik".
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            'œ' => 'o',
            other => other,
        })
        .collect()
}

/// An opened list (second level).
#[derive(Debug, Default)]
pub struct OpenList {
    pub name: String,
    pub key: String,
    pub tracks: Vec<Track>,
    pub total: Option<u32>,
    pub loading: bool,
    /// Selection among the visible (filtered) tracks.
    pub state: TableState,
    pub filter: Filter,
}

impl OpenList {
    /// Indices of the tracks matching the filter.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.tracks.len())
            .filter(|&i| {
                let t = &self.tracks[i];
                !self.filter.is_active()
                    || self.filter.matches(&format!("{} {} {}", t.name, t.artist_names(), t.album.name))
            })
            .collect()
    }

    pub fn selected_track(&self) -> Option<&Track> {
        let row = self.state.selected()?;
        self.visible().get(row).map(|&i| &self.tracks[i])
    }

    /// Tracks a whole-list action applies to: the filtered ones.
    pub fn visible_tracks(&self) -> Vec<&Track> {
        self.visible().into_iter().map(|i| &self.tracks[i]).collect()
    }
}

#[derive(Debug, Default)]
pub struct Library {
    pub playlists: Vec<PlaylistSummary>,
    pub loading: bool,
    /// Selection among the visible rows (row 0 of all rows is "liked songs").
    pub state: ListState,
    pub open: Option<OpenList>,
    pub filter: Filter,
}

pub const LIKED_NAME: &str = "Titres likés";

impl Library {
    pub fn rows(&self) -> usize {
        1 + self.playlists.len()
    }

    /// Source, display name and size of a row (0 = liked songs).
    pub fn row(&self, i: usize) -> Option<(Source, String, Option<u32>)> {
        match i {
            0 => Some((Source::Liked, LIKED_NAME.into(), None)),
            _ => self
                .playlists
                .get(i - 1)
                .map(|p| (Source::Playlist { id: p.id.clone() }, p.name.clone(), Some(p.items.total))),
        }
    }

    /// Rows matching the filter (indices for [`Library::row`]).
    pub fn visible(&self) -> Vec<usize> {
        (0..self.rows())
            .filter(|&i| !self.filter.is_active() || self.row(i).is_some_and(|(_, name, _)| self.filter.matches(&name)))
            .collect()
    }

    pub fn selected_row(&self) -> Option<(Source, String, Option<u32>)> {
        let row = self.state.selected()?;
        self.visible().get(row).and_then(|&i| self.row(i))
    }
}

/// Moves a selection by `delta` rows, clamped.
pub fn step(selected: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.unwrap_or(0).saturating_add_signed(delta).min(len - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use spotify::models::{Album, Artist};

    fn playlist(id: &str, name: &str) -> PlaylistSummary {
        serde_json::from_value(serde_json::json!({"id": id, "name": name, "items": {"total": 4}})).unwrap()
    }

    fn track(name: &str, artist: &str, album: &str) -> Track {
        Track {
            uri: format!("spotify:track:{name}"),
            name: name.into(),
            duration_ms: 1,
            artists: vec![Artist { name: artist.into() }],
            album: Album { name: album.into(), ..Default::default() },
            ..Default::default()
        }
    }

    #[test]
    fn rows_start_with_liked_songs() {
        let mut lib = Library::default();
        lib.playlists.push(playlist("p", "Soirée"));
        assert_eq!(lib.rows(), 2);
        assert_eq!(lib.row(0).unwrap().0, Source::Liked);
        assert_eq!(lib.row(1).unwrap(), (Source::Playlist { id: "p".into() }, "Soirée".into(), Some(4)));
        assert!(lib.row(2).is_none());
        assert_eq!(step(Some(0), 3, -1), Some(0));
        assert_eq!(step(Some(1), 3, 10), Some(2));
        assert_eq!(step(None, 0, 1), None);
    }

    #[test]
    fn filters_playlists_ignoring_case_and_accents() {
        let mut lib = Library {
            playlists: vec![playlist("a", "Soirée d'été"), playlist("b", "Klassique"), playlist("c", "SKI 2026")],
            ..Default::default()
        };
        lib.filter.text = "soiree".into();
        assert_eq!(lib.visible(), [1]);
        lib.state.select(Some(0));
        assert_eq!(lib.selected_row().unwrap().1, "Soirée d'été");
        lib.filter.text = "lik".into(); // "Titres likés"
        assert_eq!(lib.visible(), [0]);
        lib.filter.text.clear();
        assert_eq!(lib.visible().len(), 4);
    }

    #[test]
    fn filters_tracks_on_title_artist_and_album_with_all_words() {
        let mut open = OpenList {
            tracks: vec![
                track("One More Time", "Daft Punk", "Discovery"),
                track("Breathe", "Télépopmusik", "Genetic World"),
                track("Get Lucky", "Daft Punk", "Random Access Memories"),
            ],
            ..Default::default()
        };
        open.filter.text = "daft discovery".into();
        assert_eq!(open.visible(), [0]);
        open.filter.text = "TELEPOP".into();
        assert_eq!(open.visible(), [1]);
        open.filter.text = "daft".into();
        open.state.select(Some(1));
        assert_eq!(open.selected_track().unwrap().name, "Get Lucky");
        assert_eq!(open.visible_tracks().len(), 2);
    }
}

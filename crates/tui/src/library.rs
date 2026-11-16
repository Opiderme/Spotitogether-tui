//! The member's Spotify library (`l`): liked songs and playlists, to queue
//! tracks one by one or play a whole list.

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

/// An opened list (second level).
#[derive(Debug, Default)]
pub struct OpenList {
    pub name: String,
    pub key: String,
    pub tracks: Vec<Track>,
    pub total: Option<u32>,
    pub loading: bool,
    pub state: TableState,
}

#[derive(Debug, Default)]
pub struct Library {
    pub playlists: Vec<PlaylistSummary>,
    pub loading: bool,
    /// Row 0 is "liked songs", then the playlists.
    pub state: ListState,
    pub open: Option<OpenList>,
}

impl Library {
    pub fn rows(&self) -> usize {
        1 + self.playlists.len()
    }

    /// Source and display name of a first-level row.
    pub fn row(&self, i: usize) -> Option<(Source, String, Option<u32>)> {
        match i {
            0 => Some((Source::Liked, "Titres likés".into(), None)),
            _ => self
                .playlists
                .get(i - 1)
                .map(|p| (Source::Playlist { id: p.id.clone() }, p.name.clone(), Some(p.items.total))),
        }
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

    #[test]
    fn rows_start_with_liked_songs() {
        let mut lib = Library::default();
        lib.playlists.push(serde_json::from_value(serde_json::json!({"id": "p", "name": "Soirée", "items": {"total": 4}})).unwrap());
        assert_eq!(lib.rows(), 2);
        assert_eq!(lib.row(0).unwrap().0, Source::Liked);
        assert_eq!(lib.row(1).unwrap(), (Source::Playlist { id: "p".into() }, "Soirée".into(), Some(4)));
        assert!(lib.row(2).is_none());
        assert_eq!(step(Some(0), 3, -1), Some(0));
        assert_eq!(step(Some(1), 3, 10), Some(2));
        assert_eq!(step(None, 0, 1), None);
    }
}

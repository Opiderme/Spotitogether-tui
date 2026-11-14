//! Messages exchanged between the room server and clients over WebSocket.
//!
//! Every frame is a JSON text message `{"type": "...", "data": ...}`.
//! The server is the single source of truth; clients only mirror its state
//! on their own Spotify account.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; the server rejects other versions.
pub const PROTOCOL_VERSION: u32 = 1;

/// Path of the WebSocket endpoint on the server.
pub const WS_PATH: &str = "/ws";

/// Default TCP port of `spotitogether serve`.
pub const DEFAULT_PORT: u16 = 7420;

pub type MemberId = u64;
pub type ItemId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Must be the first message. `host_token` (printed by `serve`) grants the host role.
    Join {
        version: u32,
        code: String,
        name: String,
        #[serde(default)]
        host_token: Option<String>,
    },
    /// Clock sync: `t0` is the client's wall clock in ms, echoed back.
    Ping { t0: u64 },
    QueueAdd { track: TrackInfo },
    QueueRemove { id: ItemId },
    /// Moves an item to `index` (0 = next to play).
    QueueMove { id: ItemId, index: usize },
    Play,
    Pause,
    Seek { position_ms: u64 },
    Skip,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { member_id: MemberId, room: RoomSnapshot },
    Pong { t0: u64, server_ms: u64 },
    Playback(Playback),
    Queue { items: Vec<QueueItem> },
    Members { members: Vec<Member> },
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadVersion,
    BadCode,
    TooManyAttempts,
    NotJoined,
    InvalidMessage,
    NotFound,
    Forbidden,
}

/// Track metadata sent by the client that queues it (batch track lookup is
/// not available in Spotify Development Mode, so the server never fetches it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub uri: String,
    pub name: String,
    pub artists: String,
    pub duration_ms: u64,
    /// Album of the track: playback is started in this context (see
    /// `SpotifyClient::play_track`).
    #[serde(default)]
    pub album_uri: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: ItemId,
    pub track: TrackInfo,
    pub added_by: String,
}

/// Playback state anchored on the server clock: at `server_ms` the track was
/// at `position_ms`. `seq` increases on every change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    pub seq: u64,
    pub current: Option<QueueItem>,
    pub position_ms: u64,
    pub server_ms: u64,
    pub is_playing: bool,
}

impl Playback {
    pub fn stopped(seq: u64, server_ms: u64) -> Self {
        Self { seq, current: None, position_ms: 0, server_ms, is_playing: false }
    }

    /// Expected position at server time `now_ms`, clamped to the track length.
    pub fn position_at(&self, now_ms: u64) -> u64 {
        let elapsed = if self.is_playing { now_ms.saturating_sub(self.server_ms) } else { 0 };
        let pos = self.position_ms + elapsed;
        match &self.current {
            Some(item) => pos.min(item.track.duration_ms),
            None => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Listener,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: MemberId,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    pub code: String,
    pub playback: Playback,
    pub queue: Vec<QueueItem>,
    pub members: Vec<Member>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(duration_ms: u64) -> QueueItem {
        QueueItem {
            id: 1,
            track: TrackInfo {
                uri: "spotify:track:x".into(),
                name: "X".into(),
                artists: "Y".into(),
                duration_ms,
                album_uri: None,
                cover_url: None,
            },
            added_by: "a".into(),
        }
    }

    #[test]
    fn wire_format_is_tagged() {
        let json = serde_json::to_string(&ClientMsg::Seek { position_ms: 5 }).unwrap();
        assert_eq!(json, r#"{"type":"seek","data":{"position_ms":5}}"#);
        let json = serde_json::to_string(&ClientMsg::Skip).unwrap();
        assert_eq!(json, r#"{"type":"skip"}"#);
        let back: ClientMsg = serde_json::from_str(r#"{"type":"join","data":{"version":1,"code":"ABC","name":"n"}}"#).unwrap();
        assert!(matches!(back, ClientMsg::Join { host_token: None, .. }));
    }

    #[test]
    fn position_advances_only_while_playing() {
        let mut p = Playback { seq: 1, current: Some(item(10_000)), position_ms: 1_000, server_ms: 100, is_playing: true };
        assert_eq!(p.position_at(600), 1_500);
        assert_eq!(p.position_at(1_000_000), 10_000);
        p.is_playing = false;
        assert_eq!(p.position_at(600), 1_000);
    }
}

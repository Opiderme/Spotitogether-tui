//! Messages exchanged between the room server and clients over WebSocket.
//!
//! Every frame is a JSON text message `{"type": "...", "data": ...}`.
//! The server is the single source of truth; clients only mirror its state
//! on their own Spotify account.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; the server rejects other versions.
pub const PROTOCOL_VERSION: u32 = 2;

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
        /// Spotify user id, used to remember roles and bans across sessions.
        /// Self-declared by the client: good enough among friends, not proof.
        #[serde(default)]
        spotify_id: Option<String>,
        /// Session token from a previous `Welcome`: rejoining within the
        /// grace period gives back the same member (role, own tracks).
        #[serde(default)]
        session: Option<String>,
    },
    /// Clock sync: `t0` is the client's wall clock in ms, echoed back.
    Ping { t0: u64 },
    QueueAdd { track: TrackInfo },
    /// Several tracks at once (a playlist), in the given order. Needs
    /// `QueueManage`; `PlayNow` also needs `Playback`.
    QueueAddMany { tracks: Vec<TrackInfo>, mode: AddMode },
    QueueRemove { id: ItemId },
    /// Moves an item to `index` (0 = next to play).
    QueueMove { id: ItemId, index: usize },
    Play,
    Pause,
    Seek { position_ms: u64 },
    /// Immediate skip (needs `Perm::Skip`).
    Skip,
    /// Vote to skip the current track; it is skipped once more than half of
    /// the listeners voted.
    VoteSkip,
    /// Host only.
    Admin(AdminCmd),
}

/// Where `QueueAddMany` puts the tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddMode {
    /// After the tracks already queued.
    End,
    /// Replaces the queue and starts the first track right away.
    PlayNow,
}

/// Room administration, reserved to the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AdminCmd {
    SetRole { member: MemberId, role: Role },
    /// `value: None` removes the override (back to the role default).
    SetPerm { member: MemberId, perm: Perm, value: Option<bool> },
    Kick { member: MemberId },
    Ban { member: MemberId },
    /// Lets a member out of the waiting room.
    Admit { member: MemberId },
    SetWaitingRoom { enabled: bool },
    SetDefaultRole { role: Role },
    /// Generates a new invite code (members already inside stay).
    RotateCode,
    /// Tracks played at random when the queue is empty (read from a playlist
    /// by the host's client). An empty list turns autoplay off.
    SetAutoplay { name: String, tracks: Vec<TrackInfo> },
}

/// Maximum number of autoplay tracks kept by the server.
pub const MAX_AUTOPLAY_TRACKS: usize = 500;

/// `added_by` of tracks picked by autoplay.
pub const AUTOPLAY_NAME: &str = "Autoplay";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { member_id: MemberId, session: String, room: RoomSnapshot },
    Pong { t0: u64, server_ms: u64 },
    Playback(Playback),
    Queue { items: Vec<QueueItem> },
    Members { members: Vec<Member> },
    Settings(RoomSettings),
    SkipVotes { votes: u32, needed: u32 },
    /// Sent instead of `Welcome` while the host has not admitted the member.
    Waiting,
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
    Kicked,
    Banned,
    Refused,
    /// The host closed the room (sent just before the connection closes).
    RoomClosed,
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
    /// Album name (helps finding the lyrics).
    #[serde(default)]
    pub album_name: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: ItemId,
    pub track: TrackInfo,
    pub added_by: String,
    #[serde(default)]
    pub added_by_id: MemberId,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Dj,
    Listener,
    Spectator,
}

impl Role {
    /// Roles the host can hand out (the host role comes from the host token).
    pub const ASSIGNABLE: [Role; 3] = [Role::Dj, Role::Listener, Role::Spectator];

    pub fn label(self) -> &'static str {
        match self {
            Role::Host => "Hôte",
            Role::Dj => "DJ",
            Role::Listener => "Auditeur",
            Role::Spectator => "Spectateur",
        }
    }

    /// Default permissions of the role (see the plan's matrix).
    pub fn defaults(self) -> Permissions {
        let all = Permissions { listen: true, queue_add: true, queue_manage: true, playback: true, skip: true };
        match self {
            Role::Host | Role::Dj => all,
            Role::Listener => Permissions { listen: true, queue_add: true, ..Permissions::NONE },
            Role::Spectator => Permissions::NONE,
        }
    }

    pub fn next_assignable(self) -> Role {
        match self {
            Role::Dj => Role::Listener,
            Role::Listener => Role::Spectator,
            Role::Spectator | Role::Host => Role::Dj,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Perm {
    /// Hear the room on one's own Spotify.
    Listen,
    /// Add tracks (limited to a few pending tracks without `QueueManage`).
    QueueAdd,
    /// Remove and reorder anyone's tracks (own tracks can always be removed).
    QueueManage,
    /// Play, pause, seek.
    Playback,
    /// Skip directly (otherwise only vote).
    Skip,
}

impl Perm {
    pub const ALL: [Perm; 5] = [Perm::Listen, Perm::QueueAdd, Perm::QueueManage, Perm::Playback, Perm::Skip];

    pub fn label(self) -> &'static str {
        match self {
            Perm::Listen => "Écoute",
            Perm::QueueAdd => "Ajout",
            Perm::QueueManage => "File",
            Perm::Playback => "Lecture",
            Perm::Skip => "Skip",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub listen: bool,
    pub queue_add: bool,
    pub queue_manage: bool,
    pub playback: bool,
    pub skip: bool,
}

impl Permissions {
    pub const NONE: Permissions =
        Permissions { listen: false, queue_add: false, queue_manage: false, playback: false, skip: false };

    pub fn get(&self, perm: Perm) -> bool {
        match perm {
            Perm::Listen => self.listen,
            Perm::QueueAdd => self.queue_add,
            Perm::QueueManage => self.queue_manage,
            Perm::Playback => self.playback,
            Perm::Skip => self.skip,
        }
    }

    pub fn set(&mut self, perm: Perm, value: bool) {
        let slot = match perm {
            Perm::Listen => &mut self.listen,
            Perm::QueueAdd => &mut self.queue_add,
            Perm::QueueManage => &mut self.queue_manage,
            Perm::Playback => &mut self.playback,
            Perm::Skip => &mut self.skip,
        };
        *slot = value;
    }

    /// Role defaults with per-member overrides applied.
    pub fn effective(role: Role, overrides: &Overrides) -> Self {
        let mut p = role.defaults();
        if role != Role::Host {
            for (perm, value) in overrides {
                p.set(*perm, *value);
            }
        }
        p
    }
}

/// Per-member exceptions to the role defaults.
pub type Overrides = BTreeMap<Perm, bool>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: MemberId,
    pub name: String,
    pub role: Role,
    /// Effective permissions (role defaults + overrides).
    pub perms: Permissions,
    #[serde(default)]
    pub overrides: Overrides,
    /// In the waiting room, not admitted yet (only the host sees them).
    #[serde(default)]
    pub pending: bool,
    /// False while the member's connection is lost (their place is kept for
    /// a short grace period).
    #[serde(default = "online_default")]
    pub online: bool,
}

fn online_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomSettings {
    pub code: String,
    pub waiting_room: bool,
    pub default_role: Role,
    /// Name of the autoplay playlist, if any.
    #[serde(default)]
    pub autoplay: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    pub settings: RoomSettings,
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
                album_uri: None, album_name: None,
                cover_url: None,
            },
            added_by: "a".into(),
            added_by_id: 1,
        }
    }

    #[test]
    fn wire_format_is_tagged() {
        let json = serde_json::to_string(&ClientMsg::Seek { position_ms: 5 }).unwrap();
        assert_eq!(json, r#"{"type":"seek","data":{"position_ms":5}}"#);
        let json = serde_json::to_string(&ClientMsg::Skip).unwrap();
        assert_eq!(json, r#"{"type":"skip"}"#);
        let back: ClientMsg = serde_json::from_str(r#"{"type":"join","data":{"version":1,"code":"ABC","name":"n"}}"#).unwrap();
        assert!(matches!(back, ClientMsg::Join { host_token: None, spotify_id: None, session: None, .. }));
        let json = serde_json::to_string(&ClientMsg::Admin(AdminCmd::SetPerm { member: 2, perm: Perm::Skip, value: Some(true) })).unwrap();
        assert_eq!(json, r#"{"type":"admin","data":{"action":"set_perm","member":2,"perm":"skip","value":true}}"#);
    }

    #[test]
    fn overrides_apply_except_for_host() {
        let mut o = Overrides::new();
        o.insert(Perm::QueueAdd, false);
        o.insert(Perm::Skip, true);
        let p = Permissions::effective(Role::Listener, &o);
        assert!(p.listen && !p.queue_add && p.skip && !p.playback);
        assert_eq!(Permissions::effective(Role::Host, &o), Role::Host.defaults());
        assert_eq!(Role::Spectator.defaults(), Permissions::NONE);
        // Overrides survive a JSON round trip with enum keys.
        let back: Overrides = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(back, o);
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

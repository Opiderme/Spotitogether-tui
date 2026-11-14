//! Room state machine. Pure logic: the caller passes the current server time
//! in ms and dispatches the returned messages, which keeps it easy to test.

use std::collections::{BTreeMap, VecDeque};

use protocol::{
    ClientMsg, ErrorCode, ItemId, Member, MemberId, Playback, QueueItem, Role, RoomSnapshot, ServerMsg,
};

const MAX_NAME_LEN: usize = 32;
const MAX_QUEUE_LEN: usize = 500;

/// A message to deliver: to every member, or to one.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    All(ServerMsg),
    One(MemberId, ServerMsg),
}

pub struct Room {
    code: String,
    host_token: String,
    members: BTreeMap<MemberId, Member>,
    queue: VecDeque<QueueItem>,
    playback: Playback,
    next_member: MemberId,
    next_item: ItemId,
}

impl Room {
    pub fn new(code: impl Into<String>, host_token: impl Into<String>, now_ms: u64) -> Self {
        Self {
            code: code.into(),
            host_token: host_token.into(),
            members: BTreeMap::new(),
            queue: VecDeque::new(),
            playback: Playback::stopped(0, now_ms),
            next_member: 1,
            next_item: 1,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// Case-insensitive invite code check.
    pub fn code_matches(&self, code: &str) -> bool {
        self.code.eq_ignore_ascii_case(code.trim())
    }

    pub fn snapshot(&self) -> RoomSnapshot {
        RoomSnapshot {
            code: self.code.clone(),
            playback: self.playback.clone(),
            queue: self.queue.iter().cloned().collect(),
            members: self.members.values().cloned().collect(),
        }
    }

    pub fn join(&mut self, name: &str, host_token: Option<&str>) -> (MemberId, Vec<Out>) {
        let id = self.next_member;
        self.next_member += 1;
        let role = if host_token == Some(self.host_token.as_str()) { Role::Host } else { Role::Listener };
        let name = self.unique_name(name);
        self.members.insert(id, Member { id, name, role });
        (id, vec![self.members_msg()])
    }

    pub fn leave(&mut self, id: MemberId) -> Vec<Out> {
        if self.members.remove(&id).is_some() { vec![self.members_msg()] } else { vec![] }
    }

    pub fn member_name(&self, id: MemberId) -> Option<&str> {
        self.members.get(&id).map(|m| m.name.as_str())
    }

    /// Server time (ms) at which the current track ends, if it is playing.
    pub fn deadline(&self) -> Option<u64> {
        let p = &self.playback;
        let item = p.current.as_ref().filter(|_| p.is_playing)?;
        Some(p.server_ms + item.track.duration_ms.saturating_sub(p.position_ms))
    }

    /// Advances to the next track once the current one has finished.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Out> {
        match self.deadline() {
            Some(end) if now_ms >= end => self.advance(now_ms),
            _ => vec![],
        }
    }

    pub fn handle(&mut self, from: MemberId, msg: ClientMsg, now_ms: u64) -> Vec<Out> {
        let Some(added_by) = self.member_name(from).map(str::to_owned) else {
            return vec![error(from, ErrorCode::NotJoined, "membre inconnu")];
        };
        match msg {
            ClientMsg::QueueAdd { track } => {
                if !track.uri.starts_with("spotify:track:") || track.duration_ms == 0 {
                    return vec![error(from, ErrorCode::InvalidMessage, "morceau invalide")];
                }
                if self.queue.len() >= MAX_QUEUE_LEN {
                    return vec![error(from, ErrorCode::Forbidden, "file d'attente pleine")];
                }
                let id = self.next_item;
                self.next_item += 1;
                self.queue.push_back(QueueItem { id, track, added_by });
                if self.playback.current.is_none() { self.advance(now_ms) } else { vec![self.queue_msg()] }
            }
            ClientMsg::QueueRemove { id } => match self.queue.iter().position(|i| i.id == id) {
                Some(index) => {
                    self.queue.remove(index);
                    vec![self.queue_msg()]
                }
                None => vec![error(from, ErrorCode::NotFound, "élément absent de la file")],
            },
            ClientMsg::QueueMove { id, index } => match self.queue.iter().position(|i| i.id == id) {
                Some(from) => {
                    let item = self.queue.remove(from).expect("index from position");
                    let to = index.min(self.queue.len());
                    self.queue.insert(to, item);
                    if from == to { vec![] } else { vec![self.queue_msg()] }
                }
                None => vec![error(from, ErrorCode::NotFound, "élément absent de la file")],
            },
            ClientMsg::Play => {
                if self.playback.current.is_none() {
                    return if self.queue.is_empty() { vec![] } else { self.advance(now_ms) };
                }
                if self.playback.is_playing {
                    return vec![];
                }
                self.update_playback(now_ms, |p| p.is_playing = true)
            }
            ClientMsg::Pause => {
                if !self.playback.is_playing {
                    return vec![];
                }
                let pos = self.playback.position_at(now_ms);
                self.update_playback(now_ms, |p| {
                    p.position_ms = pos;
                    p.is_playing = false;
                })
            }
            ClientMsg::Seek { position_ms } => {
                let Some(duration) = self.playback.current.as_ref().map(|i| i.track.duration_ms) else {
                    return vec![];
                };
                self.update_playback(now_ms, |p| p.position_ms = position_ms.min(duration))
            }
            ClientMsg::Skip => self.advance(now_ms),
            ClientMsg::Join { .. } | ClientMsg::Ping { .. } => {
                vec![error(from, ErrorCode::InvalidMessage, "message inattendu")]
            }
        }
    }

    /// Re-anchors the playback on `now_ms`, applies `change` and broadcasts it.
    fn update_playback(&mut self, now_ms: u64, change: impl FnOnce(&mut Playback)) -> Vec<Out> {
        let p = &mut self.playback;
        p.position_ms = p.position_at(now_ms);
        p.server_ms = now_ms;
        change(p);
        p.seq += 1;
        vec![Out::All(ServerMsg::Playback(self.playback.clone()))]
    }

    fn advance(&mut self, now_ms: u64) -> Vec<Out> {
        let next = self.queue.pop_front();
        let seq = self.playback.seq + 1;
        self.playback = Playback {
            seq,
            is_playing: next.is_some(),
            current: next,
            position_ms: 0,
            server_ms: now_ms,
        };
        vec![Out::All(ServerMsg::Playback(self.playback.clone())), self.queue_msg()]
    }

    fn queue_msg(&self) -> Out {
        Out::All(ServerMsg::Queue { items: self.queue.iter().cloned().collect() })
    }

    fn members_msg(&self) -> Out {
        Out::All(ServerMsg::Members { members: self.members.values().cloned().collect() })
    }

    fn unique_name(&self, wanted: &str) -> String {
        let base: String = wanted.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_LEN).collect();
        let base = if base.is_empty() { "Invité".to_owned() } else { base };
        let taken = |n: &str| self.members.values().any(|m| m.name == n);
        if !taken(&base) {
            return base;
        }
        (2..).map(|i| format!("{base} ({i})")).find(|n| !taken(n)).expect("infinite range")
    }
}

fn error(to: MemberId, code: ErrorCode, message: &str) -> Out {
    Out::One(to, ServerMsg::Error { code, message: message.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TrackInfo;

    fn track(n: u8, duration_ms: u64) -> TrackInfo {
        TrackInfo {
            uri: format!("spotify:track:{n}"),
            name: format!("T{n}"),
            artists: "A".into(),
            duration_ms,
            album_uri: None,
                cover_url: None,
        }
    }

    fn room() -> (Room, MemberId) {
        let mut r = Room::new("K7XQ2M", "secret", 0);
        let (id, _) = r.join("Alex", Some("secret"));
        (r, id)
    }

    fn playback(outs: &[Out]) -> Option<&Playback> {
        outs.iter().find_map(|o| match o {
            Out::All(ServerMsg::Playback(p)) => Some(p),
            _ => None,
        })
    }

    #[test]
    fn roles_and_unique_names() {
        let (mut r, host) = room();
        let (guest, _) = r.join("Alex", Some("wrong"));
        let s = r.snapshot();
        assert_eq!(s.members.iter().find(|m| m.id == host).unwrap().role, Role::Host);
        let g = s.members.iter().find(|m| m.id == guest).unwrap();
        assert_eq!((g.role, g.name.as_str()), (Role::Listener, "Alex (2)"));
        assert!(r.code_matches(" k7xq2m "));
    }

    #[test]
    fn first_track_starts_immediately_then_queues() {
        let (mut r, a) = room();
        let outs = r.handle(a, ClientMsg::QueueAdd { track: track(1, 1000) }, 50);
        let p = playback(&outs).unwrap();
        assert!(p.is_playing);
        assert_eq!(p.current.as_ref().unwrap().track.name, "T1");
        assert_eq!(p.current.as_ref().unwrap().added_by, "Alex");

        let outs = r.handle(a, ClientMsg::QueueAdd { track: track(2, 1000) }, 60);
        assert!(playback(&outs).is_none());
        assert_eq!(r.snapshot().queue.len(), 1);
        assert_eq!(r.deadline(), Some(1050));
    }

    #[test]
    fn track_end_advances_then_stops() {
        let (mut r, a) = room();
        r.handle(a, ClientMsg::QueueAdd { track: track(1, 1000) }, 0);
        r.handle(a, ClientMsg::QueueAdd { track: track(2, 500) }, 0);
        assert!(r.tick(999).is_empty());
        let p = playback(&r.tick(1000)).unwrap().clone();
        assert_eq!(p.current.unwrap().track.name, "T2");
        assert_eq!(p.server_ms, 1000);
        let p = playback(&r.tick(1500)).unwrap().clone();
        assert!(p.current.is_none() && !p.is_playing);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn pause_play_seek_keep_position_consistent() {
        let (mut r, a) = room();
        r.handle(a, ClientMsg::QueueAdd { track: track(1, 10_000) }, 0);
        let p = playback(&r.handle(a, ClientMsg::Pause, 3_000)).unwrap().clone();
        assert_eq!((p.position_ms, p.is_playing), (3_000, false));
        assert_eq!(r.deadline(), None);

        let p = playback(&r.handle(a, ClientMsg::Play, 8_000)).unwrap().clone();
        assert_eq!((p.position_ms, p.server_ms, p.is_playing), (3_000, 8_000, true));
        assert_eq!(r.deadline(), Some(15_000));

        let p = playback(&r.handle(a, ClientMsg::Seek { position_ms: 99_999 }, 9_000)).unwrap().clone();
        assert_eq!(p.position_ms, 10_000);
        assert!(p.seq > 3);
    }

    #[test]
    fn moves_queue_items() {
        let (mut r, a) = room();
        for n in 1..=4 {
            r.handle(a, ClientMsg::QueueAdd { track: track(n, 1000) }, 0);
        }
        // T1 is playing; queue = T2 T3 T4 (ids 2, 3, 4).
        let names = |r: &Room| r.snapshot().queue.iter().map(|i| i.track.name.clone()).collect::<Vec<_>>();
        r.handle(a, ClientMsg::QueueMove { id: 4, index: 0 }, 0);
        assert_eq!(names(&r), ["T4", "T2", "T3"]);
        r.handle(a, ClientMsg::QueueMove { id: 4, index: 99 }, 0);
        assert_eq!(names(&r), ["T2", "T3", "T4"]);
        assert!(r.handle(a, ClientMsg::QueueMove { id: 3, index: 1 }, 0).is_empty());
    }

    #[test]
    fn rejects_bad_tracks_and_unknown_items() {
        let (mut r, a) = room();
        let mut bad = track(1, 1000);
        bad.uri = "spotify:episode:1".into();
        assert!(matches!(
            r.handle(a, ClientMsg::QueueAdd { track: bad }, 0).as_slice(),
            [Out::One(_, ServerMsg::Error { code: ErrorCode::InvalidMessage, .. })]
        ));
        assert!(matches!(
            r.handle(a, ClientMsg::QueueRemove { id: 42 }, 0).as_slice(),
            [Out::One(_, ServerMsg::Error { code: ErrorCode::NotFound, .. })]
        ));
        assert!(matches!(
            r.handle(999, ClientMsg::Skip, 0).as_slice(),
            [Out::One(999, ServerMsg::Error { code: ErrorCode::NotJoined, .. })]
        ));
    }
}

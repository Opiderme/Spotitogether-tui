//! Room state machine. Pure logic: the caller passes the current server time
//! in ms and dispatches the returned messages, which keeps it easy to test.
//! Every permission is checked here, whatever the client UI shows.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::IpAddr;

use protocol::{
    AUTOPLAY_NAME, AdminCmd, ClientMsg, ErrorCode, ItemId, MAX_AUTOPLAY_TRACKS, Member, MemberId, Overrides, Perm,
    Permissions, Playback, QueueItem, Role, RoomSettings, RoomSnapshot, ServerMsg, TrackInfo,
};
use rand::seq::SliceRandom;

use crate::store::{SavedMember, Store};

const MAX_NAME_LEN: usize = 32;
const MAX_QUEUE_LEN: usize = 500;
/// Pending tracks per member without `QueueManage`.
pub const MAX_PENDING_PER_MEMBER: usize = 2;
/// How long a disconnected member keeps their place (role, own tracks).
pub const RECONNECT_GRACE_MS: u64 = 120_000;

/// A message to deliver.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// To every admitted member.
    All(ServerMsg),
    One(MemberId, ServerMsg),
    /// Close the member's connection (after the messages already queued).
    Close(MemberId),
}

pub struct JoinRequest<'a> {
    pub name: &'a str,
    pub host_token: Option<&'a str>,
    pub spotify_id: Option<&'a str>,
    pub session: Option<&'a str>,
    pub ip: IpAddr,
}

struct MemberState {
    member: Member,
    spotify_id: Option<String>,
    ip: IpAddr,
    session: String,
    /// Server time at which the connection was lost.
    offline_since: Option<u64>,
}

pub struct Room {
    settings: RoomSettings,
    host_token: String,
    members: BTreeMap<MemberId, MemberState>,
    queue: VecDeque<QueueItem>,
    playback: Playback,
    skip_votes: BTreeSet<MemberId>,
    /// Remembered roles per Spotify account.
    saved: HashMap<String, SavedMember>,
    /// `spotify:<id>` / `ip:<addr>` -> name
    bans: BTreeMap<String, String>,
    store: Option<Store>,
    autoplay: Vec<TrackInfo>,
    /// Indices of `autoplay` not played yet in this round (shuffled).
    autoplay_bag: Vec<usize>,
    next_member: MemberId,
    next_item: ItemId,
}

type Rejection = (ErrorCode, String);

impl Room {
    pub fn new(code: impl Into<String>, host_token: impl Into<String>, now_ms: u64, store: Option<Store>) -> Self {
        let mut settings =
            RoomSettings { code: code.into(), waiting_room: false, default_role: Role::Listener, autoplay: None };
        let (mut saved, mut bans) = (HashMap::new(), BTreeMap::new());
        let mut autoplay = Vec::new();
        if let Some(s) = &store {
            saved = s.members().unwrap_or_else(|e| warn_empty("members", e));
            bans = s.bans().unwrap_or_else(|e| warn_empty("bans", e));
            if let Ok(Some(v)) = s.setting("waiting_room") {
                settings.waiting_room = v == "true";
            }
            if let Ok(Some(v)) = s.setting("default_role")
                && let Ok(role) = serde_json::from_str(&v)
            {
                settings.default_role = role;
            }
            if let Ok(Some(v)) = s.setting("autoplay")
                && let Ok((name, tracks)) = serde_json::from_str::<(String, Vec<TrackInfo>)>(&v)
                && !tracks.is_empty()
            {
                settings.autoplay = Some(name);
                autoplay = tracks;
            }
        }
        Self {
            settings,
            host_token: host_token.into(),
            members: BTreeMap::new(),
            queue: VecDeque::new(),
            playback: Playback::stopped(0, now_ms),
            skip_votes: BTreeSet::new(),
            saved,
            bans,
            store,
            autoplay,
            autoplay_bag: Vec::new(),
            next_member: 1,
            next_item: 1,
        }
    }

    pub fn code(&self) -> &str {
        &self.settings.code
    }

    pub fn snapshot(&self) -> RoomSnapshot {
        RoomSnapshot {
            settings: self.settings.clone(),
            playback: self.playback.clone(),
            queue: self.queue.iter().cloned().collect(),
            members: self.members.values().map(|m| m.member.clone()).collect(),
        }
    }

    pub fn member_name(&self, id: MemberId) -> Option<&str> {
        self.members.get(&id).map(|m| m.member.name.as_str())
    }

    /// Admitted (not in the waiting room).
    pub fn is_admitted(&self, id: MemberId) -> bool {
        self.members.get(&id).is_some_and(|m| !m.member.pending)
    }

    // --------------------------------------------------------- members

    pub fn join(&mut self, req: JoinRequest<'_>) -> Result<(MemberId, Vec<Out>), Rejection> {
        let is_host = req.host_token == Some(self.host_token.as_str());
        if !is_host && self.is_banned(req.spotify_id, req.ip) {
            return Err((ErrorCode::Banned, "tu es banni de ce salon".into()));
        }
        if let Some(id) = req.session.and_then(|s| self.members.iter().find(|(_, m)| m.session == s).map(|(id, _)| *id)) {
            return Ok((id, self.resume(id, req.ip)));
        }
        let id = self.next_member;
        self.next_member += 1;

        let saved = req.spotify_id.and_then(|s| self.saved.get(s)).cloned();
        let (role, overrides) = match (is_host, saved) {
            (true, _) => (Role::Host, Overrides::new()),
            (false, Some(s)) if s.role != Role::Host => (s.role, s.overrides),
            _ => (self.settings.default_role, Overrides::new()),
        };
        let pending = !is_host && self.settings.waiting_room;
        let member = Member {
            id,
            name: self.unique_name(req.name),
            role,
            perms: Permissions::effective(role, &overrides),
            overrides,
            pending,
            online: true,
        };
        let state = MemberState {
            member,
            spotify_id: req.spotify_id.map(str::to_owned),
            ip: req.ip,
            session: crate::generate_host_token(),
            offline_since: None,
        };
        self.members.insert(id, state);

        let mut outs = vec![if pending { Out::One(id, ServerMsg::Waiting) } else { self.welcome(id) }];
        outs.push(self.members_msg());
        if !pending {
            outs.extend(self.votes_changed());
        }
        Ok((id, outs))
    }

    /// Same member reconnecting with their session token.
    fn resume(&mut self, id: MemberId, ip: IpAddr) -> Vec<Out> {
        let state = self.members.get_mut(&id).expect("id from members");
        state.offline_since = None;
        state.member.online = true;
        state.ip = ip;
        let pending = state.member.pending;
        let mut outs = vec![if pending { Out::One(id, ServerMsg::Waiting) } else { self.welcome(id) }, self.members_msg()];
        if !pending {
            outs.extend(self.votes_changed());
        }
        outs
    }

    /// Connection lost: the member stays (offline) for the grace period,
    /// except in the waiting room where there is nothing to keep.
    pub fn leave(&mut self, id: MemberId, now_ms: u64) -> Vec<Out> {
        let Some(state) = self.members.get_mut(&id) else { return vec![] };
        if state.member.pending {
            return self.remove(id, now_ms);
        }
        state.offline_since = Some(now_ms);
        state.member.online = false;
        self.skip_votes.remove(&id);
        let mut outs = vec![self.members_msg()];
        outs.extend(self.check_votes(now_ms));
        outs
    }

    /// Removes members whose grace period is over.
    pub fn expire(&mut self, now_ms: u64) -> Vec<Out> {
        let expired: Vec<MemberId> = self
            .members
            .iter()
            .filter(|(_, m)| m.offline_since.is_some_and(|t| now_ms >= t + RECONNECT_GRACE_MS))
            .map(|(id, _)| *id)
            .collect();
        expired.into_iter().flat_map(|id| self.remove(id, now_ms)).collect()
    }

    fn remove(&mut self, id: MemberId, now_ms: u64) -> Vec<Out> {
        if self.members.remove(&id).is_none() {
            return vec![];
        }
        self.skip_votes.remove(&id);
        let mut outs = vec![self.members_msg()];
        outs.extend(self.check_votes(now_ms));
        outs
    }

    fn welcome(&self, id: MemberId) -> Out {
        let session = self.members[&id].session.clone();
        Out::One(id, ServerMsg::Welcome { member_id: id, session, room: self.snapshot() })
    }

    fn is_banned(&self, spotify_id: Option<&str>, ip: IpAddr) -> bool {
        spotify_id.is_some_and(|s| self.bans.contains_key(&format!("spotify:{s}")))
            || self.bans.contains_key(&format!("ip:{ip}"))
    }

    // -------------------------------------------------------- playback

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
        let Some(me) = self.members.get(&from).map(|m| m.member.clone()).filter(|m| !m.pending) else {
            return vec![error(from, ErrorCode::NotJoined, "pas encore admis dans le salon")];
        };
        let deny = |what: &str| vec![error(from, ErrorCode::Forbidden, &format!("tu n'as pas le droit de {what}"))];
        let perms = me.perms;

        match msg {
            ClientMsg::QueueAdd { track } => {
                if !perms.queue_add {
                    return deny("ajouter des morceaux");
                }
                if !track.uri.starts_with("spotify:track:") || track.duration_ms == 0 {
                    return vec![error(from, ErrorCode::InvalidMessage, "morceau invalide")];
                }
                if self.queue.len() >= MAX_QUEUE_LEN {
                    return vec![error(from, ErrorCode::Forbidden, "file d'attente pleine")];
                }
                let mine = self.queue.iter().filter(|i| i.added_by_id == from).count();
                if !perms.queue_manage && mine >= MAX_PENDING_PER_MEMBER {
                    let msg = format!("{MAX_PENDING_PER_MEMBER} morceaux en attente maximum, attends qu'ils passent");
                    return vec![error(from, ErrorCode::Forbidden, &msg)];
                }
                let id = self.next_item;
                self.next_item += 1;
                self.queue.push_back(QueueItem { id, track, added_by: me.name.clone(), added_by_id: from });
                if self.playback.current.is_none() { self.advance(now_ms) } else { vec![self.queue_msg()] }
            }
            ClientMsg::QueueRemove { id } => match self.queue.iter().position(|i| i.id == id) {
                Some(index) => {
                    if !perms.queue_manage && self.queue[index].added_by_id != from {
                        return deny("retirer les morceaux des autres");
                    }
                    self.queue.remove(index);
                    vec![self.queue_msg()]
                }
                None => vec![error(from, ErrorCode::NotFound, "élément absent de la file")],
            },
            ClientMsg::QueueMove { id, index } => {
                if !perms.queue_manage {
                    return deny("réordonner la file");
                }
                match self.queue.iter().position(|i| i.id == id) {
                    Some(from_index) => {
                        let item = self.queue.remove(from_index).expect("index from position");
                        let to = index.min(self.queue.len());
                        self.queue.insert(to, item);
                        if from_index == to { vec![] } else { vec![self.queue_msg()] }
                    }
                    None => vec![error(from, ErrorCode::NotFound, "élément absent de la file")],
                }
            }
            ClientMsg::Play | ClientMsg::Pause | ClientMsg::Seek { .. } if !perms.playback => {
                deny("contrôler la lecture")
            }
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
            ClientMsg::Skip if !perms.skip => deny("passer directement (tu peux voter à la place)"),
            ClientMsg::Skip => self.advance(now_ms),
            ClientMsg::VoteSkip => {
                if perms.skip {
                    return self.advance(now_ms);
                }
                if !perms.listen {
                    return deny("voter (réservé aux auditeurs)");
                }
                if self.playback.current.is_none() || !self.skip_votes.insert(from) {
                    return vec![];
                }
                self.check_votes(now_ms)
            }
            ClientMsg::Admin(cmd) => {
                if me.role != Role::Host {
                    return deny("administrer le salon");
                }
                self.admin(from, cmd, now_ms)
            }
            ClientMsg::Join { .. } | ClientMsg::Ping { .. } => {
                vec![error(from, ErrorCode::InvalidMessage, "message inattendu")]
            }
        }
    }

    // ----------------------------------------------------------- admin

    fn admin(&mut self, host: MemberId, cmd: AdminCmd, now_ms: u64) -> Vec<Out> {
        let target = |room: &Self, id: MemberId| -> Result<(), Box<Out>> {
            match room.members.get(&id) {
                None => Err(Box::new(error(host, ErrorCode::NotFound, "membre introuvable"))),
                Some(m) if m.member.role == Role::Host => {
                    Err(Box::new(error(host, ErrorCode::Forbidden, "impossible sur l'hôte")))
                }
                Some(_) => Ok(()),
            }
        };
        match cmd {
            AdminCmd::SetRole { member, role } => {
                if let Err(e) = target(self, member) {
                    return vec![*e];
                }
                if role == Role::Host {
                    return vec![error(host, ErrorCode::Forbidden, "le rôle d'hôte ne se donne pas")];
                }
                self.update_member(member, |m| m.role = role);
                let mut outs = vec![self.members_msg()];
                outs.extend(self.check_votes(now_ms));
                outs
            }
            AdminCmd::SetPerm { member, perm, value } => {
                if let Err(e) = target(self, member) {
                    return vec![*e];
                }
                self.update_member(member, |m| {
                    let default = m.role.defaults().get(perm);
                    match value {
                        // An override equal to the role default is no override.
                        Some(v) if v != default => {
                            m.overrides.insert(perm, v);
                        }
                        _ => {
                            m.overrides.remove(&perm);
                        }
                    }
                });
                let mut outs = vec![self.members_msg()];
                if perm == Perm::Listen {
                    outs.extend(self.check_votes(now_ms));
                }
                outs
            }
            AdminCmd::Kick { member } | AdminCmd::Ban { member } => {
                if let Err(e) = target(self, member) {
                    return vec![*e];
                }
                let banned = matches!(cmd, AdminCmd::Ban { .. });
                if banned {
                    self.ban(member, now_ms);
                }
                let (code, text) = if banned {
                    (ErrorCode::Banned, "l'hôte t'a banni du salon")
                } else {
                    (ErrorCode::Kicked, "l'hôte t'a exclu du salon")
                };
                let mut outs = vec![error(member, code, text), Out::Close(member)];
                outs.extend(self.remove(member, now_ms));
                outs
            }
            AdminCmd::Admit { member } => match self.members.get_mut(&member) {
                Some(m) if m.member.pending => {
                    m.member.pending = false;
                    let mut outs = vec![self.welcome(member), self.members_msg()];
                    outs.extend(self.votes_changed());
                    outs
                }
                _ => vec![error(host, ErrorCode::NotFound, "personne en attente avec cet identifiant")],
            },
            AdminCmd::SetWaitingRoom { enabled } => {
                self.settings.waiting_room = enabled;
                self.persist_setting("waiting_room", if enabled { "true" } else { "false" });
                vec![self.settings_msg()]
            }
            AdminCmd::SetDefaultRole { role } => {
                if !Role::ASSIGNABLE.contains(&role) {
                    return vec![error(host, ErrorCode::Forbidden, "rôle par défaut invalide")];
                }
                self.settings.default_role = role;
                self.persist_setting("default_role", &serde_json::to_string(&role).expect("serializable"));
                vec![self.settings_msg()]
            }
            AdminCmd::RotateCode => {
                self.settings.code = crate::generate_code();
                vec![self.settings_msg()]
            }
            AdminCmd::SetAutoplay { name, mut tracks } => {
                tracks.retain(|t| t.uri.starts_with("spotify:track:") && t.duration_ms > 0);
                tracks.truncate(MAX_AUTOPLAY_TRACKS);
                self.settings.autoplay = (!tracks.is_empty()).then(|| name.clone());
                let stored = if tracks.is_empty() { String::new() } else { serde_json::to_string(&(&name, &tracks)).expect("serializable") };
                self.persist_setting("autoplay", &stored);
                self.autoplay = tracks;
                self.autoplay_bag.clear();
                let mut outs = vec![self.settings_msg()];
                // An idle room starts right away.
                if self.playback.current.is_none() && !self.autoplay.is_empty() {
                    outs.extend(self.advance(now_ms));
                }
                outs
            }
        }
    }

    /// Applies `change` to a member, recomputes their permissions and
    /// remembers role/overrides for their Spotify account.
    fn update_member(&mut self, id: MemberId, change: impl FnOnce(&mut Member)) {
        let Some(state) = self.members.get_mut(&id) else { return };
        change(&mut state.member);
        let m = &mut state.member;
        m.perms = Permissions::effective(m.role, &m.overrides);
        if let Some(spotify_id) = &state.spotify_id {
            let saved = SavedMember { role: m.role, overrides: m.overrides.clone() };
            if let Some(store) = &self.store
                && let Err(e) = store.save_member(spotify_id, &saved)
            {
                tracing::warn!(%e, "could not save member");
            }
            self.saved.insert(spotify_id.clone(), saved);
        }
    }

    fn ban(&mut self, id: MemberId, now_ms: u64) {
        let Some(state) = self.members.get(&id) else { return };
        let name = state.member.name.clone();
        let mut keys = Vec::new();
        if let Some(s) = &state.spotify_id {
            keys.push(format!("spotify:{s}"));
        }
        // Banning the loopback address would ban everyone testing locally.
        if !state.ip.is_loopback() {
            keys.push(format!("ip:{}", state.ip));
        }
        for key in keys {
            if let Some(store) = &self.store
                && let Err(e) = store.add_ban(&key, &name, now_ms)
            {
                tracing::warn!(%e, "could not save ban");
            }
            self.bans.insert(key, name.clone());
        }
    }

    fn persist_setting(&self, key: &str, value: &str) {
        if let Some(store) = &self.store
            && let Err(e) = store.set_setting(key, value)
        {
            tracing::warn!(%e, "could not save setting");
        }
    }

    // ----------------------------------------------------------- votes

    fn eligible_voters(&self) -> BTreeSet<MemberId> {
        self.members
            .values()
            .filter(|m| !m.member.pending && m.member.online && m.member.perms.listen)
            .map(|m| m.member.id)
            .collect()
    }

    /// Strict majority of the admitted members who hear the room.
    fn votes_needed(&self) -> u32 {
        (self.eligible_voters().len() / 2 + 1) as u32
    }

    fn votes_changed(&self) -> Vec<Out> {
        vec![Out::All(ServerMsg::SkipVotes { votes: self.skip_votes.len() as u32, needed: self.votes_needed() })]
    }

    /// Drops votes of members who can no longer vote and skips when the
    /// majority is reached.
    fn check_votes(&mut self, now_ms: u64) -> Vec<Out> {
        let eligible = self.eligible_voters();
        self.skip_votes.retain(|id| eligible.contains(id));
        if self.playback.current.is_some()
            && !self.skip_votes.is_empty()
            && self.skip_votes.len() as u32 >= self.votes_needed()
        {
            return self.advance(now_ms);
        }
        self.votes_changed()
    }

    // ------------------------------------------------------- internals

    /// Re-anchors the playback on `now_ms`, applies `change` and broadcasts it.
    fn update_playback(&mut self, now_ms: u64, change: impl FnOnce(&mut Playback)) -> Vec<Out> {
        let p = &mut self.playback;
        p.position_ms = p.position_at(now_ms);
        p.server_ms = now_ms;
        change(p);
        p.seq += 1;
        vec![Out::All(ServerMsg::Playback(self.playback.clone()))]
    }

    /// Next autoplay track: a shuffled round over the whole list, so nothing
    /// repeats before every track was played.
    fn next_autoplay(&mut self) -> Option<QueueItem> {
        if self.autoplay.is_empty() {
            return None;
        }
        if self.autoplay_bag.is_empty() {
            self.autoplay_bag = (0..self.autoplay.len()).collect();
            self.autoplay_bag.shuffle(&mut rand::rng());
        }
        let track = self.autoplay[self.autoplay_bag.pop()?].clone();
        let id = self.next_item;
        self.next_item += 1;
        Some(QueueItem { id, track, added_by: AUTOPLAY_NAME.to_owned(), added_by_id: 0 })
    }

    fn advance(&mut self, now_ms: u64) -> Vec<Out> {
        let next = self.queue.pop_front().or_else(|| self.next_autoplay());
        let seq = self.playback.seq + 1;
        self.playback = Playback { seq, is_playing: next.is_some(), current: next, position_ms: 0, server_ms: now_ms };
        self.skip_votes.clear();
        let mut outs = vec![Out::All(ServerMsg::Playback(self.playback.clone())), self.queue_msg()];
        outs.extend(self.votes_changed());
        outs
    }

    fn queue_msg(&self) -> Out {
        Out::All(ServerMsg::Queue { items: self.queue.iter().cloned().collect() })
    }

    fn members_msg(&self) -> Out {
        Out::All(ServerMsg::Members { members: self.members.values().map(|m| m.member.clone()).collect() })
    }

    fn settings_msg(&self) -> Out {
        Out::All(ServerMsg::Settings(self.settings.clone()))
    }

    fn unique_name(&self, wanted: &str) -> String {
        let base: String = wanted.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME_LEN).collect();
        let base = if base.is_empty() { "Invité".to_owned() } else { base };
        let taken = |n: &str| self.members.values().any(|m| m.member.name == n);
        if !taken(&base) {
            return base;
        }
        (2..).map(|i| format!("{base} ({i})")).find(|n| !taken(n)).expect("infinite range")
    }
}

fn warn_empty<T: Default>(what: &str, e: rusqlite::Error) -> T {
    tracing::warn!(%e, what, "could not load from store");
    T::default()
}

fn error(to: MemberId, code: ErrorCode, message: &str) -> Out {
    Out::One(to, ServerMsg::Error { code, message: message.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::TrackInfo;
    use std::net::Ipv4Addr;

    const LAN: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 30));

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

    fn req<'a>(name: &'a str, token: Option<&'a str>, spotify_id: Option<&'a str>) -> JoinRequest<'a> {
        JoinRequest { name, host_token: token, spotify_id, session: None, ip: LAN }
    }

    fn room() -> (Room, MemberId) {
        let mut r = Room::new("K7XQ2M", "secret", 0, Some(Store::in_memory().unwrap()));
        let (id, _) = r.join(req("Alex", Some("secret"), None)).unwrap();
        (r, id)
    }

    fn guest(r: &mut Room, name: &str) -> MemberId {
        r.join(req(name, None, Some(name))).unwrap().0
    }

    fn playback(outs: &[Out]) -> Option<&Playback> {
        outs.iter().find_map(|o| match o {
            Out::All(ServerMsg::Playback(p)) => Some(p),
            _ => None,
        })
    }

    fn forbidden(outs: &[Out]) -> bool {
        matches!(outs, [Out::One(_, ServerMsg::Error { code: ErrorCode::Forbidden, .. })])
    }

    fn member(r: &Room, id: MemberId) -> Member {
        r.snapshot().members.into_iter().find(|m| m.id == id).unwrap()
    }

    #[test]
    fn roles_and_unique_names() {
        let (mut r, host) = room();
        let (guest, outs) = r.join(req("Alex", Some("wrong"), None)).unwrap();
        assert!(matches!(&outs[0], Out::One(id, ServerMsg::Welcome { .. }) if *id == guest));
        assert_eq!(member(&r, host).role, Role::Host);
        let g = member(&r, guest);
        assert_eq!((g.role, g.name.as_str()), (Role::Listener, "Alex (2)"));
        assert_eq!(g.perms, Role::Listener.defaults());
    }

    #[test]
    fn first_track_starts_immediately_then_queues() {
        let (mut r, a) = room();
        let outs = r.handle(a, ClientMsg::QueueAdd { track: track(1, 1000) }, 50);
        let p = playback(&outs).unwrap();
        assert!(p.is_playing);
        assert_eq!(p.current.as_ref().unwrap().added_by, "Alex");
        r.handle(a, ClientMsg::QueueAdd { track: track(2, 1000) }, 60);
        assert_eq!(r.snapshot().queue.len(), 1);
        assert_eq!(r.deadline(), Some(1050));
    }

    #[test]
    fn track_end_advances_then_stops() {
        let (mut r, a) = room();
        r.handle(a, ClientMsg::QueueAdd { track: track(1, 1000) }, 0);
        r.handle(a, ClientMsg::QueueAdd { track: track(2, 500) }, 0);
        assert!(r.tick(999).is_empty());
        assert_eq!(playback(&r.tick(1000)).unwrap().current.as_ref().unwrap().track.name, "T2");
        let p = playback(&r.tick(1500)).unwrap().clone();
        assert!(p.current.is_none() && !p.is_playing);
    }

    #[test]
    fn pause_play_seek_keep_position_consistent() {
        let (mut r, a) = room();
        r.handle(a, ClientMsg::QueueAdd { track: track(1, 10_000) }, 0);
        let p = playback(&r.handle(a, ClientMsg::Pause, 3_000)).unwrap().clone();
        assert_eq!((p.position_ms, p.is_playing), (3_000, false));
        let p = playback(&r.handle(a, ClientMsg::Play, 8_000)).unwrap().clone();
        assert_eq!((p.position_ms, p.server_ms), (3_000, 8_000));
        let p = playback(&r.handle(a, ClientMsg::Seek { position_ms: 99_999 }, 9_000)).unwrap().clone();
        assert_eq!(p.position_ms, 10_000);
    }

    #[test]
    fn listeners_are_limited_and_cannot_control_playback() {
        let (mut r, host) = room();
        let lou = guest(&mut r, "Lou");
        r.handle(host, ClientMsg::QueueAdd { track: track(1, 1000) }, 0); // playing
        for n in 2..=3 {
            r.handle(lou, ClientMsg::QueueAdd { track: track(n, 1000) }, 0);
        }
        assert!(forbidden(&r.handle(lou, ClientMsg::QueueAdd { track: track(4, 1000) }, 0)));
        assert!(forbidden(&r.handle(lou, ClientMsg::Pause, 0)));
        assert!(forbidden(&r.handle(lou, ClientMsg::Skip, 0)));
        assert!(forbidden(&r.handle(lou, ClientMsg::Admin(AdminCmd::RotateCode), 0)));

        // Own tracks can be removed, others' cannot; reordering is refused.
        r.handle(host, ClientMsg::QueueAdd { track: track(5, 1000) }, 0);
        let ids: Vec<_> = r.snapshot().queue.iter().map(|i| i.id).collect();
        assert!(forbidden(&r.handle(lou, ClientMsg::QueueRemove { id: ids[2] }, 0)));
        assert!(forbidden(&r.handle(lou, ClientMsg::QueueMove { id: ids[0], index: 1 }, 0)));
        assert_eq!(r.handle(lou, ClientMsg::QueueRemove { id: ids[0] }, 0).len(), 1);
    }

    #[test]
    fn vote_skip_needs_a_majority_of_listeners() {
        let (mut r, host) = room();
        let (sam, lou) = (guest(&mut r, "Sam"), guest(&mut r, "Lou"));
        let max = guest(&mut r, "Max");
        r.handle(host, ClientMsg::Admin(AdminCmd::SetRole { member: max, role: Role::Spectator }), 0);
        r.handle(host, ClientMsg::QueueAdd { track: track(1, 100_000) }, 0);
        // Voters: Alex, Sam, Lou (Max is a spectator) -> 2 needed.
        assert!(forbidden(&r.handle(max, ClientMsg::VoteSkip, 0)));
        let outs = r.handle(sam, ClientMsg::VoteSkip, 0);
        assert!(outs.contains(&Out::All(ServerMsg::SkipVotes { votes: 1, needed: 2 })));
        assert!(r.handle(sam, ClientMsg::VoteSkip, 0).is_empty(), "double vote ignored");
        let outs = r.handle(lou, ClientMsg::VoteSkip, 1);
        assert!(playback(&outs).unwrap().current.is_none(), "skipped");
        assert!(outs.contains(&Out::All(ServerMsg::SkipVotes { votes: 0, needed: 2 })));
    }

    #[test]
    fn overrides_and_roles_are_remembered_per_spotify_account() {
        let (mut r, host) = room();
        let lou = guest(&mut r, "Lou");
        let set = |perm, value| ClientMsg::Admin(AdminCmd::SetPerm { member: lou, perm, value });
        r.handle(host, set(Perm::QueueAdd, Some(false)), 0);
        r.handle(host, set(Perm::Skip, Some(true)), 0);
        r.handle(host, set(Perm::Listen, Some(true)), 0); // equals default: no override
        let m = member(&r, lou);
        assert!(!m.perms.queue_add && m.perms.skip);
        assert_eq!(m.overrides.len(), 2);
        assert!(forbidden(&r.handle(lou, ClientMsg::QueueAdd { track: track(1, 1000) }, 0)));

        // Lou comes back later: role and overrides are restored.
        r.leave(lou, 0);
        let lou2 = guest(&mut r, "Lou");
        assert_eq!(member(&r, lou2).overrides.len(), 2);

        // A fresh server with the same store remembers too.
        let store = r.store.take().unwrap();
        let mut r2 = Room::new("ABC", "t", 0, Some(store));
        let lou3 = guest(&mut r2, "Lou");
        assert!(!member(&r2, lou3).perms.queue_add);
    }

    #[test]
    fn kick_ban_and_host_protection() {
        let (mut r, host) = room();
        let lou = guest(&mut r, "Lou");
        let outs = r.handle(host, ClientMsg::Admin(AdminCmd::Ban { member: lou }), 0);
        assert!(matches!(&outs[0], Out::One(id, ServerMsg::Error { code: ErrorCode::Banned, .. }) if *id == lou));
        assert_eq!(outs[1], Out::Close(lou));
        assert!(r.snapshot().members.iter().all(|m| m.id != lou));
        // Same Spotify account, or same LAN address, is refused.
        assert_eq!(r.join(req("Lou", None, Some("Lou"))).unwrap_err().0, ErrorCode::Banned);
        assert_eq!(r.join(req("Autre", None, None)).unwrap_err().0, ErrorCode::Banned);
        // The host is never banned, and cannot be targeted.
        assert!(r.join(req("Alex", Some("secret"), None)).is_ok());
        let sam = r.join(JoinRequest { ip: IpAddr::V4(Ipv4Addr::LOCALHOST), ..req("Sam", None, None) }).unwrap().0;
        assert!(forbidden(&r.handle(host, ClientMsg::Admin(AdminCmd::Kick { member: host }), 0)));
        let outs = r.handle(host, ClientMsg::Admin(AdminCmd::Kick { member: sam }), 0);
        assert!(outs.contains(&Out::Close(sam)));
    }

    #[test]
    fn waiting_room_holds_members_until_admitted() {
        let (mut r, host) = room();
        r.handle(host, ClientMsg::Admin(AdminCmd::SetWaitingRoom { enabled: true }), 0);
        let (lou, outs) = r.join(req("Lou", None, Some("lou"))).unwrap();
        assert_eq!(outs[0], Out::One(lou, ServerMsg::Waiting));
        assert!(!r.is_admitted(lou));
        assert!(matches!(
            r.handle(lou, ClientMsg::QueueAdd { track: track(1, 1000) }, 0).as_slice(),
            [Out::One(_, ServerMsg::Error { code: ErrorCode::NotJoined, .. })]
        ));
        let outs = r.handle(host, ClientMsg::Admin(AdminCmd::Admit { member: lou }), 0);
        assert!(matches!(&outs[0], Out::One(id, ServerMsg::Welcome { .. }) if *id == lou));
        assert!(r.is_admitted(lou));
    }

    #[test]
    fn settings_default_role_and_code_rotation() {
        let (mut r, host) = room();
        r.handle(host, ClientMsg::Admin(AdminCmd::SetDefaultRole { role: Role::Spectator }), 0);
        let max = guest(&mut r, "Max");
        assert_eq!(member(&r, max).role, Role::Spectator);
        assert!(forbidden(&r.handle(host, ClientMsg::Admin(AdminCmd::SetDefaultRole { role: Role::Host }), 0)));
        let old = r.code().to_owned();
        let outs = r.handle(host, ClientMsg::Admin(AdminCmd::RotateCode), 0);
        assert!(matches!(&outs[0], Out::All(ServerMsg::Settings(s)) if s.code != old));
    }

    #[test]
    fn rejects_bad_tracks_and_unknown_members() {
        let (mut r, a) = room();
        let mut bad = track(1, 1000);
        bad.uri = "spotify:episode:1".into();
        assert!(matches!(
            r.handle(a, ClientMsg::QueueAdd { track: bad }, 0).as_slice(),
            [Out::One(_, ServerMsg::Error { code: ErrorCode::InvalidMessage, .. })]
        ));
        assert!(matches!(
            r.handle(999, ClientMsg::Skip, 0).as_slice(),
            [Out::One(999, ServerMsg::Error { code: ErrorCode::NotJoined, .. })]
        ));
    }

    #[test]
    fn disconnected_members_keep_their_place_during_the_grace_period() {
        let (mut r, host) = room();
        let lou = guest(&mut r, "Lou");
        r.handle(host, ClientMsg::QueueAdd { track: track(1, 100_000) }, 0);
        r.handle(lou, ClientMsg::QueueAdd { track: track(2, 1000) }, 0);
        let session = match r.welcome(lou) {
            Out::One(_, ServerMsg::Welcome { session, .. }) => session,
            _ => unreachable!(),
        };
        r.leave(lou, 1_000);
        assert!(!member(&r, lou).online);
        assert!(r.expire(1_000 + RECONNECT_GRACE_MS - 1).is_empty());

        // Back with the session: same id, own track still removable.
        let (again, outs) = r.join(JoinRequest { session: Some(&session), ..req("Lou", None, Some("Lou")) }).unwrap();
        assert_eq!(again, lou);
        assert!(matches!(&outs[0], Out::One(id, ServerMsg::Welcome { .. }) if *id == lou));
        assert!(member(&r, lou).online);
        let item = r.snapshot().queue[0].id;
        assert!(!forbidden(&r.handle(lou, ClientMsg::QueueRemove { id: item }, 2_000)));

        // Gone for good after the grace period.
        r.leave(lou, 3_000);
        assert!(!r.expire(3_000 + RECONNECT_GRACE_MS).is_empty());
        assert!(r.snapshot().members.iter().all(|m| m.id != lou));
        let (fresh, _) = r.join(JoinRequest { session: Some(&session), ..req("Lou", None, Some("Lou")) }).unwrap();
        assert_ne!(fresh, lou);
    }

    #[test]
    fn autoplay_fills_an_empty_queue_without_repeats() {
        let (mut r, host) = room();
        let tracks: Vec<TrackInfo> = (1..=3).map(|n| track(n, 1000)).collect();
        let outs = r.handle(host, ClientMsg::Admin(AdminCmd::SetAutoplay { name: "Ma playlist".into(), tracks }), 0);
        assert!(matches!(&outs[0], Out::All(ServerMsg::Settings(s)) if s.autoplay.as_deref() == Some("Ma playlist")));
        let first = playback(&outs).expect("idle room starts").current.clone().unwrap();
        assert_eq!(first.added_by, AUTOPLAY_NAME);

        // A queued track has priority over autoplay.
        r.handle(host, ClientMsg::QueueAdd { track: track(9, 1000) }, 10);
        assert_eq!(playback(&r.tick(1000)).unwrap().current.as_ref().unwrap().track.name, "T9");

        let mut seen = vec![first.track.name];
        for t in [2000, 3000] {
            seen.push(playback(&r.tick(t)).unwrap().current.as_ref().unwrap().track.name.clone());
        }
        seen.sort();
        assert_eq!(seen, ["T1", "T2", "T3"], "one full round before any repeat");

        // Remembered by the store, turned off with an empty list.
        let store = r.store.take().unwrap();
        assert!(store.setting("autoplay").unwrap().unwrap().contains("Ma playlist"));
        let mut r2 = Room::new("X", "secret", 0, Some(store));
        let host2 = r2.join(req("Alex", Some("secret"), None)).unwrap().0;
        assert_eq!(r2.snapshot().settings.autoplay.as_deref(), Some("Ma playlist"));
        r2.handle(host2, ClientMsg::Admin(AdminCmd::SetAutoplay { name: String::new(), tracks: vec![] }), 0);
        assert_eq!(r2.snapshot().settings.autoplay, None);
    }
}

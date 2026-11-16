//! TUI state and input handling. No I/O here: handlers return [`Effect`]s
//! that the event loop executes, which keeps this module testable.

use std::time::{Duration, Instant};

use client::{Event, SyncEvent};
use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use image::DynamicImage;
use protocol::{AddMode, AdminCmd, ClientMsg, Member, MemberId, Perm, Permissions, Playback, QueueItem, Role, RoomSettings, ServerMsg};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{ListState, TableState};
use spotify::models::{PlaylistSummary, Track};

use crate::cover::{Cell, CoverMode};
use crate::theme::Theme;

const SEEK_STEP_MS: u64 = 10_000;
const TOAST_FOR: Duration = Duration::from_secs(5);
pub const SEARCH_PAGE: u32 = 10;
/// Search-as-you-type: delay after the last key press, so a fast typist
/// sends one request per pause rather than one per character (API quota).
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// Volume keys: one request after a short pause, not one per key press.
pub const VOLUME_DEBOUNCE: Duration = Duration::from_millis(200);
const VOLUME_STEP: u8 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send(ClientMsg),
    Search { request: u64, query: String, offset: u32 },
    FetchCover(String),
    LoadPlaylists,
    /// This member's own volume (only they hear it).
    SetVolume(u8),
    /// Read the current volume of the member's player.
    LoadVolume,
    /// Which of these tracks are in the member's liked songs.
    CheckLiked(Vec<String>),
    /// Like / unlike a track for this member.
    SetLiked { uri: String, name: String, liked: bool },
    /// The member's playlists, for the library.
    LoadLibrary,
    /// Tracks of a library list.
    LoadLibraryList { source: crate::library::Source, name: String },
    /// Copy the invite link to the clipboard.
    CopyInvite(String),
    LoadPlaylistTracks { id: String, name: String },
    Resync,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Queue,
    Members,
    Search,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Queue => Self::Members,
            Self::Members => Self::Search,
            Self::Search => Self::Queue,
        }
    }

    fn prev(self) -> Self {
        self.next().next()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Warn,
    Bad,
}

/// Screen regions recorded while drawing, used for mouse hit-testing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Areas {
    pub progress: Rect,
    pub prev_button: Rect,
    pub play_button: Rect,
    pub next_button: Rect,
    pub queue: Rect,
    /// First row of queue items (after the header).
    pub queue_rows: Rect,
    pub members: Rect,
    pub members_rows: Rect,
    pub search: Rect,
    pub search_input: Rect,
    pub results_rows: Rect,
    /// Permissions popup: whole popup, member rows, and the x span of each
    /// column (role, then one per permission).
    pub perm_popup: Rect,
    pub perm_rows: Rect,
    pub picker_popup: Rect,
    pub picker_rows: Rect,
    pub library_popup: Rect,
    pub library_rows: Rect,
    pub like_button: Rect,
    pub volume_bar: Rect,
    pub perm_cols: [(u16, u16); PERM_COLUMNS],
}

/// Role column + one column per permission.
pub const PERM_COLUMNS: usize = 1 + Perm::ALL.len();

/// Host's autoplay playlist picker (`A`).
#[derive(Debug, Default)]
pub struct PlaylistPicker {
    pub loading: bool,
    pub playlists: Vec<PlaylistSummary>,
    pub state: ListState,
}

/// Host's permissions popup (`p`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermPanel {
    pub row: usize,
    /// 0 = role, 1.. = `Perm::ALL[col - 1]`
    pub col: usize,
    /// Ban asked once; a second `b` on the same member confirms.
    pub confirm_ban: Option<MemberId>,
}

#[derive(Debug, Default)]
pub struct Search {
    pub input: String,
    pub editing: bool,
    pub results: Vec<Track>,
    pub state: TableState,
    pub query: String,
    pub offset: u32,
    pub total: u32,
    pub loading: bool,
    request: u64,
    /// When the typed text should be searched (search-as-you-type).
    pub due: Option<Instant>,
}

pub struct SearchOutcome {
    pub request: u64,
    pub offset: u32,
    pub result: Result<(Vec<Track>, u32), String>,
}

pub struct CoverCache {
    pub url: String,
    pub size: (u16, u16),
    pub mode: CoverMode,
    pub cells: Vec<Vec<Cell>>,
}

pub struct App {
    /// Invite link to share (known when this process hosts the room).
    pub invite: Option<String>,
    /// This process hosts the room: quitting closes it for everyone.
    pub hosting: bool,
    /// First `q` of a host, waiting for confirmation.
    pub quit_armed: Option<Instant>,
    pub settings: RoomSettings,
    /// Skip votes: (votes, needed).
    pub votes: (u32, u32),
    pub perm_panel: Option<PermPanel>,
    pub playlist_picker: Option<PlaylistPicker>,
    pub library: Option<crate::library::Library>,
    /// This member's volume (None until read from Spotify).
    pub volume: Option<u8>,
    /// Muted for the current track only: (item id, volume to restore).
    pub muted: Option<(protocol::ItemId, u8)>,
    pub volume_due: Option<Instant>,
    /// Liked state of tracks seen so far (by URI).
    pub liked: std::collections::HashMap<String, bool>,
    pub me: MemberId,
    pub playback: Playback,
    pub queue: Vec<QueueItem>,
    pub members: Vec<Member>,
    pub clock_offset_ms: i64,
    pub can_play: bool,

    pub focus: Focus,
    pub queue_state: TableState,
    pub members_state: ListState,
    pub search: Search,
    pub drag_from: Option<usize>,
    pub show_help: bool,

    pub sync: (String, Level),
    pub toast: Option<(String, Level, Instant)>,

    pub cover_mode: CoverMode,
    pub cover: Option<(String, DynamicImage)>,
    pub cover_cache: Option<CoverCache>,
    /// Graphics protocol of the terminal (Sixel, Kitty…), if any.
    pub image_picker: Option<ratatui_image::picker::Picker>,
    /// Picture encoded for that protocol, for the current cover.
    pub cover_image: Option<ratatui_image::protocol::StatefulProtocol>,

    pub theme: Theme,
    pub areas: Areas,
    pub exit_message: Option<String>,
}

impl App {
    pub fn new(joined: &client::Joined, can_play: bool, theme: Theme) -> Self {
        let room = &joined.room;
        let mut app = Self {
            invite: None,
            hosting: false,
            quit_armed: None,
            settings: room.settings.clone(),
            votes: (0, 0),
            perm_panel: None,
            playlist_picker: None,
            library: None,
            volume: None,
            muted: None,
            volume_due: None,
            liked: std::collections::HashMap::new(),
            me: joined.member_id,
            playback: room.playback.clone(),
            queue: room.queue.clone(),
            members: room.members.clone(),
            clock_offset_ms: 0,
            can_play,
            focus: Focus::Queue,
            queue_state: TableState::default(),
            members_state: ListState::default(),
            search: Search::default(),
            drag_from: None,
            show_help: false,
            sync: if can_play {
                ("connexion à Spotify…".into(), Level::Info)
            } else {
                ("spectateur".into(), Level::Info)
            },
            toast: None,
            cover_mode: CoverMode::Ascii,
            cover: None,
            cover_cache: None,
            image_picker: None,
            cover_image: None,
            theme,
            areas: Areas::default(),
            exit_message: None,
        };
        app.clamp_selections();
        app
    }

    /// Effects to run right after start (cover of the current track).
    pub fn initial_effects(&self) -> Vec<Effect> {
        let mut fx: Vec<Effect> = self.cover_url().map(|u| Effect::FetchCover(u.to_owned())).into_iter().collect();
        if self.can_play {
            fx.push(Effect::LoadVolume);
            fx.extend(self.current_uri().map(|u| Effect::CheckLiked(vec![u])));
        }
        fx
    }

    fn current_uri(&self) -> Option<String> {
        self.playback.current.as_ref().map(|i| i.track.uri.clone())
    }

    /// Liked state of the current track, if known.
    pub fn current_liked(&self) -> Option<bool> {
        self.current_uri().and_then(|u| self.liked.get(&u).copied())
    }

    pub fn me(&self) -> Option<&Member> {
        self.members.iter().find(|m| m.id == self.me)
    }

    pub fn is_host(&self) -> bool {
        self.me().is_some_and(|m| m.role == Role::Host)
    }

    /// Invite link with the current code (the host may have rotated it).
    pub fn current_invite(&self) -> Option<String> {
        let link = self.invite.as_ref()?;
        let base = link.rsplit_once('/').map_or(link.as_str(), |(base, _)| base);
        Some(format!("{base}/{}", self.settings.code))
    }

    pub fn my_perms(&self) -> Permissions {
        self.me().map(|m| m.perms).unwrap_or(Permissions::NONE)
    }

    /// Checks a permission locally (the server enforces it anyway) so the
    /// user gets a clear message instead of a refused command.
    fn allowed(&mut self, ok: bool, what: &str) -> bool {
        if !ok {
            let role = self.me().map(|m| m.role.label()).unwrap_or("?");
            self.toast(format!("Ton rôle ({role}) ne permet pas de {what}"), Level::Warn);
        }
        ok
    }

    /// Members shown in the permissions popup, in display order.
    pub fn panel_members(&self) -> Vec<&Member> {
        let mut members: Vec<&Member> = self.members.iter().collect();
        members.sort_by_key(|m| (m.pending, m.role, m.id));
        members
    }

    pub fn server_now(&self) -> u64 {
        (client::clock::local_ms() as i64 + self.clock_offset_ms).max(0) as u64
    }

    pub fn position_ms(&self) -> u64 {
        self.playback.position_at(self.server_now())
    }

    fn cover_url(&self) -> Option<&str> {
        self.playback.current.as_ref()?.track.cover_url.as_deref()
    }

    pub fn toast(&mut self, text: impl Into<String>, level: Level) {
        self.toast = Some((text.into(), level, Instant::now()));
    }

    pub fn active_toast(&self) -> Option<(&str, Level)> {
        self.toast
            .as_ref()
            .filter(|(_, _, at)| at.elapsed() < TOAST_FOR)
            .map(|(t, l, _)| (t.as_str(), *l))
    }

    // ------------------------------------------------------------ room

    pub fn on_room_event(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Server(msg) => return self.on_server_msg(*msg),
            Event::Clock { offset_ms, .. } => self.clock_offset_ms = offset_ms,
            Event::Sync(e) => self.on_sync_event(e),
            Event::Reconnecting { attempt, .. } => {
                self.sync = (format!("connexion perdue, reconnexion… (essai {attempt})"), Level::Bad);
            }
            Event::Reconnected => self.toast("Reconnecté au salon", Level::Good),
            Event::Disconnected(reason) => {
                self.exit_message = Some(reason);
                return vec![Effect::Quit];
            }
        }
        vec![]
    }

    fn on_server_msg(&mut self, msg: ServerMsg) -> Vec<Effect> {
        match msg {
            ServerMsg::Playback(p) => {
                let old_url = self.cover_url().map(str::to_owned);
                let old_item = self.playback.current.as_ref().map(|i| i.id);
                self.playback = p;
                let mut fx = Vec::new();
                let new_item = self.playback.current.as_ref().map(|i| i.id);
                if new_item != old_item {
                    fx.extend(self.on_track_changed(new_item));
                }
                let new_url = self.cover_url().map(str::to_owned);
                if new_url != old_url {
                    self.cover = None;
                    self.cover_cache = None;
                    self.cover_image = None;
                    fx.extend(new_url.map(Effect::FetchCover));
                }
                return fx;
            }
            ServerMsg::Queue { items } => {
                self.queue = items;
                self.clamp_selections();
            }
            ServerMsg::Members { members } => {
                let newcomer = members.iter().find(|m| !self.members.iter().any(|o| o.id == m.id));
                let text = match newcomer {
                    Some(m) if m.pending && self.is_host() => {
                        Some((format!("{} attend d'être admis (p puis a)", m.name), Level::Warn))
                    }
                    Some(m) if !m.pending => Some((format!("{} a rejoint le salon", m.name), Level::Info)),
                    _ => None,
                };
                let my_role = self.me().map(|m| m.role);
                self.members = members;
                if let Some(new_role) = self.me().map(|m| m.role).filter(|r| Some(*r) != my_role && my_role.is_some()) {
                    self.toast(format!("Ton rôle est maintenant : {}", new_role.label()), Level::Info);
                } else if let Some((text, level)) = text {
                    self.toast(text, level);
                }
                self.clamp_selections();
            }
            ServerMsg::Settings(settings) => {
                if settings.code != self.settings.code {
                    self.toast(format!("Nouveau code du salon : {}", settings.code), Level::Info);
                }
                self.settings = settings;
            }
            ServerMsg::SkipVotes { votes, needed } => self.votes = (votes, needed),
            ServerMsg::Error { message, .. } => self.toast(message, Level::Bad),
            // After a reconnection: fresh state from the server.
            ServerMsg::Welcome { member_id, room, .. } => {
                self.me = member_id;
                self.settings = room.settings;
                self.queue = room.queue;
                self.members = room.members;
                self.sync = ("synchro".into(), Level::Info);
                self.clamp_selections();
                return self.on_server_msg(ServerMsg::Playback(room.playback));
            }
            ServerMsg::Pong { .. } | ServerMsg::Waiting => {}
        }
        vec![]
    }

    fn on_sync_event(&mut self, e: SyncEvent) {
        match e {
            SyncEvent::Applied { .. } => self.sync = ("synchro".into(), Level::Good),
            SyncEvent::Corrected { drift_ms } => {
                self.sync = (format!("dérive {:+.1} s corrigée", drift_ms as f64 / 1000.0), Level::Warn);
            }
            SyncEvent::Resynced { reason } => self.sync = (format!("resynchro : {reason}"), Level::Warn),
            SyncEvent::Stopped => self.sync = ("écoute désactivée par l'hôte".into(), Level::Warn),
            SyncEvent::DeviceSelected { name } => self.toast(format!("Appareil Spotify : {name}"), Level::Info),
            SyncEvent::Error(err) => self.sync = (err, Level::Bad),
        }
    }

    pub fn on_cover(&mut self, url: String, img: Result<DynamicImage, String>) {
        if self.cover_url() != Some(url.as_str()) {
            return; // the track changed meanwhile
        }
        match img {
            Ok(img) => {
                self.cover_image = self.image_picker.as_ref().map(|p| p.new_resize_protocol(img.clone()));
                self.cover = Some((url, img));
            }
            Err(e) => tracing::warn!(%e, "cover download failed"),
        }
    }

    /// New track: lift a per-track mute, refresh the liked state.
    fn on_track_changed(&mut self, item: Option<protocol::ItemId>) -> Vec<Effect> {
        if !self.can_play {
            return vec![];
        }
        let mut fx = Vec::new();
        if let Some((muted_item, restore)) = self.muted
            && Some(muted_item) != item
        {
            self.muted = None;
            self.volume = Some(restore);
            self.volume_due = None;
            self.toast("Son rétabli pour ce titre", Level::Info);
            fx.push(Effect::SetVolume(restore));
        }
        if let Some(uri) = self.current_uri().filter(|u| !self.liked.contains_key(u)) {
            fx.push(Effect::CheckLiked(vec![uri]));
        }
        fx
    }

    pub fn on_volume(&mut self, volume: Option<u8>) {
        if self.volume_due.is_none() && self.muted.is_none() {
            self.volume = volume;
        }
    }

    pub fn on_liked(&mut self, states: Vec<(String, bool)>) {
        self.liked.extend(states);
    }

    pub fn on_like_done(&mut self, uri: String, name: String, liked: bool, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.liked.insert(uri, liked);
                let text = if liked { format!("♥ « {name} » ajouté à tes titres likés") } else { format!("« {name} » retiré de tes titres likés") };
                self.toast(text, Level::Good);
            }
            Err(e) => self.toast(format!("J'aime : {e}"), Level::Bad),
        }
    }

    // ------------------------------------------------- volume and likes

    fn needs_account(&mut self, what: &str) -> bool {
        if !self.can_play {
            self.toast(format!("{what} nécessite ton compte Spotify (pas en mode spectateur)"), Level::Warn);
        }
        self.can_play
    }

    /// Changes this member's volume; sent after a short pause.
    fn change_volume(&mut self, delta: i16) -> Vec<Effect> {
        if !self.needs_account("Le volume") {
            return vec![];
        }
        let base = match self.muted.take() {
            Some((_, restore)) => restore, // any change ends the mute
            None => self.volume.unwrap_or(50),
        };
        self.set_volume((i16::from(base) + delta).clamp(0, 100) as u8)
    }

    fn set_volume(&mut self, percent: u8) -> Vec<Effect> {
        self.muted = None;
        self.volume = Some(percent);
        self.volume_due = Some(Instant::now() + VOLUME_DEBOUNCE);
        vec![]
    }

    pub fn volume_due(&self) -> Option<Instant> {
        self.volume_due
    }

    pub fn on_volume_due(&mut self) -> Vec<Effect> {
        if self.volume_due.is_none_or(|t| Instant::now() < t) {
            return vec![];
        }
        self.volume_due = None;
        self.volume.map(Effect::SetVolume).into_iter().collect()
    }

    /// Mutes this member until the next track (or unmutes).
    fn toggle_mute(&mut self) -> Vec<Effect> {
        if !self.needs_account("Le mute") {
            return vec![];
        }
        if let Some((_, restore)) = self.muted.take() {
            self.volume = Some(restore);
            self.toast("Son rétabli", Level::Info);
            return vec![Effect::SetVolume(restore)];
        }
        let Some(item) = self.playback.current.as_ref().map(|i| i.id) else {
            self.toast("Rien à couper : aucun titre en cours", Level::Info);
            return vec![];
        };
        let restore = self.volume.unwrap_or(50).max(1);
        self.muted = Some((item, restore));
        self.volume_due = None;
        self.toast("Muet pour ce titre (chez toi seulement) : le son revient au suivant", Level::Info);
        vec![Effect::SetVolume(0)]
    }

    /// Likes or unlikes a track for this member.
    fn toggle_like(&mut self, uri: String, name: String) -> Vec<Effect> {
        if !self.needs_account("J'aime") {
            return vec![];
        }
        let liked = !self.liked.get(&uri).copied().unwrap_or(false);
        vec![Effect::SetLiked { uri, name, liked }]
    }

    fn like_current(&mut self) -> Vec<Effect> {
        match self.playback.current.as_ref().map(|i| (i.track.uri.clone(), i.track.name.clone())) {
            Some((uri, name)) => self.toggle_like(uri, name),
            None => {
                self.toast("Aucun titre en cours", Level::Info);
                vec![]
            }
        }
    }

    /// Search results; returns the liked-state check of the new tracks.
    pub fn on_search(&mut self, outcome: SearchOutcome) -> Vec<Effect> {
        if outcome.request != self.search.request {
            return vec![]; // stale
        }
        let mut fx = Vec::new();
        self.search.loading = false;
        match outcome.result {
            Ok((tracks, total)) => {
                let unknown: Vec<String> =
                    tracks.iter().map(|t| t.uri.clone()).filter(|u| !self.liked.contains_key(u)).collect();
                if self.can_play && !unknown.is_empty() {
                    fx.push(Effect::CheckLiked(unknown));
                }
                self.search.results = tracks;
                self.search.total = total;
                self.search.offset = outcome.offset;
                self.search.state.select((!self.search.results.is_empty()).then_some(0));
                if self.search.results.is_empty() {
                    self.toast("Aucun résultat", Level::Info);
                }
            }
            Err(e) => self.toast(format!("Recherche : {e}"), Level::Bad),
        }
        fx
    }

    // ---------------------------------------------------------- input

    pub fn on_terminal_event(&mut self, event: TermEvent) -> Vec<Effect> {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            TermEvent::Mouse(mouse) => self.on_mouse(mouse),
            TermEvent::Paste(text) if self.search.editing => {
                self.search.input.push_str(text.trim());
                self.input_changed();
                vec![]
            }
            _ => vec![],
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return vec![Effect::Quit];
        }
        if self.show_help {
            self.show_help = false;
            return vec![];
        }
        if self.perm_panel.is_some() {
            return self.on_perm_key(key);
        }
        if self.playlist_picker.is_some() {
            return self.on_picker_key(key);
        }
        if self.library.is_some() {
            return self.on_library_key(key);
        }
        if self.focus == Focus::Search && self.search.editing {
            return self.on_search_key(key);
        }

        match key.code {
            KeyCode::Char('q') => {
                let confirmed = self.quit_armed.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
                if !self.hosting || confirmed {
                    return vec![Effect::Quit];
                }
                self.quit_armed = Some(Instant::now());
                self.toast("Tu héberges : quitter ferme le salon pour tout le monde (q encore)", Level::Warn);
            }
            KeyCode::Char('i') => match self.current_invite() {
                Some(link) => return vec![Effect::CopyInvite(link)],
                None => self.toast(format!("Code du salon : {} (demande le lien à l'hôte)", self.settings.code), Level::Info),
            },
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Char('/') => {
                self.focus = Focus::Search;
                self.search.editing = true;
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char(' ') => return self.toggle_play(),
            KeyCode::Char('s') => return self.skip_or_vote(),
            KeyCode::Char('l') => return self.open_library(),
            KeyCode::Char('+') | KeyCode::Char('=') => return self.change_volume(i16::from(VOLUME_STEP)),
            KeyCode::Char('-') => return self.change_volume(-i16::from(VOLUME_STEP)),
            KeyCode::Char('m') => return self.toggle_mute(),
            KeyCode::Char('f') if self.focus == Focus::Search => {
                if let Some(t) = self.search.state.selected().and_then(|i| self.search.results.get(i)).cloned() {
                    return self.toggle_like(t.uri, t.name);
                }
                return self.like_current();
            }
            KeyCode::Char('f') => return self.like_current(),
            KeyCode::Char('A') => {
                if !self.is_host() {
                    self.toast("L'autoplay est choisi par l'hôte", Level::Warn);
                } else if !self.can_play {
                    self.toast("L'autoplay nécessite ton compte Spotify (pas en mode spectateur)", Level::Warn);
                } else {
                    self.playlist_picker = Some(PlaylistPicker { loading: true, ..Default::default() });
                    return vec![Effect::LoadPlaylists];
                }
            }
            KeyCode::Char('p') => {
                if self.is_host() {
                    self.perm_panel = Some(PermPanel::default());
                } else {
                    self.toast("Le panneau des permissions est réservé à l'hôte", Level::Warn);
                }
            }
            KeyCode::Char('r') => {
                self.sync = ("resynchro demandée".into(), Level::Info);
                return vec![Effect::Resync];
            }
            KeyCode::Char('c') => {
                self.cover_mode = self.cover_mode.next(self.image_picker.is_some());
                self.toast(format!("Pochette : mode {}", self.cover_mode.label()), Level::Info);
            }
            KeyCode::Left => return self.seek_relative(-(SEEK_STEP_MS as i64)),
            KeyCode::Right => return self.seek_relative(SEEK_STEP_MS as i64),
            _ => return self.on_panel_key(key),
        }
        vec![]
    }

    fn on_panel_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match self.focus {
            Focus::Queue => match key.code {
                KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.queue_state, self.queue.len()),
                KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.queue_state, self.queue.len()),
                KeyCode::Char('K') => return self.move_selected(-1),
                KeyCode::Char('J') => return self.move_selected(1),
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(item) = self.queue_state.selected().and_then(|i| self.queue.get(i)) {
                        let (id, own) = (item.id, item.added_by_id == self.me);
                        if self.allowed(own || self.my_perms().queue_manage, "retirer les morceaux des autres") {
                            return vec![Effect::Send(ClientMsg::QueueRemove { id })];
                        }
                    }
                }
                _ => {}
            },
            Focus::Members => match key.code {
                KeyCode::Up | KeyCode::Char('k') => select_prev_list(&mut self.members_state, self.members.len()),
                KeyCode::Down | KeyCode::Char('j') => select_next_list(&mut self.members_state, self.members.len()),
                _ => {}
            },
            Focus::Search => match key.code {
                KeyCode::Up | KeyCode::Char('k') => select_prev(&mut self.search.state, self.search.results.len()),
                KeyCode::Down | KeyCode::Char('j') => select_next(&mut self.search.state, self.search.results.len()),
                KeyCode::Enter | KeyCode::Char('a') => return self.add_selected_result(),
                KeyCode::PageDown | KeyCode::Char(']') => return self.search_page(1),
                KeyCode::PageUp | KeyCode::Char('[') => return self.search_page(-1),
                _ => {}
            },
        }
        vec![]
    }

    fn on_search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match key.code {
            KeyCode::Esc => self.search.editing = false,
            KeyCode::Tab => {
                self.search.editing = false;
                self.focus = self.focus.next();
            }
            KeyCode::Enter => {
                // Leave the field to browse the results; search now if they
                // do not match the text yet.
                self.search.editing = false;
                self.search.due = None;
                let query = self.search.input.trim().to_owned();
                if !query.is_empty() && (query != self.search.query || self.search.offset != 0) {
                    return self.start_search(query, 0);
                }
            }
            KeyCode::Backspace => {
                self.search.input.pop();
                self.input_changed();
            }
            KeyCode::Down => {
                self.search.editing = false;
                self.search.due = None;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search.input.push(c);
                self.input_changed();
            }
            _ => {}
        }
        vec![]
    }

    pub fn on_mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        let pos = Position::new(m.column, m.row);
        if self.show_help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.show_help = false;
            }
            return vec![];
        }
        if self.perm_panel.is_some() {
            return self.on_perm_mouse(m);
        }
        if self.playlist_picker.is_some() {
            return self.on_picker_mouse(m);
        }
        if self.library.is_some() {
            return self.on_library_mouse(m);
        }
        let a = self.areas;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if a.prev_button.contains(pos) {
                    return self.seek_to(0);
                }
                if a.play_button.contains(pos) {
                    return self.toggle_play();
                }
                if a.next_button.contains(pos) {
                    return self.skip_or_vote();
                }
                if a.like_button.contains(pos) {
                    return self.like_current();
                }
                if a.volume_bar.contains(pos) && a.volume_bar.width > 1 && self.needs_account("Le volume") {
                    let frac = f64::from(pos.x - a.volume_bar.x) / f64::from(a.volume_bar.width - 1);
                    return self.set_volume((frac.clamp(0.0, 1.0) * 100.0).round() as u8);
                }
                if a.progress.contains(pos) && a.progress.width > 0 {
                    if let Some(item) = &self.playback.current {
                        let frac = f64::from(pos.x - a.progress.x) / f64::from(a.progress.width.saturating_sub(1).max(1));
                        return self.seek_to((frac.clamp(0.0, 1.0) * item.track.duration_ms as f64) as u64);
                    }
                    return vec![];
                }
                if let Some(i) = row_index(a.queue_rows, pos, self.queue_state.offset(), self.queue.len()) {
                    self.focus = Focus::Queue;
                    self.queue_state.select(Some(i));
                    // Only members who may reorder can drag.
                    self.drag_from = self.my_perms().queue_manage.then_some(i);
                    return vec![];
                }
                if let Some(i) = row_index(a.results_rows, pos, self.search.state.offset(), self.search.results.len()) {
                    self.focus = Focus::Search;
                    self.search.editing = false;
                    // A click on the selected result adds it.
                    if self.search.state.selected() == Some(i) {
                        return self.add_selected_result();
                    }
                    self.search.state.select(Some(i));
                    return vec![];
                }
                if let Some(i) = row_index(a.members_rows, pos, self.members_state.offset(), self.members.len()) {
                    self.focus = Focus::Members;
                    self.members_state.select(Some(i));
                    return vec![];
                }
                if a.search_input.contains(pos) {
                    self.focus = Focus::Search;
                    self.search.editing = true;
                } else if a.search.contains(pos) {
                    self.focus = Focus::Search;
                } else if a.queue.contains(pos) {
                    self.focus = Focus::Queue;
                } else if a.members.contains(pos) {
                    self.focus = Focus::Members;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(from) = self.drag_from.take() {
                    // Dropping outside the rows moves to the closest end.
                    let to = row_index(a.queue_rows, pos, self.queue_state.offset(), self.queue.len()).or_else(|| {
                        let rows = a.queue_rows;
                        (a.queue.contains(pos) && pos.y >= rows.y).then(|| self.queue.len().saturating_sub(1))
                    });
                    if let Some(to) = to.filter(|to| *to != from) {
                        return self.move_item(from, to);
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = m.kind == MouseEventKind::ScrollDown;
                if a.volume_bar.contains(pos) {
                    return self.change_volume(if down { -i16::from(VOLUME_STEP) } else { i16::from(VOLUME_STEP) });
                }
                if a.queue.contains(pos) {
                    scroll(&mut self.queue_state, self.queue.len(), down);
                } else if a.search.contains(pos) {
                    scroll(&mut self.search.state, self.search.results.len(), down);
                } else if a.members.contains(pos) {
                    if down {
                        select_next_list(&mut self.members_state, self.members.len());
                    } else {
                        select_prev_list(&mut self.members_state, self.members.len());
                    }
                }
            }
            _ => {}
        }
        vec![]
    }

    // -------------------------------------------------------- actions

    fn toggle_play(&mut self) -> Vec<Effect> {
        if !self.allowed(self.my_perms().playback, "contrôler la lecture") {
            return vec![];
        }
        let msg = if self.playback.is_playing { ClientMsg::Pause } else { ClientMsg::Play };
        vec![Effect::Send(msg)]
    }

    fn seek_to(&mut self, position_ms: u64) -> Vec<Effect> {
        if self.playback.current.is_none() || !self.allowed(self.my_perms().playback, "contrôler la lecture") {
            return vec![];
        }
        vec![Effect::Send(ClientMsg::Seek { position_ms })]
    }

    fn seek_relative(&mut self, delta_ms: i64) -> Vec<Effect> {
        let target = (self.position_ms() as i64 + delta_ms).max(0) as u64;
        self.seek_to(target)
    }

    /// Direct skip with the permission, otherwise a vote.
    fn skip_or_vote(&mut self) -> Vec<Effect> {
        let perms = self.my_perms();
        if perms.skip {
            return vec![Effect::Send(ClientMsg::Skip)];
        }
        if !self.allowed(perms.listen, "voter pour passer le morceau") || self.playback.current.is_none() {
            return vec![];
        }
        self.toast("Vote pour passer le morceau enregistré", Level::Info);
        vec![Effect::Send(ClientMsg::VoteSkip)]
    }

    fn move_selected(&mut self, delta: isize) -> Vec<Effect> {
        if !self.allowed(self.my_perms().queue_manage, "réordonner la file") {
            return vec![];
        }
        let Some(from) = self.queue_state.selected() else { return vec![] };
        let to = from.saturating_add_signed(delta).min(self.queue.len().saturating_sub(1));
        if to == from { vec![] } else { self.move_item(from, to) }
    }

    /// Reorders locally right away (the server echo confirms it), so the
    /// selection never points at the wrong item in between.
    fn move_item(&mut self, from: usize, to: usize) -> Vec<Effect> {
        if from >= self.queue.len() {
            return vec![];
        }
        let item = self.queue.remove(from);
        let to = to.min(self.queue.len());
        let id = item.id;
        self.queue.insert(to, item);
        self.queue_state.select(Some(to));
        vec![Effect::Send(ClientMsg::QueueMove { id, index: to })]
    }

    /// The search text changed: schedule a search, or clear the results.
    fn input_changed(&mut self) {
        if self.search.input.trim().is_empty() {
            self.search.due = None;
            self.search.request += 1; // drop answers still in flight
            self.search.loading = false;
            self.search.query.clear();
            self.search.results.clear();
            self.search.state.select(None);
            self.search.total = 0;
        } else {
            self.search.due = Some(Instant::now() + SEARCH_DEBOUNCE);
        }
    }

    /// Deadline of the pending search-as-you-type request, if any.
    pub fn search_due(&self) -> Option<Instant> {
        self.search.due
    }

    /// Runs the pending search once its delay is over.
    pub fn on_search_due(&mut self) -> Vec<Effect> {
        if self.search.due.is_none_or(|t| Instant::now() < t) {
            return vec![];
        }
        self.search.due = None;
        let query = self.search.input.trim().to_owned();
        if query.is_empty() || (query == self.search.query && self.search.offset == 0) {
            return vec![];
        }
        self.start_search(query, 0)
    }

    fn start_search(&mut self, query: String, offset: u32) -> Vec<Effect> {
        if !self.can_play {
            self.toast("La recherche nécessite un compte Spotify (mode spectateur)", Level::Warn);
            return vec![];
        }
        self.search.request += 1;
        self.search.loading = true;
        self.search.query = query.clone();
        vec![Effect::Search { request: self.search.request, query, offset }]
    }

    fn search_page(&mut self, direction: i32) -> Vec<Effect> {
        if self.search.query.is_empty() {
            return vec![];
        }
        let offset = if direction > 0 {
            self.search.offset + SEARCH_PAGE
        } else {
            self.search.offset.saturating_sub(SEARCH_PAGE)
        };
        if offset == self.search.offset || offset >= self.search.total {
            return vec![];
        }
        self.start_search(self.search.query.clone(), offset)
    }

    fn add_selected_result(&mut self) -> Vec<Effect> {
        if !self.allowed(self.my_perms().queue_add, "ajouter des morceaux") {
            return vec![];
        }
        let Some(track) = self.search.state.selected().and_then(|i| self.search.results.get(i)) else {
            return vec![];
        };
        let text = format!("Ajouté : {} — {}", track.name, track.artist_names());
        let msg = ClientMsg::QueueAdd { track: client::track_info(track) };
        self.toast(text, Level::Good);
        vec![Effect::Send(msg)]
    }

    /// Media keys / `playerctl`, with the same permission checks.
    pub fn on_mpris(&mut self, cmd: crate::mpris::Command) -> Vec<Effect> {
        use crate::mpris::Command;
        match cmd {
            Command::PlayPause => self.toggle_play(),
            Command::Play if !self.playback.is_playing => self.toggle_play(),
            Command::Pause if self.playback.is_playing => self.toggle_play(),
            Command::Play | Command::Pause => vec![],
            Command::Next => self.skip_or_vote(),
            Command::Seek(offset_ms) => self.seek_relative(offset_ms),
            Command::SetPosition(ms) => self.seek_to(ms),
        }
    }

    // ------------------------------------------------------------ library

    fn open_library(&mut self) -> Vec<Effect> {
        if !self.can_play {
            self.toast("La bibliothèque nécessite ton compte Spotify (pas en mode spectateur)", Level::Warn);
            return vec![];
        }
        let mut library = crate::library::Library { loading: true, ..Default::default() };
        library.state.select(Some(0));
        self.library = Some(library);
        vec![Effect::LoadLibrary]
    }

    pub fn on_library(&mut self, result: Result<Vec<PlaylistSummary>, String>) {
        let Some(library) = self.library.as_mut() else { return };
        library.loading = false;
        match result {
            Ok(playlists) => library.playlists = playlists,
            Err(e) => self.toast(format!("Playlists : {e}"), Level::Bad),
        }
    }

    pub fn on_library_list(&mut self, key: String, result: Result<Vec<Track>, String>) {
        let Some(open) = self.library.as_mut().and_then(|l| l.open.as_mut()).filter(|o| o.key == key) else {
            return; // closed or another list opened meanwhile
        };
        open.loading = false;
        match result {
            Ok(tracks) => {
                open.state.select((!tracks.is_empty()).then_some(0));
                let liked_list = open.key == "liked";
                let uris: Vec<String> = tracks.iter().map(|t| t.uri.clone()).collect();
                open.tracks = tracks;
                if liked_list {
                    self.liked.extend(uris.into_iter().map(|u| (u, true)));
                }
            }
            Err(e) => {
                let text = format!("« {} » : {e}", open.name);
                if let Some(library) = self.library.as_mut() {
                    library.open = None;
                }
                self.toast(text, Level::Bad);
            }
        }
    }

    fn open_library_row(&mut self) -> Vec<Effect> {
        let Some(library) = self.library.as_mut() else { return vec![] };
        let Some((source, name, total)) = library.selected_row() else { return vec![] };
        library.filter.editing = false;
        library.open = Some(crate::library::OpenList {
            name: name.clone(),
            key: source.key().to_owned(),
            total,
            loading: true,
            ..Default::default()
        });
        vec![Effect::LoadLibraryList { source, name }]
    }

    /// Typing in a library filter; returns true when the key was used.
    fn edit_filter(filter: &mut crate::library::Filter, state_select: &mut dyn FnMut(Option<usize>), key: KeyEvent) -> bool {
        if !filter.editing {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                filter.text.clear();
                filter.editing = false;
            }
            KeyCode::Enter | KeyCode::Down | KeyCode::Tab => filter.editing = false,
            KeyCode::Backspace => {
                filter.text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => filter.text.push(c),
            _ => return true,
        }
        // The visible rows changed: back to the first one.
        state_select(Some(0));
        true
    }

    fn on_library_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        use crate::library::step;
        let Some(library) = self.library.as_mut() else { return vec![] };
        let delta = match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(-1),
            KeyCode::Down | KeyCode::Char('j') => Some(1),
            KeyCode::PageUp => Some(-10),
            KeyCode::PageDown => Some(10),
            _ => None,
        };
        if let Some(open) = library.open.as_mut() {
            let state = &mut open.state;
            if Self::edit_filter(&mut open.filter, &mut |s| state.select(s), key) {
                return vec![];
            }
            if let Some(d) = delta {
                let len = open.visible().len();
                open.state.select(step(open.state.selected(), len, d));
                return vec![];
            }
            return match key.code {
                KeyCode::Char('/') => {
                    open.filter.editing = true;
                    vec![]
                }
                // Esc first clears an active filter, then goes back.
                KeyCode::Esc if open.filter.is_active() => {
                    open.filter.text.clear();
                    open.state.select(Some(0));
                    vec![]
                }
                KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                    library.open = None;
                    vec![]
                }
                KeyCode::Char('l') | KeyCode::Char('q') => {
                    self.library = None;
                    vec![]
                }
                KeyCode::Enter | KeyCode::Char('a') => self.add_library_track(),
                KeyCode::Char('t') => self.add_library_list(AddMode::End, false),
                KeyCode::Char('f') => match open.selected_track().cloned() {
                    Some(t) => self.toggle_like(t.uri, t.name),
                    None => vec![],
                },
                KeyCode::Char('p') => self.add_library_list(AddMode::PlayNow, false),
                KeyCode::Char('s') => self.add_library_list(AddMode::PlayNow, true),
                _ => vec![],
            };
        }
        let state = &mut library.state;
        if Self::edit_filter(&mut library.filter, &mut |s| state.select(s), key) {
            return vec![];
        }
        if let Some(d) = delta {
            let len = library.visible().len();
            library.state.select(step(library.state.selected(), len, d));
            return vec![];
        }
        match key.code {
            KeyCode::Char('/') => {
                library.filter.editing = true;
                vec![]
            }
            KeyCode::Esc if library.filter.is_active() => {
                library.filter.text.clear();
                library.state.select(Some(0));
                vec![]
            }
            KeyCode::Esc | KeyCode::Char('l') | KeyCode::Char('q') => {
                self.library = None;
                vec![]
            }
            KeyCode::Enter | KeyCode::Right => self.open_library_row(),
            _ => vec![],
        }
    }

    fn on_library_mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        use crate::library::step;
        let pos = Position::new(m.column, m.row);
        let a = self.areas;
        let Some(library) = self.library.as_mut() else { return vec![] };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if !a.library_popup.contains(pos) => self.library = None,
            MouseEventKind::Down(MouseButton::Left) => {
                // A click on the selected row opens the list / adds the track.
                if let Some(open) = library.open.as_mut() {
                    let len = open.visible().len();
                    if let Some(i) = row_index(a.library_rows, pos, open.state.offset(), len) {
                        if open.state.selected() == Some(i) {
                            return self.add_library_track();
                        }
                        open.state.select(Some(i));
                    }
                } else {
                    let len = library.visible().len();
                    if let Some(i) = row_index(a.library_rows, pos, library.state.offset(), len) {
                        if library.state.selected() == Some(i) {
                            return self.open_library_row();
                        }
                        library.state.select(Some(i));
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                match library.open.as_mut() {
                    Some(open) => {
                        let len = open.visible().len();
                        open.state.select(step(open.state.selected(), len, d));
                    }
                    None => {
                        let len = library.visible().len();
                        library.state.select(step(library.state.selected(), len, d));
                    }
                }
            }
            _ => {}
        }
        vec![]
    }

    fn add_library_track(&mut self) -> Vec<Effect> {
        let track = self.library.as_ref().and_then(|l| l.open.as_ref()).and_then(|o| o.selected_track()).cloned();
        let Some(track) = track else { return vec![] };
        if !self.allowed(self.my_perms().queue_add, "ajouter des morceaux") {
            return vec![];
        }
        self.toast(format!("Ajouté : {} — {}", track.name, track.artist_names()), Level::Good);
        vec![Effect::Send(ClientMsg::QueueAdd { track: client::track_info(&track) })]
    }

    /// Whole list (or the tracks matching its filter): appended, or played
    /// right away (in order or shuffled).
    fn add_library_list(&mut self, mode: AddMode, shuffle: bool) -> Vec<Effect> {
        let perms = self.my_perms();
        if !self.allowed(perms.queue_manage, "ajouter une liste entière (ajoute les titres un par un)") {
            return vec![];
        }
        if mode == AddMode::PlayNow && !self.allowed(perms.playback, "lancer une liste à la place de la lecture en cours") {
            return vec![];
        }
        let Some(open) = self.library.as_ref().and_then(|l| l.open.as_ref()).filter(|o| !o.loading) else {
            return vec![];
        };
        let name = match open.filter.is_active() {
            true => format!("{} (filtre « {} »)", open.name, open.filter.text.trim()),
            false => open.name.clone(),
        };
        let mut tracks: Vec<_> = open.visible_tracks().into_iter().map(client::track_info).collect();
        if tracks.is_empty() {
            self.toast(format!("« {name} » : aucun morceau jouable"), Level::Warn);
            return vec![];
        }
        if shuffle {
            use rand::seq::SliceRandom;
            tracks.shuffle(&mut rand::rng());
        }
        let text = match (mode, shuffle) {
            (AddMode::End, _) => format!("« {name} » ajoutée à la file ({} titres)", tracks.len()),
            (AddMode::PlayNow, false) => format!("Lecture de « {name} »"),
            (AddMode::PlayNow, true) => format!("Lecture de « {name} » en aléatoire"),
        };
        self.toast(text, Level::Good);
        if mode == AddMode::PlayNow {
            self.library = None;
        }
        vec![Effect::Send(ClientMsg::QueueAddMany { tracks, mode })]
    }

    // ---------------------------------------------------- autoplay picker

    pub fn on_playlists(&mut self, result: Result<Vec<PlaylistSummary>, String>) {
        let Some(picker) = self.playlist_picker.as_mut() else { return };
        picker.loading = false;
        match result {
            Ok(playlists) => {
                picker.state.select((!playlists.is_empty()).then_some(0));
                picker.playlists = playlists;
            }
            Err(e) => {
                self.playlist_picker = None;
                self.toast(format!("Playlists : {e}"), Level::Bad);
            }
        }
    }

    pub fn on_playlist_tracks(&mut self, name: String, result: Result<Vec<Track>, String>) -> Vec<Effect> {
        match result {
            Ok(tracks) if tracks.is_empty() => {
                self.toast(format!("« {name} » ne contient aucun morceau jouable"), Level::Warn);
                vec![]
            }
            Ok(tracks) => {
                self.toast(format!("Autoplay : {name} ({} morceaux)", tracks.len()), Level::Good);
                let tracks = tracks.iter().map(client::track_info).collect();
                vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetAutoplay { name, tracks }))]
            }
            Err(e) => {
                self.toast(format!("Playlist « {name} » : {e}"), Level::Bad);
                vec![]
            }
        }
    }

    fn on_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(picker) = self.playlist_picker.as_mut() else { return vec![] };
        let len = picker.playlists.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('A') | KeyCode::Char('q') => self.playlist_picker = None,
            KeyCode::Up | KeyCode::Char('k') => select_prev_list(&mut picker.state, len),
            KeyCode::Down | KeyCode::Char('j') => select_next_list(&mut picker.state, len),
            KeyCode::Enter => return self.choose_playlist(),
            KeyCode::Char('o') => {
                self.playlist_picker = None;
                self.toast("Autoplay désactivé", Level::Info);
                let off = AdminCmd::SetAutoplay { name: String::new(), tracks: vec![] };
                return vec![Effect::Send(ClientMsg::Admin(off))];
            }
            _ => {}
        }
        vec![]
    }

    fn on_picker_mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        let pos = Position::new(m.column, m.row);
        let a = self.areas;
        let Some(picker) = self.playlist_picker.as_mut() else { return vec![] };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if !a.picker_popup.contains(pos) => self.playlist_picker = None,
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(i) = row_index(a.picker_rows, pos, picker.state.offset(), picker.playlists.len()) {
                    if picker.state.selected() == Some(i) {
                        return self.choose_playlist();
                    }
                    picker.state.select(Some(i));
                }
            }
            MouseEventKind::ScrollDown => select_next_list(&mut picker.state, picker.playlists.len()),
            MouseEventKind::ScrollUp => select_prev_list(&mut picker.state, picker.playlists.len()),
            _ => {}
        }
        vec![]
    }

    fn choose_playlist(&mut self) -> Vec<Effect> {
        let Some(picker) = self.playlist_picker.take() else { return vec![] };
        let Some(p) = picker.state.selected().and_then(|i| picker.playlists.get(i)) else { return vec![] };
        self.toast(format!("Chargement de « {} »…", p.name), Level::Info);
        vec![Effect::LoadPlaylistTracks { id: p.id.clone(), name: p.name.clone() }]
    }

    // ------------------------------------------------- permissions popup

    fn on_perm_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let rows = self.panel_members().len();
        let Some(panel) = self.perm_panel.as_mut() else { return vec![] };
        if key.code != KeyCode::Char('b') {
            panel.confirm_ban = None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('p') | KeyCode::Char('q') => self.perm_panel = None,
            KeyCode::Up | KeyCode::Char('k') => panel.row = panel.row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => panel.row = (panel.row + 1).min(rows.saturating_sub(1)),
            KeyCode::Left | KeyCode::Char('h') => panel.col = panel.col.saturating_sub(1),
            KeyCode::Right | KeyCode::Char('l') => panel.col = (panel.col + 1).min(PERM_COLUMNS - 1),
            KeyCode::Char(' ') | KeyCode::Enter => return self.toggle_cell(),
            KeyCode::Char('r') => return self.cycle_role(),
            KeyCode::Char('x') => return self.reset_override(),
            KeyCode::Char('a') => return self.admin_on_selected(|id| AdminCmd::Admit { member: id }),
            KeyCode::Char('K') => return self.admin_on_selected(|id| AdminCmd::Kick { member: id }),
            KeyCode::Char('b') => return self.ban_selected(),
            KeyCode::Char('w') => {
                return vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetWaitingRoom { enabled: !self.settings.waiting_room }))];
            }
            KeyCode::Char('D') => {
                let role = self.settings.default_role.next_assignable();
                return vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetDefaultRole { role }))];
            }
            KeyCode::Char('n') => return vec![Effect::Send(ClientMsg::Admin(AdminCmd::RotateCode))],
            _ => {}
        }
        vec![]
    }

    fn on_perm_mouse(&mut self, m: MouseEvent) -> Vec<Effect> {
        let pos = Position::new(m.column, m.row);
        let a = self.areas;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if !a.perm_popup.contains(pos) => self.perm_panel = None,
            MouseEventKind::Down(MouseButton::Left) => {
                let rows = self.panel_members().len();
                let Some(row) = row_index(a.perm_rows, pos, 0, rows) else { return vec![] };
                let Some(col) = a.perm_cols.iter().position(|(x, w)| pos.x >= *x && pos.x < x + w) else {
                    return vec![];
                };
                if let Some(panel) = self.perm_panel.as_mut() {
                    panel.row = row;
                    panel.col = col;
                }
                return self.toggle_cell();
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let rows = self.panel_members().len();
                if let Some(panel) = self.perm_panel.as_mut() {
                    panel.row = if m.kind == MouseEventKind::ScrollDown {
                        (panel.row + 1).min(rows.saturating_sub(1))
                    } else {
                        panel.row.saturating_sub(1)
                    };
                }
            }
            _ => {}
        }
        vec![]
    }

    /// Member under the popup cursor, unless it is the host (read-only row).
    fn selected_target(&mut self) -> Option<Member> {
        let row = self.perm_panel.as_ref()?.row;
        let member = self.panel_members().get(row).map(|m| (*m).clone())?;
        if member.role == Role::Host {
            self.toast("L'hôte a tous les droits", Level::Info);
            return None;
        }
        Some(member)
    }

    fn toggle_cell(&mut self) -> Vec<Effect> {
        let col = self.perm_panel.as_ref().map_or(0, |p| p.col);
        if col == 0 {
            return self.cycle_role();
        }
        let Some(member) = self.selected_target() else { return vec![] };
        let perm = Perm::ALL[col - 1];
        let value = Some(!member.perms.get(perm));
        vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetPerm { member: member.id, perm, value }))]
    }

    fn cycle_role(&mut self) -> Vec<Effect> {
        let Some(member) = self.selected_target() else { return vec![] };
        let role = member.role.next_assignable();
        vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetRole { member: member.id, role }))]
    }

    fn reset_override(&mut self) -> Vec<Effect> {
        let col = self.perm_panel.as_ref().map_or(0, |p| p.col);
        let Some(member) = self.selected_target().filter(|_| col > 0) else { return vec![] };
        let perm = Perm::ALL[col - 1];
        vec![Effect::Send(ClientMsg::Admin(AdminCmd::SetPerm { member: member.id, perm, value: None }))]
    }

    fn admin_on_selected(&mut self, cmd: impl FnOnce(MemberId) -> AdminCmd) -> Vec<Effect> {
        let Some(member) = self.selected_target() else { return vec![] };
        vec![Effect::Send(ClientMsg::Admin(cmd(member.id)))]
    }

    fn ban_selected(&mut self) -> Vec<Effect> {
        let Some(member) = self.selected_target() else { return vec![] };
        let confirmed = self.perm_panel.as_ref().and_then(|p| p.confirm_ban) == Some(member.id);
        if let Some(panel) = self.perm_panel.as_mut() {
            panel.confirm_ban = if confirmed { None } else { Some(member.id) };
        }
        if confirmed {
            return vec![Effect::Send(ClientMsg::Admin(AdminCmd::Ban { member: member.id }))];
        }
        self.toast(format!("Appuie encore sur b pour bannir {}", member.name), Level::Warn);
        vec![]
    }

    fn clamp_selections(&mut self) {
        let rows = self.panel_members().len();
        if let Some(panel) = self.perm_panel.as_mut() {
            panel.row = panel.row.min(rows.saturating_sub(1));
        }
        clamp(&mut self.queue_state, self.queue.len());
        let sel = self.members_state.selected();
        let len = self.members.len();
        self.members_state.select(if len == 0 { None } else { Some(sel.unwrap_or(0).min(len - 1)) });
    }
}

/// Index of the list row under `pos`, if any.
fn row_index(rows: Rect, pos: Position, offset: usize, len: usize) -> Option<usize> {
    if !rows.contains(pos) {
        return None;
    }
    let i = offset + usize::from(pos.y - rows.y);
    (i < len).then_some(i)
}

fn clamp(state: &mut TableState, len: usize) {
    state.select(if len == 0 { None } else { Some(state.selected().unwrap_or(0).min(len - 1)) });
}

fn select_next(state: &mut TableState, len: usize) {
    if len > 0 {
        state.select(Some(state.selected().map_or(0, |i| (i + 1).min(len - 1))));
    }
}

fn select_prev(state: &mut TableState, len: usize) {
    if len > 0 {
        state.select(Some(state.selected().map_or(0, |i| i.saturating_sub(1))));
    }
}

fn select_next_list(state: &mut ListState, len: usize) {
    if len > 0 {
        state.select(Some(state.selected().map_or(0, |i| (i + 1).min(len - 1))));
    }
}

fn select_prev_list(state: &mut ListState, len: usize) {
    if len > 0 {
        state.select(Some(state.selected().map_or(0, |i| i.saturating_sub(1))));
    }
}

fn scroll(state: &mut TableState, len: usize, down: bool) {
    if down { select_next(state, len) } else { select_prev(state, len) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{RoomSnapshot, TrackInfo};

    fn member(id: MemberId, name: &str, role: Role) -> Member {
        Member { id, name: name.into(), role, perms: role.defaults(), overrides: Default::default(), pending: false, online: true }
    }

    fn item(id: u64) -> QueueItem {
        QueueItem {
            id,
            track: TrackInfo {
                uri: format!("spotify:track:{id}"),
                name: format!("T{id}"),
                artists: "A".into(),
                duration_ms: 100_000,
                album_uri: None,
                cover_url: Some(format!("https://i.scdn.co/image/{id}")),
            },
            added_by: "Alex".into(),
            added_by_id: 1,
        }
    }

    fn app() -> App {
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        drop(tx);
        let joined = client::Joined {
            member_id: 1,
            room: RoomSnapshot {
                settings: RoomSettings { code: "K7XQ2M".into(), waiting_room: false, default_role: Role::Listener, autoplay: None },
                playback: Playback { seq: 1, current: Some(item(1)), position_ms: 0, server_ms: 0, is_playing: true },
                queue: vec![item(2), item(3), item(4)],
                members: vec![member(1, "Alex", Role::Host), member(2, "Lou", Role::Listener)],
            },
            handle: client::RoomHandle::detached(),
            events,
        };
        App::new(&joined, true, Theme::default())
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn sent(effects: &[Effect]) -> Vec<&ClientMsg> {
        effects.iter().filter_map(|e| if let Effect::Send(m) = e { Some(m) } else { None }).collect()
    }

    #[test]
    fn keyboard_controls_playback_and_queue() {
        let mut a = app();
        assert_eq!(sent(&a.on_key(key(KeyCode::Char(' ')))), [&ClientMsg::Pause]);
        assert_eq!(sent(&a.on_key(key(KeyCode::Char('s')))), [&ClientMsg::Skip]);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.queue_state.selected(), Some(1));
        assert_eq!(sent(&a.on_key(key(KeyCode::Char('K')))), [&ClientMsg::QueueMove { id: 3, index: 0 }]);
        assert_eq!(sent(&a.on_key(key(KeyCode::Char('d')))), [&ClientMsg::QueueRemove { id: 3 }]);
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), [Effect::Quit]);
    }

    #[test]
    fn search_typing_then_enter_requests_search() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('/')));
        for c in "daft".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        // 'q' is typed, not quit, while editing.
        a.on_key(key(KeyCode::Char('q')));
        assert_eq!(a.search.input, "daftq");
        a.on_key(key(KeyCode::Backspace));
        let fx = a.on_key(key(KeyCode::Enter));
        assert_eq!(fx, [Effect::Search { request: 1, query: "daft".into(), offset: 0 }]);
        assert!(!a.search.editing);
    }

    #[test]
    fn search_as_you_type_waits_for_a_pause() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('/')));
        for c in "daf".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        // Too early: still typing.
        assert!(a.on_search_due().is_empty());
        assert!(a.search_due().is_some());

        // After the pause, one request for the whole text.
        a.search.due = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(a.on_search_due(), [Effect::Search { request: 1, query: "daf".into(), offset: 0 }]);
        assert!(a.search.editing, "still typing");
        assert!(a.on_search_due().is_empty(), "nothing pending");

        // Results for that text: Enter only leaves the field.
        a.on_search(SearchOutcome { request: 1, offset: 0, result: Ok((vec![Track::default()], 1)) });
        assert!(a.on_key(key(KeyCode::Enter)).is_empty());
        assert!(!a.search.editing);

        // Clearing the text clears the results and ignores late answers.
        a.on_key(key(KeyCode::Char('/')));
        for _ in 0..3 {
            a.on_key(key(KeyCode::Backspace));
        }
        assert!(a.search.results.is_empty() && a.search_due().is_none());
        a.on_search(SearchOutcome { request: 1, offset: 0, result: Ok((vec![Track::default()], 1)) });
        assert!(a.search.results.is_empty());
    }

    #[test]
    fn stale_search_results_are_ignored() {
        let mut a = app();
        a.search.request = 2;
        a.on_search(SearchOutcome { request: 1, offset: 0, result: Ok((vec![Track::default()], 1)) });
        assert!(a.search.results.is_empty());
        a.on_search(SearchOutcome { request: 2, offset: 0, result: Ok((vec![Track::default()], 1)) });
        assert_eq!(a.search.state.selected(), Some(0));
    }

    #[test]
    fn mouse_buttons_progress_and_drag() {
        let mut a = app();
        a.areas.play_button = Rect::new(10, 5, 6, 1);
        a.areas.progress = Rect::new(0, 3, 101, 1);
        a.areas.queue = Rect::new(0, 10, 40, 8);
        a.areas.queue_rows = Rect::new(1, 12, 38, 5);
        let click = |x, y, kind| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };

        assert_eq!(sent(&a.on_mouse(click(12, 5, MouseEventKind::Down(MouseButton::Left)))), [&ClientMsg::Pause]);
        assert_eq!(
            sent(&a.on_mouse(click(50, 3, MouseEventKind::Down(MouseButton::Left)))),
            [&ClientMsg::Seek { position_ms: 50_000 }]
        );
        // Drag the third queue row onto the first.
        assert!(a.on_mouse(click(5, 14, MouseEventKind::Down(MouseButton::Left))).is_empty());
        assert_eq!(
            sent(&a.on_mouse(click(5, 12, MouseEventKind::Up(MouseButton::Left)))),
            [&ClientMsg::QueueMove { id: 4, index: 0 }]
        );
    }

    #[test]
    fn new_track_triggers_cover_fetch_and_disconnect_quits() {
        let mut a = app();
        let mut p = a.playback.clone();
        p.seq = 2;
        p.current = Some(item(9));
        let fx = a.on_room_event(Event::Server(Box::new(ServerMsg::Playback(p))));
        assert_eq!(fx, [Effect::CheckLiked(vec!["spotify:track:9".into()]), Effect::FetchCover("https://i.scdn.co/image/9".into())]);
        assert_eq!(a.on_room_event(Event::Disconnected("bye".into())), [Effect::Quit]);
        assert_eq!(a.exit_message.as_deref(), Some("bye"));
    }

    #[test]
    fn renders_main_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.playback.position_ms = 61_000;
        a.playback.server_ms = a.server_now();
        a.playback.is_playing = true;
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(64, 64, |x, y| {
            let d = ((x as i32 - 32).pow(2) + (y as i32 - 24).pow(2)) as f32;
            let v = (255.0 * (1.0 - (d / 900.0).min(1.0))) as u8;
            image::Rgba([v, v / 2, 40, 255])
        }));
        a.cover = Some(("https://i.scdn.co/image/1".into(), img));
        a.search.input = "daft punk".into();
        a.search.results = vec![Track { uri: "spotify:track:x".into(), name: "Harder, Better, Faster, Stronger".into(), duration_ms: 224_000, ..Default::default() }];
        a.search.state.select(Some(0));
        a.sync = ("synchro".into(), Level::Good);

        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
        for needle in ["En cours", "File d'attente · 3", "Membres · 2", "Recherche", "K7XQ2M", "1:01", "1:40"] {
            assert!(text.contains(needle), "{needle} missing");
        }
        // Clickable regions were recorded.
        assert!(a.areas.play_button.width == 6 && a.areas.progress.width > 20);
        assert!(a.areas.queue_rows.height >= 3);
    }

    #[test]
    fn listener_actions_are_gated_and_skip_becomes_a_vote() {
        let mut a = app();
        a.me = 2; // Lou, listener
        assert!(a.on_key(key(KeyCode::Char(' '))).is_empty());
        assert!(a.active_toast().unwrap().0.contains("contrôler la lecture"));
        assert_eq!(sent(&a.on_key(key(KeyCode::Char('s')))), [&ClientMsg::VoteSkip]);
        assert!(a.on_key(key(KeyCode::Char('K'))).is_empty(), "no reordering");
        // Queue items were added by Alex: Lou cannot remove them.
        assert!(a.on_key(key(KeyCode::Char('d'))).is_empty());
        assert!(a.on_key(key(KeyCode::Char('p'))).is_empty());
        assert!(a.perm_panel.is_none());
    }

    #[test]
    fn host_edits_permissions_from_the_panel() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('p')));
        assert!(a.perm_panel.is_some());
        // Row 0 is the host: read-only.
        assert!(a.on_key(key(KeyCode::Char(' '))).is_empty());
        a.on_key(key(KeyCode::Down)); // Lou
        assert_eq!(
            sent(&a.on_key(key(KeyCode::Char(' ')))),
            [&ClientMsg::Admin(AdminCmd::SetRole { member: 2, role: Role::Spectator })]
        );
        a.on_key(key(KeyCode::Right));
        a.on_key(key(KeyCode::Right)); // "Ajout"
        assert_eq!(
            sent(&a.on_key(key(KeyCode::Enter))),
            [&ClientMsg::Admin(AdminCmd::SetPerm { member: 2, perm: Perm::QueueAdd, value: Some(false) })]
        );
        assert_eq!(
            sent(&a.on_key(key(KeyCode::Char('x')))),
            [&ClientMsg::Admin(AdminCmd::SetPerm { member: 2, perm: Perm::QueueAdd, value: None })]
        );
        // Ban needs two presses.
        assert!(a.on_key(key(KeyCode::Char('b'))).is_empty());
        assert_eq!(sent(&a.on_key(key(KeyCode::Char('b')))), [&ClientMsg::Admin(AdminCmd::Ban { member: 2 })]);
        a.on_key(key(KeyCode::Esc));
        assert!(a.perm_panel.is_none());
    }

    #[test]
    fn renders_permissions_panel_and_maps_clicks() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut a = app();
        a.members[1].overrides.insert(Perm::QueueAdd, false);
        a.members[1].perms.queue_add = false;
        a.members.push(Member { pending: true, ..member(3, "Max", Role::Listener) });
        a.on_key(key(KeyCode::Char('p')));
        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
        for needle in ["Permissions · salon K7XQ2M", "Écoute", "[ ]*", "[v]", "en attente", "Salle d'attente : non"] {
            assert!(text.contains(needle), "{needle} missing");
        }
        // Click Lou's "Skip" cell -> grant skip.
        let row = a.areas.perm_rows.y + 1;
        let (x, _) = a.areas.perm_cols[5];
        let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x + 1, row, modifiers: KeyModifiers::NONE };
        assert_eq!(
            sent(&a.on_mouse(click)),
            [&ClientMsg::Admin(AdminCmd::SetPerm { member: 2, perm: Perm::Skip, value: Some(true) })]
        );
    }

    #[test]
    fn host_picks_an_autoplay_playlist() {
        let mut a = app();
        assert_eq!(a.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)), [Effect::LoadPlaylists]);
        let playlist = |id: &str, name: &str| -> PlaylistSummary {
            serde_json::from_value(serde_json::json!({"id": id, "name": name, "items": {"total": 3}})).unwrap()
        };
        a.on_playlists(Ok(vec![playlist("p1", "Soirée"), playlist("p2", "Chill")]));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.on_key(key(KeyCode::Enter)), [Effect::LoadPlaylistTracks { id: "p2".into(), name: "Chill".into() }]);
        assert!(a.playlist_picker.is_none());
        let track = Track { uri: "spotify:track:x".into(), name: "X".into(), duration_ms: 1000, ..Default::default() };
        let fx = a.on_playlist_tracks("Chill".into(), Ok(vec![track]));
        assert!(matches!(sent(&fx)[..], [ClientMsg::Admin(AdminCmd::SetAutoplay { name, tracks })] if name == "Chill" && tracks.len() == 1));

        // Listeners cannot open it.
        a.me = 2;
        assert!(a.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)).is_empty());
    }

    #[test]
    fn mpris_commands_and_diff() {
        use crate::mpris::{Command, Shown, Update, diff};
        let mut a = app();
        assert_eq!(sent(&a.on_mpris(Command::PlayPause)), [&ClientMsg::Pause]);
        assert!(a.on_mpris(Command::Play).is_empty(), "already playing");
        assert_eq!(sent(&a.on_mpris(Command::Next)), [&ClientMsg::Skip]);

        let mut shown = Shown::default();
        let first = diff(&mut shown, &a);
        assert!(matches!(&first[0], Update::Track { title, .. } if title == "T1"));
        assert!(first.contains(&Update::Status { playing: true }));
        assert!(diff(&mut shown, &a).is_empty(), "nothing changed");
        a.playback.seq += 1;
        a.playback.is_playing = false;
        let next = diff(&mut shown, &a);
        assert!(matches!(next[0], Update::Seeked(_)));
        assert!(next.contains(&Update::Status { playing: false }));

        a.me = 2; // listener: no playback control
        assert!(a.on_mpris(Command::PlayPause).is_empty());
        assert!(diff(&mut shown, &a).contains(&Update::Controls { playback: false, skip: true }));
    }

    #[test]
    fn host_quit_needs_confirmation_and_i_copies_the_invite() {
        let mut a = app();
        a.hosting = true;
        a.invite = Some("spotitogether://192.168.1.19:7420/K7XQ2M".into());
        assert!(a.on_key(key(KeyCode::Char('q'))).is_empty());
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), [Effect::Quit]);

        a.settings.code = "NEW123".into();
        assert_eq!(a.on_key(key(KeyCode::Char('i'))), [Effect::CopyInvite("spotitogether://192.168.1.19:7420/NEW123".into())]);
        a.hosting = false;
        assert_eq!(a.on_key(key(KeyCode::Char('q'))), [Effect::Quit], "guests quit at once");
    }

    #[test]
    fn library_queues_tracks_or_plays_whole_lists() {
        use crate::library::Source;
        let mut a = app();
        assert_eq!(a.on_key(key(KeyCode::Char('l'))), [Effect::LoadLibrary]);
        a.on_library(Ok(vec![serde_json::from_value(serde_json::json!({"id": "p1", "name": "Soirée", "items": {"total": 3}})).unwrap()]));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.on_key(key(KeyCode::Enter)), [Effect::LoadLibraryList { source: Source::Playlist { id: "p1".into() }, name: "Soirée".into() }]);

        let tracks: Vec<Track> = (1..=3)
            .map(|n| Track { uri: format!("spotify:track:{n}"), name: format!("S{n}"), duration_ms: 1000, ..Default::default() })
            .collect();
        a.on_library_list("liked".into(), Ok(vec![])); // stale answer, ignored
        a.on_library_list("p1".into(), Ok(tracks));
        a.on_key(key(KeyCode::Down));
        assert!(matches!(sent(&a.on_key(key(KeyCode::Enter)))[..], [ClientMsg::QueueAdd { track }] if track.name == "S2"));
        assert!(matches!(sent(&a.on_key(key(KeyCode::Char('t'))))[..], [ClientMsg::QueueAddMany { tracks, mode: AddMode::End }] if tracks.len() == 3));
        let fx = a.on_key(key(KeyCode::Char('s')));
        assert!(matches!(sent(&fx)[..], [ClientMsg::QueueAddMany { tracks, mode: AddMode::PlayNow }] if tracks.len() == 3));
        assert!(a.library.is_none(), "closes once the list plays");

        // A listener can add one track but not take over the queue.
        a.me = 2;
        a.on_key(key(KeyCode::Char('l')));
        a.on_key(key(KeyCode::Enter)); // liked songs
        a.on_library_list("liked".into(), Ok(vec![Track { uri: "spotify:track:x".into(), name: "X".into(), duration_ms: 1, ..Default::default() }]));
        assert_eq!(sent(&a.on_key(key(KeyCode::Enter))).len(), 1);
        assert!(a.on_key(key(KeyCode::Char('p'))).is_empty());
        assert!(a.active_toast().unwrap().0.contains("ne permet pas"));
        a.on_key(key(KeyCode::Esc));
        assert!(a.library.as_ref().unwrap().open.is_none(), "Esc goes back to the lists");
    }

    #[test]
    fn renders_library() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = app();
        a.on_key(key(KeyCode::Char('l')));
        a.on_library(Ok(vec![serde_json::from_value(serde_json::json!({"id": "p1", "name": "Klassique", "items": {"total": 92}})).unwrap()]));
        let draw = |a: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, a)).unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
                .collect::<String>()
        };
        let text = draw(&mut a);
        assert!(text.contains("Titres likés") && text.contains("Klassique") && text.contains("92 titres"), "{text}");
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Enter));
        a.on_library_list("p1".into(), Ok(vec![Track { uri: "spotify:track:c".into(), name: "Cello Suite No. 1".into(), duration_ms: 150_000, ..Default::default() }]));
        let text = draw(&mut a);
        println!("{text}");
        for needle in ["Bibliothèque › Klassique", "1 titres (les 1 premiers sur 92)", "Cello Suite No. 1", "en aléatoire", "filtrer par titre, artiste ou album"] {
            assert!(text.contains(needle), "{needle} missing");
        }
    }

    #[test]
    fn volume_is_debounced_and_mute_lasts_one_track() {
        let mut a = app();
        a.on_volume(Some(40));
        assert!(a.on_key(key(KeyCode::Char('+'))).is_empty());
        a.on_key(key(KeyCode::Char('+')));
        assert_eq!(a.volume, Some(50));
        assert!(a.on_volume_due().is_empty(), "waits for a pause");
        a.volume_due = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(a.on_volume_due(), [Effect::SetVolume(50)]);

        // Mute: immediate, for this track only.
        assert_eq!(a.on_key(key(KeyCode::Char('m'))), [Effect::SetVolume(0)]);
        let mut p = a.playback.clone();
        p.seq += 1;
        p.position_ms = 5_000; // same track (seek): still muted
        assert!(!a.on_room_event(Event::Server(Box::new(ServerMsg::Playback(p.clone())))).contains(&Effect::SetVolume(50)));
        assert!(a.muted.is_some());
        p.seq += 1;
        p.current = Some(item(7));
        let fx = a.on_room_event(Event::Server(Box::new(ServerMsg::Playback(p))));
        assert!(fx.contains(&Effect::SetVolume(50)), "sound back on the next track: {fx:?}");
        assert!(a.muted.is_none());
        assert!(fx.contains(&Effect::CheckLiked(vec!["spotify:track:7".into()])));

        // m twice: unmute right away.
        a.on_key(key(KeyCode::Char('m')));
        assert_eq!(a.on_key(key(KeyCode::Char('m'))), [Effect::SetVolume(50)]);
    }

    #[test]
    fn like_button_toggles_for_the_current_track() {
        let mut a = app();
        a.areas.like_button = Rect::new(30, 5, 5, 1);
        let click = MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 31, row: 5, modifiers: KeyModifiers::NONE };
        assert_eq!(a.on_mouse(click), [Effect::SetLiked { uri: "spotify:track:1".into(), name: "T1".into(), liked: true }]);
        a.on_like_done("spotify:track:1".into(), "T1".into(), true, Ok(()));
        assert_eq!(a.current_liked(), Some(true));
        assert_eq!(a.on_key(key(KeyCode::Char('f'))), [Effect::SetLiked { uri: "spotify:track:1".into(), name: "T1".into(), liked: false }]);

        // Spectators have no Spotify account.
        a.can_play = false;
        assert!(a.on_key(key(KeyCode::Char('f'))).is_empty());
        assert!(a.on_key(key(KeyCode::Char('m'))).is_empty());
    }

    #[test]
    fn renders_like_button_and_volume() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut a = app();
        a.on_volume(Some(27));
        a.on_liked(vec![("spotify:track:1".into(), true)]);
        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
        assert!(text.contains("[ ♥ ]"), "like button");
        assert!(text.contains("27 %  (+/- · m muet)"), "volume");
        assert!(a.areas.like_button.width > 0 && a.areas.volume_bar.width > 5);
    }

    #[test]
    fn library_filter_narrows_lists_and_whole_list_actions() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('l')));
        let pl = |id: &str, name: &str| -> PlaylistSummary {
            serde_json::from_value(serde_json::json!({"id": id, "name": name, "items": {"total": 3}})).unwrap()
        };
        a.on_library(Ok(vec![pl("p1", "Soirée"), pl("p2", "Star Wars")]));

        // Filter the playlists, then open the only match.
        a.on_key(key(KeyCode::Char('/')));
        for c in "star".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(a.library.as_ref().unwrap().visible().len(), 1);
        a.on_key(key(KeyCode::Enter)); // leave the field
        assert_eq!(a.on_key(key(KeyCode::Enter)), [Effect::LoadLibraryList { source: crate::library::Source::Playlist { id: "p2".into() }, name: "Star Wars".into() }]);

        let track = |n: &str, artist: &str| Track {
            uri: format!("spotify:track:{n}"),
            name: n.into(),
            duration_ms: 1000,
            artists: vec![spotify::models::Artist { name: artist.into() }],
            ..Default::default()
        };
        a.on_library_list("p2".into(), Ok(vec![track("Duel of the Fates", "John Williams"), track("Imperial March", "John Williams"), track("Cantina", "Figrin D'an")]));

        // Filter the tracks: actions apply to the matches.
        a.on_key(key(KeyCode::Char('/')));
        for c in "williams".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        a.on_key(key(KeyCode::Down));
        assert!(matches!(sent(&a.on_key(key(KeyCode::Char('a'))))[..], [ClientMsg::QueueAdd { track }] if track.name == "Imperial March"));
        assert!(matches!(sent(&a.on_key(key(KeyCode::Char('t'))))[..], [ClientMsg::QueueAddMany { tracks, .. }] if tracks.len() == 2));

        // Esc clears the filter first, then goes back to the lists.
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.library.as_ref().unwrap().open.as_ref().unwrap().visible().len(), 3);
        a.on_key(key(KeyCode::Esc));
        assert!(a.library.as_ref().unwrap().open.is_none());
    }
}

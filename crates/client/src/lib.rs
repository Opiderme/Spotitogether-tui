//! Room client: WebSocket connection, clock sync and the loop that mirrors
//! the room on the user's Spotify. Front-ends (the headless `join` command,
//! later the TUI) consume [`Event`]s and send [`ClientMsg`]s.

pub mod clock;
pub mod sync;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, DEFAULT_PORT, MemberId, PROTOCOL_VERSION, Playback, RoomSnapshot, ServerMsg, WS_PATH};
use spotify::SpotifyClient;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use clock::{Clock, local_ms};
pub use sync::SyncEvent;
use sync::Syncer;

const INVITE_SCHEME: &str = "spotitogether://";
/// Quick pings right after joining, then a slow refresh.
const INITIAL_PINGS: u32 = 5;
const INITIAL_PING_EVERY: Duration = Duration::from_millis(200);
const PING_EVERY: Duration = Duration::from_secs(30);
const DRIFT_CHECK_EVERY: Duration = Duration::from_secs(5);
/// First check after (re)starting a track, to catch an ignored command early.
const CHECK_AFTER_APPLY: Duration = Duration::from_millis(1_500);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connexion au salon impossible : {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("le salon a refusé l'entrée : {0}")]
    Rejected(String),
    #[error("réponse inattendue du serveur : {0}")]
    Protocol(String),
    #[error("lien d'invitation invalide : {0}")]
    Invite(String),
}

/// Parsed invite: `spotitogether://host[:port]/CODE`, `host[:port]/CODE`,
/// or a raw `ws://` / `wss://` URL (code given separately).
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub ws_url: String,
    pub code: Option<String>,
}

pub fn parse_invite(input: &str) -> Result<Invite, Error> {
    let input = input.trim();
    if input.starts_with("ws://") || input.starts_with("wss://") {
        let base = input.trim_end_matches('/');
        let has_path = base.splitn(4, '/').nth(3).is_some();
        let ws_url = if has_path { base.to_owned() } else { format!("{base}{WS_PATH}") };
        return Ok(Invite { ws_url, code: None });
    }
    let rest = input.strip_prefix(INVITE_SCHEME).unwrap_or(input);
    let (host, code) = match rest.split_once('/') {
        Some((h, c)) => (h, Some(c.trim_matches('/')).filter(|c| !c.is_empty())),
        None => (rest, None),
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(Error::Invite(input.to_owned()));
    }
    let host = if host.contains(':') { host.to_owned() } else { format!("{host}:{DEFAULT_PORT}") };
    Ok(Invite { ws_url: format!("ws://{host}{WS_PATH}"), code: code.map(|c| c.to_uppercase()) })
}

pub struct JoinOptions {
    pub ws_url: String,
    pub code: String,
    pub name: String,
    pub host_token: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Server(Box<ServerMsg>),
    Sync(SyncEvent),
    /// New clock estimate: server time = local time + `offset_ms`.
    Clock { offset_ms: i64, rtt_ms: u64 },
    Disconnected(String),
}

enum Command {
    Send(ClientMsg),
    /// Re-apply the room state on Spotify now.
    Resync,
}

/// Sends commands to the room.
#[derive(Clone)]
pub struct RoomHandle {
    tx: mpsc::UnboundedSender<Command>,
}

impl RoomHandle {
    pub fn send(&self, msg: ClientMsg) -> bool {
        self.tx.send(Command::Send(msg)).is_ok()
    }

    /// Handle connected to nothing (tests, previews): messages are dropped.
    pub fn detached() -> Self {
        let (tx, _) = mpsc::unbounded_channel();
        Self { tx }
    }

    /// Forces the local Spotify back onto the room state.
    pub fn resync(&self) -> bool {
        self.tx.send(Command::Resync).is_ok()
    }
}

/// Room metadata for a Spotify track (sent by whoever queues it).
pub fn track_info(t: &spotify::models::Track) -> protocol::TrackInfo {
    protocol::TrackInfo {
        uri: t.uri.clone(),
        name: t.name.clone(),
        artists: t.artist_names(),
        duration_ms: t.duration_ms,
        album_uri: Some(t.album.uri.clone()).filter(|u| !u.is_empty()),
        cover_url: t.cover_url().map(str::to_owned),
    }
}

pub struct Joined {
    pub member_id: MemberId,
    pub room: RoomSnapshot,
    pub handle: RoomHandle,
    pub events: mpsc::UnboundedReceiver<Event>,
}

/// Joins the room. Without `spotify` the client only watches (spectator).
pub async fn join(opts: JoinOptions, spotify: Option<Arc<SpotifyClient>>) -> Result<Joined, Error> {
    let (mut ws, _) = connect_async(opts.ws_url.as_str()).await?;
    let join = ClientMsg::Join { version: PROTOCOL_VERSION, code: opts.code, name: opts.name, host_token: opts.host_token };
    ws.send(Message::text(serde_json::to_string(&join).expect("serializable"))).await?;

    let (member_id, room) = loop {
        match ws.next().await {
            None => return Err(Error::Protocol("connexion fermée".into())),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                Ok(ServerMsg::Welcome { member_id, room }) => break (member_id, room),
                Ok(ServerMsg::Error { message, .. }) => return Err(Error::Rejected(message)),
                Ok(other) => return Err(Error::Protocol(format!("{other:?}"))),
                Err(e) => return Err(Error::Protocol(e.to_string())),
            },
            Some(Ok(_)) => continue,
        }
    };

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(run(ws, cmd_rx, event_tx, room.playback.clone(), spotify));
    Ok(Joined { member_id, room, handle: RoomHandle { tx: cmd_tx }, events: event_rx })
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn run(
    mut ws: Ws,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    mut playback: Playback,
    spotify: Option<Arc<SpotifyClient>>,
) {
    let emit = |e: Event| {
        let _ = events.send(e);
    };
    let mut clock = Clock::default();
    let mut syncer = Syncer::new(None);
    // Apply the room state once the clock is calibrated.
    let mut pending_apply = spotify.is_some();
    if let Some(sp) = &spotify
        && let Some(e) = syncer.choose_device(sp).await
    {
        emit(Event::Sync(e));
    }

    let mut pings_sent = 0u32;
    let mut next_ping = Instant::now();
    let mut drift_check = interval(DRIFT_CHECK_EVERY);
    drift_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let reason = loop {
        tokio::select! {
            frame = ws.next() => {
                let msg = match frame {
                    None => break "connexion fermée par le serveur".to_owned(),
                    Some(Err(e)) => break e.to_string(),
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                        Ok(m) => m,
                        Err(e) => { tracing::warn!(%e, "unparsable server message"); continue; }
                    },
                    Some(Ok(Message::Close(_))) => break "le serveur a fermé le salon".to_owned(),
                    Some(Ok(_)) => continue,
                };
                match &msg {
                    ServerMsg::Pong { t0, server_ms } => {
                        clock.record(*t0, *server_ms, local_ms());
                        emit(Event::Clock { offset_ms: clock.offset_ms(), rtt_ms: clock.rtt_ms().unwrap_or(0) });
                        continue;
                    }
                    ServerMsg::Playback(p) if p.seq > playback.seq => {
                        playback = p.clone();
                        pending_apply = spotify.is_some();
                    }
                    ServerMsg::Playback(_) => continue, // stale
                    _ => {}
                }
                emit(Event::Server(Box::new(msg)));
            }
            cmd = commands.recv() => match cmd {
                Some(Command::Send(msg)) => {
                    let json = serde_json::to_string(&msg).expect("serializable");
                    if let Err(e) = ws.send(Message::text(json)).await { break e.to_string(); }
                }
                Some(Command::Resync) => pending_apply = spotify.is_some(),
                None => {
                    let _ = ws.close(None).await;
                    return;
                }
            },
            _ = sleep_until(next_ping) => {
                let ping = serde_json::to_string(&ClientMsg::Ping { t0: local_ms() }).expect("serializable");
                if let Err(e) = ws.send(Message::text(ping)).await { break e.to_string(); }
                pings_sent += 1;
                next_ping = Instant::now() + if pings_sent < INITIAL_PINGS { INITIAL_PING_EVERY } else { PING_EVERY };
            }
            _ = drift_check.tick(), if spotify.is_some() && clock.is_ready() && !pending_apply => {
                if let Some(sp) = &spotify {
                    let events = syncer.check(sp, &clock, &playback).await;
                    let resynced = events.iter().any(|e| matches!(e, SyncEvent::Resynced { .. }));
                    events.into_iter().for_each(|e| emit(Event::Sync(e)));
                    if resynced {
                        drift_check.reset_at(Instant::now() + CHECK_AFTER_APPLY);
                    }
                }
            }
        }

        if pending_apply && clock.is_ready() {
            pending_apply = false;
            if let Some(sp) = &spotify {
                for e in syncer.apply(sp, &clock, &playback).await {
                    emit(Event::Sync(e));
                }
            }
            drift_check.reset_at(Instant::now() + CHECK_AFTER_APPLY);
        }
    };
    emit(Event::Disconnected(reason));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_invites() {
        let i = parse_invite("spotitogether://salon.exemple.fr:7420/k7xq2m").unwrap();
        assert_eq!(i, Invite { ws_url: "ws://salon.exemple.fr:7420/ws".into(), code: Some("K7XQ2M".into()) });

        let i = parse_invite("192.168.1.20/K7XQ2M").unwrap();
        assert_eq!(i.ws_url, "ws://192.168.1.20:7420/ws");

        let i = parse_invite("wss://salon.exemple.fr").unwrap();
        assert_eq!(i, Invite { ws_url: "wss://salon.exemple.fr/ws".into(), code: None });

        let i = parse_invite("ws://127.0.0.1:9000/ws").unwrap();
        assert_eq!(i.ws_url, "ws://127.0.0.1:9000/ws");

        assert!(parse_invite("spotitogether:///CODE").is_err());
    }
}

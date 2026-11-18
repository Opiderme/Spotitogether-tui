//! Room client: WebSocket connection, clock sync and the loop that mirrors
//! the room on the user's Spotify. Front-ends (the headless `join` command,
//! later the TUI) consume [`Event`]s and send [`ClientMsg`]s.

pub mod clock;
pub mod sync;
pub mod tunnel;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, DEFAULT_PORT, ErrorCode, MemberId, PROTOCOL_VERSION, Playback, RoomSnapshot, ServerMsg, WS_PATH};
use spotify::SpotifyClient;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use clock::{Clock, local_ms};
pub use sync::SyncEvent;
use sync::Syncer;

const INVITE_SCHEME: &str = "spotitogether://";
/// Room behind HTTPS (reverse proxy with a certificate): `wss://`, port 443.
const INVITE_SCHEME_TLS: &str = "spotitogether+tls://";
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

/// Parsed invite: `spotitogether://<host key>/CODE` (peer-to-peer, see
/// [`tunnel`]), `spotitogether://host[:port]/CODE`, `host[:port]/CODE`,
/// `spotitogether+tls://host[:port]/CODE` (TLS, default port 443),
/// or a raw `ws://` / `wss://` URL (code given separately).
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    /// Direct WebSocket URL (empty for a peer-to-peer invite).
    pub ws_url: String,
    pub code: Option<String>,
    /// Host key of a peer-to-peer invite.
    pub peer: Option<String>,
}

impl Invite {
    /// WebSocket URL to join, opening the peer-to-peer tunnel when needed
    /// (keep the tunnel alive for the whole session).
    pub async fn connect_url(&self) -> Result<(String, Option<tunnel::GuestTunnel>), tunnel::TunnelError> {
        match &self.peer {
            Some(id) => {
                let t = tunnel::GuestTunnel::start(id).await?;
                Ok((t.ws_url(), Some(t)))
            }
            None => Ok((self.ws_url.clone(), None)),
        }
    }
}

pub fn parse_invite(input: &str) -> Result<Invite, Error> {
    let input = input.trim();
    if input.starts_with("ws://") || input.starts_with("wss://") {
        let base = input.trim_end_matches('/');
        let has_path = base.splitn(4, '/').nth(3).is_some();
        let ws_url = if has_path { base.to_owned() } else { format!("{base}{WS_PATH}") };
        return Ok(Invite { ws_url, code: None, peer: None });
    }
    let (rest, tls) = match input.strip_prefix(INVITE_SCHEME_TLS) {
        Some(rest) => (rest, true),
        None => (input.strip_prefix(INVITE_SCHEME).unwrap_or(input), false),
    };
    let (host, code) = match rest.split_once('/') {
        Some((h, c)) => (h, Some(c.trim_matches('/')).filter(|c| !c.is_empty())),
        None => (rest, None),
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(Error::Invite(input.to_owned()));
    }
    let code = code.map(|c| c.to_uppercase());
    if tunnel::is_peer_id(host) {
        return Ok(Invite { ws_url: String::new(), code, peer: Some(host.to_owned()) });
    }
    let (scheme, default_port) = if tls { ("wss", 443) } else { ("ws", DEFAULT_PORT) };
    let host = if host.contains(':') { host.to_owned() } else { format!("{host}:{default_port}") };
    Ok(Invite { ws_url: format!("{scheme}://{host}{WS_PATH}"), code, peer: None })
}

pub struct JoinOptions {
    pub ws_url: String,
    pub code: String,
    pub name: String,
    pub host_token: Option<String>,
    pub spotify_id: Option<String>,
    /// Called if the room has a waiting room and the host must admit us.
    pub on_waiting: Option<Box<dyn FnOnce() + Send>>,
}

/// What is needed to (re)connect.
#[derive(Clone)]
struct Credentials {
    ws_url: String,
    code: String,
    name: String,
    host_token: Option<String>,
    spotify_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Server(Box<ServerMsg>),
    Sync(SyncEvent),
    /// New clock estimate: server time = local time + `offset_ms`.
    Clock { offset_ms: i64, rtt_ms: u64 },
    /// Connection lost; retrying (the room keeps our place for a while).
    Reconnecting { attempt: u32, reason: String },
    /// Back in the room; a `Server(Welcome)` with the fresh state follows.
    Reconnected,
    /// Final: kicked, banned, or refused when reconnecting.
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
        album_name: Some(t.album.name.clone()).filter(|n| !n.is_empty()),
        cover_url: t.cover_url().map(str::to_owned),
    }
}

pub struct Joined {
    pub member_id: MemberId,
    pub room: RoomSnapshot,
    pub handle: RoomHandle,
    pub events: mpsc::UnboundedReceiver<Event>,
}

struct Connected {
    ws: Ws,
    member_id: MemberId,
    session: String,
    room: RoomSnapshot,
}

/// Joins the room. Without `spotify` the client only watches (spectator).
pub async fn join(opts: JoinOptions, spotify: Option<Arc<SpotifyClient>>) -> Result<Joined, Error> {
    let creds = Credentials {
        ws_url: opts.ws_url,
        code: opts.code,
        name: opts.name,
        host_token: opts.host_token,
        spotify_id: opts.spotify_id,
    };
    let mut on_waiting = opts.on_waiting;
    let conn = connect(&creds, None, &mut on_waiting).await?;
    let (member_id, room) = (conn.member_id, conn.room.clone());

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let can_listen = room.members.iter().find(|m| m.id == member_id).is_some_and(|m| m.perms.listen);
    let listener = spotify.map(|sp| Listener { spotify: sp, member_id, can_listen });
    tokio::spawn(run(conn, creds, cmd_rx, event_tx, listener));
    Ok(Joined { member_id, room, handle: RoomHandle { tx: cmd_tx }, events: event_rx })
}

async fn connect(
    creds: &Credentials,
    session: Option<&str>,
    on_waiting: &mut Option<Box<dyn FnOnce() + Send>>,
) -> Result<Connected, Error> {
    let (mut ws, _) = connect_async(creds.ws_url.as_str()).await?;
    let join = ClientMsg::Join {
        version: PROTOCOL_VERSION,
        code: creds.code.clone(),
        name: creds.name.clone(),
        host_token: creds.host_token.clone(),
        spotify_id: creds.spotify_id.clone(),
        session: session.map(str::to_owned),
    };
    ws.send(Message::text(serde_json::to_string(&join).expect("serializable"))).await?;
    loop {
        match ws.next().await {
            None => return Err(Error::Protocol("connexion fermée".into())),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                Ok(ServerMsg::Welcome { member_id, session, room }) => {
                    return Ok(Connected { ws, member_id, session, room });
                }
                Ok(ServerMsg::Waiting) => {
                    if let Some(f) = on_waiting.take() {
                        f();
                    }
                }
                Ok(ServerMsg::Error { message, .. }) => return Err(Error::Rejected(message)),
                Ok(other) => return Err(Error::Protocol(format!("{other:?}"))),
                Err(e) => return Err(Error::Protocol(e.to_string())),
            },
            Some(Ok(_)) => continue,
        }
    }
}

/// The local Spotify, played only while the member has the `Listen` permission.
struct Listener {
    spotify: Arc<SpotifyClient>,
    member_id: MemberId,
    can_listen: bool,
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Why a connection ended.
enum Ended {
    /// Network problem or server restart: try to reconnect.
    Lost(String),
    /// Kicked or banned: do not come back.
    Final(String),
    /// The front-end dropped its handle (user quit).
    Quit,
}

/// Delay before reconnection attempt `n` (1-based).
pub fn reconnect_delay(attempt: u32) -> Duration {
    const STEPS: [u64; 5] = [1, 2, 3, 5, 10];
    Duration::from_secs(STEPS[(attempt.max(1) as usize - 1).min(STEPS.len() - 1)])
}

async fn run(
    conn: Connected,
    mut creds: Credentials,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    mut listener: Option<Listener>,
) {
    let emit = |e: Event| {
        let _ = events.send(e);
    };
    let mut syncer = Syncer::new(None);
    if let Some(l) = &listener
        && l.can_listen
        && let Some(e) = syncer.choose_device(&l.spotify).await
    {
        emit(Event::Sync(e));
    }

    let Connected { mut ws, mut session, room, .. } = conn;
    let mut playback = room.playback;
    loop {
        match connection(&mut ws, &mut creds, &mut commands, &emit, &mut playback, &mut listener, &mut syncer).await {
            Ended::Quit => return,
            Ended::Final(reason) => {
                emit(Event::Disconnected(reason));
                return;
            }
            Ended::Lost(reason) => {
                let mut attempt = 0;
                let conn = loop {
                    attempt += 1;
                    emit(Event::Reconnecting { attempt, reason: reason.clone() });
                    // Keep watching the handle: quitting must not wait.
                    let deadline = Instant::now() + reconnect_delay(attempt);
                    loop {
                        tokio::select! {
                            _ = sleep_until(deadline) => break,
                            cmd = commands.recv() => if cmd.is_none() { return },
                        }
                    }
                    match connect(&creds, Some(&session), &mut None).await {
                        Ok(conn) => break conn,
                        Err(Error::Rejected(message)) => {
                            emit(Event::Disconnected(message));
                            return;
                        }
                        Err(e) => tracing::info!(%e, attempt, "reconnection failed"),
                    }
                };
                ws = conn.ws;
                session = conn.session;
                playback = conn.room.playback.clone();
                if let Some(l) = &mut listener {
                    l.member_id = conn.member_id;
                    l.can_listen = conn.room.members.iter().any(|m| m.id == conn.member_id && m.perms.listen);
                }
                emit(Event::Reconnected);
                emit(Event::Server(Box::new(ServerMsg::Welcome {
                    member_id: conn.member_id,
                    session: session.clone(),
                    room: conn.room,
                })));
            }
        }
    }
}

/// Serves one WebSocket connection until it ends.
async fn connection(
    ws: &mut Ws,
    creds: &mut Credentials,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    emit: &impl Fn(Event),
    playback: &mut Playback,
    listener: &mut Option<Listener>,
    syncer: &mut Syncer,
) -> Ended {
    let mut clock = Clock::default();
    let active = |l: &Option<Listener>| l.as_ref().is_some_and(|l| l.can_listen);
    // Apply the room state once the clock is calibrated.
    let mut pending_apply = active(listener);
    // Kick/ban reason, reported when the server then closes the socket.
    let mut close_reason: Option<String> = None;
    let lost = |close_reason: &mut Option<String>, default: String| match close_reason.take() {
        Some(r) => Ended::Final(r),
        None => Ended::Lost(default),
    };

    let mut pings_sent = 0u32;
    let mut next_ping = Instant::now();
    let mut drift_check = interval(DRIFT_CHECK_EVERY);
    drift_check.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // End of track: look at the local player a few seconds before the room
    // track ends, then pause it right at its own end (with the playback
    // state `seq` they were planned for).
    let mut end_planned_for: Option<u64> = None;
    let mut end_probe: Option<(Instant, u64)> = None;
    let mut end_pause: Option<(Instant, u64)> = None;
    let far = || Instant::now() + Duration::from_secs(86_400);

    loop {
        tokio::select! {
            frame = ws.next() => {
                let msg = match frame {
                    None | Some(Ok(Message::Close(_))) => return lost(&mut close_reason, "connexion fermée par le serveur".into()),
                    Some(Err(e)) => return lost(&mut close_reason, e.to_string()),
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ServerMsg>(t.as_str()) {
                        Ok(m) => m,
                        Err(e) => { tracing::warn!(%e, "unparsable server message"); continue; }
                    },
                    Some(Ok(_)) => continue,
                };
                match &msg {
                    ServerMsg::Pong { t0, server_ms } => {
                        clock.record(*t0, *server_ms, local_ms());
                        emit(Event::Clock { offset_ms: clock.offset_ms(), rtt_ms: clock.rtt_ms().unwrap_or(0) });
                        continue;
                    }
                    ServerMsg::Playback(p) if p.seq > playback.seq => {
                        *playback = p.clone();
                        pending_apply = active(listener);
                    }
                    ServerMsg::Playback(_) => continue, // stale
                    ServerMsg::Members { members } => {
                        if let Some(l) = listener.as_mut() {
                            let now = members.iter().find(|m| m.id == l.member_id).is_some_and(|m| m.perms.listen);
                            if now != l.can_listen {
                                l.can_listen = now;
                                if now {
                                    pending_apply = true;
                                } else if let Some(e) = syncer.stop(&l.spotify).await {
                                    emit(Event::Sync(e));
                                }
                            }
                        }
                    }
                    // Reconnecting needs the current code.
                    ServerMsg::Settings(settings) => creds.code = settings.code.clone(),
                    ServerMsg::Error { code: ErrorCode::Kicked | ErrorCode::Banned | ErrorCode::RoomClosed, message } => {
                        close_reason = Some(message.clone());
                    }
                    _ => {}
                }
                emit(Event::Server(Box::new(msg)));
            }
            cmd = commands.recv() => match cmd {
                Some(Command::Send(msg)) => {
                    let json = serde_json::to_string(&msg).expect("serializable");
                    if let Err(e) = ws.send(Message::text(json)).await {
                        return lost(&mut close_reason, e.to_string());
                    }
                }
                Some(Command::Resync) => pending_apply = active(listener),
                None => {
                    let _ = ws.close(None).await;
                    return Ended::Quit;
                }
            },
            _ = sleep_until(next_ping) => {
                let ping = serde_json::to_string(&ClientMsg::Ping { t0: local_ms() }).expect("serializable");
                if let Err(e) = ws.send(Message::text(ping)).await {
                    return lost(&mut close_reason, e.to_string());
                }
                pings_sent += 1;
                next_ping = Instant::now() + if pings_sent < INITIAL_PINGS { INITIAL_PING_EVERY } else { PING_EVERY };
            }
            _ = drift_check.tick(), if active(listener) && clock.is_ready() && !pending_apply => {
                if let Some(l) = listener.as_ref() {
                    let events = syncer.check(&l.spotify, &clock, playback).await;
                    let resynced = events.iter().any(|e| matches!(e, SyncEvent::Resynced { .. }));
                    events.into_iter().for_each(|e| emit(Event::Sync(e)));
                    if resynced || syncer.wants_recheck() {
                        drift_check.reset_at(Instant::now() + CHECK_AFTER_APPLY);
                    }
                }
            }
            _ = sleep_until(end_probe.map_or_else(far, |(at, _)| at)), if end_probe.is_some() => {
                let seq = end_probe.take().map(|(_, seq)| seq);
                if let Some(l) = listener.as_ref().filter(|l| l.can_listen && seq == Some(playback.seq))
                    && let Some(wait) = syncer.time_to_end(&l.spotify, playback).await
                {
                    end_pause = Some((Instant::now() + wait, playback.seq));
                }
            }
            _ = sleep_until(end_pause.map_or_else(far, |(at, _)| at)), if end_pause.is_some() => {
                let seq = end_pause.take().map(|(_, seq)| seq);
                if let Some(l) = listener.as_ref().filter(|l| l.can_listen && seq == Some(playback.seq))
                    && let Some(e) = syncer.hold_at_end(&l.spotify).await
                {
                    emit(Event::Sync(e));
                }
            }
        }

        if pending_apply && clock.is_ready() {
            pending_apply = false;
            if let Some(l) = listener.as_ref().filter(|l| l.can_listen) {
                for e in syncer.apply(&l.spotify, &clock, playback).await {
                    emit(Event::Sync(e));
                }
            }
            drift_check.reset_at(Instant::now() + CHECK_AFTER_APPLY);
        }

        // A new playback state (next track, seek, pause): plan its end.
        if end_planned_for != Some(playback.seq) && clock.is_ready() && !pending_apply {
            end_planned_for = Some(playback.seq);
            end_pause = None;
            end_probe = syncer
                .end_probe_in(&clock, playback)
                .filter(|_| active(listener))
                .map(|wait| (Instant::now() + wait, playback.seq));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_invites() {
        let i = parse_invite("spotitogether://salon.exemple.fr:7420/k7xq2m").unwrap();
        assert_eq!(i, Invite { ws_url: "ws://salon.exemple.fr:7420/ws".into(), code: Some("K7XQ2M".into()), peer: None });

        let i = parse_invite("192.168.1.20/K7XQ2M").unwrap();
        assert_eq!(i.ws_url, "ws://192.168.1.20:7420/ws");

        let i = parse_invite("wss://salon.exemple.fr").unwrap();
        assert_eq!(i, Invite { ws_url: "wss://salon.exemple.fr/ws".into(), code: None, peer: None });

        let i = parse_invite("ws://127.0.0.1:9000/ws").unwrap();
        assert_eq!(i.ws_url, "ws://127.0.0.1:9000/ws");

        let i = parse_invite("spotitogether+tls://salon.exemple.fr/k7xq2m").unwrap();
        assert_eq!(i, Invite { ws_url: "wss://salon.exemple.fr:443/ws".into(), code: Some("K7XQ2M".into()), peer: None });

        let key = iroh::SecretKey::generate().public().to_string();
        let i = parse_invite(&format!("spotitogether://{key}/k7xq2m")).unwrap();
        assert_eq!(i, Invite { ws_url: String::new(), code: Some("K7XQ2M".into()), peer: Some(key) });

        assert!(parse_invite("spotitogether:///CODE").is_err());
    }
}

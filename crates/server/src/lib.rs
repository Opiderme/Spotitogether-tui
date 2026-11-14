//! Self-hosted room server: one room per process (v1), WebSocket API.
//!
//! The server never talks to Spotify. It only holds the room state and a
//! timer for the current track; clients mirror that state on their account.

mod room;
mod store;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use axum::routing::get;
use protocol::{ClientMsg, ErrorCode, MemberId, PROTOCOL_VERSION, ServerMsg, WS_PATH};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub use room::Room;
use room::{JoinRequest, Out};
pub use store::Store;

const JOIN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BAD_CODES: usize = 5;
const BAD_CODE_WINDOW: Duration = Duration::from_secs(60);
/// Unambiguous characters (no 0/O, 1/I/L).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Random 6-character invite code, e.g. `K7XQ2M`.
pub fn generate_code() -> String {
    random_string(6, CODE_ALPHABET)
}

/// Random secret that grants the host role.
pub fn generate_host_token() -> String {
    random_string(32, b"abcdefghijklmnopqrstuvwxyz0123456789")
}

fn random_string(len: usize, alphabet: &[u8]) -> String {
    let mut bytes = vec![0u8; len];
    rand::fill(&mut bytes[..]);
    // 256 is not a multiple of the alphabet size: the slight bias is
    // irrelevant here thanks to the per-IP attempt limit.
    bytes.iter().map(|b| alphabet[*b as usize % alphabet.len()] as char).collect()
}

enum Cmd {
    Join {
        name: String,
        host_token: Option<String>,
        spotify_id: Option<String>,
        session: Option<String>,
        ip: IpAddr,
        tx: mpsc::UnboundedSender<ServerMsg>,
        reply: oneshot::Sender<Result<(MemberId, ConnId), (ErrorCode, String)>>,
    },
    Msg {
        from: MemberId,
        msg: ClientMsg,
    },
    Leave {
        id: MemberId,
        conn: ConnId,
    },
    /// Tell everyone the room is closing, drop all connections, stop.
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// Identifies one WebSocket connection: after a reconnection, the old
/// connection closing must not mark the member offline again.
type ConnId = u64;

const EXPIRE_EVERY: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct AppState {
    room_tx: mpsc::UnboundedSender<Cmd>,
    /// Current invite code (the host can rotate it).
    code: Arc<RwLock<String>>,
    bad_codes: Arc<Mutex<HashMap<IpAddr, Vec<std::time::Instant>>>>,
}

/// Serves the room on `listener` until the process stops. With a `store`,
/// roles, bans and settings persist across restarts.
pub async fn serve(listener: TcpListener, code: String, host_token: String, store: Option<Store>) -> std::io::Result<()> {
    serve_until(listener, code, host_token, store, std::future::pending()).await
}

/// Like [`serve`], until `shutdown` completes: members are then told that
/// the host closed the room (their clients do not try to reconnect).
pub async fn serve_until(
    listener: TcpListener,
    code: String,
    host_token: String,
    store: Option<Store>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let room = Room::new(code.clone(), host_token, now_ms(), store);
    let (room_tx, room_rx) = mpsc::unbounded_channel();
    let code = Arc::new(RwLock::new(code));
    tokio::spawn(run_room(room, room_rx, code.clone()));

    let closer = room_tx.clone();
    let state = AppState { room_tx, code, bad_codes: Default::default() };
    let app = Router::new()
        .route(WS_PATH, get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            shutdown.await;
            let (done, closed) = oneshot::channel();
            if closer.send(Cmd::Shutdown { done }).is_ok() {
                let _ = closed.await;
            }
        })
        .await
}

/// Single task owning the room: applies commands in order and fires the
/// end-of-track timer.
async fn run_room(mut room: Room, mut rx: mpsc::UnboundedReceiver<Cmd>, shared_code: Arc<RwLock<String>>) {
    let mut conns: HashMap<MemberId, (ConnId, mpsc::UnboundedSender<ServerMsg>)> = HashMap::new();
    let mut next_conn: ConnId = 1;
    let mut expire = tokio::time::interval(EXPIRE_EVERY);
    loop {
        // +5 ms so the timer never fires before the deadline (no busy loop).
        let deadline = room
            .deadline()
            .map(|end| Instant::now() + Duration::from_millis(end.saturating_sub(now_ms()) + 5));
        let cmd = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(cmd) => Some(cmd),
                None => break,
            },
            _ = sleep_until(deadline) => None,
            _ = expire.tick() => {
                let outs = room.expire(now_ms());
                dispatch(outs, &room, &mut conns);
                continue;
            }
        };

        let now = now_ms();
        let outs = match cmd {
            None => room.tick(now),
            Some(Cmd::Join { name, host_token, spotify_id, session, ip, tx, reply }) => {
                let request = JoinRequest {
                    name: &name,
                    host_token: host_token.as_deref(),
                    spotify_id: spotify_id.as_deref(),
                    session: session.as_deref(),
                    ip,
                };
                match room.join(request) {
                    Ok((id, outs)) => {
                        let conn = next_conn;
                        next_conn += 1;
                        // A resumed session replaces any previous connection.
                        conns.insert(id, (conn, tx));
                        let _ = reply.send(Ok((id, conn)));
                        tracing::info!(id, name = room.member_name(id), "member joined");
                        outs
                    }
                    Err(rejection) => {
                        tracing::info!(%ip, reason = rejection.1, "join refused");
                        let _ = reply.send(Err(rejection));
                        vec![]
                    }
                }
            }
            Some(Cmd::Msg { from, msg }) => room.handle(from, msg, now),
            Some(Cmd::Shutdown { done }) => {
                let bye = ServerMsg::Error { code: ErrorCode::RoomClosed, message: "l'hôte a fermé le salon".into() };
                for (_, tx) in conns.values() {
                    let _ = tx.send(bye.clone());
                }
                // Dropping the senders closes every connection.
                conns.clear();
                let _ = done.send(());
                tracing::info!("room closed");
                return;
            }
            Some(Cmd::Leave { id, conn }) => {
                if conns.get(&id).map(|(c, _)| *c) != Some(conn) {
                    continue; // stale connection of a resumed member
                }
                conns.remove(&id);
                tracing::info!(id, "member disconnected");
                room.leave(id, now)
            }
        };
        dispatch(outs, &room, &mut conns);

        if *shared_code.read().expect("lock") != room.code() {
            *shared_code.write().expect("lock") = room.code().to_owned();
            tracing::info!(code = room.code(), "invite code rotated");
        }
    }
}

fn dispatch(outs: Vec<Out>, room: &Room, conns: &mut HashMap<MemberId, (ConnId, mpsc::UnboundedSender<ServerMsg>)>) {
    for out in outs {
        match out {
            Out::All(msg) => {
                for (id, (_, tx)) in conns.iter() {
                    if room.is_admitted(*id) {
                        let _ = tx.send(msg.clone());
                    }
                }
            }
            Out::One(id, msg) => {
                if let Some((_, tx)) = conns.get(&id) {
                    let _ = tx.send(msg);
                }
            }
            // Dropping the sender ends the connection task once it has
            // flushed what was already queued.
            Out::Close(id) => {
                conns.remove(&id);
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, addr.ip(), state))
}

async fn send(socket: &mut WebSocket, msg: &ServerMsg) -> Result<(), axum::Error> {
    let json = serde_json::to_string(msg).expect("ServerMsg serializes");
    socket.send(Message::Text(json.into())).await
}

async fn reject(mut socket: WebSocket, code: ErrorCode, message: &str) {
    let _ = send(&mut socket, &ServerMsg::Error { code, message: message.to_owned() }).await;
    let _ = socket.send(Message::Close(None)).await;
}

/// Next text frame parsed as a client message (`None` when the socket closes).
async fn recv(socket: &mut WebSocket) -> Option<Result<ClientMsg, serde_json::Error>> {
    loop {
        match socket.recv().await? {
            Ok(Message::Text(text)) => return Some(serde_json::from_str(text.as_str())),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => continue,
        }
    }
}

impl AppState {
    /// True while `ip` is locked out after too many wrong codes.
    fn is_blocked(&self, ip: IpAddr) -> bool {
        let mut map = self.bad_codes.lock().expect("lock");
        let entries = map.entry(ip).or_default();
        entries.retain(|t| t.elapsed() < BAD_CODE_WINDOW);
        entries.len() >= MAX_BAD_CODES
    }

    fn record_bad_code(&self, ip: IpAddr) {
        self.bad_codes.lock().expect("lock").entry(ip).or_default().push(std::time::Instant::now());
    }
}

async fn handle_socket(mut socket: WebSocket, ip: IpAddr, state: AppState) {
    let (name, host_token, spotify_id, session) = match tokio::time::timeout(JOIN_TIMEOUT, recv(&mut socket)).await {
        Ok(Some(Ok(ClientMsg::Join { version, code, name, host_token, spotify_id, session }))) => {
            if version != PROTOCOL_VERSION {
                let msg = format!("version de protocole {version} non supportée (serveur : {PROTOCOL_VERSION})");
                return reject(socket, ErrorCode::BadVersion, &msg).await;
            }
            if state.is_blocked(ip) {
                return reject(socket, ErrorCode::TooManyAttempts, "trop de codes erronés, réessaie dans une minute").await;
            }
            let current = state.code.read().expect("lock").clone();
            if !current.eq_ignore_ascii_case(code.trim()) {
                state.record_bad_code(ip);
                tracing::warn!(%ip, "wrong room code");
                return reject(socket, ErrorCode::BadCode, "code de salon incorrect").await;
            }
            (name, host_token, spotify_id, session)
        }
        Ok(Some(_)) => return reject(socket, ErrorCode::NotJoined, "le premier message doit être join").await,
        _ => return,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    if state.room_tx.send(Cmd::Join { name, host_token, spotify_id, session, ip, tx, reply: reply_tx }).is_err() {
        return;
    }
    let (id, conn) = match reply_rx.await {
        Ok(Ok(joined)) => joined,
        Ok(Err((code, message))) => return reject(socket, code, &message).await,
        Err(_) => return,
    };

    loop {
        tokio::select! {
            incoming = recv(&mut socket) => match incoming {
                None => break,
                Some(Ok(ClientMsg::Ping { t0 })) => {
                    if send(&mut socket, &ServerMsg::Pong { t0, server_ms: now_ms() }).await.is_err() {
                        break;
                    }
                }
                Some(Ok(msg)) => {
                    if state.room_tx.send(Cmd::Msg { from: id, msg }).is_err() {
                        break;
                    }
                }
                Some(Err(err)) => {
                    let error = ServerMsg::Error { code: ErrorCode::InvalidMessage, message: err.to_string() };
                    if send(&mut socket, &error).await.is_err() {
                        break;
                    }
                }
            },
            outgoing = rx.recv() => match outgoing {
                Some(msg) => {
                    if send(&mut socket, &msg).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    let _ = state.room_tx.send(Cmd::Leave { id, conn });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_unambiguous_alphabet() {
        for _ in 0..100 {
            let c = generate_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        }
    }
}

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
        ip: IpAddr,
        tx: mpsc::UnboundedSender<ServerMsg>,
        reply: oneshot::Sender<Result<MemberId, (ErrorCode, String)>>,
    },
    Msg {
        from: MemberId,
        msg: ClientMsg,
    },
    Leave {
        id: MemberId,
    },
}

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
    let room = Room::new(code.clone(), host_token, now_ms(), store);
    let (room_tx, room_rx) = mpsc::unbounded_channel();
    let code = Arc::new(RwLock::new(code));
    tokio::spawn(run_room(room, room_rx, code.clone()));

    let state = AppState { room_tx, code, bad_codes: Default::default() };
    let app = Router::new()
        .route(WS_PATH, get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}

/// Single task owning the room: applies commands in order and fires the
/// end-of-track timer.
async fn run_room(mut room: Room, mut rx: mpsc::UnboundedReceiver<Cmd>, shared_code: Arc<RwLock<String>>) {
    let mut conns: HashMap<MemberId, mpsc::UnboundedSender<ServerMsg>> = HashMap::new();
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
        };

        let now = now_ms();
        let outs = match cmd {
            None => room.tick(now),
            Some(Cmd::Join { name, host_token, spotify_id, ip, tx, reply }) => {
                let request = JoinRequest { name: &name, host_token: host_token.as_deref(), spotify_id: spotify_id.as_deref(), ip };
                match room.join(request) {
                    Ok((id, outs)) => {
                        conns.insert(id, tx);
                        let _ = reply.send(Ok(id));
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
            Some(Cmd::Leave { id }) => {
                conns.remove(&id);
                tracing::info!(id, "member left");
                room.leave(id, now)
            }
        };

        for out in outs {
            match out {
                Out::All(msg) => {
                    for (id, tx) in &conns {
                        if room.is_admitted(*id) {
                            let _ = tx.send(msg.clone());
                        }
                    }
                }
                Out::One(id, msg) => {
                    if let Some(tx) = conns.get(&id) {
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

        if *shared_code.read().expect("lock") != room.code() {
            *shared_code.write().expect("lock") = room.code().to_owned();
            tracing::info!(code = room.code(), "invite code rotated");
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
    let (name, host_token, spotify_id) = match tokio::time::timeout(JOIN_TIMEOUT, recv(&mut socket)).await {
        Ok(Some(Ok(ClientMsg::Join { version, code, name, host_token, spotify_id }))) => {
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
            (name, host_token, spotify_id)
        }
        Ok(Some(_)) => return reject(socket, ErrorCode::NotJoined, "le premier message doit être join").await,
        _ => return,
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    if state.room_tx.send(Cmd::Join { name, host_token, spotify_id, ip, tx, reply: reply_tx }).is_err() {
        return;
    }
    let id = match reply_rx.await {
        Ok(Ok(id)) => id,
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
    let _ = state.room_tx.send(Cmd::Leave { id });
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

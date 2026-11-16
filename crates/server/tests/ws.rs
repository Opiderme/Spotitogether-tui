//! End-to-end: real server on a random port, real WebSocket clients.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, Role, ServerMsg, TrackInfo};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, "ABC234".into(), "hosttoken".into(), None));
    format!("ws://{addr}/ws")
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    ws.send(Message::text(serde_json::to_string(msg).unwrap())).await.unwrap();
}

async fn next(ws: &mut Ws) -> ServerMsg {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("timed out waiting for server")
            .expect("stream ended")
            .unwrap();
        if let Message::Text(t) = frame {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

/// Skips messages until `pred` matches.
async fn wait_for(ws: &mut Ws, pred: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
    loop {
        let m = next(ws).await;
        if pred(&m) {
            return m;
        }
    }
}

async fn join(url: &str, code: &str, name: &str, host: bool) -> Ws {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let host_token = host.then(|| "hosttoken".to_owned());
    send(&mut ws, &ClientMsg::Join { version: PROTOCOL_VERSION, code: code.into(), name: name.into(), host_token, spotify_id: None, session: None }).await;
    ws
}

fn track(ms: u64) -> TrackInfo {
    TrackInfo { uri: "spotify:track:1".into(), name: "One More Time".into(), artists: "Daft Punk".into(), duration_ms: ms, album_uri: None, album_name: None, cover_url: None }
}

#[tokio::test]
async fn two_members_share_playback_and_track_ends() {
    let url = start().await;

    let mut alex = join(&url, "abc234", "Alex", true).await;
    let ServerMsg::Welcome { room, .. } = next(&mut alex).await else { panic!("expected welcome") };
    assert_eq!(room.members[0].role, Role::Host);

    let mut sam = join(&url, "ABC234", "Sam", false).await;
    assert!(matches!(next(&mut sam).await, ServerMsg::Welcome { .. }));
    let ServerMsg::Members { members } = wait_for(&mut alex, |m| matches!(m, ServerMsg::Members { members } if members.len() == 2)).await else { unreachable!() };
    assert_eq!(members[1].role, Role::Listener);

    // Sam queues a 300 ms track: both receive the same playback.
    send(&mut sam, &ClientMsg::QueueAdd { track: track(300) }).await;
    let is_playing = |m: &ServerMsg| matches!(m, ServerMsg::Playback(p) if p.is_playing);
    let ServerMsg::Playback(pa) = wait_for(&mut alex, is_playing).await else { unreachable!() };
    let ServerMsg::Playback(ps) = wait_for(&mut sam, is_playing).await else { unreachable!() };
    assert_eq!(pa, ps);
    assert_eq!(pa.current.as_ref().unwrap().added_by, "Sam");

    // The server timer ends the track by itself.
    let stopped = |m: &ServerMsg| matches!(m, ServerMsg::Playback(p) if p.current.is_none());
    let ServerMsg::Playback(end) = wait_for(&mut alex, stopped).await else { unreachable!() };
    assert!(end.server_ms >= pa.server_ms + 300);

    // Clock sync.
    send(&mut sam, &ClientMsg::Ping { t0: 42 }).await;
    let ServerMsg::Pong { t0, server_ms } = wait_for(&mut sam, |m| matches!(m, ServerMsg::Pong { .. })).await else { unreachable!() };
    assert_eq!(t0, 42);
    assert!(server_ms >= end.server_ms);

    // Leaving marks the member offline (their place is kept for a while).
    sam.close(None).await.unwrap();
    wait_for(&mut alex, |m| matches!(m, ServerMsg::Members { members } if members.iter().any(|m| !m.online))).await;
}

#[tokio::test]
async fn wrong_codes_are_rejected_then_rate_limited() {
    let url = start().await;
    for _ in 0..5 {
        let mut ws = join(&url, "WRONG1", "x", false).await;
        assert!(matches!(next(&mut ws).await, ServerMsg::Error { code: ErrorCode::BadCode, .. }));
    }
    // Even the right code is refused while locked out.
    let mut ws = join(&url, "ABC234", "x", false).await;
    assert!(matches!(next(&mut ws).await, ServerMsg::Error { code: ErrorCode::TooManyAttempts, .. }));
}

#[tokio::test]
async fn bad_version_is_rejected() {
    let url = start().await;
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(&mut ws, &ClientMsg::Join { version: 999, code: "ABC234".into(), name: "x".into(), host_token: None, spotify_id: None, session: None }).await;
    assert!(matches!(next(&mut ws).await, ServerMsg::Error { code: ErrorCode::BadVersion, .. }));
}

#[tokio::test]
async fn kicked_member_is_told_then_disconnected() {
    use protocol::AdminCmd;
    let url = start().await;
    let mut alex = join(&url, "ABC234", "Alex", true).await;
    next(&mut alex).await; // welcome
    let mut lou = join(&url, "ABC234", "Lou", false).await;
    let ServerMsg::Welcome { member_id: lou_id, .. } = next(&mut lou).await else { panic!() };

    send(&mut alex, &ClientMsg::Admin(AdminCmd::Kick { member: lou_id })).await;
    wait_for(&mut lou, |m| matches!(m, ServerMsg::Error { code: ErrorCode::Kicked, .. })).await;
    let end = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match lou.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    assert!(end.is_ok(), "connection should close");
    wait_for(&mut alex, |m| matches!(m, ServerMsg::Members { members } if members.len() == 1)).await;
}

#[tokio::test]
async fn waiting_room_admission_over_the_wire() {
    use protocol::AdminCmd;
    let url = start().await;
    let mut alex = join(&url, "ABC234", "Alex", true).await;
    next(&mut alex).await;
    send(&mut alex, &ClientMsg::Admin(AdminCmd::SetWaitingRoom { enabled: true })).await;
    wait_for(&mut alex, |m| matches!(m, ServerMsg::Settings(s) if s.waiting_room)).await;

    let mut lou = join(&url, "ABC234", "Lou", false).await;
    assert_eq!(next(&mut lou).await, ServerMsg::Waiting);
    let ServerMsg::Members { members } = wait_for(&mut alex, |m| matches!(m, ServerMsg::Members { members } if members.len() == 2)).await else { unreachable!() };
    let pending = members.iter().find(|m| m.pending).unwrap().id;

    send(&mut alex, &ClientMsg::Admin(AdminCmd::Admit { member: pending })).await;
    assert!(matches!(next(&mut lou).await, ServerMsg::Welcome { .. }));
}

//! Client against a real server, without Spotify (spectator mode).

use std::time::Duration;

use client::{Event, JoinOptions, join};
use protocol::{ClientMsg, ServerMsg, TrackInfo};
use tokio::net::TcpListener;

async fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, "ABC234".into(), "tok".into(), None));
    format!("ws://{addr}/ws")
}

fn opts(url: &str, code: &str) -> JoinOptions {
    JoinOptions { ws_url: url.into(), code: code.into(), name: "Lou".into(), host_token: Some("tok".into()), spotify_id: None, on_waiting: None }
}

#[tokio::test]
async fn joins_and_receives_playback() {
    let url = start().await;
    let mut joined = join(opts(&url, "abc234"), None).await.unwrap();
    assert_eq!(joined.room.settings.code, "ABC234");

    let track = TrackInfo { uri: "spotify:track:1".into(), name: "T".into(), artists: "A".into(), duration_ms: 60_000, album_uri: None, album_name: None, cover_url: None };
    assert!(joined.handle.send(ClientMsg::QueueAdd { track }));

    let playback = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(Event::Server(msg)) = joined.events.recv().await
                && let ServerMsg::Playback(p) = *msg
            {
                return p;
            }
        }
    })
    .await
    .unwrap();
    assert!(playback.is_playing);
    assert_eq!(playback.current.unwrap().added_by, "Lou");
}

#[tokio::test]
async fn wrong_code_is_reported() {
    let url = start().await;
    let err = join(opts(&url, "NOPE22"), None).await.err().unwrap();
    assert!(err.to_string().contains("code de salon incorrect"), "{err}");
}

/// TCP proxy whose live connections can be cut, to simulate a network drop.
struct Proxy {
    url: String,
    conns: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl Proxy {
    async fn start(target: std::net::SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let conns: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = Default::default();
        let list = conns.clone();
        tokio::spawn(async move {
            loop {
                let (mut inbound, _) = listener.accept().await.unwrap();
                let task = tokio::spawn(async move {
                    let mut outbound = tokio::net::TcpStream::connect(target).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
                list.lock().unwrap().push(task);
            }
        });
        Self { url, conns }
    }

    fn cut(&self) {
        for task in self.conns.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

#[tokio::test]
async fn reconnects_after_a_network_drop_and_keeps_its_place() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, "ABC234".into(), "tok".into(), None));
    let proxy = Proxy::start(addr).await;

    let mut joined = join(opts(&proxy.url, "ABC234"), None).await.unwrap();
    let me = joined.member_id;
    proxy.cut();

    let mut saw_reconnecting = false;
    let welcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match joined.events.recv().await.expect("events") {
                Event::Reconnecting { .. } => saw_reconnecting = true,
                Event::Server(msg) => {
                    if let ServerMsg::Welcome { member_id, .. } = *msg {
                        return member_id;
                    }
                }
                Event::Disconnected(reason) => panic!("gave up: {reason}"),
                _ => {}
            }
        }
    })
    .await
    .expect("reconnected in time");
    assert!(saw_reconnecting);
    assert_eq!(welcome, me, "same member after reconnection");
    assert!(joined.handle.send(ClientMsg::Pause));
}

#[tokio::test]
async fn kicked_client_does_not_reconnect() {
    use protocol::AdminCmd;
    let url = start().await;
    let host = join(opts(&url, "ABC234"), None).await.unwrap();
    let mut guest = join(JoinOptions { host_token: None, name: "Max".into(), ..opts(&url, "ABC234") }, None).await.unwrap();
    host.handle.send(ClientMsg::Admin(AdminCmd::Kick { member: guest.member_id }));
    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match guest.events.recv().await.expect("events") {
                Event::Disconnected(reason) => return reason,
                Event::Reconnecting { .. } => panic!("must not reconnect after a kick"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(reason.contains("exclu"), "{reason}");
}

#[tokio::test]
async fn closing_the_room_disconnects_members_for_good() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/ws", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(server::serve_until(listener, "ABC234".into(), "tok".into(), None, async {
        let _ = stopped.await;
    }));
    let mut guest = join(JoinOptions { host_token: None, ..opts(&url, "ABC234") }, None).await.unwrap();
    stop.send(()).unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match guest.events.recv().await.expect("events") {
                Event::Disconnected(reason) => return reason,
                Event::Reconnecting { .. } => panic!("must not reconnect to a closed room"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(reason.contains("fermé"), "{reason}");
    tokio::time::timeout(Duration::from_secs(5), server).await.expect("server stops").unwrap().unwrap();
}

/// Real peer-to-peer path through iroh's public lookup service and relays:
/// needs Internet, so not run by default (`cargo test -- --ignored`).
#[tokio::test]
#[ignore]
async fn joins_over_the_peer_to_peer_tunnel() {
    use client::tunnel::{GuestTunnel, HostTunnel, generate_secret, is_peer_id};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, "ABC234".into(), "tok".into(), None));

    let host = HostTunnel::start(generate_secret(), local).await.unwrap();
    assert!(host.online().await, "host should reach the relay");
    let id = host.id();
    assert!(is_peer_id(&id));

    let guest = GuestTunnel::start(&id).await.unwrap();
    let mut joined = join(JoinOptions { host_token: None, name: "Pair".into(), ..opts(&guest.ws_url(), "ABC234") }, None)
        .await
        .expect("join through the tunnel");
    assert_eq!(joined.room.settings.code, "ABC234");

    // Commands flow both ways through the tunnel.
    let track = TrackInfo { uri: "spotify:track:1".into(), name: "T".into(), artists: "A".into(), duration_ms: 60_000, album_uri: None, album_name: None, cover_url: None };
    let host_client = join(opts(&guest.ws_url(), "ABC234"), None).await.unwrap();
    host_client.handle.send(ClientMsg::QueueAdd { track });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Event::Server(msg)) = joined.events.recv().await
                && let ServerMsg::Playback(p) = *msg
                && p.is_playing
            {
                return;
            }
        }
    })
    .await
    .expect("playback received through the tunnel");
    guest.close().await;
    host.close().await;
}

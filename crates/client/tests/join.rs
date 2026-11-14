//! Client against a real server, without Spotify (spectator mode).

use std::time::Duration;

use client::{Event, JoinOptions, join};
use protocol::{ClientMsg, ServerMsg, TrackInfo};
use tokio::net::TcpListener;

async fn start() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(server::serve(listener, "ABC234".into(), "tok".into()));
    format!("ws://{addr}/ws")
}

fn opts(url: &str, code: &str) -> JoinOptions {
    JoinOptions { ws_url: url.into(), code: code.into(), name: "Lou".into(), host_token: None }
}

#[tokio::test]
async fn joins_and_receives_playback() {
    let url = start().await;
    let mut joined = join(opts(&url, "abc234"), None).await.unwrap();
    assert_eq!(joined.room.code, "ABC234");

    let track = TrackInfo { uri: "spotify:track:1".into(), name: "T".into(), artists: "A".into(), duration_ms: 60_000, album_uri: None, cover_url: None };
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

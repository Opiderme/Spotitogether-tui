//! MPRIS (D-Bus) player "Spotitogether": the room's track shows up in
//! Caelestia's bar, `playerctl` and media keys. Controls go through the same
//! permission checks as the keyboard.
//!
//! `mpris_server::Player` is not `Send`, so it lives on its own thread with a
//! single-threaded runtime; the TUI talks to it through channels.

use mpris_server::{Metadata, PlaybackStatus, Player, Time, TrackId};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Track { item_id: u64, title: String, artists: String, length_ms: u64, art_url: Option<String> },
    NoTrack,
    Status { playing: bool },
    /// Position changed by a seek (emits the `Seeked` signal).
    Seeked(u64),
    /// Periodic position refresh (no signal).
    Position(u64),
    Controls { playback: bool, skip: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    PlayPause,
    Play,
    Pause,
    Next,
    /// Relative seek in ms.
    Seek(i64),
    SetPosition(u64),
}

/// Starts the D-Bus player in the background (the room screen does not wait
/// for the bus). Without a session bus the thread just ends and updates go
/// nowhere.
pub fn spawn(commands: mpsc::UnboundedSender<Command>) -> Option<mpsc::UnboundedSender<Update>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("mpris".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, serve(commands, rx));
        })
        .ok()?;
    Some(tx)
}

async fn serve(
    commands: mpsc::UnboundedSender<Command>,
    mut updates: mpsc::UnboundedReceiver<Update>,
) {
    // One bus name per process: host and guest can run on the same machine.
    let suffix = format!("spotitogether.instance{}", std::process::id());
    let player = match Player::builder(&suffix)
        .identity("Spotitogether")
        .desktop_entry("spotitogether")
        .can_control(true)
        .build()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::info!(%e, "MPRIS unavailable");
            return;
        }
    };

    let send = |c: Command| {
        let tx = commands.clone();
        move |_: &Player| {
            let _ = tx.send(c);
        }
    };
    player.connect_play_pause(send(Command::PlayPause));
    player.connect_play(send(Command::Play));
    player.connect_pause(send(Command::Pause));
    player.connect_next(send(Command::Next));
    let tx = commands.clone();
    player.connect_seek(move |_, offset| {
        let _ = tx.send(Command::Seek(offset.as_millis()));
    });
    let tx = commands.clone();
    player.connect_set_position(move |_, _, position| {
        let _ = tx.send(Command::SetPosition(position.as_millis().max(0) as u64));
    });
    tokio::task::spawn_local(player.run());

    while let Some(update) = updates.recv().await {
        let result = match update {
            Update::Track { item_id, title, artists, length_ms, art_url } => {
                let mut meta = Metadata::builder()
                    .title(title)
                    .artist([artists])
                    .length(Time::from_millis(length_ms as i64));
                if let Ok(id) = TrackId::try_from(format!("/org/spotitogether/item/{item_id}")) {
                    meta = meta.trackid(id);
                }
                if let Some(url) = art_url {
                    meta = meta.art_url(url);
                }
                player.set_position(Time::ZERO);
                player.set_metadata(meta.build()).await
            }
            Update::NoTrack => {
                let _ = player.set_metadata(Metadata::new()).await;
                player.set_playback_status(PlaybackStatus::Stopped).await
            }
            Update::Status { playing } => {
                let status = if playing { PlaybackStatus::Playing } else { PlaybackStatus::Paused };
                player.set_playback_status(status).await
            }
            Update::Seeked(ms) => {
                player.set_position(Time::from_millis(ms as i64));
                player.seeked(Time::from_millis(ms as i64)).await
            }
            Update::Position(ms) => {
                player.set_position(Time::from_millis(ms as i64));
                Ok(())
            }
            Update::Controls { playback, skip } => {
                let _ = player.set_can_play(playback).await;
                let _ = player.set_can_pause(playback).await;
                let _ = player.set_can_seek(playback).await;
                player.set_can_go_next(skip).await
            }
        };
        if let Err(e) = result {
            tracing::warn!(%e, "MPRIS update failed");
        }
    }
}

/// What MPRIS currently shows, to send only the differences.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Shown {
    pub item_id: Option<u64>,
    pub playing: bool,
    pub seq: u64,
    pub controls: (bool, bool),
}

/// Updates needed to go from `shown` to the app state.
pub fn diff(shown: &mut Shown, app: &crate::app::App) -> Vec<Update> {
    let mut out = Vec::new();
    let p = &app.playback;
    let item_id = p.current.as_ref().map(|i| i.id);
    if item_id != shown.item_id {
        out.push(match &p.current {
            Some(item) => Update::Track {
                item_id: item.id,
                title: item.track.name.clone(),
                artists: item.track.artists.clone(),
                length_ms: item.track.duration_ms,
                art_url: item.track.cover_url.clone(),
            },
            None => Update::NoTrack,
        });
    } else if p.seq != shown.seq && item_id.is_some() {
        // Same track, new playback state: probably a seek.
        out.push(Update::Seeked(app.position_ms()));
    }
    if item_id.is_some() && (p.is_playing != shown.playing || item_id != shown.item_id) {
        out.push(Update::Status { playing: p.is_playing });
    }
    let perms = app.my_perms();
    let controls = (perms.playback, perms.skip || perms.listen);
    if controls != shown.controls {
        out.push(Update::Controls { playback: controls.0, skip: controls.1 });
    }
    *shown = Shown { item_id, playing: p.is_playing, seq: p.seq, controls };
    out
}

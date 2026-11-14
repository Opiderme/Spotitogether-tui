//! `serve` and `join`. In phase 2 `join` is a line-based client (type
//! commands such as `/add`); the full-screen TUI replaces it in phase 3.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use client::{Event, JoinOptions, SyncEvent, parse_invite};
use protocol::{ClientMsg, Member, Playback, QueueItem, Role, ServerMsg, TrackInfo};
use spotify::models::Track;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

use crate::config::project_dirs;
use crate::spotify_cmd::{connect, fmt_ms, parse_position, resolve_track};

// ------------------------------------------------------------------ serve

pub async fn serve(port: u16, bind: IpAddr, code: Option<String>) -> Result<()> {
    let code = code.map(|c| c.trim().to_uppercase()).unwrap_or_else(server::generate_code);
    if code.len() < 4 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("code invalide : au moins 4 lettres ou chiffres");
    }
    let host_token = server::generate_host_token();
    let listener = TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("impossible d'écouter sur {bind}:{port}"))?;
    let token_file = save_host_token(port, &host_token);

    // Listening on every interface: advertise the LAN address.
    let lan = if bind.is_unspecified() {
        lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "<ton-ip>".into())
    } else {
        bind.to_string()
    };
    println!("Salon {code} ouvert sur {bind}:{port}\n");
    println!("Invitation à partager : spotitogether://{lan}:{port}/{code}");
    println!("  (hors de ton réseau local : Tailscale, Caddy ou redirection de port, voir le README)\n");
    match token_file {
        Ok(_) => println!("Pour rejoindre en tant qu'hôte, sur cette machine :\n  spotitogether join spotitogether://127.0.0.1:{port}/{code}\n"),
        Err(_) => println!("Pour rejoindre en tant qu'hôte :\n  spotitogether join spotitogether://127.0.0.1:{port}/{code} --host-token {host_token}\n"),
    }
    println!("Ctrl+C pour fermer le salon.");

    server::serve(listener, code, host_token).await?;
    Ok(())
}

/// The host token is written to a 0600 file so that `join` on the same
/// machine gets the host role without copying it around.
fn host_token_path(port: u16) -> Result<PathBuf> {
    let dirs = project_dirs()?;
    let dir = dirs.runtime_dir().unwrap_or_else(|| dirs.cache_dir());
    Ok(dir.join(format!("host-token-{port}")))
}

fn save_host_token(port: u16, token: &str) -> Result<()> {
    let path = host_token_path(port)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(token.as_bytes())?;
    Ok(())
}

fn load_host_token(port: u16) -> Option<String> {
    fs::read_to_string(host_token_path(port).ok()?).ok().map(|s| s.trim().to_owned())
}

/// Local network address, found by asking the OS which interface it would
/// use to reach a public address (UDP connect sends no packet).
fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

// ------------------------------------------------------------------- join

pub struct JoinArgs {
    pub invite: String,
    pub code: Option<String>,
    pub name: Option<String>,
    pub host_token: Option<String>,
    pub spectator: bool,
}

#[derive(Default)]
struct View {
    playback: Option<Playback>,
    queue: Vec<QueueItem>,
    members: Vec<Member>,
}

const HELP: &str = "\
Commandes :
  /add <recherche | lien | URI>   ajoute un morceau à la file (texte sans / = /add)
  /queue                          affiche la file
  /rm <n>                         retire le morceau n de la file
  /play  /pause  /skip            contrôle du salon
  /seek <m:ss>                    se déplace dans le morceau
  /who                            membres du salon
  /quit                           quitte le salon";

pub async fn join(args: JoinArgs) -> Result<()> {
    let invite = parse_invite(&args.invite)?;
    let code = args.code.or(invite.code).context("code du salon manquant : spotitogether://hôte:port/CODE ou --code")?;

    let spotify = if args.spectator { None } else { Some(Arc::new(connect()?)) };
    let name = match (args.name, &spotify) {
        (Some(n), _) => n,
        (None, Some(sp)) => {
            let me = sp.me().await.context("lecture du profil Spotify")?;
            me.display_name.unwrap_or(me.id)
        }
        (None, None) => "Spectateur".into(),
    };
    let host_token = args.host_token.or_else(|| local_port(&invite.ws_url).and_then(load_host_token));

    let mut joined = client::join(
        JoinOptions { ws_url: invite.ws_url, code: code.to_uppercase(), name, host_token },
        spotify.clone(),
    )
    .await?;

    let mut view = View {
        playback: Some(joined.room.playback.clone()),
        queue: joined.room.queue.clone(),
        members: joined.room.members.clone(),
    };
    let me = view.members.iter().find(|m| m.id == joined.member_id);
    println!(
        "Dans le salon {} en tant que {}{}.",
        joined.room.code,
        me.map(|m| m.name.as_str()).unwrap_or("?"),
        if me.is_some_and(|m| m.role == Role::Host) { " (hôte)" } else { "" }
    );
    if spotify.is_none() {
        println!("Mode spectateur : rien n'est joué sur ton compte.");
    }
    print_members(&view.members);
    print_playback(&joined.room.playback);
    println!("Tape /help pour les commandes.\n");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            event = joined.events.recv() => match event {
                Some(Event::Disconnected(reason)) => bail!("déconnecté du salon : {reason}"),
                None => bail!("déconnecté du salon : connexion perdue"),
                Some(event) => show_event(event, &mut view),
            },
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()) }; // stdin closed
                match run_command(line.trim(), &view, &joined.handle, spotify.as_deref()).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(err) => println!("⚠ {err:#}"),
                }
            }
        }
    }
}

fn local_port(ws_url: &str) -> Option<u16> {
    let rest = ws_url.strip_prefix("ws://")?;
    let host_port = rest.split('/').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    matches!(host, "127.0.0.1" | "localhost").then(|| port.parse().ok()).flatten()
}

/// Returns `Ok(true)` to quit.
async fn run_command(
    line: &str,
    view: &View,
    room: &client::RoomHandle,
    spotify: Option<&spotify::SpotifyClient>,
) -> Result<bool> {
    if line.is_empty() {
        return Ok(false);
    }
    let (cmd, arg) = match line.strip_prefix('/') {
        Some(rest) => rest.split_once(' ').map(|(c, a)| (c, a.trim())).unwrap_or((rest, "")),
        None => ("add", line),
    };
    let msg = match cmd {
        "add" | "a" => {
            if arg.is_empty() {
                bail!("/add <recherche | lien | URI>");
            }
            let sp = spotify.context("la recherche nécessite un compte Spotify (pas en mode spectateur)")?;
            let track = resolve_track(sp, arg).await?;
            println!("+ {} — {}", track.name, track.artist_names());
            ClientMsg::QueueAdd { track: track_info(&track) }
        }
        "rm" => {
            let n: usize = arg.parse().context("/rm <numéro dans /queue>")?;
            let item = view.queue.get(n.wrapping_sub(1)).context("numéro hors de la file")?;
            ClientMsg::QueueRemove { id: item.id }
        }
        "play" => ClientMsg::Play,
        "pause" => ClientMsg::Pause,
        "skip" | "next" => ClientMsg::Skip,
        "seek" => ClientMsg::Seek { position_ms: parse_position(arg)? },
        "queue" | "q" => {
            print_queue(&view.queue);
            return Ok(false);
        }
        "who" => {
            print_members(&view.members);
            return Ok(false);
        }
        "help" | "h" | "?" => {
            println!("{HELP}");
            return Ok(false);
        }
        "quit" | "exit" => return Ok(true),
        other => bail!("commande inconnue : /{other} (voir /help)"),
    };
    if !room.send(msg) {
        bail!("connexion au salon perdue");
    }
    Ok(false)
}

fn track_info(t: &Track) -> TrackInfo {
    TrackInfo {
        uri: t.uri.clone(),
        name: t.name.clone(),
        artists: t.artist_names(),
        duration_ms: t.duration_ms,
        album_uri: Some(t.album.uri.clone()).filter(|u| !u.is_empty()),
        cover_url: t.cover_url().map(str::to_owned),
    }
}

fn show_event(event: Event, view: &mut View) {
    match event {
        Event::Server(msg) => show_server_msg(*msg, view),
        Event::Sync(e) => show_sync_event(e),
        Event::Disconnected(_) => {}
    }
}

fn show_server_msg(msg: ServerMsg, view: &mut View) {
    match msg {
        ServerMsg::Playback(p) => {
            let previous = view.playback.as_ref().and_then(|old| old.current.as_ref().map(|i| (i.id, old.is_playing)));
            match (previous, &p.current) {
                // Same track, same play state: only the position changed.
                (Some((id, playing)), Some(item)) if id == item.id && playing == p.is_playing => {
                    println!("⏩ {} — position {}", item.track.name, fmt_ms(p.position_ms));
                }
                (Some((id, _)), Some(item)) if id == item.id && p.is_playing => {
                    println!("▶ reprise à {}", fmt_ms(p.position_ms));
                }
                _ => print_playback(&p),
            }
            view.playback = Some(p);
        }
        ServerMsg::Queue { items } => {
            if items.len() != view.queue.len() {
                println!("File d'attente : {} morceau(x) (/queue pour la voir)", items.len());
            }
            view.queue = items;
        }
        ServerMsg::Members { members } => {
            for m in members.iter().filter(|m| !view.members.iter().any(|o| o.id == m.id)) {
                println!("→ {} a rejoint le salon", m.name);
            }
            for m in view.members.iter().filter(|m| !members.iter().any(|n| n.id == m.id)) {
                println!("← {} est parti", m.name);
            }
            view.members = members;
        }
        ServerMsg::Error { message, .. } => println!("⚠ {message}"),
        ServerMsg::Welcome { .. } | ServerMsg::Pong { .. } => {}
    }
}

fn show_sync_event(e: SyncEvent) {
    match e {
        SyncEvent::Applied { track: Some(_), position_ms, playing: true } => {
            println!("  ↳ synchro : lecture à {} sur ton Spotify", fmt_ms(position_ms));
        }
        SyncEvent::Applied { .. } => {}
        SyncEvent::Resynced { reason } => println!("  ↳ resynchro : {reason}"),
        SyncEvent::Corrected { drift_ms } => {
            println!("  ↳ dérive de {:.1} s corrigée", drift_ms as f64 / 1000.0);
        }
        SyncEvent::DeviceSelected { name } => println!("Appareil Spotify : {name}"),
        SyncEvent::Error(e) => println!("⚠ Spotify : {e}"),
    }
}

fn print_playback(p: &Playback) {
    match &p.current {
        None => println!("■ Rien en lecture : ajoute un morceau avec /add"),
        Some(item) if p.is_playing => println!(
            "▶ {} — {}  ({}, ajouté par {})",
            item.track.name,
            item.track.artists,
            fmt_ms(item.track.duration_ms),
            item.added_by
        ),
        Some(item) => println!("⏸ {} — {}  en pause à {}", item.track.name, item.track.artists, fmt_ms(p.position_ms)),
    }
}

fn print_queue(queue: &[QueueItem]) {
    if queue.is_empty() {
        println!("File d'attente vide.");
    }
    for (i, item) in queue.iter().enumerate() {
        println!("{:>3}. {} — {}  {}  ({})", i + 1, item.track.name, item.track.artists, fmt_ms(item.track.duration_ms), item.added_by);
    }
}

fn print_members(members: &[Member]) {
    let list: Vec<String> = members
        .iter()
        .map(|m| if m.role == Role::Host { format!("{} (hôte)", m.name) } else { m.name.clone() })
        .collect();
    println!("Membres : {}", list.join(", "));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_local_server_port() {
        assert_eq!(local_port("ws://127.0.0.1:7420/ws"), Some(7420));
        assert_eq!(local_port("ws://localhost:9000/ws"), Some(9000));
        assert_eq!(local_port("ws://192.168.1.2:7420/ws"), None);
    }
}

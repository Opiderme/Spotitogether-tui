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
use protocol::{ClientMsg, Member, Playback, QueueItem, ServerMsg};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

use crate::config::project_dirs;
use crate::spotify_cmd::{connect, fmt_ms, parse_position, resolve_track};

// ------------------------------------------------------------------ serve

pub struct ServeArgs {
    pub port: u16,
    pub bind: IpAddr,
    pub code: Option<String>,
    pub data: Option<PathBuf>,
    pub ephemeral: bool,
    pub public: Option<String>,
    pub tls: bool,
    pub no_p2p: bool,
}

pub async fn serve(args: ServeArgs) -> Result<()> {
    let ServeArgs { port, bind, code, data, ephemeral, public, tls, no_p2p } = args;
    let code = code.map(|c| c.trim().to_uppercase()).unwrap_or_else(server::generate_code);
    if code.len() < 4 || !code.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("code invalide : au moins 4 lettres ou chiffres");
    }
    let host_token = server::generate_host_token();
    let listener = TcpListener::bind((bind, port))
        .await
        .with_context(|| format!("impossible d'écouter sur {bind}:{port}"))?;
    let token_file = save_host_token(port, &host_token);
    let store = if ephemeral {
        None
    } else {
        let (store, path) = open_store(port, data)?;
        println!("Rôles, bannis et réglages enregistrés dans {}", path.display());
        Some(store)
    };

    // Peer-to-peer access: works from anywhere, nothing to configure.
    let tunnel = if no_p2p {
        None
    } else {
        match start_host_tunnel(port).await {
            Ok(t) => Some(t),
            Err(e) => {
                println!("⚠ accès pair-à-pair indisponible ({e:#}) : seul le lien direct marchera");
                None
            }
        }
    };
    let (lan, _) = if bind.is_unspecified() { invite_host() } else { (bind.to_string(), is_local_only(&bind.to_string())) };
    println!("Salon {code} ouvert sur {bind}:{port}\n");
    if let Some(t) = &tunnel {
        println!("Invitation à partager (marche partout) : spotitogether://{}/{code}", t.id());
    }
    let direct = invite_link(public.as_deref(), tls, &lan, port, &code);
    match (&public, &tunnel) {
        (Some(_), _) => println!("Invitation directe : {direct}"),
        (None, Some(_)) => println!("Sur ton réseau local uniquement : {direct}"),
        (None, None) => println!("Invitation (réseau local uniquement) : {direct}"),
    }
    println!();
    match token_file {
        Ok(_) => println!(
            "Pour rejoindre en tant qu'hôte, sur cette machine :\n  spotitogether join spotitogether://127.0.0.1:{port}/{code}\n\
             Pour tester en invité sur cette machine : ajoute --guest\n"
        ),
        Err(_) => println!("Pour rejoindre en tant qu'hôte :\n  spotitogether join spotitogether://127.0.0.1:{port}/{code} --host-token {host_token}\n"),
    }
    println!("Ctrl+C pour fermer le salon.");

    server::serve(listener, code, host_token, store).await?;
    Ok(())
}

/// SQLite store of the room (default `~/.local/share/spotitogether/room-<port>.sqlite`).
pub(crate) fn open_store(port: u16, path: Option<PathBuf>) -> Result<(server::Store, PathBuf)> {
    let path = match path {
        Some(p) => p,
        None => project_dirs()?.data_dir().join(format!("room-{port}.sqlite")),
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
    }
    let store = server::Store::open(&path).with_context(|| format!("ouverture de {}", path.display()))?;
    Ok((store, path))
}

/// Invite to print: the public address when given (`--public`), the LAN one
/// otherwise.
pub(crate) fn invite_link(public: Option<&str>, tls: bool, lan: &str, port: u16, code: &str) -> String {
    let scheme = if tls { "spotitogether+tls" } else { "spotitogether" };
    match public {
        Some(addr) => format!("{scheme}://{}/{code}", addr.trim_end_matches('/')),
        None => format!("{scheme}://{lan}:{port}/{code}"),
    }
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

pub(crate) fn load_host_token(port: u16) -> Option<String> {
    fs::read_to_string(host_token_path(port).ok()?).ok().map(|s| s.trim().to_owned())
}

/// Local network address, found by asking the OS which interface it would
/// use to reach a public address (UDP connect sends no packet).
pub(crate) fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(1, 1, 1, 1), 80)).ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// LAN address for direct invites, and whether it only works locally.
pub(crate) fn invite_host() -> (String, bool) {
    match lan_ip() {
        Some(ip) => (ip.to_string(), true),
        None => ("127.0.0.1".into(), true),
    }
}

/// Peer-to-peer host key, kept in `~/.local/share/spotitogether/p2p-host.key`
/// (0600) so the invite address stays the same between sessions.
pub(crate) fn host_secret() -> Result<[u8; 32]> {
    let path = project_dirs()?.data_dir().join("p2p-host.key");
    if let Ok(bytes) = fs::read(&path)
        && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return Ok(key);
    }
    let key = client::tunnel::generate_secret();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(&key)?;
    Ok(key)
}

/// Starts the peer-to-peer access to a room server listening on `port`.
pub(crate) async fn start_host_tunnel(port: u16) -> Result<client::tunnel::HostTunnel> {
    let local = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    Ok(client::tunnel::HostTunnel::start(host_secret()?, local).await?)
}

/// True for addresses that only exist on a local network (or this machine).
pub(crate) fn is_local_only(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => host == "localhost",
    }
}

// ------------------------------------------------------------------- join

pub struct JoinArgs {
    pub invite: String,
    pub code: Option<String>,
    pub name: Option<String>,
    pub host_token: Option<String>,
    pub guest: bool,
    pub spectator: bool,
    pub plain: bool,
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
  /play  /pause  /skip            contrôle du salon (selon ton rôle)
  /vote                           vote pour passer le morceau
  /seek <m:ss>                    se déplace dans le morceau
  /who                            membres du salon
  /quit                           quitte le salon";

pub async fn join(args: JoinArgs) -> Result<()> {
    let invite = parse_invite(&args.invite)?;
    let code = args.code.or(invite.code.clone()).context("code du salon manquant : spotitogether://hôte:port/CODE ou --code")?;
    // Peer-to-peer invite: the tunnel lives as long as this session.
    let (ws_url, _tunnel) = invite.connect_url().await?;

    let spotify = if args.spectator { None } else { Some(Arc::new(connect()?)) };
    let profile = match &spotify {
        Some(sp) => Some(sp.me().await.context("lecture du profil Spotify")?),
        None => None,
    };
    let name = match (args.name, &profile) {
        (Some(n), _) => n,
        (None, Some(me)) => me.display_name.clone().unwrap_or_else(|| me.id.clone()),
        (None, None) => "Spectateur".into(),
    };
    // On the server's machine the host token is picked up automatically,
    // unless `--guest` asks to join as a regular member.
    let host_token = match args.guest {
        true => None,
        false => args.host_token.or_else(|| local_port(&invite.ws_url).and_then(load_host_token)),
    };

    let mut joined = client::join(
        JoinOptions {
            ws_url,
            code: code.to_uppercase(),
            name,
            host_token,
            spotify_id: profile.map(|p| p.id),
            on_waiting: Some(Box::new(|| println!("Salle d'attente : l'hôte doit t'admettre…"))),
        },
        spotify.clone(),
    )
    .await?;

    if !args.plain {
        let options = tui::RunOptions { invite: Some(args.invite.clone()), hosting: false, notice: None };
        return match tui::run(joined, spotify, options).await? {
            Some(reason) => bail!("déconnecté du salon : {reason}"),
            None => Ok(()),
        };
    }

    let mut view = View {
        playback: Some(joined.room.playback.clone()),
        queue: joined.room.queue.clone(),
        members: joined.room.members.clone(),
    };
    let me = view.members.iter().find(|m| m.id == joined.member_id);
    println!(
        "Dans le salon {} en tant que {}{}.",
        joined.room.settings.code,
        me.map(|m| m.name.as_str()).unwrap_or("?"),
        me.map(|m| format!(" ({})", m.role.label().to_lowercase())).unwrap_or_default()
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

pub(crate) fn local_port(ws_url: &str) -> Option<u16> {
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
            ClientMsg::QueueAdd { track: client::track_info(&track) }
        }
        "rm" => {
            let n: usize = arg.parse().context("/rm <numéro dans /queue>")?;
            let item = view.queue.get(n.wrapping_sub(1)).context("numéro hors de la file")?;
            ClientMsg::QueueRemove { id: item.id }
        }
        "play" => ClientMsg::Play,
        "pause" => ClientMsg::Pause,
        "skip" | "next" => ClientMsg::Skip,
        "vote" => ClientMsg::VoteSkip,
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

fn show_event(event: Event, view: &mut View) {
    match event {
        Event::Server(msg) => show_server_msg(*msg, view),
        Event::Sync(e) => show_sync_event(e),
        Event::Reconnecting { attempt, reason } => println!("⚠ connexion perdue ({reason}), reconnexion… essai {attempt}"),
        Event::Reconnected => println!("Reconnecté au salon."),
        Event::Clock { .. } | Event::Disconnected(_) => {}
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
        ServerMsg::SkipVotes { votes, needed } if votes > 0 => println!("Votes pour passer : {votes}/{needed}"),
        ServerMsg::Settings(s) => println!("Code du salon : {}", s.code),
        ServerMsg::Welcome { room, .. } => {
            view.queue = room.queue;
            view.members = room.members;
            print_playback(&room.playback);
            view.playback = Some(room.playback);
        }
        ServerMsg::Pong { .. } | ServerMsg::Waiting | ServerMsg::SkipVotes { .. } => {}
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
        SyncEvent::Stopped => println!("  ↳ écoute désactivée par l'hôte"),
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
        .filter(|m| !m.pending)
        .map(|m| format!("{} ({})", m.name, m.role.label().to_lowercase()))
        .collect();
    println!("Membres : {}", list.join(", "));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_invite_links() {
        assert_eq!(invite_link(None, false, "192.168.1.2", 7420, "AB"), "spotitogether://192.168.1.2:7420/AB");
        assert_eq!(invite_link(Some("salon.fr/"), true, "x", 7420, "AB"), "spotitogether+tls://salon.fr/AB");
        assert_eq!(invite_link(Some("pc.ts.net:7420"), false, "x", 7420, "AB"), "spotitogether://pc.ts.net:7420/AB");
    }

    #[test]
    fn classifies_addresses() {
        assert!(is_local_only("192.168.1.19"));
        assert!(is_local_only("127.0.0.1"));
        assert!(!is_local_only("salon.exemple.fr"));
    }

    #[test]
    fn detects_local_server_port() {
        assert_eq!(local_port("ws://127.0.0.1:7420/ws"), Some(7420));
        assert_eq!(local_port("ws://localhost:9000/ws"), Some(9000));
        assert_eq!(local_port("ws://192.168.1.2:7420/ws"), None);
    }
}

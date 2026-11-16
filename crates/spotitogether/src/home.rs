//! `spotitogether` without arguments: home screen, then create a room hosted
//! by this process or join one, and come back to the home screen afterwards.

use std::io::{self, BufRead};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use client::{JoinOptions, parse_invite};
use spotify::SpotifyClient;
use spotify::models::User;
use tokio::net::TcpListener;
use tui::launcher::{Account, LauncherChoice, LauncherInfo};

use crate::config::Config;
use crate::room_cmd::{invite_host, invite_link, is_local_only, load_host_token, local_port, open_store, start_host_tunnel};
use crate::{secrets, spotify_cmd};

/// Ports tried, in order, when creating a room.
const HOST_PORTS: std::ops::Range<u16> = protocol::DEFAULT_PORT..protocol::DEFAULT_PORT + 10;

type Profile = (Arc<SpotifyClient>, User);

pub async fn run(invite: Option<String>) -> Result<()> {
    let mut message: Option<(String, bool)> = None;
    let mut auto_join = invite.is_some();
    let mut prefill = invite;
    let mut profile: Option<Profile> = None;
    loop {
        let mut config = Config::load()?;
        // The home screen shows at once; the Spotify account (keyring and a
        // network request) is checked in the background.
        let (account, mut check, account_check) = match (&profile, &config.client_id) {
            (Some((_, me)), _) => (Account::Ready(display_name(me)), None, None),
            (None, None) => (Account::NotConfigured, None, None),
            (None, Some(client_id)) => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let client_id = client_id.clone();
                let task = tokio::spawn(async move {
                    let (account, profile, message) = check_account(client_id).await;
                    let _ = tx.send((account, message));
                    profile
                });
                (Account::Checking, Some(task), Some(rx))
            }
        };
        let info = LauncherInfo {
            account,
            invite: prefill.take().or_else(|| config.last_invite.clone()).unwrap_or_default(),
            message: message.take(),
            auto_join,
            account_check,
        };
        auto_join = false;

        let choice = tui::launcher::run(info).await?;
        // Creating or joining needs the profile: wait for the check if it is
        // still running (usually already done).
        if let Some(task) = check.take() {
            if choice == LauncherChoice::Quit {
                task.abort();
            } else {
                profile = task.await.ok().flatten();
            }
        }
        match choice {
            LauncherChoice::Quit => return Ok(()),
            LauncherChoice::Setup => {
                if let Err(e) = crate::setup(None) {
                    message = Some((format!("{e:#}"), true));
                }
                pause();
            }
            LauncherChoice::Login => {
                profile = None;
                match crate::login().await {
                    Ok(()) => message = Some(("Connecté à Spotify.".into(), false)),
                    Err(e) => message = Some((format!("{e:#}"), true)),
                }
                pause();
            }
            LauncherChoice::Host => {
                message = Some(match host(profile.clone()).await {
                    Ok(None) => ("Salon fermé.".into(), false),
                    Ok(Some(reason)) => (format!("Salon interrompu : {reason}"), true),
                    Err(e) => (format!("{e:#}"), true),
                });
            }
            LauncherChoice::Join { invite } => {
                config.last_invite = Some(invite.clone());
                if let Err(e) = config.save() {
                    tracing::warn!(%e, "could not remember the invite");
                }
                message = Some(match join(&invite, profile.clone()).await {
                    Ok(None) => ("Tu as quitté le salon.".into(), false),
                    Ok(Some(reason)) => (format!("Déconnecté : {reason}"), true),
                    Err(e) => (format!("{e:#}"), true),
                });
                prefill = Some(invite);
            }
        }
    }
}

/// Spotify account: one keyring read (blocking D-Bus call, off the async
/// threads) and the profile request.
async fn check_account(client_id: String) -> (Account, Option<Profile>, Option<(String, bool)>) {
    let token = tokio::task::spawn_blocking(secrets::load).await.ok().and_then(|r| r.ok()).flatten();
    let Some(token) = token else { return (Account::NotLoggedIn, None, None) };
    let client = Arc::new(spotify_cmd::client_from(client_id, token));
    match client.me().await {
        Ok(me) => (Account::Ready(display_name(&me)), Some((client, me)), None),
        Err(e) => (Account::NotLoggedIn, None, Some((format!("Spotify : {e}"), true))),
    }
}

fn display_name(me: &User) -> String {
    me.display_name.clone().unwrap_or_else(|| me.id.clone())
}

fn guest_name(profile: &Option<Profile>) -> String {
    match profile {
        Some((_, me)) => display_name(me),
        None => std::env::var("USER").unwrap_or_else(|_| "Spectateur".into()),
    }
}

/// `192.168.1.19` from `ws://192.168.1.19:7420/ws`.
fn host_of(ws_url: &str) -> String {
    let rest = ws_url.split("://").nth(1).unwrap_or(ws_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.rsplit_once(':').map_or(authority, |(h, _)| h).to_owned()
}

/// Turns network errors into something actionable.
fn explain(e: client::Error, host: &str, peer: bool) -> anyhow::Error {
    match e {
        client::Error::Ws(_) | client::Error::Protocol(_) if peer => {
            anyhow::anyhow!("impossible de joindre l'hôte : son salon est fermé ou il est hors ligne")
        }
        client::Error::Ws(_) if is_local_only(host) => anyhow::anyhow!(
            "impossible de joindre {host} : cette adresse n'existe que sur le réseau local de l'hôte. \
             Demande-lui le lien affiché par « Créer un salon » : il marche depuis n'importe où."
        ),
        client::Error::Ws(_) => anyhow::anyhow!(
            "impossible de joindre {host} : le salon est fermé, l'adresse est fausse, ou un pare-feu bloque le port"
        ),
        other => other.into(),
    }
}

fn pause() {
    println!("\nAppuie sur Entrée pour revenir à l'accueil…");
    let _ = io::stdin().lock().read_line(&mut String::new());
}

/// Hosts a room in this process and joins it as host. The room closes when
/// the host leaves.
async fn host(profile: Option<Profile>) -> Result<Option<String>> {
    let (listener, port) = bind_free_port().await?;
    let code = server::generate_code();
    let host_token = server::generate_host_token();
    let store = match open_store(port, None) {
        Ok((store, _)) => Some(store),
        Err(e) => {
            tracing::warn!(%e, "room settings will not be saved");
            None
        }
    };
    // Peer-to-peer link (works from anywhere); LAN link if that fails.
    let tunnel = start_host_tunnel(port).await;
    let (invite, notice) = match &tunnel {
        Ok(t) => (format!("spotitogether://{}/{code}", t.id()), None),
        Err(e) => {
            tracing::warn!(%e, "peer-to-peer unavailable");
            let (address, _) = invite_host();
            let notice = format!("Accès Internet indisponible ({e:#}) : ce lien ne marche que sur ton réseau local");
            (invite_link(None, false, &address, port, &code), Some(notice))
        }
    };

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(server::serve_until(listener, code.clone(), host_token.clone(), store, async {
        let _ = stopped.await;
    }));

    let spotify = profile.as_ref().map(|(sp, _)| sp.clone());
    let joined = client::join(
        JoinOptions {
            ws_url: format!("ws://127.0.0.1:{port}{}", protocol::WS_PATH),
            code,
            name: guest_name(&profile),
            host_token: Some(host_token),
            spotify_id: profile.as_ref().map(|(_, me)| me.id.clone()),
            on_waiting: None,
        },
        spotify.clone(),
    )
    .await;
    let result = match joined {
        Ok(joined) => tui::run(joined, spotify, tui::RunOptions { invite: Some(invite), hosting: true, notice }).await,
        Err(e) => Err(e.into()),
    };

    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), server).await;
    // Closing the peer-to-peer endpoint can take a moment: not on the way
    // back to the home screen.
    if let Ok(t) = tunnel {
        tokio::spawn(t.close());
    }
    result
}

/// First free port from 7420, on every interface (reachable from the LAN).
async fn bind_free_port() -> Result<(TcpListener, u16)> {
    for port in HOST_PORTS {
        if let Ok(listener) = TcpListener::bind((IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).await {
            return Ok((listener, port));
        }
    }
    bail!("aucun port libre entre {} et {}", HOST_PORTS.start, HOST_PORTS.end - 1)
}

async fn join(invite_text: &str, profile: Option<Profile>) -> Result<Option<String>> {
    let invite = parse_invite(invite_text)?;
    let code = invite.code.clone().context("le lien doit contenir le code du salon : spotitogether://hôte:port/CODE")?;
    // A `serve` running on this machine: get the host role, as `join` does.
    let host_token = local_port(&invite.ws_url).and_then(load_host_token);

    let host = invite.peer.clone().unwrap_or_else(|| host_of(&invite.ws_url));
    println!("Connexion au salon…");
    // Peer-to-peer invite: the tunnel lives as long as this session.
    let (ws_url, tunnel) = invite.connect_url().await?;
    let spotify = profile.as_ref().map(|(sp, _)| sp.clone());
    let joined = client::join(
        JoinOptions {
            ws_url,
            code,
            name: guest_name(&profile),
            host_token,
            spotify_id: profile.as_ref().map(|(_, me)| me.id.clone()),
            on_waiting: Some(Box::new(|| println!("Salle d'attente : l'hôte doit t'admettre…"))),
        },
        spotify.clone(),
    )
    .await
    .map_err(|e| explain(e, &host, invite.peer.is_some()))?;
    let options = tui::RunOptions { invite: Some(invite_text.to_owned()), hosting: false, notice: None };
    let result = tui::run(joined, spotify, options).await;
    if let Some(t) = tunnel {
        tokio::spawn(t.close());
    }
    result
}

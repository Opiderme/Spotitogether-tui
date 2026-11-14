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
use crate::room_cmd::{invite_link, lan_ip, load_host_token, local_port, open_store};
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
        let account = account(&config, &mut profile, &mut message).await;
        let info = LauncherInfo {
            account,
            invite: prefill.take().or_else(|| config.last_invite.clone()).unwrap_or_default(),
            message: message.take(),
            auto_join,
        };
        auto_join = false;

        match tui::launcher::run(info).await? {
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

/// Spotify account state; the profile is cached between home screens.
async fn account(config: &Config, profile: &mut Option<Profile>, message: &mut Option<(String, bool)>) -> Account {
    if let Some((_, me)) = profile {
        return Account::Ready(display_name(me));
    }
    if config.client_id.is_none() {
        return Account::NotConfigured;
    }
    if !matches!(secrets::load(), Ok(Some(_))) {
        return Account::NotLoggedIn;
    }
    let client = match spotify_cmd::connect() {
        Ok(c) => Arc::new(c),
        Err(_) => return Account::NotLoggedIn,
    };
    match client.me().await {
        Ok(me) => {
            let name = display_name(&me);
            *profile = Some((client, me));
            Account::Ready(name)
        }
        Err(e) => {
            message.get_or_insert((format!("Spotify : {e}"), true));
            Account::NotLoggedIn
        }
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
    let lan = lan_ip().map(|ip| ip.to_string()).unwrap_or_else(|| "127.0.0.1".into());
    let invite = invite_link(None, false, &lan, port, &code);

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
        Ok(joined) => tui::run(joined, spotify, tui::RunOptions { invite: Some(invite), hosting: true }).await,
        Err(e) => Err(e.into()),
    };

    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(3), server).await;
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

    println!("Connexion à {}…", invite.ws_url);
    let spotify = profile.as_ref().map(|(sp, _)| sp.clone());
    let joined = client::join(
        JoinOptions {
            ws_url: invite.ws_url,
            code,
            name: guest_name(&profile),
            host_token,
            spotify_id: profile.as_ref().map(|(_, me)| me.id.clone()),
            on_waiting: Some(Box::new(|| println!("Salle d'attente : l'hôte doit t'admettre…"))),
        },
        spotify.clone(),
    )
    .await?;
    let options = tui::RunOptions { invite: Some(invite_text.to_owned()), hosting: false };
    tui::run(joined, spotify, options).await
}

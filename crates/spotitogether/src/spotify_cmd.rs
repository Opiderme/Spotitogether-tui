//! `spotitogether spotify ...`: direct Spotify commands used to validate an
//! installation (phase 1) before rooms and the TUI exist.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use spotify::SpotifyClient;
use spotify::models::{Device, Track};

use crate::config::Config;
use crate::secrets;

#[derive(Subcommand)]
pub enum SpotifyCmd {
    /// Affiche le compte connecté
    Whoami,
    /// Liste tes appareils Spotify (app, spotifyd, téléphone…)
    Devices,
    /// Morceau en cours et position
    Status,
    /// Cherche des morceaux (10 résultats max par page)
    Search {
        /// Texte recherché
        query: Vec<String>,
        /// Décalage pour voir les résultats suivants
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Lance un morceau (URI, lien open.spotify.com ou recherche) ; sans argument, reprend la lecture
    Play {
        /// spotify:track:…, https://open.spotify.com/track/… ou texte à chercher
        target: Vec<String>,
        /// Appareil (nom ou id) ; par défaut l'appareil actif
        #[arg(long, short)]
        device: Option<String>,
        /// Position de départ (m:ss ou secondes)
        #[arg(long)]
        at: Option<String>,
    },
    /// Met en pause
    Pause {
        #[arg(long, short)]
        device: Option<String>,
    },
    /// Se déplace dans le morceau (m:ss ou secondes)
    Seek {
        position: String,
        #[arg(long, short)]
        device: Option<String>,
    },
    /// Transfère la lecture vers un appareil (nom ou id)
    Transfer { device: String },
}

pub async fn run(cmd: SpotifyCmd) -> Result<()> {
    let client = connect()?;
    match cmd {
        SpotifyCmd::Whoami => {
            let me = client.me().await?;
            println!("{} ({})", me.display_name.as_deref().unwrap_or("sans nom"), me.id);
        }
        SpotifyCmd::Devices => {
            let devices = client.devices().await?;
            if devices.is_empty() {
                println!("Aucun appareil. Ouvre Spotify (ou lance spotifyd) puis réessaie.");
            }
            for d in devices {
                println!(
                    "{} {:<28} {:<12} {}{}",
                    if d.is_active { "●" } else { " " },
                    d.name,
                    d.kind,
                    d.id.as_deref().unwrap_or("-"),
                    if d.is_restricted { "  (non contrôlable)" } else { "" }
                );
            }
        }
        SpotifyCmd::Status => match client.playback().await? {
            None => println!("Rien en cours de lecture."),
            Some(p) => {
                let device = p.device.as_ref().map(|d| d.name.as_str()).unwrap_or("?");
                match &p.item {
                    Some(t) => println!(
                        "{} {}  {} / {}  sur {device}",
                        if p.is_playing { "▶" } else { "⏸" },
                        describe(t),
                        fmt_ms(p.progress_ms.unwrap_or(0)),
                        fmt_ms(t.duration_ms),
                    ),
                    None => println!("Lecture sur {device} (contenu inconnu)"),
                }
            }
        },
        SpotifyCmd::Search { query, offset } => {
            let query = join_query(&query)?;
            let page = client.search_tracks(&query, 10, offset).await?;
            if page.items.is_empty() {
                println!("Aucun résultat.");
            }
            for (i, t) in page.items.iter().enumerate() {
                println!("{:>3}. {:<60} {:>5}  {}", offset as usize + i + 1, describe(t), fmt_ms(t.duration_ms), t.uri);
            }
            if page.total > offset + page.items.len() as u32 {
                println!("… {} résultats au total, suite avec --offset {}", page.total, offset + 10);
            }
        }
        SpotifyCmd::Play { target, device, at } => {
            let device_id = resolve_device(&client, device.as_deref()).await?;
            let position = at.as_deref().map(parse_position).transpose()?.unwrap_or(0);
            if target.is_empty() {
                client.resume(device_id.as_deref()).await?;
                println!("Lecture reprise.");
            } else {
                let track = resolve_track(&client, &join_query(&target)?).await?;
                client.play(device_id.as_deref(), std::slice::from_ref(&track.uri), position).await?;
                println!("▶ {}  à partir de {}", describe(&track), fmt_ms(position));
            }
        }
        SpotifyCmd::Pause { device } => {
            let device_id = resolve_device(&client, device.as_deref()).await?;
            client.pause(device_id.as_deref()).await?;
            println!("En pause.");
        }
        SpotifyCmd::Seek { position, device } => {
            let device_id = resolve_device(&client, device.as_deref()).await?;
            let ms = parse_position(&position)?;
            client.seek(device_id.as_deref(), ms).await?;
            println!("Position : {}", fmt_ms(ms));
        }
        SpotifyCmd::Transfer { device } => {
            let id = resolve_device(&client, Some(&device)).await?.expect("device given");
            client.transfer(&id, true).await?;
            println!("Lecture transférée.");
        }
    }
    Ok(())
}

fn connect() -> Result<SpotifyClient> {
    let config = Config::load()?;
    let client_id = config.require_client_id()?.to_owned();
    let token = secrets::load()?.context("pas connecté à Spotify : lance `spotitogether login`")?;
    Ok(SpotifyClient::new(client_id, token).with_token_sink(Box::new(|t| {
        if let Err(err) = secrets::save(t) {
            tracing::warn!(%err, "could not persist refreshed token");
        }
    })))
}

fn join_query(parts: &[String]) -> Result<String> {
    let q = parts.join(" ").trim().to_owned();
    if q.is_empty() {
        bail!("recherche vide");
    }
    Ok(q)
}

/// Accepts a device id or a (case-insensitive, partial) device name.
/// Without one, keeps the active device; if none is active, Spotify would
/// answer NO_ACTIVE_DEVICE, so the first controllable device is picked.
async fn resolve_device(client: &SpotifyClient, wanted: Option<&str>) -> Result<Option<String>> {
    let devices = client.devices().await?;
    let Some(wanted) = wanted else {
        if devices.iter().any(|d| d.is_active) {
            return Ok(None);
        }
        return match pick_idle_device(&devices) {
            Some(d) => {
                println!("Aucun appareil actif, utilisation de « {} ».", d.name);
                Ok(d.id.clone())
            }
            None => Err(spotify::Error::NoActiveDevice.into()),
        };
    };
    let lower = wanted.to_lowercase();
    let found: Option<&Device> = devices
        .iter()
        .find(|d| d.id.as_deref() == Some(wanted))
        .or_else(|| devices.iter().find(|d| d.name.to_lowercase() == lower))
        .or_else(|| devices.iter().find(|d| d.name.to_lowercase().contains(&lower)));
    match found.and_then(|d| d.id.clone()) {
        Some(id) => Ok(Some(id)),
        None => {
            let names: Vec<_> = devices.iter().map(|d| d.name.as_str()).collect();
            bail!("appareil « {wanted} » introuvable ; disponibles : {}", names.join(", "))
        }
    }
}

/// First controllable device, preferring this computer over phones/speakers.
fn pick_idle_device(devices: &[Device]) -> Option<&Device> {
    let usable = || devices.iter().filter(|d| d.id.is_some() && !d.is_restricted);
    usable().find(|d| d.kind == "Computer").or_else(|| usable().next())
}

/// URI, open.spotify.com link, or a search whose first result is used.
async fn resolve_track(client: &SpotifyClient, target: &str) -> Result<Track> {
    if let Some(id) = track_id_from(target) {
        return Ok(client.track(&id).await?);
    }
    let page = client.search_tracks(target, 1, 0).await?;
    page.items
        .into_iter()
        .next()
        .with_context(|| format!("aucun morceau trouvé pour « {target} »"))
}

fn track_id_from(target: &str) -> Option<String> {
    if let Some(id) = target.strip_prefix("spotify:track:") {
        return Some(id.to_owned());
    }
    let url = url_path(target)?;
    // https://open.spotify.com/track/<id>  or  /intl-fr/track/<id>
    let mut segments = url.split('/').filter(|s| !s.is_empty());
    segments.find(|s| *s == "track")?;
    segments.next().map(str::to_owned)
}

fn url_path(target: &str) -> Option<&str> {
    let rest = target
        .strip_prefix("https://open.spotify.com")
        .or_else(|| target.strip_prefix("http://open.spotify.com"))?;
    Some(rest.split(['?', '#']).next().unwrap_or(rest))
}

/// "1:23" -> 83 000 ms, "83" -> 83 000 ms.
fn parse_position(s: &str) -> Result<u64> {
    let s = s.trim();
    let secs = match s.split_once(':') {
        Some((m, sec)) => {
            let m: u64 = m.parse().context("minutes invalides")?;
            let sec: u64 = sec.parse().context("secondes invalides")?;
            if sec >= 60 {
                bail!("secondes invalides : {sec}");
            }
            m * 60 + sec
        }
        None => s.parse().with_context(|| format!("position invalide : {s} (m:ss ou secondes)"))?,
    };
    Ok(secs * 1000)
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn describe(t: &Track) -> String {
    let artists = t.artist_names();
    if artists.is_empty() { t.name.clone() } else { format!("{} — {artists}", t.name) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positions() {
        assert_eq!(parse_position("1:23").unwrap(), 83_000);
        assert_eq!(parse_position("90").unwrap(), 90_000);
        assert!(parse_position("1:75").is_err());
        assert!(parse_position("abc").is_err());
    }

    #[test]
    fn extracts_track_ids() {
        assert_eq!(track_id_from("spotify:track:abc").as_deref(), Some("abc"));
        assert_eq!(track_id_from("https://open.spotify.com/track/abc?si=x").as_deref(), Some("abc"));
        assert_eq!(track_id_from("https://open.spotify.com/intl-fr/track/abc").as_deref(), Some("abc"));
        assert_eq!(track_id_from("daft punk"), None);
    }

    fn dev(name: &str, kind: &str, restricted: bool) -> Device {
        Device {
            id: Some(name.to_owned()),
            name: name.to_owned(),
            kind: kind.to_owned(),
            is_active: false,
            is_restricted: restricted,
            volume_percent: None,
        }
    }

    #[test]
    fn picks_computer_then_any_controllable_device() {
        let devices = [dev("tel", "Smartphone", false), dev("pc", "Computer", false)];
        assert_eq!(pick_idle_device(&devices).unwrap().name, "pc");
        let devices = [dev("enceinte", "Speaker", true), dev("tel", "Smartphone", false)];
        assert_eq!(pick_idle_device(&devices).unwrap().name, "tel");
        assert!(pick_idle_device(&[dev("x", "Speaker", true)]).is_none());
    }

    #[test]
    fn formats_ms() {
        assert_eq!(fmt_ms(220_000), "3:40");
        assert_eq!(fmt_ms(5_000), "0:05");
    }
}

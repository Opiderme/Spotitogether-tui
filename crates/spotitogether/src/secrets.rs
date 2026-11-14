//! Spotify token storage: Secret Service (gnome-keyring, KWallet...) first,
//! falling back to a 0600 file when no keyring is reachable.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use spotify::Token;

use crate::config::project_dirs;

const SERVICE: &str = "spotitogether";
const ACCOUNT: &str = "spotify-token";

pub enum Backend {
    Keyring,
    File(PathBuf),
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Keyring => write!(f, "trousseau système (Secret Service)"),
            Backend::File(p) => write!(f, "fichier {} (droits 600)", p.display()),
        }
    }
}

fn fallback_path() -> Result<PathBuf> {
    Ok(project_dirs()?.data_dir().join("token.json"))
}

fn entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT)
}

pub fn save(token: &Token) -> Result<Backend> {
    let json = serde_json::to_string(token)?;
    match entry().and_then(|e| e.set_password(&json)) {
        Ok(()) => {
            // Do not leave a stale plaintext copy behind.
            if let Ok(p) = fallback_path() {
                let _ = fs::remove_file(p);
            }
            Ok(Backend::Keyring)
        }
        Err(err) => {
            tracing::warn!(%err, "keyring unavailable, using file fallback");
            let path = fallback_path()?;
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("écriture de {}", path.display()))?;
            f.write_all(json.as_bytes())?;
            Ok(Backend::File(path))
        }
    }
}

pub fn load() -> Result<Option<Token>> {
    match entry().and_then(|e| e.get_password()) {
        Ok(json) => return Ok(Some(serde_json::from_str(&json).context("jeton du trousseau illisible")?)),
        Err(keyring::Error::NoEntry) => {}
        Err(err) => tracing::warn!(%err, "keyring unavailable, trying file fallback"),
    }
    let path = fallback_path()?;
    match fs::read_to_string(&path) {
        Ok(json) => Ok(Some(serde_json::from_str(&json).context("fichier de jeton illisible")?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("lecture de {}", path.display())),
    }
}

pub fn delete() -> Result<()> {
    match entry().and_then(|e| e.delete_credential()) {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(err) => tracing::warn!(%err, "could not delete keyring entry"),
    }
    match fs::remove_file(fallback_path()?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

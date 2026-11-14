use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// `~/.config/spotitogether/config.toml`
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Client ID of the user's own Spotify app (see `spotitogether setup`).
    pub client_id: Option<String>,
    /// Last invite joined from the home screen.
    #[serde(default)]
    pub last_invite: Option<String>,
}

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "spotitogether").context("impossible de déterminer le dossier personnel")
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(project_dirs()?.config_dir().join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("config invalide : {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("lecture de {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("création de {}", dir.display()))?;
        }
        fs::write(&path, toml::to_string_pretty(self)?).with_context(|| format!("écriture de {}", path.display()))?;
        Ok(path)
    }

    pub fn require_client_id(&self) -> Result<&str> {
        self.client_id
            .as_deref()
            .context("aucun Client ID Spotify configuré : lance d'abord `spotitogether setup`")
    }
}

/// A Spotify Client ID is 32 lowercase hex characters.
pub fn is_valid_client_id(id: &str) -> bool {
    id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_client_id() {
        assert!(is_valid_client_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_client_id("too-short"));
        assert!(!is_valid_client_id("0123456789abcdef0123456789abcdeg"));
    }
}

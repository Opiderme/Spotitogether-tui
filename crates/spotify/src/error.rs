use std::time::Duration;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("requête HTTP échouée : {0}")]
    Http(#[from] reqwest::Error),

    #[error("réponse JSON invalide : {0}")]
    Json(#[from] serde_json::Error),

    #[error("erreur réseau locale : {0}")]
    Io(#[from] std::io::Error),

    #[error("authentification Spotify : {0}")]
    Auth(String),

    #[error("un compte Spotify Premium est nécessaire pour contrôler la lecture")]
    PremiumRequired,

    #[error("aucun appareil Spotify actif : ouvre Spotify (ou spotifyd) puis réessaie, ou choisis un appareil avec --device")]
    NoActiveDevice,

    #[error("quota de l'API Spotify dépassé, réessaie dans {0:?}")]
    RateLimited(Duration),

    #[error("erreur API Spotify {status} : {message}")]
    Api {
        status: u16,
        message: String,
        reason: Option<String>,
    },
}

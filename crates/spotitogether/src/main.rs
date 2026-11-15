mod config;
mod home;
mod room_cmd;
mod secrets;
mod spotify_cmd;

use std::io::{self, BufRead, Write};
use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::{Config, is_valid_client_id};

#[derive(Parser)]
#[command(
    name = "spotitogether",
    version,
    about = "Écoute Spotify synchronisée entre amis, dans le terminal",
    long_about = "Écoute Spotify synchronisée entre amis, dans le terminal.\n\n\
                  Sans argument : écran d'accueil pour créer un salon ou en rejoindre un.\n\
                  Avec un lien d'invitation : rejoint ce salon directement.",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Lien d'invitation à rejoindre directement (spotitogether://hôte:port/CODE)
    invite: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Configure ton Client ID Spotify (assistant pas à pas)
    Setup {
        /// Client ID à enregistrer directement, sans questions
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Connecte ton compte Spotify (ouvre le navigateur)
    Login,
    /// Supprime le jeton Spotify enregistré
    Logout,
    /// Héberge un salon sur cette machine
    Serve {
        /// Port d'écoute
        #[arg(long, short, default_value_t = protocol::DEFAULT_PORT)]
        port: u16,
        /// Adresse d'écoute (0.0.0.0 = tout le réseau local)
        #[arg(long, default_value = "0.0.0.0")]
        bind: IpAddr,
        /// Code d'invitation choisi (sinon généré au hasard)
        #[arg(long)]
        code: Option<String>,
        /// Base SQLite des rôles, bannis et réglages
        /// (défaut : ~/.local/share/spotitogether/room-<port>.sqlite)
        #[arg(long)]
        data: Option<std::path::PathBuf>,
        /// Ne rien enregistrer sur disque
        #[arg(long, conflicts_with = "data")]
        ephemeral: bool,
        /// Adresse publique à mettre dans l'invitation (ex. salon.exemple.fr ou
        /// mon-pc.tailnet.ts.net:7420), si elle diffère de l'adresse locale
        #[arg(long)]
        public: Option<String>,
        /// Le salon est derrière un reverse proxy HTTPS (invitation en wss://)
        #[arg(long, requires = "public")]
        tls: bool,
        /// Désactiver l'accès pair-à-pair (lien qui marche hors du réseau local)
        #[arg(long)]
        no_p2p: bool,
    },
    /// Rejoint un salon : spotitogether join spotitogether://hôte:port/CODE
    Join {
        /// Lien d'invitation (spotitogether://hôte:port/CODE, hôte:port/CODE ou ws://…)
        invite: String,
        /// Code du salon, s'il n'est pas dans le lien
        #[arg(long)]
        code: Option<String>,
        /// Pseudo affiché (par défaut ton nom Spotify)
        #[arg(long)]
        name: Option<String>,
        /// Jeton d'hôte affiché par `serve` (automatique sur la même machine)
        #[arg(long, conflicts_with = "guest")]
        host_token: Option<String>,
        /// Rejoindre en invité, même sur la machine de l'hôte (pour tester les droits)
        #[arg(long)]
        guest: bool,
        /// Rejoindre sans rien jouer sur ton compte Spotify
        #[arg(long)]
        spectator: bool,
        /// Interface en lignes de commande au lieu de la TUI plein écran
        #[arg(long)]
        plain: bool,
    },
    /// Commandes Spotify directes, pour tester ton installation
    #[command(subcommand)]
    Spotify(spotify_cmd::SpotifyCmd),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.command.as_ref());

    if let Err(err) = run(cli).await {
        eprintln!("Erreur : {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        return home::run(cli.invite).await;
    };
    match command {
        Command::Setup { client_id } => setup(client_id),
        Command::Login => login().await,
        Command::Logout => {
            secrets::delete()?;
            println!("Jeton Spotify supprimé.");
            Ok(())
        }
        Command::Serve { port, bind, code, data, ephemeral, public, tls, no_p2p } => {
            room_cmd::serve(room_cmd::ServeArgs { port, bind, code, data, ephemeral, public, tls, no_p2p }).await
        }
        Command::Join { invite, code, name, host_token, guest, spectator, plain } => {
            room_cmd::join(room_cmd::JoinArgs { invite, code, name, host_token, guest, spectator, plain }).await
        }
        Command::Spotify(cmd) => spotify_cmd::run(cmd).await,
    }
}

/// The server logs joins/leaves to stderr; the TUI owns the screen, so it
/// logs to `~/.cache/spotitogether/spotitogether.log` instead.
fn init_logging(command: Option<&Command>) {
    let builder = tracing_subscriber::fmt().with_target(false);
    match command {
        Some(Command::Serve { .. }) => builder.with_max_level(tracing::Level::INFO).with_writer(io::stderr).init(),
        None | Some(Command::Join { plain: false, .. }) => {
            let file = config::project_dirs().ok().and_then(|d| {
                std::fs::create_dir_all(d.cache_dir()).ok()?;
                std::fs::OpenOptions::new().create(true).append(true).open(d.cache_dir().join("spotitogether.log")).ok()
            });
            match file {
                Some(f) => builder
                    .with_max_level(tracing::Level::WARN)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(f))
                    .init(),
                None => builder.with_max_level(tracing::Level::ERROR).with_writer(io::sink).init(),
            }
        }
        _ => builder.with_max_level(tracing::Level::WARN).with_writer(io::stderr).init(),
    }
}

const DASHBOARD_URL: &str = "https://developer.spotify.com/dashboard";

fn setup(client_id: Option<String>) -> Result<()> {
    let mut config = Config::load()?;

    let id = match client_id {
        Some(id) => id.trim().to_owned(),
        None => {
            println!("Configuration de Spotitogether\n");
            println!("Spotify limite chaque app de développement à 5 utilisateurs : chaque participant");
            println!("crée donc sa propre app (gratuit, 2 minutes). Le compte propriétaire doit être Premium.\n");
            println!("1. Ouvre {DASHBOARD_URL} et connecte-toi.");
            println!("2. Clique sur « Create app » et remplis :");
            println!("     App name      : Spotitogether (ou ce que tu veux)");
            println!("     Description   : ce que tu veux");
            println!("     Redirect URI  : {}   (exactement, puis « Add »)", spotify::auth::REDIRECT_URI);
            println!("     API utilisée  : Web API");
            println!("3. Ouvre les « Settings » de l'app et copie le Client ID.\n");
            let _ = open::that_detached(DASHBOARD_URL);

            let hint = match &config.client_id {
                Some(current) => format!(" (Entrée pour garder {current})"),
                None => String::new(),
            };
            loop {
                let input = prompt(&format!("Client ID{hint} : "))?;
                let candidate = match (input.is_empty(), &config.client_id) {
                    (true, Some(current)) => current.clone(),
                    _ => input,
                };
                if is_valid_client_id(&candidate) {
                    break candidate;
                }
                println!("Ce n'est pas un Client ID valide (32 caractères hexadécimaux). Réessaie.");
            }
        }
    };

    if !is_valid_client_id(&id) {
        bail!("Client ID invalide : 32 caractères hexadécimaux attendus");
    }
    config.client_id = Some(id.to_lowercase());
    let path = config.save()?;
    println!("\nClient ID enregistré dans {}.", path.display());
    println!("Étape suivante : spotitogether login");
    Ok(())
}

async fn login() -> Result<()> {
    let config = Config::load()?;
    let client_id = config.require_client_id()?;
    let http = spotify::SpotifyClient::http_client();

    println!("Connexion à Spotify : autorise l'app dans ton navigateur.");
    let token = spotify::auth::login(&http, client_id, |url| {
        if open::that_detached(url).is_err() {
            println!("Impossible d'ouvrir le navigateur ; ouvre ce lien :");
        } else {
            println!("Si rien ne s'ouvre, ouvre ce lien :");
        }
        println!("{url}\n");
        println!("En attente de la réponse de Spotify (5 min max)…");
    })
    .await?;

    let backend = secrets::save(&token)?;
    let client = spotify::SpotifyClient::new(client_id, token);
    let me = client.me().await.context("jeton obtenu mais /me a échoué")?;
    println!(
        "Connecté en tant que {} ({}). Jeton enregistré dans le {backend}.",
        me.display_name.as_deref().unwrap_or(&me.id),
        me.id
    );
    println!("Teste avec : spotitogether spotify devices");
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("entrée fermée");
    }
    Ok(line.trim().to_owned())
}

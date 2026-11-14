mod config;
mod secrets;
mod spotify_cmd;

use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::{Config, is_valid_client_id};

#[derive(Parser)]
#[command(name = "spotitogether", version, about = "Écoute Spotify synchronisée entre amis, dans le terminal")]
struct Cli {
    #[command(subcommand)]
    command: Command,
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
    /// Commandes Spotify directes, pour tester ton installation
    #[command(subcommand)]
    Spotify(spotify_cmd::SpotifyCmd),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_writer(io::stderr)
        .with_target(false)
        .init();

    if let Err(err) = run(Cli::parse()).await {
        eprintln!("Erreur : {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Setup { client_id } => setup(client_id),
        Command::Login => login().await,
        Command::Logout => {
            secrets::delete()?;
            println!("Jeton Spotify supprimé.");
            Ok(())
        }
        Command::Spotify(cmd) => spotify_cmd::run(cmd).await,
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

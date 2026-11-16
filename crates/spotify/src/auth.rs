//! OAuth 2.0 Authorization Code flow with PKCE (no client secret).
//!
//! Each user registers their own Spotify app (Development Mode is capped at
//! 5 users per Client ID), so the Client ID comes from the user's config.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::{Error, Result};

/// Port of the local callback server. Must match the Redirect URI
/// registered in the Spotify dashboard (loopback IP, not `localhost`).
pub const CALLBACK_PORT: u16 = 8973;
pub const REDIRECT_URI: &str = "http://127.0.0.1:8973/callback";
pub const SCOPES: &str = "user-read-playback-state user-modify-playback-state user-read-currently-playing \
                          playlist-read-private playlist-read-collaborative user-library-read";

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub(crate) const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix timestamp (seconds) after which `access_token` is invalid.
    pub expires_at: u64,
}

impl Token {
    /// True if the access token expires within the next minute.
    pub fn needs_refresh(&self) -> bool {
        now_secs() + 60 >= self.expires_at
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

/// PKCE verifier/challenge pair (RFC 7636, S256).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        // 64 random bytes -> 86 chars, within the 43..=128 allowed range.
        let verifier = random_urlsafe(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

pub fn authorize_url(client_id: &str, challenge: &str, state: &str) -> String {
    let mut url = Url::parse(AUTHORIZE_URL).expect("static URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge)
        .append_pair("state", state)
        .append_pair("scope", SCOPES);
    url.into()
}

/// Runs the whole interactive login: starts the callback server, hands the
/// authorization URL to `open_browser`, waits for Spotify's redirect and
/// exchanges the code for tokens.
pub async fn login(
    http: &reqwest::Client,
    client_id: &str,
    open_browser: impl FnOnce(&str),
) -> Result<Token> {
    let pkce = Pkce::generate();
    let state = random_urlsafe(16);
    // Bind before opening the browser so the redirect cannot arrive first.
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .map_err(|e| {
            Error::Auth(format!(
                "impossible d'écouter sur le port {CALLBACK_PORT} ({e}) ; un autre login est-il en cours ?"
            ))
        })?;

    open_browser(&authorize_url(client_id, &pkce.challenge, &state));

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| Error::Auth("délai de connexion dépassé (5 min)".into()))??;

    exchange_code(http, client_id, &code, &pkce.verifier).await
}

/// Accepts connections on the callback server until one carries the
/// authorization code (or an error) for our `state`.
async fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await?;
        let request = String::from_utf8_lossy(&buf[..n]);

        // "GET /callback?code=...&state=... HTTP/1.1"
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != "/callback" {
            respond(&mut stream, 404, "Not found").await;
            continue;
        }

        let param = |name: &str| {
            url.query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };

        if param("state").as_deref() != Some(expected_state) {
            respond(&mut stream, 400, "État OAuth invalide, relance la connexion.").await;
            continue;
        }
        if let Some(err) = param("error") {
            respond(&mut stream, 400, "Connexion refusée. Tu peux fermer cet onglet.").await;
            return Err(Error::Auth(format!("Spotify a refusé l'autorisation : {err}")));
        }
        if let Some(code) = param("code") {
            respond(
                &mut stream,
                200,
                "Connexion réussie ! Tu peux fermer cet onglet et revenir au terminal.",
            )
            .await;
            return Ok(code);
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Spotitogether</title>\
         <body style=\"font-family:sans-serif;padding:3em\"><h2>Spotitogether</h2><p>{message}</p>"
    );
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn exchange_code(
    http: &reqwest::Client,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> Result<Token> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;
    let token = parse_token_response(resp, None).await?;
    Ok(token)
}

/// Refreshes an access token. Spotify may rotate the refresh token; when it
/// does not send a new one the previous one stays valid.
pub async fn refresh(http: &reqwest::Client, client_id: &str, refresh_token: &str) -> Result<Token> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await?;
    parse_token_response(resp, Some(refresh_token)).await
}

async fn parse_token_response(resp: reqwest::Response, previous_refresh: Option<&str>) -> Result<Token> {
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        let detail = serde_json::from_str::<TokenErrorResponse>(&body)
            .map(|e| match e.error_description {
                Some(d) => format!("{} ({d})", e.error),
                None => e.error,
            })
            .unwrap_or(body);
        return Err(Error::Auth(format!("échange du jeton refusé ({status}) : {detail}")));
    }
    let parsed: TokenResponse = serde_json::from_str(&body)?;
    let refresh_token = parsed
        .refresh_token
        .or_else(|| previous_refresh.map(str::to_owned))
        .ok_or_else(|| Error::Auth("Spotify n'a pas renvoyé de refresh token".into()))?;
    Ok(Token {
        access_token: parsed.access_token,
        refresh_token,
        expires_at: now_secs() + parsed.expires_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // Appendix B of RFC 7636.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn verifier_length_is_valid() {
        let p = Pkce::generate();
        assert!((43..=128).contains(&p.verifier.len()));
    }

    async fn hit(port: u16, target: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn callback_server_ignores_bad_state_then_returns_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { wait_for_code(&listener, "good").await });

        assert!(hit(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(hit(port, "/callback?code=x&state=evil").await.starts_with("HTTP/1.1 400"));
        assert!(hit(port, "/callback?code=abc&state=good").await.starts_with("HTTP/1.1 200"));
        assert_eq!(server.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn callback_server_reports_denied_access() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move { wait_for_code(&listener, "s").await });
        hit(port, "/callback?error=access_denied&state=s").await;
        assert!(matches!(server.await.unwrap(), Err(Error::Auth(_))));
    }

    #[test]
    fn authorize_url_contains_required_params() {
        let url = authorize_url("abc", "chal", "st");
        for needle in ["client_id=abc", "code_challenge=chal", "state=st", "code_challenge_method=S256", "redirect_uri=http%3A%2F%2F127.0.0.1%3A8973%2Fcallback"] {
            assert!(url.contains(needle), "{needle} missing from {url}");
        }
    }
}

//! Full-screen terminal interface (ratatui + crossterm), driven by keyboard
//! and mouse, themed with Caelestia's colours when available.

mod app;
mod cover;
pub mod launcher;
mod mpris;
mod theme;
mod ui;

use std::io::stdout;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, EventStream};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use spotify::SpotifyClient;
use tokio::sync::mpsc;

use app::{App, Effect, SearchOutcome};
pub use theme::Theme;

const FRAME_EVERY: Duration = Duration::from_millis(250);

/// Results of background Spotify requests.
enum Loaded {
    Playlists(Result<Vec<spotify::models::PlaylistSummary>, String>),
    PlaylistTracks { name: String, result: Result<Vec<spotify::models::Track>, String> },
}

/// How the room screen was opened.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Invite link to show and copy (`i`).
    pub invite: Option<String>,
    /// This process hosts the room: quitting closes it.
    pub hosting: bool,
}

/// Runs the TUI until the user quits. Returns the disconnection reason when
/// the room connection was lost.
pub async fn run(joined: client::Joined, spotify: Option<Arc<SpotifyClient>>, options: RunOptions) -> Result<Option<String>> {
    let mut terminal = ratatui::init();
    let image_picker = cover::image_picker();
    let _ = execute!(stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, joined, spotify, image_picker, options).await;
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut joined: client::Joined,
    spotify: Option<Arc<SpotifyClient>>,
    image_picker: Option<ratatui_image::picker::Picker>,
    options: RunOptions,
) -> Result<Option<String>> {
    let mut app = App::new(&joined, spotify.is_some(), Theme::load());
    app.invite = options.invite;
    app.hosting = options.hosting;
    if let Some(p) = &image_picker {
        tracing::info!(protocol = ?p.protocol_type(), "terminal graphics");
    }
    app.image_picker = image_picker;
    let room = joined.handle.clone();
    let http = SpotifyClient::http_client();

    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<SearchOutcome>();
    let (cover_tx, mut cover_rx) = mpsc::unbounded_channel();
    let (theme_tx, mut theme_rx) = mpsc::unbounded_channel::<()>();
    let (loaded_tx, mut loaded_rx) = mpsc::unbounded_channel::<Loaded>();
    let (mpris_cmd_tx, mut mpris_cmd_rx) = mpsc::unbounded_channel();
    let mpris = mpris::spawn(mpris_cmd_tx);
    let mut mpris_shown = mpris::Shown::default();
    let mut frames = 0u64;
    // Kept alive for the whole session.
    let _theme_watcher = watch_theme(theme_tx);

    let mut terminal_events = EventStream::new();
    let mut frame = tokio::time::interval(FRAME_EVERY);
    let mut effects = app.initial_effects();

    loop {
        for effect in effects.drain(..) {
            match effect {
                Effect::Quit => return Ok(app.exit_message.take()),
                Effect::Send(msg) => {
                    room.send(msg);
                }
                Effect::Resync => {
                    room.resync();
                }
                Effect::Search { request, query, offset } => {
                    if let Some(sp) = spotify.clone() {
                        let tx = search_tx.clone();
                        tokio::spawn(async move {
                            let result = sp
                                .search_tracks(&query, app::SEARCH_PAGE, offset)
                                .await
                                .map(|page| (page.items, page.total))
                                .map_err(|e| e.to_string());
                            let _ = tx.send(SearchOutcome { request, offset, result });
                        });
                    }
                }
                Effect::CopyInvite(link) => {
                    let how = copy_to_clipboard(&link);
                    app.toast(format!("Lien copié ({how}) : {link}"), app::Level::Good);
                }
                Effect::LoadPlaylists => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            let result = sp.my_playlists().await.map_err(|e| e.to_string());
                            let _ = tx.send(Loaded::Playlists(result));
                        });
                    }
                }
                Effect::LoadPlaylistTracks { id, name } => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            let result = sp
                                .playlist_tracks(&id, protocol::MAX_AUTOPLAY_TRACKS)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = tx.send(Loaded::PlaylistTracks { name, result });
                        });
                    }
                }
                Effect::FetchCover(url) => {
                    let (http, tx) = (http.clone(), cover_tx.clone());
                    tokio::spawn(async move {
                        let img = cover::fetch(&http, &url).await.map_err(|e| e.to_string());
                        let _ = tx.send((url, img));
                    });
                }
            }
        }

        terminal.draw(|f| ui::draw(f, &mut app))?;
        if let Some(tx) = &mpris {
            for update in mpris::diff(&mut mpris_shown, &app) {
                let _ = tx.send(update);
            }
        }

        effects = tokio::select! {
            Some(Ok(event)) = terminal_events.next() => app.on_terminal_event(event),
            event = joined.events.recv() => match event {
                Some(event) => app.on_room_event(event),
                None => {
                    app.exit_message = Some("connexion perdue".into());
                    vec![Effect::Quit]
                }
            },
            Some(outcome) = search_rx.recv() => { app.on_search(outcome); vec![] }
            Some((url, img)) = cover_rx.recv() => { app.on_cover(url, img); vec![] }
            Some(()) = theme_rx.recv() => { app.theme = Theme::load(); vec![] }
            Some(loaded) = loaded_rx.recv() => match loaded {
                Loaded::Playlists(result) => { app.on_playlists(result); vec![] }
                Loaded::PlaylistTracks { name, result } => app.on_playlist_tracks(name, result),
            },
            Some(cmd) = mpris_cmd_rx.recv() => app.on_mpris(cmd),
            _ = frame.tick() => {
                frames += 1;
                // Position refresh for MPRIS clients, once per second.
                if let Some(tx) = mpris.as_ref().filter(|_| frames.is_multiple_of(4) && app.playback.current.is_some()) {
                    let _ = tx.send(mpris::Update::Position(app.position_ms()));
                }
                vec![]
            }
        };
    }
}

/// Copies `text`: `wl-copy` on Wayland (Hyprland), `xclip` on X11, else the
/// terminal's OSC 52 sequence (supported by foot, kitty, WezTerm…).
fn copy_to_clipboard(text: &str) -> &'static str {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let tools: [(&str, &[&str]); 2] = [("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for (tool, args) in tools {
        let child = Command::new(tool).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = child {
            let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
            if written && child.wait().is_ok_and(|s| s.success()) {
                return tool;
            }
        }
    }
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
    "terminal"
}

/// Reloads colours when Caelestia rewrites its scheme (wallpaper change).
fn watch_theme(tx: mpsc::UnboundedSender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let path = theme::caelestia_scheme_path()?;
    let dir = path.parent()?.to_owned();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && event.paths.iter().any(|p| p.ends_with("scheme.json"))
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

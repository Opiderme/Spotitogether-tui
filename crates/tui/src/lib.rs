//! Full-screen terminal interface (ratatui + crossterm), driven by keyboard
//! and mouse, themed with Caelestia's colours when available.

mod app;
mod audio;
mod cover;
mod library;
mod lyrics;
pub mod launcher;
mod mpris;
mod theme;
mod ui;
mod visualizer;

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
    Library(Result<Vec<spotify::models::PlaylistSummary>, String>),
    LibraryList { key: String, result: Result<Vec<spotify::models::Track>, String> },
    Volume(Option<u8>),
    Liked(Vec<(String, bool)>),
    LikeDone { uri: String, name: String, liked: bool, result: Result<(), String> },
    Failed(String),
    Lyrics { key: String, result: Result<lyrics::Lyrics, String> },
}

/// How the room screen was opened.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Invite link to show and copy (`i`).
    pub invite: Option<String>,
    /// This process hosts the room: quitting closes it.
    pub hosting: bool,
    /// Shown once at start (e.g. "this link only works on your LAN").
    pub notice: Option<String>,
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
    if let Some(notice) = options.notice {
        app.toast(notice, app::Level::Warn);
    }
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
    // Visualizer: 30 frames per second and the system sound, only while open.
    let vis_started = std::time::Instant::now();
    let mut vis_tick = tokio::time::interval(Duration::from_millis(33));
    vis_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut audio: Option<audio::AudioCapture> = None;
    let mut audio_failed = false;
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
                Effect::FetchLyrics(query) => {
                    let (http, tx) = (http.clone(), loaded_tx.clone());
                    tokio::spawn(async move {
                        let duration = query.duration_ms;
                        let result = lyrics::fetch(&http, &query)
                            .await
                            .map(|record| lyrics::Lyrics::from_record(record, duration))
                            .map_err(|e| e.to_string());
                        let _ = tx.send(Loaded::Lyrics { key: query.key, result });
                    });
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
                Effect::SetVolume(percent) => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = sp.set_volume(None, percent).await {
                                let _ = tx.send(Loaded::Failed(format!("Volume : {e}")));
                            }
                        });
                    }
                }
                Effect::LoadVolume => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(v) = sp.volume().await {
                                let _ = tx.send(Loaded::Volume(v));
                            }
                        });
                    }
                }
                Effect::CheckLiked(uris) => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            // Missing scope (old login) or other error: just no ♥.
                            if let Ok(flags) = sp.library_contains(&uris).await {
                                let _ = tx.send(Loaded::Liked(uris.into_iter().zip(flags).collect()));
                            }
                        });
                    }
                }
                Effect::SetLiked { uri, name, liked } => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            let result = sp.set_liked(std::slice::from_ref(&uri), liked).await.map_err(|e| e.to_string());
                            let _ = tx.send(Loaded::LikeDone { uri, name, liked, result });
                        });
                    }
                }
                Effect::LoadLibrary => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            let result = sp.my_playlists().await.map_err(|e| e.to_string());
                            let _ = tx.send(Loaded::Library(result));
                        });
                    }
                }
                Effect::LoadLibraryList { source, name: _ } => {
                    if let Some(sp) = spotify.clone() {
                        let tx = loaded_tx.clone();
                        tokio::spawn(async move {
                            let max = library::MAX_TRACKS;
                            let result = match &source {
                                library::Source::Liked => sp.saved_tracks(max).await,
                                library::Source::Playlist { id } => sp.playlist_tracks(id, max).await,
                            };
                            let key = source.key().to_owned();
                            let _ = tx.send(Loaded::LibraryList { key, result: result.map_err(|e| e.to_string()) });
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

        // Closing the visualizer stops the sound capture.
        if app.visualizer.is_none() && (audio.is_some() || audio_failed) {
            audio = None;
            audio_failed = false;
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
            Some(outcome) = search_rx.recv() => app.on_search(outcome),
            Some((url, img)) = cover_rx.recv() => { app.on_cover(url, img); vec![] }
            Some(()) = theme_rx.recv() => {
                let theme = Theme::load();
                if theme != app.theme {
                    app.theme = theme;
                }
                vec![]
            }
            Some(loaded) = loaded_rx.recv() => match loaded {
                Loaded::Playlists(result) => { app.on_playlists(result); vec![] }
                Loaded::PlaylistTracks { name, result } => app.on_playlist_tracks(name, result),
                Loaded::Library(result) => { app.on_library(result); vec![] }
                Loaded::Volume(v) => { app.on_volume(v); vec![] }
                Loaded::Liked(states) => { app.on_liked(states); vec![] }
                Loaded::LikeDone { uri, name, liked, result } => { app.on_like_done(uri, name, liked, result); vec![] }
                Loaded::Failed(text) => { app.toast(text, app::Level::Bad); vec![] }
                Loaded::Lyrics { key, result } => { app.on_lyrics(key, result); vec![] }
                Loaded::LibraryList { key, result } => { app.on_library_list(key, result); vec![] }
            },
            Some(cmd) = mpris_cmd_rx.recv() => app.on_mpris(cmd),
            // 30 fps for the visualizer, and smooth typing on the lyrics screen.
            _ = vis_tick.tick(), if app.visualizer.is_some() || app.lyrics.is_some() => {
                if app.visualizer.is_some() {
                    if audio.is_none() && !audio_failed {
                        match audio::AudioCapture::start() {
                            Ok(capture) => audio = Some(capture),
                            Err(e) => {
                                tracing::info!(%e, "system sound unavailable, simulated visualizer");
                                audio_failed = true;
                            }
                        }
                    }
                    let live = audio.as_ref().and_then(|a| a.latest());
                    let simulated = live.is_none();
                    let analysis = live.unwrap_or_else(|| {
                        audio::Analysis::simulated(frames as f32 * 0.25 + vis_started.elapsed().as_secs_f32(), app.playback.is_playing)
                    });
                    if let Some(v) = app.visualizer.as_mut() {
                        v.simulated = simulated;
                        v.update(&analysis);
                    }
                }
                vec![]
            }
            _ = sleep_until_opt(app.search_due()) => app.on_search_due(),
            _ = sleep_until_opt(app.volume_due()) => app.on_volume_due(),
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

async fn sleep_until_opt(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d.into()).await,
        None => std::future::pending().await,
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

/// A real change of Caelestia's scheme. Reading the file (which we do on
/// every reload) produces "access" events: reacting to those made the
/// reload trigger itself in a loop and burn a CPU core.
fn is_scheme_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;
    let written = matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
    );
    written && event.paths.iter().any(|p| p.ends_with("scheme.json"))
}

/// Reloads colours when Caelestia rewrites its scheme (wallpaper change).
fn watch_theme(tx: mpsc::UnboundedSender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let path = theme::caelestia_scheme_path()?;
    let dir = path.parent()?.to_owned();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && is_scheme_change(&event)
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::is_scheme_change;
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, Event, EventKind, MetadataKind, ModifyKind};

    fn event(kind: EventKind, file: &str) -> Event {
        Event::new(kind).add_path(format!("/home/u/.local/state/caelestia/{file}").into())
    }

    #[test]
    fn only_writes_to_the_scheme_reload_the_theme() {
        assert!(is_scheme_change(&event(EventKind::Modify(ModifyKind::Data(DataChange::Content)), "scheme.json")));
        assert!(is_scheme_change(&event(EventKind::Create(CreateKind::File), "scheme.json")));
        // Our own reads must not trigger a reload (that was a busy loop).
        assert!(!is_scheme_change(&event(EventKind::Access(AccessKind::Open(AccessMode::Read)), "scheme.json")));
        assert!(!is_scheme_change(&event(EventKind::Access(AccessKind::Close(AccessMode::Read)), "scheme.json")));
        assert!(!is_scheme_change(&event(EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)), "scheme.json")));
        assert!(!is_scheme_change(&event(EventKind::Modify(ModifyKind::Data(DataChange::Content)), "notifs.json")));
    }
}

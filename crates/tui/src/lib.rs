//! Full-screen terminal interface (ratatui + crossterm), driven by keyboard
//! and mouse, themed with Caelestia's colours when available.

mod app;
mod cover;
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

/// Runs the TUI until the user quits. Returns the disconnection reason when
/// the room connection was lost.
pub async fn run(joined: client::Joined, spotify: Option<Arc<SpotifyClient>>) -> Result<Option<String>> {
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, joined, spotify).await;
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    mut joined: client::Joined,
    spotify: Option<Arc<SpotifyClient>>,
) -> Result<Option<String>> {
    let mut app = App::new(&joined, spotify.is_some(), Theme::load());
    let room = joined.handle.clone();
    let http = SpotifyClient::http_client();

    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<SearchOutcome>();
    let (cover_tx, mut cover_rx) = mpsc::unbounded_channel();
    let (theme_tx, mut theme_rx) = mpsc::unbounded_channel::<()>();
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
            _ = frame.tick() => vec![],
        };
    }
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

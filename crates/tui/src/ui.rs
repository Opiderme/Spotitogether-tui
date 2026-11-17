//! Drawing. Records clickable regions in `app.areas` as it goes.

use protocol::{Member, Perm, Role};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, HighlightSpacing, List, ListItem, Paragraph, Row, Table};

use crate::app::{App, CoverCache, Focus, Level, PERM_COLUMNS, SEARCH_PAGE};
use crate::cover;

const PREV: &str = "[ |◀ ]";
const NEXT: &str = "[ ▶| ]";
const PAUSE: &str = "[ ▮▮ ]";
const PLAY: &str = "[ ▶  ]";
const LIKED: &str = "[ ♥ ]";
const NOT_LIKED: &str = "[ ♡ ]";

pub fn draw(f: &mut Frame, app: &mut App) {
    if app.visualizer.is_some() {
        return draw_visualizer(f, app);
    }
    if app.lyrics.is_some() {
        return draw_lyrics(f, app);
    }
    let area = f.area();
    let now_height = if area.height >= 34 { 14 } else { 10 };
    let [now, middle, search, status] = Layout::vertical([
        Constraint::Length(now_height),
        Constraint::Min(6),
        Constraint::Length(9),
        Constraint::Length(1),
    ])
    .areas(area);
    let [queue, members] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(middle);

    draw_now_playing(f, app, now);
    draw_queue(f, app, queue);
    draw_members(f, app, members);
    draw_search(f, app, search);
    draw_status(f, app, status);
    if app.perm_panel.is_some() {
        draw_permissions(f, app, area);
    }
    if app.playlist_picker.is_some() {
        draw_picker(f, app, area);
    }
    if app.library.is_some() {
        draw_library(f, app, area);
    }
    if app.show_help {
        draw_help(f, app, area);
    }
}

fn level_color(app: &App, level: Level) -> ratatui::style::Color {
    match level {
        Level::Info => app.theme.muted,
        Level::Good => app.theme.good,
        Level::Warn => app.theme.warn,
        Level::Bad => app.theme.bad,
    }
}

fn panel<'a>(app: &App, title: String, focused: bool) -> Block<'a> {
    let t = app.theme;
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { t.accent } else { t.border }))
        .title(Line::from(format!(" {title} ")).fg(t.accent).bold())
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn draw_now_playing(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let (sync_text, sync_level) = app.sync.clone();
    let block = panel(app, "En cours".into(), false)
        .title(Line::from(vec![Span::raw(" ● "), Span::raw(sync_text), Span::raw(" ")]).fg(level_color(app, sync_level)).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cover_w = (inner.height * 2).min(inner.width / 2);
    let [cover_area, _, info] =
        Layout::horizontal([Constraint::Length(cover_w), Constraint::Length(3), Constraint::Min(10)]).areas(inner);
    draw_cover(f, app, cover_area);

    let Some(item) = app.playback.current.clone() else {
        app.areas.progress = Rect::default();
        app.areas.prev_button = Rect::default();
        app.areas.play_button = Rect::default();
        app.areas.next_button = Rect::default();
        app.areas.like_button = Rect::default();
        app.areas.volume_bar = Rect::default();
        let lines = vec![
            Line::default(),
            Line::from("Rien en lecture").fg(t.text).bold(),
            Line::from("Appuie sur / pour chercher un morceau, puis Entrée pour l'ajouter.").fg(t.muted),
        ];
        f.render_widget(Paragraph::new(lines), info);
        return;
    };

    let rows = Layout::vertical([Constraint::Length(1); 11]).split(info);
    let line = |i: usize| rows.get(i).copied().unwrap_or_default();
    f.render_widget(Paragraph::new(Line::from(item.track.name.clone()).fg(t.text).bold()), line(1));
    f.render_widget(Paragraph::new(Line::from(item.track.artists.clone()).fg(t.muted)), line(2));
    f.render_widget(Paragraph::new(Line::from(format!("Ajouté par {}", item.added_by)).fg(t.muted)), line(4));

    // Progress bar: "2:14 ━━━━━━●──────── 3:40"
    let progress_row = Rect { width: line(6).width.min(72), ..line(6) };
    let pos = app.position_ms();
    let left = format!("{} ", fmt_ms(pos));
    let right = format!(" {}", fmt_ms(item.track.duration_ms));
    let bar_w = progress_row.width.saturating_sub((left.chars().count() + right.chars().count()) as u16);
    let frac = if item.track.duration_ms == 0 { 0.0 } else { pos as f64 / item.track.duration_ms as f64 };
    let done = ((frac.clamp(0.0, 1.0) * f64::from(bar_w.saturating_sub(1))).round()) as usize;
    let rest = (bar_w as usize).saturating_sub(done + 1);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(left.clone()).fg(t.text),
            Span::raw("━".repeat(done)).fg(t.accent),
            Span::raw("●").fg(t.accent),
            Span::raw("─".repeat(rest)).fg(t.border),
            Span::raw(right).fg(t.text),
        ])),
        progress_row,
    );
    app.areas.progress = Rect { x: progress_row.x + left.chars().count() as u16, width: bar_w, ..progress_row };

    // Buttons
    let buttons = line(8);
    let play = if app.playback.is_playing { PAUSE } else { PLAY };
    let like = match app.current_liked() {
        Some(true) => Span::raw(LIKED).fg(t.bad).bold(),
        _ => Span::raw(NOT_LIKED).fg(t.text),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(PREV).fg(t.text),
            Span::raw("  "),
            Span::raw(play).fg(t.accent).bold(),
            Span::raw("  "),
            Span::raw(NEXT).fg(t.text),
            Span::raw("    "),
            like,
            Span::raw(match app.votes {
                (0, _) if app.my_perms().skip => String::new(),
                (votes, needed) => format!("      vote skip {votes}/{needed}"),
            })
            .fg(t.muted),
        ])),
        buttons,
    );
    let button = |offset: u16| Rect { x: buttons.x + offset, width: 6.min(buttons.width.saturating_sub(offset)), ..buttons };
    app.areas.prev_button = button(0);
    app.areas.play_button = button(8);
    app.areas.next_button = button(16);
    app.areas.like_button = Rect { width: 7, ..button(26) };

    // This member's volume: "Volume ━━━━━●────── 27 %  (+/- · m muet)"
    let volume_row = Rect { width: line(9).width.min(72), ..line(9) };
    if app.can_play {
        let label = "Volume ";
        let bar_w = volume_row.width.saturating_sub(label.len() as u16 + 28).min(30);
        let (text, level) = match (app.muted, app.volume) {
            (Some(_), _) => ("muet pour ce titre (m pour rétablir)".to_owned(), 0),
            (None, Some(v)) => (format!("{v} %  (+/- · m muet)"), v),
            (None, None) => ("— (+/- · m muet)".to_owned(), 0),
        };
        let done = (f64::from(level) / 100.0 * f64::from(bar_w.saturating_sub(1))).round() as usize;
        let rest = (bar_w as usize).saturating_sub(done + 1);
        let color = if app.muted.is_some() { t.warn } else { t.accent };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(label).fg(t.muted),
                Span::raw("━".repeat(done)).fg(color),
                Span::raw("●").fg(color),
                Span::raw("─".repeat(rest)).fg(t.border),
                Span::raw(format!(" {text}")).fg(if app.muted.is_some() { t.warn } else { t.muted }),
            ])),
            volume_row,
        );
        app.areas.volume_bar = Rect { x: volume_row.x + label.len() as u16, width: bar_w, ..volume_row };
    } else {
        app.areas.volume_bar = Rect::default();
    }

    let footer = if app.can_play && !app.my_perms().listen {
        "Ton rôle ne permet pas d'écouter : rien n'est joué sur ton compte".to_owned()
    } else if let Some(name) = &app.settings.autoplay {
        format!("Autoplay : {name}   ·   pochette : {} (c)", app.cover_mode.label())
    } else if app.can_play {
        format!("Pochette : {} (c pour changer)", app.cover_mode.label())
    } else {
        "Mode spectateur : rien n'est joué sur ton compte".into()
    };
    f.render_widget(Paragraph::new(Line::from(footer).fg(t.muted)), line(10));
}

/// Full screen lyrics: lines appear at their time all over the screen.
fn draw_lyrics(f: &mut Frame, app: &mut App) {
    use crate::lyrics::{Lyrics, Status};
    let t = app.theme;
    let area = f.area();
    let position = app.position_ms();
    let Some(screen) = app.lyrics.as_mut() else { return };
    // Colours of the cover, recomputed only when the cover changes.
    let cover_key = app.cover.as_ref().map(|(url, _)| url.clone());
    if screen.palette.is_empty() || screen.palette_key != cover_key {
        let from_cover = app.cover.as_ref().map(|(_, img)| {
            crate::visualizer::palette(&img.resize_exact(64, 64, image::imageops::FilterType::Triangle).to_rgba8())
        });
        screen.palette = from_cover.filter(|p| p.len() >= 2).unwrap_or_else(|| vec![t.accent, t.accent2, t.warn, t.good]);
        screen.palette_key = cover_key;
    }
    let stage = Rect { height: area.height.saturating_sub(2), ..area };
    screen.update(position, stage);
    screen.draw(f.buffer_mut(), position);

    // Centered message when there is nothing to show.
    let message = match &screen.status {
        _ if app.playback.current.is_none() => Some("Rien en lecture".to_owned()),
        Status::Loading => Some("Recherche des paroles…".to_owned()),
        Status::Failed(e) => Some(format!("Paroles indisponibles : {e}")),
        Status::Ready(Lyrics::NotFound) => Some("Pas de paroles trouvées pour ce titre".to_owned()),
        Status::Ready(Lyrics::Instrumental) => Some("♪  Instrumental  ♪".to_owned()),
        Status::Ready(Lyrics::Timed { lines, .. }) if lines.first().is_some_and(|l| position + 150 < l.at_ms) => {
            Some("♪".to_owned())
        }
        Status::Ready(Lyrics::Timed { .. }) => None,
    };
    if let Some(text) = message {
        let y = stage.y + stage.height / 2;
        f.render_widget(Paragraph::new(Line::from(text).fg(t.muted)).alignment(Alignment::Center), Rect { y, height: 1, ..stage });
    }

    // Bottom: progress and info.
    let bar = Rect { y: area.bottom().saturating_sub(2), height: 1, ..area };
    let info = Rect { y: area.bottom().saturating_sub(1), height: 1, ..area };
    if let Some(item) = &app.playback.current {
        let frac = if item.track.duration_ms == 0 { 0.0 } else { position as f64 / item.track.duration_ms as f64 };
        let done = (frac.clamp(0.0, 1.0) * f64::from(bar.width)) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("━".repeat(done)).fg(screen.palette.first().copied().unwrap_or(t.accent)),
                Span::raw("─".repeat(usize::from(bar.width).saturating_sub(done))).fg(t.border),
            ])),
            bar,
        );
        let note = match &screen.status {
            Status::Ready(Lyrics::Timed { approximate: true, .. }) => "  (paroles non synchronisées, approximatives)",
            _ => "",
        };
        let left = format!(
            " {} {} — {}   {} / {}{note}",
            if app.playback.is_playing { "▶" } else { "⏸" },
            item.track.name,
            item.track.artists,
            fmt_ms(position),
            fmt_ms(item.track.duration_ms)
        );
        f.render_widget(Paragraph::new(Line::from(left).fg(t.text)), info);
    }
    let keys = format!(
        " {}/8 {}   ←→ style   v visualiseur   Échap retour   paroles : LRCLIB ",
        screen.style + 1,
        screen.style_name()
    );
    f.render_widget(Paragraph::new(Line::from(keys).fg(t.muted)).alignment(Alignment::Right), info);
    if let Some((name, k)) = screen.banner() {
        let level = (255.0 * k) as u8;
        let banner = Line::from(format!("  ◆ {name} ◆  ")).fg(ratatui::style::Color::Rgb(level, level, level)).bold();
        f.render_widget(Paragraph::new(banner).alignment(Alignment::Center), Rect { y: area.y + 1, height: 1, ..area });
    }
    if let Some((text, level)) = app.active_toast() {
        let line = Line::from(format!(" {text} ")).fg(level_color(app, level)).bold();
        f.render_widget(Paragraph::new(line).alignment(Alignment::Center), Rect { y: area.y, height: 1, ..area });
    }
}

/// Full screen: cover, animation and a discreet info line.
fn draw_visualizer(f: &mut Frame, app: &mut App) {
    let Some(mut v) = app.visualizer.take() else { return };
    let t = app.theme;
    let fallback = [t.accent, t.accent2, t.warn, t.good, t.bad];
    let cover = app.cover.as_ref().map(|(url, img)| (url.as_str(), img));
    v.set_cover(cover.map(|c| c.0), cover.map(|c| c.1), &fallback);
    v.ink = t.text;
    let position_ms = app.position_ms();
    let track = app.playback.current.as_ref().map(|i| crate::visualizer::Track {
        title: &i.track.name,
        artists: &i.track.artists,
        position_ms,
        duration_ms: i.track.duration_ms,
        playing: app.playback.is_playing,
    });
    let dt = v.dt;
    v.draw(f, track.as_ref(), dt, app.cover_image.as_mut());
    if let Some((text, level)) = app.active_toast() {
        let area = f.area();
        let line = Line::from(format!(" {text} ")).fg(level_color(app, level)).bold();
        f.render_widget(Paragraph::new(line).alignment(Alignment::Center), Rect { y: area.y, height: 1, ..area });
    }
    app.visualizer = Some(v);
}

fn draw_cover(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Pictures are drawn over the cells by the terminal and could show
    // through popups: characters while one is open.
    let popup_open = app.show_help || app.perm_panel.is_some() || app.playlist_picker.is_some() || app.library.is_some();
    if app.cover_mode == crate::cover::CoverMode::Image
        && !popup_open
        && let Some(protocol) = app.cover_image.as_mut()
    {
        f.render_stateful_widget(ratatui_image::StatefulImage::default(), area, protocol);
        return;
    }
    let Some((url, img)) = &app.cover else {
        let placeholder = vec![Line::default(); (area.height / 2) as usize]
            .into_iter()
            .chain([Line::from("♪").fg(app.theme.border)])
            .collect::<Vec<_>>();
        f.render_widget(Paragraph::new(placeholder).alignment(Alignment::Center), area);
        return;
    };
    let size = (area.width, area.height);
    let fresh = app
        .cover_cache
        .as_ref()
        .is_some_and(|c| c.url == *url && c.size == size && c.mode == app.cover_mode);
    if !fresh {
        app.cover_cache = Some(CoverCache {
            url: url.clone(),
            size,
            mode: app.cover_mode,
            cells: cover::render(img, area.width, area.height, app.cover_mode),
        });
    }
    if let Some(cache) = &app.cover_cache {
        cover::draw(&cache.cells, area, f.buffer_mut());
    }
}

fn draw_queue(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Queue;
    let block = panel(app, format!("File d'attente · {}", app.queue.len()), focused);
    let inner = block.inner(area);
    app.areas.queue = area;
    f.render_widget(block, area);

    let [content, hint] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    app.areas.queue_rows = Rect { y: content.y + 1, height: content.height.saturating_sub(1), ..content };
    if app.queue.is_empty() {
        let empty = Paragraph::new(vec![Line::default(), Line::from("File vide : / pour chercher un morceau").fg(t.muted)]);
        f.render_widget(empty, content);
    } else {
        let dragging = app.drag_from;
        let rows = app.queue.iter().enumerate().map(|(i, item)| {
            let style = if dragging == Some(i) { Style::new().fg(t.accent2) } else { Style::new().fg(t.text) };
            Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                format!("{:>2}", i + 1).fg(t.muted).into(),
                item.track.name.clone().into(),
                item.track.artists.clone().fg(t.muted).into(),
                item.added_by.clone().fg(t.muted).into(),
            ])
            .style(style)
        });
        let table = Table::new(
            rows,
            [Constraint::Length(2), Constraint::Percentage(45), Constraint::Percentage(35), Constraint::Percentage(20)],
        )
        .header(Row::new(vec!["#", "Titre", "Artiste", "Par"]).fg(t.muted))
        .row_highlight_style(Style::new().bg(t.selection).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ")
        .highlight_spacing(HighlightSpacing::Always);
        f.render_stateful_widget(table, content, &mut app.queue_state);
    }
    let hint_text = if focused { "j/k sélection · t en tête · J/K ou glisser pour déplacer · d retirer" } else { "" };
    f.render_widget(Paragraph::new(Line::from(hint_text).fg(t.muted)), hint);
}

fn draw_members(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let block = panel(app, format!("Membres · {}", app.members.len()), app.focus == Focus::Members);
    let inner = block.inner(area);
    app.areas.members = area;
    app.areas.members_rows = inner;
    f.render_widget(block, area);

    let name_w = app
        .members
        .iter()
        .map(|m| m.name.chars().count() + if m.id == app.me { 6 } else { 0 })
        .max()
        .unwrap_or(0);
    let host = app.is_host();
    let items = app.members.iter().filter(|m| host || !m.pending).map(|m| {
        let me = if m.id == app.me { " (toi)" } else { "" };
        let name = format!("{}{me}", m.name);
        let role = match (m.pending, m.online) {
            (true, _) => "en attente".to_owned(),
            (_, false) => "hors ligne".to_owned(),
            _ => m.role.label().to_owned(),
        };
        let custom = if m.overrides.is_empty() { "" } else { " *" };
        let name_color = if m.online { t.text } else { t.muted };
        ListItem::new(Line::from(vec![
            Span::raw(format!("{} ", role_icon(m.role))).fg(if m.role == Role::Host { t.accent } else { t.muted }),
            Span::raw(format!("{name:<name_w$}")).fg(name_color),
            Span::raw(format!("  {role}{custom}")).fg(if m.pending { t.warn } else { t.muted }),
        ]))
    });
    let list = List::new(items).highlight_style(Style::new().bg(t.selection));
    f.render_stateful_widget(list, inner, &mut app.members_state);
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Search;
    let block = panel(app, "Recherche".into(), focused);
    let inner = block.inner(area);
    app.areas.search = area;
    f.render_widget(block, area);

    let [input, results] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    app.areas.search_input = input;
    app.areas.results_rows = results;

    let s = &app.search;
    let cursor = if s.editing { "█" } else { "" };
    let mut spans = vec![Span::raw("/ ").fg(t.accent), Span::raw(s.input.clone()).fg(t.text), Span::raw(cursor).fg(t.accent)];
    if !s.editing && s.input.is_empty() {
        spans.push(Span::raw("tape / pour chercher un titre, un artiste…").fg(t.muted));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), input);
    if s.total > SEARCH_PAGE && !s.editing && !s.loading {
        let page = format!("page {}/{} · [ ] ", s.offset / SEARCH_PAGE + 1, s.total.div_ceil(SEARCH_PAGE));
        f.render_widget(Paragraph::new(Line::from(page).fg(t.muted)).alignment(Alignment::Right), input);
    }

    // While typing, keep the previous results on screen until the new ones
    // arrive (no flicker); the spinner only shows on an empty list.
    if s.loading && s.results.is_empty() {
        f.render_widget(Paragraph::new(Line::from("Recherche…").fg(t.muted)), results);
        return;
    }
    if s.loading {
        f.render_widget(Paragraph::new(Line::from("recherche… ").fg(t.muted)).alignment(Alignment::Right), input);
    }
    let liked = &app.liked;
    let rows = s.results.iter().map(|track| {
        let heart = if liked.get(&track.uri) == Some(&true) { "♥ " } else { "" };
        Row::new::<Vec<ratatui::widgets::Cell>>(vec![
            Line::from(vec![Span::raw(heart).fg(t.bad), Span::raw(track.name.clone()).fg(t.text)]).into(),
            track.artist_names().fg(t.muted).into(),
            fmt_ms(track.duration_ms).fg(t.muted).into(),
        ])
    });
    let hint = if focused && !s.results.is_empty() { "Entrée ajouter" } else { "" };
    let table = Table::new(rows, [Constraint::Percentage(55), Constraint::Percentage(35), Constraint::Length(6)])
        .row_highlight_style(Style::new().bg(t.selection).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(table, results, &mut app.search.state);
    if !hint.is_empty() {
        let corner = Rect { y: area.bottom().saturating_sub(1), height: 1, ..area.inner(Margin::new(2, 0)) };
        f.render_widget(Paragraph::new(Line::from(format!(" {hint} ")).fg(t.accent)).alignment(Alignment::Right), corner);
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let me = app.me().map(|m| m.name.as_str()).unwrap_or("?");
    let role = app.me().map(|m| format!(" ({})", m.role.label().to_lowercase())).unwrap_or_default();
    // The full invite link is long (peer-to-peer key): only the code here,
    // `i` copies the link.
    let room = if app.invite.is_some() {
        format!("Salon {} · i inviter", app.settings.code)
    } else {
        format!("Salon {}", app.settings.code)
    };
    let members = format!("{} membre{}", app.members.len(), if app.members.len() > 1 { "s" } else { "" });
    let left = status_line(&room, &members, &format!("{me}{role}"), area.width as usize);
    let bar = Style::new().bg(t.selection).fg(t.text);
    f.render_widget(Paragraph::new(Line::from(left)).style(bar), area);
    if let Some((text, level)) = app.active_toast() {
        let toast = Line::from(format!(" {text} ")).fg(level_color(app, level)).bold();
        f.render_widget(Paragraph::new(toast).style(bar).alignment(Alignment::Right), area);
    }
}

/// Status bar text fitting `width`: the key hints are dropped from the least
/// useful one until the line fits.
pub fn status_line(room: &str, members: &str, me: &str, width: usize) -> String {
    const HINTS: [&str; 9] = ["? aide", "q quitter", "/ chercher", "l bibliothèque", "v visualiseur", "y paroles", "espace pause", "s suivant", "+/- volume"];
    let base = format!(" {room} │ {members} │ {me}");
    let mut hints: Vec<&str> = HINTS.to_vec();
    loop {
        let line = if hints.is_empty() { base.clone() } else { format!("{base} │ {}", hints.join("  ")) };
        if line.chars().count() <= width || hints.is_empty() {
            return line;
        }
        hints.pop();
    }
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let w = 64.min(area.width);
    let h = 32.min(area.height);
    let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, popup);
    let key = |k: &str, what: &str| Line::from(vec![Span::raw(format!("  {k:<14}")).fg(t.accent), Span::raw(what.to_owned()).fg(t.text)]);
    let lines = vec![
        Line::default(),
        key("tab / maj+tab", "panneau suivant / précédent"),
        key("/", "chercher (Entrée lance, Échap sort)"),
        key("Entrée ou a", "ajouter le résultat sélectionné"),
        key("[ et ]", "page de résultats précédente / suivante"),
        key("espace", "lecture / pause du salon"),
        key("s", "morceau suivant (ou vote, selon ton rôle)"),
        key("← →", "reculer / avancer de 10 s"),
        key("j k  ↑ ↓", "sélection dans le panneau"),
        key("J K", "déplacer le morceau dans la file"),
        key("t", "passer le morceau en tête de file"),
        key("d  Suppr", "retirer le morceau de la file"),
        key("r", "resynchroniser ton Spotify"),
        key("c", "pochette : ascii / blocs"),
        key("p", "permissions des membres (hôte)"),
        key("l", "ta bibliothèque : titres likés et playlists"),
        key("v", "visualiseur plein écran (10 styles)"),
        key("y", "paroles synchronisées plein écran"),
        key("+ -", "ton volume (toi seul)"),
        key("m", "muet pour ce titre (toi seul)"),
        key("f", "j'aime / je n'aime plus"),
        key("A", "playlist d'autoplay (hôte)"),
        key("i", "copier le lien d'invitation"),
        key("q  Ctrl+C", "quitter le salon"),
        Line::default(),
        Line::from("  Souris : clic sur les boutons, la barre de progression,").fg(t.muted),
        Line::from("  les lignes ; glisser une ligne de la file pour la déplacer ;").fg(t.muted),
        Line::from("  clic sur un résultat déjà sélectionné pour l'ajouter.").fg(t.muted),
    ];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.accent))
        .title(Line::from(" Aide · une touche pour fermer ").fg(t.accent).bold());
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn role_icon(role: Role) -> &'static str {
    match role {
        Role::Host => "★",
        Role::Dj => "♫",
        Role::Listener => "·",
        Role::Spectator => "○",
    }
}

/// Permission cell: `[x]` granted, `[ ]` refused, `[~]` own tracks only,
/// `[v]` vote only; `*` marks a per-member override.
fn perm_cell(m: &Member, perm: Perm) -> String {
    let p = m.perms;
    let mark = match perm {
        _ if p.get(perm) => "[x]",
        Perm::QueueManage if p.queue_add => "[~]",
        Perm::Skip if p.listen => "[v]",
        _ => "[ ]",
    };
    let star = if m.overrides.contains_key(&perm) { "*" } else { " " };
    format!("{mark}{star}")
}

fn draw_permissions(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let members: Vec<Member> = app.panel_members().into_iter().cloned().collect();
    let w = 86.min(area.width);
    let h = (members.len() as u16 + 10).min(area.height);
    let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    app.areas.perm_popup = popup;
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.accent))
        .title(Line::from(format!(" Permissions · salon {} ", app.settings.code)).fg(t.accent).bold())
        .title(Line::from(" Échap fermer ").fg(t.muted).right_aligned());
    let inner = block.inner(popup).inner(Margin::new(1, 0));
    f.render_widget(block, popup);

    // Columns: name, role, then the permissions.
    const NAME_W: u16 = 20;
    const ROLE_W: u16 = 14;
    const PERM_W: u16 = 9;
    let mut cols = [(0u16, 0u16); PERM_COLUMNS];
    for (i, col) in cols.iter_mut().enumerate() {
        *col = match i {
            0 => (inner.x + NAME_W, ROLE_W),
            _ => (inner.x + NAME_W + ROLE_W + (i as u16 - 1) * PERM_W, PERM_W),
        };
    }
    app.areas.perm_cols = cols;

    let mut header = vec![Span::raw(format!("{:<w$}", "Membre", w = NAME_W as usize)), Span::raw(format!("{:<w$}", "Rôle", w = ROLE_W as usize))];
    header.extend(Perm::ALL.iter().map(|p| Span::raw(format!("{:<w$}", p.label(), w = PERM_W as usize))));
    let mut lines = vec![Line::default(), Line::from(header).fg(t.muted).bold()];

    let panel = app.perm_panel.clone().unwrap_or_default();
    for (row, m) in members.iter().enumerate() {
        let cell_style = |col: usize| {
            if row == panel.row && col == panel.col { Style::new().bg(t.accent).fg(t.selection).bold() } else { Style::new() }
        };
        let name: String = format!("{} {}", role_icon(m.role), m.name).chars().take(NAME_W as usize - 1).collect();
        let role = if m.pending { "en attente".to_owned() } else { m.role.label().to_owned() };
        let read_only = m.role == Role::Host;
        let mut spans = vec![
            Span::raw(format!("{name:<w$}", w = NAME_W as usize)).fg(t.text),
            Span::styled(format!("{role:<w$}", w = ROLE_W as usize), cell_style(0)).fg(if m.pending { t.warn } else { t.text }),
        ];
        for (i, perm) in Perm::ALL.iter().enumerate() {
            let text = format!("{:<w$}", perm_cell(m, *perm), w = PERM_W as usize);
            let color = if read_only { t.muted } else if m.overrides.contains_key(perm) { t.warn } else { t.text };
            spans.push(Span::styled(text, cell_style(i + 1)).fg(color));
        }
        let line = Line::from(spans);
        lines.push(if row == panel.row { line.bg(t.selection) } else { line });
    }
    app.areas.perm_rows = Rect { x: inner.x, y: inner.y + 2, width: inner.width, height: members.len() as u16 };

    let yes_no = |b: bool| if b { "oui" } else { "non" };
    lines.extend([
        Line::default(),
        Line::from("[x] autorisé  [ ] refusé  [~] ses propres ajouts  [v] vote seulement  * exception").fg(t.muted),
        Line::default(),
        Line::from(vec![
            Span::raw("Salle d'attente : ").fg(t.muted),
            Span::raw(yes_no(app.settings.waiting_room)).fg(t.text),
            Span::raw(" (w)   Rôle des nouveaux : ").fg(t.muted),
            Span::raw(app.settings.default_role.label()).fg(t.text),
            Span::raw(" (D)   Code : ").fg(t.muted),
            Span::raw(app.settings.code.clone()).fg(t.text),
            Span::raw(" (n)").fg(t.muted),
        ]),
        Line::from("↑↓ membre  ←→ colonne  espace/clic basculer  r rôle  x exception par défaut").fg(t.muted),
        Line::from("a admettre  K exclure  b bannir (2 fois)  n nouveau code").fg(t.muted),
    ]);
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_picker(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let Some(picker) = app.playlist_picker.as_mut() else { return };
    let w = 60.min(area.width);
    let h = 20.min(area.height);
    let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.accent))
        .title(Line::from(" Playlist d'autoplay ").fg(t.accent).bold())
        .title(Line::from(" Échap fermer ").fg(t.muted).right_aligned());
    let inner = block.inner(popup).inner(Margin::new(1, 0));
    f.render_widget(block, popup);
    let [intro, list, help] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::default(),
            Line::from("Jouée au hasard quand la file est vide.").fg(t.muted),
        ]),
        intro,
    );
    if picker.loading {
        f.render_widget(Paragraph::new(Line::from("Chargement de tes playlists…").fg(t.muted)), list);
    } else if picker.playlists.is_empty() {
        f.render_widget(Paragraph::new(Line::from("Aucune playlist sur ton compte.").fg(t.muted)), list);
    } else {
        let items = picker.playlists.iter().map(|p| {
            ListItem::new(Line::from(vec![
                Span::raw(p.name.clone()).fg(t.text),
                Span::raw(format!("  {} morceaux", p.items.total)).fg(t.muted),
            ]))
        });
        let widget = List::new(items).highlight_style(Style::new().bg(t.selection).bold()).highlight_symbol("› ");
        f.render_stateful_widget(widget, list, &mut picker.state);
    }
    f.render_widget(
        Paragraph::new(vec![
            Line::from("Entrée choisir  o désactiver l'autoplay").fg(t.muted),
            Line::from("Seules tes propres playlists sont lisibles (limite Spotify).").fg(t.muted),
        ]),
        help,
    );
    app.areas.picker_popup = popup;
    app.areas.picker_rows = list;
}

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let perms = app.my_perms();
    let liked = &app.liked;
    let Some(library) = app.library.as_mut() else { return };
    let w = 100.min(area.width.saturating_sub(4)).max(40.min(area.width));
    let h = (area.height * 4 / 5).max(12.min(area.height));
    let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, popup);
    let title = match &library.open {
        Some(open) => format!(" Bibliothèque › {} ", open.name),
        None => " Bibliothèque ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.accent))
        .title(Line::from(title).fg(t.accent).bold())
        .title(Line::from(" Échap retour/fermer ").fg(t.muted).right_aligned());
    let inner = block.inner(popup).inner(Margin::new(1, 0));
    f.render_widget(block, popup);
    let [head, list, help] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    app.areas.library_popup = popup;
    app.areas.library_rows = list;

    let key = |k: &str, label: &str, ok: bool| {
        vec![
            Span::raw(format!("{k} ")).fg(if ok { t.accent } else { t.border }),
            Span::raw(format!("{label}   ")).fg(if ok { t.text } else { t.border }),
        ]
    };

    // "/ filtre█   3 / 92" on the head line.
    let filter_line = |filter: &crate::library::Filter, count: String, placeholder: &str| {
        let mut spans = vec![Span::raw("/ ").fg(t.accent)];
        match (filter.is_active(), filter.editing) {
            (false, false) => spans.push(Span::raw(placeholder.to_owned()).fg(t.muted)),
            _ => {
                spans.push(Span::raw(filter.text.clone()).fg(t.text));
                if filter.editing {
                    spans.push(Span::raw("█").fg(t.accent));
                }
            }
        }
        spans.push(Span::raw(format!("   {count}")).fg(t.muted));
        Line::from(spans)
    };

    match library.open.as_mut() {
        None => {
            let visible = library.visible();
            let count = if library.filter.is_active() {
                format!("{} / {} listes", visible.len(), library.rows())
            } else {
                format!("{} listes", library.rows())
            };
            f.render_widget(Paragraph::new(filter_line(&library.filter, count, "filtrer tes playlists")), head);
            if library.loading {
                f.render_widget(Paragraph::new(Line::from("Chargement de tes playlists…").fg(t.muted)), list);
            } else if visible.is_empty() {
                f.render_widget(Paragraph::new(Line::from("Aucune liste ne correspond au filtre.").fg(t.muted)), list);
            } else {
                let items = visible.iter().filter_map(|&i| library.row(i)).map(|(source, name, total)| {
                    let liked = source == crate::library::Source::Liked;
                    ListItem::new(Line::from(vec![
                        if liked { Span::raw("♥ ").fg(t.bad) } else { Span::raw("♪ ").fg(t.accent) },
                        if liked { Span::raw(name).fg(t.text).bold() } else { Span::raw(name).fg(t.text) },
                        Span::raw(total.map(|n| format!("  {n} titres")).unwrap_or_default()).fg(t.muted),
                    ]))
                });
                let widget = List::new(items.collect::<Vec<_>>())
                    .highlight_style(Style::new().bg(t.selection).bold())
                    .highlight_symbol("› ");
                f.render_stateful_widget(widget, list, &mut library.state);
            }
            let mut spans = key("Entrée", "ouvrir", true);
            spans.extend(key("/", "filtrer", true));
            spans.extend(key("↑↓", "choisir", true));
            spans.extend(key("Échap", if library.filter.is_active() { "effacer le filtre" } else { "fermer" }, true));
            f.render_widget(Paragraph::new(Line::from(spans)), help);
        }
        Some(open) => {
            let visible = open.visible();
            let loaded = open.tracks.len();
            let mut count = match (open.loading, open.total) {
                (true, _) => "chargement…".to_owned(),
                (false, Some(total)) if total as usize > loaded => format!("{loaded} titres (les {loaded} premiers sur {total})"),
                (false, _) => format!("{loaded} titres"),
            };
            if open.filter.is_active() && !open.loading {
                count = format!("{} / {count}", visible.len());
            }
            f.render_widget(Paragraph::new(filter_line(&open.filter, count, "filtrer par titre, artiste ou album")), head);
            if open.loading {
                f.render_widget(Paragraph::new(Line::from("Chargement des titres…").fg(t.muted)), list);
            } else if visible.is_empty() {
                f.render_widget(Paragraph::new(Line::from("Aucun titre ne correspond au filtre.").fg(t.muted)), list);
            } else {
                let rows = visible.iter().map(|&i| (i, &open.tracks[i])).map(|(i, track)| {
                    let heart = if liked.get(&track.uri) == Some(&true) { "♥ " } else { "" };
                    Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                        format!("{:>3}", i + 1).fg(t.muted).into(),
                        Line::from(vec![Span::raw(heart).fg(t.bad), Span::raw(track.name.clone()).fg(t.text)]).into(),
                        track.artist_names().fg(t.muted).into(),
                        fmt_ms(track.duration_ms).fg(t.muted).into(),
                    ])
                });
                let table = Table::new(
                    rows,
                    [Constraint::Length(3), Constraint::Percentage(50), Constraint::Percentage(35), Constraint::Length(6)],
                )
                .row_highlight_style(Style::new().bg(t.selection).add_modifier(Modifier::BOLD))
                .highlight_symbol("› ")
                .highlight_spacing(HighlightSpacing::Always);
                f.render_stateful_widget(table, list, &mut open.state);
            }
            // With a filter, whole-list actions apply to the matching tracks.
            let (all, list_word) = if open.filter.is_active() { ("ajouter ces titres", "les jouer") } else { ("tout ajouter à la file", "jouer la liste") };
            let mut first = key("Entrée", "ajouter le titre", perms.queue_add);
            first.extend(key("t", all, perms.queue_manage));
            first.extend(key("/", "filtrer", true));
            first.extend(key("f", "j'aime", true));
            let mut second = key("p", list_word, perms.queue_manage && perms.playback);
            second.extend(key("s", "en aléatoire", perms.queue_manage && perms.playback));
            second.extend(key("Échap", if open.filter.is_active() { "effacer le filtre" } else { "retour" }, true));
            f.render_widget(Paragraph::new(vec![Line::from(first), Line::from(second)]), help);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::status_line;

    #[test]
    fn status_line_keeps_the_essentials_on_narrow_terminals() {
        let wide = status_line("Salon KD8YZS · i inviter", "1 membre", "Alenzo.amico (hôte)", 200);
        assert!(wide.contains("KD8YZS") && wide.contains("+/- volume") && wide.contains("? aide"));
        let narrow = status_line("Salon KD8YZS · i inviter", "1 membre", "Alenzo.amico (hôte)", 90);
        assert!(narrow.chars().count() <= 90, "{narrow}");
        assert!(narrow.contains("KD8YZS") && narrow.contains("Alenzo.amico") && narrow.contains("? aide"));
        assert!(!narrow.contains("+/- volume"));
    }
}

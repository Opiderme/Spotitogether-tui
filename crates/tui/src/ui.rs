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

pub fn draw(f: &mut Frame, app: &mut App) {
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
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(PREV).fg(t.text),
            Span::raw("  "),
            Span::raw(play).fg(t.accent).bold(),
            Span::raw("  "),
            Span::raw(NEXT).fg(t.text),
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
    let hint_text = if focused { "j/k sélection · J/K ou glisser pour déplacer · d retirer" } else { "" };
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
    let rows = s.results.iter().map(|track| {
        Row::new::<Vec<ratatui::widgets::Cell>>(vec![
            track.name.clone().fg(t.text).into(),
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
    let room = match app.current_invite() {
        Some(link) if app.hosting => format!("{link} (i copier)"),
        _ => app.settings.code.clone(),
    };
    let left = format!(
        " {room} │ {} membre{} │ {me}{role} │ / chercher  l bibliothèque  espace pause  s suivant  ? aide  q quitter",
        app.members.len(),
        if app.members.len() > 1 { "s" } else { "" },
    );
    let bar = Style::new().bg(t.selection).fg(t.text);
    f.render_widget(Paragraph::new(Line::from(left)).style(bar), area);
    if let Some((text, level)) = app.active_toast() {
        let toast = Line::from(format!(" {text} ")).fg(level_color(app, level)).bold();
        f.render_widget(Paragraph::new(toast).style(bar).alignment(Alignment::Right), area);
    }
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let t = app.theme;
    let w = 64.min(area.width);
    let h = 26.min(area.height);
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
        key("d  Suppr", "retirer le morceau de la file"),
        key("r", "resynchroniser ton Spotify"),
        key("c", "pochette : ascii / blocs"),
        key("p", "permissions des membres (hôte)"),
        key("l", "ta bibliothèque : titres likés et playlists"),
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

    match library.open.as_mut() {
        None => {
            f.render_widget(Paragraph::new(Line::from("Tes titres likés et tes playlists").fg(t.muted)), head);
            if library.loading {
                f.render_widget(Paragraph::new(Line::from("Chargement de tes playlists…").fg(t.muted)), list);
            } else {
                let mut items = vec![ListItem::new(Line::from(vec![
                    Span::raw("♥ ").fg(t.bad),
                    Span::raw("Titres likés").fg(t.text).bold(),
                ]))];
                items.extend(library.playlists.iter().map(|p| {
                    ListItem::new(Line::from(vec![
                        Span::raw("♪ ").fg(t.accent),
                        Span::raw(p.name.clone()).fg(t.text),
                        Span::raw(format!("  {} titres", p.items.total)).fg(t.muted),
                    ]))
                }));
                let widget = List::new(items).highlight_style(Style::new().bg(t.selection).bold()).highlight_symbol("› ");
                f.render_stateful_widget(widget, list, &mut library.state);
            }
            let mut spans = key("Entrée", "ouvrir", true);
            spans.extend(key("↑↓", "choisir", true));
            spans.extend(key("Échap", "fermer", true));
            f.render_widget(Paragraph::new(Line::from(spans)), help);
        }
        Some(open) => {
            let count = match (open.loading, open.total) {
                (true, _) => "chargement…".to_owned(),
                (false, Some(total)) if total as usize > open.tracks.len() => {
                    format!("{} titres (les {} premiers sur {total})", open.tracks.len(), open.tracks.len())
                }
                (false, _) => format!("{} titres", open.tracks.len()),
            };
            f.render_widget(Paragraph::new(Line::from(count).fg(t.muted)), head);
            if open.loading {
                f.render_widget(Paragraph::new(Line::from("Chargement des titres…").fg(t.muted)), list);
            } else {
                let rows = open.tracks.iter().enumerate().map(|(i, track)| {
                    Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                        format!("{:>3}", i + 1).fg(t.muted).into(),
                        track.name.clone().fg(t.text).into(),
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
            let mut first = key("Entrée", "ajouter le titre", perms.queue_add);
            first.extend(key("t", "tout ajouter à la file", perms.queue_manage));
            let mut second = key("p", "jouer la liste", perms.queue_manage && perms.playback);
            second.extend(key("s", "jouer en aléatoire", perms.queue_manage && perms.playback));
            second.extend(key("Échap", "retour", true));
            f.render_widget(Paragraph::new(vec![Line::from(first), Line::from(second)]), help);
        }
    }
}

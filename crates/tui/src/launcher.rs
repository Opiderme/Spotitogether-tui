//! Home screen shown by `spotitogether` without arguments: create a room
//! (hosted by this process) or join one with an invite link.

use std::io::stdout;

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyCode,
    KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq)]
pub enum Account {
    /// Logged in to Spotify as this display name.
    Ready(String),
    /// Client ID configured, no token (or an expired one).
    NotLoggedIn,
    /// No Spotify app configured yet.
    NotConfigured,
}

pub struct LauncherInfo {
    pub account: Account,
    /// Pre-filled invite (last one used, or given on the command line).
    pub invite: String,
    /// Message from the previous attempt (error or "left the room").
    pub message: Option<(String, bool)>,
    /// Join `invite` right away (link opened from outside).
    pub auto_join: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LauncherChoice {
    Host,
    Join { invite: String },
    Setup,
    Login,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Item {
    Setup,
    Login,
    Host,
    Join,
    Quit,
}

pub struct Launcher {
    account: Account,
    items: Vec<Item>,
    selected: usize,
    invite: String,
    editing: bool,
    message: Option<(String, bool)>,
    theme: Theme,
    /// Clickable rows (one per item) and the invite field, from the last draw.
    item_rows: Vec<Rect>,
    input: Rect,
}

impl Launcher {
    pub fn new(info: LauncherInfo, theme: Theme) -> Self {
        let mut items = match info.account {
            Account::NotConfigured => vec![Item::Setup],
            Account::NotLoggedIn => vec![Item::Login],
            Account::Ready(_) => vec![],
        };
        items.extend([Item::Host, Item::Join, Item::Quit]);
        Self {
            account: info.account,
            items,
            selected: 0,
            invite: info.invite,
            editing: false,
            message: info.message,
            theme,
            item_rows: vec![],
            input: Rect::default(),
        }
    }

    fn select(&mut self, item: Item) {
        if let Some(i) = self.items.iter().position(|it| *it == item) {
            self.selected = i;
        }
    }

    fn current(&self) -> Item {
        self.items[self.selected]
    }

    fn activate(&mut self) -> Option<LauncherChoice> {
        match self.current() {
            Item::Setup => Some(LauncherChoice::Setup),
            Item::Login => Some(LauncherChoice::Login),
            Item::Host => Some(LauncherChoice::Host),
            Item::Quit => Some(LauncherChoice::Quit),
            Item::Join if self.editing && !self.invite.trim().is_empty() => {
                Some(LauncherChoice::Join { invite: self.invite.trim().to_owned() })
            }
            Item::Join => {
                self.editing = true;
                None
            }
        }
    }

    pub fn on_event(&mut self, event: Event) -> Option<LauncherChoice> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Paste(text) => {
                self.select(Item::Join);
                if !self.editing {
                    self.invite.clear();
                }
                self.editing = true;
                self.invite.push_str(text.trim());
                None
            }
            Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                let pos = Position::new(m.column, m.row);
                if self.input.contains(pos) {
                    self.select(Item::Join);
                    self.editing = true;
                    return None;
                }
                let i = self.item_rows.iter().position(|r| r.contains(pos))?;
                self.selected = i;
                self.editing = false;
                self.activate()
            }
            _ => None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<LauncherChoice> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(LauncherChoice::Quit);
        }
        if self.editing {
            match key.code {
                KeyCode::Esc => self.editing = false,
                KeyCode::Enter => return self.activate(),
                KeyCode::Backspace => {
                    self.invite.pop();
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                    self.editing = false;
                    return self.on_key(key);
                }
                KeyCode::Char(c) => self.invite.push(c),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                self.selected = (self.selected + self.items.len() - 1) % self.items.len();
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.selected = (self.selected + 1) % self.items.len(),
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
            KeyCode::Char('q') | KeyCode::Esc => return Some(LauncherChoice::Quit),
            KeyCode::Char('c') => {
                self.select(Item::Host);
                return self.activate();
            }
            KeyCode::Char('r') | KeyCode::Char('/') => {
                self.select(Item::Join);
                self.editing = true;
            }
            _ => {}
        }
        None
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let t = self.theme;
        let area = f.area();
        let w = 74.min(area.width);
        let extra = if self.message.is_some() { 3 } else { 0 };
        let h = (self.items.len() as u16 * 2 + 12 + extra).min(area.height);
        let card = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
        f.render_widget(Clear, card);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(t.accent))
            .title(Line::from(" Spotitogether ").fg(t.accent).bold());
        let inner = block.inner(card).inner(Margin::new(2, 1));
        f.render_widget(block, card);

        let [head, _, menu, message, help] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(self.items.len() as u16 * 2 + 1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let account = match &self.account {
            Account::Ready(name) => Line::from(vec![
                Span::raw("Compte Spotify : ").fg(t.muted),
                Span::raw(name.clone()).fg(t.text).bold(),
                Span::raw("  ●").fg(t.good),
            ]),
            Account::NotLoggedIn => Line::from("Compte Spotify : non connecté (tu peux rejoindre en spectateur)").fg(t.warn),
            Account::NotConfigured => Line::from("Spotify n'est pas encore configuré (tu peux rejoindre en spectateur)").fg(t.warn),
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from("♪ Écoute Spotify synchronisée entre amis").fg(t.text).bold(),
                Line::default(),
                account,
            ]),
            head,
        );

        self.item_rows.clear();
        self.input = Rect::default();
        let rows = Layout::vertical(vec![Constraint::Length(2); self.items.len()]).split(menu);
        for (i, (item, row)) in self.items.iter().zip(rows.iter()).enumerate() {
            let selected = i == self.selected;
            let (title, detail) = match item {
                Item::Setup => ("Configurer Spotify", "créer ton app Spotify et enregistrer son Client ID".to_owned()),
                Item::Login => ("Se connecter à Spotify", "ouvre le navigateur pour autoriser l'accès".to_owned()),
                Item::Host => ("Créer un salon", "héberger ici : tes amis te rejoignent avec un lien".to_owned()),
                Item::Join => ("Rejoindre un salon", String::new()),
                Item::Quit => ("Quitter", String::new()),
            };
            let marker = if selected { "› " } else { "  " };
            let title_style = if selected { Style::new().fg(t.accent).bold() } else { Style::new().fg(t.text) };
            let [line1, line2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(*row);
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw(marker).fg(t.accent), Span::styled(title, title_style)])), line1);
            if *item == Item::Join {
                // Invite field on the second line.
                let field = Rect { x: line2.x + 4, width: line2.width.saturating_sub(4), ..line2 };
                let cursor = if self.editing { "█" } else { "" };
                let text = if self.invite.is_empty() && !self.editing {
                    Span::raw("colle le lien d'invitation (spotitogether://…)").fg(t.muted)
                } else {
                    Span::raw(self.invite.clone()).fg(t.text)
                };
                let style = if self.editing { Style::new().bg(t.selection) } else { Style::new() };
                f.render_widget(Paragraph::new(Line::from(vec![text, Span::raw(cursor).fg(t.accent)])).style(style), field);
                self.input = field;
            } else if !detail.is_empty() {
                f.render_widget(Paragraph::new(Line::from(format!("    {detail}")).fg(t.muted)), line2);
            }
            self.item_rows.push(*row);
        }

        if let Some((text, error)) = &self.message {
            let color = if *error { t.bad } else { t.muted };
            f.render_widget(Paragraph::new(Line::from(text.clone()).fg(color)).wrap(ratatui::widgets::Wrap { trim: true }), message);
        }
        let hint = if self.editing {
            "Entrée rejoindre · Échap annuler · Ctrl+Maj+V coller"
        } else {
            "↑↓ choisir · Entrée valider · c créer · r rejoindre · q quitter"
        };
        f.render_widget(Paragraph::new(Line::from(hint).fg(t.muted)).alignment(Alignment::Center), help);
    }
}

/// Shows the home screen until the user picks something.
pub async fn run(info: LauncherInfo) -> Result<LauncherChoice> {
    if info.auto_join && !info.invite.trim().is_empty() {
        return Ok(LauncherChoice::Join { invite: info.invite.trim().to_owned() });
    }
    let mut launcher = Launcher::new(info, Theme::load());
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut events = EventStream::new();
    let choice = loop {
        if let Err(e) = terminal.draw(|f| launcher.draw(f)) {
            break Err(e.into());
        }
        match events.next().await {
            Some(Ok(event)) => {
                if let Some(choice) = launcher.on_event(event) {
                    break Ok(choice);
                }
            }
            Some(Err(e)) => break Err(e.into()),
            None => break Ok(LauncherChoice::Quit),
        }
    };
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    choice
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launcher(account: Account, invite: &str) -> Launcher {
        Launcher::new(LauncherInfo { account, invite: invite.into(), message: None, auto_join: false }, Theme::default())
    }

    fn key(c: KeyCode) -> Event {
        Event::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    #[test]
    fn menu_depends_on_the_account() {
        assert_eq!(launcher(Account::Ready("A".into()), "").items, [Item::Host, Item::Join, Item::Quit]);
        assert_eq!(launcher(Account::NotLoggedIn, "").items[0], Item::Login);
        assert_eq!(launcher(Account::NotConfigured, "").items[0], Item::Setup);
    }

    #[test]
    fn create_and_join_flows() {
        let mut l = launcher(Account::Ready("A".into()), "");
        assert_eq!(l.on_event(key(KeyCode::Enter)), Some(LauncherChoice::Host));

        let mut l = launcher(Account::Ready("A".into()), "");
        l.on_event(key(KeyCode::Down));
        assert_eq!(l.on_event(key(KeyCode::Enter)), None, "first Enter edits the field");
        for c in "192.168.1.2/AB12CD".chars() {
            l.on_event(key(KeyCode::Char(c)));
        }
        assert_eq!(l.on_event(key(KeyCode::Enter)), Some(LauncherChoice::Join { invite: "192.168.1.2/AB12CD".into() }));
    }

    #[test]
    fn pasting_a_link_fills_the_field_and_q_is_typed_while_editing() {
        let mut l = launcher(Account::NotLoggedIn, "ancien");
        l.on_event(Event::Paste(" spotitogether://h:7420/QQ1 ".into()));
        assert_eq!(l.current(), Item::Join);
        assert_eq!(l.invite, "spotitogether://h:7420/QQ1");
        assert_eq!(l.on_event(key(KeyCode::Char('q'))), None);
        assert!(l.invite.ends_with('q'));
        l.on_event(key(KeyCode::Esc));
        assert_eq!(l.on_event(key(KeyCode::Char('q'))), Some(LauncherChoice::Quit));
    }

    #[test]
    fn renders_and_maps_clicks() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut l = launcher(Account::Ready("Alenzo.amico".into()), "spotitogether://192.168.1.19:7420/K7XQ2M");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| l.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
            .collect();
        println!("{text}");
        for needle in ["Spotitogether", "Alenzo.amico", "Créer un salon", "Rejoindre un salon", "K7XQ2M", "Quitter"] {
            assert!(text.contains(needle), "{needle} missing");
        }
        let quit = l.item_rows[2];
        let click = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: quit.x + 3,
            row: quit.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(l.on_event(click), Some(LauncherChoice::Quit));
    }
}

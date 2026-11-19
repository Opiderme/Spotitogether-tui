//! Held keys for driving. Terminals normally only report key presses (and
//! auto-repeats); the kitty keyboard protocol (foot, kitty, WezTerm,
//! Ghostty…) also reports releases. Without it, a key counts as held for a
//! moment after its last press or repeat.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::physics::Controls;

/// Longer than the usual auto-repeat delay (500–600 ms), so holding a key
/// without release events does not stutter.
const HOLD_WITHOUT_RELEASE: Duration = Duration::from_millis(650);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Accelerate,
    Brake,
    Left,
    Right,
    Handbrake,
}

pub struct Keys {
    /// Release events are reported (kitty keyboard protocol).
    pub releases: bool,
    held: HashMap<Action, Instant>,
}

/// Arrows, ZQSD (AZERTY) and WASD (QWERTY) all drive; space is the
/// handbrake.
pub fn action(code: KeyCode) -> Option<Action> {
    let code = match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    };
    Some(match code {
        KeyCode::Up | KeyCode::Char('z' | 'w') => Action::Accelerate,
        KeyCode::Down | KeyCode::Char('s') => Action::Brake,
        KeyCode::Left | KeyCode::Char('q' | 'a') => Action::Left,
        KeyCode::Right | KeyCode::Char('d') => Action::Right,
        KeyCode::Char(' ') => Action::Handbrake,
        _ => return None,
    })
}

impl Keys {
    pub fn new(releases: bool) -> Self {
        Self { releases, held: HashMap::new() }
    }

    /// Records a driving key; returns false for other keys.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        let Some(action) = action(key.code) else { return false };
        match key.kind {
            KeyEventKind::Release => {
                self.held.remove(&action);
            }
            KeyEventKind::Press | KeyEventKind::Repeat => {
                self.held.insert(action, Instant::now());
            }
        }
        true
    }

    pub fn down(&self, action: Action) -> bool {
        self.held
            .get(&action)
            .is_some_and(|at| self.releases || at.elapsed() < HOLD_WITHOUT_RELEASE)
    }

    pub fn controls(&self) -> Controls {
        let axis = |plus: Action, minus: Action| f32::from(u8::from(self.down(plus))) - f32::from(u8::from(self.down(minus)));
        Controls {
            throttle: axis(Action::Accelerate, Action::Brake),
            steer: axis(Action::Left, Action::Right),
            handbrake: self.down(Action::Handbrake),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    fn ev(code: KeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent { code, modifiers: KeyModifiers::NONE, kind, state: KeyEventState::NONE }
    }

    #[test]
    fn holds_until_release_with_the_kitty_protocol() {
        let mut k = Keys::new(true);
        assert!(k.on_key(ev(KeyCode::Char('Z'), KeyEventKind::Press)));
        k.on_key(ev(KeyCode::Left, KeyEventKind::Press));
        let c = k.controls();
        assert_eq!((c.throttle, c.steer), (1.0, 1.0));
        k.on_key(ev(KeyCode::Char('z'), KeyEventKind::Release));
        assert_eq!(k.controls().throttle, 0.0);
        assert!(!k.on_key(ev(KeyCode::Char('c'), KeyEventKind::Press)), "not a driving key");
    }

    #[test]
    fn opposite_keys_cancel() {
        let mut k = Keys::new(true);
        k.on_key(ev(KeyCode::Left, KeyEventKind::Press));
        k.on_key(ev(KeyCode::Char('d'), KeyEventKind::Press));
        assert_eq!(k.controls().steer, 0.0);
    }
}

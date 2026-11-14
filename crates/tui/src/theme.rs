//! Colours from Caelestia (`~/.local/state/caelestia/scheme.json`), reloaded
//! when the wallpaper changes; ANSI fallback elsewhere.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub accent: Color,
    pub accent2: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub selection: Color,
    pub good: Color,
    pub warn: Color,
    pub bad: Color,
}

impl Default for Theme {
    /// Terminal palette: follows whatever colours the terminal uses.
    fn default() -> Self {
        Self {
            accent: Color::Blue,
            accent2: Color::Magenta,
            text: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            selection: Color::Indexed(236),
            good: Color::Green,
            warn: Color::Yellow,
            bad: Color::Red,
        }
    }
}

#[derive(Deserialize)]
struct Scheme {
    colours: std::collections::HashMap<String, String>,
}

pub fn caelestia_scheme_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("caelestia/scheme.json"))
}

impl Theme {
    /// Caelestia theme when available, terminal palette otherwise.
    pub fn load() -> Self {
        caelestia_scheme_path()
            .and_then(|p| Self::from_caelestia(&p))
            .unwrap_or_default()
    }

    pub fn from_caelestia(path: &Path) -> Option<Self> {
        let scheme: Scheme = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        Self::from_colours(&scheme.colours)
    }

    fn from_colours(c: &std::collections::HashMap<String, String>) -> Option<Self> {
        let get = |keys: &[&str]| keys.iter().find_map(|k| c.get(*k).and_then(|v| hex(v)));
        Some(Self {
            accent: get(&["primary"])?,
            accent2: get(&["tertiary", "secondary"])?,
            text: get(&["text", "onSurface"])?,
            muted: get(&["subtext0", "onSurfaceVariant"])?,
            border: get(&["outline", "outlineVariant"])?,
            selection: get(&["surfaceContainerHighest", "surface1", "surfaceVariant"])?,
            good: get(&["success", "green"])?,
            warn: get(&["yellow", "peach"])?,
            bad: get(&["error", "red"])?,
        })
    }
}

/// "a1b2c3" or "#a1b2c3"
fn hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex() {
        assert_eq!(hex("0a0f0f"), Some(Color::Rgb(10, 15, 15)));
        assert_eq!(hex("#FFFFFF"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(hex("fff"), None);
    }

    #[test]
    fn maps_caelestia_keys_with_fallbacks() {
        let mut c = std::collections::HashMap::new();
        for (k, v) in [
            ("primary", "111111"),
            ("secondary", "222222"),
            ("onSurface", "333333"),
            ("onSurfaceVariant", "444444"),
            ("outline", "555555"),
            ("surfaceVariant", "666666"),
            ("green", "777777"),
            ("yellow", "888888"),
            ("error", "999999"),
        ] {
            c.insert(k.to_owned(), v.to_owned());
        }
        let t = Theme::from_colours(&c).unwrap();
        assert_eq!(t.accent2, Color::Rgb(0x22, 0x22, 0x22));
        assert_eq!(t.good, Color::Rgb(0x77, 0x77, 0x77));
        c.remove("primary");
        assert!(Theme::from_colours(&c).is_none());
    }
}

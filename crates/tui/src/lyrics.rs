//! Lyrics screen (`y`): synced lyrics from LRCLIB (lrclib.net, free and
//! open, no API key), each line appearing at its time somewhere on the
//! screen, typed out, then fading away.

use std::path::PathBuf;
use std::time::Duration;

use rand::RngExt;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use serde::{Deserialize, Serialize};

const API: &str = "https://lrclib.net/api";
/// LRCLIB asks clients to identify themselves (no personal data here).
const USER_AGENT: &str = concat!("Spotitogether/", env!("CARGO_PKG_VERSION"));
/// How long a line stays after the next one started.
const FADE_MS: u64 = 7_000;
/// Show a line slightly before its timestamp (rendering delay).
const LEAD_MS: u64 = 150;
/// Maximum duration difference to accept a search result.
const DURATION_TOLERANCE_S: f64 = 5.0;

/// What to look up (from the room's current track).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Track URI: identifies the request and the cache entry.
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_ms: u64,
}

/// LRCLIB record (subset).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub instrumental: bool,
    #[serde(default)]
    pub plain_lyrics: Option<String>,
    #[serde(default)]
    pub synced_lyrics: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lyrics {
    /// Lines with their timestamps; `approximate` when spread evenly from
    /// unsynced lyrics.
    Timed { lines: Vec<Line>, approximate: bool },
    Instrumental,
    NotFound,
}

/// Parses LRC lines (`[mm:ss.xx] text`, several stamps allowed per line).
pub fn parse_lrc(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let Some((tag, after)) = body.split_once(']') else { break };
            match parse_stamp(tag) {
                Some(ms) => stamps.push(ms),
                None => break, // metadata tag such as [ar:...]
            }
            rest = after;
        }
        for ms in stamps {
            lines.push(Line { at_ms: ms, text: rest.trim().to_owned() });
        }
    }
    lines.sort_by_key(|l| l.at_ms);
    lines
}

fn parse_stamp(tag: &str) -> Option<u64> {
    let (min, sec) = tag.split_once(':')?;
    let min: u64 = min.trim().parse().ok()?;
    let sec: f64 = sec.trim().parse().ok()?;
    Some(min * 60_000 + (sec * 1000.0).round() as u64)
}

impl Lyrics {
    pub fn from_record(record: Option<Record>, duration_ms: u64) -> Self {
        let Some(r) = record else { return Lyrics::NotFound };
        if r.instrumental {
            return Lyrics::Instrumental;
        }
        if let Some(synced) = r.synced_lyrics.as_deref() {
            let lines = parse_lrc(synced);
            if !lines.is_empty() {
                return Lyrics::Timed { lines, approximate: false };
            }
        }
        if let Some(plain) = r.plain_lyrics.as_deref() {
            // Unsynced: spread the lines over the track (rough).
            let texts: Vec<&str> = plain.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            if !texts.is_empty() {
                let step = duration_ms / (texts.len() as u64 + 1);
                let lines = texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Line { at_ms: step * (i as u64 + 1) / 2 + step * i as u64 / 2, text: (*t).to_owned() })
                    .collect();
                return Lyrics::Timed { lines, approximate: true };
            }
        }
        Lyrics::NotFound
    }
}

// ------------------------------------------------------------- fetching

fn cache_path(key: &str) -> Option<PathBuf> {
    let name: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let dirs = directories::ProjectDirs::from("", "", "spotitogether")?;
    Some(dirs.cache_dir().join("lyrics").join(format!("{name}.json")))
}

/// Looks the track up on LRCLIB (exact match, then search), with a disk
/// cache; `Ok(None)` when LRCLIB has nothing for it.
pub async fn fetch(http: &reqwest::Client, q: &Query) -> anyhow::Result<Option<Record>> {
    let path = cache_path(&q.key);
    if let Some(p) = &path
        && let Ok(bytes) = tokio::fs::read(p).await
        && let Ok(cached) = serde_json::from_slice::<Option<Record>>(&bytes)
    {
        return Ok(cached);
    }
    let found = lookup(http, q).await?;
    if let Some(p) = path {
        if let Some(dir) = p.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let _ = tokio::fs::write(p, serde_json::to_vec(&found)?).await;
    }
    Ok(found)
}

async fn lookup(http: &reqwest::Client, q: &Query) -> anyhow::Result<Option<Record>> {
    let duration = (q.duration_ms / 1000).to_string();
    let mut params = vec![("track_name", q.title.as_str()), ("artist_name", q.artist.as_str()), ("duration", duration.as_str())];
    if let Some(album) = &q.album {
        params.push(("album_name", album));
    }
    let exact = get(http, "get", &params).await?;
    if exact.status().is_success() {
        return Ok(Some(exact.json().await?));
    }
    // No exact match: search and keep the closest duration with lyrics.
    let results: Vec<Record> = get(http, "search", &[("track_name", q.title.as_str()), ("artist_name", q.artist.as_str())])
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(best_match(results, q.duration_ms))
}

async fn get(http: &reqwest::Client, endpoint: &str, params: &[(&str, &str)]) -> anyhow::Result<reqwest::Response> {
    for attempt in 0..2 {
        let resp = http
            .get(format!("{API}/{endpoint}"))
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .query(params)
            .send()
            .await?;
        // LRCLIB asks to honour Retry-After.
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
            let wait = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(2u64)
                .min(10);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        return Ok(resp);
    }
    unreachable!("returns on the second attempt")
}

/// Closest duration among results that have lyrics (synced preferred).
pub fn best_match(results: Vec<Record>, duration_ms: u64) -> Option<Record> {
    let target = duration_ms as f64 / 1000.0;
    results
        .into_iter()
        .filter(|r| (r.duration - target).abs() <= DURATION_TOLERANCE_S)
        .filter(|r| r.instrumental || r.synced_lyrics.is_some() || r.plain_lyrics.is_some())
        .min_by(|a, b| {
            let score = |r: &Record| (r.synced_lyrics.is_none(), ((r.duration - target).abs() * 10.0) as i64);
            score(a).cmp(&score(b))
        })
}

// --------------------------------------------------------------- screen

/// How lines appear and disappear (`←/→` or `1`–`8`).
pub const STYLES: [&str; 8] = ["Dispersé", "Karaoké", "Chute", "Décodage", "Vague", "Explosion", "Néon", "Bulles"];
const KARAOKE: usize = 1;
const FALL: usize = 2;
const DECODE: usize = 3;
const WAVE: usize = 4;
const EXPLODE: usize = 5;
const NEON: usize = 6;
const BUBBLES: usize = 7;
const NOISE: &[char] = &['#', '%', '&', '@', '$', '*', '+', '=', '?', '/', '\\', '<', '>', '{', '}', '0', '1', 'X', 'Z', '¤', '§'];

/// How long a line stays once the next one started, per style.
fn exit_ms(style: usize, chars: usize) -> u64 {
    match style {
        FALL => 2_600,
        DECODE => chars as u64 * 30 + 900,
        WAVE => 1_400,
        EXPLODE => 1_600,
        NEON => 1_800,
        BUBBLES => 9_000,
        _ => FADE_MS,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Loading,
    Ready(Lyrics),
    Failed(String),
}

/// A line placed on screen.
#[derive(Debug, Clone)]
struct Placed {
    index: usize,
    x: u16,
    y: u16,
    /// Wrapped text rows.
    rows: Vec<String>,
    color: usize,
    /// Per-line randomness (explosion directions, flicker…).
    seed: u64,
}

impl Placed {
    fn width(&self) -> u16 {
        self.rows.iter().map(|r| r.chars().count()).max().unwrap_or(1) as u16
    }

    fn chars(&self) -> usize {
        self.rows.iter().map(|r| r.chars().count()).sum()
    }
}

pub struct LyricsScreen {
    pub key: Option<String>,
    pub status: Status,
    placed: Vec<Placed>,
    current: Option<usize>,
    last_pos: u64,
    pub palette: Vec<Color>,
    /// Cover the palette was computed from.
    pub palette_key: Option<String>,
    pub style: usize,
    style_changed: std::time::Instant,
    /// Wall clock for effects independent of the song (flicker, wobble).
    clock: std::time::Instant,
}

impl LyricsScreen {
    pub fn new() -> Self {
        Self::with_style(0)
    }

    pub fn with_style(style: usize) -> Self {
        Self {
            key: None,
            status: Status::Loading,
            placed: vec![],
            current: None,
            last_pos: 0,
            palette: vec![],
            palette_key: None,
            style: style % STYLES.len(),
            style_changed: std::time::Instant::now(),
            clock: std::time::Instant::now(),
        }
    }

    pub fn style_name(&self) -> &'static str {
        STYLES[self.style]
    }

    pub fn set_style(&mut self, style: usize) {
        self.style = style % STYLES.len();
        self.style_changed = std::time::Instant::now();
        // Positions depend on the style: start over from the current line.
        self.placed.clear();
        self.current = None;
    }

    pub fn next_style(&mut self, delta: isize) {
        let n = STYLES.len() as isize;
        self.set_style((self.style as isize + delta).rem_euclid(n) as usize);
    }

    /// Name of the style for a moment after a change (banner).
    pub fn banner(&self) -> Option<(&'static str, f32)> {
        let shown = self.style_changed.elapsed().as_secs_f32();
        (shown < 1.5).then(|| (self.style_name(), 1.0 - shown / 1.5))
    }

    /// New track: forget the previous lyrics.
    pub fn reset(&mut self, key: Option<String>) {
        self.key = key;
        self.status = Status::Loading;
        self.placed.clear();
        self.current = None;
        self.last_pos = 0;
    }

    pub fn set_result(&mut self, key: &str, result: Result<Lyrics, String>) {
        if self.key.as_deref() != Some(key) {
            return; // the track changed meanwhile
        }
        self.status = match result {
            Ok(l) => Status::Ready(l),
            Err(e) => Status::Failed(e),
        };
    }

    fn lines(&self) -> Option<&[Line]> {
        match &self.status {
            Status::Ready(Lyrics::Timed { lines, .. }) => Some(lines),
            _ => None,
        }
    }

    /// Index of the line being sung at `pos_ms`.
    pub fn index_at(lines: &[Line], pos_ms: u64) -> Option<usize> {
        let pos = pos_ms + LEAD_MS;
        lines.iter().rposition(|l| l.at_ms <= pos)
    }

    /// Advances to `pos_ms`, placing new lines in `stage`.
    pub fn update(&mut self, pos_ms: u64, stage: Rect) {
        let Some(lines) = self.lines().map(<[Line]>::to_vec) else { return };
        // Seeking backwards: start over.
        if pos_ms + 1500 < self.last_pos {
            self.placed.clear();
            self.current = None;
        }
        self.last_pos = pos_ms;
        let current = Self::index_at(&lines, pos_ms);
        if current != self.current {
            if let Some(i) = current
                && self.style != KARAOKE
            {
                self.place(i, &lines[i].text, stage);
            }
            self.current = current;
        }
        let style = self.style;
        self.placed.retain(|p| {
            let end = lines.get(p.index + 1).map_or(u64::MAX, |l| l.at_ms);
            pos_ms < end.saturating_add(exit_ms(style, p.chars()))
        });
    }

    fn place(&mut self, index: usize, text: &str, stage: Rect) {
        let text = if text.is_empty() { "♪" } else { text };
        let max_w = (usize::from(stage.width) * 2 / 3).max(10);
        let rows = wrap(text, max_w);
        let w = rows.iter().map(|r| r.chars().count()).max().unwrap_or(1) as u16;
        let h = rows.len() as u16;
        let mut rng = rand::rng();
        let seed = rng.random();
        if stage.width < w + 4 || stage.height < h + 2 {
            self.placed.push(Placed { index, x: stage.x, y: stage.y, rows, color: index, seed });
            return;
        }
        // Bubbles are born at the bottom; the other styles anywhere.
        let rows_range = if self.style == BUBBLES {
            stage.bottom().saturating_sub(h + 3).max(stage.y + 1)..=stage.bottom().saturating_sub(h + 1).max(stage.y + 1)
        } else {
            stage.y + 1..=stage.y + stage.height - h - 1
        };
        let overlaps = |x: u16, y: u16, placed: &[Placed]| {
            placed.iter().any(|p| {
                let (pw, ph) = (p.width(), p.rows.len() as u16);
                // One empty row and two columns around each line.
                y < p.y + ph + 1 && p.y < y + h + 1 && x < p.x + pw + 2 && p.x < x + w + 2
            })
        };
        let mut spot = None;
        for _ in 0..60 {
            let x = stage.x + 2 + rng.random_range(0..=stage.width - w - 4);
            let y = rng.random_range(rows_range.clone());
            // Rising bubbles move away: only the newest ones can collide.
            let others: Vec<Placed> = if self.style == BUBBLES { self.placed.iter().rev().take(1).cloned().collect() } else { self.placed.clone() };
            if !overlaps(x, y, &others) {
                spot = Some((x, y));
                break;
            }
        }
        // Screen full: replace the oldest line.
        let (x, y) = spot.unwrap_or_else(|| {
            let oldest = self.placed.remove(0);
            (oldest.x.min(stage.right().saturating_sub(w + 2)), oldest.y.min(stage.bottom().saturating_sub(h + 1)))
        });
        self.placed.push(Placed { index, x, y, rows, color: index, seed });
    }

    fn color(&self, i: usize) -> Color {
        self.palette.get(i % self.palette.len().max(1)).copied().unwrap_or(Color::White)
    }

    pub fn draw(&self, buf: &mut Buffer, pos_ms: u64) {
        let Some(lines) = self.lines() else { return };
        if self.style == KARAOKE {
            return self.draw_karaoke(buf, lines, pos_ms);
        }
        let stage = buf.area;
        let t = self.clock.elapsed().as_secs_f32();
        for p in &self.placed {
            let line = &lines[p.index];
            let next = lines.get(p.index + 1).map(|l| l.at_ms);
            let current = Some(p.index) == self.current;
            let age = pos_ms.saturating_sub(line.at_ms) as f32;
            // Time since the line was replaced (0 while it is current).
            let gone = if current { 0.0 } else { pos_ms.saturating_sub(next.unwrap_or(pos_ms)) as f32 };
            let span = next.map_or(2500, |n| n.saturating_sub(line.at_ms)).clamp(600, 5000) as f32;
            let n = p.chars().max(1);
            let color = self.color(p.color);
            let mut k = 0usize;
            for (r, row) in p.rows.iter().enumerate() {
                for (i, ch) in row.chars().enumerate() {
                    let (bx, by) = (f32::from(p.x) + i as f32, f32::from(p.y) + r as f32);
                    let glyph = Glyph { k, n, ch, bx, by, age, gone, span, current, seed: p.seed, t, stage };
                    if let Some((x, y, shown, k_bright)) = self.animate(&glyph) {
                        put(buf, x, y, shown, dim(color, k_bright), current && k_bright > 0.6);
                    }
                    k += 1;
                }
            }
        }
    }

    /// Where and how one character is drawn in the current style:
    /// (x, y, character, brightness) or nothing.
    fn animate(&self, g: &Glyph) -> Option<(f32, f32, char, f32)> {
        let past = (1.0 - g.gone / FADE_MS as f32).clamp(0.0, 1.0) * 0.55;
        let fading = if g.current { 1.0 } else { past };
        match self.style {
            FALL => {
                // Letters drop in one after the other, then fall off-screen.
                let delay = g.k as f32 * 28.0;
                if g.age < delay {
                    return None;
                }
                let landing = ease((g.age - delay) / 380.0);
                let mut y = g.by - (1.0 - landing) * (g.by + 2.0);
                let leave = g.gone - (g.n - g.k) as f32 * 18.0;
                let mut bright = if g.current { 1.0 } else { 0.7 };
                if leave > 0.0 {
                    let s = leave / 1000.0;
                    y += 30.0 * s * s;
                    bright = (0.7 - s * 0.3).max(0.2);
                }
                Some((g.bx, y, g.ch, bright))
            }
            DECODE => {
                // Random glyphs "decrypting" into letters, scrambling back out.
                let decoded_at = g.k as f32 * (g.span * 0.5 / g.n as f32).clamp(15.0, 60.0);
                let flicker = (g.t * 18.0) as u64;
                let noise = NOISE[(hash(g.seed ^ g.k as u64, flicker) * NOISE.len() as f32) as usize % NOISE.len()];
                if g.age < decoded_at {
                    return (g.age + 250.0 > decoded_at).then_some((g.bx, g.by, noise, 0.45));
                }
                let scramble_at = (g.n - g.k) as f32 * 30.0;
                if g.gone > scramble_at {
                    let k = (1.0 - (g.gone - scramble_at) / 900.0).max(0.0) * 0.5;
                    return (k > 0.03).then_some((g.bx, g.by, noise, k));
                }
                Some((g.bx, g.by, g.ch, fading.max(if g.current { 1.0 } else { 0.5 })))
            }
            WAVE => {
                // Slides in from the right on a wave, leaves to the left.
                let width = f32::from(g.stage.width);
                let enter = ease((g.age - g.k as f32 * 6.0) / 750.0);
                let leave = ease(g.gone / 1300.0);
                let x = g.bx + (1.0 - enter) * width - leave * (g.bx + 10.0 + width * 0.3);
                let amp = if g.current { 1.0 } else { 0.4 };
                let y = g.by + (g.k as f32 * 0.45 - g.t * 5.0).sin() * amp;
                Some((x, y, g.ch, if g.current { 1.0 } else { (0.6 - leave * 0.5).max(0.1) }))
            }
            EXPLODE => {
                // Converge from scattered points, burst outwards on exit.
                let (rx, ry) = (hash(g.seed, g.k as u64 * 2), hash(g.seed, g.k as u64 * 2 + 1));
                let from = (f32::from(g.stage.x) + rx * f32::from(g.stage.width), f32::from(g.stage.y) + ry * f32::from(g.stage.height));
                let gather = ease(g.age / 650.0);
                let (mut x, mut y) = (from.0 + (g.bx - from.0) * gather, from.1 + (g.by - from.1) * gather);
                let mut ch = g.ch;
                let mut bright = if g.current { 0.4 + 0.6 * gather } else { 0.6 };
                if g.gone > 0.0 {
                    let a = hash(g.seed ^ 0x5eed, g.k as u64) * std::f32::consts::TAU;
                    let burst = ease(g.gone / 1500.0) * 28.0;
                    x += a.cos() * burst;
                    y += a.sin() * burst * 0.5;
                    bright = (0.7 - g.gone / 1600.0).max(0.0);
                    if g.gone > 500.0 {
                        ch = if g.gone > 1000.0 { '·' } else { '*' };
                    }
                }
                (bright > 0.03).then_some((x, y, ch, bright))
            }
            NEON => {
                // Each letter flickers on like a neon sign, then off.
                let frame = (g.t * 22.0) as u64;
                let on_chance = if g.gone > 0.0 { 1.0 - g.gone / 1700.0 } else { (g.age / 900.0).min(1.0) * 1.05 };
                let lit = hash(g.seed ^ g.k as u64, frame) < on_chance;
                // Even lit, a rare blink keeps it alive.
                let blink = g.current && hash(g.seed ^ 0xb11, frame ^ g.k as u64) < 0.02;
                if !lit || blink {
                    return (on_chance > 0.0).then_some((g.bx, g.by, g.ch, 0.12));
                }
                Some((g.bx, g.by, g.ch, if g.current { 1.0 } else { 0.55 }))
            }
            BUBBLES => {
                // Born at the bottom, rising slowly, faster once replaced.
                let rise = g.age / 1000.0 * 0.9 + (g.gone / 1000.0).powf(1.5) * 1.6;
                let y = g.by - rise;
                let wobble = (g.t * 2.0 + g.k as f32 * 0.6 + hash(g.seed, 7) * 6.0).sin() * 0.35;
                let top_fade = ((y - f32::from(g.stage.y)) / 4.0).clamp(0.0, 1.0);
                let bright = if g.current { 1.0 } else { (0.65 - g.gone / 9000.0).max(0.1) } * top_fade;
                (bright > 0.03).then_some((g.bx + wobble, y, g.ch, bright))
            }
            _ => {
                // Scatter: typed out, then fading.
                if g.current {
                    let typed = ((g.age / (g.span * 0.6)).min(1.0) * g.n as f32).ceil() as usize;
                    return (g.k < typed.max(1)).then_some((g.bx, g.by, g.ch, 1.0));
                }
                (fading > 0.0).then_some((g.bx, g.by, g.ch, fading.max(0.08)))
            }
        }
    }

    /// Karaoke: a centred column, the current line filling with colour as it
    /// is sung, the next one announced below, older ones above.
    fn draw_karaoke(&self, buf: &mut Buffer, lines: &[Line], pos_ms: u64) {
        let area = buf.area;
        let Some(cur) = self.current else { return };
        let mid = area.y + area.height / 2;
        let line = &lines[cur];
        let next = lines.get(cur + 1).map_or(line.at_ms + 3000, |l| l.at_ms);
        let sung = (pos_ms.saturating_sub(line.at_ms) as f32 / next.saturating_sub(line.at_ms).max(1) as f32).clamp(0.0, 1.0);
        let accent = self.color(0);
        let quiet = Color::Rgb(150, 150, 150);
        for offset in -4i32..=2 {
            let Some(i) = cur.checked_add_signed(offset as isize).filter(|i| *i < lines.len()) else { continue };
            let text = if lines[i].text.is_empty() { "♪" } else { lines[i].text.as_str() };
            let y = mid as i32 + offset * 2;
            if y < i32::from(area.y) || y >= i32::from(area.bottom()) {
                continue;
            }
            let chars: Vec<char> = text.chars().take(usize::from(area.width).saturating_sub(4)).collect();
            let x0 = area.x + area.width.saturating_sub(chars.len() as u16) / 2;
            let filled = (sung * chars.len() as f32).round() as usize;
            for (k, ch) in chars.iter().enumerate() {
                let (fg, bold) = match offset {
                    0 if k < filled => (accent, true),
                    0 => (Color::Rgb(220, 220, 220), true),
                    o if o > 0 => (dim(quiet, 0.55), false),
                    o => (dim(quiet, 0.7 + o as f32 * 0.14), false),
                };
                put(buf, f32::from(x0) + k as f32, y as f32, *ch, fg, bold);
            }
        }
    }
}

/// One character to animate.
struct Glyph {
    /// Index in the line, line length.
    k: usize,
    n: usize,
    ch: char,
    /// Resting position.
    bx: f32,
    by: f32,
    /// ms since the line started / since it was replaced (0 if current).
    age: f32,
    gone: f32,
    /// Duration of the line (ms, clamped).
    span: f32,
    current: bool,
    seed: u64,
    /// Seconds, wall clock.
    t: f32,
    stage: Rect,
}

/// Smooth start and stop, 0..=1.
fn ease(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Deterministic pseudo-random number in 0..1 (splitmix64).
fn hash(a: u64, b: u64) -> f32 {
    let mut z = a.wrapping_add(b.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}

fn put(buf: &mut Buffer, x: f32, y: f32, ch: char, fg: Color, bold: bool) {
    if x < 0.0 || y < 0.0 {
        return;
    }
    let (x, y) = (x.round() as u16, y.round() as u16);
    if !buf.area.contains((x, y).into()) {
        return;
    }
    let cell = &mut buf[(x, y)];
    cell.set_char(ch).set_fg(fg);
    if bold {
        cell.modifier.insert(Modifier::BOLD);
    }
}

impl Default for LyricsScreen {
    fn default() -> Self {
        Self::new()
    }
}

/// Word wrap to `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = vec![String::new()];
    for word in text.split_whitespace() {
        let row = rows.last_mut().expect("never empty");
        if !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
            rows.push(word.to_owned());
        } else {
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
    }
    rows
}

fn dim(c: Color, k: f32) -> Color {
    match c {
        Color::Rgb(r, g, b) => {
            let s = |v: u8| (f32::from(v) * k).round() as u8;
            Color::Rgb(s(r), s(g), s(b))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LRC: &str = "[ar:Daft Punk]\n[00:01.50]One more time\n[00:05.00][00:20.00]We're gonna celebrate\n[00:09.25]\n[00:12.00]Oh yeah, all right";

    #[test]
    fn parses_lrc_with_metadata_repeats_and_blank_lines() {
        let lines = parse_lrc(LRC);
        let stamps: Vec<u64> = lines.iter().map(|l| l.at_ms).collect();
        assert_eq!(stamps, [1500, 5000, 9250, 12000, 20000]);
        assert_eq!(lines[1].text, "We're gonna celebrate");
        assert_eq!(lines[2].text, "", "instrumental break");
        assert_eq!(LyricsScreen::index_at(&lines, 0), None);
        assert_eq!(LyricsScreen::index_at(&lines, 5100), Some(1));
        assert_eq!(LyricsScreen::index_at(&lines, 4900), Some(1), "small lead");
    }

    #[test]
    fn picks_the_closest_synced_result() {
        let r = |d: f64, synced: bool| Record {
            duration: d,
            synced_lyrics: synced.then(|| "[00:01.00]x".to_owned()),
            plain_lyrics: Some("x".into()),
            ..Default::default()
        };
        let best = best_match(vec![r(200.0, true), r(321.0, false), r(322.0, true), r(400.0, true)], 320_000).unwrap();
        assert_eq!(best.duration, 322.0, "synced preferred over a closer unsynced one");
        assert!(best_match(vec![r(250.0, true)], 320_000).is_none(), "too far");
    }

    #[test]
    fn records_become_lyrics() {
        let synced = Record { synced_lyrics: Some(LRC.into()), ..Default::default() };
        assert!(matches!(Lyrics::from_record(Some(synced), 30_000), Lyrics::Timed { approximate: false, .. }));
        let plain = Record { plain_lyrics: Some("a\n\nb\nc".into()), ..Default::default() };
        let Lyrics::Timed { lines, approximate: true } = Lyrics::from_record(Some(plain), 40_000) else { panic!() };
        assert_eq!(lines.len(), 3);
        assert!(lines.windows(2).all(|w| w[0].at_ms < w[1].at_ms));
        assert_eq!(Lyrics::from_record(Some(Record { instrumental: true, ..Default::default() }), 1), Lyrics::Instrumental);
        assert_eq!(Lyrics::from_record(None, 1), Lyrics::NotFound);
    }

    #[test]
    fn lines_appear_without_overlap_type_out_and_fade() {
        let mut s = LyricsScreen::new();
        s.reset(Some("k".into()));
        s.set_result("k", Ok(Lyrics::from_record(Some(Record { synced_lyrics: Some(LRC.into()), ..Default::default() }), 30_000)));
        s.palette = vec![Color::Rgb(250, 100, 100)];
        let stage = Rect::new(0, 0, 100, 30);
        for pos in (0..14_000).step_by(100) {
            s.update(pos, stage);
        }
        // At 14 s: the first line (sung 1.5–5 s) has faded out (7 s after
        // the next one started); lines 2, 3 (♪) and 4 remain.
        let shown: Vec<usize> = s.placed.iter().map(|p| p.index).collect();
        assert_eq!(shown, [1, 2, 3]);
        for (i, a) in s.placed.iter().enumerate() {
            for b in &s.placed[i + 1..] {
                let overlap = a.y < b.y + b.rows.len() as u16 && b.y < a.y + a.rows.len() as u16
                    && a.x < b.x + b.rows[0].chars().count() as u16 && b.x < a.x + a.rows[0].chars().count() as u16;
                assert!(!overlap, "lines overlap");
            }
        }
        // The current line is typed out progressively.
        let mut buf = Buffer::empty(stage);
        s.draw(&mut buf, 12_100);
        let current = s.placed.iter().find(|p| p.index == 3).unwrap();
        let first: String = (0..17).map(|i| buf[(current.x + i, current.y)].symbol().to_owned()).collect();
        assert!(first.starts_with('O') && !first.contains("right"), "partially typed: {first:?}");
        // Old lines fade out after a while, and seeking back starts over.
        s.update(30_000, stage);
        assert!(s.placed.len() <= 2);
        s.update(2_000, stage);
        assert_eq!(s.placed.len(), 1);
    }

    /// Real LRCLIB request (needs Internet: `-- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn fetches_real_synced_lyrics() {
        let q = Query {
            key: "test:one-more-time".into(),
            title: "One More Time".into(),
            artist: "Daft Punk".into(),
            album: Some("Discovery".into()),
            duration_ms: 320_000,
        };
        if let Some(p) = cache_path(&q.key) {
            let _ = std::fs::remove_file(p);
        }
        let http = reqwest::Client::new();
        let started = std::time::Instant::now();
        let record = fetch(&http, &q).await.expect("request").expect("found");
        let first = started.elapsed();
        let Lyrics::Timed { lines, approximate } = Lyrics::from_record(Some(record), q.duration_ms) else { panic!("no lines") };
        println!("{} lignes synchronisées={} en {:?} ; ex. [{}] {}", lines.len(), !approximate, first, lines[3].at_ms, lines[3].text);
        assert!(!approximate && lines.len() > 10);
        let started = std::time::Instant::now();
        fetch(&http, &q).await.unwrap().unwrap();
        println!("depuis le cache en {:?}", started.elapsed());
        if let Some(p) = cache_path(&q.key) {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    #[ignore]
    fn preview_screen() {
        let lrc = "[00:01.00]One more time\n[00:04.00]We're gonna celebrate\n[00:07.00]Oh yeah, all right, don't stop the dancing\n[00:10.00]One more time\n[00:13.00]Mmm, you know I'm just feeling\n[00:16.00]Celebration tonight";
        for style in [FALL, EXPLODE, BUBBLES, KARAOKE] {
            let mut s = LyricsScreen::with_style(style);
            s.reset(Some("k".into()));
            s.set_result("k", Ok(Lyrics::from_record(Some(Record { synced_lyrics: Some(lrc.into()), ..Default::default() }), 30_000)));
            s.palette = vec![Color::Rgb(250, 100, 100)];
            let stage = Rect::new(0, 0, 90, 16);
            for pos in (0..13_400).step_by(50) {
                s.update(pos, stage);
            }
            let mut buf = Buffer::empty(stage);
            s.draw(&mut buf, 13_400);
            println!("===== {} (13,4 s) =====", STYLES[style]);
            for y in 0..stage.height {
                let line: String = (0..stage.width).map(|x| buf[(x, y)].symbol().to_owned()).collect();
                println!("|{line}|");
            }
        }
    }

    #[test]
    fn every_style_animates_without_panicking() {
        let lrc = "[00:01.00]One more time\n[00:04.00]We're gonna celebrate\n[00:07.00]\n[00:09.00]Oh yeah, all right, don't stop the dancing\n[00:12.00]One more time";
        for (style, name) in STYLES.iter().enumerate() {
            let mut s = LyricsScreen::with_style(style);
            s.reset(Some("k".into()));
            s.set_result("k", Ok(Lyrics::from_record(Some(Record { synced_lyrics: Some(lrc.into()), ..Default::default() }), 20_000)));
            s.palette = vec![Color::Rgb(250, 100, 100), Color::Rgb(100, 200, 250)];
            for (w, h) in [(100u16, 30u16), (24, 8)] {
                let stage = Rect::new(0, 0, w, h);
                let mut drawn = 0;
                for pos in (0..30_000).step_by(50) {
                    s.update(pos, stage);
                    let mut buf = Buffer::empty(stage);
                    s.draw(&mut buf, pos);
                    drawn = drawn.max(buf.content().iter().filter(|c| c.symbol() != " ").count());
                }
                assert!(drawn > 5, "style {name} draws something at {w}x{h}");
            }
            // Long after the last line, everything except the last line is gone.
            assert!(s.placed.len() <= 1, "style {name} cleans up: {}", s.placed.len());
        }
    }

    #[test]
    fn karaoke_fills_the_current_line_in_the_middle() {
        let mut s = LyricsScreen::with_style(KARAOKE);
        s.reset(Some("k".into()));
        s.set_result("k", Ok(Lyrics::from_record(Some(Record { synced_lyrics: Some(LRC.into()), ..Default::default() }), 30_000)));
        s.palette = vec![Color::Rgb(250, 50, 50)];
        let stage = Rect::new(0, 0, 80, 21);
        s.update(6_000, stage);
        let mut buf = Buffer::empty(stage);
        s.draw(&mut buf, 6_000);
        let mid: String = (0..80).map(|x| buf[(x, 10)].symbol().to_owned()).collect();
        assert!(mid.contains("We're gonna celebrate"), "{mid:?}");
        // Partly sung: some letters in the accent colour, some not.
        let colors: Vec<Color> = (0..80).map(|x| buf[(x, 10)].fg).filter(|c| *c != Color::Reset).collect();
        assert!(colors.contains(&Color::Rgb(250, 50, 50)) && colors.iter().any(|c| *c != Color::Rgb(250, 50, 50)));
        let below: String = (0..80).map(|x| buf[(x, 12)].symbol().to_owned()).collect();
        assert!(below.trim().is_empty() || below.contains('♪'), "next line (blank = ♪) below: {below:?}");
    }

    #[test]
    fn style_cycles_and_restarts_the_layout() {
        let mut s = LyricsScreen::with_style(7);
        s.next_style(1);
        assert_eq!(s.style_name(), "Dispersé");
        s.next_style(-1);
        assert_eq!(s.style_name(), "Bulles");
        assert!(s.banner().is_some());
    }
}

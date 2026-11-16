//! Full-screen visualizer (`v`): the cover in the middle, an animation that
//! reacts to the sound around it, ten styles (`←/→`, `1`–`0`).

use std::f32::consts::TAU;
use std::time::Instant;

use image::imageops::FilterType;
use image::{DynamicImage, RgbaImage};
use rand::RngExt;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::audio::{Analysis, BANDS};

pub const STYLES: [&str; 10] =
    ["Barres", "Miroir", "Halo", "Pulsation", "Onde", "Particules", "Étoiles", "Pluie", "Glitch", "Aurore"];
const PULSE: usize = 3;
const GLITCH: usize = 8;
/// Styles that animate the cover itself (always drawn with characters).
pub fn animates_cover(style: usize) -> bool {
    style == PULSE || style == GLITCH
}

const MAX_PARTICLES: usize = 600;
const STARS: usize = 260;
const RAIN_GLYPHS: &[char] = &[
    'ﾊ', 'ﾐ', 'ﾋ', 'ｰ', 'ｳ', 'ｼ', 'ﾅ', 'ﾓ', 'ﾆ', 'ｻ', 'ﾜ', 'ﾂ', 'ｵ', 'ﾘ', 'ｱ', 'ﾎ', 'ﾃ', 'ﾏ', 'ｹ', 'ﾒ', 'ｴ', 'ｶ', 'ｷ', 'ﾑ', 'ﾕ', 'ﾗ',
    'ｾ', 'ﾈ', 'ｽ', 'ﾀ', 'ﾇ', 'ﾍ', '0', '1', '2', '3', '4', '5', '7', '8', '9', 'Z', ':', '.', '=', '*', '+', '<', '>',
];

/// How the cover is drawn in the visualizer (`c` cycles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoverLook {
    /// Coloured half blocks: two pixels per cell.
    #[default]
    Blocks,
    /// Coloured characters ` .:-=+*#%@`.
    Ascii,
    /// The same characters in the terminal's text colour: classic ASCII art.
    PureAscii,
    /// The real picture (Sixel, Kitty…), when the terminal supports it.
    Image,
}

impl CoverLook {
    pub fn next(self, images: bool) -> Self {
        match self {
            Self::Blocks => Self::Ascii,
            Self::Ascii => Self::PureAscii,
            Self::PureAscii if images => Self::Image,
            Self::PureAscii | Self::Image => Self::Blocks,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Blocks => "blocs",
            Self::Ascii => "ASCII couleur",
            Self::PureAscii => "ASCII pur",
            Self::Image => "image",
        }
    }
}

struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    /// Initial life, to fade proportionally.
    max_life: f32,
    color: usize,
}

struct Star {
    x: f32,
    y: f32,
    z: f32,
}

struct Drop {
    y: f32,
    speed: f32,
    len: usize,
    glyphs: Vec<char>,
}

/// What the visualizer needs from the room screen.
pub struct Track<'a> {
    pub title: &'a str,
    pub artists: &'a str,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub playing: bool,
}

pub struct Visualizer {
    pub style: usize,
    style_changed: Instant,
    pub look: CoverLook,
    /// Text colour of the terminal, for pure ASCII art.
    pub ink: Color,
    /// Very slow loudness envelope (aurora: no flashing).
    glow: f32,
    /// The sound could not be captured: simulated motion.
    pub simulated: bool,
    start: Instant,
    last: Instant,
    /// Duration of the last frame (for the physics of the effects).
    pub dt: f32,
    // Smoothed sound.
    bands: [f32; BANDS],
    level: f32,
    bass: f32,
    flash: f32,
    last_beats: u64,
    wave: Vec<f32>,
    // Effects.
    particles: Vec<Particle>,
    stars: Vec<Star>,
    drops: Vec<Drop>,
    ripples: Vec<f32>,
    glitch_rows: Vec<i16>,
    /// Particles owed to the continuous flow (fractional per frame).
    spawn_debt: f32,
    /// A beat happened: throw a burst at the next frame.
    burst: bool,
    // Cover.
    cover_key: Option<String>,
    small: Option<RgbaImage>,
    palette: Vec<Color>,
    /// Cover resized to the cell grid (2 pixels per cell, vertically).
    pixels: Option<((u16, u16), RgbaImage)>,
}

impl Visualizer {
    pub fn new(style: usize) -> Self {
        let now = Instant::now();
        Self {
            style: style % STYLES.len(),
            style_changed: now,
            look: CoverLook::Blocks,
            ink: Color::Reset,
            glow: 0.0,
            simulated: false,
            start: now,
            last: now,
            dt: 0.033,
            bands: [0.0; BANDS],
            level: 0.0,
            bass: 0.0,
            flash: 0.0,
            last_beats: 0,
            wave: vec![],
            particles: vec![],
            stars: vec![],
            drops: vec![],
            ripples: vec![],
            glitch_rows: vec![],
            spawn_debt: 0.0,
            burst: false,
            cover_key: None,
            small: None,
            palette: vec![],
            pixels: None,
        }
    }

    pub fn set_style(&mut self, style: usize) {
        self.style = style % STYLES.len();
        self.style_changed = Instant::now();
    }

    pub fn next_style(&mut self, delta: isize) {
        let n = STYLES.len() as isize;
        self.set_style(((self.style as isize + delta).rem_euclid(n)) as usize);
    }

    pub fn style_name(&self) -> &'static str {
        STYLES[self.style]
    }

    /// New sound data: smoothing (fast rise, slow fall) and beat flash.
    pub fn update(&mut self, a: &Analysis) -> f32 {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32().min(0.1);
        self.last = now;
        self.dt = dt;
        let fall = (-dt * 6.0).exp();
        for (s, &v) in self.bands.iter_mut().zip(&a.bands) {
            *s = v.max(*s * fall);
        }
        self.level = a.level.max(self.level * fall);
        // ~1.5 s time constant, beats ignored.
        self.glow += (a.level - self.glow) * (1.0 - (-dt / 1.5).exp());
        self.bass = a.bass.max(self.bass * fall);
        if a.beats != self.last_beats {
            self.last_beats = a.beats;
            self.flash = 1.0;
            self.on_beat();
        }
        self.flash *= (-dt * 5.0).exp();
        self.wave = a.wave.clone();
        dt
    }

    fn on_beat(&mut self) {
        self.ripples.push(0.0);
        self.burst = true;
        let mut rng = rand::rng();
        for r in self.glitch_rows.iter_mut() {
            if rng.random_bool(0.3) {
                *r = rng.random_range(-6..=6);
            }
        }
    }

    fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// Small copy of the cover and its palette (recomputed when it changes).
    pub fn set_cover(&mut self, key: Option<&str>, img: Option<&DynamicImage>, fallback: &[Color]) {
        if self.cover_key.as_deref() == key && (self.small.is_some() || img.is_none()) && !self.palette.is_empty() {
            return;
        }
        self.cover_key = key.map(str::to_owned);
        self.small = img.map(|i| i.resize_exact(128, 128, FilterType::Triangle).to_rgba8());
        self.pixels = None;
        self.palette = self.small.as_ref().map(palette).filter(|p| p.len() >= 3).unwrap_or_else(|| fallback.to_vec());
    }

    fn color(&self, i: usize) -> Color {
        self.palette.get(i % self.palette.len().max(1)).copied().unwrap_or(Color::White)
    }

    /// Colour along the palette, `x` in 0..=1.
    fn gradient(&self, x: f32) -> Color {
        let n = self.palette.len().max(1);
        let pos = x.clamp(0.0, 1.0) * (n - 1) as f32;
        let i = pos.floor() as usize;
        mix(self.color(i), self.color((i + 1).min(n - 1)), pos.fract())
    }

    // ---------------------------------------------------------- drawing

    pub fn draw(&mut self, f: &mut Frame, track: Option<&Track>, dt: f32, image: Option<&mut ratatui_image::protocol::StatefulProtocol>) {
        let area = f.area();
        if area.width < 20 || area.height < 8 {
            return;
        }
        let info_h = 2;
        let stage = Rect { height: area.height - info_h, ..area };
        let scale = if self.style == PULSE { 0.85 + 0.25 * self.bass + 0.05 * self.flash } else { 1.0 };
        let cover = cover_rect(stage, scale);
        let buf = f.buffer_mut();

        match self.style {
            0 => self.bars(buf, stage, cover),
            1 => self.mirror(buf, stage, cover),
            2 => self.halo(buf, stage, cover),
            3 => self.pulse_rings(buf, stage, cover, dt),
            4 => self.wave_lines(buf, stage, cover),
            5 => self.sparks(buf, stage, cover, dt),
            6 => self.starfield(buf, stage, cover, dt),
            7 => self.rain(buf, stage, cover, dt),
            8 => {}
            _ => self.aurora(buf, stage, cover),
        }

        // The cover: real picture when asked and possible, else characters.
        let drew_image = self.look == CoverLook::Image && !animates_cover(self.style) && image.is_some();
        if drew_image {
            if let Some(protocol) = image {
                f.render_stateful_widget(ratatui_image::StatefulImage::default(), cover, protocol);
            }
        } else {
            self.draw_cover(f.buffer_mut(), cover);
        }
        if self.style == PULSE {
            frame(f.buffer_mut(), cover, mix(Color::Rgb(40, 40, 40), self.color(0), 0.3 + 0.7 * self.flash));
        }
        self.info(f.buffer_mut(), area, track);
    }

    fn draw_cover(&mut self, buf: &mut Buffer, rect: Rect) {
        let Some(small) = &self.small else {
            let x = rect.x + rect.width / 2;
            put(buf, x, rect.y + rect.height / 2, '♪', self.color(0), None);
            return;
        };
        let size = (rect.width, rect.height);
        if self.pixels.as_ref().is_none_or(|(s, _)| *s != size) {
            let img = DynamicImage::ImageRgba8(small.clone())
                .resize_exact(u32::from(rect.width), u32::from(rect.height) * 2, FilterType::Triangle)
                .to_rgba8();
            self.pixels = Some((size, img));
        }
        let glitch = self.style == GLITCH;
        if glitch && self.glitch_rows.len() != usize::from(rect.height) {
            self.glitch_rows = vec![0; usize::from(rect.height)];
        }
        // Glitch: rows shifted on beats and colour channels split.
        let split = if glitch { (self.bass * 4.0 + self.flash * 3.0) as i32 } else { 0 };
        let look = if self.look == CoverLook::Image { CoverLook::Blocks } else { self.look };
        let Some((_, img)) = &self.pixels else { return };
        let w = i32::from(rect.width);
        for dy in 0..rect.height {
            let offset = if glitch { i32::from(self.glitch_rows[usize::from(dy)]) } else { 0 };
            for dx in 0..rect.width {
                let x = i32::from(dx) - offset;
                // Pixel at column x (wrapping), mixing channels from shifted
                // columns when glitching.
                let px = |row: u32| {
                    let at = |shift: i32| *img.get_pixel((x + shift).rem_euclid(w) as u32, row);
                    let (r, g, b) = (at(-split), at(0), at(split));
                    image::Rgba([r[0], g[1], b[2], 255])
                };
                let (top, bottom) = (px(u32::from(dy) * 2), px(u32::from(dy) * 2 + 1));
                let (xx, yy) = (rect.x + dx, rect.y + dy);
                match look {
                    CoverLook::Blocks | CoverLook::Image => {
                        put(buf, xx, yy, '▀', rgba(top), Some(rgba(bottom)));
                    }
                    CoverLook::Ascii | CoverLook::PureAscii => {
                        let avg = image::Rgba([
                            ((u16::from(top[0]) + u16::from(bottom[0])) / 2) as u8,
                            ((u16::from(top[1]) + u16::from(bottom[1])) / 2) as u8,
                            ((u16::from(top[2]) + u16::from(bottom[2])) / 2) as u8,
                            255,
                        ]);
                        let ch = crate::cover::ramp_char(avg);
                        let fg = if look == CoverLook::Ascii { rgba(avg) } else { self.ink };
                        put(buf, xx, yy, ch, fg, None);
                    }
                }
            }
        }
        if glitch {
            // Offsets fade back; noise bars while it is loud.
            for r in self.glitch_rows.iter_mut() {
                *r = (f32::from(*r) * 0.85) as i16;
            }
            let mut rng = rand::rng();
            for _ in 0..(self.flash * 6.0) as usize {
                let y = rect.y + rng.random_range(0..rect.height);
                let x0 = rect.x + rng.random_range(0..rect.width);
                let len = rng.random_range(2..10).min(rect.right() - x0);
                let (ch, color) = match look {
                    CoverLook::PureAscii => ('#', self.ink),
                    _ => ('▓', self.color(rng.random_range(0..4))),
                };
                for x in x0..x0 + len {
                    put(buf, x, y, ch, color, None);
                }
            }
        }
    }

    // 1. Spectrum under the cover, faint reflection above.
    fn bars(&self, buf: &mut Buffer, stage: Rect, cover: Rect) {
        let floor = stage.bottom();
        let height = floor.saturating_sub(cover.bottom() + 1);
        let count = (stage.width / 2).max(1);
        for i in 0..count {
            let v = self.band_at(i as f32 / count as f32);
            let x = stage.x + i * 2;
            let color = self.gradient(i as f32 / count as f32);
            column_up(buf, x, floor, f32::from(height) * v, color);
            // Reflection above the cover, dimmer.
            let top_h = f32::from(cover.y.saturating_sub(stage.y)) * v * 0.5;
            column_down(buf, x, stage.y, top_h, dim(color, 0.45));
        }
    }

    // 2. Horizontal bars growing out of both sides of the cover.
    fn mirror(&self, buf: &mut Buffer, stage: Rect, cover: Rect) {
        let left_room = f32::from(cover.x.saturating_sub(stage.x + 1));
        let right_room = f32::from(stage.right().saturating_sub(cover.right() + 1));
        for y in stage.y..stage.bottom() {
            // Low frequencies in the middle rows, highs at the top/bottom.
            let mid = f32::from(stage.y) + f32::from(stage.height) / 2.0;
            let d = ((f32::from(y) - mid).abs() / (f32::from(stage.height) / 2.0)).min(1.0);
            let v = self.band_at(d);
            let color = self.gradient(d);
            let len_l = left_room * v;
            for k in 0..len_l as u16 {
                put(buf, cover.x - 2 - k, y, '█', fade(color, k as f32 / left_room.max(1.0)), None);
            }
            let len_r = right_room * v;
            for k in 0..len_r as u16 {
                put(buf, cover.right() + 1 + k, y, '█', fade(color, k as f32 / right_room.max(1.0)), None);
            }
        }
    }

    // 3. Spectrum in a circle around the cover.
    fn halo(&self, buf: &mut Buffer, stage: Rect, cover: Rect) {
        let (cx, cy) = center(cover);
        let r0 = f32::from(cover.width) / 2.0 + 2.0;
        let max_len = (f32::from(stage.width) / 2.0 - r0).min(f32::from(stage.height) - r0 / 2.0).max(4.0);
        let rays = 96;
        let spin = self.t() * 0.15;
        for i in 0..rays {
            let a = i as f32 / rays as f32 * TAU + spin;
            // Mirror the spectrum on both halves.
            let pos = (i as f32 / rays as f32 * 2.0 - 1.0).abs();
            let v = self.band_at(1.0 - pos);
            let len = 1.0 + v * max_len;
            let color = self.gradient(pos);
            let mut d = 0.0;
            while d < len {
                let r = r0 + d;
                let x = cx + a.cos() * r;
                let y = cy + a.sin() * r / 2.0;
                let ch = if d > len - 1.0 { '•' } else { '·' };
                put_f(buf, stage, cover, x, y, ch, fade(color, d / (max_len + 1.0) * 0.7));
                d += 0.7;
            }
        }
        if self.flash > 0.2 {
            ring(buf, stage, cover, (cx, cy, r0 - 0.5), '∙', dim(self.color(0), self.flash));
        }
    }

    // 4. Rings sent out on beats (the cover itself pulses).
    fn pulse_rings(&mut self, buf: &mut Buffer, stage: Rect, cover: Rect, dt: f32) {
        let (cx, cy) = center(cover);
        let max_r = f32::from(stage.width.max(stage.height * 2)) / 2.0;
        for r in self.ripples.iter_mut() {
            *r += dt * (20.0 + 40.0 * self.level);
        }
        self.ripples.retain(|r| *r < max_r);
        let start = f32::from(cover.width) / 2.0 + 2.0;
        for (i, r) in self.ripples.iter().enumerate() {
            let radius = start + r;
            let life = 1.0 - r / max_r;
            ring(buf, stage, cover, (cx, cy, radius), '·', dim(self.color(i), life));
        }
    }

    // 5. Oscilloscope above and below the cover.
    fn wave_lines(&self, buf: &mut Buffer, stage: Rect, cover: Rect) {
        if self.wave.is_empty() {
            return;
        }
        let bands = [(stage.y, cover.y.saturating_sub(1)), (cover.bottom() + 1, stage.bottom())];
        for (k, (top, bottom)) in bands.into_iter().enumerate() {
            if bottom <= top + 1 {
                continue;
            }
            let mid = f32::from(top + bottom) / 2.0;
            let amp = f32::from(bottom - top) / 2.0 * (0.4 + 0.6 * self.level);
            let color = self.color(k);
            let mut prev: Option<(u16, f32)> = None;
            for x in stage.x..stage.right() {
                let i = (usize::from(x - stage.x) * self.wave.len()) / usize::from(stage.width);
                let s = self.wave[i.min(self.wave.len() - 1)] * if k == 1 { -1.0 } else { 1.0 };
                let y = mid - s * amp;
                // Join consecutive points vertically for a continuous line.
                let (from, to) = match prev {
                    Some((_, py)) => (py.min(y), py.max(y)),
                    None => (y, y),
                };
                let mut yy = from;
                while yy <= to + 0.01 {
                    let row = yy.round() as u16;
                    if row >= top && row < bottom {
                        put(buf, x, row, '•', color, None);
                    }
                    yy += 1.0;
                }
                prev = Some((x, y));
            }
        }
    }

    // 6. Sparks: a steady flow out of the cover, a burst of colours on
    // every beat, each spark leaving a short trail.
    fn sparks(&mut self, buf: &mut Buffer, stage: Rect, cover: Rect, dt: f32) {
        let mut rng = rand::rng();
        let (cx, cy) = center(cover);
        let (hw, hh) = (f32::from(cover.width) / 2.0, f32::from(cover.height) / 2.0);
        // Continuous flow, kept as a fractional debt so low levels still
        // emit (a few per second when quiet, ~150/s when loud).
        self.spawn_debt += (6.0 + 150.0 * self.level) * dt;
        let mut spawn = self.spawn_debt as usize;
        self.spawn_debt -= spawn as f32;
        let burst = std::mem::take(&mut self.burst);
        if burst {
            spawn += 70;
        }
        for k in 0..spawn {
            if self.particles.len() >= MAX_PARTICLES {
                break;
            }
            // Leave from a point of the cover's edge, outwards.
            let a = rng.random_range(0.0..TAU);
            let from_burst = burst && k >= spawn - 70;
            let speed = if from_burst { rng.random_range(25.0..60.0) } else { rng.random_range(8.0..30.0) * (0.5 + self.level) };
            let life = if from_burst { rng.random_range(0.8..1.8) } else { rng.random_range(0.6..1.4) };
            self.particles.push(Particle {
                x: cx + a.cos() * hw,
                y: cy + a.sin() * hh,
                vx: a.cos() * speed,
                vy: a.sin() * speed / 2.0,
                life,
                max_life: life,
                color: rng.random_range(0..5),
            });
        }
        for p in self.particles.iter_mut() {
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            let drag = (-dt * 1.2).exp();
            p.vx *= drag;
            p.vy = p.vy * drag + 6.0 * dt; // a little gravity
            p.life -= dt;
        }
        self.particles.retain(|p| p.life > 0.0);
        for p in &self.particles {
            let k = (p.life / p.max_life).clamp(0.0, 1.0);
            let color = self.color(p.color);
            // Trail: two dimmer points behind the spark.
            for (step, ch) in [(0.06, '·'), (0.03, '∙')] {
                put_f(buf, stage, cover, p.x - p.vx * step, p.y - p.vy * step, ch, dim(color, k * 0.45));
            }
            let head = match k {
                k if k > 0.75 => '✦',
                k if k > 0.5 => '✧',
                k if k > 0.25 => '•',
                _ => '·',
            };
            let bright = mix(Color::Rgb(255, 255, 255), color, 1.0 - k * 0.4);
            put_f(buf, stage, cover, p.x, p.y, head, dim(bright, 0.3 + 0.7 * k));
        }
    }

    // 7. Warp-speed stars, faster when it is loud.
    fn starfield(&mut self, buf: &mut Buffer, stage: Rect, cover: Rect, dt: f32) {
        let mut rng = rand::rng();
        while self.stars.len() < STARS {
            self.stars.push(Star { x: rng.random_range(-1.0..1.0), y: rng.random_range(-1.0..1.0), z: rng.random_range(0.05..1.0) });
        }
        let speed = 0.12 + 1.1 * self.level + 0.8 * self.flash;
        let (cx, cy) = (f32::from(stage.x) + f32::from(stage.width) / 2.0, f32::from(stage.y) + f32::from(stage.height) / 2.0);
        for (i, s) in self.stars.iter_mut().enumerate() {
            s.z -= speed * dt;
            if s.z <= 0.03 {
                *s = Star { x: rng.random_range(-1.0..1.0), y: rng.random_range(-1.0..1.0), z: 1.0 };
            }
            let project = |z: f32| {
                (cx + s.x / z * f32::from(stage.width) * 0.25, cy + s.y / z * f32::from(stage.height) * 0.25)
            };
            let (x, y) = project(s.z);
            // Hyperspace streak: from where the star was a moment ago to
            // where it is, longer when faster and closer.
            let (tx, ty) = project((s.z + speed * 0.12).min(1.0));
            let color = mix(Color::Rgb(255, 255, 255), self.palette.get(i % 5).copied().unwrap_or(Color::White), 0.35);
            let brightness = (1.15 - s.z).clamp(0.15, 1.0);
            let steps = ((x - tx).abs().max((y - ty).abs() * 2.0)).ceil().max(1.0) as usize;
            for k in 1..steps {
                let f = k as f32 / steps as f32;
                let ch = if (x - tx).abs() > (y - ty).abs() * 2.0 { '─' } else { '·' };
                put_f(buf, stage, cover, tx + (x - tx) * f, ty + (y - ty) * f, ch, dim(color, brightness * f * 0.6));
            }
            let head = if s.z < 0.3 { '+' } else { '·' };
            put_f(buf, stage, cover, x, y, head, dim(color, brightness));
        }
    }

    // 8. Falling glyph columns.
    fn rain(&mut self, buf: &mut Buffer, stage: Rect, cover: Rect, dt: f32) {
        let mut rng = rand::rng();
        let width = usize::from(stage.width);
        if self.drops.len() != width {
            self.drops = (0..width)
                .map(|_| Drop {
                    y: rng.random_range(-40.0..0.0),
                    speed: rng.random_range(6.0..20.0),
                    len: rng.random_range(6..24),
                    glyphs: (0..32).map(|_| RAIN_GLYPHS[rng.random_range(0..RAIN_GLYPHS.len())]).collect(),
                })
                .collect();
        }
        let boost = 0.35 + 1.6 * self.level + self.flash;
        let head = Color::Rgb(230, 255, 230);
        for (col, d) in self.drops.iter_mut().enumerate() {
            d.y += d.speed * boost * dt;
            if d.y - d.len as f32 > f32::from(stage.height) {
                d.y = rng.random_range(-20.0..0.0);
                d.speed = rng.random_range(6.0..20.0);
            }
            if rng.random_bool(0.05) {
                let i = rng.random_range(0..d.glyphs.len());
                d.glyphs[i] = RAIN_GLYPHS[rng.random_range(0..RAIN_GLYPHS.len())];
            }
            let x = stage.x + col as u16;
            let tail = self.palette.get(col % 3).copied().unwrap_or(Color::Green);
            for k in 0..d.len {
                let y = d.y - k as f32;
                if y < 0.0 || y >= f32::from(stage.height) {
                    continue;
                }
                let color = if k == 0 { head } else { fade(tail, k as f32 / d.len as f32) };
                let ch = d.glyphs[(k + y as usize) % d.glyphs.len()];
                let (xx, yy) = (x, stage.y + y as u16);
                if !cover.contains((xx, yy).into()) {
                    put(buf, xx, yy, ch, color, None);
                }
            }
        }
    }

    // 10. Northern lights: slow curtains of light hanging from the top of
    // the screen, drawn with shade blocks so they read as glowing bands,
    // longer and brighter with the overall loudness (smoothed over ~1.5 s,
    // beats ignored: no flashing).
    fn aurora(&self, buf: &mut Buffer, stage: Rect, cover: Rect) {
        let t = self.t();
        let sky = f32::from(stage.height);
        let energy = 0.6 + 0.4 * self.glow.clamp(0.0, 1.0);
        let hues = [Color::Rgb(70, 255, 160), Color::Rgb(60, 200, 255), Color::Rgb(200, 110, 255)];
        for (layer, hue) in hues.into_iter().enumerate() {
            let lf = layer as f32;
            // Vivid hues, nudged towards the cover's colours.
            let color = mix(hue, self.color(layer), 0.3);
            for x in stage.x..stage.right() {
                let fx = f32::from(x);
                // Folds of light along the curtain: bright bands and gaps.
                let fold = 0.5 + 0.5 * (fx * 0.13 + t * 0.45 + lf * 0.9).sin() * (fx * 0.05 - t * 0.2 + lf).cos();
                if fold < 0.15 {
                    continue;
                }
                let top = f32::from(stage.y)
                    + sky * (0.04 + 0.08 * lf)
                    + (fx * 0.03 + t * 0.2 + lf * 1.7).sin() * sky * 0.07
                    + (fx * 0.009 - t * 0.09 + lf).sin() * sky * 0.05;
                let len = sky * (0.3 + 0.3 * energy) * (0.55 + 0.45 * (fx * 0.04 + t * 0.3 + lf * 2.3).sin().abs());
                for y in stage.y..stage.bottom() {
                    let d = (f32::from(y) - top) / len;
                    if !(0.0..=1.0).contains(&d) || cover.contains((x, y).into()) {
                        continue;
                    }
                    // Brightest near the top edge, fading downwards.
                    let k = (fold * (1.0 - d).powf(1.2) * energy).min(1.0);
                    if k < 0.08 {
                        continue;
                    }
                    let ch = match k {
                        k if k > 0.7 => '█',
                        k if k > 0.45 => '▓',
                        k if k > 0.25 => '▒',
                        _ => '░',
                    };
                    put(buf, x, y, ch, dim(color, 0.45 + 0.55 * k), None);
                }
            }
        }
    }

    /// Bottom lines: progress, track, style and keys.
    fn info(&self, buf: &mut Buffer, area: Rect, track: Option<&Track>) {
        let y = area.bottom() - 1;
        let bar_y = area.bottom() - 2;
        if let Some(t) = track {
            let frac = if t.duration_ms == 0 { 0.0 } else { t.position_ms as f32 / t.duration_ms as f32 };
            let done = (frac.clamp(0.0, 1.0) * f32::from(area.width)) as u16;
            for x in area.x..area.right() {
                let (ch, color) = if x - area.x < done { ('━', self.color(0)) } else { ('─', Color::Rgb(60, 60, 60)) };
                put(buf, x, bar_y, ch, color, None);
            }
        }
        let text = match track {
            Some(t) => format!(
                " {} {} — {}   {} / {}   ",
                if t.playing { "▶" } else { "⏸" },
                t.title,
                t.artists,
                fmt_ms(t.position_ms),
                fmt_ms(t.duration_ms)
            ),
            None => " Rien en lecture   ".into(),
        };
        let sim = if self.simulated { "   (son système indisponible : animation simulée)" } else { "" };
        let keys = format!(
            "{}/10 {}   ←→ style   c pochette : {}   y paroles   Échap quitter{sim} ",
            self.style + 1,
            self.style_name(),
            self.look.label()
        );
        write(buf, area.x, y, &text, Color::Rgb(230, 230, 230), area.width);
        let kx = area.right().saturating_sub(keys.chars().count() as u16);
        if kx > area.x + text.chars().count() as u16 {
            write(buf, kx, y, &keys, Color::Rgb(140, 140, 140), area.width);
        }
        // Style name shown big for a moment after a change.
        let shown = self.style_changed.elapsed().as_secs_f32();
        if shown < 1.5 {
            let name = format!("  ◆ {} ◆  ", self.style_name());
            let x = area.x + area.width.saturating_sub(name.chars().count() as u16) / 2;
            write(buf, x, area.y + 1, &name, dim(Color::Rgb(255, 255, 255), 1.0 - shown / 1.5), area.width);
        }
    }

    /// Band value at position `x` (0 = lowest, 1 = highest), interpolated.
    fn band_at(&self, x: f32) -> f32 {
        let pos = x.clamp(0.0, 1.0) * (BANDS - 1) as f32;
        let i = pos.floor() as usize;
        let j = (i + 1).min(BANDS - 1);
        self.bands[i] * (1.0 - pos.fract()) + self.bands[j] * pos.fract()
    }
}

// ------------------------------------------------------------ helpers

/// Square cover in the middle of the stage (cells are about twice as tall
/// as wide).
pub fn cover_rect(stage: Rect, scale: f32) -> Rect {
    let base = (f32::from(stage.width) * 0.42).min(f32::from(stage.height) * 2.0 * 0.62);
    let w = ((base * scale).round() as u16).clamp(4, stage.width) & !1;
    let h = (w / 2).min(stage.height);
    Rect { x: stage.x + (stage.width - w) / 2, y: stage.y + (stage.height - h) / 2, width: w, height: h }
}

fn center(r: Rect) -> (f32, f32) {
    (f32::from(r.x) + f32::from(r.width) / 2.0, f32::from(r.y) + f32::from(r.height) / 2.0)
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, fg: Color, bg: Option<Color>) {
    if !buf.area.contains((x, y).into()) {
        return;
    }
    let cell = &mut buf[(x, y)];
    cell.set_char(ch).set_fg(fg);
    if let Some(bg) = bg {
        cell.set_bg(bg);
    }
}

/// Plots at float coordinates inside `stage`, behind the cover.
fn put_f(buf: &mut Buffer, stage: Rect, cover: Rect, x: f32, y: f32, ch: char, color: Color) {
    if x < f32::from(stage.x) || y < f32::from(stage.y) {
        return;
    }
    let (xi, yi) = (x as u16, y as u16);
    if xi < stage.right() && yi < stage.bottom() && !cover.contains((xi, yi).into()) {
        put(buf, xi, yi, ch, color, None);
    }
}

/// Circle of centre (cx, cy) and radius r (cells are twice as tall).
fn ring(buf: &mut Buffer, stage: Rect, cover: Rect, (cx, cy, r): (f32, f32, f32), ch: char, color: Color) {
    let steps = (r * 8.0).max(24.0) as usize;
    for i in 0..steps {
        let a = i as f32 / steps as f32 * TAU;
        put_f(buf, stage, cover, cx + a.cos() * r, cy + a.sin() * r / 2.0, ch, color);
    }
}

fn frame(buf: &mut Buffer, r: Rect, color: Color) {
    let (x0, y0) = (r.x.saturating_sub(1), r.y.saturating_sub(1));
    let (x1, y1) = (r.right(), r.bottom());
    for x in x0..=x1 {
        put(buf, x, y0, '─', color, None);
        put(buf, x, y1, '─', color, None);
    }
    for y in y0..=y1 {
        put(buf, x0, y, '│', color, None);
        put(buf, x1, y, '│', color, None);
    }
    for (x, y, ch) in [(x0, y0, '╭'), (x1, y0, '╮'), (x0, y1, '╰'), (x1, y1, '╯')] {
        put(buf, x, y, ch, color, None);
    }
}

const EIGHTHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Vertical bar of `height` cells (fractional) standing on `floor`.
fn column_up(buf: &mut Buffer, x: u16, floor: u16, height: f32, color: Color) {
    let full = height.floor() as u16;
    for k in 0..full {
        put(buf, x, floor.saturating_sub(1 + k), '█', color, None);
    }
    let rest = ((height - f32::from(full)) * 8.0) as usize;
    if rest > 0 {
        put(buf, x, floor.saturating_sub(1 + full), EIGHTHS[rest - 1], color, None);
    }
}

/// Bar hanging from `top` (reflection), whole cells only.
fn column_down(buf: &mut Buffer, x: u16, top: u16, height: f32, color: Color) {
    for k in 0..height.round() as u16 {
        put(buf, x, top + k, '▀', color, None);
    }
}

fn write(buf: &mut Buffer, x: u16, y: u16, text: &str, color: Color, max: u16) {
    for (i, ch) in text.chars().enumerate().take(usize::from(max)) {
        put(buf, x + i as u16, y, ch, color, None);
    }
}

fn rgb(c: Color) -> (f32, f32, f32) {
    match c {
        Color::Rgb(r, g, b) => (f32::from(r), f32::from(g), f32::from(b)),
        _ => (200.0, 200.0, 200.0),
    }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let ((r1, g1, b1), (r2, g2, b2)) = (rgb(a), rgb(b));
    let t = t.clamp(0.0, 1.0);
    let l = |x: f32, y: f32| (x + (y - x) * t).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(l(r1, r2), l(g1, g2), l(b1, b2))
}

/// Brightness factor (0 = black).
fn dim(c: Color, k: f32) -> Color {
    mix(Color::Rgb(0, 0, 0), c, k)
}

/// Darker towards the end of a bar.
fn fade(c: Color, along: f32) -> Color {
    dim(c, 1.0 - 0.7 * along.clamp(0.0, 1.0))
}

fn rgba(p: image::Rgba<u8>) -> Color {
    Color::Rgb(p[0], p[1], p[2])
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Up to five dominant, vivid colours of a cover.
pub fn palette(img: &RgbaImage) -> Vec<Color> {
    let mut buckets: std::collections::HashMap<(u8, u8, u8), (u32, [u32; 3])> = std::collections::HashMap::new();
    for p in img.pixels() {
        let key = (p[0] >> 5, p[1] >> 5, p[2] >> 5);
        let e = buckets.entry(key).or_insert((0, [0; 3]));
        e.0 += 1;
        for c in 0..3 {
            e.1[c] += u32::from(p[c]);
        }
    }
    let mut colors: Vec<((f32, f32, f32), f32)> = buckets
        .values()
        .map(|(n, sum)| {
            let avg = (sum[0] as f32 / *n as f32, sum[1] as f32 / *n as f32, sum[2] as f32 / *n as f32);
            let max = avg.0.max(avg.1).max(avg.2);
            let min = avg.0.min(avg.1).min(avg.2);
            let saturation = if max > 0.0 { (max - min) / max } else { 0.0 };
            let score = *n as f32 * (0.25 + saturation) * if max < 40.0 { 0.1 } else { 1.0 };
            (avg, score)
        })
        .collect();
    colors.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut picked: Vec<(f32, f32, f32)> = Vec::new();
    for (c, _) in colors {
        let far = picked.iter().all(|p| ((p.0 - c.0).powi(2) + (p.1 - c.1).powi(2) + (p.2 - c.2).powi(2)).sqrt() > 70.0);
        if far {
            picked.push(c);
        }
        if picked.len() == 5 {
            break;
        }
    }
    picked
        .into_iter()
        .map(|(r, g, b)| {
            // Bright enough to show on a dark terminal.
            let k = (190.0 / r.max(g).max(b).max(1.0)).max(1.0);
            Color::Rgb((r * k).min(255.0) as u8, (g * k).min(255.0) as u8, (b * k).min(255.0) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn cover() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, y| {
            if x < 32 { image::Rgba([220, 40, 60, 255]) } else if y < 32 { image::Rgba([30, 90, 230, 255]) } else { image::Rgba([250, 200, 30, 255]) }
        }))
    }

    #[test]
    fn palette_finds_the_main_colours() {
        let p = palette(&cover().to_rgba8());
        assert_eq!(p.len(), 3, "{p:?}");
    }

    #[test]
    fn style_cycles_and_wraps() {
        let mut v = Visualizer::new(9);
        v.next_style(1);
        assert_eq!(v.style_name(), "Barres");
        v.next_style(-1);
        assert_eq!(v.style_name(), "Aurore");
        v.set_style(12);
        assert_eq!(v.style, 2);
    }

    #[test]
    fn every_style_draws_without_panicking_on_any_size() {
        let track = Track { title: "One More Time", artists: "Daft Punk", position_ms: 60_000, duration_ms: 320_000, playing: true };
        for (w, h) in [(160, 45), (80, 24), (30, 10)] {
            for (style, name) in STYLES.iter().enumerate() {
                let mut v = Visualizer::new(style);
                v.set_cover(Some("c"), Some(&cover()), &[Color::Red, Color::Blue, Color::Green]);
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                for frame_no in 0..20 {
                    let a = Analysis::simulated(frame_no as f32 * 0.05, true);
                    let dt = v.update(&a).max(0.033);
                    terminal.draw(|f| v.draw(f, Some(&track), dt, None)).unwrap();
                }
                let buf = terminal.backend().buffer().clone();
                let text: String = (0..buf.area.height)
                    .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
                    .map(|p| buf[p].symbol().to_owned())
                    .collect();
                if w >= 80 {
                    assert!(text.contains("One More Time"), "style {name} at {w}x{h}");
                    assert!(text.contains('▀'), "cover drawn in style {name}");
                    assert!(!text.contains("c image"), "keys line mentions the cover look");
                }
            }
        }
    }

    /// Prints a few styles for a visual check (`-- --ignored --nocapture`).
    #[test]
    #[ignore]
    fn preview_styles() {
        let track = Track { title: "One More Time", artists: "Daft Punk", position_ms: 60_000, duration_ms: 320_000, playing: true };
        for style in [9, 0] {
            let mut v = Visualizer::new(style);
            if style == 0 {
                v.look = CoverLook::PureAscii;
            }
            let disc = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, y| {
                let d = (((x as f32 - 32.0).powi(2) + (y as f32 - 32.0).powi(2)).sqrt() / 32.0).min(1.0);
                let v = (255.0 * (1.0 - d)) as u8;
                image::Rgba([v, v / 2, 255 - v, 255])
            }));
            v.set_cover(Some("c"), Some(&disc), &[]);
            let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
            for frame_no in 0..45 {
                let a = Analysis::simulated(frame_no as f32 * 0.033, true);
                v.update(&a);
                terminal.draw(|f| v.draw(f, Some(&track), 0.033, None)).unwrap();
            }
            let buf = terminal.backend().buffer().clone();
            println!("===== {} =====", STYLES[style]);
            for y in 0..buf.area.height {
                let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_owned()).collect();
                println!("{line}");
            }
        }
    }

    #[test]
    fn particles_flow_and_burst_on_beats() {
        let mut v = Visualizer::new(5);
        v.set_cover(Some("c"), Some(&cover()), &[]);
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        let count = |t: &Terminal<TestBackend>| {
            let buf = t.backend().buffer();
            buf.content().iter().filter(|c| matches!(c.symbol(), "✦" | "✧" | "•" | "·" | "∙")).count()
        };
        // Quiet: a few sparks still flow.
        let quiet = Analysis::default();
        for _ in 0..30 {
            v.update(&quiet);
            terminal.draw(|f| v.draw(f, None, 0.033, None)).unwrap();
        }
        let calm = count(&terminal);
        assert!(calm > 0, "steady flow even when quiet");
        // A beat: a burst.
        let beat = Analysis { beats: 1, level: 0.8, bass: 0.9, ..Analysis::default() };
        v.update(&beat);
        for _ in 0..6 {
            terminal.draw(|f| v.draw(f, None, 0.033, None)).unwrap();
        }
        assert!(count(&terminal) > calm + 40, "burst: {} vs {calm}", count(&terminal));
    }

    #[test]
    fn cover_looks_cycle_and_draw_characters() {
        assert_eq!(CoverLook::Blocks.next(false), CoverLook::Ascii);
        assert_eq!(CoverLook::Ascii.next(false), CoverLook::PureAscii);
        assert_eq!(CoverLook::PureAscii.next(false), CoverLook::Blocks);
        assert_eq!(CoverLook::PureAscii.next(true), CoverLook::Image);
        for (look, glitchy) in [(CoverLook::Ascii, false), (CoverLook::PureAscii, false), (CoverLook::PureAscii, true)] {
            let mut v = Visualizer::new(if glitchy { GLITCH } else { 0 });
            v.look = look;
            v.ink = Color::Rgb(200, 200, 200);
            v.set_cover(Some("c"), Some(&cover()), &[]);
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            v.update(&Analysis::simulated(0.5, true));
            terminal.draw(|f| v.draw(f, None, 0.033, None)).unwrap();
            let buf = terminal.backend().buffer().clone();
            let cover_rect = cover_rect(Rect { height: 28, ..buf.area }, 1.0);
            let cells: Vec<_> = (cover_rect.y..cover_rect.bottom())
                .flat_map(|y| (cover_rect.x..cover_rect.right()).map(move |x| (x, y)))
                .map(|p| buf[p].clone())
                .collect();
            assert!(cells.iter().all(|c| c.symbol() != "▀"), "{look:?}: no blocks");
            assert!(cells.iter().any(|c| "@%#*+=".contains(c.symbol())), "{look:?}: ascii ramp");
            if look == CoverLook::PureAscii && !glitchy {
                assert!(cells.iter().all(|c| c.fg == Color::Rgb(200, 200, 200)), "pure ascii is monochrome");
            }
        }
    }

    #[test]
    fn aurora_does_not_flash_on_beats() {
        let mut v = Visualizer::new(9);
        v.set_cover(Some("c"), Some(&cover()), &[]);
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        let brightness = |t: &Terminal<TestBackend>| {
            let lum: Vec<f32> = t
                .backend()
                .buffer()
                .content()
                .iter()
                .filter(|c| matches!(c.symbol(), "█" | "▓" | "▒" | "░"))
                .map(|c| match c.fg {
                    Color::Rgb(r, g, b) => (f32::from(r) + f32::from(g) + f32::from(b)) / 765.0,
                    _ => 0.0,
                })
                .collect();
            lum.iter().sum::<f32>() / lum.len().max(1) as f32
        };
        let mut previous: Option<f32> = None;
        for i in 0..20u64 {
            // Worst case: silence and a loud beat alternating every frame.
            let a = if i % 2 == 0 { Analysis { beats: i, level: 1.0, bass: 1.0, ..Analysis::default() } } else { Analysis::default() };
            v.update(&a);
            terminal.draw(|f| v.draw(f, None, 0.033, None)).unwrap();
            let b = brightness(&terminal);
            if let Some(p) = previous {
                assert!((b - p).abs() < 0.05, "frame {i}: brightness jumped {p} -> {b}");
            }
            previous = Some(b);
        }
        assert!(previous.unwrap() > 0.0, "the aurora is visible");
    }
}

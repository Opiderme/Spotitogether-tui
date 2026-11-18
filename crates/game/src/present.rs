//! Rendered pixels to terminal cells, in two looks:
//! - colour: one upper half block `▀` per cell = two pixels (fg + bg);
//! - ASCII: one character per cell from the brightness, coloured, with the
//!   outlines of shapes drawn with `/ \ | _` from jumps in depth.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::raster::Frame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Color,
    Ascii,
}

impl Style {
    pub fn next(self) -> Self {
        match self {
            Self::Color => Self::Ascii,
            Self::Ascii => Self::Color,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Color => "couleur",
            Self::Ascii => "ascii",
        }
    }

    /// Pixel rows per terminal row.
    pub fn rows_per_cell(self) -> usize {
        match self {
            Self::Color => 2,
            Self::Ascii => 1,
        }
    }
}

const RAMP: &[u8] = b".:-=+*#%@";

pub fn draw(frame: &Frame, style: Style, area: Rect, buf: &mut Buffer) {
    match style {
        Style::Color => half_blocks(frame, area, buf),
        Style::Ascii => ascii(frame, area, buf),
    }
}

fn rgb(c: [u8; 3]) -> Color {
    Color::Rgb(c[0], c[1], c[2])
}

fn half_blocks(frame: &Frame, area: Rect, buf: &mut Buffer) {
    for row in 0..(area.height as usize).min(frame.h / 2) {
        for col in 0..(area.width as usize).min(frame.w) {
            let top = frame.color[2 * row * frame.w + col];
            let bottom = frame.color[(2 * row + 1) * frame.w + col];
            buf[(area.x + col as u16, area.y + row as u16)].set_char('▀').set_fg(rgb(top)).set_bg(rgb(bottom));
        }
    }
}

fn ascii(frame: &Frame, area: Rect, buf: &mut Buffer) {
    let (w, h) = (frame.w, frame.h);
    let depth = |x: usize, y: usize| frame.inv_depth[y * w + x];
    let object = |x: usize, y: usize| frame.object[y * w + x];
    // Outline where one object meets another (or the sky), on the side of
    // the nearer one. Ground against ground never makes an edge.
    let border = |x: usize, y: usize, nx: usize, ny: usize| {
        let (a, b) = (depth(x, y), depth(nx, ny));
        if b == 0.0 {
            return false;
        }
        object(x, y) != object(nx, ny) && a >= b * 0.9
    };
    for y in 0..(area.height as usize).min(h) {
        for x in 0..(area.width as usize).min(w) {
            let c = frame.color[y * w + x];
            let d = depth(x, y);
            let cell = &mut buf[(area.x + x as u16, area.y + y as u16)];
            cell.set_bg(Color::Reset);
            if d == 0.0 {
                // Sky: mostly empty, a faint texture near the horizon.
                let near_horizon = y + 1 < h && depth(x, y + 1) != 0.0;
                cell.set_char(if near_horizon { '_' } else { ' ' }).set_fg(rgb(c));
                continue;
            }
            let right = (x + 1 < w && border(x, y, x + 1, y)) || (x > 0 && border(x, y, x - 1, y));
            let below = (y + 1 < h && border(x, y, x, y + 1)) || (y > 0 && depth(x, y - 1) == 0.0 && object(x, y) != 0);
            let ch = match (right, below) {
                (true, true) => {
                    // Corner: lean towards the side that is further away.
                    if x + 1 < w && y + 1 < h && depth(x + 1, y + 1) < d { '/' } else { '\\' }
                }
                (true, false) => '|',
                (false, true) => '_',
                (false, false) => {
                    let luma = 0.2126 * f32::from(c[0]) + 0.7152 * f32::from(c[1]) + 0.0722 * f32::from(c[2]);
                    let i = ((luma / 255.0) * (RAMP.len() - 1) as f32).round() as usize;
                    RAMP[i.min(RAMP.len() - 1)] as char
                }
            };
            cell.set_char(ch).set_fg(rgb(c));
        }
    }
}

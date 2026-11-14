//! Album cover rendered with characters: the image is downloaded once
//! (cached in `~/.cache/spotitogether/covers/`), then each terminal cell gets
//! a character and a 24-bit colour.

use std::path::PathBuf;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Density ramp, light to dense.
const RAMP: &[u8] = b" .:-=+*#%@";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverMode {
    /// One ramp character per cell, coloured with the pixel colour.
    Ascii,
    /// Upper half block: two pixels per cell (foreground + background).
    Blocks,
}

impl CoverMode {
    pub fn next(self) -> Self {
        match self {
            Self::Ascii => Self::Blocks,
            Self::Blocks => Self::Ascii,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ascii => "ascii",
            Self::Blocks => "blocs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Option<Color>,
}

/// Converts `img` to `width` × `height` cells. Terminal cells are about twice
/// as tall as wide, so a square cover fits in `2h × h` cells.
pub fn render(img: &DynamicImage, width: u16, height: u16, mode: CoverMode) -> Vec<Vec<Cell>> {
    if width == 0 || height == 0 {
        return vec![];
    }
    let (w, h) = (u32::from(width), u32::from(height));
    match mode {
        CoverMode::Ascii => {
            let small = img.resize_exact(w, h, FilterType::Triangle);
            (0..h)
                .map(|y| {
                    (0..w)
                        .map(|x| {
                            let p = small.get_pixel(x, y);
                            Cell { ch: ramp_char(p), fg: rgb(p), bg: None }
                        })
                        .collect()
                })
                .collect()
        }
        CoverMode::Blocks => {
            let small = img.resize_exact(w, h * 2, FilterType::Triangle);
            (0..h)
                .map(|y| {
                    (0..w)
                        .map(|x| Cell {
                            ch: '▀',
                            fg: rgb(small.get_pixel(x, y * 2)),
                            bg: Some(rgb(small.get_pixel(x, y * 2 + 1))),
                        })
                        .collect()
                })
                .collect()
        }
    }
}

/// Character for a pixel: brighter pixels get denser characters, so the
/// coloured glyphs carry the image on a dark terminal.
fn ramp_char(p: Rgba<u8>) -> char {
    let luma = 0.2126 * f32::from(p[0]) + 0.7152 * f32::from(p[1]) + 0.0722 * f32::from(p[2]);
    let index = ((luma / 255.0) * (RAMP.len() - 1) as f32).round() as usize;
    // Keep a visible dot even for near-black pixels.
    RAMP[index.clamp(1, RAMP.len() - 1)] as char
}

fn rgb(p: Rgba<u8>) -> Color {
    Color::Rgb(p[0], p[1], p[2])
}

pub fn draw(cells: &[Vec<Cell>], area: Rect, buf: &mut Buffer) {
    for (dy, row) in cells.iter().enumerate().take(area.height as usize) {
        for (dx, cell) in row.iter().enumerate().take(area.width as usize) {
            let target = &mut buf[(area.x + dx as u16, area.y + dy as u16)];
            target.set_char(cell.ch).set_fg(cell.fg);
            if let Some(bg) = cell.bg {
                target.set_bg(bg);
            }
        }
    }
}

fn cache_path(url: &str) -> Option<PathBuf> {
    // i.scdn.co/image/<id>: the id is a stable file name.
    let id: String = url.rsplit('/').next()?.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if id.is_empty() {
        return None;
    }
    let dirs = directories::ProjectDirs::from("", "", "spotitogether")?;
    Some(dirs.cache_dir().join("covers").join(id))
}

/// Loads a cover from the cache or downloads it.
pub async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<DynamicImage> {
    let path = cache_path(url);
    if let Some(p) = &path
        && let Ok(bytes) = tokio::fs::read(p).await
        && let Ok(img) = image::load_from_memory(&bytes)
    {
        return Ok(img);
    }
    let bytes = http.get(url).send().await?.error_for_status()?.bytes().await?;
    let img = image::load_from_memory(&bytes)?;
    if let Some(p) = path {
        if let Some(dir) = p.parent() {
            let _ = tokio::fs::create_dir_all(dir).await;
        }
        let _ = tokio::fs::write(p, &bytes).await;
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;

    fn gradient() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, _| {
            let v = (x * 4) as u8;
            Rgba([v, v, v, 255])
        }))
    }

    #[test]
    fn ascii_has_requested_size_and_follows_brightness() {
        let cells = render(&gradient(), 24, 12, CoverMode::Ascii);
        assert_eq!((cells.len(), cells[0].len()), (12, 24));
        assert_eq!(cells[0][0].ch, '.');
        assert_eq!(cells[0][23].ch, '@');
    }

    #[test]
    fn blocks_use_two_pixels_per_cell() {
        let cells = render(&gradient(), 10, 5, CoverMode::Blocks);
        assert_eq!((cells.len(), cells[0].len()), (5, 10));
        assert!(cells.iter().flatten().all(|c| c.ch == '▀' && c.bg.is_some()));
    }

    #[test]
    fn cache_name_comes_from_image_id() {
        let p = cache_path("https://i.scdn.co/image/ab67616d0000b273abc").unwrap();
        assert!(p.ends_with("covers/ab67616d0000b273abc"));
    }
}

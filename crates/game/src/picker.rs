//! Choosing where to drive on the OpenStreetMap street map, as on the
//! website: tiles assembled at their own resolution and shown as a real
//! picture (Sixel, Kitty) in terminals that can, as coloured half blocks
//! elsewhere. A cross marks the centre and a frame the square that becomes
//! the world.

use image::{Rgba, RgbaImage, imageops};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::tiles::{self, StreetTiles};

const EARTH: f64 = 40_075_016.0;
pub const MIN_ZOOM: u8 = 2;
/// OpenStreetMap's standard tiles stop at 19.
pub const MAX_ZOOM: u8 = 19;
/// The map's own land colour, shown while tiles load.
const PAPER: Rgba<u8> = Rgba([242, 239, 233, 255]);

/// What the player picked.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    Place { lat: f64, lon: f64, name: String },
    Generated,
    Quit,
}

/// Where the map looks.
pub struct MapView {
    /// Centre in Web Mercator units ([0, 1], y down).
    pub x: f64,
    pub y: f64,
    /// OpenStreetMap zoom level: 256 × 2^zoom pixels around the world.
    pub zoom: u8,
    /// Name of the place at the centre (after a search).
    pub label: Option<String>,
    /// Message on the status line (errors, hints).
    pub status: Option<String>,
    /// Search text being typed.
    pub search: Option<String>,
}

impl Default for MapView {
    fn default() -> Self {
        // Western Europe.
        Self::centred(47.0, 4.0, 5)
    }
}

impl MapView {
    pub fn centred(lat: f64, lon: f64, zoom: u8) -> Self {
        let (x, y) = tiles::mercator(lat, lon);
        Self { x, y, zoom: zoom.clamp(MIN_ZOOM, MAX_ZOOM), label: None, status: None, search: None }
    }

    fn world_px(&self) -> f64 {
        256.0 * f64::from(1u32 << self.zoom)
    }

    /// Ground metres per map pixel at the centre.
    pub fn metres_per_px(&self) -> f64 {
        let (lat, _) = self.centre();
        EARTH * lat.to_radians().cos() / self.world_px()
    }

    pub fn centre(&self) -> (f64, f64) {
        tiles::degrees(self.x, self.y)
    }

    pub fn zoom_by(&mut self, steps: i8) {
        self.zoom = (self.zoom as i8 + steps).clamp(MIN_ZOOM as i8, MAX_ZOOM as i8) as u8;
    }

    /// Moves by map pixels.
    pub fn pan_px(&mut self, dx: f64, dy: f64) {
        let world = self.world_px();
        self.x = (self.x + dx / world).rem_euclid(1.0);
        self.y = (self.y + dy / world).clamp(0.0, 1.0);
    }
}

/// Picture of the map around the centre, `w` × `h` pixels at the zoom's
/// own resolution, with the cross and the frame of the world to build.
/// Returns it and whether every tile was there.
pub fn compose(view: &MapView, tiles: &StreetTiles, w: u32, h: u32, area_metres: f32) -> (RgbaImage, bool) {
    let z = view.zoom;
    let n = 1i64 << z;
    let world = view.world_px();
    let (left, top) = (view.x * world - f64::from(w) / 2.0, view.y * world - f64::from(h) / 2.0);
    let mut out = RgbaImage::from_pixel(w, h, PAPER);
    let mut complete = true;
    let (tx0, tx1) = ((left / 256.0).floor() as i64, ((left + f64::from(w)) / 256.0).floor() as i64);
    let (ty0, ty1) = ((top / 256.0).floor() as i64, ((top + f64::from(h)) / 256.0).floor() as i64);
    for ty in ty0.max(0)..=ty1.min(n - 1) {
        for tx in tx0..=tx1 {
            let wrapped = tx.rem_euclid(n) as u32;
            let (ox, oy) = ((tx * 256) as f64 - left, (ty * 256) as f64 - top);
            if let Some(tile) = tiles.get(z, wrapped, ty as u32) {
                imageops::overlay(&mut out, &*tile, ox.round() as i64, oy.round() as i64);
                continue;
            }
            complete = false;
            // Meanwhile, a blown-up piece of a coarser tile already loaded.
            for up in 1..=z.min(6) {
                let Some(parent) = tiles.peek(z - up, wrapped >> up, ty as u32 >> up) else { continue };
                let size = 256 >> up;
                let (sx, sy) = ((wrapped & ((1 << up) - 1)) * size, (ty as u32 & ((1 << up) - 1)) * size);
                let piece = imageops::crop_imm(&*parent, sx, sy, size, size).to_image();
                let piece = imageops::resize(&piece, 256, 256, imageops::FilterType::Triangle);
                imageops::overlay(&mut out, &piece, ox.round() as i64, oy.round() as i64);
                break;
            }
        }
    }
    draw_target(&mut out, (f64::from(area_metres) / view.metres_per_px()) as i64);
    (out, complete)
}

fn put(img: &mut RgbaImage, x: i64, y: i64, c: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < img.width() && (y as u32) < img.height() {
        img.put_pixel(x as u32, y as u32, c);
    }
}

/// Frame of the world to build (`side` pixels) and a cross at the centre.
fn draw_target(img: &mut RgbaImage, side: i64) {
    let (cx, cy) = (i64::from(img.width()) / 2, i64::from(img.height()) / 2);
    let thick = (i64::from(img.width()) / 500).max(1);
    let gold = Rgba([235, 160, 20, 255]);
    let red = Rgba([220, 40, 40, 255]);
    if side >= 12 && side < i64::from(img.width()) {
        let (x0, y0, x1, y1) = (cx - side / 2, cy - side / 2, cx + side / 2, cy + side / 2);
        for t in 0..thick * 2 {
            for x in x0..=x1 {
                put(img, x, y0 + t, gold);
                put(img, x, y1 - t, gold);
            }
            for y in y0..=y1 {
                put(img, x0 + t, y, gold);
                put(img, x1 - t, y, gold);
            }
        }
    }
    let arm = 6 * thick + 4;
    for d in -arm..=arm {
        for t in -thick..=thick {
            if d.abs() > thick + 1 {
                put(img, cx + d, cy + t, red);
                put(img, cx + t, cy + d, red);
            }
        }
    }
}

/// Half blocks for terminals without pictures: two pixels per cell, each
/// the average of a 2 × 2 block of the map.
pub fn draw_blocks(img: &RgbaImage, area: Rect, buf: &mut Buffer) {
    let px = |x: u32, y: u32| -> [u8; 3] {
        let mut sum = [0u32; 3];
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let p = img.get_pixel((x * 2 + dx).min(img.width() - 1), (y * 2 + dy).min(img.height() - 1));
            for (k, s) in sum.iter_mut().enumerate() {
                *s += u32::from(p[k]);
            }
        }
        sum.map(|v| (v / 4) as u8)
    };
    for row in 0..area.height {
        for col in 0..area.width {
            let (top, bottom) = (px(u32::from(col), u32::from(row) * 2), px(u32::from(col), u32::from(row) * 2 + 1));
            buf[(area.x + col, area.y + row)]
                .set_char('▀')
                .set_fg(Color::Rgb(top[0], top[1], top[2]))
                .set_bg(Color::Rgb(bottom[0], bottom[1], bottom[2]));
        }
    }
}

/// Graphics protocol from the environment, without querying the terminal
/// (a query left unanswered would swallow key presses).
fn graphics_protocol() -> Option<ProtocolType> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if !env("TMUX").is_empty() || env("TERM").starts_with("screen") {
        return None;
    }
    let (term, program) = (env("TERM"), env("TERM_PROGRAM"));
    if !env("KITTY_WINDOW_ID").is_empty() || term == "xterm-kitty" || term == "xterm-ghostty" {
        Some(ProtocolType::Kitty)
    } else if term.starts_with("foot") {
        Some(ProtocolType::Sixel)
    } else if matches!(program.as_str(), "WezTerm" | "iTerm.app") {
        Some(ProtocolType::Iterm2)
    } else {
        None
    }
}

/// Picture support for this terminal, with its cell size in pixels.
pub fn image_picker() -> Option<Picker> {
    let protocol = graphics_protocol()?;
    let size = crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.columns == 0 || size.rows == 0 {
        return None;
    }
    let font = ratatui_image::FontSize { width: size.width / size.columns, height: size.height / size.rows };
    // Deprecated in favour of a terminal query, which is what is avoided.
    #[allow(deprecated)]
    let mut picker = Picker::from_fontsize(font);
    picker.set_protocol_type(protocol);
    Some(picker)
}

/// "45.8992° N, 6.1289° E"
pub fn format_position(lat: f64, lon: f64) -> String {
    format!(
        "{:.4}° {}, {:.4}° {}",
        lat.abs(),
        if lat >= 0.0 { "N" } else { "S" },
        lon.abs(),
        if lon >= 0.0 { "E" } else { "O" }
    )
}

/// "3,2 km" or "25 m".
pub fn format_distance(metres: f64) -> String {
    if metres >= 1000.0 {
        format!("{} km", format!("{:.1}", metres / 1000.0).replace('.', ","))
    } else {
        format!("{metres:.0} m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_pan_and_centre() {
        let mut v = MapView::centred(45.9, 6.13, 12);
        let (lat, lon) = v.centre();
        assert!((lat - 45.9).abs() < 1e-9 && (lon - 6.13).abs() < 1e-9);
        let before = v.metres_per_px();
        v.zoom_by(1);
        assert!((v.metres_per_px() - before / 2.0).abs() < 1e-6);
        v.pan_px(100.0, 0.0);
        assert!(v.centre().1 > 6.13, "panned east");
        v.zoom_by(40);
        assert_eq!(v.zoom, MAX_ZOOM);
        // Longitude wraps around.
        let mut w = MapView::centred(0.0, 179.9, 3);
        w.pan_px(500.0, 0.0);
        assert!((0.0..1.0).contains(&w.x));
    }

    #[test]
    fn target_frame_matches_the_area() {
        let mut img = RgbaImage::from_pixel(400, 200, PAPER);
        draw_target(&mut img, 100);
        // Frame edges 50 px from the centre, nothing far outside.
        assert_ne!(*img.get_pixel(150, 100), PAPER);
        assert_ne!(*img.get_pixel(250, 120), PAPER);
        assert_eq!(*img.get_pixel(20, 20), PAPER);
    }

    #[test]
    fn formats() {
        assert_eq!(format_position(45.89923, -6.1), "45.8992° N, 6.1000° O");
        assert_eq!(format_distance(3200.0), "3,2 km");
        assert_eq!(format_distance(24.4), "24 m");
    }
}

#[cfg(test)]
mod snapshots {
    use super::*;

    /// Composes the map at a few zooms into $SNAPSHOT_DIR as PNG files.
    #[test]
    #[ignore = "downloads OpenStreetMap tiles, writes files"]
    fn map_snapshots() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let tiles = StreetTiles::street();
        for (name, view) in [
            ("europe", MapView::default()),
            ("annecy-13", MapView::centred(45.8992, 6.1289, 13)),
            ("annecy-16", MapView::centred(45.8992, 6.1289, 16)),
        ] {
            let (mut img, mut done) = compose(&view, &tiles, 1000, 520, 1200.0);
            for _ in 0..200 {
                if done {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
                (img, done) = compose(&view, &tiles, 1000, 520, 1200.0);
            }
            img.save(dir.join(format!("street-{name}.png"))).unwrap();
        }
        println!("{} tuiles en échec", tiles.failures());
    }
}

#[cfg(test)]
mod encode_cost {
    #[test]
    #[ignore = "timing"]
    fn sixel_full_screen() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let img = image::open(dir.join("street-annecy-13.png")).unwrap();
        let img = img.resize_exact(1900, 1000, image::imageops::FilterType::Nearest);
        #[allow(deprecated)]
        let mut picker = ratatui_image::picker::Picker::from_fontsize(ratatui_image::FontSize { width: 9, height: 19 });
        picker.set_protocol_type(ratatui_image::picker::ProtocolType::Sixel);
        let t = std::time::Instant::now();
        let _p = picker.new_protocol(img, ratatui::layout::Rect::new(0, 0, 211, 52).into(), ratatui_image::Resize::Fit(None)).unwrap();
        println!("sixel 1900×1000 : {:?}", t.elapsed());
    }
}

//! The street map in ASCII art, drawn from OpenStreetMap vector data (as
//! `mapscii` does): every road with characters following its direction and
//! a colour for its class, water in `~`, woods in `&`, fields and meadows,
//! buildings in `#`, the names of places, lakes, summits and streets.
//!
//! One character covers 4 × 8 map pixels at the view's zoom, so a screen
//! shows about four times what a web map would.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::mvt::{Feature, Kind, VectorTile};
use crate::noise;
use crate::picker::MapView;
use crate::tiles::{VECTOR_MAX_ZOOM, VectorTiles};

/// Map pixels per character at the view's zoom.
pub const CELL_W: f64 = 4.0;
pub const CELL_H: f64 = 8.0;

const EMPTY: Color = Color::Rgb(70, 72, 80);
const LABEL_BG: Color = Color::Rgb(16, 18, 24);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Option<Color>,
    pub bold: bool,
}

const BLANK: Cell = Cell { ch: ' ', fg: EMPTY, bg: None, bold: false };

struct Canvas {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    prio: Vec<u8>,
    /// Feature that drew the cell (junctions are where two roads meet).
    owner: Vec<u32>,
    /// Taken by a label.
    label: Vec<bool>,
    /// Names already written (neighbouring tiles repeat them).
    named: std::collections::HashSet<String>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            w,
            h,
            cells: vec![BLANK; n],
            prio: vec![0; n],
            owner: vec![u32::MAX; n],
            label: vec![false; n],
            named: std::collections::HashSet::new(),
        }
    }

    fn index(&self, x: i64, y: i64) -> Option<usize> {
        (x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h).then(|| y as usize * self.w + x as usize)
    }

    fn set(&mut self, x: i64, y: i64, cell: Cell, prio: u8, owner: u32) {
        let Some(i) = self.index(x, y) else { return };
        if self.label[i] || prio < self.prio[i] {
            return;
        }
        let mut cell = cell;
        // Lines and texture keep the area's background.
        if cell.bg.is_none() {
            cell.bg = self.cells[i].bg;
        }
        // Two different roads crossing: a junction.
        if prio >= ROAD && self.prio[i] >= ROAD && self.owner[i] != owner && self.cells[i].ch != cell.ch {
            cell.ch = '+';
        }
        self.cells[i] = cell;
        self.prio[i] = prio;
        self.owner[i] = owner;
    }
}

/// Priorities: higher draws over lower.
const ROAD: u8 = 20;

/// Where a tile's units land on the canvas.
#[derive(Clone, Copy)]
struct Placement {
    /// Canvas position of the tile's top-left corner and size in cells.
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Placement {
    fn at(&self, extent: f32, (u, v): (f32, f32)) -> (f64, f64) {
        (self.x + f64::from(u / extent) * self.w, self.y + f64::from(v / extent) * self.h)
    }
}

/// An area: a background tint and a sparse texture character (one cell
/// in `every`), so roads and names stay readable on top.
struct Area {
    ch: char,
    color: (u8, u8, u8),
    bg: (u8, u8, u8),
    every: u32,
    prio: u8,
}

fn area_style(layer: &str, f: &Feature, zoom: u8) -> Option<Area> {
    let class = f.tag("class").unwrap_or_default();
    let a = |ch, color, bg, every, prio| Some(Area { ch, color, bg, every, prio });
    match (layer, class) {
        ("water", _) => a('~', (70, 140, 220), (14, 34, 66), 3, 6),
        ("building", _) if zoom >= 15 => a('#', (190, 152, 120), (54, 44, 36), 1, 8),
        ("park", _) => a('"', (92, 168, 92), (16, 42, 22), 5, 4),
        ("landcover", "wood" | "forest") => a('&', (60, 138, 70), (14, 36, 20), 3, 3),
        ("landcover", "grass") => a(',', (96, 158, 80), (20, 38, 20), 6, 3),
        ("landcover", "farmland") => a('"', (150, 146, 88), (34, 34, 20), 6, 2),
        ("landcover", "wetland") => a('_', (72, 140, 140), (16, 36, 38), 4, 3),
        ("landcover", "sand") => a('.', (212, 192, 132), (48, 42, 28), 4, 3),
        ("landcover", "rock") => a('%', (150, 144, 138), (34, 32, 32), 4, 3),
        ("landcover", "ice") => a('*', (222, 232, 246), (50, 56, 66), 4, 3),
        ("landuse", "residential" | "suburb" | "neighbourhood" | "quarter") => a(' ', (0, 0, 0), (30, 30, 34), 1, 1),
        ("landuse", "commercial" | "retail" | "industrial" | "railway" | "garages") => a(' ', (0, 0, 0), (36, 28, 36), 1, 1),
        ("landuse", "cemetery") => a('+', (92, 134, 92), (22, 34, 24), 4, 2),
        ("landuse", "school" | "university" | "college" | "hospital") => a(' ', (0, 0, 0), (40, 34, 26), 1, 1),
        ("landuse", "pitch" | "stadium" | "playground") => a('"', (110, 170, 110), (18, 44, 24), 4, 2),
        ("aeroway", "runway" | "taxiway" | "apron") => a('=', (160, 160, 170), (40, 40, 44), 1, 5),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Pattern {
    /// `-` `|` `/` `\` along the line.
    Direction,
    /// Direction with a `+` every third character (railways).
    Rail,
    Fixed(char),
}

struct Stroke {
    color: (u8, u8, u8),
    prio: u8,
    bold: bool,
    pattern: Pattern,
}

fn line_style(layer: &str, f: &Feature, zoom: u8) -> Option<Stroke> {
    let class = f.tag("class").unwrap_or_default();
    let tunnel = f.tag("brunnel") == Some("tunnel");
    let s = |color: (u8, u8, u8), prio, bold, pattern, min_zoom: u8| {
        let color = if tunnel { (color.0 / 2, color.1 / 2, color.2 / 2) } else { color };
        (zoom >= min_zoom).then_some(Stroke { color, prio, bold, pattern })
    };
    match (layer, class) {
        ("transportation", "motorway") => s((236, 92, 88), ROAD + 9, true, Pattern::Direction, 0),
        ("transportation", "trunk") => s((242, 134, 72), ROAD + 8, true, Pattern::Direction, 0),
        ("transportation", "primary") => s((246, 168, 82), ROAD + 7, true, Pattern::Direction, 5),
        ("transportation", "secondary") => s((240, 216, 104), ROAD + 6, false, Pattern::Direction, 8),
        ("transportation", "tertiary") => s((236, 236, 206), ROAD + 5, false, Pattern::Direction, 10),
        ("transportation", "minor") => s((204, 204, 204), ROAD + 4, false, Pattern::Direction, 15),
        ("transportation", "service") => s((150, 150, 150), ROAD + 3, false, Pattern::Direction, 16),
        ("transportation", "track") => s((164, 132, 92), ROAD + 2, false, Pattern::Fixed(':'), 15),
        ("transportation", "path") => s((140, 140, 140), ROAD + 1, false, Pattern::Fixed('.'), 17),
        ("transportation", "rail" | "transit") => s((172, 172, 196), ROAD, false, Pattern::Rail, 9),
        ("waterway", "river" | "canal") => s((84, 150, 226), 10, false, Pattern::Fixed('~'), 8),
        ("waterway", _) => s((96, 160, 220), 9, false, Pattern::Fixed('~'), 13),
        ("aeroway", "runway") => s((170, 170, 180), 11, true, Pattern::Fixed('='), 10),
        ("boundary", _) if f.tags.get("maritime").and_then(|v| v.number()) == Some(1.0) => None,
        ("boundary", _) => match f.tags.get("admin_level").and_then(|v| v.number()) {
            Some(2.0) if zoom <= 11 => s((184, 120, 184), 12, false, Pattern::Fixed(':'), 0),
            Some(4.0) if (5..=9).contains(&zoom) => s((140, 104, 140), 11, false, Pattern::Fixed('.'), 0),
            _ => None,
        },
        _ => None,
    }
}

fn direction(dx: f64, dy: f64) -> char {
    // Characters are twice as tall as wide.
    let angle = (-dy * 2.0).atan2(dx).to_degrees().rem_euclid(180.0);
    match angle {
        a if !(22.5..157.5).contains(&a) => '-',
        a if (67.5..112.5).contains(&a) => '|',
        a if a < 90.0 => '/',
        _ => '\\',
    }
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

fn fill(canvas: &mut Canvas, rings: &[Vec<(f64, f64)>], area: &Area, owner: u32) {
    let (mut y0, mut y1) = (f64::MAX, f64::MIN);
    for p in rings.iter().flatten() {
        y0 = y0.min(p.1);
        y1 = y1.max(p.1);
    }
    let rows = (y0.floor().max(0.0) as i64)..=(y1.ceil().min(canvas.h as f64 - 1.0) as i64);
    let mut xs = Vec::new();
    for row in rows {
        let cy = row as f64 + 0.5;
        xs.clear();
        for ring in rings {
            for w in ring.windows(2) {
                let (a, b) = (w[0], w[1]);
                if (a.1 <= cy) != (b.1 <= cy) {
                    xs.push(a.0 + (cy - a.1) / (b.1 - a.1) * (b.0 - a.0));
                }
            }
        }
        xs.sort_by(f64::total_cmp);
        for pair in xs.as_chunks::<2>().0 {
            let from = (pair[0] - 0.5).ceil().max(0.0) as i64;
            let to = (pair[1] - 0.5).floor().min(canvas.w as f64 - 1.0) as i64;
            for col in from..=to {
                // Sparse texture, scattered so it does not form lines.
                let mark = area.every <= 1 || noise::hash(col as i32, row as i32, 3) < 1.0 / area.every as f32;
                let ch = if mark { area.ch } else { ' ' };
                canvas.set(col, row, Cell { ch, fg: rgb(area.color), bg: Some(rgb(area.bg)), bold: false }, area.prio, owner);
            }
        }
    }
}

fn stroke(canvas: &mut Canvas, line: &[(f64, f64)], style: &Stroke, owner: u32) {
    let mut count = 0usize;
    for w in line.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        // Skip segments far outside the canvas.
        if (a.0 < -2.0 && b.0 < -2.0) || (a.1 < -2.0 && b.1 < -2.0) {
            continue;
        }
        if (a.0 > canvas.w as f64 + 2.0 && b.0 > canvas.w as f64 + 2.0) || (a.1 > canvas.h as f64 + 2.0 && b.1 > canvas.h as f64 + 2.0) {
            continue;
        }
        let dir = direction(dx, dy);
        let steps = ((dx.abs().max(dy.abs())) * 2.0).ceil().max(1.0) as usize;
        let mut last = (i64::MIN, i64::MIN);
        for k in 0..=steps {
            let t = k as f64 / steps as f64;
            let cell = ((a.0 + dx * t).floor() as i64, (a.1 + dy * t).floor() as i64);
            if cell == last {
                continue;
            }
            last = cell;
            count += 1;
            let ch = match style.pattern {
                Pattern::Direction => dir,
                Pattern::Rail if count.is_multiple_of(3) => '+',
                Pattern::Rail => dir,
                Pattern::Fixed(c) => c,
            };
            canvas.set(cell.0, cell.1, Cell { ch, fg: rgb(style.color), bg: None, bold: style.bold }, style.prio, owner);
        }
    }
}

/// Writes a name once per map, where it does not cover another label.
fn name_label(canvas: &mut Canvas, x: i64, y: i64, text: &str, fg: (u8, u8, u8), bold: bool, bg: Color) -> bool {
    let key = text.trim().to_owned();
    if canvas.named.contains(&key) {
        return false;
    }
    let written = label(canvas, x, y, text, fg, bold, bg);
    if written {
        canvas.named.insert(key);
    }
    written
}

/// Writes `text` at (x, y) unless it would cover another label.
fn label(canvas: &mut Canvas, x: i64, y: i64, text: &str, fg: (u8, u8, u8), bold: bool, bg: Color) -> bool {
    let len = text.chars().count() as i64;
    if y < 0 || y >= canvas.h as i64 || x < 0 || x + len > canvas.w as i64 {
        return false;
    }
    let row = y as usize * canvas.w;
    let clear = (x - 1).max(0)..(x + len + 1).min(canvas.w as i64);
    if clear.clone().any(|c| canvas.label[row + c as usize]) {
        return false;
    }
    for (k, ch) in text.chars().enumerate() {
        let i = row + x as usize + k;
        canvas.cells[i] = Cell { ch, fg: rgb(fg), bg: Some(bg), bold };
        canvas.label[i] = true;
    }
    true
}

/// Display name: French, then Latin script, then the local name.
fn name(f: &Feature) -> Option<&str> {
    f.tag("name:fr").or_else(|| f.tag("name:latin")).or_else(|| f.tag("name")).filter(|n| !n.is_empty())
}

/// How a class of place is labelled: zooms where it shows, colour, bold,
/// marker, order (lower first).
type PlaceStyle = (u8, u8, (u8, u8, u8), bool, Option<char>, u8);

fn place_style(class: &str) -> Option<PlaceStyle> {
    Some(match class {
        "continent" => (0, 2, (230, 230, 230), true, None, 0),
        "country" => (2, 6, (240, 240, 240), true, None, 1),
        "state" => (5, 8, (200, 190, 210), false, None, 2),
        "city" => (4, 14, (255, 255, 255), true, Some('O'), 3),
        "town" => (8, 15, (240, 240, 240), true, Some('o'), 4),
        "village" => (11, 16, (220, 220, 220), false, Some('o'), 5),
        "suburb" => (12, 16, (200, 200, 210), false, None, 6),
        "hamlet" | "neighbourhood" | "quarter" => (14, 19, (180, 180, 190), false, Some('.'), 7),
        "isolated_dwelling" | "locality" => (16, 19, (160, 160, 170), false, Some('.'), 8),
        _ => return None,
    })
}

/// The map around the view's centre, `cols` × `rows` characters, with the
/// frame of the world to build. Returns it and whether every tile was
/// there.
pub fn render(view: &MapView, tiles: &VectorTiles, cols: u16, rows: u16, area_metres: f32) -> (Vec<Cell>, bool) {
    let (w, h) = (usize::from(cols), usize::from(rows));
    let mut canvas = Canvas::new(w, h);
    let zoom = view.zoom;
    // One data zoom below the view: its detail suits 4 × 8 pixel cells.
    let dz = zoom.saturating_sub(1).min(VECTOR_MAX_ZOOM);
    let scale = f64::from(1u32 << (zoom - dz));
    let world = 256.0 * f64::from(1u32 << zoom);
    let (left, top) = (view.x * world - f64::from(cols) * CELL_W / 2.0, view.y * world - f64::from(rows) * CELL_H / 2.0);
    let tile_px = 256.0 * scale;
    let n = 1i64 << dz;
    let mut complete = true;
    let mut visible: Vec<(std::sync::Arc<VectorTile>, Placement)> = Vec::new();
    let (tx0, tx1) = ((left / tile_px).floor() as i64, ((left + f64::from(cols) * CELL_W) / tile_px).floor() as i64);
    let (ty0, ty1) = ((top / tile_px).floor() as i64, ((top + f64::from(rows) * CELL_H) / tile_px).floor() as i64);
    for ty in ty0.max(0)..=ty1.min(n - 1) {
        for tx in tx0..=tx1 {
            let placement = Placement {
                x: (tx as f64 * tile_px - left) / CELL_W,
                y: (ty as f64 * tile_px - top) / CELL_H,
                w: tile_px / CELL_W,
                h: tile_px / CELL_H,
            };
            match tiles.get(dz, tx.rem_euclid(n) as u32, ty as u32) {
                Some(t) => visible.push((t, placement)),
                None => complete = false,
            }
        }
    }

    // Areas, lower layers first.
    let mut owner = 0u32;
    for layer_name in ["landuse", "landcover", "park", "aeroway", "water", "building"] {
        for (tile, place) in &visible {
            let Some(layer) = tile.layer(layer_name) else { continue };
            for f in layer.features.iter().filter(|f| f.kind == Kind::Polygon) {
                owner += 1;
                if let Some(style) = area_style(layer_name, f, zoom) {
                    let rings: Vec<Vec<(f64, f64)>> =
                        f.parts.iter().map(|r| r.iter().map(|&p| place.at(layer.extent, p)).collect()).collect();
                    fill(&mut canvas, &rings, &style, owner);
                }
            }
        }
    }
    // Lines: minor roads first, so major ones stay on top at crossings.
    let mut lines: Vec<(u8, &Feature, Placement, f32, &str)> = Vec::new();
    for (tile, place) in &visible {
        for layer_name in ["boundary", "waterway", "aeroway", "transportation"] {
            let Some(layer) = tile.layer(layer_name) else { continue };
            for f in layer.features.iter().filter(|f| f.kind == Kind::Line) {
                if let Some(style) = line_style(layer_name, f, zoom) {
                    lines.push((style.prio, f, *place, layer.extent, layer.name.as_str()));
                }
            }
        }
    }
    lines.sort_by_key(|l| l.0);
    for (_, f, place, extent, layer_name) in &lines {
        owner += 1;
        let style = line_style(layer_name, f, zoom).expect("filtered above");
        for part in &f.parts {
            let pts: Vec<(f64, f64)> = part.iter().map(|&p| place.at(*extent, p)).collect();
            stroke(&mut canvas, &pts, &style, owner);
        }
    }

    // Names: places first (most important first), then lakes, summits and
    // streets where room is left.
    let mut places: Vec<(u8, f64, &Feature, Placement, f32)> = Vec::new();
    for (tile, place) in &visible {
        if let Some(layer) = tile.layer("place") {
            for f in &layer.features {
                if let Some((lo, hi, _, _, _, order)) = f.tag("class").and_then(place_style)
                    && (lo..=hi).contains(&zoom)
                {
                    let rank = f.tags.get("rank").and_then(|v| v.number()).unwrap_or(99.0);
                    places.push((order, rank, f, *place, layer.extent));
                }
            }
        }
    }
    places.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    for (_, _, f, place, extent) in places {
        let (Some(text), Some(&p)) = (name(f), f.parts.first().and_then(|p| p.first())) else { continue };
        let (_, _, color, bold, marker, _) = place_style(f.tag("class").unwrap_or_default()).expect("filtered above");
        let (x, y) = place.at(extent, p);
        let text = if f.tag("class") == Some("country") { text.to_uppercase() } else { text.to_owned() };
        let (x, y) = (x.floor() as i64, y.floor() as i64);
        match marker {
            Some(m) => {
                if name_label(&mut canvas, x + 1, y, &text, color, bold, LABEL_BG) {
                    label(&mut canvas, x, y, &m.to_string(), (255, 214, 80), true, LABEL_BG);
                }
            }
            None => {
                name_label(&mut canvas, x - text.chars().count() as i64 / 2, y, &text, color, bold, LABEL_BG);
            }
        }
    }
    for (tile, place) in &visible {
        if zoom >= 10
            && let Some(layer) = tile.layer("water_name")
        {
            for f in &layer.features {
                let (Some(text), Some(&p)) = (name(f), f.parts.first().and_then(|p| p.first())) else { continue };
                let (x, y) = place.at(layer.extent, p);
                name_label(&mut canvas, x as i64 - text.chars().count() as i64 / 2, y as i64, text, (150, 200, 250), false, LABEL_BG);
            }
        }
        if zoom >= 11
            && let Some(layer) = tile.layer("mountain_peak")
        {
            for f in &layer.features {
                let (Some(text), Some(&p)) = (name(f), f.parts.first().and_then(|p| p.first())) else { continue };
                let (x, y) = place.at(layer.extent, p);
                let ele = f.tags.get("ele").and_then(|v| v.number()).map(|e| format!(" {e:.0} m")).unwrap_or_default();
                if name_label(&mut canvas, x as i64 + 1, y as i64, &format!("{text}{ele}"), (214, 170, 120), false, LABEL_BG) {
                    label(&mut canvas, x as i64, y as i64, "^", (230, 180, 120), true, LABEL_BG);
                }
            }
        }
        if zoom >= 15
            && let Some(layer) = tile.layer("transportation_name")
        {
            for f in layer.features.iter().filter(|f| f.kind == Kind::Line) {
                let Some(text) = name(f) else { continue };
                street_name(&mut canvas, f, place, layer.extent, text);
            }
        }
    }

    draw_target(&mut canvas, view, area_metres);
    (canvas.cells, complete)
}

/// A street's name written on its longest flat stretch, like a road sign.
fn street_name(canvas: &mut Canvas, f: &Feature, place: &Placement, extent: f32, text: &str) {
    let len = text.chars().count() as f64 + 2.0;
    for part in &f.parts {
        for w in part.windows(2) {
            let (a, b) = (place.at(extent, w[0]), place.at(extent, w[1]));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            if dx.abs() < len || (dy * 2.0).abs() > dx.abs() * 0.15 {
                continue;
            }
            let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
            let x = (mid.0 - (len - 2.0) / 2.0).round() as i64;
            if name_label(canvas, x, mid.1.floor() as i64, &format!(" {text} "), (24, 24, 30), false, Color::Rgb(206, 206, 196)) {
                return;
            }
        }
    }
}

/// The frame of the world to build and a cross at the centre.
fn draw_target(canvas: &mut Canvas, view: &MapView, metres: f32) {
    let (cx, cy) = (canvas.w as i64 / 2, canvas.h as i64 / 2);
    let px = f64::from(metres) / view.metres_per_px();
    let (hw, hh) = ((px / CELL_W / 2.0).round() as i64, (px / CELL_H / 2.0).round() as i64);
    let gold = Cell { ch: '-', fg: Color::Rgb(255, 196, 40), bg: None, bold: true };
    if hw >= 3 && hh >= 1 && (hw as usize) < canvas.w {
        let mut put = |x: i64, y: i64, ch: char| {
            if let Some(i) = canvas.index(x, y) {
                canvas.cells[i] = Cell { ch, ..gold };
            }
        };
        for x in cx - hw..=cx + hw {
            let corner = x == cx - hw || x == cx + hw;
            put(x, cy - hh, if corner { '+' } else { '-' });
            put(x, cy + hh, if corner { '+' } else { '-' });
        }
        for y in cy - hh + 1..cy + hh {
            put(cx - hw, y, '|');
            put(cx + hw, y, '|');
        }
    }
    if let Some(i) = canvas.index(cx, cy) {
        canvas.cells[i] = Cell { ch: 'X', fg: Color::Rgb(255, 70, 60), bg: None, bold: true };
    }
}

pub fn draw(cells: &[Cell], area: Rect, buf: &mut Buffer) {
    for (i, cell) in cells.iter().enumerate() {
        let (x, y) = ((i % usize::from(area.width)) as u16, (i / usize::from(area.width)) as u16);
        if y >= area.height {
            break;
        }
        let mut style = Style::default().fg(cell.fg).bg(cell.bg.unwrap_or(Color::Reset));
        if cell.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        buf[(area.x + x, area.y + y)].set_char(cell.ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn road_characters_follow_the_direction() {
        assert_eq!(direction(1.0, 0.0), '-');
        assert_eq!(direction(0.0, 1.0), '|');
        assert_eq!(direction(1.0, -0.5), '/');
        assert_eq!(direction(1.0, 0.5), '\\');
        assert_eq!(direction(-1.0, 0.0), '-');
    }

    #[test]
    fn fills_polygons_and_draws_crossings() {
        let mut c = Canvas::new(20, 10);
        let square = vec![vec![(2.0, 2.0), (8.0, 2.0), (8.0, 6.0), (2.0, 6.0), (2.0, 2.0)]];
        fill(&mut c, &square, &Area { ch: '~', color: (0, 0, 255), bg: (0, 0, 60), every: 1, prio: 6 }, 1);
        assert_eq!(c.cells[3 * 20 + 4].ch, '~');
        assert_eq!(c.cells[8 * 20 + 4].ch, ' ');
        let road = Stroke { color: (255, 255, 255), prio: ROAD + 4, bold: false, pattern: Pattern::Direction };
        stroke(&mut c, &[(0.0, 8.5), (19.0, 8.5)], &road, 2);
        stroke(&mut c, &[(10.5, 0.0), (10.5, 9.9)], &road, 3);
        assert_eq!(c.cells[8 * 20 + 3].ch, '-');
        assert_eq!(c.cells[2 * 20 + 10].ch, '|');
        assert_eq!(c.cells[8 * 20 + 10].ch, '+', "junction");
        assert!(label(&mut c, 1, 0, "Annecy", (255, 255, 255), true, LABEL_BG));
        assert!(!label(&mut c, 4, 0, "Seynod", (255, 255, 255), true, LABEL_BG), "overlaps");
    }
}

#[cfg(test)]
mod snapshots {
    use super::*;

    #[test]
    #[ignore = "downloads OpenFreeMap tiles, writes $SNAPSHOT_DIR/ascii-*.txt"]
    fn ascii_snapshots() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let tiles = VectorTiles::vector();
        for (name, view) in [
            ("europe", MapView::centred(47.0, 4.0, 5)),
            ("annecy-12", MapView::centred(45.8992, 6.1289, 12)),
            ("annecy-14", MapView::centred(45.8992, 6.1289, 14)),
            ("annecy-16", MapView::centred(45.8992, 6.1289, 16)),
        ] {
            let (mut cells, mut done) = render(&view, &tiles, 210, 52, 1200.0);
            for _ in 0..200 {
                if done {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
                (cells, done) = render(&view, &tiles, 210, 52, 1200.0);
            }
            let t = std::time::Instant::now();
            let _ = render(&view, &tiles, 210, 52, 1200.0);
            println!("{name} : rendu en {:?}", t.elapsed());
            let text: String = cells.chunks(210).map(|row| row.iter().map(|c| c.ch).collect::<String>() + "\n").collect();
            std::fs::write(dir.join(format!("ascii-{name}.txt")), text).unwrap();
            // Colour picture: each character as a 4 × 8 block of its colour.
            let mut img = image::RgbImage::new(210 * 4, 52 * 8);
            for (i, c) in cells.iter().enumerate() {
                let (cx, cy) = ((i % 210) as u32, (i / 210) as u32);
                let fg = match c.fg {
                    Color::Rgb(r, g, b) if c.ch != ' ' => [r, g, b],
                    _ => [10, 12, 18],
                };
                let fg = if c.ch == ' ' { match c.bg { Some(Color::Rgb(r, g, b)) => [r, g, b], _ => fg } } else { fg };
                let bg = match c.bg {
                    Some(Color::Rgb(r, g, b)) => [r, g, b],
                    _ => [10, 12, 18],
                };
                for dy in 0..8 {
                    for dx in 0..4 {
                        let ink = c.ch != ' ' && (1..3).contains(&dx) && (2..6).contains(&dy);
                        img.put_pixel(cx * 4 + dx, cy * 8 + dy, image::Rgb(if ink { fg } else { bg }));
                    }
                }
            }
            img.save(dir.join(format!("ascii-{name}.png"))).unwrap();
        }
        println!("{} tuiles en échec", tiles.failures());
    }
}

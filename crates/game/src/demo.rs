//! Drive the physics car around a real place (OpenStreetMap) or the
//! generated world. The top line keeps the render measurements (frames per
//! second, render time, bytes sent to the terminal).

use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, MouseButton,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor, execute, terminal};
use glam::Vec3;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style as TextStyle};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::input::Keys;
use crate::lighting::{self, Headlights, Light};
use crate::physics::Car;
use crate::picker::{self, Choice, MapView};
use crate::tiles::{StreetTiles, VectorTiles};

/// Map sources for the place picker, and the look chosen (kept between
/// trips).
struct Maps {
    street: StreetTiles,
    vector: VectorTiles,
    ascii: std::cell::Cell<bool>,
}
use crate::present::{self, Style};
use crate::raster::{Camera, Frame};
use crate::map::MapData;
use crate::mesh;
use crate::world::World;

/// Frame rate targets cycled with `f` (0 = as fast as possible).
const TARGETS: [u32; 3] = [60, 30, 0];

/// Counts the bytes ratatui writes to the terminal.
struct Counting {
    inner: Stdout,
    bytes: Arc<AtomicU64>,
}

impl Write for Counting {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Chase,
    Driver,
}

/// Averages over the last second.
#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    render: Duration,
    write: Duration,
    bytes: u64,
    triangles: usize,
    shown: String,
}

impl Stats {
    fn record(&mut self, render: Duration, write: Duration, bytes: u64, triangles: usize) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.frames += 1;
        self.render += render;
        self.write += write;
        self.bytes += bytes;
        self.triangles = triangles;
        let elapsed = since.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let n = self.frames.max(1);
            let secs = elapsed.as_secs_f64();
            self.shown = format!(
                "{:.0} i/s · calcul {:.1} ms · écriture {:.1} ms · {:.0} Ko/image · {:.1} Mo/s · {} triangles",
                f64::from(self.frames) / secs,
                self.render.as_secs_f64() * 1000.0 / f64::from(n),
                self.write.as_secs_f64() * 1000.0 / f64::from(n),
                self.bytes as f64 / 1024.0 / f64::from(n),
                self.bytes as f64 / 1_048_576.0 / secs,
                self.triangles,
            );
            *self = Self { shown: std::mem::take(&mut self.shown), ..Self::default() };
        }
    }
}

/// Map size for real places (metres per side).
pub const PLACE_SIZE: f32 = 1200.0;

/// The map to drive on: a place name or "lat,lon", or the generated world.
pub fn load_map(place: Option<&str>) -> anyhow::Result<MapData> {
    let Some(query) = place else { return Ok(MapData::generate(7)) };
    println!("Recherche de « {query} »…");
    let (at, name) = crate::osm::locate(query)?;
    println!("{name} ({:.4}, {:.4})", at.lat, at.lon);
    crate::osm::load(at, &name, PLACE_SIZE, &|step| println!("  {step}"))
}

type Term = Terminal<CrosstermBackend<Counting>>;

/// The terminal set up for the game (raw mode, alternate screen, mouse,
/// kitty keyboard protocol when available), restored however it ends.
struct Session {
    /// Key releases are reported (precise pedals).
    releases: bool,
}

impl Session {
    fn start() -> anyhow::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
        let releases = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if releases {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )?;
        }
        Ok(Self { releases })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.releases {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(io::stdout(), DisableMouseCapture, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

/// The game: pick a place on the world map, drive there, `Échap` goes back
/// to the map. With `place`, the first trip skips the map.
pub fn run(place: Option<String>) -> anyhow::Result<()> {
    let session = Session::start()?;
    let bytes = Arc::new(AtomicU64::new(0));
    let mut term = Terminal::new(CrosstermBackend::new(Counting { inner: io::stdout(), bytes: bytes.clone() }))?;
    let maps = Maps { street: StreetTiles::street(), vector: VectorTiles::vector(), ascii: std::cell::Cell::new(true) };
    let mut view = MapView::default();
    let mut pending = place;
    loop {
        let choice = match pending.take() {
            Some(query) => {
                message(&mut term, &format!("Recherche de « {query} »…"), "")?;
                match crate::osm::locate(&query) {
                    Ok((at, name)) => Choice::Place { lat: at.lat, lon: at.lon, name },
                    Err(e) => {
                        view.status = Some(format!("{e:#}"));
                        continue;
                    }
                }
            }
            None => choose(&mut term, &maps, &mut view)?,
        };
        let (map, real) = match choice {
            Choice::Quit => return Ok(()),
            Choice::Generated => (MapData::generate(7), false),
            Choice::Place { lat, lon, name } => {
                // Back on the map after the drive, on this place.
                view = MapView { label: Some(name.clone()), ..MapView::centred(lat, lon, 15) };
                match loading(&mut term, crate::osm::Place { lat, lon }, &name) {
                    Ok(map) => (map, true),
                    Err(e) => {
                        view.status = Some(format!("{e:#}"));
                        continue;
                    }
                }
            }
        };
        drive(&mut term, &bytes, session.releases, &map, real)?;
    }
}

/// A centred message on an empty screen.
fn message(term: &mut Term, title: &str, detail: &str) -> anyhow::Result<()> {
    term.draw(|f| {
        let area = f.area();
        let buf = f.buffer_mut();
        let y = area.height / 2;
        for (dy, text, style) in [
            (0, title, TextStyle::default().fg(Color::Rgb(255, 214, 80)).add_modifier(Modifier::BOLD)),
            (2, detail, TextStyle::default().fg(Color::Gray)),
        ] {
            let w = text.chars().count() as u16;
            let x = area.width.saturating_sub(w) / 2;
            Line::styled(text, style).render(Rect { x, y: y + dy, width: w.min(area.width), height: 1 }, buf);
        }
    })?;
    Ok(())
}

/// The street map until the player picks somewhere.
fn choose(term: &mut Term, maps: &Maps, view: &mut MapView) -> anyhow::Result<Choice> {
    let tiles = &maps.street;
    let bar = TextStyle::default().fg(Color::White).bg(Color::Rgb(20, 22, 30));
    // The ASCII map by default; `i` shows the map as a picture (a real one
    // where the terminal can, half blocks elsewhere).
    let gfx = picker::image_picker();
    let (picture_w, picture_h) = gfx.as_ref().map_or((2, 4), |p| (u32::from(p.font_size().width), u32::from(p.font_size().height)));
    let mut ascii_cells: Option<Vec<crate::ascii_map::Cell>> = None;
    // What the shown picture was made from, to rebuild it only on change.
    let mut shown: Option<(f64, f64, u8, u64, Rect)> = None;
    // Half blocks are drawn from the composed picture directly; pictures
    // are encoded (about 200 ms for a full screen of Sixel) on another
    // thread, which only ever encodes the latest view.
    let mut blocks: Option<image::RgbaImage> = None;
    let mut protocol: Option<ratatui_image::protocol::Protocol> = None;
    let (job_tx, job_rx) = std::sync::mpsc::channel::<(image::RgbaImage, Rect)>();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    if let Some(p) = gfx.clone() {
        std::thread::spawn(move || {
            while let Ok(mut job) = job_rx.recv() {
                while let Ok(newer) = job_rx.try_recv() {
                    job = newer;
                }
                let encoded = p.new_protocol(image::DynamicImage::ImageRgba8(job.0), job.1.into(), ratatui_image::Resize::Fit(None));
                if let Ok(e) = encoded
                    && done_tx.send(e).is_err()
                {
                    break;
                }
            }
        });
    }
    let mut composed_at = Instant::now();
    loop {
        let ascii = maps.ascii.get();
        let (cell_w, cell_h) = if ascii { (crate::ascii_map::CELL_W as u32, crate::ascii_map::CELL_H as u32) } else { (picture_w, picture_h) };
        let size = term.size()?;
        let map_area = Rect { x: 0, y: 1, width: size.width, height: size.height.saturating_sub(2) };
        let generation = if ascii { maps.vector.generation() } else { tiles.generation() } * 2 + u64::from(ascii);
        let key = (view.x, view.y, view.zoom, generation, map_area);
        // Tiles arriving one by one: recompose at most every 120 ms.
        let moved = shown.is_none_or(|s| (s.0, s.1, s.2, s.4) != (key.0, key.1, key.2, key.4));
        if (moved || shown.is_some_and(|s| s.3 != key.3) && composed_at.elapsed() > Duration::from_millis(120))
            && map_area.width > 0
            && map_area.height > 0
        {
            if ascii {
                ascii_cells = Some(crate::ascii_map::render(view, &maps.vector, map_area.width, map_area.height, PLACE_SIZE).0);
            } else {
                let (w, h) = (u32::from(map_area.width) * cell_w, u32::from(map_area.height) * cell_h);
                let (img, _) = picker::compose(view, tiles, w, h, PLACE_SIZE);
                if gfx.is_some() {
                    let _ = job_tx.send((img, map_area));
                } else {
                    blocks = Some(img);
                }
            }
            shown = Some(key);
            composed_at = Instant::now();
        }

        let (lat, lon) = view.centre();
        let across = view.metres_per_px() * f64::from(map_area.width) * f64::from(cell_w);
        let top = match (&view.status, &view.label) {
            (Some(status), _) => format!(" {status} "),
            (None, label) => {
                let failures = if ascii { maps.vector.failures() } else { tiles.failures() };
                let failed = if failures > 0 { " · carte en partie indisponible (hors ligne ?)" } else { "" };
                format!(
                    " {}{} · largeur affichée {}{failed} · cadre : zone de {} ",
                    label.as_ref().map(|l| format!("{l} · ")).unwrap_or_default(),
                    picker::format_position(lat, lon),
                    picker::format_distance(across),
                    picker::format_distance(f64::from(PLACE_SIZE)),
                )
            }
        };
        let bottom = match &view.search {
            Some(text) => format!(" Chercher un lieu : {text}▏  (Entrée valider, Échap annuler)"),
            None => " ↑↓←→/ZQSD déplacer · +/- ou molette zoom · clic centrer · / chercher · i ascii/image · Entrée rouler ici · g monde généré · Échap ".into(),
        };
        while let Ok(encoded) = done_rx.try_recv() {
            protocol = Some(encoded);
        }
        term.draw(|f| {
            let full = Rect { x: 0, y: 0, width: size.width, height: 1 };
            if ascii {
                if let Some(cells) = &ascii_cells {
                    crate::ascii_map::draw(cells, map_area, f.buffer_mut());
                }
            } else if let Some(p) = &protocol {
                f.render_widget(ratatui_image::Image::new(p), map_area);
            } else if let Some(img) = &blocks {
                picker::draw_blocks(img, map_area, f.buffer_mut());
            }
            let buf = f.buffer_mut();
            Line::styled(top.as_str(), bar).render(full, buf);
            let last = Rect { y: size.height.saturating_sub(1), ..full };
            Line::styled(bottom.as_str(), bar).render(last, buf);
            // Required by the map providers: always visible.
            let credit = if ascii { " OpenFreeMap © OpenMapTiles © contributeurs OpenStreetMap " } else { " © les contributeurs d'OpenStreetMap " };
            Line::styled(credit, bar.fg(Color::Rgb(160, 200, 255))).right_aligned().render(last, buf);
        })?;

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let step_x = f64::from(map_area.width) * f64::from(cell_w) / 4.0;
        let step_y = f64::from(map_area.height) * f64::from(cell_h) / 4.0;
        match event::read()? {
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                if let Some(text) = view.search.as_mut() {
                    match k.code {
                        KeyCode::Esc => view.search = None,
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        KeyCode::Char(c) => text.push(c),
                        KeyCode::Enter => {
                            let query = view.search.take().unwrap_or_default();
                            if query.trim().is_empty() {
                                continue;
                            }
                            message(term, &format!("Recherche de « {query} »…"), "")?;
                            shown = None;
                            match crate::osm::locate(&query) {
                                Ok((at, name)) => *view = MapView { label: Some(name), ..MapView::centred(at.lat, at.lon, 15) },
                                Err(e) => view.status = Some(format!("{e:#}")),
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                view.status = None;
                let mut pan = |dx: f64, dy: f64| {
                    view.pan_px(dx, dy);
                    view.label = None;
                };
                match k.code {
                    KeyCode::Esc => return Ok(Choice::Quit),
                    KeyCode::Enter => {
                        let (lat, lon) = view.centre();
                        let name = view.label.clone().unwrap_or_else(|| picker::format_position(lat, lon));
                        return Ok(Choice::Place { lat, lon, name });
                    }
                    KeyCode::Char('g' | 'G') => return Ok(Choice::Generated),
                    KeyCode::Char('i' | 'I') => {
                        maps.ascii.set(!ascii);
                        shown = None;
                    }
                    KeyCode::Char('/') => view.search = Some(String::new()),
                    KeyCode::Char('+' | '=') | KeyCode::PageUp => view.zoom_by(1),
                    KeyCode::Char('-') | KeyCode::PageDown => view.zoom_by(-1),
                    KeyCode::Up | KeyCode::Char('z' | 'w') => pan(0.0, -step_y),
                    KeyCode::Down | KeyCode::Char('s') => pan(0.0, step_y),
                    KeyCode::Left | KeyCode::Char('q' | 'a') => pan(-step_x, 0.0),
                    KeyCode::Right | KeyCode::Char('d') => pan(step_x, 0.0),
                    _ => {}
                }
            }
            Event::Mouse(m) if m.row >= map_area.y && m.row < map_area.y + map_area.height => {
                // Offset of the click from the centre, in map pixels.
                let dx = (f64::from(m.column) + 0.5 - f64::from(map_area.width) / 2.0) * f64::from(cell_w);
                let dy = (f64::from(m.row - map_area.y) + 0.5 - f64::from(map_area.height) / 2.0) * f64::from(cell_h);
                match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        view.pan_px(dx, dy);
                        view.label = None;
                    }
                    MouseEventKind::ScrollUp => view.zoom_by(1),
                    MouseEventKind::ScrollDown => view.zoom_by(-1),
                    _ => {}
                }
            }
            Event::Resize(..) => shown = None,
            _ => {}
        }
    }
}

/// Downloads and builds a real place, with a progress screen.
fn loading(term: &mut Term, place: crate::osm::Place, name: &str) -> anyhow::Result<MapData> {
    let progress = Arc::new(std::sync::Mutex::new(String::from("…")));
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let (progress, name) = (progress.clone(), name.to_owned());
        std::thread::spawn(move || {
            let result = crate::osm::load(place, &name, PLACE_SIZE, &|step| *progress.lock().unwrap() = step.to_owned());
            let _ = tx.send(result);
        });
    }
    let started = Instant::now();
    let spinner = ['|', '/', '-', '\\'];
    loop {
        if let Ok(result) = rx.try_recv() {
            return result;
        }
        let step = progress.lock().unwrap().clone();
        let tick = (started.elapsed().as_millis() / 150) as usize % 4;
        message(
            term,
            &format!("{} Chargement de {name}", spinner[tick]),
            &format!("{step}  ({} s ; la première fois peut prendre une minute, ensuite c'est en cache)", started.elapsed().as_secs()),
        )?;
        // Keys typed meanwhile are dropped.
        while event::poll(Duration::from_millis(100))? {
            let _ = event::read()?;
        }
    }
}

/// Drives on `map` until `Échap`.
fn drive(term: &mut Term, bytes: &Arc<AtomicU64>, releases: bool, map: &MapData, real: bool) -> anyhow::Result<()> {
    let generated = Instant::now();
    let world = World::build(map);
    // Start on the road nearest the centre (real place) or the main loop.
    let start = if real { world.nearest_road(Vec3::new(world.size / 2.0, 0.0, world.size / 2.0)) } else { world.on_road(0.0) };
    let mut car = Car::new(&world.solids, start.pos + Vec3::Y * 1.0, start.dir);
    let generated = generated.elapsed();

    let mut keys = Keys::new(releases);
    let mut style = Style::Color;
    let mut view = View::Chase;
    let mut target = 0usize;
    let mut frame = Frame::new(1, 1);
    let mut stats = Stats {
        shown: format!(
            "{} : construit en {} ms, {} triangles, {} arbres",
            world.name,
            generated.as_millis(),
            world.triangle_count(),
            world.tree_count()
        ),
        ..Stats::default()
    };
    let mut last = Instant::now();
    let begin = last;
    let mut eye = start.pos - start.dir * 9.0 + Vec3::Y * 4.0;
    // Late afternoon: long shadows. `y` lets the day run (24 h in 4 min).
    let mut hours = 16.5f32;
    let mut day_runs = false;
    // None = automatic (on when it gets dark).
    let mut lamps: Option<bool> = None;

    loop {
        let started = Instant::now();
        while event::poll(Duration::ZERO)? {
            let Event::Key(k) = event::read()? else { continue };
            if keys.on_key(k) || k.kind == KeyEventKind::Release {
                continue;
            }
            match k.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Char('c' | 'C') => style = style.next(),
                KeyCode::Char('v' | 'V') => view = if view == View::Chase { View::Driver } else { View::Chase },
                KeyCode::Char('f' | 'F') => target = (target + 1) % TARGETS.len(),
                KeyCode::Char('t') => hours = (hours + 1.0).rem_euclid(24.0),
                KeyCode::Char('T') => hours = (hours - 1.0).rem_euclid(24.0),
                KeyCode::Char('y' | 'Y') => day_runs = !day_runs,
                KeyCode::Char('l' | 'L') => {
                    lamps = match lamps {
                        None => Some(true),
                        Some(true) => Some(false),
                        Some(false) => None,
                    }
                }
                KeyCode::Char('r' | 'R') => {
                    let back = world.nearest_road(car.pose().position);
                    car.reset(back.pos + Vec3::Y * 1.0, back.dir);
                }
                _ => {}
            }
        }

        let dt = last.elapsed().as_secs_f32().min(0.1);
        last = Instant::now();
        car.update(keys.controls(), dt);
        if day_runs {
            hours = (hours + dt * 24.0 / 240.0).rem_euclid(24.0);
        }
        let pose = car.pose();
        let forward = pose.forward();
        let mut light = Light::at(hours);
        let lamps_on = lamps.unwrap_or_else(|| light.is_dark());
        if lamps_on {
            let lamp = |z: f32| pose.position + pose.rotation * Vec3::new(crate::physics::HALF_LENGTH, 0.35, z);
            light.headlights = Some(Headlights { lamps: [lamp(-0.62), lamp(0.62)], dir: (forward - Vec3::Y * 0.1).normalize() });
        }
        let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::X);

        let camera = match view {
            View::Chase => {
                // The camera trails the car with some lag: turns and bumps
                // read as motion instead of a fixed picture.
                let wanted = pose.position - flat * 8.5 + Vec3::Y * 3.4;
                eye = eye.lerp(wanted, 1.0 - (-dt * 6.0).exp());
                Camera { eye, target: pose.position + flat * 6.0 + Vec3::Y * 1.0, fov_y: 1.05 }
            }
            View::Driver => {
                let seat = pose.position + pose.rotation * Vec3::new(0.1, 0.95, 0.0);
                eye = seat;
                Camera { eye: seat, target: seat + forward * 20.0 - Vec3::Y * 1.2, fov_y: 1.15 }
            }
        };

        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let rows = style.rows_per_cell();
        frame.resize(area.width as usize, area.height as usize * rows, if rows == 2 { 1.0 } else { 2.0 });
        let render_start = Instant::now();
        let car_mesh = mesh::car(&pose, [200, 40, 48]);
        let mut meshes: Vec<&[mesh::Triangle]> = Vec::new();
        if view == View::Chase {
            meshes.push(car_mesh.tris.as_slice());
        }
        light.time = started.duration_since(begin).as_secs_f32();
        let triangles = frame.render(&camera, &light, &world, &meshes);
        let render = render_start.elapsed();

        let target_fps = TARGETS[target];
        let header = format!(
            " {} {}{} · phares {} · {} · cible {} · {} ",
            lighting::clock(hours),
            if day_runs { "▶ " } else { "" },
            if light.is_dark() { "nuit" } else { "jour" },
            match lamps {
                None if lamps_on => "auto (allumés)",
                None => "auto",
                Some(true) => "allumés",
                Some(false) => "éteints",
            },
            stats.shown,
            if target_fps == 0 { "max".to_owned() } else { format!("{target_fps} i/s") },
            style.label(),
        );
        let help = if releases {
            " ↑↓←→ / ZQSD conduire · espace frein à main · r route · t/T heure ±1 h · y jour qui défile · l phares · c style · v caméra · f i/s · Échap "
        } else {
            " ↑↓←→ / ZQSD conduire (terminal sans relâchement des touches : conduite moins précise) · r route · c style · v caméra · Échap "
        };
        let speed = car.forward_speed() * 3.6;
        let gauge = format!(" {}{:>3.0} km/h ", if speed < -1.0 { "R " } else { "" }, speed.abs());
        let before = bytes.load(Ordering::Relaxed);
        let write_start = Instant::now();
        term.draw(|f| {
            let buf = f.buffer_mut();
            present::draw(&frame, style, area, buf);
            let bar = TextStyle::default().fg(Color::White).bg(Color::Black);
            Line::styled(header.as_str(), bar).render(Rect { height: 1, ..area }, buf);
            if area.height > 3 {
                let bottom = Rect { y: area.height - 1, height: 1, ..area };
                Line::styled(help, bar).render(bottom, buf);
                let w = gauge.chars().count() as u16;
                if area.width > w {
                    let speedo = Rect { x: area.width - w, y: area.height - 2, width: w, height: 1 };
                    let big = TextStyle::default().fg(Color::Black).bg(Color::Rgb(240, 200, 60)).add_modifier(Modifier::BOLD);
                    Line::styled(gauge.as_str(), big).render(speedo, buf);
                }
            }
        })?;
        let write = write_start.elapsed();
        stats.record(render, write, bytes.load(Ordering::Relaxed) - before, triangles);

        if target_fps > 0 {
            let budget = Duration::from_secs_f64(1.0 / f64::from(target_fps));
            if let Some(rest) = budget.checked_sub(started.elapsed()) {
                std::thread::sleep(rest);
            }
        }
    }
}

/// Headless measurement: renders `frames` images of a `cols` × `rows`
/// terminal along the road and returns (generation ms, render ms per
/// image, bytes per image) for each style.
pub fn bench(cols: u16, rows: u16, frames: u32) -> anyhow::Result<Vec<(Style, f64, f64, f64)>> {
    let world = World::build(&MapData::generate(7));
    let mut out = Vec::new();
    for style in [Style::Color, Style::Ascii] {
        let sink = Arc::new(AtomicU64::new(0));
        let backend = CrosstermBackend::new(CountingSink(sink.clone()));
        let mut term = Terminal::with_options(
            backend,
            ratatui::TerminalOptions { viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, cols, rows)) },
        )?;
        let area = Rect::new(0, 0, cols, rows);
        let rows_px = style.rows_per_cell();
        let mut frame = Frame::new(1, 1);
        let mut render = Duration::ZERO;
        let mut worst = Duration::ZERO;
        let mut bytes = 0;
        for i in 0..frames {
            // 22 m/s at 30 frames per second.
            let here = world.on_road(i as f32 * 22.0 / 30.0);
            let pose = crate::physics::CarPose {
                position: here.pos + Vec3::Y * 0.6,
                rotation: glam::Quat::from_rotation_arc(Vec3::X, here.dir),
                wheels: Vec::new(),
            };
            let car = mesh::car(&pose, [200, 40, 48]);
            let camera = Camera {
                eye: here.pos - here.dir * 9.0 + Vec3::Y * 3.6,
                target: here.pos + here.dir * 10.0 + Vec3::Y * 1.0,
                fov_y: 1.0,
            };
            frame.resize(cols as usize, rows as usize * rows_px, if rows_px == 2 { 1.0 } else { 2.0 });
            let t = Instant::now();
            frame.render(&camera, &Light::at(16.5), &world, &[car.tris.as_slice()]);
            render += t.elapsed();
            if i > 0 {
                worst = worst.max(t.elapsed());
            }
            let before = sink.load(Ordering::Relaxed);
            term.draw(|f| present::draw(&frame, style, area, f.buffer_mut()))?;
            if i > 0 {
                bytes += sink.load(Ordering::Relaxed) - before;
            }
        }
        let n = f64::from(frames);
        out.push((style, render.as_secs_f64() * 1000.0 / n, worst.as_secs_f64() * 1000.0, bytes as f64 / (n - 1.0)));
    }
    Ok(out)
}

struct CountingSink(Arc<AtomicU64>);

impl Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "measurement: cargo test -p game --release -- --ignored --nocapture"]
    fn measure_full_screen() {
        let t = std::time::Instant::now();
        let results = super::bench(210, 55, 300).unwrap();
        println!("total {:.1} s", t.elapsed().as_secs_f64());
        for (style, ms, worst, bytes) in results {
            println!(
                "{}: calcul {ms:.2} ms/image (pire {worst:.1} ms), {:.0} Ko/image, {:.1} Mo/s à 30 i/s",
                style.label(),
                bytes / 1024.0,
                bytes * 30.0 / 1_048_576.0
            );
        }
    }
}

#[cfg(test)]
mod light_snapshots {
    use super::*;
    use crate::physics::{Controls, HALF_LENGTH, STEP};

    #[test]
    #[ignore = "writes $SNAPSHOT_DIR/light-*.ppm"]
    fn times_of_day() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let map = match std::env::var("SNAPSHOT_PLACE") {
            Ok(place) => load_map(Some(&place)).unwrap(),
            Err(_) => MapData::generate(7),
        };
        let world = World::build(&map);
        let start = match std::env::var("SNAPSHOT_PLACE") {
            Ok(_) => world.nearest_road(Vec3::new(world.size / 2.0, 0.0, world.size / 2.0)),
            Err(_) => world.on_road(300.0),
        };
        let mut car = Car::new(&world.solids, start.pos + Vec3::Y, start.dir);
        for _ in 0..90 {
            car.update(Controls { throttle: 0.4, steer: 0.0, handbrake: false }, STEP);
        }
        let pose = car.pose();
        let flat = Vec3::new(pose.forward().x, 0.0, pose.forward().z).normalize();
        let camera = Camera { eye: pose.position - flat * 8.5 + Vec3::Y * 3.4, target: pose.position + flat * 6.0 + Vec3::Y, fov_y: 1.05 };
        let mesh = mesh::car(&pose, [200, 40, 48]);
        for hours in [9.0f32, 16.5, 17.6, 18.1, 23.0] {
            let mut light = Light::at(hours);
            if light.is_dark() {
                let lamp = |z: f32| pose.position + pose.rotation * Vec3::new(HALF_LENGTH, 0.35, z);
                light.headlights = Some(Headlights { lamps: [lamp(-0.62), lamp(0.62)], dir: (pose.forward() - Vec3::Y * 0.1).normalize() });
            }
            let mut frame = Frame::new(210, 110);
            let t = Instant::now();
            frame.render(&camera, &light, &world, &[mesh.tris.as_slice()]);
            println!("{hours} h : {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
            let mut ppm = format!("P6 {} {} 255\n", frame.w, frame.h).into_bytes();
            frame.color.iter().for_each(|c| ppm.extend_from_slice(c));
            std::fs::write(dir.join(format!("light-{hours}.ppm")), ppm).unwrap();
        }
    }
}

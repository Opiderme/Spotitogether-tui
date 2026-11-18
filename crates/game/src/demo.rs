//! Render benchmark: the camera drives around the generated world while the
//! top line shows frames per second, render time and the bytes sent to the
//! terminal (the real limit of a terminal game).

use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::{cursor, execute, terminal};
use glam::Vec3;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style as TextStyle};
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::present::{self, Style};
use crate::raster::{Camera, Frame};
use crate::world::{self, World};

/// Frame rate targets cycled with `f` (0 = as fast as possible).
const TARGETS: [u32; 4] = [30, 60, 20, 0];

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

/// Restores the terminal however the demo ends.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
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

pub fn run() -> anyhow::Result<()> {
    let generated = Instant::now();
    let world = World::generate(7);
    let generated = generated.elapsed();

    terminal::enable_raw_mode()?;
    let _restore = Restore;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let bytes = Arc::new(AtomicU64::new(0));
    let mut term = Terminal::new(CrosstermBackend::new(Counting { inner: io::stdout(), bytes: bytes.clone() }))?;

    let mut style = Style::Color;
    let mut view = View::Chase;
    let mut target = 0usize;
    let mut speed = 22.0f32; // m/s, about 80 km/h
    let mut paused = false;
    let mut distance = 0.0f32;
    let mut frame = Frame::new(1, 1);
    let mut stats = Stats {
        shown: format!("monde généré en {} ms, {} triangles", generated.as_millis(), world.mesh.tris.len()),
        ..Stats::default()
    };
    let mut last = Instant::now();

    loop {
        let started = Instant::now();
        while event::poll(Duration::ZERO)? {
            if let Event::Key(k) = event::read()?
                && k.kind != KeyEventKind::Release
            {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') => style = style.next(),
                    KeyCode::Char('v') => view = if view == View::Chase { View::Driver } else { View::Chase },
                    KeyCode::Char('f') => target = (target + 1) % TARGETS.len(),
                    KeyCode::Char(' ') => paused = !paused,
                    KeyCode::Up => speed = (speed + 5.0).min(70.0),
                    KeyCode::Down => speed = (speed - 5.0).max(0.0),
                    _ => {}
                }
            }
        }

        let dt = last.elapsed().as_secs_f32().min(0.1);
        last = Instant::now();
        if !paused {
            distance += speed * dt;
        }
        let here = world.on_road(distance);
        let car = world::car(here.pos, here.dir, [200, 40, 48]);
        let camera = match view {
            View::Chase => Camera {
                eye: here.pos - here.dir * 9.0 + Vec3::Y * 3.6,
                target: here.pos + here.dir * 10.0 + Vec3::Y * 1.0,
                fov_y: 1.0,
            },
            View::Driver => Camera {
                eye: here.pos + Vec3::Y * 1.25 + here.dir * 0.3,
                target: here.pos + here.dir * 20.0 + Vec3::Y * 0.9,
                fov_y: 1.0,
            },
        };

        let size = term.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let rows = style.rows_per_cell();
        let aspect = if rows == 2 { 1.0 } else { 2.0 };
        frame.resize(area.width as usize, area.height as usize * rows, aspect);
        let render_start = Instant::now();
        let mut meshes = vec![world.mesh.tris.as_slice()];
        if view == View::Chase {
            meshes.push(car.tris.as_slice());
        }
        let triangles = frame.render(&camera, meshes);
        let render = render_start.elapsed();

        let target_fps = TARGETS[target];
        let header = format!(
            " {} · cible {} · {} · {:.0} km/h ",
            stats.shown,
            if target_fps == 0 { "max".to_owned() } else { format!("{target_fps} i/s") },
            style.label(),
            speed * 3.6
        );
        let help = " c style · v caméra · ↑↓ vitesse · f cible i/s · espace pause · q quitter ";
        let before = bytes.load(Ordering::Relaxed);
        let write_start = Instant::now();
        term.draw(|f| {
            let buf = f.buffer_mut();
            present::draw(&frame, style, area, buf);
            let bar = TextStyle::default().fg(Color::White).bg(Color::Black);
            Line::styled(header.as_str(), bar).render(Rect { height: 1, ..area }, buf);
            if area.height > 2 {
                Line::styled(help, bar).render(Rect { y: area.height - 1, height: 1, ..area }, buf);
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
pub fn bench(cols: u16, rows: u16, frames: u32) -> anyhow::Result<Vec<(Style, f64, f64)>> {
    let world = World::generate(7);
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
        let mut bytes = 0;
        for i in 0..frames {
            // 22 m/s at 30 frames per second.
            let here = world.on_road(i as f32 * 22.0 / 30.0);
            let car = world::car(here.pos, here.dir, [200, 40, 48]);
            let camera = Camera {
                eye: here.pos - here.dir * 9.0 + Vec3::Y * 3.6,
                target: here.pos + here.dir * 10.0 + Vec3::Y * 1.0,
                fov_y: 1.0,
            };
            frame.resize(cols as usize, rows as usize * rows_px, if rows_px == 2 { 1.0 } else { 2.0 });
            let t = Instant::now();
            frame.render(&camera, [world.mesh.tris.as_slice(), car.tris.as_slice()]);
            render += t.elapsed();
            let before = sink.load(Ordering::Relaxed);
            term.draw(|f| present::draw(&frame, style, area, f.buffer_mut()))?;
            if i > 0 {
                bytes += sink.load(Ordering::Relaxed) - before;
            }
        }
        let n = f64::from(frames);
        out.push((style, render.as_secs_f64() * 1000.0 / n, bytes as f64 / (n - 1.0)));
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
        for (style, ms, bytes) in results {
            println!("{}: calcul {ms:.2} ms/image, {:.0} Ko/image, {:.1} Mo/s à 30 i/s", style.label(), bytes / 1024.0, bytes * 30.0 / 1_048_576.0);
        }
    }
}

#[cfg(test)]
mod snapshot {
    use super::*;

    /// Writes a few views as PPM images and the ASCII look as text, to look
    /// at the renderer without a terminal.
    #[test]
    #[ignore = "writes files to $SNAPSHOT_DIR"]
    fn snapshots() {
        let dir = std::path::PathBuf::from(std::env::var("SNAPSHOT_DIR").unwrap());
        let world = World::generate(7);
        for (k, s) in [0.0f32, 400.0, 900.0].into_iter().enumerate() {
            let here = world.on_road(s);
            let car = world::car(here.pos, here.dir, [200, 40, 48]);
            let camera = Camera {
                eye: here.pos - here.dir * 9.0 + Vec3::Y * 3.6,
                target: here.pos + here.dir * 10.0 + Vec3::Y * 1.0,
                fov_y: 1.0,
            };
            let mut frame = Frame::new(210, 110);
            frame.render(&camera, [world.mesh.tris.as_slice(), car.tris.as_slice()]);
            let mut ppm = format!("P6 {} {} 255\n", frame.w, frame.h).into_bytes();
            frame.color.iter().for_each(|c| ppm.extend_from_slice(c));
            std::fs::write(dir.join(format!("view{k}.ppm")), ppm).unwrap();

            let mut ascii = Frame::new(1, 1);
            ascii.resize(210, 55, 2.0);
            ascii.render(&camera, [world.mesh.tris.as_slice(), car.tris.as_slice()]);
            let area = Rect::new(0, 0, 210, 55);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            present::draw(&ascii, Style::Ascii, area, &mut buf);
            let text: String = (0..55)
                .map(|y| (0..210).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>() + "\n")
                .collect();
            std::fs::write(dir.join(format!("view{k}.txt")), text).unwrap();
        }
    }
}

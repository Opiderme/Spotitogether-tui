//! Map tiles, 256 × 256 pixels, zoom 0 (the world in one tile) and up:
//! - elevation (Terrain Tiles on AWS open data), read while building a
//!   world;
//! - the OpenStreetMap street map, as pictures (osm.org) or as vector data
//!   (OpenFreeMap, drawn in ASCII art), shown by the place picker and loaded
//!   in the background.
//!
//! All are cached on disk. OpenStreetMap's tile usage policy is followed:
//! an application User-Agent, two connections at most, only the tiles on
//! screen, tiles kept at least 7 days (30 here), the attribution shown.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::Context;
use image::RgbaImage;

const ELEVATION_URL: &str = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium";
/// Overridable with `SPOTITOGETHER_MAP_TILES` (`{z}`, `{x}`, `{y}`).
const STREET_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
/// OpenFreeMap: free vector tiles (OpenMapTiles schema), zoom 0–14. The
/// description gives the current versioned tile address.
const VECTOR_TILEJSON: &str = "https://tiles.openfreemap.org/planet";
pub const VECTOR_MAX_ZOOM: u8 = 14;
pub const USER_AGENT: &str = "Spotitogether/0.1.0";
/// Street tiles are downloaded again after this long.
const STREET_FRESH: Duration = Duration::from_secs(30 * 24 * 3600);
/// Requests beyond this are dropped (the view moved on).
const QUEUE: usize = 48;

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(90))).build().into()
}

pub fn cache_dir() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "spotitogether")?;
    Some(dirs.cache_dir().join("drive"))
}

fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    Ok(agent().get(url).header("User-Agent", USER_AGENT).call()?.body_mut().read_to_vec()?)
}

/// Elevation tile: heights in metres, row by row (256 × 256).
pub fn fetch(z: u8, x: u32, y: u32) -> anyhow::Result<Vec<f32>> {
    let path = cache_dir().map(|d| d.join("tiles").join(format!("{z}_{x}_{y}.png")));
    let bytes = match path.as_ref().and_then(|p| std::fs::read(p).ok()) {
        Some(b) => b,
        None => {
            let b = download(&format!("{ELEVATION_URL}/{z}/{x}/{y}.png")).context("téléchargement du relief")?;
            if let Some(p) = &path {
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                let _ = std::fs::write(p, &b);
            }
            b
        }
    };
    let img = image::load_from_memory(&bytes)?.to_rgb8();
    Ok(img.pixels().map(|p| f32::from(p[0]) * 256.0 + f32::from(p[1]) + f32::from(p[2]) / 256.0 - 32768.0).collect())
}

/// Street map tile (OpenStreetMap's standard style).
pub fn fetch_street(z: u8, x: u32, y: u32) -> anyhow::Result<RgbaImage> {
    let path = cache_dir().map(|d| d.join("street").join(format!("{z}_{x}_{y}.png")));
    let cached = path.as_ref().and_then(|p| {
        let age = std::fs::metadata(p).and_then(|m| m.modified()).ok()?.elapsed().ok()?;
        Some((std::fs::read(p).ok()?, age < STREET_FRESH))
    });
    let bytes = match cached {
        Some((b, true)) => b,
        stale => {
            let url = std::env::var("SPOTITOGETHER_MAP_TILES").unwrap_or_else(|_| STREET_URL.into());
            let url = url.replace("{z}", &z.to_string()).replace("{x}", &x.to_string()).replace("{y}", &y.to_string());
            match download(&url) {
                Ok(b) => {
                    if let Some(p) = &path {
                        let _ = std::fs::create_dir_all(p.parent().unwrap());
                        let _ = std::fs::write(p, &b);
                    }
                    b
                }
                // Offline: an old tile is better than none.
                Err(e) => match stale {
                    Some((b, false)) => b,
                    _ => return Err(e.context("téléchargement de la carte")),
                },
            }
        }
    };
    Ok(image::load_from_memory(&bytes)?.to_rgba8())
}

/// Tile address template from OpenFreeMap's description, read once.
fn vector_template() -> anyhow::Result<String> {
    static TEMPLATE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(t) = TEMPLATE.get() {
        return Ok(t.clone());
    }
    let text = agent()
        .get(VECTOR_TILEJSON)
        .header("User-Agent", USER_AGENT)
        .call()
        .context("description de la carte (OpenFreeMap)")?
        .body_mut()
        .read_to_string()?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let template = json["tiles"][0].as_str().context("description de la carte inattendue")?.to_owned();
    Ok(TEMPLATE.get_or_init(|| template).clone())
}

/// Vector map tile (roads, water, land use, buildings, names).
pub fn fetch_vector(z: u8, x: u32, y: u32) -> anyhow::Result<crate::mvt::VectorTile> {
    let path = cache_dir().map(|d| d.join("vector").join(format!("{z}_{x}_{y}.pbf")));
    let cached = path.as_ref().and_then(|p| {
        let age = std::fs::metadata(p).and_then(|m| m.modified()).ok()?.elapsed().ok()?;
        Some((std::fs::read(p).ok()?, age < STREET_FRESH))
    });
    let bytes = match cached {
        Some((b, true)) => b,
        stale => {
            let url = vector_template().and_then(|t| {
                let url = t.replace("{z}", &z.to_string()).replace("{x}", &x.to_string()).replace("{y}", &y.to_string());
                download(&url)
            });
            match url {
                Ok(b) => {
                    if let Some(p) = &path {
                        let _ = std::fs::create_dir_all(p.parent().unwrap());
                        let _ = std::fs::write(p, &b);
                    }
                    b
                }
                Err(e) => match stale {
                    Some((b, false)) => b,
                    _ => return Err(e.context("téléchargement de la carte")),
                },
            }
        }
    };
    crate::mvt::decode(&bytes).map_err(|e| anyhow::anyhow!("tuile de carte illisible : {e}"))
}

type Key = (u8, u32, u32);

enum Slot<T> {
    Pending,
    Ready(Arc<T>),
    Failed,
}

struct Shared<T> {
    slots: Mutex<HashMap<Key, Slot<T>>>,
    queue: Mutex<VecDeque<Key>>,
    wake: Condvar,
    /// Bumped whenever a tile arrives (time to redraw).
    generation: AtomicU64,
    failures: AtomicU64,
}

/// Tiles loaded in the background, most recently asked first.
pub struct TileCache<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for TileCache<T> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

/// The street map, two downloads at a time.
pub type StreetTiles = TileCache<RgbaImage>;

impl StreetTiles {
    pub fn street() -> Self {
        Self::new(2, fetch_street)
    }
}

/// The vector map, four downloads at a time (OpenFreeMap's limit).
pub type VectorTiles = TileCache<crate::mvt::VectorTile>;

impl VectorTiles {
    pub fn vector() -> Self {
        Self::new(4, fetch_vector)
    }
}

impl<T: Send + Sync + 'static> TileCache<T> {
    pub fn new(workers: usize, fetch: fn(u8, u32, u32) -> anyhow::Result<T>) -> Self {
        let shared = Arc::new(Shared {
            slots: Mutex::new(HashMap::new()),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        });
        for _ in 0..workers {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("tiles".into())
                .spawn(move || worker(&shared, fetch))
                .expect("tile thread");
        }
        Self { shared }
    }

    /// The tile if it is there; otherwise asks for it and returns None.
    pub fn get(&self, z: u8, x: u32, y: u32) -> Option<Arc<T>> {
        let key = (z, x, y);
        let mut slots = self.shared.slots.lock().unwrap();
        match slots.get(&key) {
            Some(Slot::Ready(t)) => return Some(t.clone()),
            Some(Slot::Pending | Slot::Failed) => return None,
            None => {}
        }
        slots.insert(key, Slot::Pending);
        drop(slots);
        let mut queue = self.shared.queue.lock().unwrap();
        queue.push_front(key);
        while queue.len() > QUEUE {
            if let Some(old) = queue.pop_back() {
                // Forgotten: asked again if it comes back into view.
                self.shared.slots.lock().unwrap().remove(&old);
            }
        }
        self.shared.wake.notify_one();
        None
    }

    /// The tile if it is already there, without asking for it.
    pub fn peek(&self, z: u8, x: u32, y: u32) -> Option<Arc<T>> {
        match self.shared.slots.lock().unwrap().get(&(z, x, y)) {
            Some(Slot::Ready(t)) => Some(t.clone()),
            _ => None,
        }
    }

    /// Changes whenever a tile arrives.
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Relaxed)
    }

    /// Tiles asked for and not arrived yet.
    pub fn pending(&self) -> usize {
        self.shared.slots.lock().unwrap().values().filter(|s| matches!(s, Slot::Pending)).count()
    }

    pub fn failures(&self) -> u64 {
        self.shared.failures.load(Ordering::Relaxed)
    }
}

fn worker<T>(shared: &Shared<T>, fetch: fn(u8, u32, u32) -> anyhow::Result<T>) {
    loop {
        let key = {
            let mut queue = shared.queue.lock().unwrap();
            loop {
                if let Some(k) = queue.pop_front() {
                    break k;
                }
                queue = shared.wake.wait(queue).unwrap();
            }
        };
        let slot = match fetch(key.0, key.1, key.2) {
            Ok(t) => Slot::Ready(Arc::new(t)),
            // Shown as a blank tile and counted in the picker's status.
            Err(_) => {
                shared.failures.fetch_add(1, Ordering::Relaxed);
                Slot::Failed
            }
        };
        shared.slots.lock().unwrap().insert(key, slot);
        shared.generation.fetch_add(1, Ordering::Relaxed);
    }
}

/// Web Mercator: latitude/longitude to [0, 1] × [0, 1] (y down, north up).
pub fn mercator(lat: f64, lon: f64) -> (f64, f64) {
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    ((lon + 180.0) / 360.0, (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0)
}

/// Inverse of [`mercator`].
pub fn degrees(x: f64, y: f64) -> (f64, f64) {
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees();
    (lat, x * 360.0 - 180.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_round_trips() {
        let (x, y) = mercator(45.9, 6.13);
        let (lat, lon) = degrees(x, y);
        assert!((lat - 45.9).abs() < 1e-9 && (lon - 6.13).abs() < 1e-9);
        assert_eq!(mercator(0.0, 0.0), (0.5, 0.5));
    }
}

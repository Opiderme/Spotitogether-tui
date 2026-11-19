//! A real place as [`MapData`]: the location from Nominatim (or "lat,lon"),
//! roads, buildings, land use, water and trees from the Overpass API, and
//! the relief from the open Terrain Tiles (AWS). Everything is cached in
//! `~/.cache/spotitogether/drive/`, so a place is downloaded once.
//!
//! Usage rules respected: one Nominatim and one Overpass request per new
//! place, an application User-Agent, results cached.

use anyhow::{Context, bail};
use glam::Vec2;
use serde_json::Value;

use crate::map::{Building, Cover, MapData, Road, RoadKind, Tree, inside};

use crate::tiles::{USER_AGENT, agent, cache_dir};
const OVERPASS: &str = "https://overpass-api.de/api/interpreter";
const NOMINATIM: &str = "https://nominatim.openstreetmap.org/search";
/// Terrain tile zoom: about 9.5 m per pixel at the equator.
const ZOOM: u32 = 14;
const CELL: f32 = 8.0;

/// Where the map is centred, in degrees.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
}

/// "45.89,6.13" or a place name looked up with Nominatim.
pub fn locate(query: &str) -> anyhow::Result<(Place, String)> {
    if let Some((a, b)) = query.split_once(',')
        && let (Ok(lat), Ok(lon)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>())
    {
        return Ok((Place { lat, lon }, format!("{lat:.4}, {lon:.4}")));
    }
    let body = agent()
        .get(NOMINATIM)
        .header("User-Agent", USER_AGENT)
        .query("format", "jsonv2")
        .query("limit", "1")
        .query("q", query)
        .call()
        .context("recherche du lieu (Nominatim)")?
        .body_mut()
        .read_to_string()?;
    let found: Value = serde_json::from_str(&body)?;
    let first = found.get(0).with_context(|| format!("lieu introuvable : « {query} »"))?;
    let num = |k: &str| first[k].as_str().and_then(|s| s.parse::<f64>().ok());
    let (Some(lat), Some(lon)) = (num("lat"), num("lon")) else { bail!("réponse de Nominatim inattendue") };
    let name = first["display_name"].as_str().unwrap_or(query).split(',').take(2).collect::<Vec<_>>().join(",");
    Ok((Place { lat, lon }, name))
}

/// Local metres from degrees: x east, z south, the place at the centre.
struct Projection {
    lat0: f64,
    lon0: f64,
    half: f32,
}

impl Projection {
    const M_PER_DEG_LAT: f64 = 110_540.0;

    fn m_per_deg_lon(&self) -> f64 {
        111_320.0 * self.lat0.to_radians().cos()
    }

    fn to_local(&self, lat: f64, lon: f64) -> Vec2 {
        Vec2::new(
            ((lon - self.lon0) * self.m_per_deg_lon()) as f32 + self.half,
            ((self.lat0 - lat) * Self::M_PER_DEG_LAT) as f32 + self.half,
        )
    }

    fn to_degrees(&self, p: Vec2) -> (f64, f64) {
        (
            self.lat0 - f64::from(p.y - self.half) / Self::M_PER_DEG_LAT,
            self.lon0 + f64::from(p.x - self.half) / self.m_per_deg_lon(),
        )
    }

    /// South, west, north, east.
    fn bbox(&self) -> (f64, f64, f64, f64) {
        let (north, west) = self.to_degrees(Vec2::ZERO);
        let (south, east) = self.to_degrees(Vec2::splat(self.half * 2.0));
        (south, west, north, east)
    }
}

/// Downloads (or reads from the cache) the map around `place`.
pub fn load(place: Place, name: &str, size: f32, progress: &dyn Fn(&str)) -> anyhow::Result<MapData> {
    let proj = Projection { lat0: place.lat, lon0: place.lon, half: size / 2.0 };
    progress("relief (Terrain Tiles)…");
    let heights = elevation(&proj, size)?;
    progress("routes, bâtiments, forêts et eau (OpenStreetMap)…");
    let osm = overpass(&proj, place, size)?;
    progress("construction de la carte…");
    Ok(build(&proj, size, heights, &osm, name))
}

fn overpass(proj: &Projection, place: Place, size: f32) -> anyhow::Result<Value> {
    let cache = cache_dir().map(|d| d.join("osm").join(format!("{:.5}_{:.5}_{}.json", place.lat, place.lon, size as u32)));
    if let Some(path) = &cache
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str(&text)
    {
        return Ok(v);
    }
    let (s, w, n, e) = proj.bbox();
    let b = format!("{s:.6},{w:.6},{n:.6},{e:.6}");
    let query = format!(
        r#"[out:json][timeout:90];
(
  way["highway"]({b});
  way["building"]({b});
  relation["building"]({b});
  way["landuse"]({b});
  relation["landuse"]({b});
  way["natural"]({b});
  relation["natural"]({b});
  way["leisure"~"park|garden|pitch|golf_course|playground"]({b});
  way["waterway"]({b});
  node["natural"="tree"]({b});
);
out geom;"#
    );
    let mut resp = agent()
        .post(OVERPASS)
        .header("User-Agent", USER_AGENT)
        .send_form([("data", query.as_str())])
        .map_err(|e| match e {
            ureq::Error::StatusCode(429 | 504) => anyhow::anyhow!("le serveur OpenStreetMap (Overpass) est surchargé, réessaie dans une minute"),
            e => anyhow::Error::new(e).context("téléchargement OpenStreetMap (Overpass)"),
        })?;
    let text = resp.body_mut().with_config().limit(200 * 1024 * 1024).read_to_string()?;
    let v: Value = serde_json::from_str(&text).context("réponse Overpass illisible")?;
    if let Some(path) = &cache {
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(path, &text);
    }
    Ok(v)
}

/// Height grid (n × n vertices every `CELL` metres).
fn elevation(proj: &Projection, size: f32) -> anyhow::Result<Vec<f32>> {
    let n = (size / CELL).round() as usize + 1;
    let scale = f64::from(1u32 << ZOOM);
    let pixel = |lat: f64, lon: f64| {
        let x = (lon + 180.0) / 360.0 * scale * 256.0;
        let lat = lat.to_radians();
        let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * scale * 256.0;
        (x, y)
    };
    let mut tiles: std::collections::HashMap<(u32, u32), Vec<f32>> = std::collections::HashMap::new();
    let mut sample = |px: f64, py: f64| -> anyhow::Result<f32> {
        let (tx, ty) = ((px / 256.0) as u32, (py / 256.0) as u32);
        let t = match tiles.entry((tx, ty)) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(crate::tiles::fetch(ZOOM as u8, tx, ty)?),
        };
        let (x, y) = ((px.rem_euclid(256.0)) as usize, (py.rem_euclid(256.0)) as usize);
        Ok(t[y.min(255) * 256 + x.min(255)])
    };
    let mut heights = vec![0.0; n * n];
    for gz in 0..n {
        for gx in 0..n {
            let (lat, lon) = proj.to_degrees(Vec2::new(gx as f32, gz as f32) * CELL);
            let (px, py) = pixel(lat, lon);
            // Bilinear between the four nearest pixels.
            let (x0, y0) = ((px - 0.5).floor(), (py - 0.5).floor());
            let (fx, fy) = ((px - 0.5 - x0) as f32, (py - 0.5 - y0) as f32);
            let h00 = sample(x0 + 0.5, y0 + 0.5)?;
            let h10 = sample(x0 + 1.5, y0 + 0.5)?;
            let h01 = sample(x0 + 0.5, y0 + 1.5)?;
            let h11 = sample(x0 + 1.5, y0 + 1.5)?;
            let top = h00 + (h10 - h00) * fx;
            let bottom = h01 + (h11 - h01) * fx;
            heights[gz * n + gx] = top + (bottom - top) * fy;
        }
    }
    Ok(heights)
}

fn tag<'a>(e: &'a Value, key: &str) -> Option<&'a str> {
    e["tags"][key].as_str()
}

fn line(proj: &Projection, geometry: &Value) -> Vec<Vec2> {
    geometry
        .as_array()
        .map(|pts| {
            pts.iter()
                .filter_map(|p| Some(proj.to_local(p["lat"].as_f64()?, p["lon"].as_f64()?)))
                .collect()
        })
        .unwrap_or_default()
}

/// Closed outlines of an element: a closed way, or the outer rings of a
/// multipolygon relation (its member ways joined end to end).
fn rings(proj: &Projection, e: &Value) -> Vec<Vec<Vec2>> {
    match e["type"].as_str() {
        Some("way") => {
            let pts = line(proj, &e["geometry"]);
            if pts.len() >= 4 && pts[0].distance(*pts.last().unwrap()) < 0.5 { vec![pts] } else { Vec::new() }
        }
        Some("relation") => {
            let parts: Vec<Vec<Vec2>> = e["members"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"].as_str() == Some("outer"))
                .map(|m| line(proj, &m["geometry"]))
                .filter(|l| l.len() >= 2)
                .collect();
            join_rings(parts)
        }
        _ => Vec::new(),
    }
}

/// Joins open polylines sharing end points into closed rings.
fn join_rings(mut parts: Vec<Vec<Vec2>>) -> Vec<Vec<Vec2>> {
    let close = |a: Vec2, b: Vec2| a.distance(b) < 0.5;
    let mut out = Vec::new();
    while let Some(mut ring) = parts.pop() {
        while !close(ring[0], *ring.last().unwrap()) {
            let end = *ring.last().unwrap();
            let Some(i) = parts.iter().position(|p| close(p[0], end) || close(*p.last().unwrap(), end)) else { break };
            let mut next = parts.swap_remove(i);
            if !close(next[0], end) {
                next.reverse();
            }
            ring.extend(next.into_iter().skip(1));
        }
        if ring.len() >= 4 && close(ring[0], *ring.last().unwrap()) {
            out.push(ring);
        }
    }
    out
}

/// Land cover of an element and its priority (higher wins where areas
/// overlap).
fn cover_of(e: &Value) -> Option<(Cover, u8)> {
    let landuse = tag(e, "landuse");
    let natural = tag(e, "natural");
    let leisure = tag(e, "leisure");
    Some(match (landuse, natural, leisure) {
        (_, Some("water"), _) | (Some("reservoir" | "basin"), _, _) => (Cover::Water, 6),
        (_, Some("beach" | "sand"), _) => (Cover::Sand, 5),
        (_, Some("bare_rock" | "scree" | "cliff" | "glacier"), _) => (Cover::Rock, 5),
        (Some("forest"), _, _) | (_, Some("wood"), _) => (Cover::Forest, 4),
        (_, _, Some("park" | "garden" | "golf_course" | "playground")) => (Cover::Grass, 3),
        (Some("grass" | "village_green" | "recreation_ground" | "cemetery"), _, _) | (_, _, Some("pitch")) => (Cover::Grass, 3),
        (Some("meadow"), _, _) | (_, Some("grassland" | "heath" | "scrub" | "wetland" | "fell"), _) => (Cover::Meadow, 3),
        (Some("farmland" | "orchard" | "vineyard" | "allotments" | "farmyard"), _, _) => (Cover::Farmland, 2),
        (Some("residential" | "commercial" | "industrial" | "retail" | "construction" | "garages" | "railway"), _, _) => {
            (Cover::Residential, 1)
        }
        _ => return None,
    })
}

fn road_of(e: &Value) -> Option<(f32, RoadKind)> {
    if tag(e, "tunnel").is_some_and(|t| t != "no") || tag(e, "area") == Some("yes") {
        return None;
    }
    Some(match tag(e, "highway")? {
        "motorway" | "motorway_link" => (14.0, RoadKind::Major),
        "trunk" | "trunk_link" => (12.0, RoadKind::Major),
        "primary" | "primary_link" => (10.0, RoadKind::Major),
        "secondary" | "secondary_link" => (9.0, RoadKind::Major),
        "tertiary" | "tertiary_link" => (8.0, RoadKind::Major),
        "unclassified" | "residential" | "living_street" | "road" => (6.0, RoadKind::Minor),
        "service" | "pedestrian" => (4.0, RoadKind::Minor),
        "track" => (3.5, RoadKind::Track),
        _ => return None,
    })
}

fn building_height(e: &Value) -> f32 {
    let number = |k: &str| tag(e, k).and_then(|v| v.trim_end_matches(" m").trim().replace(',', ".").parse::<f32>().ok());
    if let Some(h) = number("height") {
        return h.clamp(2.5, 300.0);
    }
    if let Some(levels) = number("building:levels") {
        return (levels * 3.2 + 1.0).clamp(3.0, 300.0);
    }
    match tag(e, "building") {
        Some("garage" | "garages" | "shed" | "hut" | "carport" | "roof") => 3.0,
        Some("apartments" | "office" | "hotel") => 15.0,
        Some("church" | "cathedral") => 18.0,
        Some("industrial" | "warehouse" | "retail" | "supermarket") => 9.0,
        _ => 7.0,
    }
}

/// Splits a polyline into the runs inside the map, cutting segments at
/// its edges (Liang–Barsky).
fn clip(points: &[Vec2], size: f32) -> Vec<Vec<Vec2>> {
    let segment = |a: Vec2, b: Vec2| -> Option<(Vec2, Vec2)> {
        let d = b - a;
        let (mut t0, mut t1) = (0.0f32, 1.0f32);
        for (p, q) in [(-d.x, a.x), (d.x, size - a.x), (-d.y, a.y), (d.y, size - a.y)] {
            if p == 0.0 {
                if q < 0.0 {
                    return None;
                }
            } else {
                let r = q / p;
                if p < 0.0 {
                    t0 = t0.max(r);
                } else {
                    t1 = t1.min(r);
                }
            }
        }
        (t0 < t1).then(|| (a + d * t0, a + d * t1))
    };
    let mut out: Vec<Vec<Vec2>> = Vec::new();
    let mut run: Vec<Vec2> = Vec::new();
    for w in points.windows(2) {
        match segment(w[0], w[1]) {
            Some((a, b)) => {
                if run.last().is_none_or(|l| l.distance(a) > 0.01) {
                    if run.len() >= 2 {
                        out.push(std::mem::take(&mut run));
                    }
                    run.clear();
                    run.push(a);
                }
                run.push(b);
            }
            None => {
                if run.len() >= 2 {
                    out.push(std::mem::take(&mut run));
                }
                run.clear();
            }
        }
    }
    if run.len() >= 2 {
        out.push(run);
    }
    out
}

fn build(proj: &Projection, size: f32, heights: Vec<f32>, osm: &Value, name: &str) -> MapData {
    let n = (size / CELL).round() as usize + 1;
    let cells = n - 1;
    let mut cover = vec![Cover::Grass; cells * cells];
    let mut priority = vec![0u8; cells * cells];
    let mut roads = Vec::new();
    let mut buildings = Vec::new();
    let mut trees = Vec::new();
    let (mut needle, mut broad) = (0, 0);

    let mut paint = |ring: &[Vec2], c: Cover, prio: u8| {
        let (lo, hi) = ring.iter().fold((Vec2::MAX, Vec2::MIN), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        let x0 = ((lo.x / CELL).floor().max(0.0) as usize).min(cells);
        let x1 = ((hi.x / CELL).ceil().max(0.0) as usize).min(cells);
        let z0 = ((lo.y / CELL).floor().max(0.0) as usize).min(cells);
        let z1 = ((hi.y / CELL).ceil().max(0.0) as usize).min(cells);
        for cz in z0..z1 {
            for cx in x0..x1 {
                let i = cz * cells + cx;
                if prio >= priority[i] && inside(Vec2::new(cx as f32 + 0.5, cz as f32 + 0.5) * CELL, ring) {
                    cover[i] = c;
                    priority[i] = prio;
                }
            }
        }
    };

    let elements = osm["elements"].as_array().map(Vec::as_slice).unwrap_or_default();
    for e in elements {
        if let Some((c, prio)) = cover_of(e) {
            if c == Cover::Forest {
                match tag(e, "leaf_type") {
                    Some("needleleaved") => needle += 1,
                    Some("broadleaved") => broad += 1,
                    _ => {}
                }
            }
            for ring in rings(proj, e) {
                paint(&ring, c, prio);
            }
        }
    }
    // Rivers and streams as bands of water.
    for e in elements {
        let width = match tag(e, "waterway") {
            Some("river") => 18.0,
            Some("canal") => 12.0,
            Some("stream" | "ditch" | "drain") => 3.0,
            _ => continue,
        };
        let width: f32 = tag(e, "width").and_then(|w| w.parse().ok()).unwrap_or(width);
        let radius = (width / 2.0).max(CELL * 0.5);
        let pts = line(proj, &e["geometry"]);
        for w in pts.windows(2) {
            let steps = (w[0].distance(w[1]) / (CELL / 2.0)).ceil().max(1.0) as usize;
            for k in 0..=steps {
                let p = w[0].lerp(w[1], k as f32 / steps as f32);
                let r = (radius / CELL).ceil() as isize;
                let (cx, cz) = ((p.x / CELL) as isize, (p.y / CELL) as isize);
                for dz in -r..=r {
                    for dx in -r..=r {
                        let (x, z) = (cx + dx, cz + dz);
                        if x < 0 || z < 0 || x >= cells as isize || z >= cells as isize {
                            continue;
                        }
                        let centre = Vec2::new(x as f32 + 0.5, z as f32 + 0.5) * CELL;
                        if centre.distance(p) <= radius {
                            cover[z as usize * cells + x as usize] = Cover::Water;
                        }
                    }
                }
            }
        }
    }
    for e in elements {
        if let Some((width, kind)) = road_of(e) {
            for points in clip(&line(proj, &e["geometry"]), size) {
                roads.push(Road { points, width, kind });
            }
        }
        if tag(e, "building").is_some() {
            for mut outline in rings(proj, e) {
                outline.pop(); // closing point
                let area = outline.windows(2).map(|w| w[0].perp_dot(w[1])).sum::<f32>().abs() / 2.0;
                let in_map = outline.iter().all(|p| p.x > 0.0 && p.y > 0.0 && p.x < size && p.y < size);
                if in_map && area > 6.0 && area < 30_000.0 {
                    buildings.push(Building { outline, height: building_height(e) });
                }
            }
        }
        if e["type"].as_str() == Some("node")
            && tag(e, "natural") == Some("tree")
            && let (Some(lat), Some(lon)) = (e["lat"].as_f64(), e["lon"].as_f64())
        {
            let at = proj.to_local(lat, lon);
            if at.cmpge(Vec2::ZERO).all() && at.cmple(Vec2::splat(size)).all() {
                trees.push(Tree { at, conifer: tag(e, "leaf_type") == Some("needleleaved") });
            }
        }
    }

    let mean_height = heights.iter().sum::<f32>() / heights.len() as f32;
    let conifers = if needle + broad > 0 {
        needle as f32 / (needle + broad) as f32
    } else if proj.lat0.abs() > 55.0 || mean_height > 900.0 {
        0.8
    } else {
        0.35
    };
    MapData {
        name: name.to_owned(),
        size,
        cell: CELL,
        heights,
        cover,
        roads,
        buildings,
        trees,
        conifers,
        seed: ((proj.lat0 * 1000.0) as i64 ^ (proj.lon0 * 1000.0) as i64) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proj() -> Projection {
        Projection { lat0: 45.9, lon0: 6.1, half: 500.0 }
    }

    #[test]
    fn projection_round_trips() {
        let p = proj();
        let local = p.to_local(45.901, 6.102);
        let (lat, lon) = p.to_degrees(local);
        assert!((lat - 45.901).abs() < 1e-6 && (lon - 6.102).abs() < 1e-6);
        assert!(local.x > 500.0 && local.y < 500.0, "north-east of the centre: {local:?}");
    }

    #[test]
    fn joins_relation_parts_into_rings() {
        let parts = vec![
            vec![Vec2::ZERO, Vec2::new(10.0, 0.0)],
            vec![Vec2::new(10.0, 10.0), Vec2::new(10.0, 0.0)],
            vec![Vec2::new(10.0, 10.0), Vec2::new(0.0, 10.0), Vec2::ZERO],
        ];
        let rings = join_rings(parts);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 5);
    }

    #[test]
    fn turns_overpass_elements_into_a_map() {
        let p = proj();
        let pt = |x: f32, z: f32| {
            let (lat, lon) = p.to_degrees(Vec2::new(x, z));
            json!({"lat": lat, "lon": lon})
        };
        let osm = json!({"elements": [
            {"type": "way", "tags": {"natural": "water"}, "geometry": [pt(100.0,100.0), pt(300.0,100.0), pt(300.0,300.0), pt(100.0,300.0), pt(100.0,100.0)]},
            {"type": "way", "tags": {"landuse": "forest", "leaf_type": "needleleaved"}, "geometry": [pt(600.0,600.0), pt(900.0,600.0), pt(900.0,900.0), pt(600.0,900.0), pt(600.0,600.0)]},
            {"type": "way", "tags": {"highway": "primary"}, "geometry": [pt(-50.0,500.0), pt(500.0,500.0), pt(1100.0,500.0)]},
            {"type": "way", "tags": {"highway": "footway"}, "geometry": [pt(0.0,0.0), pt(50.0,50.0)]},
            {"type": "way", "tags": {"building": "yes", "building:levels": "3"}, "geometry": [pt(400.0,400.0), pt(420.0,400.0), pt(420.0,410.0), pt(400.0,410.0), pt(400.0,400.0)]},
            {"type": "node", "tags": {"natural": "tree"}, "lat": p.to_degrees(Vec2::new(50.0, 950.0)).0, "lon": p.to_degrees(Vec2::new(50.0, 950.0)).1}
        ]});
        let n = (1000.0 / CELL) as usize + 1;
        let map = build(&p, 1000.0, vec![100.0; n * n], &osm, "test");
        assert_eq!(map.cover_at(Vec2::new(200.0, 200.0)), Cover::Water);
        assert_eq!(map.cover_at(Vec2::new(750.0, 750.0)), Cover::Forest);
        assert_eq!(map.cover_at(Vec2::new(50.0, 50.0)), Cover::Grass);
        assert_eq!(map.roads.len(), 1, "the footway is skipped");
        assert!(map.roads[0].points.iter().all(|q| q.x >= 0.0 && q.x <= 1000.0), "clipped to the map");
        assert_eq!(map.buildings.len(), 1);
        assert!((map.buildings[0].height - 10.6).abs() < 0.01);
        assert_eq!(map.trees.len(), 1);
        assert_eq!(map.conifers, 1.0);
    }
}

//! A drivable world built from [`MapData`]: terrain whose material follows
//! the land cover, roads flattened into it, water in its hollows,
//! buildings, and trees sown where the map has forests. Triangles are
//! grouped in square chunks so the renderer can skip what is out of view
//! and use simpler trees far away.

use glam::{Vec2, Vec3};

use crate::map::{Cover, MapData, RoadKind};
use crate::material::Material;
use crate::mesh::{self, Mesh, Surface, Triangle};
use crate::noise::{self, Rng};

/// Chunk side (metres).
const CHUNK: f32 = 64.0;
/// Road centre lines are resampled to this spacing (metres).
const ROAD_STEP: f32 = 4.0;
/// Ground blends back to its natural height this far beyond the verge.
const ROAD_BLEND: f32 = 24.0;
/// Water beds are dug this far below the surface.
const LAKE_DEPTH: f32 = 1.6;

pub struct Chunk {
    pub min: Vec3,
    pub max: Vec3,
    /// Ground, roads, water, buildings.
    pub base: Vec<Triangle>,
    pub trees_near: Vec<Triangle>,
    pub trees_far: Vec<Triangle>,
}

pub struct RoadLine {
    pub points: Vec<Vec3>,
    /// Cumulative length at each point.
    pub length: Vec<f32>,
}

pub struct World {
    pub name: String,
    pub size: f32,
    pub chunks: Vec<Chunk>,
    pub roads: Vec<RoadLine>,
    /// Index of the longest road (the generated map's loop).
    main_road: usize,
    /// Everything the car collides with.
    pub solids: Vec<Triangle>,
}

/// Where the road is at some distance along it.
pub struct OnRoad {
    pub pos: Vec3,
    /// Unit direction of travel (horizontal).
    pub dir: Vec3,
}

/// Road centre points in buckets, for "nearest road" queries.
struct RoadIndex {
    bucket: f32,
    cols: usize,
    cells: Vec<Vec<(usize, usize)>>,
}

impl RoadIndex {
    fn new(roads: &[RoadLine], size: f32) -> Self {
        let bucket = 16.0;
        let cols = (size / bucket).ceil() as usize + 1;
        let mut cells = vec![Vec::new(); cols * cols];
        let clamp = |v: f32| (v / bucket).clamp(0.0, (cols - 1) as f32) as usize;
        for (r, road) in roads.iter().enumerate() {
            for (i, p) in road.points.iter().enumerate() {
                cells[clamp(p.z) * cols + clamp(p.x)].push((r, i));
            }
        }
        Self { bucket, cols, cells }
    }

    /// Road points within about `radius` of `p`.
    fn near(&self, p: Vec2, radius: f32) -> impl Iterator<Item = (usize, usize)> + '_ {
        let span = (radius / self.bucket).ceil() as isize;
        let (cx, cz) = ((p.x / self.bucket) as isize, (p.y / self.bucket) as isize);
        (-span..=span).flat_map(move |dz| {
            (-span..=span).flat_map(move |dx| {
                let (x, z) = (cx + dx, cz + dz);
                let ok = x >= 0 && z >= 0 && (x as usize) < self.cols && (z as usize) < self.cols;
                let list: &[(usize, usize)] = if ok { &self.cells[z as usize * self.cols + x as usize] } else { &[] };
                list.iter().copied()
            })
        })
    }
}

fn cover_surface(cover: Cover, height: f32, steep: bool, jitter: f32) -> Surface {
    let material = match cover {
        _ if height > 2600.0 => Material::Snow,
        Cover::Water => Material::Sand,
        _ if steep => Material::Rock,
        Cover::Grass => Material::Grass,
        Cover::Meadow => Material::Meadow,
        Cover::Forest => Material::ForestFloor,
        Cover::Farmland => Material::Farmland,
        Cover::Residential => Material::Residential,
        Cover::Sand => Material::Sand,
        Cover::Rock => Material::Rock,
    };
    // A little variation so large fields are not one flat colour.
    Surface::tinted(material, 0.93 + jitter * 0.14)
}

impl World {
    pub fn build(map: &MapData) -> Self {
        let n = map.n();
        let cells = n - 1;
        let natural = map.heights.clone();
        let mut heights = map.heights.clone();
        let vertex = |gx: usize, gz: usize| Vec2::new(gx as f32, gz as f32) * map.cell;

        // Roads: resample, then smooth their height along the way so slopes
        // stay drivable.
        let mut widths = Vec::new();
        let mut roads: Vec<RoadLine> = Vec::new();
        for road in &map.roads {
            let mut pts = Vec::new();
            for w in road.points.windows(2) {
                let steps = ((w[1] - w[0]).length() / ROAD_STEP).ceil().max(1.0) as usize;
                for k in 0..steps {
                    pts.push(w[0].lerp(w[1], k as f32 / steps as f32));
                }
            }
            if let Some(&last) = road.points.last() {
                pts.push(last);
            }
            if pts.len() < 2 {
                continue;
            }
            let mut ys: Vec<f32> = pts.iter().map(|&p| map.height(p)).collect();
            // A loop is smoothed across its seam, so both ends meet.
            let closed = pts.len() > 3 && pts[0].distance(pts[pts.len() - 1]) < 1.0;
            for _ in 0..4 {
                let prev = ys.clone();
                let len = prev.len() - usize::from(closed);
                for (i, y) in ys.iter_mut().enumerate() {
                    *y = if closed {
                        (0..=10).map(|k| prev[(i % len + len + k - 5) % len]).sum::<f32>() / 11.0
                    } else {
                        let lo = i.saturating_sub(5);
                        let hi = (i + 5).min(prev.len() - 1);
                        prev[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32
                    };
                }
            }
            let points: Vec<Vec3> = pts.iter().zip(&ys).map(|(p, &y)| Vec3::new(p.x, y, p.y)).collect();
            let mut length = vec![0.0];
            for w in points.windows(2) {
                length.push(length.last().unwrap() + w[0].distance(w[1]));
            }
            roads.push(RoadLine { points, length });
            widths.push((road.width, road.kind));
        }
        let index = RoadIndex::new(&roads, map.size);
        let max_width = widths.iter().map(|w| w.0).fold(0.0, f32::max);

        // Pull the ground to the road level near roads.
        for gz in 0..n {
            for gx in 0..n {
                let p = vertex(gx, gz);
                let mut best: Option<(f32, f32)> = None;
                for (r, i) in index.near(p, max_width / 2.0 + ROAD_BLEND + 2.0) {
                    let q = roads[r].points[i];
                    let d = p.distance(Vec2::new(q.x, q.z)) - widths[r].0 / 2.0;
                    if best.is_none_or(|b| d < b.0) {
                        best = Some((d, q.y));
                    }
                }
                if let Some((d, road_y)) = best {
                    let edge = 1.5;
                    let target = road_y - 0.3;
                    let h = &mut heights[gz * n + gx];
                    *h = if d < edge {
                        target
                    } else {
                        let t = ((d - edge) / ROAD_BLEND).clamp(0.0, 1.0);
                        let t = t * t * (3.0 - 2.0 * t);
                        target + (*h - target) * t
                    };
                }
            }
        }

        // Water: each water cell's surface sits a little under its lowest
        // natural corner; the bed is dug below it.
        let mut water_level = vec![f32::NAN; cells * cells];
        for cz in 0..cells {
            for cx in 0..cells {
                if map.cover[cz * cells + cx] != Cover::Water {
                    continue;
                }
                let lowest = [(0, 0), (1, 0), (0, 1), (1, 1)]
                    .iter()
                    .map(|&(dx, dz)| natural[(cz + dz) * n + cx + dx])
                    .fold(f32::MAX, f32::min);
                water_level[cz * cells + cx] = lowest - 0.3;
            }
        }
        for cz in 0..cells {
            for cx in 0..cells {
                let level = water_level[cz * cells + cx];
                if level.is_nan() {
                    continue;
                }
                for (dx, dz) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let h = &mut heights[(cz + dz) * n + cx + dx];
                    *h = h.min(level - LAKE_DEPTH);
                }
            }
        }

        let at = |gx: usize, gz: usize| Vec3::new(gx as f32 * map.cell, heights[gz * n + gx], gz as f32 * map.cell);
        let mut ground = Mesh::default();
        for cz in 0..cells {
            for cx in 0..cells {
                let (p00, p10, p01, p11) = (at(cx, cz), at(cx + 1, cz), at(cx, cz + 1), at(cx + 1, cz + 1));
                let cover = map.cover[cz * cells + cx];
                let jitter = noise::hash(cx as i32, cz as i32, map.seed + 99);
                for (a, b, c) in [(p00, p01, p10), (p10, p01, p11)] {
                    let normal = (b - a).cross(c - a).normalize();
                    let steep = normal.y < 0.78 && cover != Cover::Water;
                    ground.tri(a, b, c, Vec3::Y, cover_surface(cover, (a.y + b.y + c.y) / 3.0, steep, jitter));
                }
                let level = water_level[cz * cells + cx];
                if !level.is_nan() {
                    let w = |p: Vec3| Vec3::new(p.x, level, p.z);
                    ground.quad(w(p00), w(p10), w(p11), w(p01), Vec3::Y, Surface::of(Material::Water));
                }
            }
        }

        // Road ribbons, majors on top where roads overlap.
        for (road, &(width, kind)) in roads.iter().zip(&widths) {
            let (surface, lift) = match kind {
                RoadKind::Major => (Surface::of(Material::Asphalt), 0.09),
                RoadKind::Minor => (Surface::tinted(Material::Asphalt, 1.08), 0.07),
                RoadKind::Track => (Surface::of(Material::Dirt), 0.05),
            };
            let pts = &road.points;
            let side = |i: usize| {
                let d = pts[(i + 1).min(pts.len() - 1)] - pts[i.saturating_sub(1)];
                Vec3::new(-d.z, 0.0, d.x).normalize_or(Vec3::X)
            };
            for i in 0..pts.len() - 1 {
                let (a, b) = (pts[i] + Vec3::Y * lift, pts[i + 1] + Vec3::Y * lift);
                let (sa, sb) = (side(i) * width / 2.0, side(i + 1) * width / 2.0);
                ground.quad(a - sa, a + sa, b + sb, b - sb, Vec3::Y, surface);
                if kind == RoadKind::Major && width >= 7.0 && i % 4 < 2 {
                    let (la, lb) = (side(i) * 0.15, side(i + 1) * 0.15);
                    let (a, b) = (a + Vec3::Y * 0.03, b + Vec3::Y * 0.03);
                    ground.quad(a - la, a + la, b + lb, b - lb, Vec3::Y, Surface::of(Material::Marking));
                }
            }
        }

        // Buildings, each its own object for the ASCII outlines.
        let mut rng = Rng(u64::from(map.seed) * 31 + 7);
        let walls = [[206, 188, 160], [178, 178, 186], [214, 200, 172], [190, 160, 140], [222, 214, 200]];
        let roofs = [[150, 70, 56], [90, 92, 100], [120, 60, 50], [110, 104, 96]];
        for (k, b) in map.buildings.iter().enumerate() {
            if b.outline.len() < 3 {
                continue;
            }
            let ground_ys: Vec<f32> = b.outline.iter().map(|&p| map.height(p)).collect();
            let low = ground_ys.iter().copied().fold(f32::MAX, f32::min);
            let high = ground_ys.iter().copied().fold(f32::MIN, f32::max);
            ground.object = 1000 + (k % 30_000) as u16;
            let wall = Surface { material: Material::Wall, color: walls[(rng.unit() * walls.len() as f32) as usize % walls.len()] };
            let roof = Surface { material: Material::Roof, color: roofs[(rng.unit() * roofs.len() as f32) as usize % roofs.len()] };
            ground.prism(&b.outline, low - 0.5, high + b.height, wall, roof);
        }
        ground.object = 0;

        // Trees: the map's own, then sown by land cover.
        let near_road = |p: Vec2, margin: f32| {
            index.near(p, max_width / 2.0 + margin).any(|(r, i)| {
                let q = roads[r].points[i];
                p.distance(Vec2::new(q.x, q.z)) < widths[r].0 / 2.0 + margin
            })
        };
        let boxes: Vec<(Vec2, Vec2)> = map
            .buildings
            .iter()
            .map(|b| b.outline.iter().fold((Vec2::MAX, Vec2::MIN), |(lo, hi), &q| (lo.min(q), hi.max(q))))
            .collect();
        let in_building = |p: Vec2| {
            map.buildings
                .iter()
                .zip(&boxes)
                .any(|(b, (lo, hi))| p.cmpge(*lo - 2.0).all() && p.cmple(*hi + 2.0).all() && crate::map::inside(p, &b.outline))
        };
        let mut trees: Vec<(Vec2, bool, f32)> = map.trees.iter().map(|t| (t.at, t.conifer, rng.range(7.0, 13.0))).collect();
        for cz in 0..cells {
            for cx in 0..cells {
                let cover = map.cover[cz * cells + cx];
                let per_m2 = match cover {
                    Cover::Forest => 1.0 / 55.0,
                    Cover::Residential => 1.0 / 900.0,
                    Cover::Grass | Cover::Meadow => 1.0 / 2500.0,
                    _ => 0.0,
                };
                let expected = per_m2 * map.cell * map.cell;
                let count = expected.floor() as usize + usize::from(rng.unit() < expected.fract());
                for _ in 0..count {
                    let p = Vec2::new(cx as f32 + rng.unit(), cz as f32 + rng.unit()) * map.cell;
                    let height = if cover == Cover::Forest { rng.range(9.0, 17.0) } else { rng.range(6.0, 11.0) };
                    trees.push((p, rng.unit() < map.conifers, height));
                }
            }
        }
        trees.retain(|&(p, _, _)| !near_road(p, 2.5) && map.cover_at(p) != Cover::Water && !in_building(p));

        // Chunks.
        let cols = (map.size / CHUNK).ceil().max(1.0) as usize;
        let chunk_of = |p: Vec3| {
            let x = (p.x / CHUNK).clamp(0.0, (cols - 1) as f32) as usize;
            let z = (p.z / CHUNK).clamp(0.0, (cols - 1) as f32) as usize;
            z * cols + x
        };
        let mut chunks: Vec<Chunk> = (0..cols * cols)
            .map(|_| Chunk { min: Vec3::MAX, max: Vec3::MIN, base: Vec::new(), trees_near: Vec::new(), trees_far: Vec::new() })
            .collect();
        let mut solids = Vec::new();
        for t in ground.tris {
            if t.material.is_solid() {
                solids.push(t);
            }
            chunks[chunk_of((t.p[0] + t.p[1] + t.p[2]) / 3.0)].base.push(t);
        }
        for (k, &(p, conifer, height)) in trees.iter().enumerate() {
            let at = Vec3::new(p.x, map.height(p), p.y);
            let variant = noise::hash(k as i32, 3, map.seed);
            let object = 1 + (k % 999) as u16;
            let mut near = Mesh { object, ..Mesh::default() };
            if conifer {
                mesh::conifer(&mut near, at, height, variant);
            } else {
                mesh::broadleaf(&mut near, at, height, variant);
            }
            let mut far = Mesh { object, ..Mesh::default() };
            mesh::tree_far(&mut far, at, height, conifer, variant);
            let c = &mut chunks[chunk_of(at)];
            solids.extend(near.tris.iter().filter(|t| t.material.is_solid()));
            c.trees_near.extend(near.tris);
            c.trees_far.extend(far.tris);
        }
        for c in &mut chunks {
            for t in c.base.iter().chain(&c.trees_near) {
                for p in t.p {
                    c.min = c.min.min(p);
                    c.max = c.max.max(p);
                }
            }
        }
        chunks.retain(|c| !c.base.is_empty() || !c.trees_near.is_empty());

        let main_road = (0..roads.len())
            .max_by(|&a, &b| roads[a].length.last().unwrap_or(&0.0).total_cmp(roads[b].length.last().unwrap_or(&0.0)))
            .unwrap_or(0);
        Self { name: map.name.clone(), size: map.size, chunks, roads, main_road, solids }
    }

    /// A world made of the given triangles only (tests, tools).
    pub fn from_triangles(tris: Vec<Triangle>) -> Self {
        let (min, max) = tris.iter().flat_map(|t| t.p).fold((Vec3::MAX, Vec3::MIN), |(lo, hi), p| (lo.min(p), hi.max(p)));
        let solids = tris.iter().filter(|t| t.material.is_solid()).copied().collect();
        let chunk = Chunk { min, max, base: tris, trees_near: Vec::new(), trees_far: Vec::new() };
        Self { name: String::new(), size: (max - min).max_element(), chunks: vec![chunk], roads: Vec::new(), main_road: 0, solids }
    }

    pub fn triangle_count(&self) -> usize {
        self.chunks.iter().map(|c| c.base.len() + c.trees_near.len()).sum()
    }

    pub fn tree_count(&self) -> usize {
        self.chunks.iter().map(|c| c.trees_far.len() / 4).sum()
    }

    /// The road point closest to `p`, heading along the road.
    pub fn nearest_road(&self, p: Vec3) -> OnRoad {
        let best = self
            .roads
            .iter()
            .enumerate()
            .flat_map(|(r, road)| road.points.iter().enumerate().map(move |(i, q)| (r, i, q.distance_squared(p))))
            .min_by(|a, b| a.2.total_cmp(&b.2));
        match best {
            Some((r, i, _)) => self.point_on(r, self.roads[r].length[i]),
            None => OnRoad { pos: Vec3::new(self.size / 2.0, p.y, self.size / 2.0), dir: Vec3::X },
        }
    }

    /// Length of the main road (metres).
    pub fn road_length(&self) -> f32 {
        self.roads.get(self.main_road).and_then(|r| r.length.last().copied()).unwrap_or(1.0)
    }

    /// Point on the main road `s` metres from its start, looping.
    pub fn on_road(&self, s: f32) -> OnRoad {
        if self.roads.is_empty() {
            return OnRoad { pos: Vec3::new(self.size / 2.0, 0.0, self.size / 2.0), dir: Vec3::X };
        }
        self.point_on(self.main_road, s.rem_euclid(self.road_length()))
    }

    fn point_on(&self, r: usize, s: f32) -> OnRoad {
        let road = &self.roads[r];
        let last = road.points.len() - 1;
        let i = road.length.partition_point(|&l| l <= s).saturating_sub(1).min(last.saturating_sub(1));
        let j = (i + 1).min(last);
        let (a, b) = (road.points[i], road.points[j]);
        let t = ((s - road.length[i]) / (road.length[j] - road.length[i]).max(1e-3)).clamp(0.0, 1.0);
        let ahead = road.points[(i + 3).min(last)] - road.points[i.saturating_sub(2)];
        let dir = Vec3::new(ahead.x, 0.0, ahead.z).normalize_or(Vec3::X);
        OnRoad { pos: a.lerp(b, t), dir }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_world_has_water_trees_and_a_loop() {
        let w = World::build(&MapData::generate(7));
        assert!(w.road_length() > 1000.0);
        let (a, b) = (w.on_road(0.0).pos, w.on_road(w.road_length() - 0.01).pos);
        assert!(a.distance(b) < 6.0, "loop closes: {}", a.distance(b));
        let all = || w.chunks.iter().flat_map(|c| c.base.iter().chain(&c.trees_near));
        assert!(all().any(|t| t.material == Material::Water));
        assert!(w.tree_count() > 500, "{} trees", w.tree_count());
        assert!(w.solids.iter().all(|t| t.material.is_solid()));
        assert!(w.chunks.iter().all(|c| c.min.cmple(c.max).all()));
    }
}

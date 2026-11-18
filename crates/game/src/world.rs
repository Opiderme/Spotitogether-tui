//! A generated test world for the render prototype: hilly terrain, a road
//! loop flattened into it, trees and a few buildings. Phase 3 replaces the
//! noise with real OpenStreetMap and elevation data; the mesh format stays.

use glam::{Vec2, Vec3};

/// One flat-shaded triangle in world space (metres, Y up). Vertices are
/// counter-clockwise seen from the front face.
#[derive(Debug, Clone, Copy)]
pub struct Triangle {
    pub p: [Vec3; 3],
    pub color: [u8; 3],
    /// Which object it belongs to (0 = the ground and road): the ASCII look
    /// outlines the borders between objects.
    pub object: u16,
}

/// Direction the light comes from.
pub fn sun() -> Vec3 {
    Vec3::new(0.45, 0.8, 0.35).normalize()
}

#[derive(Default)]
pub struct Mesh {
    pub tris: Vec<Triangle>,
    /// Object id given to the triangles added from now on.
    pub object: u16,
}

impl Mesh {
    /// Adds a triangle facing `outward`, shaded by the sun.
    pub fn tri(&mut self, a: Vec3, b: Vec3, c: Vec3, outward: Vec3, base: [u8; 3]) {
        let n = (b - a).cross(c - a);
        if n.length_squared() < 1e-12 {
            return;
        }
        let (b, c) = if n.dot(outward) < 0.0 { (c, b) } else { (b, c) };
        let light = 0.38 + 0.62 * (b - a).cross(c - a).normalize().dot(sun()).max(0.0);
        self.tris.push(Triangle { p: [a, b, c], color: shade(base, light), object: self.object });
    }

    /// Quad `a b c d` (in order around its edge) facing `outward`.
    pub fn quad(&mut self, a: Vec3, b: Vec3, c: Vec3, d: Vec3, outward: Vec3, base: [u8; 3]) {
        self.tri(a, b, c, outward, base);
        self.tri(a, c, d, outward, base);
    }

    /// Axis-aligned box with its own colour for the top.
    pub fn cuboid(&mut self, min: Vec3, max: Vec3, side: [u8; 3], top: [u8; 3]) {
        let [x0, y0, z0] = min.to_array();
        let [x1, y1, z1] = max.to_array();
        let v = |x, y, z| Vec3::new(x, y, z);
        self.quad(v(x0, y1, z0), v(x1, y1, z0), v(x1, y1, z1), v(x0, y1, z1), Vec3::Y, top);
        self.quad(v(x0, y0, z1), v(x1, y0, z1), v(x1, y1, z1), v(x0, y1, z1), Vec3::Z, side);
        self.quad(v(x0, y0, z0), v(x1, y0, z0), v(x1, y1, z0), v(x0, y1, z0), -Vec3::Z, side);
        self.quad(v(x1, y0, z0), v(x1, y0, z1), v(x1, y1, z1), v(x1, y1, z0), Vec3::X, side);
        self.quad(v(x0, y0, z0), v(x0, y0, z1), v(x0, y1, z1), v(x0, y1, z0), -Vec3::X, side);
    }

    /// Four-sided pyramid (a fir tree from far away).
    pub fn pyramid(&mut self, base_center: Vec3, radius: f32, height: f32, color: [u8; 3]) {
        let apex = base_center + Vec3::Y * height;
        let corners = [Vec3::X, Vec3::Z, -Vec3::X, -Vec3::Z].map(|d| base_center + d * radius);
        for i in 0..4 {
            let (a, b) = (corners[i], corners[(i + 1) % 4]);
            let outward = ((a + b) * 0.5 - base_center).normalize() + Vec3::Y * 0.3;
            self.tri(a, b, apex, outward, color);
        }
    }
}

pub fn shade(c: [u8; 3], light: f32) -> [u8; 3] {
    c.map(|v| (f32::from(v) * light).clamp(0.0, 255.0) as u8)
}

// ------------------------------------------------------------------ noise

fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x27d4_eb2d) ^ (y as u32).wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9e37_79b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h as f32 / u32::MAX as f32
}

fn value_noise(p: Vec2, seed: u32) -> f32 {
    let (i, f) = (p.floor(), p.fract());
    let (x, y) = (i.x as i32, i.y as i32);
    let s = f * f * (Vec2::splat(3.0) - 2.0 * f);
    let a = hash(x, y, seed);
    let b = hash(x + 1, y, seed);
    let c = hash(x, y + 1, seed);
    let d = hash(x + 1, y + 1, seed);
    let top = a + (b - a) * s.x;
    let bottom = c + (d - c) * s.x;
    top + (bottom - top) * s.y
}

/// Fractal noise in [0, 1).
fn fbm(p: Vec2, seed: u32) -> f32 {
    let (mut sum, mut amp, mut freq, mut norm) = (0.0, 1.0, 1.0, 0.0);
    for octave in 0..5 {
        sum += amp * value_noise(p * freq, seed + octave);
        norm += amp;
        amp *= 0.5;
        freq *= 2.0;
    }
    sum / norm
}

/// Small deterministic generator for placing props.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next()
    }
}

// ------------------------------------------------------------------ world

/// Grid cells per side and their size (metres).
const CELLS: usize = 150;
const CELL: f32 = 6.0;
const ROAD_WIDTH: f32 = 8.0;
const ROAD_POINTS: usize = 720;

pub const GRASS: [u8; 3] = [86, 140, 62];
const DRY: [u8; 3] = [150, 150, 84];
const ROCK: [u8; 3] = [128, 120, 112];
const SNOW: [u8; 3] = [235, 238, 242];
const ASPHALT: [u8; 3] = [72, 72, 78];
const LINE: [u8; 3] = [230, 230, 210];
const FIR: [u8; 3] = [38, 92, 52];

pub struct World {
    pub mesh: Mesh,
    /// Road centre line (closed loop), with cumulative length.
    road: Vec<Vec3>,
    road_len: Vec<f32>,
}

/// Where the road is at some distance along it.
pub struct OnRoad {
    pub pos: Vec3,
    /// Unit direction of travel (horizontal).
    pub dir: Vec3,
}

impl World {
    pub fn generate(seed: u32) -> Self {
        let size = CELLS as f32 * CELL;
        let center = Vec2::splat(size / 2.0);
        let raw_height = |p: Vec2| {
            let n = fbm(p / 260.0, seed);
            let ridges = 1.0 - (fbm(p / 140.0, seed + 17) * 2.0 - 1.0).abs();
            n * 90.0 + ridges * 25.0
        };

        // Road: a wobbly loop around the centre, its height smoothed so the
        // slopes stay drivable.
        let mut road: Vec<Vec3> = (0..ROAD_POINTS)
            .map(|i| {
                let t = i as f32 / ROAD_POINTS as f32 * std::f32::consts::TAU;
                let r = size * 0.3 * (1.0 + 0.2 * (3.0 * t + 1.0).sin() + 0.12 * (5.0 * t + 2.0).sin());
                let p = center + Vec2::new(t.cos(), t.sin()) * r;
                Vec3::new(p.x, raw_height(p), p.y)
            })
            .collect();
        for _ in 0..6 {
            let heights: Vec<f32> = road.iter().map(|p| p.y).collect();
            for (i, p) in road.iter_mut().enumerate() {
                let w = 20;
                let sum: f32 = (0..=2 * w).map(|k| heights[(i + ROAD_POINTS + k - w) % ROAD_POINTS]).sum();
                p.y = sum / (2 * w + 1) as f32;
            }
        }

        // Terrain heights, pulled to the road level near it.
        let n = CELLS + 1;
        let mut h = vec![0.0f32; n * n];
        for gz in 0..n {
            for gx in 0..n {
                let p = Vec2::new(gx as f32 * CELL, gz as f32 * CELL);
                let mut best = (f32::MAX, 0.0);
                for q in &road {
                    let d = p.distance_squared(Vec2::new(q.x, q.z));
                    if d < best.0 {
                        best = (d, q.y);
                    }
                }
                let (d, road_y) = (best.0.sqrt(), best.1);
                let natural = raw_height(p);
                let edge = ROAD_WIDTH / 2.0 + 2.0;
                h[gz * n + gx] = if d < edge {
                    road_y - 0.3
                } else {
                    let t = ((d - edge) / 26.0).clamp(0.0, 1.0);
                    let t = t * t * (3.0 - 2.0 * t);
                    road_y - 0.3 + (natural - road_y + 0.3) * t
                };
            }
        }
        let at = |gx: usize, gz: usize| Vec3::new(gx as f32 * CELL, h[gz * n + gx], gz as f32 * CELL);

        let mut mesh = Mesh::default();
        for gz in 0..CELLS {
            for gx in 0..CELLS {
                let (p00, p10, p01, p11) = (at(gx, gz), at(gx + 1, gz), at(gx, gz + 1), at(gx + 1, gz + 1));
                for (a, b, c) in [(p00, p01, p10), (p10, p01, p11)] {
                    let normal = (b - a).cross(c - a).normalize();
                    let height = (a.y + b.y + c.y) / 3.0;
                    let color = ground_color(height, normal.y, hash(gx as i32, gz as i32, seed + 99));
                    mesh.tri(a, b, c, Vec3::Y, color);
                }
            }
        }

        // Road ribbon and dashed centre line, slightly above the ground.
        let side = |i: usize| {
            let d = road[(i + 1) % ROAD_POINTS] - road[(i + ROAD_POINTS - 1) % ROAD_POINTS];
            Vec3::new(-d.z, 0.0, d.x).normalize()
        };
        for i in 0..ROAD_POINTS {
            let j = (i + 1) % ROAD_POINTS;
            let (a, b) = (road[i] + Vec3::Y * 0.05, road[j] + Vec3::Y * 0.05);
            let (sa, sb) = (side(i) * ROAD_WIDTH / 2.0, side(j) * ROAD_WIDTH / 2.0);
            mesh.quad(a - sa, a + sa, b + sb, b - sb, Vec3::Y, ASPHALT);
            if i % 4 < 2 {
                let (la, lb) = (side(i) * 0.15, side(j) * 0.15);
                let (a, b) = (a + Vec3::Y * 0.04, b + Vec3::Y * 0.04);
                mesh.quad(a - la, a + la, b + lb, b - lb, Vec3::Y, LINE);
            }
        }

        // Props, kept off the road.
        let mut rng = Rng(u64::from(seed) * 7 + 3);
        let ground = |x: f32, z: f32| {
            let (gx, gz) = ((x / CELL) as usize, (z / CELL) as usize);
            h[gz.min(CELLS) * n + gx.min(CELLS)]
        };
        let road_distance = |x: f32, z: f32| {
            road.iter().map(|q| Vec2::new(q.x, q.z).distance(Vec2::new(x, z))).fold(f32::MAX, f32::min)
        };
        let mut trees: u16 = 0;
        while trees < 420 {
            let (x, z) = (rng.range(10.0, size - 10.0), rng.range(10.0, size - 10.0));
            let y = ground(x, z);
            if road_distance(x, z) < 12.0 || y > 95.0 {
                continue;
            }
            let height = rng.range(6.0, 12.0);
            mesh.object = 1 + trees;
            mesh.pyramid(Vec3::new(x, y - 0.5, z), height * 0.28, height, FIR);
            trees += 1;
        }
        for k in 0..28 {
            let i = (k * ROAD_POINTS / 28 + rng.range(0.0, 10.0) as usize) % ROAD_POINTS;
            let offset = if k % 2 == 0 { 1.0 } else { -1.0 } * rng.range(15.0, 26.0);
            let c = road[i] + side(i) * offset;
            let (w, d, tall) = (rng.range(7.0, 14.0), rng.range(7.0, 12.0), rng.range(5.0, 16.0));
            let base = ground(c.x, c.z) - 1.0;
            let walls = [[196, 176, 150], [170, 172, 180], [210, 196, 170]][k % 3];
            let roof = [[150, 70, 56], [90, 92, 100], [120, 60, 50]][k % 3];
            mesh.object = 1000 + k as u16;
            mesh.cuboid(Vec3::new(c.x - w / 2.0, base, c.z - d / 2.0), Vec3::new(c.x + w / 2.0, base + tall, c.z + d / 2.0), walls, roof);
        }

        mesh.object = 0;
        let mut road_len = Vec::with_capacity(ROAD_POINTS + 1);
        let mut total = 0.0;
        road_len.push(0.0);
        for i in 0..ROAD_POINTS {
            total += road[i].distance(road[(i + 1) % ROAD_POINTS]);
            road_len.push(total);
        }
        Self { mesh, road, road_len }
    }

    /// Length of the road loop (metres).
    pub fn road_length(&self) -> f32 {
        *self.road_len.last().unwrap_or(&1.0)
    }

    /// Point on the road `s` metres from the start, looping.
    pub fn on_road(&self, s: f32) -> OnRoad {
        let s = s.rem_euclid(self.road_length());
        let i = self.road_len.partition_point(|&l| l <= s).saturating_sub(1).min(ROAD_POINTS - 1);
        let (a, b) = (self.road[i], self.road[(i + 1) % ROAD_POINTS]);
        let t = (s - self.road_len[i]) / (self.road_len[i + 1] - self.road_len[i]).max(1e-3);
        let pos = a.lerp(b, t);
        // Smooth heading: look a few metres ahead.
        let ahead = self.road[(i + 4) % ROAD_POINTS] - self.road[(i + ROAD_POINTS - 2) % ROAD_POINTS];
        let dir = Vec3::new(ahead.x, 0.0, ahead.z).normalize_or(Vec3::Z);
        OnRoad { pos, dir }
    }
}

fn ground_color(height: f32, up: f32, jitter: f32) -> [u8; 3] {
    let base = if height > 100.0 {
        SNOW
    } else if up < 0.82 {
        ROCK
    } else if height > 70.0 {
        DRY
    } else {
        GRASS
    };
    // A little variation so large fields are not one flat colour.
    shade(base, 0.92 + jitter * 0.16)
}

pub const CAR_OBJECT: u16 = u16::MAX;

/// A simple car (body, cabin, wheels) at `pos` facing `dir`.
pub fn car(pos: Vec3, dir: Vec3, color: [u8; 3]) -> Mesh {
    let forward = dir.normalize();
    // Local axes (x, y, z) map to (side, up, forward) with side = up ×
    // forward, which keeps the faces' winding (a mirror would turn the car
    // inside out). The car is symmetric, so which side is x does not matter.
    let right = Vec3::Y.cross(forward).normalize();
    let mut local = Mesh { object: CAR_OBJECT, ..Mesh::default() };
    local.cuboid(Vec3::new(-0.9, 0.35, -2.1), Vec3::new(0.9, 0.95, 2.1), color, color);
    local.cuboid(Vec3::new(-0.75, 0.95, -1.2), Vec3::new(0.75, 1.45, 0.7), shade(color, 0.8), [40, 44, 52]);
    for (x, z) in [(-0.95, -1.35), (0.95, -1.35), (-0.95, 1.35), (0.95, 1.35)] {
        local.cuboid(Vec3::new(x - 0.12, 0.0, z - 0.35), Vec3::new(x + 0.12, 0.6, z + 0.35), [24, 24, 26], [24, 24, 26]);
    }
    // Local x = right, z = forward.
    let place = |p: Vec3| pos + right * p.x + Vec3::Y * p.y + forward * p.z;
    Mesh {
        tris: local.tris.into_iter().map(|t| Triangle { p: t.p.map(place), ..t }).collect(),
        object: CAR_OBJECT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faces_point_outward() {
        let mut m = Mesh::default();
        m.cuboid(Vec3::ZERO, Vec3::ONE, [255; 3], [255; 3]);
        let centre = Vec3::splat(0.5);
        for t in &m.tris {
            let n = (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]);
            let mid = (t.p[0] + t.p[1] + t.p[2]) / 3.0;
            assert!(n.dot(mid - centre) > 0.0, "inward face {:?}", t.p);
        }
    }

    #[test]
    fn placed_car_faces_point_outward() {
        let dir = Vec3::new(0.6, 0.0, -0.8);
        let m = car(Vec3::new(5.0, 1.0, 3.0), dir, [255; 3]);
        // The first box is the body; its centre is 0.65 m above the origin.
        let centre = Vec3::new(5.0, 1.65, 3.0);
        for t in &m.tris[..10] {
            let n = (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]);
            let mid = (t.p[0] + t.p[1] + t.p[2]) / 3.0;
            assert!(n.dot(mid - centre) > 0.0);
        }
    }

    #[test]
    fn road_loops_and_stays_on_the_ground() {
        let w = World::generate(1);
        assert!(w.road_length() > 1000.0);
        let a = w.on_road(0.0).pos;
        let b = w.on_road(w.road_length()).pos;
        assert!(a.distance(b) < 1.0, "loop closes");
        assert!((w.on_road(123.0).dir.length() - 1.0).abs() < 1e-3);
    }
}

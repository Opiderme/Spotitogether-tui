//! What a map is made of, before it becomes triangles: a height grid, a
//! land-cover grid (forest, field, water…), roads, buildings and single
//! trees. OpenStreetMap ([`crate::osm`]) and the generator below both
//! produce this; [`crate::world`] turns it into a drivable world.
//!
//! Coordinates are metres: x east, z south (so north is -z), y up. The map
//! covers `[0, size]` on both axes.

use glam::{Vec2, Vec3};

use crate::noise::{self, Rng};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cover {
    Grass,
    Meadow,
    Forest,
    Farmland,
    Residential,
    Sand,
    Rock,
    Water,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoadKind {
    Major,
    Minor,
    Track,
}

#[derive(Debug, Clone)]
pub struct Road {
    pub points: Vec<Vec2>,
    pub width: f32,
    pub kind: RoadKind,
}

#[derive(Debug, Clone)]
pub struct Building {
    /// Footprint, either winding.
    pub outline: Vec<Vec2>,
    pub height: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Tree {
    pub at: Vec2,
    pub conifer: bool,
}

pub struct MapData {
    pub name: String,
    pub size: f32,
    /// Grid spacing (metres); `heights` has `n × n` values, `cover`
    /// `(n - 1) × (n - 1)`, with n = size / cell + 1.
    pub cell: f32,
    pub heights: Vec<f32>,
    pub cover: Vec<Cover>,
    pub roads: Vec<Road>,
    pub buildings: Vec<Building>,
    pub trees: Vec<Tree>,
    /// Share of conifers in generated forests (from the latitude or the
    /// data).
    pub conifers: f32,
    pub seed: u32,
}

impl MapData {
    pub fn n(&self) -> usize {
        (self.size / self.cell).round() as usize + 1
    }

    /// Ground height at grid vertex (gx, gz).
    pub fn height_at(&self, gx: usize, gz: usize) -> f32 {
        let n = self.n();
        self.heights[gz.min(n - 1) * n + gx.min(n - 1)]
    }

    /// Bilinear ground height at a point.
    pub fn height(&self, p: Vec2) -> f32 {
        let n = self.n();
        let g = (p / self.cell).clamp(Vec2::ZERO, Vec2::splat((n - 1) as f32 - 1e-3));
        let (x, z) = (g.x as usize, g.y as usize);
        let (fx, fz) = (g.x.fract(), g.y.fract());
        let h = |x, z| self.height_at(x, z);
        let top = h(x, z) + (h(x + 1, z) - h(x, z)) * fx;
        let bottom = h(x, z + 1) + (h(x + 1, z + 1) - h(x, z + 1)) * fx;
        top + (bottom - top) * fz
    }

    pub fn cover_at(&self, p: Vec2) -> Cover {
        let cells = self.n() - 1;
        let (x, z) = ((p.x / self.cell) as usize, (p.y / self.cell) as usize);
        self.cover[z.min(cells - 1) * cells + x.min(cells - 1)]
    }

    /// A procedural map: hills, a lake in a hollow, forests, fields, a road
    /// loop with a few houses. Used when no place is given.
    pub fn generate(seed: u32) -> Self {
        const SIZE: f32 = 960.0;
        const CELL: f32 = 6.0;
        let n = (SIZE / CELL) as usize + 1;
        let centre = Vec2::splat(SIZE / 2.0);
        let raw = |p: Vec2| {
            let base = noise::fbm(p / 260.0, seed);
            let ridges = 1.0 - (noise::fbm(p / 140.0, seed + 17) * 2.0 - 1.0).abs();
            base * 90.0 + ridges * 25.0
        };
        let mut heights = vec![0.0; n * n];
        for gz in 0..n {
            for gx in 0..n {
                heights[gz * n + gx] = raw(Vec2::new(gx as f32, gz as f32) * CELL);
            }
        }

        // Land cover from noise and height: a lake in the lowest hollow,
        // forest patches, fields on gentle ground, rock on steep slopes.
        let lake_level = {
            let mut sorted = heights.clone();
            sorted.sort_by(f32::total_cmp);
            sorted[sorted.len() / 14]
        };
        let cells = n - 1;
        let mut cover = vec![Cover::Grass; cells * cells];
        for cz in 0..cells {
            for cx in 0..cells {
                let p = Vec2::new(cx as f32 + 0.5, cz as f32 + 0.5) * CELL;
                let corner = |dx: usize, dz: usize| heights[(cz + dz) * n + cx + dx];
                let h = (corner(0, 0) + corner(1, 0) + corner(0, 1) + corner(1, 1)) / 4.0;
                let slope = (corner(1, 0) - corner(0, 0)).abs().max((corner(0, 1) - corner(0, 0)).abs()) / CELL;
                let woods = noise::fbm(p / 120.0, seed + 5);
                let fields = noise::fbm(p / 160.0, seed + 9);
                cover[cz * cells + cx] = if h < lake_level {
                    Cover::Water
                } else if h < lake_level + 1.2 {
                    Cover::Sand
                } else if slope > 0.7 {
                    Cover::Rock
                } else if woods > 0.56 {
                    Cover::Forest
                } else if fields > 0.55 && slope < 0.25 {
                    Cover::Farmland
                } else if woods < 0.38 {
                    Cover::Meadow
                } else {
                    Cover::Grass
                };
            }
        }

        // A wobbly road loop around the centre.
        let road: Vec<Vec2> = (0..720)
            .map(|i| {
                let t = i as f32 / 720.0 * std::f32::consts::TAU;
                let r = SIZE * 0.3 * (1.0 + 0.2 * (3.0 * t + 1.0).sin() + 0.12 * (5.0 * t + 2.0).sin());
                centre + Vec2::new(t.cos(), t.sin()) * r
            })
            .chain(std::iter::once(centre + Vec2::new(SIZE * 0.3 * (1.0 + 0.2 * 1f32.sin() + 0.12 * 2f32.sin()), 0.0)))
            .collect();

        // Houses along the road.
        let mut rng = Rng(u64::from(seed) * 7 + 3);
        let mut buildings = Vec::new();
        for k in 0..26 {
            let i = (k * 720 / 26 + rng.range(0.0, 10.0) as usize) % 720;
            let (a, b) = (road[i], road[(i + 1) % 720]);
            let along = (b - a).normalize();
            let side = Vec2::new(-along.y, along.x) * if k % 2 == 0 { 1.0 } else { -1.0 };
            let c = a + side * rng.range(16.0, 26.0);
            let (w, d) = (rng.range(7.0, 13.0) / 2.0, rng.range(6.0, 10.0) / 2.0);
            let outline = vec![c - along * w - side * d, c + along * w - side * d, c + along * w + side * d, c - along * w + side * d];
            buildings.push(Building { outline, height: rng.range(5.0, 14.0) });
        }

        Self {
            name: "monde généré".into(),
            size: SIZE,
            cell: CELL,
            heights,
            cover,
            roads: vec![Road { points: road, width: 8.0, kind: RoadKind::Major }],
            buildings,
            trees: Vec::new(),
            conifers: 0.6,
            seed,
        }
    }
}

/// Point on a 3D polyline helper: horizontal position with a height.
pub fn at_height(p: Vec2, y: f32) -> Vec3 {
    Vec3::new(p.x, y, p.y)
}

/// Even-odd point-in-polygon test.
pub fn inside(p: Vec2, polygon: &[Vec2]) -> bool {
    let mut inside = false;
    let mut j = polygon.len().wrapping_sub(1);
    for i in 0..polygon.len() {
        let (a, b) = (polygon[i], polygon[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_map_has_every_kind_of_land() {
        let m = MapData::generate(7);
        assert_eq!(m.heights.len(), m.n() * m.n());
        assert_eq!(m.cover.len(), (m.n() - 1) * (m.n() - 1));
        for kind in [Cover::Water, Cover::Forest, Cover::Farmland, Cover::Grass] {
            let share = m.cover.iter().filter(|&&c| c == kind).count() as f32 / m.cover.len() as f32;
            assert!(share > 0.02, "{kind:?}: {share:.3}");
        }
        assert!((m.height(Vec2::new(3.0, 3.0)) - m.height_at(0, 0)).abs() < 20.0);
    }

    #[test]
    fn point_in_polygon() {
        let square = [Vec2::ZERO, Vec2::new(10.0, 0.0), Vec2::new(10.0, 10.0), Vec2::new(0.0, 10.0)];
        assert!(inside(Vec2::new(5.0, 5.0), &square));
        assert!(!inside(Vec2::new(15.0, 5.0), &square));
    }
}

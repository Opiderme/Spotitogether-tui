//! Surface materials and their procedural textures. At a couple of hundred
//! pixels wide a bitmap would be a blur, so each material is a pattern
//! computed from the world position: patches of grass, rows in fields,
//! ripples and reflections on water, windows on walls. Patterns use few
//! distinct shades, which keeps the terminal output small.

use glam::{Vec2, Vec3};

use crate::noise;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Material {
    Grass,
    Meadow,
    ForestFloor,
    Farmland,
    Residential,
    Sand,
    Rock,
    Snow,
    Water,
    Asphalt,
    Dirt,
    Marking,
    Wall,
    Roof,
    Bark,
    Needles,
    Leaves,
    Paint,
    Glass,
    Tyre,
    /// Shines by itself: not lit, not shadowed.
    Lamp,
}

impl Material {
    pub fn is_emissive(self) -> bool {
        self == Self::Lamp
    }

    /// Whether the car collides with it (water and foliage do not stop it).
    pub fn is_solid(self) -> bool {
        !matches!(self, Self::Water | Self::Needles | Self::Leaves | Self::Marking)
    }

    /// Typical colour, tinted per triangle by the caller.
    pub fn color(self) -> [u8; 3] {
        match self {
            Self::Grass => [88, 142, 62],
            Self::Meadow => [128, 150, 70],
            Self::ForestFloor => [62, 88, 46],
            Self::Farmland => [170, 150, 86],
            Self::Residential => [120, 128, 104],
            Self::Sand => [214, 196, 146],
            Self::Rock => [128, 120, 112],
            Self::Snow => [235, 238, 242],
            Self::Water => [44, 92, 130],
            Self::Asphalt => [70, 70, 76],
            Self::Dirt => [132, 108, 78],
            Self::Marking => [228, 228, 210],
            Self::Wall => [200, 182, 156],
            Self::Roof => [150, 72, 56],
            Self::Bark => [92, 66, 44],
            Self::Needles => [36, 86, 50],
            Self::Leaves => [70, 124, 52],
            Self::Paint => [200, 40, 48],
            Self::Glass => [60, 80, 100],
            Self::Tyre => [26, 26, 28],
            Self::Lamp => [255, 240, 200],
        }
    }
}

/// What a texture does to a lit pixel.
pub struct Texel {
    /// Multiplies the surface colour.
    pub tint: f32,
    /// Light of its own (lit windows at night), added after lighting.
    pub glow: Option<[u8; 3]>,
    /// Water: how much of the sky it reflects (0–1).
    pub reflect: f32,
}

const PLAIN: Texel = Texel { tint: 1.0, glow: None, reflect: 0.0 };

/// Pattern of `material` at world position `p` with normal `n`. `time` (s)
/// animates water; `night` (0–1) lights windows.
pub fn texel(material: Material, p: Vec3, n: Vec3, time: f32, night: f32) -> Texel {
    let xz = Vec2::new(p.x, p.z);
    let steps = |v: f32, levels: f32| (v * levels).round() / levels;
    match material {
        Material::Grass | Material::Meadow | Material::Residential => {
            let v = noise::value(xz / 6.0, 11) * 0.6 + noise::value(xz / 1.7, 12) * 0.4;
            Texel { tint: 0.86 + steps(v, 3.0) * 0.26, ..PLAIN }
        }
        Material::ForestFloor => {
            let v = noise::value(xz / 2.5, 21);
            Texel { tint: 0.8 + steps(v, 3.0) * 0.3, ..PLAIN }
        }
        Material::Farmland => {
            // Rows a couple of metres apart, the direction changing per field.
            let field = (noise::value(xz / 90.0, 31) * 4.0).floor();
            let angle = field * 0.8;
            let along = xz.dot(Vec2::new(angle.cos(), angle.sin()));
            let row = (along / 2.2).rem_euclid(1.0) < 0.5;
            Texel { tint: if row { 0.84 } else { 1.06 }, ..PLAIN }
        }
        Material::Sand => {
            let v = noise::value(xz / 3.0, 41);
            Texel { tint: 0.92 + steps(v, 2.0) * 0.12, ..PLAIN }
        }
        Material::Rock => {
            let v = noise::value(Vec2::new(p.x + p.z, p.y * 2.0) / 3.0, 51);
            Texel { tint: 0.78 + steps(v, 3.0) * 0.36, ..PLAIN }
        }
        Material::Water => {
            // Moving ripples break up the reflection.
            let ripple = noise::value(xz / 4.0 + Vec2::new(time * 0.35, time * 0.2), 61)
                + noise::value(xz / 1.6 - Vec2::new(time * 0.5, 0.0), 62) * 0.5;
            Texel { tint: 0.9 + steps(ripple / 1.5, 3.0) * 0.2, reflect: 0.35 + steps(ripple / 1.5, 3.0) * 0.3, glow: None }
        }
        Material::Asphalt | Material::Dirt => {
            let v = noise::value(xz / 1.3, 71);
            Texel { tint: 0.93 + steps(v, 2.0) * 0.12, ..PLAIN }
        }
        Material::Wall => window(p, n, night),
        Material::Roof => {
            // Tile rows.
            let row = (p.y / 0.45).rem_euclid(1.0) < 0.5;
            Texel { tint: if row { 0.92 } else { 1.04 }, ..PLAIN }
        }
        Material::Needles | Material::Leaves => {
            let v = noise::value(Vec2::new(p.x + p.y, p.z - p.y) / 0.9, 81);
            Texel { tint: 0.78 + steps(v, 3.0) * 0.4, ..PLAIN }
        }
        _ => PLAIN,
    }
}

/// Windows on a wall: a grid of 3 m floors, dark panes by day, some lit at
/// night (fixed per window).
fn window(p: Vec3, n: Vec3, night: f32) -> Texel {
    // Horizontal coordinate along the wall.
    let along = Vec2::new(-n.z, n.x).normalize_or(Vec2::X).dot(Vec2::new(p.x, p.z));
    let (u, v) = (along / 3.2, p.y / 3.0);
    let (fu, fv) = (u.rem_euclid(1.0), v.rem_euclid(1.0));
    let pane = (0.28..0.72).contains(&fu) && (0.3..0.78).contains(&fv);
    if !pane || n.y.abs() > 0.5 {
        return PLAIN;
    }
    let lit = noise::hash(u.floor() as i32, v.floor() as i32, 91) < 0.45;
    Texel {
        tint: 0.45,
        glow: (night > 0.3 && lit).then_some([255, 206, 120]),
        reflect: 0.0,
    }
}

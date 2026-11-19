//! Triangles and the models built from them: boxes, cones, trees, buildings
//! extruded from their footprint, the car.

use glam::{Quat, Vec2, Vec3};

use crate::material::Material;

/// One flat triangle in world space (metres, Y up). Vertices are
/// counter-clockwise seen from the front face.
#[derive(Debug, Clone, Copy)]
pub struct Triangle {
    pub p: [Vec3; 3],
    /// The surface's own colour, before texture and lighting.
    pub color: [u8; 3],
    pub material: Material,
    /// Which object it belongs to (0 = the ground and roads): the ASCII
    /// look outlines the borders between objects.
    pub object: u16,
}

/// A material with its colour.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    pub material: Material,
    pub color: [u8; 3],
}

impl Surface {
    pub fn of(material: Material) -> Self {
        Self { material, color: material.color() }
    }

    /// The material's colour, lightened (> 1) or darkened (< 1).
    pub fn tinted(material: Material, k: f32) -> Self {
        Self { material, color: shade(material.color(), k) }
    }
}

pub fn shade(c: [u8; 3], k: f32) -> [u8; 3] {
    c.map(|v| (f32::from(v) * k).clamp(0.0, 255.0) as u8)
}

#[derive(Default)]
pub struct Mesh {
    pub tris: Vec<Triangle>,
    /// Object id given to the triangles added from now on.
    pub object: u16,
}

impl Mesh {
    /// Adds a triangle facing `outward`.
    pub fn tri(&mut self, a: Vec3, b: Vec3, c: Vec3, outward: Vec3, s: Surface) {
        let n = (b - a).cross(c - a);
        if n.length_squared() < 1e-12 {
            return;
        }
        let (b, c) = if n.dot(outward) < 0.0 { (c, b) } else { (b, c) };
        self.tris.push(Triangle { p: [a, b, c], color: s.color, material: s.material, object: self.object });
    }

    /// Quad `a b c d` (in order around its edge) facing `outward`.
    pub fn quad(&mut self, a: Vec3, b: Vec3, c: Vec3, d: Vec3, outward: Vec3, s: Surface) {
        self.tri(a, b, c, outward, s);
        self.tri(a, c, d, outward, s);
    }

    /// Axis-aligned box, with its own surface for the top.
    pub fn cuboid(&mut self, min: Vec3, max: Vec3, side: Surface, top: Surface) {
        let [x0, y0, z0] = min.to_array();
        let [x1, y1, z1] = max.to_array();
        let v = |x, y, z| Vec3::new(x, y, z);
        self.quad(v(x0, y1, z0), v(x1, y1, z0), v(x1, y1, z1), v(x0, y1, z1), Vec3::Y, top);
        self.quad(v(x0, y0, z1), v(x1, y0, z1), v(x1, y1, z1), v(x0, y1, z1), Vec3::Z, side);
        self.quad(v(x0, y0, z0), v(x1, y0, z0), v(x1, y1, z0), v(x0, y1, z0), -Vec3::Z, side);
        self.quad(v(x1, y0, z0), v(x1, y0, z1), v(x1, y1, z1), v(x1, y1, z0), Vec3::X, side);
        self.quad(v(x0, y0, z0), v(x0, y0, z1), v(x0, y1, z1), v(x0, y1, z0), -Vec3::X, side);
    }

    /// Cone without a base: `sides` faces from a ring to the apex.
    pub fn cone(&mut self, base: Vec3, radius: f32, height: f32, sides: usize, twist: f32, s: Surface) {
        let apex = base + Vec3::Y * height;
        let ring = |i: usize| {
            let a = twist + i as f32 / sides as f32 * std::f32::consts::TAU;
            base + Vec3::new(a.cos(), 0.0, a.sin()) * radius
        };
        for i in 0..sides {
            let (a, b) = (ring(i), ring(i + 1));
            let outward = ((a + b) * 0.5 - base).normalize() + Vec3::Y * (radius / height);
            self.tri(a, b, apex, outward, s);
        }
    }

    /// Double cone (a rough ball of leaves): `sides` faces up, `sides` down.
    pub fn crown(&mut self, centre: Vec3, radius: f32, height: f32, sides: usize, twist: f32, s: Surface) {
        let (top, bottom) = (centre + Vec3::Y * height * 0.55, centre - Vec3::Y * height * 0.45);
        let ring = |i: usize| {
            let a = twist + i as f32 / sides as f32 * std::f32::consts::TAU;
            centre + Vec3::new(a.cos(), 0.0, a.sin()) * radius
        };
        for i in 0..sides {
            let (a, b) = (ring(i), ring(i + 1));
            let out = ((a + b) * 0.5 - centre).normalize();
            self.tri(a, b, top, out + Vec3::Y, s);
            self.tri(a, b, bottom, out - Vec3::Y, Surface { color: shade(s.color, 0.85), ..s });
        }
    }

    /// Walls from `outline` between heights `bottom` and `top`, and a flat
    /// roof.
    pub fn prism(&mut self, outline: &[Vec2], bottom: f32, top: f32, wall: Surface, roof: Surface) {
        let ccw = signed_area(outline) > 0.0;
        for i in 0..outline.len() {
            let (a, b) = (outline[i], outline[(i + 1) % outline.len()]);
            let edge = b - a;
            if edge.length_squared() < 1e-4 {
                continue;
            }
            // Positive signed area: the inside is left of each edge (in
            // the (x, z) plane), so outward is to the right.
            let side = Vec2::new(edge.y, -edge.x) * if ccw { 1.0 } else { -1.0 };
            let out = Vec3::new(side.x, 0.0, side.y);
            let v = |p: Vec2, y: f32| Vec3::new(p.x, y, p.y);
            self.quad(v(a, bottom), v(b, bottom), v(b, top), v(a, top), out, wall);
        }
        for [a, b, c] in triangulate(outline) {
            let v = |p: Vec2| Vec3::new(p.x, top, p.y);
            self.tri(v(a), v(b), v(c), Vec3::Y, roof);
        }
    }
}

fn signed_area(poly: &[Vec2]) -> f32 {
    (0..poly.len()).map(|i| poly[i].perp_dot(poly[(i + 1) % poly.len()])).sum::<f32>() / 2.0
}

/// Ear clipping: triangles covering a simple polygon (either winding).
pub fn triangulate(poly: &[Vec2]) -> Vec<[Vec2; 3]> {
    let mut idx: Vec<usize> = (0..poly.len()).collect();
    if signed_area(poly) < 0.0 {
        idx.reverse();
    }
    let mut out = Vec::new();
    let mut guard = 0;
    while idx.len() > 3 && guard < poly.len() * poly.len() {
        guard += 1;
        let n = idx.len();
        let ear = (0..n).find(|&i| {
            let (a, b, c) = (poly[idx[(i + n - 1) % n]], poly[idx[i]], poly[idx[(i + 1) % n]]);
            if (b - a).perp_dot(c - b) <= 0.0 {
                return false;
            }
            !idx.iter().any(|&j| {
                let p = poly[j];
                p != a && p != b && p != c && in_triangle(p, a, b, c)
            })
        });
        match ear {
            Some(i) => {
                out.push([poly[idx[(i + n - 1) % n]], poly[idx[i]], poly[idx[(i + 1) % n]]]);
                idx.remove(i);
            }
            None => break,
        }
    }
    if idx.len() == 3 {
        out.push([poly[idx[0]], poly[idx[1]], poly[idx[2]]]);
    }
    out
}

fn in_triangle(p: Vec2, a: Vec2, b: Vec2, c: Vec2) -> bool {
    let d1 = (b - a).perp_dot(p - a);
    let d2 = (c - b).perp_dot(p - b);
    let d3 = (a - c).perp_dot(p - c);
    d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0
}

// ------------------------------------------------------------------ trees

/// A fir: thin trunk and two stacked cones. `variant` (0–1) varies the
/// shade and the rotation.
pub fn conifer(mesh: &mut Mesh, at: Vec3, height: f32, variant: f32) {
    let trunk = height * 0.22;
    let r = height * 0.05 + 0.1;
    mesh.cuboid(at - Vec3::new(r, 0.5, r), at + Vec3::new(r, trunk, r), Surface::of(Material::Bark), Surface::of(Material::Bark));
    let leaves = Surface::tinted(Material::Needles, 0.85 + variant * 0.3);
    let twist = variant * 6.0;
    mesh.cone(at + Vec3::Y * trunk, height * 0.3, height * 0.55, 6, twist, leaves);
    mesh.cone(at + Vec3::Y * (trunk + height * 0.35), height * 0.21, height * 0.43, 6, twist + 0.5, leaves);
}

/// A broadleaf tree: trunk and a rounded crown.
pub fn broadleaf(mesh: &mut Mesh, at: Vec3, height: f32, variant: f32) {
    let trunk = height * 0.42;
    let r = height * 0.05 + 0.12;
    mesh.cuboid(at - Vec3::new(r, 0.5, r), at + Vec3::new(r, trunk + 0.5, r), Surface::of(Material::Bark), Surface::of(Material::Bark));
    let leaves = Surface::tinted(Material::Leaves, 0.85 + variant * 0.3);
    mesh.crown(at + Vec3::Y * (trunk + height * 0.25), height * 0.36, height * 0.62, 6, variant * 6.0, leaves);
}

/// Far away: a single four-sided cone.
pub fn tree_far(mesh: &mut Mesh, at: Vec3, height: f32, conifer: bool, variant: f32) {
    let material = if conifer { Material::Needles } else { Material::Leaves };
    let (radius, start) = if conifer { (height * 0.3, height * 0.15) } else { (height * 0.36, height * 0.35) };
    mesh.cone(at + Vec3::Y * start, radius, height - start, 4, variant * 3.0, Surface::tinted(material, 0.85 + variant * 0.3));
}

// ------------------------------------------------------------------ car

pub const CAR_OBJECT: u16 = u16::MAX;

/// The physics car as triangles: body and cabin follow the chassis, each
/// wheel sits where its suspension puts it. Body-local X is forward.
pub fn car(pose: &crate::physics::CarPose, paint: [u8; 3]) -> Mesh {
    use crate::physics::{HALF_LENGTH, HALF_WIDTH, WHEEL_RADIUS};
    let body_paint = Surface { material: Material::Paint, color: paint };
    let mut local = Mesh { object: CAR_OBJECT, ..Mesh::default() };
    local.cuboid(Vec3::new(-HALF_LENGTH, -0.05, -HALF_WIDTH), Vec3::new(HALF_LENGTH, 0.55, HALF_WIDTH), body_paint, body_paint);
    local.cuboid(
        Vec3::new(-1.05, 0.55, -0.72),
        Vec3::new(0.55, 1.0, 0.72),
        Surface { material: Material::Paint, color: shade(paint, 0.8) },
        Surface::tinted(Material::Glass, 0.7),
    );
    local.cuboid(Vec3::new(0.55, 0.55, -0.7), Vec3::new(0.75, 0.62, 0.7), Surface::of(Material::Glass), Surface::of(Material::Glass));
    for z in [-0.8, 0.45] {
        let red = Surface { material: Material::Lamp, color: [255, 50, 40] };
        let white = Surface::of(Material::Lamp);
        local.cuboid(Vec3::new(-HALF_LENGTH - 0.03, 0.3, z), Vec3::new(-HALF_LENGTH, 0.45, z + 0.35), red, red);
        local.cuboid(Vec3::new(HALF_LENGTH, 0.28, z), Vec3::new(HALF_LENGTH + 0.03, 0.42, z + 0.35), white, white);
    }
    let body = |p: Vec3| pose.position + pose.rotation * p;
    let mut mesh = Mesh { object: CAR_OBJECT, ..Mesh::default() };
    mesh.tris.extend(local.tris.into_iter().map(|t| Triangle { p: t.p.map(body), ..t }));

    let tyre = Surface::of(Material::Tyre);
    for &(centre, steer) in &pose.wheels {
        let mut wheel = Mesh { object: CAR_OBJECT, ..Mesh::default() };
        let (r, w) = (WHEEL_RADIUS, 0.13);
        wheel.cuboid(Vec3::new(-r, -r, -w), Vec3::new(r, r, w), tyre, tyre);
        let turn = pose.rotation * Quat::from_rotation_y(steer);
        mesh.tris.extend(wheel.tris.into_iter().map(|t| Triangle { p: t.p.map(|p| centre + turn * p), ..t }));
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outward(mesh: &Mesh, centre: Vec3) {
        for t in &mesh.tris {
            let n = (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]);
            let mid = (t.p[0] + t.p[1] + t.p[2]) / 3.0;
            assert!(n.dot(mid - centre) > 0.0, "inward face {:?}", t.p);
        }
    }

    #[test]
    fn boxes_and_prisms_face_outward() {
        let mut m = Mesh::default();
        m.cuboid(Vec3::ZERO, Vec3::ONE, Surface::of(Material::Wall), Surface::of(Material::Roof));
        outward(&m, Vec3::splat(0.5));
        // An L-shaped house, both windings.
        let l = [Vec2::ZERO, Vec2::new(10.0, 0.0), Vec2::new(10.0, 4.0), Vec2::new(4.0, 4.0), Vec2::new(4.0, 10.0), Vec2::new(0.0, 10.0)];
        for outline in [l.to_vec(), l.iter().rev().copied().collect()] {
            let mut m = Mesh::default();
            m.prism(&outline, 0.0, 6.0, Surface::of(Material::Wall), Surface::of(Material::Roof));
            let roof: f32 = m.tris.iter().filter(|t| t.material == Material::Roof).map(|t| (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]).length() / 2.0).sum();
            assert!((roof - 64.0).abs() < 0.01, "roof covers the L: {roof}");
            assert!(m.tris.iter().filter(|t| t.material == Material::Roof).all(|t| (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]).y > 0.0));
            // Each wall faces away from the inside of the L.
            for t in m.tris.iter().filter(|t| t.material == Material::Wall) {
                let n = (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]).normalize();
                let mid = (t.p[0] + t.p[1] + t.p[2]) / 3.0;
                let probe = Vec2::new(mid.x + n.x * 0.1, mid.z + n.z * 0.1);
                assert!(!crate::map::inside(probe, &outline), "wall faces inward at {mid:?}");
            }
        }
    }

    #[test]
    fn placed_car_faces_point_outward() {
        let pose = crate::physics::CarPose {
            position: Vec3::new(5.0, 1.0, 3.0),
            rotation: Quat::from_rotation_y(0.7),
            wheels: vec![],
        };
        let m = car(&pose, [200, 40, 48]);
        outward(&Mesh { tris: m.tris[..10].to_vec(), object: 0 }, pose.position + pose.rotation * Vec3::new(0.0, 0.25, 0.0));
    }
}

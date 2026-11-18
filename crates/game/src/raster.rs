//! Software rasteriser: flat-shaded triangles into a small colour + depth
//! buffer, with distance fog. No GPU: the picture is a couple of hundred
//! pixels wide, which a single core fills in a few milliseconds.

use glam::Vec3;

use crate::world::Triangle;

pub struct Camera {
    pub eye: Vec3,
    pub target: Vec3,
    /// Vertical field of view (radians).
    pub fov_y: f32,
}

/// Fog starts at this distance and hides everything at `FAR` (metres).
pub const FOG_START: f32 = 140.0;
pub const FAR: f32 = 650.0;
const NEAR: f32 = 0.3;
/// Fog and sky use few distinct shades: neighbouring terminal cells then
/// share colours and the terminal receives fewer colour codes.
const FOG_STEPS: f32 = 10.0;

pub const SKY_TOP: [u8; 3] = [52, 92, 168];
pub const HORIZON: [u8; 3] = [176, 200, 222];

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub color: Vec<[u8; 3]>,
    /// 1 / distance, 0 = nothing drawn (sky).
    pub inv_depth: Vec<f32>,
    /// Pixel height / width: 1 for half blocks, about 2 for whole cells.
    pub pixel_aspect: f32,
    /// Object drawn at each pixel (see `Triangle::object`).
    pub object: Vec<u16>,
}

/// View-space basis of a camera.
struct View {
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    /// Points away from the scene (right-handed: the camera looks down -back).
    back: Vec3,
    /// Pixels per unit of tangent, horizontally and vertically.
    focal_x: f32,
    focal: f32,
    half_w: f32,
    half_h: f32,
}

impl View {
    fn to_view(&self, p: Vec3) -> Vec3 {
        let d = p - self.eye;
        Vec3::new(d.dot(self.right), d.dot(self.up), d.dot(self.back))
    }

    /// Screen position (pixels) and 1 / distance of a view-space point in
    /// front of the camera.
    fn project(&self, v: Vec3) -> (f32, f32, f32) {
        let dist = -v.z;
        let x = self.half_w + v.x / dist * self.focal_x;
        let y = self.half_h - v.y / dist * self.focal;
        (x, y, 1.0 / dist)
    }
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Self {
        Self { w, h, color: vec![[0; 3]; w * h], inv_depth: vec![0.0; w * h], pixel_aspect: 1.0, object: vec![0; w * h] }
    }

    pub fn resize(&mut self, w: usize, h: usize, pixel_aspect: f32) {
        if (w, h) != (self.w, self.h) {
            *self = Self::new(w, h);
        }
        self.pixel_aspect = pixel_aspect;
    }

    /// Draws `meshes` seen from `camera` over a sky gradient. Returns the
    /// number of triangles that reached the screen.
    pub fn render<'a>(&mut self, camera: &Camera, meshes: impl IntoIterator<Item = &'a [Triangle]>) -> usize {
        let forward = (camera.target - camera.eye).normalize();
        let right = forward.cross(Vec3::Y).normalize();
        let up = right.cross(forward);
        let focal = self.h as f32 / 2.0 / (camera.fov_y / 2.0).tan();
        let view = View {
            eye: camera.eye,
            right,
            up,
            back: -forward,
            focal_x: focal * self.pixel_aspect,
            focal,
            half_w: self.w as f32 / 2.0,
            half_h: self.h as f32 / 2.0,
        };
        self.sky(&view);
        let mut drawn = 0;
        for mesh in meshes {
            for t in mesh {
                drawn += usize::from(self.triangle(&view, t));
            }
        }
        drawn
    }

    fn sky(&mut self, view: &View) {
        self.inv_depth.fill(0.0);
        for y in 0..self.h {
            // Elevation of this row's ray at the screen centre.
            let ray = view.back * -view.focal + view.up * (view.half_h - y as f32);
            let elevation = (ray.normalize().dot(Vec3::Y)).max(0.0);
            let t = quantize((elevation * 2.5).min(1.0), 8.0);
            let c = mix(HORIZON, SKY_TOP, t);
            self.color[y * self.w..(y + 1) * self.w].fill(c);
        }
    }

    fn triangle(&mut self, view: &View, t: &Triangle) -> bool {
        let v = t.p.map(|p| view.to_view(p));
        // Back face: the front side points away from the camera.
        if (v[1] - v[0]).cross(v[2] - v[0]).dot(v[0]) >= 0.0 {
            return false;
        }
        if v.iter().all(|p| -p.z > FAR) || v.iter().all(|p| -p.z < NEAR) {
            return false;
        }
        // Quick reject when entirely beside the view.
        let slope_x = view.half_w / view.focal_x;
        let slope_y = view.half_h / view.focal;
        if v.iter().all(|p| p.x > -p.z * slope_x + 2.0)
            || v.iter().all(|p| p.x < p.z * slope_x - 2.0)
            || v.iter().all(|p| p.y > -p.z * slope_y + 2.0)
            || v.iter().all(|p| p.y < p.z * slope_y - 2.0)
        {
            return false;
        }

        let poly = clip_near(v);
        if poly.len() < 3 {
            return false;
        }
        let pts: Vec<(f32, f32, f32)> = poly.iter().map(|&p| view.project(p)).collect();
        for i in 1..pts.len() - 1 {
            self.fill(pts[0], pts[i], pts[i + 1], t.color, t.object);
        }
        true
    }

    fn fill(&mut self, a: (f32, f32, f32), b: (f32, f32, f32), c: (f32, f32, f32), color: [u8; 3], object: u16) {
        let area = edge(a, b, (c.0, c.1));
        if area.abs() < 1e-6 {
            return;
        }
        let min_x = a.0.min(b.0).min(c.0).floor().max(0.0) as usize;
        let max_x = (a.0.max(b.0).max(c.0).ceil() as isize).min(self.w as isize - 1);
        let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as usize;
        let max_y = (a.1.max(b.1).max(c.1).ceil() as isize).min(self.h as isize - 1);
        if max_x < 0 || max_y < 0 {
            return;
        }
        let inv_area = 1.0 / area;
        for y in min_y..=max_y as usize {
            let py = y as f32 + 0.5;
            for x in min_x..=max_x as usize {
                let p = (x as f32 + 0.5, py);
                let w0 = edge(b, c, p) * inv_area;
                let w1 = edge(c, a, p) * inv_area;
                let w2 = edge(a, b, p) * inv_area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let inv_z = w0 * a.2 + w1 * b.2 + w2 * c.2;
                let i = y * self.w + x;
                if inv_z <= self.inv_depth[i] {
                    continue;
                }
                self.inv_depth[i] = inv_z;
                self.object[i] = object;
                let fog = quantize(((1.0 / inv_z - FOG_START) / (FAR - FOG_START)).clamp(0.0, 1.0), FOG_STEPS);
                self.color[i] = mix(color, HORIZON, fog);
            }
        }
    }
}

fn edge(a: (f32, f32, f32), b: (f32, f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Sutherland–Hodgman against the near plane (view space, z = -NEAR).
fn clip_near(v: [Vec3; 3]) -> Vec<Vec3> {
    let inside = |p: Vec3| -p.z >= NEAR;
    let mut out = Vec::with_capacity(4);
    for i in 0..3 {
        let (cur, next) = (v[i], v[(i + 1) % 3]);
        if inside(cur) {
            out.push(cur);
        }
        if inside(cur) != inside(next) {
            let t = (-NEAR - cur.z) / (next.z - cur.z);
            out.push(cur.lerp(next, t));
        }
    }
    out
}

fn quantize(t: f32, steps: f32) -> f32 {
    (t * steps).round() / steps
}

pub fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [0, 1, 2].map(|k| (f32::from(a[k]) + (f32::from(b[k]) - f32::from(a[k])) * t) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::Mesh;

    fn camera() -> Camera {
        Camera { eye: Vec3::new(0.0, 2.0, 10.0), target: Vec3::new(0.0, 0.0, 0.0), fov_y: 1.1 }
    }

    #[test]
    fn draws_a_box_in_front_and_nothing_behind() {
        let mut box_mesh = Mesh::default();
        box_mesh.cuboid(Vec3::splat(-1.0), Vec3::splat(1.0), [200, 0, 0], [200, 0, 0]);
        let mut f = Frame::new(80, 40);
        assert!(f.render(&camera(), [box_mesh.tris.as_slice()]) > 0);
        let centre = f.color[20 * 80 + 40];
        assert!(centre[0] > centre[1] && f.inv_depth[20 * 80 + 40] > 0.0, "box at the centre: {centre:?}");

        let behind = Camera { target: Vec3::new(0.0, 2.0, 20.0), ..camera() };
        assert_eq!(f.render(&behind, [box_mesh.tris.as_slice()]), 0);
        assert!(f.inv_depth.iter().all(|&d| d == 0.0));
    }

    #[test]
    fn clips_triangles_crossing_the_camera() {
        // A large ground triangle under and around the camera.
        let ground = [Triangle {
            p: [Vec3::new(-50.0, 0.0, 50.0), Vec3::new(50.0, 0.0, 50.0), Vec3::new(0.0, 0.0, -80.0)],
            color: [0, 200, 0],
            object: 0,
        }];
        let mut f = Frame::new(60, 30);
        f.render(&Camera { eye: Vec3::new(0.0, 1.5, 0.0), target: Vec3::new(0.0, 1.0, -10.0), fov_y: 1.1 }, [ground.as_slice()]);
        // The bottom row sees the ground, the top row the sky.
        assert!(f.inv_depth[29 * 60 + 30] > 0.0);
        assert_eq!(f.inv_depth[30], 0.0);
    }
}

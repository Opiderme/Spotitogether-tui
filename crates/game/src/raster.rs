//! Software renderer, in two passes:
//! 1. geometry: triangles are rasterised into per-pixel buffers (surface
//!    colour, normal, position, depth, object);
//! 2. lighting: each pixel gets sunlight (unless a shadow map from the sun
//!    says something is in the way), ambient light, the headlights, then
//!    distance fog; empty pixels get the sky (sun disk, glow, stars).
//!
//! No GPU: the picture is a couple of hundred pixels wide.

use glam::Vec3;

use crate::lighting::{HEADLIGHT_COLOR, HEADLIGHT_COS_INNER, HEADLIGHT_COS_OUTER, HEADLIGHT_RANGE, Light, Rgb};
use crate::material::{self, Material};
use crate::mesh::{Triangle, shade};
use crate::world::World;

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
/// Fog, sky and light use few distinct shades: neighbouring terminal cells
/// then share colours and the terminal receives fewer colour codes.
const FOG_STEPS: f32 = 10.0;
const LIGHT_STEPS: f32 = 14.0;

/// Shadow map: covers a square of 2 × `SHADOW_HALF` metres around the
/// area in front of the camera, `SHADOW_SIZE` texels wide.
const SHADOW_SIZE: usize = 640;
const SHADOW_HALF: f32 = 150.0;
/// The static part of the shadow map is redrawn only when its centre moves
/// by this much (metres) or the sun by more than about 0.3°.
const SHADOW_STEP: f32 = 24.0;
/// Trees closer than this get their full model.
const TREE_DETAIL: f32 = 140.0;
const SUN_MOVED: f32 = 0.999_985;
/// Threads drawing the static shadow map.
const SHADOW_THREADS: usize = 8;
/// Chunks this far outside the shadow map can still cast into it.
const CHUNK_MARGIN: f32 = 64.0;

pub struct Frame {
    pub w: usize,
    pub h: usize,
    /// Final colours.
    pub color: Vec<[u8; 3]>,
    /// 1 / distance, 0 = nothing drawn (sky).
    pub inv_depth: Vec<f32>,
    /// Pixel height / width: 1 for half blocks, about 2 for whole cells.
    pub pixel_aspect: f32,
    /// Object drawn at each pixel (see `Triangle::object`).
    pub object: Vec<u16>,
    base: Vec<[u8; 3]>,
    normal: Vec<Vec3>,
    /// View-space position.
    position: Vec<Vec3>,
    material: Vec<Material>,
    shadow: ShadowMap,
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

    fn to_world(&self, v: Vec3) -> Vec3 {
        self.eye + self.right * v.x + self.up * v.y + self.back * v.z
    }

    /// Screen position (pixels) and 1 / distance of a view-space point in
    /// front of the camera.
    fn project(&self, v: Vec3) -> (f32, f32, f32) {
        let dist = -v.z;
        let x = self.half_w + v.x / dist * self.focal_x;
        let y = self.half_h - v.y / dist * self.focal;
        (x, y, 1.0 / dist)
    }

    /// World direction of the ray through pixel (x, y).
    fn ray(&self, x: usize, y: usize) -> Vec3 {
        let vx = (x as f32 + 0.5 - self.half_w) / self.focal_x;
        let vy = (self.half_h - y as f32 - 0.5) / self.focal;
        (self.right * vx + self.up * vy - self.back).normalize()
    }
}

/// A projected vertex: screen x, y, 1 / distance and view-space position.
#[derive(Clone, Copy)]
struct Vertex {
    x: f32,
    y: f32,
    iz: f32,
    v: Vec3,
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            w,
            h,
            color: vec![[0; 3]; n],
            inv_depth: vec![0.0; n],
            pixel_aspect: 1.0,
            object: vec![0; n],
            base: vec![[0; 3]; n],
            normal: vec![Vec3::Y; n],
            position: vec![Vec3::ZERO; n],
            material: vec![Material::Grass; n],
            shadow: ShadowMap::new(),
        }
    }

    pub fn resize(&mut self, w: usize, h: usize, pixel_aspect: f32) {
        if (w, h) != (self.w, self.h) {
            *self = Self::new(w, h);
        }
        self.pixel_aspect = pixel_aspect;
    }

    /// Draws `world` and the moving `dynamic` meshes seen from `camera`, lit
    /// by `light`. Returns the number of triangles that reached the screen.
    pub fn render(&mut self, camera: &Camera, light: &Light, world: &World, dynamic: &[&[Triangle]]) -> usize {
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

        self.inv_depth.fill(0.0);
        let mut meshes: Vec<&[Triangle]> = Vec::new();
        for chunk in &world.chunks {
            let dist = distance_to_box(camera.eye, chunk.min, chunk.max);
            if dist > FAR || !view.sees_box(chunk.min, chunk.max) {
                continue;
            }
            meshes.push(&chunk.base);
            meshes.push(if dist < TREE_DETAIL { &chunk.trees_near } else { &chunk.trees_far });
        }
        meshes.extend_from_slice(dynamic);
        let mut drawn = 0;
        for mesh in &meshes {
            for t in *mesh {
                drawn += usize::from(self.triangle(&view, t));
            }
        }

        let sunlit = light.sun_color.iter().any(|&c| c > 0.0);
        if sunlit {
            // Centre the map a little ahead of the camera, where most of
            // the visible ground is.
            let flat = Vec3::new(forward.x, 0.0, forward.z).normalize_or(Vec3::X);
            let centre = camera.eye + flat * SHADOW_HALF * 0.55;
            self.shadow.render(light.sun, centre, world, dynamic);
        }
        self.light(&view, light, sunlit);
        drawn
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
        let normal = (t.p[1] - t.p[0]).cross(t.p[2] - t.p[0]).normalize_or(Vec3::Y);
        let pts: Vec<Vertex> = poly
            .iter()
            .map(|&p| {
                let (x, y, iz) = view.project(p);
                Vertex { x, y, iz, v: p }
            })
            .collect();
        for i in 1..pts.len() - 1 {
            self.fill([pts[0], pts[i], pts[i + 1]], t, normal);
        }
        true
    }

    fn fill(&mut self, [a, b, c]: [Vertex; 3], t: &Triangle, normal: Vec3) {
        let area = edge(a, b, c.x, c.y);
        if area.abs() < 1e-6 {
            return;
        }
        let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as usize;
        let max_x = (a.x.max(b.x).max(c.x).ceil() as isize).min(self.w as isize - 1);
        let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as usize;
        let max_y = (a.y.max(b.y).max(c.y).ceil() as isize).min(self.h as isize - 1);
        if max_x < 0 || max_y < 0 {
            return;
        }
        let inv_area = 1.0 / area;
        // Perspective-correct position: interpolate position / distance.
        let (pa, pb, pc) = (a.v * a.iz, b.v * b.iz, c.v * c.iz);
        for y in min_y..=max_y as usize {
            let py = y as f32 + 0.5;
            for x in min_x..=max_x as usize {
                let px = x as f32 + 0.5;
                let w0 = edge(b, c, px, py) * inv_area;
                let w1 = edge(c, a, px, py) * inv_area;
                let w2 = edge(a, b, px, py) * inv_area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let iz = w0 * a.iz + w1 * b.iz + w2 * c.iz;
                let i = y * self.w + x;
                if iz <= self.inv_depth[i] {
                    continue;
                }
                self.inv_depth[i] = iz;
                self.object[i] = t.object;
                self.base[i] = t.color;
                self.normal[i] = normal;
                self.material[i] = t.material;
                self.position[i] = (pa * w0 + pb * w1 + pc * w2) / iz;
            }
        }
    }

    fn light(&mut self, view: &View, light: &Light, sunlit: bool) {
        let fog_color = light.sky_horizon;
        for y in 0..self.h {
            for x in 0..self.w {
                let i = y * self.w + x;
                if self.inv_depth[i] == 0.0 {
                    self.color[i] = sky(view.ray(x, y), light);
                    continue;
                }
                let dist = 1.0 / self.inv_depth[i];
                let material = self.material[i];
                let lit = if material.is_emissive() {
                    self.base[i]
                } else {
                    let p = view.to_world(self.position[i]);
                    let n = self.normal[i];
                    let tex = material::texel(material, p, n, light.time, light.stars);
                    let base = shade(self.base[i], tex.tint);
                    let mut l = light.ambient;
                    let facing = n.dot(light.sun);
                    if sunlit && facing > 0.0 {
                        let s = facing * self.shadow.lit(p, n, light.sun);
                        for (k, c) in l.iter_mut().enumerate() {
                            *c += light.sun_color[k] * s;
                        }
                    }
                    if let Some(h) = &light.headlights {
                        let beam = headlights(h, p, n);
                        for (k, c) in l.iter_mut().enumerate() {
                            *c += HEADLIGHT_COLOR[k] * beam;
                        }
                    }
                    let mut c = apply(base, l);
                    if tex.reflect > 0.0 {
                        // Water mirrors the sky, more at grazing angles.
                        let to = (p - view.eye).normalize();
                        let mirrored = Vec3::new(to.x, -to.y, to.z);
                        let fresnel = 0.25 + 0.75 * (1.0 - (-to.y).clamp(0.0, 1.0)).powi(3);
                        c = mix(c, sky(mirrored, light), quantize(tex.reflect * fresnel, 6.0));
                    }
                    if let Some(glow) = tex.glow {
                        c = [0, 1, 2].map(|k| c[k].max(glow[k]));
                    }
                    c
                };
                let fog = quantize(((dist - FOG_START) / (FAR - FOG_START)).clamp(0.0, 1.0), FOG_STEPS);
                self.color[i] = mix(lit, fog_color, fog);
            }
        }
    }
}

/// Distance from `p` to an axis-aligned box (0 inside).
fn distance_to_box(p: Vec3, min: Vec3, max: Vec3) -> f32 {
    (p.clamp(min, max) - p).length()
}

impl View {
    /// Whether any of the box may be on screen: false when all its corners
    /// are beyond one edge of the view.
    fn sees_box(&self, min: Vec3, max: Vec3) -> bool {
        let corners = [0, 1, 2, 3, 4, 5, 6, 7].map(|i| {
            self.to_view(Vec3::new(
                if i & 1 == 0 { min.x } else { max.x },
                if i & 2 == 0 { min.y } else { max.y },
                if i & 4 == 0 { min.z } else { max.z },
            ))
        });
        let slope_x = self.half_w / self.focal_x * 1.05;
        let slope_y = self.half_h / self.focal * 1.05;
        let outside = |f: &dyn Fn(Vec3) -> bool| corners.iter().all(|&c| f(c));
        !(outside(&|c| -c.z < NEAR)
            || outside(&|c| c.x > -c.z * slope_x)
            || outside(&|c| c.x < c.z * slope_x)
            || outside(&|c| c.y > -c.z * slope_y)
            || outside(&|c| c.y < c.z * slope_y))
    }
}

/// Surface colour times light, in steps.
fn apply(base: [u8; 3], light: Rgb) -> [u8; 3] {
    [0, 1, 2].map(|k| (f32::from(base[k]) * quantize(light[k].min(2.0), LIGHT_STEPS)).min(255.0) as u8)
}

/// Light from both headlamps on a surface at `p` facing `n`.
fn headlights(h: &crate::lighting::Headlights, p: Vec3, n: Vec3) -> f32 {
    let mut total = 0.0;
    for lamp in h.lamps {
        let to = p - lamp;
        let d = to.length();
        if !(0.5..=HEADLIGHT_RANGE).contains(&d) {
            continue;
        }
        let dir = to / d;
        let cone = dir.dot(h.dir);
        if cone < HEADLIGHT_COS_OUTER {
            continue;
        }
        let spot = ((cone - HEADLIGHT_COS_OUTER) / (HEADLIGHT_COS_INNER - HEADLIGHT_COS_OUTER)).clamp(0.0, 1.0);
        let falloff = (1.0 - d / HEADLIGHT_RANGE).powi(2);
        // Ground seen at a grazing angle still catches some light.
        let facing = (-dir).dot(n).max(0.0).sqrt();
        total += spot * falloff * facing;
    }
    total.min(1.3)
}

/// Sky colour along `dir`: gradient, glow around a low sun, sun disk, stars.
fn sky(dir: Vec3, light: &Light) -> [u8; 3] {
    let elevation = dir.y.max(0.0);
    let mut c = mix(light.sky_horizon, light.sky_top, quantize((elevation * 2.5).min(1.0), 8.0));
    let towards_sun = dir.dot(light.sun);
    if light.glow_amount > 0.0 && towards_sun > 0.0 {
        let glow = towards_sun.powi(6) * light.glow_amount * (1.0 - elevation * 2.0).max(0.0);
        c = mix(c, light.glow, quantize(glow.min(1.0), 6.0));
    }
    if light.sun.y > -0.05 && towards_sun > 0.9992 {
        return mix([255, 214, 140], [255, 250, 230], (light.sun.y * 3.0).clamp(0.0, 1.0));
    }
    if light.stars > 0.0 && dir.y > 0.02 {
        // Stars fixed in the sky: a hash of the quantised direction.
        let q = (dir * 220.0).round();
        let h = hash3(q.x as i32, q.y as i32, q.z as i32);
        if h > 1.0 - 0.006 * light.stars {
            let b = (150.0 + 105.0 * light.stars) as u8;
            return [b, b, b];
        }
    }
    c
}

fn hash3(x: i32, y: i32, z: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841) ^ (z as u32).wrapping_mul(0xcb1a_b31f);
    h ^= h >> 13;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 16;
    h as f32 / u32::MAX as f32
}

/// Depth of the scene seen from the sun (orthographic), to tell whether a
/// point is in the shade.
struct ShadowMap {
    depth: Vec<f32>,
    /// The static world alone, kept while the sun and the area stay put.
    world: Vec<f32>,
    cached: Option<(Vec3, Vec3)>,
    origin: Vec3,
    right: Vec3,
    up: Vec3,
    /// From the sun towards the scene.
    forward: Vec3,
}

type ShadowVertex = (f32, f32, f32);

impl ShadowMap {
    fn new() -> Self {
        Self {
            depth: vec![f32::MAX; SHADOW_SIZE * SHADOW_SIZE],
            world: vec![f32::MAX; SHADOW_SIZE * SHADOW_SIZE],
            cached: None,
            origin: Vec3::ZERO,
            right: Vec3::X,
            up: Vec3::Z,
            forward: -Vec3::Y,
        }
    }

    fn texel(&self) -> f32 {
        2.0 * SHADOW_HALF / SHADOW_SIZE as f32
    }

    /// (u, v) in texels and depth along the sun's rays.
    fn project(&self, p: Vec3) -> ShadowVertex {
        self.frame().project(p)
    }

    fn render(&mut self, sun: Vec3, centre: Vec3, world: &World, dynamic: &[&[Triangle]]) {
        // The map only moves in whole steps (which also keeps its texels
        // fixed on the ground: no shimmering while driving).
        let snap = |v: f32| (v / SHADOW_STEP).round() * SHADOW_STEP;
        let centre = Vec3::new(snap(centre.x), 0.0, snap(centre.z));
        let fresh = match self.cached {
            Some((s, c)) => s.dot(sun) > SUN_MOVED && c == centre,
            None => false,
        };
        if !fresh {
            self.forward = -sun;
            let helper = if sun.y.abs() > 0.99 { Vec3::X } else { Vec3::Y };
            self.right = self.forward.cross(helper).normalize();
            self.up = self.right.cross(self.forward);
            self.origin = centre - self.forward * 1000.0;
            self.depth.fill(f32::MAX);
            // Chunks under the map (seen from the sun, with some margin for
            // tall things).
            let reach = SHADOW_HALF * 1.5 + CHUNK_MARGIN;
            let mut meshes: Vec<&[Triangle]> = Vec::new();
            for chunk in &world.chunks {
                let mid = (chunk.min + chunk.max) / 2.0;
                let d = mid - self.origin;
                if d.dot(self.right).abs() > reach || d.dot(self.up).abs() > reach {
                    continue;
                }
                // Simple trees cast nearly the same shadow at a fraction of
                // the cost.
                meshes.push(&chunk.base);
                meshes.push(&chunk.trees_far);
            }
            // Bands of rows drawn in parallel: this runs about once a
            // second while driving and would otherwise stall a frame.
            let frame = self.frame();
            let rows = SHADOW_SIZE.div_ceil(SHADOW_THREADS);
            std::thread::scope(|scope| {
                for (k, band) in self.depth.chunks_mut(rows * SHADOW_SIZE).enumerate() {
                    let (meshes, frame) = (&meshes, &frame);
                    scope.spawn(move || {
                        for mesh in meshes {
                            draw_band(frame, mesh, band, k * rows);
                        }
                    });
                }
            });
            self.world.copy_from_slice(&self.depth);
            self.cached = Some((sun, centre));
        } else {
            self.depth.copy_from_slice(&self.world);
        }
        for mesh in dynamic {
            self.draw(mesh);
        }
    }

    fn draw(&mut self, mesh: &[Triangle]) {
        let frame = self.frame();
        draw_band(&frame, mesh, &mut self.depth, 0);
    }

    fn frame(&self) -> LightFrame {
        LightFrame { origin: self.origin, right: self.right, up: self.up, forward: self.forward, scale: 1.0 / self.texel() }
    }

    /// Fraction of sunlight reaching `p` (0 = full shade). Outside the map
    /// counts as lit.
    fn lit(&self, p: Vec3, n: Vec3, sun: Vec3) -> f32 {
        // Push the point along its normal, more on surfaces the sun grazes,
        // so a surface does not shadow itself.
        let texel = self.texel();
        let grazing = 1.0 - n.dot(sun).clamp(0.0, 1.0);
        let p = p + n * texel * (0.6 + 1.5 * grazing);
        let (u, v, d) = self.project(p);
        let size = SHADOW_SIZE as f32;
        if u < 1.0 || v < 1.0 || u >= size - 1.0 || v >= size - 1.0 {
            return 1.0;
        }
        let bias = texel * (0.4 + 1.2 * grazing);
        // Bilinear filtering of the four nearest comparisons: smooth edges
        // instead of texel-sized stairs.
        let (fu, fv) = (u - 0.5, v - 0.5);
        let (bu, bv) = (fu.floor() as usize, fv.floor() as usize);
        let (tu, tv) = (fu - fu.floor(), fv - fv.floor());
        let sample = |du: usize, dv: usize| f32::from(u8::from(d - bias <= self.depth[(bv + dv) * SHADOW_SIZE + bu + du]));
        let top = sample(0, 0) * (1.0 - tu) + sample(1, 0) * tu;
        let bottom = sample(0, 1) * (1.0 - tu) + sample(1, 1) * tu;
        top * (1.0 - tv) + bottom * tv
    }
}

/// The sun's view: orthographic, `scale` texels per metre.
#[derive(Clone, Copy)]
struct LightFrame {
    origin: Vec3,
    right: Vec3,
    up: Vec3,
    forward: Vec3,
    scale: f32,
}

impl LightFrame {
    fn project(&self, p: Vec3) -> ShadowVertex {
        let d = p - self.origin;
        let half = SHADOW_SIZE as f32 / 2.0;
        (d.dot(self.right) * self.scale + half, half - d.dot(self.up) * self.scale, d.dot(self.forward))
    }
}

/// Rasterises `mesh` depths into `band`, the shadow map rows from `row0`.
fn draw_band(frame: &LightFrame, mesh: &[Triangle], band: &mut [f32], row0: usize) {
    let size = SHADOW_SIZE as f32;
    let rows = band.len() / SHADOW_SIZE;
    let (top, bottom) = (row0 as f32, (row0 + rows) as f32);
    let e = |a: ShadowVertex, b: ShadowVertex, x: f32, y: f32| (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0);
    for t in mesh {
        if t.material.is_emissive() {
            continue;
        }
        let v = t.p.map(|p| frame.project(p));
        let (min_u, max_u) = (v[0].0.min(v[1].0).min(v[2].0), v[0].0.max(v[1].0).max(v[2].0));
        let (min_v, max_v) = (v[0].1.min(v[1].1).min(v[2].1), v[0].1.max(v[1].1).max(v[2].1));
        if max_u < 0.0 || min_u >= size || max_v < top || min_v >= bottom {
            continue;
        }
        let area = e(v[0], v[1], v[2].0, v[2].1);
        if area.abs() < 1e-6 {
            continue;
        }
        let inv = 1.0 / area;
        let x0 = min_u.floor().max(0.0) as usize;
        let x1 = (max_u.ceil() as usize).min(SHADOW_SIZE - 1);
        let y0 = (min_v.floor().max(top) as usize).max(row0);
        let y1 = (max_v.ceil() as usize).min(row0 + rows - 1);
        for y in y0..=y1 {
            let py = y as f32 + 0.5;
            let line = &mut band[(y - row0) * SHADOW_SIZE..(y - row0 + 1) * SHADOW_SIZE];
            for (x, cell) in line.iter_mut().enumerate().take(x1 + 1).skip(x0) {
                let px = x as f32 + 0.5;
                let w0 = e(v[1], v[2], px, py) * inv;
                let w1 = e(v[2], v[0], px, py) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let d = w0 * v[0].2 + w1 * v[1].2 + w2 * v[2].2;
                if d < *cell {
                    *cell = d;
                }
            }
        }
    }
}

fn edge(a: Vertex, b: Vertex, px: f32, py: f32) -> f32 {
    (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x)
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
    use crate::material::Material;
    use crate::mesh::{Mesh, Surface};

    fn camera() -> Camera {
        Camera { eye: Vec3::new(0.0, 2.0, 10.0), target: Vec3::new(0.0, 0.0, 0.0), fov_y: 1.1 }
    }

    fn noon() -> Light {
        Light::at(12.0)
    }

    fn plane(s: f32) -> Mesh {
        let mut m = Mesh::default();
        m.quad(Vec3::new(-s, 0.0, -s), Vec3::new(s, 0.0, -s), Vec3::new(s, 0.0, s), Vec3::new(-s, 0.0, s), Vec3::Y, Surface { material: Material::Paint, color: [200, 200, 200] });
        m
    }

    #[test]
    fn draws_a_box_in_front_and_nothing_behind() {
        let mut box_mesh = Mesh::default();
        box_mesh.cuboid(Vec3::splat(-1.0), Vec3::splat(1.0), Surface { material: Material::Paint, color: [200, 0, 0] }, Surface { material: Material::Paint, color: [200, 0, 0] });
        let mut f = Frame::new(80, 40);
        assert!(f.render(&camera(), &noon(), &World::from_triangles(box_mesh.tris.clone()), &[]) > 0);
        let centre = f.color[20 * 80 + 40];
        assert!(centre[0] > centre[1] && f.inv_depth[20 * 80 + 40] > 0.0, "box at the centre: {centre:?}");

        let behind = Camera { target: Vec3::new(0.0, 2.0, 20.0), ..camera() };
        assert_eq!(f.render(&behind, &noon(), &World::from_triangles(box_mesh.tris.clone()), &[]), 0);
        assert!(f.inv_depth.iter().all(|&d| d == 0.0));
    }

    #[test]
    fn clips_triangles_crossing_the_camera() {
        let mut ground = Mesh::default();
        ground.tri(Vec3::new(-50.0, 0.0, 50.0), Vec3::new(50.0, 0.0, 50.0), Vec3::new(0.0, 0.0, -80.0), Vec3::Y, Surface { material: Material::Paint, color: [0, 200, 0] });
        let mut f = Frame::new(60, 30);
        let cam = Camera { eye: Vec3::new(0.0, 1.5, 0.0), target: Vec3::new(0.0, 1.0, -10.0), fov_y: 1.1 };
        f.render(&cam, &noon(), &World::from_triangles(ground.tris.clone()), &[]);
        // The bottom row sees the ground, the top row the sky.
        assert!(f.inv_depth[29 * 60 + 30] > 0.0);
        assert_eq!(f.inv_depth[30], 0.0);
    }

    /// A pillar on a plane: the ground behind it (away from the sun) is in
    /// the shade, open ground and the pillar's top are lit.
    #[test]
    fn casts_shadows() {
        let mut scene = plane(60.0);
        scene.object = 1;
        scene.cuboid(Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 10.0, 2.0), Surface { material: Material::Paint, color: [200, 200, 200] }, Surface { material: Material::Paint, color: [200, 200, 200] });
        let sun = Vec3::new(0.0, 1.0, 1.0).normalize(); // shadow falls towards -Z
        let mut map = ShadowMap::new();
        map.render(sun, Vec3::ZERO, &World::from_triangles(scene.tris), &[]);
        let shade = map.lit(Vec3::new(0.0, 0.0, -6.0), Vec3::Y, sun);
        let open = map.lit(Vec3::new(0.0, 0.0, 15.0), Vec3::Y, sun);
        let top = map.lit(Vec3::new(0.0, 10.0, 0.0), Vec3::Y, sun);
        assert_eq!((shade, open, top), (0.0, 1.0, 1.0));
    }

    #[test]
    fn night_is_dark_except_in_the_headlights() {
        let ground = plane(80.0);
        let mut light = Light::at(0.0);
        let cam = Camera { eye: Vec3::new(0.0, 3.0, 8.0), target: Vec3::new(0.0, 0.0, -10.0), fov_y: 1.0 };
        let mut f = Frame::new(60, 40);
        f.render(&cam, &light, &World::from_triangles(ground.tris.clone()), &[]);
        let dark = f.color[30 * 60 + 30];
        light.headlights = Some(crate::lighting::Headlights {
            lamps: [Vec3::new(-0.6, 0.5, 4.0), Vec3::new(0.6, 0.5, 4.0)],
            dir: Vec3::new(0.0, -0.15, -1.0).normalize(),
        });
        f.render(&cam, &light, &World::from_triangles(ground.tris.clone()), &[]);
        let beam = f.color[30 * 60 + 30];
        assert!(dark[0] < 40, "night: {dark:?}");
        assert!(beam[0] > dark[0] + 60, "headlights: {beam:?}");
    }
}

#[cfg(test)]
mod shadow_cost {
    use super::*;

    #[test]
    #[ignore = "timing"]
    fn shadow_refresh_cost() {
        let world = World::build(&crate::map::MapData::generate(7));
        let sun = Light::at(16.5).sun;
        let mut map = ShadowMap::new();
        let mut tris = 0;
        for chunk in &world.chunks {
            tris += chunk.base.len() + chunk.trees_far.len();
        }
        let t = std::time::Instant::now();
        for k in 0..20 {
            map.render(sun, Vec3::new(300.0 + k as f32 * 30.0, 0.0, 400.0), &world, &[]);
        }
        println!("refresh {:.2} ms ({tris} triangles in the world)", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
        let t = std::time::Instant::now();
        map.depth.fill(f32::MAX);
        println!("fill {:?}", t.elapsed());
    }
}

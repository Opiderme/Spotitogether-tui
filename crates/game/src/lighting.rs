//! Time of day: where the sun is, and the colours of the sky, the fog, the
//! sunlight and the ambient light that follow from it. Plus the car's
//! headlights, the only light at night.

use glam::Vec3;

/// Linear RGB multipliers (1.0 = the surface's own colour).
pub type Rgb = [f32; 3];

#[derive(Debug, Clone)]
pub struct Light {
    /// Unit vector towards the sun (below the horizon at night).
    pub sun: Vec3,
    /// Direct sunlight (zero at night).
    pub sun_color: Rgb,
    /// Light reaching surfaces in the shade.
    pub ambient: Rgb,
    pub sky_top: [u8; 3],
    pub sky_horizon: [u8; 3],
    /// Colour of the sun's glow near the horizon (dawn, dusk).
    pub glow: [u8; 3],
    /// How much of the glow shows (0 at noon and midnight).
    pub glow_amount: f32,
    /// Star visibility (0 by day, 1 at night).
    pub stars: f32,
    pub headlights: Option<Headlights>,
    /// Seconds since the start, to animate water.
    pub time: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Headlights {
    /// The two lamps, and where they point.
    pub lamps: [Vec3; 2],
    pub dir: Vec3,
}

/// Headlights reach this far (metres) and spread this wide.
pub const HEADLIGHT_RANGE: f32 = 70.0;
pub const HEADLIGHT_COS_OUTER: f32 = 0.88; // about 28°
pub const HEADLIGHT_COS_INNER: f32 = 0.96; // about 16°
pub const HEADLIGHT_COLOR: Rgb = [1.25, 1.15, 0.9];

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn mix3(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

fn mix_u8(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    [0, 1, 2].map(|i| (f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t) as u8)
}

impl Light {
    /// Lighting at `hours` (0–24, 12 = noon).
    pub fn at(hours: f32) -> Self {
        // The sun turns once a day, peaking at 65° at noon.
        let angle = (hours - 6.0) / 24.0 * std::f32::consts::TAU;
        let elevation = (angle.sin() * 65f32.to_radians().sin()).asin();
        let azimuth = hours / 24.0 * std::f32::consts::TAU + 0.6;
        let sun = Vec3::new(elevation.cos() * azimuth.cos(), elevation.sin(), elevation.cos() * azimuth.sin());

        let day = smoothstep(-0.12, 0.22, sun.y);
        // Low sun: orange light and a glowing horizon.
        let low = (1.0 - sun.y.abs() / 0.32).clamp(0.0, 1.0) * smoothstep(-0.2, -0.02, sun.y);
        let direct = smoothstep(-0.03, 0.1, sun.y);
        let sun_color = mix3([1.0, 0.5, 0.25], [1.0, 0.96, 0.88], smoothstep(0.02, 0.45, sun.y)).map(|c| c * 0.78 * direct);
        let ambient = mix3([0.07, 0.08, 0.15], [0.34, 0.36, 0.42], day);

        let sky_top = mix_u8([6, 8, 22], [52, 92, 168], day);
        let sky_horizon = mix_u8(mix_u8([16, 20, 42], [176, 200, 222], day), [236, 140, 72], low * 0.75);
        Self {
            sun,
            sun_color,
            ambient,
            sky_top,
            sky_horizon,
            glow: [255, 170, 90],
            glow_amount: low,
            stars: 1.0 - smoothstep(-0.2, 0.0, sun.y),
            headlights: None,
            time: 0.0,
        }
    }

    /// Lamps on when it gets dark.
    pub fn is_dark(&self) -> bool {
        self.sun.y < 0.04
    }
}

/// "14:05" for a time in hours.
pub fn clock(hours: f32) -> String {
    let minutes = (hours.rem_euclid(24.0) * 60.0) as u32;
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_and_night() {
        let noon = Light::at(12.0);
        assert!(noon.sun.y > 0.8 && noon.sun_color[0] > 0.7 && noon.stars == 0.0);
        let midnight = Light::at(0.0);
        assert!(midnight.sun.y < -0.8 && midnight.sun_color == [0.0; 3] && midnight.stars == 1.0);
        assert!(midnight.is_dark() && !noon.is_dark());
        let dusk = Light::at(18.2);
        assert!(dusk.glow_amount > 0.5, "glowing horizon at dusk");
        assert!((noon.sun.length() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn formats_the_clock() {
        assert_eq!(clock(14.5), "14:30");
        assert_eq!(clock(24.25), "00:15");
    }
}

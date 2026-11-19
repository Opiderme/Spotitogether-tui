//! Car physics with Rapier: the world's triangles become one static
//! collider, the car a box on four ray-cast wheels (suspension, engine,
//! brakes, grip). Arcade tuning: generous grip, quick steering.

use glam::{Quat, Vec3};
use rapier3d::control::{DynamicRayCastVehicleController, WheelTuning};
use rapier3d::prelude::*;

use crate::mesh::Triangle;

/// Physics runs at a fixed rate, whatever the frame rate.
pub const STEP: f32 = 1.0 / 60.0;

/// Chassis half extents (metres): length along local X (forward), height,
/// width along local Z.
pub const HALF_LENGTH: f32 = 2.0;
pub const HALF_HEIGHT: f32 = 0.35;
pub const HALF_WIDTH: f32 = 0.85;
pub const WHEEL_RADIUS: f32 = 0.34;
const SUSPENSION_REST: f32 = 0.3;
/// Engine force per driven wheel (N) and braking per wheel.
const ENGINE_FORCE: f32 = 2600.0;
const REVERSE_FORCE: f32 = 1300.0;
const BRAKE: f32 = 60.0;
const HANDBRAKE: f32 = 120.0;
const MAX_STEER: f32 = 0.6;
/// Above this speed, the car only coasts.
const TOP_SPEED: f32 = 50.0;

/// What the driver asks for, each in -1..1 (or 0..1).
#[derive(Debug, Clone, Copy, Default)]
pub struct Controls {
    /// Accelerate (> 0) or brake then reverse (< 0).
    pub throttle: f32,
    /// Left (> 0) or right (< 0).
    pub steer: f32,
    pub handbrake: bool,
}

pub struct Car {
    world: PhysicsWorld,
    vehicle: DynamicRayCastVehicleController,
    chassis: RigidBodyHandle,
    /// Smoothed steering angle (radians).
    steering: f32,
    leftover: f32,
}

/// Position and orientation of the car body, and of its wheels.
pub struct CarPose {
    pub position: Vec3,
    pub rotation: Quat,
    /// Wheel centres and their steering angle.
    pub wheels: Vec<(Vec3, f32)>,
}

impl Car {
    /// A car at `position` (on the ground) heading along `dir`, in a world
    /// whose static geometry is `ground`.
    pub fn new(ground: &[Triangle], position: Vec3, dir: Vec3) -> Self {
        let mut world = PhysicsWorld::default();
        world.integration_parameters.dt = STEP;

        let mut vertices = Vec::with_capacity(ground.len() * 3);
        let mut indices = Vec::with_capacity(ground.len());
        for t in ground {
            let i = vertices.len() as u32;
            vertices.extend_from_slice(&t.p);
            indices.push([i, i + 1, i + 2]);
        }
        let collider = ColliderBuilder::trimesh(vertices, indices)
            .expect("world mesh")
            .friction(0.8);
        world.insert(RigidBodyBuilder::fixed(), collider);

        let (chassis, _) = world.insert(
            RigidBodyBuilder::dynamic()
                .pose(Pose::from_parts(position, heading(dir)))
                .linear_damping(0.05)
                .angular_damping(0.6),
            // About 1 100 kg, the centre of mass a little low for stability.
            ColliderBuilder::cuboid(HALF_LENGTH, HALF_HEIGHT, HALF_WIDTH)
                .translation(Vec3::new(0.0, 0.25, 0.0))
                .density(1100.0 / (8.0 * HALF_LENGTH * HALF_HEIGHT * HALF_WIDTH))
                .friction(0.3),
        );

        let tuning = WheelTuning {
            suspension_stiffness: 32.0,
            suspension_compression: 2.4,
            suspension_damping: 3.2,
            max_suspension_travel: 0.25,
            friction_slip: 2.2,
            side_friction_stiffness: 1.0,
            max_suspension_force: 60_000.0,
        };
        let mut vehicle = DynamicRayCastVehicleController::new(chassis);
        for (x, z) in [(1.35, -0.78), (1.35, 0.78), (-1.3, -0.78), (-1.3, 0.78)] {
            vehicle.add_wheel(Vec3::new(x, 0.0, z), -Vec3::Y, Vec3::Z, SUSPENSION_REST, WHEEL_RADIUS, &tuning);
        }
        Self { world, vehicle, chassis, steering: 0.0, leftover: 0.0 }
    }

    /// Advances the simulation by `dt` seconds (in fixed steps).
    pub fn update(&mut self, controls: Controls, dt: f32) {
        self.leftover += dt.min(0.1);
        while self.leftover >= STEP {
            self.leftover -= STEP;
            self.step(controls);
        }
    }

    fn step(&mut self, c: Controls) {
        let speed = self.forward_speed();
        // Less steering lock at speed, eased in over a few frames.
        let lock = MAX_STEER / (1.0 + speed.abs() / 18.0);
        let wanted = c.steer.clamp(-1.0, 1.0) * lock;
        self.steering += (wanted - self.steering).clamp(-3.0 * STEP, 3.0 * STEP);

        let (engine, brake) = if c.throttle > 0.0 {
            let force = if speed < TOP_SPEED { ENGINE_FORCE * c.throttle } else { 0.0 };
            if speed < -0.5 { (0.0, BRAKE) } else { (force, 0.0) }
        } else if c.throttle < 0.0 {
            // Brake while rolling forward, then reverse.
            if speed > 0.5 { (0.0, BRAKE * -c.throttle) } else { (-REVERSE_FORCE * -c.throttle, 0.0) }
        } else {
            (0.0, 0.0)
        };
        for (i, wheel) in self.vehicle.wheels_mut().iter_mut().enumerate() {
            let front = i < 2;
            wheel.steering = if front { self.steering } else { 0.0 };
            wheel.engine_force = if front { 0.0 } else { engine };
            wheel.brake = brake + if c.handbrake && !front { HANDBRAKE } else { 0.0 };
        }

        let world = &mut self.world;
        let queries = world.broad_phase.as_query_pipeline_mut(
            world.narrow_phase.query_dispatcher(),
            &mut world.bodies,
            &mut world.colliders,
            QueryFilter::exclude_dynamic().exclude_rigid_body(self.chassis),
        );
        self.vehicle.update_vehicle(STEP, queries);
        world.step();
    }

    /// Speed along the car's heading (m/s, negative in reverse).
    pub fn forward_speed(&self) -> f32 {
        let body = &self.world.bodies[self.chassis];
        body.linvel().dot(body.rotation() * Vec3::X)
    }

    pub fn pose(&self) -> CarPose {
        let body = &self.world.bodies[self.chassis];
        let wheels = self
            .vehicle
            .wheels()
            .iter()
            .map(|w| (w.center(), w.steering))
            .collect();
        CarPose { position: body.translation(), rotation: *body.rotation(), wheels }
    }

    /// Puts the car back at `position`, heading along `dir`, stopped.
    pub fn reset(&mut self, position: Vec3, dir: Vec3) {
        let body = &mut self.world.bodies[self.chassis];
        body.set_position(Pose::from_parts(position, heading(dir)), true);
        body.set_linvel(Vec3::ZERO, true);
        body.set_angvel(Vec3::ZERO, true);
        self.steering = 0.0;
    }
}

/// Rotation turning the car's local X (forward) towards `dir`.
fn heading(dir: Vec3) -> Quat {
    let flat = Vec3::new(dir.x, 0.0, dir.z).normalize_or(Vec3::X);
    Quat::from_rotation_arc(Vec3::X, flat)
}

impl CarPose {
    /// Forward direction of the body.
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::X
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn settles_on_the_ground_upright() {
        let mut car = Car::new(&flat(), Vec3::new(0.0, 1.5, 0.0), Vec3::X);
        drive(&mut car, Controls::default(), 2.0);
        let pose = car.pose();
        assert!(pose.position.y > 0.3 && pose.position.y < 1.2, "height {}", pose.position.y);
        assert!((pose.rotation * Vec3::Y).y > 0.98, "upright");
        assert!(car.forward_speed().abs() < 0.3);
    }

    #[test]
    fn accelerates_brakes_and_reverses() {
        let mut car = Car::new(&flat(), Vec3::new(-150.0, 1.0, 0.0), Vec3::X);
        drive(&mut car, Controls::default(), 1.0);
        drive(&mut car, Controls { throttle: 1.0, ..Controls::default() }, 6.0);
        let fast = car.forward_speed();
        assert!(fast > 20.0, "0 to {:.0} km/h in 6 s", fast * 3.6);
        assert!(car.pose().position.x > -150.0, "moved forward");
        drive(&mut car, Controls { throttle: -1.0, ..Controls::default() }, 4.0);
        assert!(car.forward_speed() < 1.0, "stopped: {:.1} m/s", car.forward_speed());
        drive(&mut car, Controls { throttle: -1.0, ..Controls::default() }, 2.0);
        assert!(car.forward_speed() < -1.0, "reversing");
    }

    #[test]
    fn steers_left_and_right() {
        for (steer, side) in [(1.0, -1.0), (-1.0, 1.0)] {
            let mut car = Car::new(&flat(), Vec3::new(-100.0, 1.0, 0.0), Vec3::X);
            drive(&mut car, Controls::default(), 1.0);
            drive(&mut car, Controls { throttle: 0.6, steer, handbrake: false }, 3.0);
            let p = car.pose();
            // Facing +X, turning left goes towards -Z (right-handed, Y up).
            assert!(p.position.z * side > 2.0, "steer {steer}: z = {:.1}", p.position.z);
            assert!((p.rotation * Vec3::Y).y > 0.9, "still upright");
        }
    }
}

#[cfg(test)]
mod report {
    use super::tests_support::*;
    use super::*;

    #[test]
    #[ignore = "prints the handling figures"]
    fn handling_report() {
        let mut car = Car::new(&flat(), Vec3::new(-180.0, 1.0, 0.0), Vec3::X);
        drive(&mut car, Controls::default(), 1.5);
        println!("hauteur au repos {:.2} m", car.pose().position.y);
        let full = Controls { throttle: 1.0, ..Controls::default() };
        let mut t = 0.0;
        while car.forward_speed() < 100.0 / 3.6 && t < 20.0 {
            drive(&mut car, full, 0.1);
            t += 0.1;
        }
        println!("0 à 100 km/h : {t:.1} s");
        drive(&mut car, full, 6.0);
        println!("vitesse après 6 s de plus : {:.0} km/h", car.forward_speed() * 3.6);
        let start = car.pose().position.x;
        let v = car.forward_speed();
        let mut t = 0.0;
        while car.forward_speed() > 0.5 && t < 20.0 {
            drive(&mut car, Controls { throttle: -1.0, ..Controls::default() }, 0.05);
            t += 0.05;
        }
        println!("freinage depuis {:.0} km/h : {:.0} m en {t:.1} s", v * 3.6, car.pose().position.x - start);
        for speed_kmh in [30.0, 70.0] {
            let mut car = Car::new(&flat(), Vec3::new(0.0, 1.0, 0.0), Vec3::X);
            drive(&mut car, Controls::default(), 1.0);
            while car.forward_speed() < speed_kmh / 3.6 {
                drive(&mut car, full, 0.05);
            }
            let mut pts = vec![];
            for _ in 0..80 {
                drive(&mut car, Controls { throttle: 0.25, steer: 1.0, handbrake: false }, 0.05);
                pts.push(car.pose().position);
            }
            let (min_z, max_z) = pts.iter().fold((f32::MAX, f32::MIN), |(a, b), p| (a.min(p.z), b.max(p.z)));
            let up = (car.pose().rotation * Vec3::Y).y;
            println!("virage à fond à {speed_kmh} km/h : diamètre ≈ {:.0} m, vitesse finale {:.0} km/h, verticalité {up:.2}", max_z - min_z, car.forward_speed() * 3.6);
        }
    }
}

#[cfg(test)]
mod tests_support {
    use super::*;

    pub fn flat() -> Vec<Triangle> {
        let mut m = crate::mesh::Mesh::default();
        let s = 400.0;
        let ground = crate::mesh::Surface::of(crate::material::Material::Asphalt);
        m.quad(Vec3::new(-s, 0.0, -s), Vec3::new(s, 0.0, -s), Vec3::new(s, 0.0, s), Vec3::new(-s, 0.0, s), Vec3::Y, ground);
        m.tris
    }

    pub fn drive(car: &mut Car, controls: Controls, seconds: f32) {
        for _ in 0..(seconds / STEP).round() as usize {
            car.update(controls, STEP);
        }
    }
}

#[cfg(test)]
mod on_the_world {
    use super::tests_support::drive;
    use super::*;
    use crate::world::World;
    use std::time::Instant;

    #[test]
    fn lands_on_the_road_and_drives_along_it() {
        let world = World::build(&crate::map::MapData::generate(7));
        let start = world.on_road(0.0);
        let t = Instant::now();
        let mut car = Car::new(&world.solids, start.pos + Vec3::Y, start.dir);
        let built = t.elapsed();
        drive(&mut car, Controls::default(), 2.0);
        // Height above the road right under the car (it may roll a little
        // on a slope).
        let rest = car.pose().position.y - world.nearest_road(car.pose().position).pos.y;
        assert!((0.3..1.2).contains(&rest), "resting {rest:.2} m above the road");
        let t = Instant::now();
        // Follow the road with a simple steering rule for 15 s.
        for _ in 0..(15.0 / STEP) as usize {
            let pose = car.pose();
            let road = world.nearest_road(pose.position + pose.forward() * 8.0);
            let to_road = (road.pos - pose.position).normalize();
            let side = pose.forward().cross(to_road).y;
            car.update(Controls { throttle: 0.5, steer: (side * 3.0).clamp(-1.0, 1.0), handbrake: false }, STEP);
        }
        let per_step = t.elapsed() / (15.0 / STEP) as u32;
        let pose = car.pose();
        let road = world.nearest_road(pose.position);
        println!("collider {built:?}, {per_step:?} par pas, vitesse {:.0} km/h", car.forward_speed() * 3.6);
        assert!(pose.position.distance(start.pos) > 100.0, "travelled");
        assert!(pose.position.distance(road.pos) < 8.0, "still near the road");
        assert!((pose.rotation * Vec3::Y).y > 0.8, "upright");
        assert!(pose.position.y > road.pos.y - 0.5, "above the road surface");
    }
}

//! One player's body against the world: `step(world, state, input, dt, params) -> state`, in the frame of the zone, Z up,
//! with the position at the feet as `SCActorStateDataSynced::SMovement::m_entityPosition` holds it.
//!
//! The model is CryEngine's living entity, whose parameters the game's records give (see `Params`): the collider is a
//! capsule that floats above the feet, and the ground under it is traced. What passes under the capsule is stepped onto
//! as soon as the trace stands on it, so the step height is the gap between the feet and the capsule, 0.51 m standing and
//! 0.35 m crouched. Ground steeper than `max_climb_angle` is no ground: the player falls along it.
//!
//! Per frame: the stance changes if it can, the horizontal velocity goes to the wished one at the rate `inertia` on the
//! ground and stays as it was in the air (`air_control` 0), a jump starts on the ground, then the frame is cut in
//! sub-steps of at most `MAX_SUBSTEP` metres. Each sub-step applies gravity in the air, sweeps the capsule and slides it
//! along what it hits, pushes it out of anything it still overlaps, and traces the ground.

use parry3d::math::{Pose, Vector};
use parry3d::query::{cast_shapes, contact, intersection_test, Ray, RayCast, ShapeCastOptions};
use parry3d::shape::{Ball, Capsule, Shape};

use super::World;

/// A capsule standing on the feet: its centre `center` above them, `half` the half length of its cylinder, `radius`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collider {
    pub center: f64,
    pub half: f64,
    pub radius: f64,
}

impl Collider {
    /// What passes under the capsule.
    pub fn gap(&self) -> f64 {
        self.center - self.half - self.radius
    }

    pub fn top(&self) -> f64 {
        self.center + self.half + self.radius
    }
}

/// `ActorStanceDimensionsInfo.Stand`: `heightCollider` 1.05, `size` 0.39, 0.41, 0.15, axis Z. `size.x` is taken as the
/// radius and `size.z` as the half length, as CryEngine's `pe_player_dimensions.sizeCollider` reads.
pub const STAND: Collider = Collider { center: 1.05, half: 0.15, radius: 0.39 };
/// `ActorStanceDimensionsInfo.Crouch`: `heightCollider` 0.77, `size` 0.37, 0.41, 0.05.
pub const CROUCH: Collider = Collider { center: 0.77, half: 0.05, radius: 0.37 };

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stance {
    #[default]
    Stand,
    Crouch,
}

/// Movement constants. `Default` holds the game's values where its records give them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub stand: Collider,
    pub crouch: Collider,
    /// m/s holding a direction: `ActorStanceSpeedsInfo.Stand` `runFastSpeed` 5.58 (a client measured 5.7).
    pub run_speed: f64,
    /// `sprintSpeed` 7.2 (measured 7.3).
    pub sprint_speed: f64,
    /// `ActorStanceSpeedsInfo.Crouch` `defaultSpeed` and `runFastSpeed` 2.3. Crouched has no sprint.
    pub crouch_speed: f64,
    /// `JumpFallLandParams.ActorJumpFallLandPlayerParams` `jumpHeight`, which the player's `SActorComponentParams` names.
    pub jump_height: f64,
    /// m/s². No record read gives it: standard gravity, which a station's artificial gravity is taken to be.
    pub gravity: f64,
    /// Degrees. `EntityClassDefinition.Player`, `SSCActorPhysicsControllerComponentParams` `maxClimbAngle` 50 (and
    /// `minFallAngle` 50).
    pub max_climb_angle: f64,
    /// 1/s, how fast the velocity reaches the wished one on the ground: the same record's `inertiaAccel` 50.
    pub inertia: f64,
    /// 1/s, the same in the air: `airControl` 0.
    pub air_control: f64,
    /// Radius of the ground trace: half of the stances' `groundTraceSpreadSizes` 0.25.
    pub ground_probe: f64,
    /// How far the feet follow the ground down without falling, as down stairs. Not from the game: two of its steps.
    pub snap_down: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            stand: STAND,
            crouch: CROUCH,
            run_speed: 5.58,
            sprint_speed: 7.2,
            crouch_speed: 2.3,
            jump_height: 0.5,
            gravity: 9.81,
            max_climb_angle: 50.0,
            inertia: 50.0,
            air_control: 0.0,
            ground_probe: 0.125,
            snap_down: 0.45,
        }
    }
}

impl Params {
    pub fn collider(&self, stance: Stance) -> Collider {
        match stance {
            Stance::Stand => self.stand,
            Stance::Crouch => self.crouch,
        }
    }

    fn min_ground_normal(&self) -> f64 {
        self.max_climb_angle.to_radians().cos()
    }
}

/// A player's body.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct State {
    /// The feet, in the zone's frame.
    pub position: [f64; 3],
    pub velocity: [f64; 3],
    /// About Z, counter-clockwise seen from above, 0 facing +Y, as `movement::Movement` counts it.
    pub yaw: f64,
    pub grounded: bool,
    pub stance: Stance,
}

impl State {
    pub fn standing_at(position: [f64; 3], yaw: f64) -> Self {
        Self { position, yaw, ..Self::default() }
    }
}

/// What the player asks for in one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    /// Right and forward, each -1 to 1, relative to `yaw`. Longer than 1 is cut to 1.
    pub wish: [f64; 2],
    pub yaw: f64,
    pub sprint: bool,
    /// A jump starts this frame (`SJump`, action 0x9c).
    pub jump: bool,
    /// The stance the player asks for (`SStance`, action 0x64: 1 stand, 2 crouch).
    pub crouch: bool,
}

/// Distance kept from surfaces, metres.
const SKIN: f32 = 0.01;
/// The longest move of one sub-step, metres: less than the smallest radius, so nothing is tunnelled through.
const MAX_SUBSTEP: f64 = 0.2;
const SLIDES: usize = 4;
const PUSHES: usize = 4;
/// The ground counts while the feet are this close above it.
const GROUND_EPS: f64 = 0.02;

fn vector(v: [f64; 3]) -> Vector {
    Vector::new(v[0] as f32, v[1] as f32, v[2] as f32)
}

fn capsule(collider: &Collider) -> Capsule {
    Capsule::new_z(collider.half as f32, collider.radius as f32)
}

fn capsule_pose(collider: &Collider, feet: [f64; 3]) -> Pose {
    Pose::translation(feet[0] as f32, feet[1] as f32, (feet[2] + collider.center) as f32)
}

/// Whether the collider fits at `feet`, touching nothing.
pub fn fits(world: &World, collider: &Collider, feet: [f64; 3]) -> bool {
    !intersection_test(&capsule_pose(collider, feet), &capsule(collider), &Pose::identity(), &world.mesh).is_ok_and(|hit| hit.intersecting)
}

/// The first thing `shape` at `pose` hits moving by `delta`: the fraction of `delta` it can move, and the surface's normal.
pub(super) fn sweep(world: &World, shape: &dyn Shape, pose: &Pose, delta: Vector, target: f32) -> Option<(f32, Vector)> {
    let options =
        ShapeCastOptions { max_time_of_impact: 1.0, target_distance: target, stop_at_penetration: false, compute_impact_geometry_on_penetration: true };
    let hit = cast_shapes(pose, delta, shape, &Pose::identity(), Vector::ZERO, &world.mesh, options).ok()??;
    Some((hit.time_of_impact, -hit.normal1))
}

/// Moves the capsule by `delta`, sliding along what it hits. Returns the normals of the surfaces hit.
fn slide(world: &World, collider: &Collider, feet: &mut [f64; 3], mut delta: Vector, grounded: bool, min_ground: f32) -> Vec<Vector> {
    let shape = capsule(collider);
    let mut normals = Vec::new();
    for _ in 0..SLIDES {
        let length = delta.length();
        if length < 1e-6 {
            break;
        }
        let pose = capsule_pose(collider, *feet);
        let Some((toi, mut normal)) = sweep(world, &shape, &pose, delta, SKIN) else {
            add(feet, delta);
            break;
        };
        add(feet, delta * toi);
        // On the ground a wall is a wall: sliding along it must not lift the player, the ground trace does that.
        if grounded && normal.z < min_ground {
            normal.z = 0.0;
            if normal.length() < 1e-4 {
                break;
            }
            normal = normal.normalize();
        }
        normals.push(normal);
        let rest = delta * (1.0 - toi);
        delta = rest - normal * rest.dot(normal).min(0.0);
        if grounded {
            delta.z = 0.0;
        }
    }
    normals
}

fn add(feet: &mut [f64; 3], delta: Vector) {
    feet[0] += f64::from(delta.x);
    feet[1] += f64::from(delta.y);
    feet[2] += f64::from(delta.z);
}

/// Pushes the capsule out of what it overlaps.
fn push_out(world: &World, collider: &Collider, feet: &mut [f64; 3]) {
    let shape = capsule(collider);
    for _ in 0..PUSHES {
        let pose = capsule_pose(collider, *feet);
        let Ok(Some(touch)) = contact(&pose, &shape, &Pose::identity(), &world.mesh, 0.0) else { break };
        if touch.dist >= 0.0 {
            break;
        }
        add(feet, -touch.normal1 * (-touch.dist + SKIN));
    }
}

/// Traces the ground from the bottom of the capsule down by `length`: the height of the ground and its normal. A ball the
/// size of the game's ground traces first; where what it touches is too steep to stand on, as the foot of a wall can be,
/// a ray straight down from the middle.
fn ground(world: &World, collider: &Collider, feet: [f64; 3], radius: f64, length: f64, min_ground: f32) -> Option<(f64, Vector)> {
    let ball = Ball::new(radius as f32);
    let start = feet[2] + collider.gap() + radius;
    let pose = Pose::translation(feet[0] as f32, feet[1] as f32, start as f32);
    let found = sweep(world, &ball, &pose, Vector::new(0.0, 0.0, -(length as f32)), 0.0)
        .map(|(toi, normal)| (start - radius - f64::from(toi) * length, normal));
    if found.is_some_and(|(_, normal)| normal.z >= min_ground) {
        return found;
    }
    let top = feet[2] + collider.gap();
    let ray = Ray::new(Vector::new(feet[0] as f32, feet[1] as f32, top as f32), Vector::new(0.0, 0.0, -1.0));
    let hit = world.mesh.cast_ray_and_get_normal(&Pose::identity(), &ray, length as f32, false);
    hit.map(|hit| (top - f64::from(hit.time_of_impact), if hit.normal.z < 0.0 { -hit.normal } else { hit.normal })).or(found)
}

/// Moves `state` for `dt` seconds of `input`.
pub fn step(world: &World, state: &State, input: &Input, dt: f64, params: &Params) -> State {
    let mut state = *state;
    state.yaw = input.yaw;
    if dt <= 0.0 {
        return state;
    }
    let wanted = if input.crouch { Stance::Crouch } else { Stance::Stand };
    if wanted != state.stance && (wanted == Stance::Crouch || fits(world, &params.stand, state.position)) {
        state.stance = wanted;
    }

    let [right, forward] = input.wish;
    let length = right.hypot(forward).max(1.0);
    let speed = match (state.stance, input.sprint) {
        (Stance::Crouch, _) => params.crouch_speed,
        (Stance::Stand, true) => params.sprint_speed,
        (Stance::Stand, false) => params.run_speed,
    };
    let (sin, cos) = state.yaw.sin_cos();
    let target = [(right * cos - forward * sin) / length * speed, (right * sin + forward * cos) / length * speed];
    let rate = if state.grounded { params.inertia } else { params.air_control };
    let blend = (rate * dt).min(1.0);
    for (velocity, target) in state.velocity.iter_mut().zip(target) {
        *velocity += (target - *velocity) * blend;
    }
    if input.jump && state.grounded {
        state.velocity[2] = (2.0 * params.gravity * params.jump_height).sqrt();
        state.grounded = false;
    }

    let travel = (state.velocity[0].hypot(state.velocity[1]).hypot(state.velocity[2]) + params.gravity * dt) * dt;
    let substeps = (travel / MAX_SUBSTEP).ceil().clamp(1.0, 64.0) as usize;
    let h = dt / substeps as f64;
    let min_ground = params.min_ground_normal();
    for _ in 0..substeps {
        substep(world, &mut state, h, params, min_ground);
    }
    state
}

fn substep(world: &World, state: &mut State, h: f64, params: &Params, min_ground: f64) {
    let collider = params.collider(state.stance);
    let was_grounded = state.grounded;
    // Half the gravity before the move and half after, so a jump peaks at its height whatever the frame rate.
    if !state.grounded {
        state.velocity[2] -= params.gravity * h / 2.0;
    } else {
        state.velocity[2] = 0.0;
    }
    let before = state.position;
    let delta = vector(state.velocity) * h as f32;
    let mut perched = false;
    for normal in slide(world, &collider, &mut state.position, delta, state.grounded, min_ground as f32) {
        perched |= !was_grounded && state.velocity[2] <= 0.0 && f64::from(normal.z) >= min_ground;
        let normal = [f64::from(normal.x), f64::from(normal.y), f64::from(normal.z)];
        let into = state.velocity[0] * normal[0] + state.velocity[1] * normal[1] + state.velocity[2] * normal[2];
        if into < 0.0 {
            for (velocity, normal) in state.velocity.iter_mut().zip(normal) {
                *velocity -= normal * into;
            }
        }
    }
    push_out(world, &collider, &mut state.position);
    if !state.grounded {
        state.velocity[2] -= params.gravity * h / 2.0;
    }

    if state.velocity[2] > 0.0 && !was_grounded {
        state.grounded = false;
        return;
    }
    let fell = (before[2] - state.position[2]).max(0.0);
    let reach = collider.gap() + if was_grounded { params.snap_down } else { GROUND_EPS + fell };
    match ground(world, &collider, state.position, params.ground_probe, reach, min_ground as f32) {
        Some((height, normal)) if f64::from(normal.z) >= min_ground => {
            let raised = [state.position[0], state.position[1], height];
            if height <= state.position[2] || fits(world, &collider, raised) {
                state.position[2] = height;
            } else {
                // No room above the step: it is a wall.
                state.position = [before[0], before[1], state.position[2]];
                state.velocity[0] = 0.0;
                state.velocity[1] = 0.0;
            }
            state.velocity[2] = 0.0;
            state.grounded = true;
        }
        // Landed on the capsule itself, on something walkable that the trace under the middle misses: a ledge.
        _ if perched => {
            state.velocity[2] = 0.0;
            state.grounded = true;
        }
        _ => state.grounded = false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const DT: f64 = 1.0 / 30.0;

    /// The 12 triangles of a box.
    fn cuboid(min: [f32; 3], max: [f32; 3], vertices: &mut Vec<[f32; 3]>, triangles: &mut Vec<[u32; 3]>) {
        let base = vertices.len() as u32;
        for corner in 0..8 {
            vertices.push(std::array::from_fn(|axis| if corner & (1 << axis) != 0 { max[axis] } else { min[axis] }));
        }
        let faces = [[0, 2, 3, 1], [4, 5, 7, 6], [0, 1, 5, 4], [2, 6, 7, 3], [0, 4, 6, 2], [1, 3, 7, 5]];
        for [a, b, c, d] in faces {
            triangles.push([base + a, base + b, base + c]);
            triangles.push([base + a, base + c, base + d]);
        }
    }

    fn world(boxes: &[([f32; 3], [f32; 3])], extra: &[[[f32; 3]; 3]]) -> World {
        let (mut vertices, mut triangles) = (Vec::new(), Vec::new());
        for (min, max) in boxes {
            cuboid(*min, *max, &mut vertices, &mut triangles);
        }
        for triangle in extra {
            let base = vertices.len() as u32;
            vertices.extend(triangle);
            triangles.push([base, base + 1, base + 2]);
        }
        World::from_triangles(vertices, triangles).unwrap()
    }

    /// A world of boxes only.
    pub(crate) fn cuboid_world(boxes: &[([f32; 3], [f32; 3])]) -> World {
        world(boxes, &[])
    }

    /// A floor 40 m square, its top at z 0.
    pub(crate) const FLOOR: ([f32; 3], [f32; 3]) = ([-20.0, -20.0, -1.0], [20.0, 20.0, 0.0]);

    fn run(world: &World, mut state: State, input: Input, seconds: f64) -> State {
        for _ in 0..(seconds / DT).round() as usize {
            state = step(world, &state, &input, DT, &Params::default());
        }
        state
    }

    fn settled(world: &World, position: [f64; 3]) -> State {
        run(world, State::standing_at(position, 0.0), Input::default(), 0.5)
    }

    const FORWARD: Input = Input { wish: [0.0, 1.0], yaw: 0.0, sprint: false, jump: false, crouch: false };

    #[test]
    fn standing_still_on_the_floor_stays_put() {
        let world = world(&[FLOOR], &[]);
        let start = State::standing_at([1.0, 2.0, 0.0], 0.0);
        let state = run(&world, start, Input::default(), 5.0);
        assert!(state.grounded);
        assert!((0..3).all(|axis| (state.position[axis] - start.position[axis]).abs() < 1e-3), "{state:?}");
    }

    #[test]
    fn walking_moves_at_the_run_speed() {
        let world = world(&[FLOOR], &[]);
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 1.0);
        assert!((state.position[1] - 5.58).abs() < 0.2, "{state:?}");
        assert!(state.position[0].abs() < 1e-3 && state.position[2].abs() < 1e-3);
    }

    #[test]
    fn walking_into_a_wall_stops() {
        let world = world(&[FLOOR, ([-10.0, 3.0, 0.0], [10.0, 3.5, 4.0])], &[]);
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 3.0);
        assert!(state.position[1] < 3.0 - STAND.radius + 0.02 && state.position[1] > 3.0 - STAND.radius - 0.1, "{state:?}");
        assert!(state.grounded && state.position[2].abs() < 1e-3);
    }

    #[test]
    fn a_wall_at_an_angle_is_slid_along() {
        let world = world(&[FLOOR], &[[[-10.0, 3.0, 0.0], [10.0, 13.0, 0.0], [-10.0, 3.0, 5.0]], [[10.0, 13.0, 0.0], [10.0, 13.0, 5.0], [-10.0, 3.0, 5.0]]]);
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 3.0);
        // The wall runs along (2, 1): pushing along +Y slides towards +X, staying in front of it.
        assert!(state.position[0] > 1.0, "slid sideways along the wall: {state:?}");
        assert!(state.position[1] < 3.0 + (state.position[0] + 10.0) / 2.0 - 0.3, "kept off the wall: {state:?}");
    }

    /// Five steps of 0.2 m rise and 0.3 m run, from y 2, to a landing at z 1.
    fn stairs() -> World {
        let mut boxes = vec![FLOOR];
        for step in 0..5 {
            let y = 2.0 + 0.3 * step as f32;
            boxes.push(([-2.0, y, 0.0], [2.0, 4.0, 0.2 * (step + 1) as f32]));
        }
        boxes.push(([-2.0, 4.0, 0.0], [2.0, 20.0, 1.0]));
        world(&boxes, &[])
    }

    #[test]
    fn stairs_are_climbed() {
        let world = stairs();
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 1.5);
        assert!(state.position[1] > 5.0, "{state:?}");
        assert!((state.position[2] - 1.0).abs() < 1e-3 && state.grounded, "{state:?}");
    }

    #[test]
    fn stairs_are_walked_down_without_falling() {
        let world = stairs();
        let start = settled(&world, [0.0, 6.0, 1.0]);
        let back = Input { wish: [0.0, -1.0], ..FORWARD };
        let mut state = start;
        for _ in 0..45 {
            state = step(&world, &state, &back, DT, &Params::default());
            assert!(state.grounded, "{state:?}");
        }
        assert!(state.position[1] < 1.0 && state.position[2].abs() < 1e-3, "{state:?}");
    }

    #[test]
    fn a_ramp_within_the_climb_angle_is_walked_up() {
        // 30 degrees, rising along +Y from y 2.
        let rise = 4.0 * 30f32.to_radians().tan();
        let ramp = [[[-3.0, 2.0, 0.0], [3.0, 2.0, 0.0], [3.0, 6.0, rise]], [[-3.0, 2.0, 0.0], [3.0, 6.0, rise], [-3.0, 6.0, rise]]];
        let world = world(&[FLOOR, ([-3.0, 6.0, 0.0], [3.0, 20.0, rise])], &ramp);
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 2.0);
        assert!(state.position[1] > 6.5 && (state.position[2] - f64::from(rise)).abs() < 0.01, "{state:?}");
    }

    #[test]
    fn a_ramp_steeper_than_the_climb_angle_is_not() {
        // 65 degrees.
        let rise = 4.0 * 65f32.to_radians().tan();
        let ramp = [[[-3.0, 2.0, 0.0], [3.0, 2.0, 0.0], [3.0, 6.0, rise]], [[-3.0, 2.0, 0.0], [3.0, 6.0, rise], [-3.0, 6.0, rise]]];
        let world = world(&[FLOOR], &ramp);
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 3.0);
        assert!(state.position[2] < 1.0 && state.position[1] < 3.0, "{state:?}");
    }

    #[test]
    fn a_jump_goes_up_and_lands() {
        let world = world(&[FLOOR], &[]);
        let mut state = settled(&world, [0.0, 0.0, 0.0]);
        state = step(&world, &state, &Input { jump: true, ..Input::default() }, DT, &Params::default());
        assert!(!state.grounded);
        let mut peak: f64 = 0.0;
        let mut frames = 0;
        while !state.grounded && frames < 100 {
            state = step(&world, &state, &Input::default(), DT, &Params::default());
            peak = peak.max(state.position[2]);
            frames += 1;
        }
        assert!((peak - 0.5).abs() < 0.05, "peak {peak}");
        assert!(state.grounded && state.position[2].abs() < 1e-3, "{state:?}");
        // 2 * sqrt(2h/g) = 0.64 s.
        assert!((17..=22).contains(&frames), "{frames} frames in the air");
    }

    #[test]
    fn a_ceiling_stops_a_jump() {
        let world = world(&[FLOOR, ([-5.0, -5.0, 1.8], [5.0, 5.0, 2.0])], &[]);
        let mut state = settled(&world, [0.0, 0.0, 0.0]);
        let mut peak: f64 = 0.0;
        state = step(&world, &state, &Input { jump: true, ..Input::default() }, DT, &Params::default());
        for _ in 0..40 {
            state = step(&world, &state, &Input::default(), DT, &Params::default());
            peak = peak.max(state.position[2]);
        }
        assert!(peak + STAND.top() <= 1.8 + 0.001, "head at {}", peak + STAND.top());
        assert!(state.grounded);
    }

    #[test]
    fn crouching_lowers_the_body_and_passes_under_a_beam() {
        // A beam whose underside is at 1.4 m: above the crouched top (1.19), below the standing one (1.59).
        let world = world(&[FLOOR, ([-5.0, 3.0, 1.4], [5.0, 4.0, 2.0])], &[]);
        let crouch = Input { crouch: true, ..FORWARD };
        let standing = run(&world, settled(&world, [0.0, 0.0, 0.0]), FORWARD, 2.0);
        assert!(standing.position[1] < 3.0, "a standing player is stopped: {standing:?}");
        let crouched = run(&world, settled(&world, [0.0, 0.0, 0.0]), crouch, 3.0);
        assert_eq!(crouched.stance, Stance::Crouch);
        assert!(crouched.position[1] > 4.5, "a crouched player passes: {crouched:?}");
        assert!((crouched.position[1] - 3.0 * 2.3).abs() < 0.2, "at the crouched speed: {crouched:?}");
        assert!(Params::default().crouch.top() < Params::default().stand.top());
    }

    #[test]
    fn a_crouched_player_cannot_stand_up_under_a_beam() {
        let world = world(&[FLOOR, ([-5.0, -1.0, 1.4], [5.0, 1.0, 2.0])], &[]);
        let crouched = run(&world, settled(&world, [0.0, 0.0, 0.0]), Input { crouch: true, ..Input::default() }, 0.2);
        let state = run(&world, crouched, Input::default(), 0.5);
        assert_eq!(state.stance, Stance::Crouch);
        let away = run(&world, state, FORWARD, 1.0);
        assert_eq!(away.stance, Stance::Stand, "stands once clear: {away:?}");
    }

    #[test]
    fn walking_off_an_edge_falls() {
        let world = world(&[([-5.0, -5.0, -1.0], [5.0, 5.0, 0.0]), ([-20.0, -20.0, -4.0], [20.0, 20.0, -3.0])], &[]);
        let mut state = settled(&world, [0.0, 3.0, 0.0]);
        let mut fell = false;
        for _ in 0..90 {
            state = step(&world, &state, &FORWARD, DT, &Params::default());
            fell |= !state.grounded && state.position[2] < -0.5;
        }
        assert!(fell);
        assert!(state.grounded && (state.position[2] + 3.0).abs() < 1e-3, "{state:?}");
    }

    #[test]
    fn a_fall_from_high_lands_on_the_floor() {
        let world = world(&[FLOOR], &[]);
        let state = run(&world, State::standing_at([0.0, 0.0, 30.0], 0.0), Input::default(), 4.0);
        assert!(state.grounded && state.position[2].abs() < 1e-3, "{state:?}");
    }

    #[test]
    fn nothing_moves_in_the_air() {
        let world = world(&[FLOOR], &[]);
        let state = run(&world, State::standing_at([0.0, 0.0, 30.0], 0.0), FORWARD, 0.5);
        assert!(state.position[1].abs() < 1e-9, "air control is 0: {state:?}");
    }

    #[test]
    fn the_yaw_turns_the_wish() {
        let world = world(&[FLOOR], &[]);
        let turned = Input { yaw: std::f64::consts::FRAC_PI_2, ..FORWARD };
        let state = run(&world, settled(&world, [0.0, 0.0, 0.0]), turned, 1.0);
        assert!(state.position[0] < -5.0 && state.position[1].abs() < 1e-3, "facing +Y turned left is -X: {state:?}");
    }
}

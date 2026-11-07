//! The server's check of a position a client reports while it moves itself (`SClientAuthoritativePosition`): did the
//! move go through solid geometry, and is there anything under the feet. See "Client positions" in docs/DGS_PHYSICS.md.
//!
//! Both are lenient on purpose. The client collides with its own physics, which this collision only approximates (shapes
//! simplified to 3 cm, moving entities left out, the step height inferred), and a refused report is never sent back to
//! the client as a correction, so the cost of a wrong refusal is the server's state lagging behind, and the cost of a
//! wrong acceptance is what any real server would also accept.

use parry3d::math::{Pose, Vector};
use parry3d::query::{Ray, RayCast};
use parry3d::shape::Ball;

use super::controller::sweep;
use super::World;

/// Heights above the feet at which a move is traced. A move goes through solid geometry only when all of them are
/// blocked, as by a wall, a floor or a ceiling: one beam, counter or stair riser blocks only some, and a player can pass
/// under, over or onto those (crouched, prone, by the step or a mantle). The lowest is under the crouched capsule's
/// bottom (0.35 m) only by a little, so a prone player sliding under a low table is still judged on the others.
pub const PATH_HEIGHTS: [f64; 3] = [0.3, 0.9, 1.5];

/// Whether moving the feet straight from `from` to `to` passes through the world at every height of `PATH_HEIGHTS`.
pub fn crosses(world: &World, from: [f64; 3], to: [f64; 3]) -> bool {
    let delta = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
    if delta.iter().all(|axis| axis.abs() < 1e-6) {
        return false;
    }
    PATH_HEIGHTS.iter().all(|height| {
        let origin = Vector::new(from[0] as f32, from[1] as f32, (from[2] + height) as f32);
        let ray = Ray::new(origin, Vector::new(delta[0] as f32, delta[1] as f32, delta[2] as f32));
        // The direction is the whole move, so a time of impact up to 1 is on the way.
        world.mesh.cast_ray(&Pose::identity(), &ray, 1.0, false).is_some()
    })
}

/// How far above the feet the trace for support starts, and how far below them it still counts.
const SUPPORT_ABOVE: f64 = 0.5;
pub const SUPPORT_BELOW: f64 = 0.3;
/// The trace is a ball a little smaller than the capsule (0.39), so it finds a ledge the capsule stands on with the feet
/// past its edge, and does not touch the walls the capsule keeps away from.
const SUPPORT_RADIUS: f64 = 0.3;

/// The height of what the feet at `feet` stand on: anything within `SUPPORT_BELOW` under them, or up to `SUPPORT_ABOVE`
/// above them (a client standing a little into a simplified floor). Any slope counts, so sliding down a steep one is
/// supported too. `None` in the air.
pub fn support(world: &World, feet: [f64; 3]) -> Option<f64> {
    let start = feet[2] + SUPPORT_ABOVE + SUPPORT_RADIUS;
    let pose = Pose::translation(feet[0] as f32, feet[1] as f32, start as f32);
    let length = SUPPORT_ABOVE + SUPPORT_BELOW;
    let (toi, _) = sweep(world, &Ball::new(SUPPORT_RADIUS as f32), &pose, Vector::new(0.0, 0.0, -(length as f32)), 0.0)?;
    Some(start - SUPPORT_RADIUS - f64::from(toi) * length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::controller::tests::{cuboid_world, FLOOR};

    #[test]
    fn a_wall_is_crossed_and_a_beam_is_not() {
        let world = cuboid_world(&[FLOOR, ([-10.0, 3.0, 0.0], [10.0, 3.5, 4.0]), ([-10.0, -3.5, 1.4], [10.0, -3.0, 2.0])]);
        assert!(crosses(&world, [0.0, 2.8, 0.0], [0.0, 3.7, 0.0]), "through the wall");
        assert!(!crosses(&world, [0.0, 2.0, 0.0], [0.0, 2.6, 0.0]), "up to it");
        assert!(!crosses(&world, [0.0, -2.8, 0.0], [0.0, -3.7, 0.0]), "under the beam, crouched");
        assert!(!crosses(&world, [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]));
    }

    #[test]
    fn a_floor_is_crossed_going_through_it() {
        let world = cuboid_world(&[FLOOR]);
        assert!(crosses(&world, [0.0, 0.0, 0.0], [0.0, 0.2, -2.5]));
        assert!(!crosses(&world, [0.0, 0.0, 0.0], [0.0, 0.2, 0.5]), "a jump");
    }

    #[test]
    fn a_step_up_is_not_crossing() {
        // A 0.6 m crate, higher than the server's own step: the client may climb more than the server would.
        let world = cuboid_world(&[FLOOR, ([-2.0, 1.0, 0.0], [2.0, 3.0, 0.6])]);
        assert!(!crosses(&world, [0.0, 0.8, 0.0], [0.0, 1.05, 0.6]));
    }

    #[test]
    fn support_is_the_ground_near_the_feet() {
        let world = cuboid_world(&[FLOOR, ([-2.0, 1.0, 0.0], [2.0, 3.0, 0.6])]);
        assert!(support(&world, [0.0, 0.0, 0.0]).is_some_and(|height| height.abs() < 1e-3));
        assert!(support(&world, [0.0, 0.0, 0.02]).is_some(), "a client standing on its own floor, a little higher");
        assert!(support(&world, [0.0, 0.0, -0.05]).is_some(), "or a little lower");
        assert!(support(&world, [0.0, 0.0, 0.25]).is_some());
        assert_eq!(support(&world, [0.0, 0.0, 0.5]), None, "a jump");
        assert!(support(&world, [0.0, 3.2, 0.6]).is_some_and(|height| (height - 0.6).abs() < 0.1), "feet past the crate's edge");
    }
}

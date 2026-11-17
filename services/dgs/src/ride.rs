//! A ship that rides the loading platform: the bind has no joint, so a ship bound under the hangar does not follow the pad's
//! animated node. The DGS moves the ship
//! itself, with `EntityMoved` updates (the replicant sends them as `EntitySnapshotUpdate` with the position mask) in steps of
//! `RIDE_TICK` over the time the pad takes, from the pit pose to the up pose and back.
//!
//! `SC_DGS_SHIP_RIDE=off` (or `0`, `false`, `no`) disables it: the ship then stays where it was bound.

use std::time::Duration;

use sc_api::internal::{EntityMoved, EntityPosition};

pub const RIDE_SETTING: &str = "SC_DGS_SHIP_RIDE";
pub const RIDE_OFF: [&str; 4] = ["off", "0", "false", "no"];
/// How often the ship's position is sent while the pad moves (about 10 Hz).
pub const RIDE_TICK: Duration = Duration::from_millis(100);

/// The setting of the pop-in (the default): the ship is bound on the raised pad when the pad has finished rising, and the ship
/// on the pad goes as the pad starts down, with no ride. Position updates cannot move a bound ship (4.10.3: the client
/// only updates its replication record) and a bind under the pad is not verified, so nothing moves a ship with the pad yet.
/// `off`, `0`, `false` or `no` bind the ship at the pit and (with `SC_DGS_SHIP_RIDE`) send its positions as before.
pub const POP_IN_SETTING: &str = "SC_DGS_SHIP_POP_IN";

#[cfg(test)]
thread_local! {
    /// A test's own setting of the pop-in, so that tests in parallel do not share the environment.
    pub static POP_IN_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the ship pops in on the raised pad (the default).
pub fn pop_in() -> bool {
    #[cfg(test)]
    if let Some(forced) = POP_IN_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    !std::env::var(POP_IN_SETTING).is_ok_and(|value| RIDE_OFF.contains(&value.to_ascii_lowercase().as_str()))
}

/// Whether the ship rides the pad (the default).
pub fn enabled() -> bool {
    !std::env::var(RIDE_SETTING).is_ok_and(|value| RIDE_OFF.contains(&value.to_ascii_lowercase().as_str()))
}

/// The ship on the pad, and where it is between the pit and the up pose (the hangar frame it is bound in).
#[derive(Clone, Debug, PartialEq)]
pub struct Ride {
    geid: u64,
    /// The pose of the ship with the pad down, and with it up.
    pit: [f64; 3],
    up: [f64; 3],
    /// The pose of the last tick: the DGS's record of where the ship is.
    pose: [f64; 3],
    from: [f64; 3],
    to: [f64; 3],
    started: Duration,
    duration: Duration,
    moving: bool,
}

impl Ride {
    /// A ship bound at the pit pose.
    pub fn at_pit(geid: u64, pit: [f64; 3], up: [f64; 3]) -> Self {
        Self {
            geid,
            pit,
            up,
            pose: pit,
            from: pit,
            to: pit,
            started: Duration::ZERO,
            duration: Duration::ZERO,
            moving: false,
        }
    }

    pub fn geid(&self) -> u64 {
        self.geid
    }

    /// Where the ship is, as of the last tick.
    pub fn pose(&self) -> [f64; 3] {
        self.pose
    }

    pub fn moving(&self) -> bool {
        self.moving
    }

    /// The pad starts to rise (`up`) or to lower at `now` and takes `duration`; the ship goes from where it is.
    pub fn start(&mut self, up: bool, now: Duration, duration: Duration) {
        self.from = self.pose;
        self.to = if up { self.up } else { self.pit };
        self.started = now;
        self.duration = duration;
        self.moving = true;
    }

    /// The ship's position at `now` while it moves, on the pad's curve (linear) and exactly at the end pose from `duration` on.
    pub fn position_at(&self, now: Duration) -> [f64; 3] {
        let elapsed = now.saturating_sub(self.started);
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        let fraction = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        [0, 1, 2].map(|axis| self.from[axis] + (self.to[axis] - self.from[axis]) * fraction)
    }

    /// The next position to send at `now`, `None` when the ship does not move. The tick that reaches the end pose ends the move.
    pub fn tick(&mut self, now: Duration) -> Option<EntityMoved> {
        if !self.moving {
            return None;
        }
        self.pose = self.position_at(now);
        self.moving = now.saturating_sub(self.started) < self.duration;
        Some(EntityMoved {
            entity_id: self.geid,
            position: Some(EntityPosition {
                x: self.pose[0],
                y: self.pose[1],
                z: self.pose[2],
            }),
            rotation: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIT: [f64; 3] = [0.0, -45.0, -22.22];
    const UP: [f64; 3] = [0.0, -45.0, 9.78];
    const OPEN: Duration = Duration::from_millis(33_000);

    fn z(moved: &EntityMoved) -> f64 {
        moved.position.as_ref().unwrap().z
    }

    #[test]
    fn the_ship_rises_from_the_pit_to_the_up_pose_monotonically() {
        let mut ride = Ride::at_pit(5, PIT, UP);
        assert_eq!(ride.tick(Duration::from_secs(1)), None, "nothing moves before the pad does");
        let begin = Duration::from_secs(10);
        ride.start(true, begin, OPEN);
        let mut now = begin;
        let mut last = PIT[2];
        let mut ticks = 0;
        while let Some(moved) = ride.tick(now) {
            assert!(z(&moved) >= last, "{} after {last}", z(&moved));
            last = z(&moved);
            now += RIDE_TICK;
            ticks += 1;
        }
        assert_eq!(last, UP[2], "the last tick is the up pose exactly");
        assert_eq!(ticks, 331, "ticks at 0, 100, ... 33000 ms");
        assert_eq!(ride.pose(), UP);
        assert!(!ride.moving());
    }

    #[test]
    fn the_first_tick_is_the_pit_pose_and_the_middle_is_halfway() {
        let mut ride = Ride::at_pit(5, PIT, UP);
        ride.start(true, Duration::ZERO, OPEN);
        assert_eq!(z(&ride.tick(Duration::ZERO).unwrap()), PIT[2]);
        let half = z(&ride.tick(OPEN / 2).unwrap());
        assert!((half - (PIT[2] + UP[2]) / 2.0).abs() < 1e-9, "{half}");
    }

    #[test]
    fn the_ship_descends_to_the_pit_from_where_it_is() {
        let mut ride = Ride::at_pit(5, PIT, UP);
        ride.start(true, Duration::ZERO, OPEN);
        ride.tick(OPEN).unwrap();
        ride.start(false, Duration::from_secs(100), Duration::from_micros(31_666_700));
        let mut now = Duration::from_secs(100);
        let mut last = UP[2];
        while let Some(moved) = ride.tick(now) {
            assert!(z(&moved) <= last);
            last = z(&moved);
            now += RIDE_TICK;
        }
        assert_eq!(last, PIT[2]);
        assert_eq!(ride.pose(), PIT);
    }

    #[test]
    fn a_tick_long_after_the_end_is_the_end_pose() {
        let mut ride = Ride::at_pit(5, PIT, UP);
        ride.start(true, Duration::from_secs(1), OPEN);
        assert_eq!(z(&ride.tick(Duration::from_secs(500)).unwrap()), UP[2]);
        assert_eq!(ride.tick(Duration::from_secs(501)), None);
    }
}

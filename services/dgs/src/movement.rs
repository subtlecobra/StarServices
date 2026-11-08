//! Where a player is: the DGS owns the server's movement state of each player it holds.
//!
//! The replicant gives the client its own movement (`m_clientAuthoritative` 0x10, see "Smooth movement" in
//! the protocol notes (kept outside this repository)), so the client moves itself and reports where it went in each frame (`SClientAuthoritativePosition`).
//! The server takes each report it believes and keeps its state where it was for one it does not: a report in another
//! zone, faster than twice the sprint speed, through solid geometry or rising where nothing holds the player up (see
//! "Client positions" in docs/DGS_PHYSICS.md). A refusal is not sent back as a correction: in that mode the client does
//! not compare itself with the server's state, and no real server corrects it either (its `DoMovementCheatCheck` only
//! reports `ActorMovementAnomaly`).
//!
//! Without reports, the server moves the player from its input: each frame moves it over the client's own time since the
//! frame before, so the state is where the client was at its last frame. Where the hangar's collision is loaded
//! (`physics::World`), the player's body moves against it: walls, stairs, ramps, gravity, jumping and crouching, see
//! docs/DGS_PHYSICS.md. Everywhere else, and until the hangar's collision is built, it is the simplest model that walks: a
//! flat plane at the spawn height, in the frame of the zone host, at a constant speed, with no collision but the bounds of
//! the hangar's grid. The two meet in one branch of `advance`. That simulation is the fallback only: while the client
//! reports, it never runs.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tracing::{info, warn};

use crate::physics::{self, check, World};
use crate::player::Placement;

/// Run speed, m/s: what the client's own player moved at holding W in the hangar, measured from its position frame by
/// frame on a running client. Strafing and walking backwards are taken to be as fast, which has not been measured.
pub const RUN_SPEED: f64 = 5.7;
/// With Shift held, measured the same way.
pub const SPRINT_SPEED: f64 = 7.3;
/// The client sends a frame about every 33 ms, buttons or not. Input older than this stops the player, so a client that
/// goes quiet does not walk on.
pub const INPUT_TIMEOUT: Duration = Duration::from_millis(250);
/// A frame never moves the player over more than this, whatever its time says.
const LONGEST_FRAME: f64 = 0.1;
/// The client's clock counts 100 ns.
const TICKS_PER_SECOND: f64 = 10_000_000.0;
/// A quiet client's player keeps falling on the tick once nothing has come for this long, so it lands rather than hangs
/// in the air. Under `INPUT_TIMEOUT`, so its buttons still hold for that time.
const QUIET_PHYSICS: Duration = Duration::from_millis(100);
/// The view's pitch stops short of straight up and down.
const PITCH_LIMIT: f64 = 89.0 * std::f64::consts::PI / 180.0;

/// `SMoveButtonMask` bits, as CryEngine's `EMoveButtonMask`. Forward is confirmed on a running client (W sends 1 and moves
/// along +Y with no rotation); the others follow the engine's order.
pub const FORWARD: u32 = 1;
pub const BACK: u32 = 2;
pub const LEFT: u32 = 4;
pub const RIGHT: u32 = 8;

/// An axis-aligned box in the zone host's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Bounds {
    fn contains(&self, point: [f64; 3]) -> bool {
        (0..3).all(|axis| self.min[axis] <= point[axis] && point[axis] <= self.max[axis])
    }

    fn clamp(&self, point: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|axis| point[axis].clamp(self.min[axis], self.max[axis]))
    }
}

/// The physics grid of the small front hangar of the rest stops (`hangar_smlfrnt_002_rest_rund`), in the hangar's frame, read
/// from a running client. A player who leaves it leaves the hangar's zone, which this model does not follow.
pub const SMALL_FRONT_HANGAR_GRID: Bounds = Bounds { min: [-54.0, -92.0, -29.0], max: [54.0, -2.5, 52.0] };

/// One frame of input, as the replicant decoded it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    /// The client's time at the end of the frame, in 100 ns ticks.
    pub time: u64,
    pub move_buttons: u32,
    pub sprint: bool,
    /// The view's aim at the start of the frame, absolute in the zone's frame, `x y z w` (`m_originAimRotation`).
    pub aim_rotation: Option<[f64; 4]>,
    /// Where the client put its actor at the end of the frame, when it owns its movement (`m_clientAuthoritative`).
    pub client_position: Option<ClientPosition>,
    /// The replicant could not read the whole frame: buttons and sprint not read in it are those of the frame before.
    pub partial: bool,
    /// Jump actions of the frame, as `InputFrame.jump`: bit 1 starts a jump.
    pub jump: u32,
    /// `SStance`, when the frame changed it: 2 crouches, 3 (prone, inferred) is taken as a crouch.
    pub stance: Option<u32>,
}

/// The end of a client's `SClientAuthoritativePosition`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClientPosition {
    pub zone_host: u64,
    pub position: [f64; 3],
    /// `x y z w`.
    pub base_rotation: [f64; 4],
}

/// The most a client may move its actor in a second and still be taken at its word: twice the sprint speed, as a real
/// server's cheat check (`DoMovementCheatCheck`) allows, plus a margin for a frame's jitter. The time is the client's since
/// the last report taken, up to `LONGEST_ALLOWANCE`, so a client that hitched catches up in one report.
const CLIENT_SPEED_LIMIT: f64 = 2.0 * SPRINT_SPEED;
const CLIENT_MOVE_MARGIN: f64 = 0.5;
const LONGEST_ALLOWANCE: f64 = 5.0;
/// A frame without a report this soon after one leaves the player where the client last put it, rather than simulating:
/// a client in charge skips the report in some states (dead, staging, a synced teleport, jumping off; RE repo
/// `GatherClientAuthoritativePosition`). Seconds of the client's time.
const REPORT_HOLD: f64 = 0.25;
/// Rising with nothing underfoot: from the last report taken that stood on something, the feet stay under a jump's arc
/// at half the gravity, plus a margin. A jump (0.5 m, 3.13 m/s up) and any fall stay well under it; hovering is refused
/// after about 1.4 s, and climbing more than 1.5 m above the last ground at once.
const JUMP_SPEED: f64 = 3.13;
const AIR_GRAVITY: f64 = 9.81 / 2.0;
const AIR_MARGIN: f64 = 0.5;
/// After this long of refused reports, the server takes the client's word again if the reports since the first refused
/// one never stepped through geometry and the latest stands on something: a client that hitched around a corner, or rode
/// something the collision does not hold (an elevator), is not refused for ever. Seconds of the client's time.
const REANCHOR_AFTER: f64 = 3.0;
/// The longest step between two refused reports that is judged for geometry: twice the sprint speed over the longest
/// network frame (133 ms, the "Garbage" frame rate gate) is under 2 m. A longer one, after a hitch, is a straight line
/// that says nothing about the path.
const JUDGED_STEP: f64 = 2.0;

/// Why a client's report was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// In another zone than the server's state.
    Zone,
    /// Farther than the speed limit allows.
    Speed,
    /// Through solid geometry.
    Wall,
    /// Rising where nothing holds the player up.
    Air,
}

/// What the server makes of the client's reports.
#[derive(Clone, Debug, Default, PartialEq)]
struct Reports {
    /// The client's time of the last frame that carried one, taken or not.
    last: Option<u64>,
    /// The client's time of the last one taken.
    taken: Option<u64>,
    /// The ground's height and the client's time of the last one taken that stood on something.
    support: Option<(f64, u64)>,
    /// The ones refused since the last one taken.
    refused: Option<Refused>,
    /// All refused, while the DGS holds the player.
    refused_total: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Refused {
    /// The client's time of the first.
    since: u64,
    count: u32,
    reason: Refusal,
    /// The latest, and whether any judged step from the server's state through them went through geometry.
    last: [f64; 3],
    crossed: bool,
}

fn distance(from: [f64; 3], to: [f64; 3]) -> f64 {
    (0..3).map(|axis| (to[axis] - from[axis]).powi(2)).sum::<f64>().sqrt()
}

fn seconds_between(from: u64, to: u64) -> f64 {
    to.saturating_sub(from) as f64 / TICKS_PER_SECOND
}

/// What `SCActorStateDataSynced::SMovement` and `SMovementAux` hold for one player.
#[derive(Clone, Debug, PartialEq)]
pub struct Movement {
    pub position: [f64; 3],
    /// About the zone's up axis, counter-clockwise seen from above, 0 facing +Y.
    pub yaw: f64,
    pub pitch: f64,
    pub zone_host: u64,
    pub parent: u64,
    pub revision: u64,
    bounds: Option<Bounds>,
    move_buttons: u32,
    sprint: bool,
    /// Since the last frame of input.
    quiet: Duration,
    /// The time of the last frame.
    last_frame: Option<u64>,
    /// The jump actions of the last frame, and the stance the client last chose.
    pub jump: u32,
    pub stance: Option<u32>,
    body: Body,
    /// The base rotation the client last reported, while it owns its movement.
    client_base: Option<[f64; 4]>,
    reports: Reports,
    /// The zone the player was bound in, its hangar.
    home: u64,
    /// Zones besides `home` the player may move into, such as the carriages of its hangar's elevator.
    vehicles: Vec<u64>,
}

/// The physics side: the hangar's collision once it is built, and the player's body in it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Body {
    /// The hangar socpak whose collision is looked for until it is built.
    socpak: Option<u32>,
    world: Option<Collision>,
    state: physics::State,
    /// A jump start not yet simulated.
    jump: bool,
    /// Seconds the tick simulated since the last frame, which the next frame does not simulate again.
    ticked: f64,
}

#[derive(Clone)]
struct Collision(Arc<World>);

impl std::fmt::Debug for Collision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "Collision({} triangles)", self.0.triangles())
    }
}

impl PartialEq for Collision {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The smallest move of the body that changes the published position, metres.
const PUBLISHED_STEP: f64 = 0.001;
const STAND: u32 = 1;
const JUMP_START: u32 = 1;

/// Yaw of a rotation `x y z w` about Z, as the client's yaw counts it.
fn yaw_of([x, y, z, w]: [f64; 4]) -> f64 {
    (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z))
}

/// Pitch of a rotation `x y z w` about X.
fn pitch_of([x, y, z, w]: [f64; 4]) -> f64 {
    (2.0 * (w * x - y * z)).clamp(-1.0, 1.0).asin()
}

impl Movement {
    /// A player standing where it was bound. `bounds` keeps it inside its zone; a spawn point outside them means they are
    /// not this zone's, and nothing is clamped.
    pub fn new(placement: &Placement, bounds: Option<Bounds>) -> Self {
        let position = placement.transform.position;
        Self {
            position,
            yaw: yaw_of(placement.transform.rotation),
            pitch: 0.0,
            zone_host: placement.parent_id,
            parent: placement.parent_id,
            revision: 0,
            bounds: bounds.filter(|bounds| bounds.contains(position)),
            move_buttons: 0,
            sprint: false,
            quiet: Duration::ZERO,
            last_frame: None,
            jump: 0,
            stance: None,
            body: Body { state: physics::State::standing_at(position, yaw_of(placement.transform.rotation)), ..Body::default() },
            client_base: None,
            reports: Reports::default(),
            home: placement.parent_id,
            vehicles: Vec::new(),
        }
    }

    /// Moves against the collision of the hangar socpak `socpak` once it is built (`World::from_bytes`, at startup), and on
    /// the flat plane until then.
    pub fn with_collision(mut self, socpak: u32) -> Self {
        self.body.socpak = Some(socpak);
        self
    }

    /// The collision the player moves against, looked up until it is built. Never builds it.
    /// The hangar's bounds, while the player is in the hangar's zone.
    fn home_bounds(&self) -> Option<Bounds> {
        self.bounds.filter(|_| self.zone_host == self.home)
    }

    /// The zones besides its hangar the player may move into.
    pub fn set_vehicles(&mut self, vehicles: &[u64]) {
        if self.vehicles != vehicles {
            self.vehicles = vehicles.to_vec();
        }
    }

    /// The hangar's collision, while the player is in the hangar's zone: a carriage's zone has positions of its own.
    fn world(&mut self) -> Option<Arc<World>> {
        if self.zone_host != self.home {
            return None;
        }
        if self.body.world.is_none() {
            self.body.world = self.body.socpak.and_then(World::loaded).map(Collision);
        }
        self.body.world.as_ref().map(|collision| collision.0.clone())
    }

    /// Takes one frame of input: the view turns, then the buttons held in the frame move the player for its length. The
    /// first frame, one out of order and one with no time only set the buttons.
    pub fn input(&mut self, input: &Input) {
        let previous = self.last_frame;
        let seconds = match previous {
            Some(last) if input.time > last => ((input.time - last) as f64 / TICKS_PER_SECOND).min(LONGEST_FRAME),
            _ => 0.0,
        };
        if input.time != 0 && self.last_frame.is_none_or(|last| input.time > last) {
            self.last_frame = Some(input.time);
        }
        let now = self.last_frame.unwrap_or(0);
        if !input.partial || input.move_buttons != 0 {
            self.move_buttons = input.move_buttons;
            self.sprint = input.sprint;
        }
        self.jump = input.jump;
        self.body.jump |= input.jump & JUMP_START != 0;
        self.stance = input.stance.or(self.stance);
        self.quiet = Duration::ZERO;
        if let Some(aim) = input.aim_rotation {
            let (yaw, pitch) = (yaw_of(aim).rem_euclid(std::f64::consts::TAU), pitch_of(aim).clamp(-PITCH_LIMIT, PITCH_LIMIT));
            if yaw != self.yaw || pitch != self.pitch {
                self.yaw = yaw;
                self.pitch = pitch;
                self.revision += 1;
            }
        }
        let seconds = (seconds - std::mem::take(&mut self.body.ticked)).max(0.0);
        match &input.client_position {
            Some(client) => self.report(client, now, previous, seconds),
            // The client is in charge and skipped a report: the player stays where it put it.
            None if self.client_moves() => {}
            None => self.advance(seconds),
        }
    }

    /// Whether the last frame came from a client in charge of its movement, which the tick then leaves alone.
    fn client_moves(&self) -> bool {
        self.reports.last.is_some_and(|last| seconds_between(last, self.last_frame.unwrap_or(0)) <= REPORT_HOLD)
    }

    /// Reports refused while the DGS has held the player.
    pub fn refused_reports(&self) -> u64 {
        self.reports.refused_total
    }

    /// One report of a client in charge of its movement, at the client's time `now`: taken, or refused and the state kept.
    fn report(&mut self, client: &ClientPosition, now: u64, previous: Option<u64>, seconds: f64) {
        self.reports.last = Some(now);
        // A step into a carriage of its elevator, or back into its hangar: the zone the client reports is the player's
        // from here, at the place it reports in that zone. The zone's own frame is the client's, so nothing is judged.
        let allowed = client.zone_host == self.home || self.vehicles.contains(&client.zone_host);
        if client.zone_host != self.zone_host && allowed {
            info!(from = self.zone_host, to = client.zone_host, at = ?client.position, "player changed zone");
            self.zone_host = client.zone_host;
            self.parent = client.zone_host;
            self.reports.refused = None;
            self.reports.support = None;
            self.reports.taken = Some(now);
            self.revision += 1;
            self.follow(client, true, seconds);
            return;
        }
        let world = self.world();
        match self.judge(world.as_deref(), client, now, previous) {
            Ok(support) => self.take(client, now, support, seconds),
            Err(reason) => {
                self.reports.refused_total += 1;
                let refused = self.reports.refused.get_or_insert_with(|| {
                    warn!(?reason, from = ?self.position, to = ?client.position, zone = client.zone_host, "client position refused");
                    Refused { since: now, count: 0, reason, last: self.position, crossed: false }
                });
                let short = reason != Refusal::Zone && distance(refused.last, client.position) <= JUDGED_STEP;
                refused.crossed |= world.as_deref().is_some_and(|world| short && check::crosses(world, refused.last, client.position));
                refused.last = client.position;
                refused.count += 1;
                refused.reason = reason;
                let refused = *refused;
                // Nothing in the trail went through a wall, and the client stands on something: take it from here.
                let support = world.as_deref().and_then(|world| check::support(world, client.position));
                let stands = world.is_none() || support.is_some();
                if reason != Refusal::Zone && !refused.crossed && stands && seconds_between(refused.since, now) >= REANCHOR_AFTER {
                    self.take(client, now, support, seconds);
                }
            }
        }
    }

    /// Whether a report is believable: in this zone, within the speed limit and, against the hangar's collision, not
    /// through solid geometry nor rising with nothing underfoot. Taken, it is what the feet stand on, if known.
    fn judge(&self, world: Option<&World>, client: &ClientPosition, now: u64, previous: Option<u64>) -> Result<Option<f64>, Refusal> {
        if client.zone_host != self.zone_host {
            return Err(Refusal::Zone);
        }
        let elapsed = self.reports.taken.or(previous).map_or(0.0, |since| seconds_between(since, now)).min(LONGEST_ALLOWANCE);
        if distance(self.position, client.position) > CLIENT_SPEED_LIMIT * elapsed + CLIENT_MOVE_MARGIN {
            return Err(Refusal::Speed);
        }
        let Some(world) = world else { return Ok(None) };
        if check::crosses(world, self.position, client.position) {
            return Err(Refusal::Wall);
        }
        let support = check::support(world, client.position);
        if let (None, Some((height, at))) = (support, self.reports.support) {
            let air = seconds_between(at, now);
            if client.position[2] > height + JUMP_SPEED * air - AIR_GRAVITY * air * air / 2.0 + AIR_MARGIN {
                return Err(Refusal::Air);
            }
        }
        Ok(support)
    }

    fn take(&mut self, client: &ClientPosition, now: u64, support: Option<f64>, seconds: f64) {
        if let Some(refused) = self.reports.refused.take() {
            let after = seconds_between(refused.since, now);
            info!(count = refused.count, reason = ?refused.reason, after, at = ?client.position, "client position taken again");
        }
        self.reports.taken = Some(now);
        if let Some(ground) = support {
            self.reports.support = Some((ground, now));
        }
        self.follow(client, support.is_some(), seconds);
    }

    /// Takes the client's word for where its actor is: the client moves itself while it owns its movement, and the
    /// state follows it exactly. The body follows too, so the simulation goes on from there if the reports stop.
    fn follow(&mut self, client: &ClientPosition, grounded: bool, seconds: f64) {
        let position = self.home_bounds().map_or(client.position, |bounds| bounds.clamp(client.position));
        let body = &mut self.body.state;
        if seconds > 0.0 {
            body.velocity = std::array::from_fn(|axis| (position[axis] - body.position[axis]) / seconds);
        }
        body.position = position;
        body.grounded = grounded;
        body.stance = if self.stance.is_some_and(|stance| stance != STAND) { physics::Stance::Crouch } else { physics::Stance::Stand };
        if position != self.position || self.client_base != Some(client.base_rotation) {
            self.position = position;
            self.client_base = Some(client.base_rotation);
            self.revision += 1;
        }
    }

    /// Where the buttons point, in the zone's frame, as a unit vector or zero.
    fn heading(&self) -> [f64; 2] {
        let held = |bit| f64::from(u8::from(self.move_buttons & bit != 0));
        let (right, forward) = (held(RIGHT) - held(LEFT), held(FORWARD) - held(BACK));
        let length = right.hypot(forward);
        if length == 0.0 {
            return [0.0, 0.0];
        }
        let (sin, cos) = self.yaw.sin_cos();
        [(right * cos - forward * sin) / length, (right * sin + forward * cos) / length]
    }

    /// One tick: a player whose client has sent nothing for a while lets go of its buttons, and with a collision keeps
    /// falling and landing, unless the client was in charge of its movement: then the next report says where it went.
    pub fn step(&mut self, elapsed: Duration) {
        self.quiet += elapsed;
        if self.quiet > INPUT_TIMEOUT {
            self.move_buttons = 0;
        }
        if self.quiet > QUIET_PHYSICS && self.body.world.is_some() && !self.client_moves() {
            let seconds = elapsed.as_secs_f64().min(LONGEST_FRAME);
            self.body.ticked = (self.body.ticked + seconds).min(LONGEST_FRAME);
            self.advance(seconds);
        }
    }

    /// Moves the player as its buttons say, for `seconds`.
    fn advance(&mut self, seconds: f64) {
        if let Some(world) = self.world() {
            return self.advance_against(&world, seconds);
        }
        let [x, y] = self.heading();
        if seconds == 0.0 || (x == 0.0 && y == 0.0) {
            return;
        }
        let distance = if self.sprint { SPRINT_SPEED } else { RUN_SPEED } * seconds;
        let moved = [self.position[0] + x * distance, self.position[1] + y * distance, self.position[2]];
        let moved = self.home_bounds().map_or(moved, |bounds| bounds.clamp(moved));
        if moved != self.position {
            self.position = moved;
            self.revision += 1;
        }
    }

    /// The physics model: the body against the hangar's collision, at the client's measured speeds. No grid clamp, as
    /// the collision holds the player in.
    fn advance_against(&mut self, world: &World, seconds: f64) {
        if seconds == 0.0 {
            return;
        }
        let held = |bit| f64::from(u8::from(self.move_buttons & bit != 0));
        let input = physics::Input {
            wish: [held(RIGHT) - held(LEFT), held(FORWARD) - held(BACK)],
            yaw: self.yaw,
            sprint: self.sprint,
            jump: std::mem::take(&mut self.body.jump),
            crouch: self.stance.is_some_and(|stance| stance != STAND),
        };
        let params = physics::Params { run_speed: RUN_SPEED, sprint_speed: SPRINT_SPEED, ..physics::Params::default() };
        self.body.state = physics::step(world, &self.body.state, &input, seconds, &params);
        // Against a wall the body settles by micrometres each frame, which is no reason to send an update.
        let moved = (0..3).map(|axis| (self.body.state.position[axis] - self.position[axis]).powi(2)).sum::<f64>().sqrt();
        if moved >= PUBLISHED_STEP {
            self.position = self.body.state.position;
            self.revision += 1;
        }
    }

    /// Whether the body stands on the ground, and crouches. `None` on the flat plane.
    pub fn body(&self) -> Option<(bool, physics::Stance)> {
        self.body.world.as_ref().map(|_| (self.body.state.grounded, self.body.state.stance))
    }

    /// The client's time at the end of the last frame this state includes, 0 before the first: what the client compares
    /// the state with (`m_actionFrameEndTimestamp`).
    pub fn frame_time(&self) -> u64 {
        self.last_frame.unwrap_or(0)
    }

    /// `m_baseRotation`: the body turns about the up axis only.
    pub fn base_rotation(&self) -> [f64; 4] {
        if let Some(base) = self.client_base {
            return base;
        }
        let (sin, cos) = (self.yaw / 2.0).sin_cos();
        [0.0, 0.0, sin, cos]
    }

    /// `m_aimRotation`: the yaw, then the pitch about the turned X axis.
    pub fn aim_rotation(&self) -> [f64; 4] {
        let (sz, cz) = (self.yaw / 2.0).sin_cos();
        let (sx, cx) = (self.pitch / 2.0).sin_cos();
        [cz * sx, sz * sx, sz * cx, cz * cx]
    }
}

/// The movement of every player a DGS holds, by player entity.
#[derive(Default)]
pub struct Movements(HashMap<u64, Movement>);

impl Movements {
    pub fn join(&mut self, player: u64, movement: Movement) {
        self.0.insert(player, movement);
    }

    pub fn leave(&mut self, player: u64) {
        self.0.remove(&player);
    }

    pub fn get(&self, player: u64) -> Option<&Movement> {
        self.0.get(&player)
    }

    pub fn get_mut(&mut self, player: u64) -> Option<&mut Movement> {
        self.0.get_mut(&player)
    }

    /// Each player's zone host, as the DGS has it.
    pub fn zones(&self) -> Vec<(u64, u64)> {
        self.0.iter().map(|(player, movement)| (*player, movement.zone_host)).collect()
    }

    /// The zones besides their hangars that the players may move into.
    pub fn set_vehicles(&mut self, vehicles: &[u64]) {
        for movement in self.0.values_mut() {
            movement.set_vehicles(vehicles);
        }
    }

    /// Keeps the players for which `keep` holds.
    pub fn retain(&mut self, mut keep: impl FnMut(u64) -> bool) {
        self.0.retain(|player, _| keep(*player));
    }

    /// One tick for everyone.
    pub fn step(&mut self, elapsed: Duration) {
        for movement in self.0.values_mut() {
            movement.step(elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::Transform;

    const HANGAR: u64 = 220_000_065_536;
    const TICK: Duration = Duration::from_nanos(33_333_333);
    const SPAWN: [f64; 3] = [5.9, -85.14, 4.51];

    fn spawned() -> Movement {
        let placement = Placement { parent_id: HANGAR, attach_id: 0, transform: Transform { position: SPAWN, rotation: [0.0, 0.0, 0.0, 1.0] } };
        Movement::new(&placement, Some(SMALL_FRONT_HANGAR_GRID))
    }

    /// A frame every 1/30 s, as the client sends them.
    const FRAME: u64 = 333_333;

    /// `seconds` of frames holding `buttons`, after the frames `movement` has had.
    fn walk(movement: &mut Movement, buttons: u32, seconds: u32) {
        walk_with(movement, Input { move_buttons: buttons, ..Input::default() }, seconds);
    }

    fn walk_with(movement: &mut Movement, input: Input, seconds: u32) {
        let start = movement.last_frame.unwrap_or_else(|| {
            movement.input(&Input { time: 1_000 * FRAME, ..Input::default() });
            1_000 * FRAME
        });
        for frame in 1..=u64::from(seconds) * 30 {
            movement.input(&Input { time: start + frame * FRAME, ..input });
            movement.step(TICK);
        }
    }

    fn close(left: [f64; 3], right: [f64; 3]) -> bool {
        (0..3).all(|axis| (left[axis] - right[axis]).abs() < 1e-4)
    }

    /// A turn about Z by `angle`, `x y z w`.
    fn turn(angle: f64) -> [f64; 4] {
        let (sin, cos) = (angle / 2.0).sin_cos();
        [0.0, 0.0, sin, cos]
    }

    #[test]
    fn the_player_starts_where_it_was_bound_in_the_hangar() {
        let movement = spawned();
        assert_eq!((movement.position, movement.zone_host, movement.parent, movement.revision), (SPAWN, HANGAR, HANGAR, 0));
        assert_eq!(movement.base_rotation(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn forward_runs_along_y_at_the_measured_speed() {
        let mut movement = spawned();
        walk(&mut movement, FORWARD, 1);
        assert!(close(movement.position, [5.9, -85.14 + RUN_SPEED, 4.51]), "{:?}", movement.position);
        assert!(movement.revision > 0);
    }

    #[test]
    fn the_buttons_follow_the_engines_axes() {
        let cases = [(BACK, [0.0, -1.0]), (LEFT, [-1.0, 0.0]), (RIGHT, [1.0, 0.0])];
        for (buttons, [x, y]) in cases {
            let mut movement = spawned();
            walk(&mut movement, buttons, 1);
            assert!(close(movement.position, [5.9 + x * RUN_SPEED, -85.14 + y * RUN_SPEED, 4.51]), "{buttons}: {:?}", movement.position);
        }
    }

    #[test]
    fn diagonals_are_no_faster() {
        let mut movement = spawned();
        walk(&mut movement, FORWARD | RIGHT, 1);
        let moved = (movement.position[0] - SPAWN[0]).hypot(movement.position[1] - SPAWN[1]);
        assert!((moved - RUN_SPEED).abs() < 1e-4, "{moved}");
    }

    #[test]
    fn opposite_buttons_cancel() {
        let mut movement = spawned();
        walk(&mut movement, FORWARD | BACK, 1);
        assert_eq!((movement.position, movement.revision), (SPAWN, 0));
    }

    #[test]
    fn sprint_is_faster() {
        let mut movement = spawned();
        walk_with(&mut movement, Input { move_buttons: FORWARD, sprint: true, ..Input::default() }, 1);
        assert!(close(movement.position, [5.9, -85.14 + SPRINT_SPEED, 4.51]), "{:?}", movement.position);
    }

    #[test]
    fn a_turn_left_makes_forward_run_along_minus_x() {
        let mut movement = spawned();
        movement.input(&Input { aim_rotation: Some(turn(std::f64::consts::FRAC_PI_2)), ..Input::default() });
        assert_eq!(movement.revision, 1);
        walk(&mut movement, FORWARD, 1);
        assert!(close(movement.position, [5.9 - RUN_SPEED, -85.14, 4.51]), "{:?}", movement.position);
        let [x, y, z, w] = movement.base_rotation();
        assert!(x == 0.0 && y == 0.0 && (z - w).abs() < 1e-9 && z > 0.0);
    }

    #[test]
    fn the_aim_is_absolute_and_the_latest_wins() {
        let mut movement = spawned();
        for _ in 0..6 {
            movement.input(&Input { aim_rotation: Some(turn(-std::f64::consts::FRAC_PI_4)), ..Input::default() });
        }
        assert!((movement.yaw - 7.0 * std::f64::consts::FRAC_PI_4).abs() < 1e-9, "{}", movement.yaw);
        assert_eq!(movement.revision, 1, "the same aim again changes nothing");
    }

    fn client_at(position: [f64; 3]) -> ClientPosition {
        ClientPosition { zone_host: HANGAR, position, base_rotation: turn(0.3) }
    }

    #[test]
    fn a_client_that_owns_its_movement_is_followed_exactly() {
        let mut movement = spawned();
        movement.input(&Input { time: 10_000_000, ..Input::default() });
        let client = client_at([5.9, -85.14 + 0.06, 4.51]);
        movement.input(&Input { time: 10_333_333, move_buttons: FORWARD, client_position: Some(client), ..Input::default() });
        assert_eq!(movement.position, client.position, "not the server's own 5.7 m/s");
        assert_eq!(movement.base_rotation(), turn(0.3));
        assert_eq!(movement.revision, 1);
    }

    #[test]
    fn a_client_too_fast_or_in_another_zone_is_not_believed() {
        let mut movement = spawned();
        movement.input(&Input { time: 10_000_000, ..Input::default() });
        let far = client_at([5.9, -80.0, 4.51]);
        movement.input(&Input { time: 10_333_333, client_position: Some(far), ..Input::default() });
        assert_eq!(movement.position, SPAWN, "5 m in a thirtieth of a second");
        let elsewhere = ClientPosition { zone_host: 1, ..client_at([5.9, -85.0, 4.51]) };
        movement.input(&Input { time: 10_666_666, client_position: Some(elsewhere), ..Input::default() });
        assert_eq!(movement.position, SPAWN);
        assert_eq!(movement.base_rotation(), [0.0, 0.0, 0.0, 1.0]);
    }

    /// A carriage of the hangar's elevator is a zone of its own: the player steps into it when the DGS allows that zone,
    /// at the place the client reports in it, and back into the hangar.
    #[test]
    fn a_player_steps_into_an_allowed_carriage_and_back() {
        const CARRIAGE: u64 = 230_000_000_000;
        let mut movement = spawned();
        movement.input(&Input { time: 10_000_000, ..Input::default() });
        let inside = ClientPosition { zone_host: CARRIAGE, ..client_at([0.5, 0.2, 0.1]) };
        movement.input(&Input { time: 10_333_333, client_position: Some(inside), ..Input::default() });
        assert_eq!(movement.zone_host, HANGAR, "not a carriage of its elevator yet");
        movement.set_vehicles(&[CARRIAGE]);
        movement.input(&Input { time: 10_666_666, client_position: Some(inside), ..Input::default() });
        assert_eq!((movement.zone_host, movement.parent, movement.position), (CARRIAGE, CARRIAGE, [0.5, 0.2, 0.1]));
        let back = client_at([5.9, -85.1, 4.51]);
        movement.input(&Input { time: 11_000_000, client_position: Some(back), ..Input::default() });
        assert_eq!((movement.zone_host, movement.position), (HANGAR, back.position));
    }

    #[test]
    fn pitch_turns_the_aim_and_not_the_body() {
        let mut movement = spawned();
        let (sin, cos) = (0.2f64).sin_cos();
        movement.input(&Input { aim_rotation: Some([sin, 0.0, 0.0, cos]), ..Input::default() });
        assert!((movement.pitch - 0.4).abs() < 1e-9);
        assert_eq!(movement.base_rotation(), [0.0, 0.0, 0.0, 1.0]);
        let [x, _, _, w] = movement.aim_rotation();
        assert!((x - sin).abs() < 1e-9 && (w - cos).abs() < 1e-9);
    }

    #[test]
    fn the_player_stops_at_the_edge_of_the_hangars_grid() {
        let mut movement = spawned();
        walk(&mut movement, BACK, 10);
        assert!(close(movement.position, [5.9, -92.0, 4.51]), "{:?}", movement.position);
        let revision = movement.revision;
        walk(&mut movement, BACK, 1);
        assert_eq!(movement.revision, revision, "nothing changes against the edge");
    }

    #[test]
    fn bounds_that_do_not_hold_the_spawn_point_are_not_the_zones() {
        let placement = Placement { parent_id: 1, attach_id: 0, transform: Transform::IDENTITY };
        let mut movement = Movement::new(&placement, Some(SMALL_FRONT_HANGAR_GRID));
        walk(&mut movement, FORWARD, 1);
        assert!(close(movement.position, [0.0, RUN_SPEED, 0.0]));
    }

    #[test]
    fn a_client_that_goes_quiet_stops() {
        let mut movement = spawned();
        walk(&mut movement, FORWARD, 1);
        for _ in 0..30 {
            movement.step(TICK);
        }
        let revision = movement.revision;
        // The next frame, after a second of nothing, moves for no longer than a frame may last, holding nothing.
        let time = movement.last_frame.unwrap() + 30 * FRAME;
        movement.input(&Input { time, ..Input::default() });
        assert_eq!(movement.revision, revision);
    }

    #[test]
    fn a_partial_frame_keeps_the_buttons_it_could_not_read() {
        let mut movement = spawned();
        walk(&mut movement, FORWARD, 1);
        let time = movement.last_frame.unwrap() + FRAME;
        movement.input(&Input { time, partial: true, jump: 1, stance: Some(2), ..Input::default() });
        assert_eq!((movement.move_buttons, movement.jump, movement.stance), (FORWARD, 1, Some(2)));
        movement.input(&Input { time: time + FRAME, ..Input::default() });
        assert_eq!((movement.move_buttons, movement.jump, movement.stance), (0, 0, Some(2)));
    }

    #[test]
    fn a_frame_moves_for_its_own_length() {
        let mut movement = spawned();
        movement.input(&Input { time: 10_000_000, move_buttons: FORWARD, ..Input::default() });
        assert_eq!(movement.position, SPAWN, "the first frame has no length");
        movement.input(&Input { time: 12_000_000, move_buttons: FORWARD, ..Input::default() });
        assert!(close(movement.position, [5.9, -85.14 + RUN_SPEED * 0.1, 4.51]), "capped: {:?}", movement.position);
        movement.input(&Input { time: 12_500_000, move_buttons: FORWARD, ..Input::default() });
        assert!(close(movement.position, [5.9, -85.14 + RUN_SPEED * 0.15, 4.51]), "{:?}", movement.position);
        movement.input(&Input { time: 12_400_000, move_buttons: FORWARD, ..Input::default() });
        assert!(close(movement.position, [5.9, -85.14 + RUN_SPEED * 0.15, 4.51]), "out of order: {:?}", movement.position);
    }

    #[test]
    fn the_spawn_rotation_sets_the_heading() {
        let placement = Placement { parent_id: HANGAR, attach_id: 0, transform: Transform { position: SPAWN, rotation: turn(std::f64::consts::PI) } };
        let mut movement = Movement::new(&placement, Some(SMALL_FRONT_HANGAR_GRID));
        walk(&mut movement, FORWARD, 1);
        assert!(close(movement.position, [5.9, -85.14 - RUN_SPEED, 4.51]), "{:?}", movement.position);
    }

    /// `hangar_smlfrnt_002_rest_rund`, whose collision tests read from `testdata/`.
    const REST_STOP: u32 = crate::physics::world::REST_STOP;

    fn against_collision() -> Movement {
        crate::physics::world::rest_stop();
        spawned().with_collision(REST_STOP)
    }

    #[test]
    fn a_hangar_with_no_collision_built_stays_on_the_flat_plane() {
        let mut movement = spawned().with_collision(7);
        walk(&mut movement, BACK, 10);
        assert!(close(movement.position, [5.9, -92.0, 4.51]), "{:?}", movement.position);
        assert_eq!(movement.body(), None);
    }

    #[test]
    fn with_collision_the_spawn_closets_wall_stops_the_player() {
        let mut movement = against_collision();
        walk(&mut movement, FORWARD, 3);
        assert!(movement.position[1] < -82.0, "{:?}", movement.position);
        assert!((movement.position[2] - 4.49).abs() < 0.03, "on the floor: {:?}", movement.position);
        assert_eq!(movement.body(), Some((true, physics::Stance::Stand)));
        // The wall leans by a fraction of a degree after simplification, so pushing on it creeps along it.
        let before = movement.position;
        walk(&mut movement, FORWARD, 1);
        let moved = (movement.position[0] - before[0]).hypot(movement.position[1] - before[1]);
        assert!(moved < 0.05, "held by the wall: {moved}");
    }

    #[test]
    fn with_collision_the_player_walks_out_of_the_closet_at_the_run_speed() {
        let mut movement = against_collision();
        movement.input(&Input { aim_rotation: Some(turn(std::f64::consts::FRAC_PI_2)), ..Input::default() });
        // The door frame catches the capsule for half a second, then it slides past.
        walk(&mut movement, FORWARD, 3);
        let moved = SPAWN[0] - movement.position[0];
        assert!(moved > 2.0 * RUN_SPEED, "{:?}", movement.position);
    }

    #[test]
    fn with_collision_a_jump_rises_and_lands() {
        let mut movement = against_collision();
        walk(&mut movement, 0, 1);
        let floor = movement.position[2];
        walk_with(&mut movement, Input { jump: JUMP_START, ..Input::default() }, 0);
        let start = movement.last_frame.unwrap();
        movement.input(&Input { time: start + FRAME, jump: JUMP_START, ..Input::default() });
        let mut peak = floor;
        for frame in 2..40 {
            movement.input(&Input { time: start + frame * FRAME, ..Input::default() });
            peak = peak.max(movement.position[2]);
        }
        assert!((peak - floor - 0.5).abs() < 0.06, "{peak} over {floor}");
        assert!((movement.position[2] - floor).abs() < 1e-3 && movement.body().unwrap().0);
    }

    #[test]
    fn with_collision_a_quiet_client_still_lands() {
        let mut movement = against_collision();
        walk(&mut movement, 0, 1);
        let floor = movement.position[2];
        let start = movement.last_frame.unwrap();
        movement.input(&Input { time: start + FRAME, jump: JUMP_START, ..Input::default() });
        movement.input(&Input { time: start + 2 * FRAME, ..Input::default() });
        assert!(movement.position[2] > floor);
        for _ in 0..60 {
            movement.step(TICK);
        }
        assert!((movement.position[2] - floor).abs() < 1e-3 && movement.body().unwrap().0, "{:?}", movement.position);
        // The frame after the silence does not move for the time the tick already did.
        let before = movement.position;
        movement.input(&Input { time: start + 62 * FRAME, ..Input::default() });
        assert_eq!(movement.position, before);
    }

    // A client in charge of its movement, against the collision: what it reports is taken unless it goes through a wall
    // or flies, and the server's own simulation stays out of the way.

    fn report(movement: &mut Movement, time: u64, position: [f64; 3]) {
        let client = ClientPosition { zone_host: HANGAR, position, base_rotation: [0.0, 0.0, 0.0, 1.0] };
        movement.input(&Input { time, client_position: Some(client), ..Input::default() });
    }

    /// Reports `path`, one every `every` frames of 1/30 s, from `start`. The next time.
    fn report_path(movement: &mut Movement, start: u64, path: &[[f64; 3]], every: usize) -> u64 {
        let mut time = start;
        for position in path.iter().step_by(every) {
            time += every as u64 * FRAME;
            report(movement, time, *position);
        }
        time
    }

    /// The feet of the server's own body doing `inputs` for their frame counts, a frame each: a path a client could
    /// walk. The client's own physics differs in the details, which the offsets of the tests stand for.
    fn simulated(world: &World, start: [f64; 3], inputs: &[(physics::Input, usize)]) -> Vec<[f64; 3]> {
        let params = physics::Params { run_speed: RUN_SPEED, sprint_speed: SPRINT_SPEED, ..physics::Params::default() };
        let mut state = physics::State::standing_at(start, 0.0);
        let mut path = Vec::new();
        for (input, frames) in inputs {
            for _ in 0..*frames {
                state = physics::step(world, &state, input, 1.0 / 30.0, &params);
                path.push(state.position);
            }
        }
        path
    }

    fn in_world(boxes: &[([f32; 3], [f32; 3])]) -> Movement {
        let placement = Placement { parent_id: HANGAR, attach_id: 0, transform: Transform::IDENTITY };
        let mut movement = Movement::new(&placement, None);
        movement.body.world = Some(Collision(Arc::new(crate::physics::controller::tests::cuboid_world(boxes))));
        movement
    }

    use crate::physics::controller::tests::FLOOR;

    fn ahead(yaw: f64, sprint: bool, jump: bool, crouch: bool) -> physics::Input {
        physics::Input { wish: [0.0, 1.0], yaw, sprint, jump, crouch }
    }

    #[test]
    fn with_collision_a_client_walking_the_hangar_is_never_refused() {
        let world = crate::physics::world::rest_stop();
        let west = std::f64::consts::FRAC_PI_2;
        let inputs = [
            (physics::Input::default(), 15),
            (ahead(west, false, false, false), 30),
            (ahead(west, true, false, false), 60),
            (ahead(west, true, true, false), 1),
            (ahead(west, true, false, false), 30),
            (ahead(0.3, false, false, true), 45),
            (ahead(0.3 + std::f64::consts::PI, true, false, false), 60),
            (physics::Input { jump: true, ..physics::Input::default() }, 1),
            (physics::Input::default(), 30),
            (ahead(-west, true, false, false), 90),
        ];
        let path = simulated(&world, SPAWN, &inputs);
        assert!(path.iter().any(|feet| feet[0] < SPAWN[0] - 10.0), "the walk leaves the closet");
        // The client stands a little higher than the simplified floor (4.505 seen against 4.493), or lower.
        for (every, lift) in [(1, 0.012), (1, -0.02), (2, 0.0), (4, 0.012)] {
            let mut movement = against_collision();
            let lifted: Vec<[f64; 3]> = path.iter().map(|feet| [feet[0], feet[1], feet[2] + lift]).collect();
            movement.input(&Input { time: 1_000 * FRAME, ..Input::default() });
            report_path(&mut movement, 1_000 * FRAME, &lifted, every);
            assert_eq!(movement.refused_reports(), 0, "every {every} frames, {lift} m up");
            assert_eq!(movement.position, *lifted.iter().step_by(every).next_back().unwrap());
        }
    }

    #[test]
    fn with_collision_a_report_through_the_closets_back_wall_is_refused_until_the_client_comes_back() {
        let mut movement = against_collision();
        movement.input(&Input { time: 1_000 * FRAME, ..Input::default() });
        // The closet's back wall is at y -86.2, solid at every height traced, with a ledge 0.5 m up before and behind it.
        let floor = 4.505;
        let through: Vec<[f64; 3]> =
            (0..9).map(|step| SPAWN[1] - 0.2 * f64::from(step)).map(|y| [SPAWN[0], y, if y > -85.9 { floor } else { 5.005 }]).collect();
        let time = report_path(&mut movement, 1_000 * FRAME, &through, 1);
        assert!(movement.refused_reports() > 0);
        assert!(movement.position[1] > -86.2, "held inside: {:?}", movement.position);
        assert_eq!(movement.reports.refused.map(|refused| refused.reason), Some(Refusal::Wall));
        // Standing on the other side for long is still refused: the client went through.
        let beyond = *through.last().unwrap();
        let time = report_path(&mut movement, time, &[beyond; 150], 1);
        assert!(movement.position[1] > -86.2, "{:?}", movement.position);
        // Back inside, it is taken again.
        let back: Vec<[f64; 3]> = through.iter().rev().copied().collect();
        report_path(&mut movement, time, &back, 1);
        assert_eq!(movement.position, [SPAWN[0], SPAWN[1], floor]);
        assert_eq!(movement.reports.refused, None);
    }

    #[test]
    fn a_client_climbing_stairs_jumping_and_stepping_onto_a_crate_is_never_refused() {
        let mut boxes = vec![FLOOR];
        for step in 0..5 {
            let y = 2.0 + 0.3 * step as f32;
            boxes.push(([-2.0, y, 0.0], [2.0, 4.0, 0.2 * (step + 1) as f32]));
        }
        boxes.push(([-2.0, 4.0, 0.0], [2.0, 20.0, 1.0]));
        let mut movement = in_world(&boxes);
        let world = movement.world().unwrap();
        let path = simulated(&world, [0.0, 0.0, 0.0], &[(ahead(0.0, false, false, false), 45), (ahead(0.0, false, true, false), 1), (ahead(0.0, false, false, false), 30)]);
        assert!(path.last().unwrap()[2] > 0.99, "up the stairs: {:?}", path.last());
        let time = report_path(&mut movement, 1_000 * FRAME, &path, 1);
        // Down again, and off the landing's side (1 m) to the floor.
        let down = simulated(&world, *path.last().unwrap(), &[(ahead(std::f64::consts::FRAC_PI_2, true, false, false), 30)]);
        assert!(down.last().unwrap()[2].abs() < 1e-3, "{:?}", down.last());
        let time = report_path(&mut movement, time, &down, 1);
        assert_eq!(movement.refused_reports(), 0);
        // A crate of 0.6 m, more than the server would step: the client's own step is its own.
        let mut crate_world = in_world(&[FLOOR, ([-2.0, 1.0, 0.0], [2.0, 3.0, 0.6])]);
        let onto: Vec<[f64; 3]> = (0..10).map(|step| [0.0, 0.1 * f64::from(step), if step < 7 { 0.0 } else { 0.6 }]).collect();
        report_path(&mut crate_world, time, &onto, 1);
        assert_eq!(crate_world.refused_reports(), 0);
    }

    #[test]
    fn a_client_rising_with_nothing_underfoot_is_refused_and_taken_again_once_it_lands() {
        let mut movement = in_world(&[FLOOR]);
        let start = 1_000 * FRAME;
        report(&mut movement, start, [0.0, 0.0, 0.0]);
        // Up 0.1 m a frame, for two seconds.
        let up: Vec<[f64; 3]> = (1..=60).map(|frame| [0.0, 0.0, 0.1 * f64::from(frame)]).collect();
        let time = report_path(&mut movement, start, &up, 1);
        assert!(movement.position[2] < 1.6, "{:?}", movement.position);
        assert_eq!(movement.reports.refused.map(|refused| refused.reason), Some(Refusal::Air));
        // Hovering at 1 m is refused too, once a jump would have come down.
        let mut hover = in_world(&[FLOOR]);
        report(&mut hover, start, [0.0, 0.0, 0.0]);
        let level: Vec<[f64; 3]> = (1..=60).map(|frame| [0.05 * f64::from(frame), 0.0, 1.0]).collect();
        let time_hover = report_path(&mut hover, start, &level, 1);
        assert!(hover.refused_reports() > 0 && hover.position[0] < 2.0, "{:?}", hover.position);
        // Down on the floor, it is taken.
        report(&mut hover, time_hover + FRAME, [3.0, 0.0, 0.0]);
        assert_eq!(hover.position, [3.0, 0.0, 0.0]);
        report(&mut movement, time + FRAME, [0.0, 0.0, 0.0]);
        assert_eq!(movement.position, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_client_that_hitched_around_a_corner_is_taken_again() {
        // A wall from x 1 along +Y: a hitch takes the client from in front of it to behind it in one report.
        let mut movement = in_world(&[FLOOR, ([1.0, -1.0, 0.0], [1.5, 10.0, 4.0])]);
        let start = 1_000 * FRAME;
        report(&mut movement, start, [0.0, 0.0, 0.0]);
        let behind = [3.0, -2.5, 0.0];
        report(&mut movement, start + 15 * FRAME, behind);
        assert_eq!((movement.position, movement.reports.refused.map(|refused| refused.reason)), ([0.0, 0.0, 0.0], Some(Refusal::Wall)));
        let mut path: Vec<[f64; 3]> = (1..=100).map(|frame| [3.0, -2.5 + 0.02 * f64::from(frame), 0.0]).collect();
        report_path(&mut movement, start + 15 * FRAME, &path, 1);
        assert_eq!(movement.position, path.pop().unwrap(), "taken again after three seconds");
        assert_eq!(movement.reports.refused, None);
    }

    #[test]
    fn a_client_in_charge_is_not_simulated() {
        let mut movement = against_collision();
        movement.input(&Input { time: 1_000 * FRAME, ..Input::default() });
        let feet = [SPAWN[0], SPAWN[1], 4.505];
        report(&mut movement, 1_001 * FRAME, feet);
        // A frame that skipped the report, holding W, and quiet ticks: the player stays where the client put it.
        movement.input(&Input { time: 1_002 * FRAME, move_buttons: FORWARD, ..Input::default() });
        for _ in 0..10 {
            movement.step(TICK);
        }
        assert_eq!(movement.position, feet);
        // Once the reports stop for longer, the server moves the player itself.
        walk(&mut movement, FORWARD, 1);
        assert!(movement.position[1] > feet[1] + 1.0, "{:?}", movement.position);
    }

    #[test]
    fn with_collision_the_stance_crouches() {
        let mut movement = against_collision();
        walk_with(&mut movement, Input { stance: Some(2), ..Input::default() }, 1);
        assert_eq!(movement.body(), Some((true, physics::Stance::Crouch)));
        walk_with(&mut movement, Input { stance: Some(1), ..Input::default() }, 1);
        assert_eq!(movement.body(), Some((true, physics::Stance::Stand)));
    }
}

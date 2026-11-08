//! The rides of one manager's elevator, simulated: calls, picks, doors, carriages in limbo, the readiness gate, the
//! slow-rider split and riders who leave. docs/DGS_TRANSIT.md is the design and its "Simulation" section describes this
//! module; this one only says how the code holds it.
//!
//! A `Line` is pure and deterministic: no network, no clock, no task. Each step takes the time it happens at and an
//! `Event` (or none, for `Line::tick`), and returns the `Command`s for the DGS to apply, in order. The same events at the
//! same times give the same commands. Every map is ordered, so is the output.
//!
//! What the line does not know is abstracted: a rider's client having a location loaded is `Event::Ready`, whoever works
//! it out (the replicant's per-location batch answers); a location is a `Location`, whatever the wiring maps it to; a
//! gateway's place, its door and limbo node are the data's (`super::Gateway`), named here by `GatewayRef`.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use super::{Manager, System};

/// A player, by its entity id.
pub type PlayerId = u64;

/// A carriage of the line: its slot in the fleet, `0 .. fleet`. Transit's `tcAssignedId`, Transport's `carriage_N`. A
/// slot is reused once its carriage is removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CarriageId(pub usize);

/// Where a carriage can stop: a destination of the manager (its `destination` index in the data), or the instance of
/// one player's hangar served by the manager.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stop {
    Destination(usize),
    Hangar(PlayerId),
}

/// What a client loads for a stop, and reports as loaded. A destination's is the key `DestinationSpec::location`; a
/// hangar's is its owner's instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Location {
    Persistent(u32),
    Hangar(PlayerId),
}

/// A gateway of a stop. For a destination, `gateway` is the manager's gateway index (`super::Gateway::index`); for a
/// hangar, the index in its hangar's gateways (`super::Tables::stop_gateways`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GatewayRef {
    pub stop: Stop,
    pub gateway: usize,
}

/// Every timing of the rides. `Default` gives the documented defaults; `with_manager` takes the manager's own where its
/// data has them.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// The doors of a stop open no earlier than this after it was picked: the official median pick-to-exit time.
    pub min_ride: Duration,
    /// Nor earlier than this after the carriage left its previous stop, so a stop picked long ago still takes a ride:
    /// `min_ride` less the doors' wait and closing, so a stop picked on boarding opens `min_ride` after the pick.
    pub min_leg: Duration,
    /// A free carriage called to a gateway arrives this long after it is sent: the official call-to-present median.
    pub summon: Duration,
    /// The doors stay open this long after the last rider moved (boarded, got out, asked for the doors):
    /// `carriageWaitTime`.
    pub door_wait: Duration,
    /// How long the doors take to close, before the carriage leaves: `carriageDoorTimeout`.
    pub door_close: Duration,
    /// What a hold request adds to the doors' wait: Transport's `holdDoorsDelay`.
    pub hold_doors: Duration,
    /// `transit.slow_rider_grace`: once the ride time has passed, how long ready riders wait for one who is not before
    /// the carriage splits.
    pub slow_rider_grace: Duration,
    /// `transit.resend_after`: a location asked of a rider and not reported loaded after this is asked again.
    pub resend_after: Duration,
    /// `transit.idle_removal`: a carriage with no riders and nothing to do is removed after this.
    pub idle_removal: Duration,
    /// `transit.preload_limit`: how many queued stops after the next one each rider loads ahead. `None`: all of them.
    pub preload_limit: Option<usize>,
    /// Whether a carriage splits from a slow rider. Off is the design's fallback: the ready riders wait, doors closed.
    pub split: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_ride: Duration::from_secs(16),
            min_leg: Duration::from_millis(8500),
            summon: Duration::from_millis(300),
            door_wait: Duration::from_secs(5),
            door_close: Duration::from_millis(2500),
            hold_doors: Duration::from_secs(5),
            slow_rider_grace: Duration::from_secs(20),
            resend_after: Duration::from_secs(60),
            idle_removal: Duration::from_secs(120),
            preload_limit: None,
            split: true,
        }
    }
}

impl Settings {
    /// The doors' timings of a manager's data (`carriageWaitTime`, `carriageDoorTimeout`) over these.
    pub fn with_manager(mut self, manager: &Manager) -> Self {
        let seconds = |value: f64, fallback: Duration| if value.is_finite() && value >= 0.0 { Duration::from_secs_f64(value) } else { fallback };
        self.door_wait = seconds(manager.wait_time, self.door_wait);
        self.door_close = seconds(manager.door_timeout, self.door_close);
        self
    }
}

/// A destination of the manager: its enabled gateways and the location a client loads for it.
#[derive(Clone, Debug, PartialEq)]
pub struct DestinationSpec {
    /// The manager's gateway indices.
    pub gateways: Vec<usize>,
    /// The key of its `Location::Persistent`.
    pub location: u32,
}

/// What the line serves.
#[derive(Clone, Debug, PartialEq)]
pub struct LineSpec {
    /// Transit creates carriage entities when needed. Transport keeps the state of every slot in its manager and only
    /// creates and removes the entities: `Command::Create` and `Command::Remove` are about the entity, and a slot without
    /// one stays `Idle`, streamed out.
    pub system: System,
    /// At most this many carriages at once: the data's fleet.
    pub fleet: usize,
    /// By destination index; a destination with no gateway cannot be picked.
    pub destinations: Vec<DestinationSpec>,
    /// The destination in the persistent location where a rider who leaves the game aboard is saved when their trip's
    /// destination is a hangar (the spaceport's lobby, for a hangar elevator).
    pub persistent: usize,
}

impl LineSpec {
    /// A Transit manager's line: its enabled gateways by destination, each destination's location keyed by its index,
    /// and the persistent stop its first important destination.
    pub fn from_manager(manager: &Manager) -> Self {
        let count = manager.gateways.iter().map(|gateway| gateway.destination + 1).max().unwrap_or(0);
        let mut destinations: Vec<DestinationSpec> =
            (0..count).map(|index| DestinationSpec { gateways: Vec::new(), location: index as u32 }).collect();
        for gateway in manager.gateways.iter().filter(|gateway| gateway.enabled) {
            destinations[gateway.destination].gateways.push(gateway.index);
        }
        let enabled = || manager.gateways.iter().filter(|gateway| gateway.enabled);
        let persistent = enabled().find(|gateway| gateway.important).or_else(|| enabled().next()).map_or(0, |gateway| gateway.destination);
        Self { system: System::Transit, fleet: manager.carriages, destinations, persistent }
    }
}

/// What happens to the line, as the DGS learns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A player pressed a gateway's call panel.
    Call { player: PlayerId, gateway: GatewayRef },
    /// The server put a player in a carriage's zone. Only a carriage at a gateway can be boarded.
    Board { player: PlayerId, carriage: CarriageId },
    /// The server took a player out of a carriage's zone.
    Exit { player: PlayerId, carriage: CarriageId },
    /// A rider pressed a stop on the carriage's panel. It replaces their previous pick.
    Pick { player: PlayerId, stop: Stop },
    /// A rider asked for the doors (`RequestOpenDoors`).
    OpenDoors { player: PlayerId },
    /// A rider asked to hold the doors (`RequestHoldDoors`).
    HoldDoors { player: PlayerId },
    /// A rider's client has every batch of a location spawned.
    Ready { player: PlayerId, location: Location },
    /// A player's connection ended.
    Disconnected { player: PlayerId },
    /// An owner's hangar instance exists, with this many gateways.
    HangarOpened { owner: PlayerId, gateways: usize },
    /// An owner's hangar instance is gone.
    HangarClosed { owner: PlayerId },
}

/// The state of a carriage, as the client is told it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CarriageState {
    /// At a gateway (or in limbo), doors closed, no riders and nothing to do.
    Idle,
    /// Through limbo to its target, before the ride time is over.
    Travelling,
    /// Held in limbo, the ride time over (Transit's `tcWaitInLimbo`): for its riders to be ready, for a gateway, or for
    /// a pick.
    WaitingInLimbo,
    /// At a gateway, with riders or doors open.
    Present,
}

/// Where a new carriage's entity is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Gateway(GatewayRef),
    /// In limbo below this stop, for a split.
    Limbo(Stop),
}

/// What the DGS applies, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Create the carriage's entity (the manager's carriage class) and bind it.
    Create { carriage: CarriageId, place: Place },
    /// Remove the carriage's entity. It never has riders.
    Remove { carriage: CarriageId },
    /// The carriage's state from `since`: at a gateway (`at`), or through limbo to `target`.
    State { carriage: CarriageId, state: CarriageState, at: Option<GatewayRef>, target: Option<GatewayRef>, since: Instant },
    OpenDoors { carriage: CarriageId, gateway: GatewayRef },
    CloseDoors { carriage: CarriageId, gateway: GatewayRef },
    /// Teleport a rider from one carriage into another at the same place relative to the cab (the split).
    MoveRider { player: PlayerId, from: CarriageId, to: CarriageId },
    /// Start sending a location to a rider's client; `resend` when it was asked before and never reported loaded.
    Load { player: PlayerId, location: Location, resend: bool },
    /// The rider no longer needs a location this line asked for.
    Unload { player: PlayerId, location: Location },
    /// Save a player whose connection ended aboard at this stop, outside this gateway's door.
    Save { player: PlayerId, stop: Stop, gateway: Option<GatewayRef> },
}

/// A carriage as the line holds it, for the DGS and tests.
#[derive(Clone, Debug, PartialEq)]
pub struct CarriageInfo {
    pub id: CarriageId,
    pub state: CarriageState,
    pub at: Option<GatewayRef>,
    pub target: Option<GatewayRef>,
    pub doors_open: bool,
    pub riders: Vec<PlayerId>,
    /// Its stops, in order.
    pub queue: Vec<Stop>,
    /// The gateway it was sent to for a call.
    pub call: Option<GatewayRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Doors {
    Open,
    /// Closing since then.
    Closing(Instant),
    Closed,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    /// No riders, no call, no queue. `at` is `None` in limbo.
    Idle { at: Option<GatewayRef>, since: Instant },
    Present { at: GatewayRef, doors: Doors, last_movement: Instant, held_until: Option<Instant> },
    /// In limbo since `leg_start`, aiming at `target` (shared with others until it arrives, see `arrive`).
    Travelling { from: Option<GatewayRef>, leg_start: Instant, target: Option<GatewayRef>, waiting: bool },
}

#[derive(Clone, Debug)]
struct Queued {
    stop: Stop,
    /// The riders aboard who wait for it. Never empty: an entry whose pickers are gone is dropped.
    pickers: BTreeSet<PlayerId>,
    picked_at: Instant,
}

type Described = (CarriageState, Option<GatewayRef>, Option<GatewayRef>);

#[derive(Clone, Debug)]
struct Carriage {
    phase: Phase,
    riders: BTreeSet<PlayerId>,
    queue: Vec<Queued>,
    call: Option<GatewayRef>,
    slow_since: Option<Instant>,
    reported: Option<Described>,
    state_since: Instant,
}

impl Carriage {
    fn new(phase: Phase, now: Instant) -> Self {
        Self { phase, riders: BTreeSet::new(), queue: Vec::new(), call: None, slow_since: None, reported: None, state_since: now }
    }

    fn describe(&self) -> Described {
        match self.phase {
            Phase::Idle { at, .. } => (CarriageState::Idle, at, None),
            Phase::Present { at, .. } => (CarriageState::Present, Some(at), None),
            Phase::Travelling { target, waiting: false, .. } => (CarriageState::Travelling, None, target),
            Phase::Travelling { target, waiting: true, .. } => (CarriageState::WaitingInLimbo, None, target),
        }
    }

    /// The gateway it stands at.
    fn at(&self) -> Option<GatewayRef> {
        match self.phase {
            Phase::Idle { at, .. } => at,
            Phase::Present { at, .. } => Some(at),
            Phase::Travelling { .. } => None,
        }
    }

    fn target(&self) -> Option<GatewayRef> {
        match self.phase {
            Phase::Travelling { target, .. } => target,
            _ => None,
        }
    }

    fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle { .. })
    }

    /// Takes a rider out of every entry's pickers, and drops the entries nobody waits for.
    fn unpick(&mut self, player: PlayerId) {
        for entry in &mut self.queue {
            entry.pickers.remove(&player);
        }
        self.queue.retain(|entry| !entry.pickers.is_empty());
    }
}

#[derive(Clone, Debug)]
struct Rider {
    carriage: CarriageId,
    pick: Option<Stop>,
    /// The last stop they were at, where they boarded or where the doors last opened for them: never unloaded while
    /// they are aboard, so they can always go back.
    origin: Location,
    ready: BTreeSet<Location>,
    /// The locations this line asked for, and when last.
    wanted: BTreeMap<Location, Instant>,
}

#[derive(Clone, Debug)]
struct Call {
    gateway: GatewayRef,
    callers: BTreeSet<PlayerId>,
}

/// The carriages of one manager and everyone riding them.
#[derive(Clone, Debug)]
pub struct Line {
    spec: LineSpec,
    settings: Settings,
    /// The open hangar instances: owner and gateway count.
    hangars: BTreeMap<PlayerId, usize>,
    carriages: BTreeMap<CarriageId, Carriage>,
    riders: BTreeMap<PlayerId, Rider>,
    calls: Vec<Call>,
}

/// A carriage settles within a few transitions per step; this bounds the loop all the same.
const MAX_TRANSITIONS: usize = 16;

impl Line {
    pub fn new(spec: LineSpec, settings: Settings) -> Self {
        Self { spec, settings, hangars: BTreeMap::new(), carriages: BTreeMap::new(), riders: BTreeMap::new(), calls: Vec::new() }
    }

    pub fn spec(&self) -> &LineSpec {
        &self.spec
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// One event at `now`, then everything due by `now`.
    pub fn handle(&mut self, now: Instant, event: Event) -> Vec<Command> {
        let mut out = Vec::new();
        match event {
            Event::Call { player, gateway } => self.call(player, gateway),
            Event::Board { player, carriage } => self.board(player, carriage, now, &mut out),
            Event::Exit { player, carriage } => {
                if self.riders.get(&player).is_some_and(|rider| rider.carriage == carriage) {
                    self.leave(player, now, true, &mut out);
                }
            }
            Event::Pick { player, stop } => self.pick(player, stop, now, &mut out),
            Event::OpenDoors { player } => self.doors_request(player, None, now, &mut out),
            Event::HoldDoors { player } => {
                let until = now + self.settings.door_wait + self.settings.hold_doors;
                self.doors_request(player, Some(until), now, &mut out);
            }
            Event::Ready { player, location } => {
                if let Some(rider) = self.riders.get_mut(&player) {
                    rider.ready.insert(location);
                }
            }
            Event::Disconnected { player } => self.disconnected(player, now, &mut out),
            Event::HangarOpened { owner, gateways } => {
                if gateways > 0 {
                    self.hangars.insert(owner, gateways);
                }
            }
            Event::HangarClosed { owner } => self.hangar_closed(owner, now, &mut out),
        }
        self.advance(now, &mut out);
        out
    }

    /// Everything due by `now`.
    pub fn tick(&mut self, now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        self.advance(now, &mut out);
        out
    }

    pub fn carriages(&self) -> Vec<CarriageInfo> {
        self.carriages.keys().filter_map(|id| self.carriage(*id)).collect()
    }

    pub fn carriage(&self, id: CarriageId) -> Option<CarriageInfo> {
        let carriage = self.carriages.get(&id)?;
        let (state, at, target) = carriage.describe();
        Some(CarriageInfo {
            id,
            state,
            at,
            target,
            doors_open: matches!(carriage.phase, Phase::Present { doors: Doors::Open, .. }),
            riders: carriage.riders.iter().copied().collect(),
            queue: carriage.queue.iter().map(|entry| entry.stop).collect(),
            call: carriage.call,
        })
    }

    /// The carriage a player rides and their pending pick.
    pub fn rider(&self, player: PlayerId) -> Option<(CarriageId, Option<Stop>)> {
        self.riders.get(&player).map(|rider| (rider.carriage, rider.pick))
    }

    /// The gateways called and not yet opened at.
    pub fn calls(&self) -> Vec<GatewayRef> {
        self.calls.iter().map(|call| call.gateway).collect()
    }

    pub fn location(&self, stop: Stop) -> Location {
        match stop {
            Stop::Destination(index) => Location::Persistent(self.spec.destinations.get(index).map_or(index as u32, |destination| destination.location)),
            Stop::Hangar(owner) => Location::Hangar(owner),
        }
    }

    /// A stop's gateways; none for a stop that does not exist (now).
    pub fn gateways(&self, stop: Stop) -> Vec<GatewayRef> {
        match stop {
            Stop::Destination(index) => self.spec.destinations.get(index).map_or_else(Vec::new, |destination| {
                destination.gateways.iter().map(|&gateway| GatewayRef { stop, gateway }).collect()
            }),
            Stop::Hangar(owner) => (0..self.hangars.get(&owner).copied().unwrap_or(0)).map(|gateway| GatewayRef { stop, gateway }).collect(),
        }
    }

    fn exists(&self, gateway: GatewayRef) -> bool {
        self.gateways(gateway.stop).contains(&gateway)
    }

    // Events.

    fn call(&mut self, player: PlayerId, gateway: GatewayRef) {
        if !self.exists(gateway) {
            return;
        }
        match self.calls.iter_mut().find(|call| call.gateway == gateway) {
            Some(call) => {
                call.callers.insert(player);
            }
            None => self.calls.push(Call { gateway, callers: BTreeSet::from([player]) }),
        }
    }

    fn board(&mut self, player: PlayerId, id: CarriageId, now: Instant, out: &mut Vec<Command>) {
        let Some(at) = self.carriages.get(&id).and_then(Carriage::at) else { return };
        match self.riders.get(&player) {
            Some(rider) if rider.carriage == id => return,
            Some(_) => self.leave(player, now, true, out),
            None => {}
        }
        let location = self.location(at.stop);
        self.riders.insert(
            player,
            Rider { carriage: id, pick: None, origin: location, ready: BTreeSet::from([location]), wanted: BTreeMap::new() },
        );
        let carriage = self.carriages.get_mut(&id).expect("checked above");
        carriage.riders.insert(player);
        match &mut carriage.phase {
            Phase::Present { last_movement, .. } => *last_movement = now,
            phase => *phase = Phase::Present { at, doors: Doors::Closed, last_movement: now, held_until: None },
        }
    }

    /// Takes a rider out of their carriage. `unload` gives back what the line asked them to load, but where they stand.
    fn leave(&mut self, player: PlayerId, now: Instant, unload: bool, out: &mut Vec<Command>) {
        let Some(rider) = self.riders.remove(&player) else { return };
        let Some(carriage) = self.carriages.get_mut(&rider.carriage) else { return };
        carriage.riders.remove(&player);
        carriage.unpick(player);
        if carriage.riders.is_empty() {
            carriage.queue.clear();
            carriage.slow_since = None;
        }
        let here = carriage.at().map(|gateway| gateway.stop);
        if let Phase::Present { last_movement, .. } = &mut carriage.phase {
            *last_movement = now;
        }
        if unload {
            let here = here.map(|stop| self.location(stop));
            for location in rider.wanted.keys().filter(|location| Some(**location) != here) {
                out.push(Command::Unload { player, location: *location });
            }
        }
    }

    fn pick(&mut self, player: PlayerId, stop: Stop, now: Instant, out: &mut Vec<Command>) {
        if self.gateways(stop).is_empty() {
            return;
        }
        let Some(rider) = self.riders.get_mut(&player) else { return };
        let id = rider.carriage;
        let carriage = self.carriages.get_mut(&id).expect("a rider's carriage exists");
        carriage.unpick(player);
        if let Phase::Present { at, .. } = carriage.phase {
            if at.stop == stop {
                // Already there: the doors open again rather than a ride back.
                rider.pick = None;
                self.reopen(id, now, out);
                return;
            }
        }
        rider.pick = Some(stop);
        match carriage.queue.iter_mut().find(|entry| entry.stop == stop) {
            Some(entry) => {
                entry.pickers.insert(player);
            }
            None => carriage.queue.push(Queued { stop, pickers: BTreeSet::from([player]), picked_at: now }),
        }
    }

    fn doors_request(&mut self, player: PlayerId, hold: Option<Instant>, now: Instant, out: &mut Vec<Command>) {
        let Some(id) = self.riders.get(&player).map(|rider| rider.carriage) else { return };
        let Some(carriage) = self.carriages.get_mut(&id) else { return };
        if let Phase::Present { held_until, .. } = &mut carriage.phase {
            if hold.is_some() {
                *held_until = hold;
            }
            self.reopen(id, now, out);
        }
    }

    fn disconnected(&mut self, player: PlayerId, now: Instant, out: &mut Vec<Command>) {
        for call in &mut self.calls {
            call.callers.remove(&player);
        }
        let deserted: Vec<GatewayRef> = self.calls.iter().filter(|call| call.callers.is_empty()).map(|call| call.gateway).collect();
        self.calls.retain(|call| !call.callers.is_empty());
        for carriage in self.carriages.values_mut() {
            if carriage.call.is_some_and(|gateway| deserted.contains(&gateway)) {
                carriage.call = None;
            }
        }
        if self.riders.contains_key(&player) {
            let (stop, gateway) = self.save_place(player);
            out.push(Command::Save { player, stop, gateway });
            self.leave(player, now, false, out);
        }
    }

    /// Where a rider who leaves the game aboard is saved: their trip's destination (their pick, else the stop the
    /// doors are open at, else the carriage's next stop, else the stop it came from), or the persistent stop when that
    /// is a hangar or there is none.
    fn save_place(&self, player: PlayerId) -> (Stop, Option<GatewayRef>) {
        let rider = &self.riders[&player];
        let carriage = &self.carriages[&rider.carriage];
        let from = match carriage.phase {
            Phase::Travelling { from, .. } => from.map(|gateway| gateway.stop),
            _ => None,
        };
        let destination = rider.pick.or(carriage.at().map(|gateway| gateway.stop)).or(carriage.queue.first().map(|entry| entry.stop)).or(from);
        let stop = match destination {
            Some(stop @ Stop::Destination(_)) => stop,
            _ => Stop::Destination(self.spec.persistent),
        };
        let gateway =
            [carriage.at(), carriage.target()].into_iter().flatten().find(|gateway| gateway.stop == stop).or_else(|| self.gateways(stop).first().copied());
        (stop, gateway)
    }

    fn hangar_closed(&mut self, owner: PlayerId, now: Instant, out: &mut Vec<Command>) {
        if self.hangars.remove(&owner).is_none() {
            return;
        }
        let stop = Stop::Hangar(owner);
        self.calls.retain(|call| call.gateway.stop != stop);
        for rider in self.riders.values_mut() {
            if rider.pick == Some(stop) {
                rider.pick = None;
            }
        }
        let lobby = Stop::Destination(self.spec.persistent);
        let mut gone = Vec::new();
        for (id, carriage) in &mut self.carriages {
            carriage.queue.retain(|entry| entry.stop != stop);
            if carriage.call.is_some_and(|gateway| gateway.stop == stop) {
                carriage.call = None;
            }
            let stranded = match &mut carriage.phase {
                Phase::Travelling { target, .. } if target.is_some_and(|gateway| gateway.stop == stop) => {
                    *target = None;
                    true
                }
                Phase::Present { at, .. } => at.stop == stop,
                Phase::Idle { at: Some(gateway), .. } if gateway.stop == stop => {
                    gone.push(*id);
                    false
                }
                _ => false,
            };
            // Riders at or bound for a hangar that is gone are taken to the persistent stop (the lobby) first, the
            // doors there gated like any other stop's.
            if stranded && !carriage.riders.is_empty() && carriage.queue.first().map(|entry| entry.stop) != Some(lobby) {
                carriage.queue.retain(|entry| entry.stop != lobby);
                carriage.queue.insert(0, Queued { stop: lobby, pickers: carriage.riders.clone(), picked_at: now });
            }
        }
        for id in gone {
            self.carriages.remove(&id);
            out.push(Command::Remove { carriage: id });
        }
    }

    // The machine.

    fn advance(&mut self, now: Instant, out: &mut Vec<Command>) {
        let ids: Vec<CarriageId> = self.carriages.keys().copied().collect();
        for id in ids {
            self.step(id, now, out);
        }
        self.dispatch(now, out);
        self.remove_idle(now, out);
        self.update_loads(now, out);
    }

    fn step(&mut self, id: CarriageId, now: Instant, out: &mut Vec<Command>) {
        for _ in 0..MAX_TRANSITIONS {
            let Some(carriage) = self.carriages.get(&id) else { return };
            let changed = match carriage.phase {
                Phase::Idle { .. } => false,
                Phase::Present { .. } => self.step_present(id, now, out),
                Phase::Travelling { .. } => self.step_travelling(id, now, out),
            };
            self.report(id, out);
            if !changed {
                return;
            }
        }
    }

    /// Emits the carriage's state when it changed since last told.
    fn report(&mut self, id: CarriageId, out: &mut Vec<Command>) {
        let Some(carriage) = self.carriages.get_mut(&id) else { return };
        let described = carriage.describe();
        if carriage.reported == Some(described) {
            return;
        }
        carriage.reported = Some(described);
        let (state, at, target) = described;
        out.push(Command::State { carriage: id, state, at, target, since: carriage.state_since });
    }

    /// Sets a carriage's phase, restarting its state time when the state changes.
    fn set_phase(&mut self, id: CarriageId, phase: Phase, now: Instant) {
        let carriage = self.carriages.get_mut(&id).expect("a carriage being moved exists");
        let before = carriage.describe().0;
        carriage.phase = phase;
        if carriage.describe().0 != before || carriage.reported.is_none() {
            carriage.state_since = now;
        }
    }

    fn step_present(&mut self, id: CarriageId, now: Instant, out: &mut Vec<Command>) -> bool {
        let settings = &self.settings;
        let carriage = &self.carriages[&id];
        let Phase::Present { at, doors, last_movement, held_until } = carriage.phase else { return false };
        let release = (last_movement + settings.door_wait).max(held_until.unwrap_or(last_movement));
        let leaving = !carriage.queue.is_empty();
        match doors {
            Doors::Open if now >= release => {
                self.carriages.get_mut(&id).unwrap().phase = Phase::Present { at, doors: Doors::Closing(release), last_movement, held_until };
                out.push(Command::CloseDoors { carriage: id, gateway: at });
                true
            }
            Doors::Closing(since) if now >= since + settings.door_close => {
                let when = since + settings.door_close;
                if leaving {
                    self.depart(id, at, when);
                } else if carriage.riders.is_empty() {
                    self.set_phase(id, Phase::Idle { at: Some(at), since: when }, when);
                } else {
                    self.carriages.get_mut(&id).unwrap().phase = Phase::Present { at, doors: Doors::Closed, last_movement, held_until };
                }
                true
            }
            Doors::Closed if leaving && now >= release => {
                self.depart(id, at, release);
                true
            }
            Doors::Closed if carriage.riders.is_empty() => {
                // Its riders left with the doors closed.
                self.set_phase(id, Phase::Idle { at: Some(at), since: now }, now);
                true
            }
            _ => false,
        }
    }

    fn depart(&mut self, id: CarriageId, at: GatewayRef, when: Instant) {
        self.set_phase(id, Phase::Travelling { from: Some(at), leg_start: when, target: None, waiting: false }, when);
        self.carriages.get_mut(&id).unwrap().slow_since = None;
    }

    fn step_travelling(&mut self, id: CarriageId, now: Instant, out: &mut Vec<Command>) -> bool {
        let settings = self.settings.clone();
        let carriage = &self.carriages[&id];
        let Phase::Travelling { from, leg_start, target, waiting } = carriage.phase else { return false };
        if carriage.riders.is_empty() && carriage.call.is_none() {
            let carriage = self.carriages.get_mut(&id).unwrap();
            carriage.queue.clear();
            carriage.slow_since = None;
            self.set_phase(id, Phase::Idle { at: None, since: now }, now);
            return true;
        }
        let call = carriage.call;
        let head = match (call, carriage.queue.first()) {
            (Some(gateway), _) => Some((gateway.stop, leg_start + settings.summon)),
            (None, Some(entry)) => Some((entry.stop, (entry.picked_at + settings.min_ride).max(leg_start + settings.min_leg))),
            (None, None) => None,
        };
        let Some((stop, arrival)) = head else {
            // Riders and nothing picked: they wait in limbo with the panel.
            let changed = target.is_some() || !waiting;
            self.carriages.get_mut(&id).unwrap().phase = Phase::Travelling { from, leg_start, target: None, waiting: true };
            self.set_phase_time_if_changed(id, now);
            return changed;
        };
        let mut new_target = target;
        if let Some(gateway) = call {
            new_target = Some(gateway);
        } else if target.is_none_or(|gateway| gateway.stop != stop) {
            new_target = self.aim(stop, id);
        }
        let mut changed = new_target != target;
        self.carriages.get_mut(&id).unwrap().phase = Phase::Travelling { from, leg_start, target: new_target, waiting };
        if now < arrival {
            return changed;
        }
        if call.is_some() {
            if let Some(gateway) = new_target.filter(|gateway| !self.stood_at(*gateway, id)) {
                self.open(id, gateway, now, out);
                return true;
            }
        } else {
            let location = self.location(stop);
            let carriage = &self.carriages[&id];
            let (ready, slow): (Vec<PlayerId>, Vec<PlayerId>) =
                carriage.riders.iter().partition(|player| self.riders[*player].ready.contains(&location));
            if slow.is_empty() {
                self.carriages.get_mut(&id).unwrap().slow_since = None;
                if let Some(gateway) = self.arrival_gateway(stop, new_target, id, now, out) {
                    self.open(id, gateway, now, out);
                    return true;
                }
            } else if ready.is_empty() {
                self.carriages.get_mut(&id).unwrap().slow_since = None;
            } else {
                let since = *self.carriages.get_mut(&id).unwrap().slow_since.get_or_insert(now);
                if settings.split && now >= since + settings.slow_rider_grace && self.split(id, stop, &slow, now, out) {
                    return true;
                }
            }
        }
        if !waiting {
            if let Phase::Travelling { waiting, .. } = &mut self.carriages.get_mut(&id).unwrap().phase {
                *waiting = true;
            }
            self.set_phase_time_if_changed(id, now);
            changed = true;
        }
        changed
    }

    /// Restarts the state time when the described state no longer matches the last one told.
    fn set_phase_time_if_changed(&mut self, id: CarriageId, now: Instant) {
        let carriage = self.carriages.get_mut(&id).unwrap();
        if carriage.reported.map(|reported| reported.0) != Some(carriage.describe().0) {
            carriage.state_since = now;
        }
    }

    /// Whether another carriage stands at a gateway.
    fn stood_at(&self, gateway: GatewayRef, except: CarriageId) -> bool {
        self.carriages.iter().any(|(id, carriage)| *id != except && carriage.at() == Some(gateway))
    }

    fn aimed_at(&self, gateway: GatewayRef, except: CarriageId) -> bool {
        self.carriages.iter().any(|(id, carriage)| *id != except && (carriage.target() == Some(gateway) || carriage.call == Some(gateway)))
    }

    /// The gateway to wait below: one nobody stands at or aims at, else one nobody stands at, else the first. A target is
    /// not a reservation: whoever is ready first takes the gateway (`arrival_gateway`).
    fn aim(&self, stop: Stop, id: CarriageId) -> Option<GatewayRef> {
        let gateways = self.gateways(stop);
        gateways
            .iter()
            .find(|gateway| !self.stood_at(**gateway, id) && !self.aimed_at(**gateway, id))
            .or_else(|| gateways.iter().find(|gateway| !self.stood_at(**gateway, id)))
            .or(gateways.first())
            .copied()
    }

    /// A gateway of `stop` to open at now: the target if free, another free one, or one freed by removing an idle
    /// carriage standing there or sending a carriage whose riders have not picked anything, doors closed, into limbo.
    fn arrival_gateway(&mut self, stop: Stop, target: Option<GatewayRef>, id: CarriageId, now: Instant, out: &mut Vec<Command>) -> Option<GatewayRef> {
        let gateways = self.gateways(stop);
        if let Some(gateway) = target.filter(|gateway| gateways.contains(gateway) && !self.stood_at(*gateway, id)) {
            return Some(gateway);
        }
        if let Some(gateway) = gateways.iter().find(|gateway| !self.stood_at(**gateway, id)) {
            return Some(*gateway);
        }
        for gateway in gateways {
            let Some((&other, carriage)) = self.carriages.iter().find(|(other, carriage)| **other != id && carriage.at() == Some(gateway)) else {
                continue;
            };
            match carriage.phase {
                Phase::Idle { .. } => {
                    self.carriages.remove(&other);
                    out.push(Command::Remove { carriage: other });
                    return Some(gateway);
                }
                Phase::Present { doors: Doors::Closed, .. } if carriage.queue.is_empty() && carriage.call.is_none() => {
                    self.set_phase(other, Phase::Travelling { from: Some(gateway), leg_start: now, target: None, waiting: true }, now);
                    self.report(other, out);
                    return Some(gateway);
                }
                _ => {}
            }
        }
        None
    }

    /// Opens a carriage's doors at a gateway on arrival. Every rider aboard is ready for its stop (rule 3).
    fn open(&mut self, id: CarriageId, gateway: GatewayRef, now: Instant, out: &mut Vec<Command>) {
        assert!(self.all_ready(id, gateway.stop), "doors open only where every rider is ready");
        self.set_phase(id, Phase::Present { at: gateway, doors: Doors::Open, last_movement: now, held_until: None }, now);
        let carriage = self.carriages.get_mut(&id).unwrap();
        if carriage.call == Some(gateway) {
            carriage.call = None;
        }
        carriage.queue.retain(|entry| entry.stop != gateway.stop);
        carriage.slow_since = None;
        let riders = carriage.riders.clone();
        let location = self.location(gateway.stop);
        for player in riders {
            let rider = self.riders.get_mut(&player).unwrap();
            rider.origin = location;
            if rider.pick == Some(gateway.stop) {
                rider.pick = None;
            }
        }
        self.calls_met(gateway);
        self.report(id, out);
        out.push(Command::OpenDoors { carriage: id, gateway });
    }

    /// Opens the doors of a carriage present at a gateway again, or keeps them open.
    fn reopen(&mut self, id: CarriageId, now: Instant, out: &mut Vec<Command>) {
        let Some(Phase::Present { at, doors, held_until, .. }) = self.carriages.get(&id).map(|carriage| carriage.phase) else { return };
        if !self.all_ready(id, at.stop) {
            return;
        }
        self.carriages.get_mut(&id).unwrap().phase = Phase::Present { at, doors: Doors::Open, last_movement: now, held_until };
        if doors != Doors::Open {
            out.push(Command::OpenDoors { carriage: id, gateway: at });
        }
        self.calls_met(at);
    }

    /// A carriage opened at a gateway: its calls are met, and a carriage sent for them is free.
    fn calls_met(&mut self, gateway: GatewayRef) {
        self.calls.retain(|call| call.gateway != gateway);
        for carriage in self.carriages.values_mut() {
            if carriage.call == Some(gateway) && !matches!(carriage.phase, Phase::Present { .. }) {
                carriage.call = None;
            }
        }
    }

    fn all_ready(&self, id: CarriageId, stop: Stop) -> bool {
        let location = self.location(stop);
        self.carriages[&id].riders.iter().all(|player| self.riders[player].ready.contains(&location))
    }

    /// Moves the slow riders into a free carriage, or a new one, in limbo below the same stop, with a copy of the queue
    /// for them. `false` when the fleet has no carriage for it.
    fn split(&mut self, id: CarriageId, stop: Stop, slow: &[PlayerId], now: Instant, out: &mut Vec<Command>) -> bool {
        let Some((other, reused)) = self.free_carriage(id).map(|other| (other, true)).or_else(|| self.new_id().map(|other| (other, false))) else {
            return false;
        };
        let slow: BTreeSet<PlayerId> = slow.iter().copied().collect();
        let carriage = self.carriages.get_mut(&id).unwrap();
        let Phase::Travelling { from, leg_start, .. } = carriage.phase else { return false };
        let ready: BTreeSet<PlayerId> = carriage.riders.difference(&slow).copied().collect();
        let mut copy = Vec::new();
        for entry in &mut carriage.queue {
            if entry.stop == stop {
                // Both carriages stop here for all of their riders.
                copy.push(Queued { stop, pickers: slow.clone(), picked_at: entry.picked_at });
                entry.pickers = ready.clone();
            } else {
                let theirs: BTreeSet<PlayerId> = entry.pickers.intersection(&slow).copied().collect();
                if !theirs.is_empty() {
                    copy.push(Queued { stop: entry.stop, pickers: theirs, picked_at: entry.picked_at });
                }
                entry.pickers.retain(|player| !slow.contains(player));
            }
        }
        carriage.queue.retain(|entry| !entry.pickers.is_empty());
        carriage.riders = ready;
        carriage.slow_since = None;
        let phase = Phase::Travelling { from, leg_start, target: None, waiting: true };
        if reused {
            self.set_phase(other, phase, now);
        } else {
            self.carriages.insert(other, Carriage::new(phase, now));
            out.push(Command::Create { carriage: other, place: Place::Limbo(stop) });
        }
        let new = self.carriages.get_mut(&other).unwrap();
        new.riders = slow.clone();
        new.queue = copy;
        let target = self.aim(stop, other);
        if let Phase::Travelling { target: aimed, .. } = &mut self.carriages.get_mut(&other).unwrap().phase {
            *aimed = target;
        }
        self.report(other, out);
        for player in slow {
            self.riders.get_mut(&player).unwrap().carriage = other;
            out.push(Command::MoveRider { player, from: id, to: other });
        }
        true
    }

    /// An idle carriage, the lowest slot first.
    fn free_carriage(&self, except: CarriageId) -> Option<CarriageId> {
        self.carriages.iter().find(|(id, carriage)| **id != except && carriage.idle()).map(|(id, _)| *id)
    }

    /// The lowest free slot, while the fleet is not all out.
    fn new_id(&self) -> Option<CarriageId> {
        (0..self.spec.fleet).map(CarriageId).find(|id| !self.carriages.contains_key(id))
    }

    /// Serves the calls: a carriage at the gateway opens; otherwise, unless one is on its way, an idle one is sent, or a
    /// new one is created there while the fleet allows.
    fn dispatch(&mut self, now: Instant, out: &mut Vec<Command>) {
        let pending: Vec<GatewayRef> = self.calls.iter().map(|call| call.gateway).collect();
        for gateway in pending {
            if !self.calls.iter().any(|call| call.gateway == gateway) {
                continue;
            }
            if let Some((&id, carriage)) = self.carriages.iter().find(|(_, carriage)| carriage.at() == Some(gateway)) {
                if carriage.idle() {
                    self.open(id, gateway, now, out);
                } else {
                    self.reopen(id, now, out);
                }
                self.report(id, out);
                continue;
            }
            if self.carriages.values().any(|carriage| carriage.call == Some(gateway) || carriage.target() == Some(gateway)) {
                continue;
            }
            if let Some(id) = self.free_carriage(CarriageId(usize::MAX)) {
                let from = self.carriages[&id].at();
                self.carriages.get_mut(&id).unwrap().call = Some(gateway);
                self.set_phase(id, Phase::Travelling { from, leg_start: now, target: Some(gateway), waiting: false }, now);
                self.report(id, out);
            } else if let Some(id) = self.new_id() {
                self.carriages.insert(id, Carriage::new(Phase::Idle { at: Some(gateway), since: now }, now));
                out.push(Command::Create { carriage: id, place: Place::Gateway(gateway) });
                self.open(id, gateway, now, out);
                self.report(id, out);
            }
        }
    }

    /// Rule 2: a carriage idle for `idle_removal` goes. Idle carriages have no riders.
    fn remove_idle(&mut self, now: Instant, out: &mut Vec<Command>) {
        let expired: Vec<CarriageId> = self
            .carriages
            .iter()
            .filter(|(_, carriage)| matches!(carriage.phase, Phase::Idle { since, .. } if now >= since + self.settings.idle_removal))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            debug_assert!(self.carriages[&id].riders.is_empty());
            self.carriages.remove(&id);
            out.push(Command::Remove { carriage: id });
        }
    }

    /// Every rider loads every queued stop of their carriage (up to the preload limit past the next one), stops loading
    /// those no longer queued, and is asked again for one not loaded after `resend_after`.
    fn update_loads(&mut self, now: Instant, out: &mut Vec<Command>) {
        let window = self.settings.preload_limit.map_or(usize::MAX, |limit| limit.saturating_add(1));
        let resend_after = self.settings.resend_after;
        let players: Vec<PlayerId> = self.riders.keys().copied().collect();
        for player in players {
            let carriage = &self.carriages[&self.riders[&player].carriage];
            let mut desired: Vec<Location> = Vec::new();
            for entry in carriage.queue.iter().take(window) {
                let location = self.location(entry.stop);
                if !desired.contains(&location) {
                    desired.push(location);
                }
            }
            let here = carriage.at().map(|gateway| self.location(gateway.stop));
            let rider = self.riders.get_mut(&player).unwrap();
            for location in &desired {
                if *location != rider.origin && !rider.wanted.contains_key(location) && !rider.ready.contains(location) {
                    rider.wanted.insert(*location, now);
                    out.push(Command::Load { player, location: *location, resend: false });
                }
            }
            let mut dropped = Vec::new();
            for (location, sent) in &mut rider.wanted {
                if !desired.contains(location) && Some(*location) != here && *location != rider.origin {
                    dropped.push(*location);
                } else if !rider.ready.contains(location) && now >= *sent + resend_after {
                    *sent = now;
                    out.push(Command::Load { player, location: *location, resend: true });
                }
            }
            for location in dropped {
                rider.wanted.remove(&location);
                rider.ready.remove(&location);
                out.push(Command::Unload { player, location });
            }
        }
    }
}

#[cfg(test)]
mod tests;

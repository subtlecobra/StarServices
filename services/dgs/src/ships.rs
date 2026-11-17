//! The ASOP kiosk's server side and the retrieval of a ship into the hangar (see docs/SHIPS.md).
//!
//! The kiosk is the kiosk's insurance component on an entity of the hangar; its authority (this DGS) answers the
//! player's list, claim, retrieve and store requests, and keeps the hangar's ship platform (the loading platform manager)
//! moving. The ships themselves are sc-entitygraph's (`ShipsInternal`): this module asks it and never reads a database.
//!
//! What is here is the logic, with nothing of the client's wire in it. Which remote methods carry the requests, and their
//! arguments, are not read from the client yet (`KioskRequest` is what they must become); the platform is driven through
//! `Action`s that the join code turns into entity variable changes once their keys are known. Everything marked assumed is
//! to be checked with the game.

use std::time::Duration;

use sc_api::internal::{
    ships_internal_client::ShipsInternalClient, ClaimShipRequest, DeliverShipRequest, HangarSize,
    ListShipsRequest, ReportShipRequest, RetrieveShipRequest, ShipClaimKind, ShipState, ShipView,
};
use tonic::{transport::Channel, Status};
use tracing::{info, warn};

use crate::{
    platform::{PlatformLink, Step as PlatformStep},
    platform_times::{self, Times},
};

/// the loading platform manager's states as the movement the platform makes. The manager's own names:
/// `RequestOpen` opens the loading gate and raises the pad (`RaisingPlatform`, then `OpenIdle`: the pad is up, at the hangar's
/// floor, and a ship on it can be flown or used); `RequestClose` lowers the pad and closes the gate (`LoweringPlatform`, then
/// `ClosedIdle`: the pad is down, below the floor, where a ship is stored and the next one is spawned).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformState {
    ClosedIdle,
    RaisingPlatform,
    OpenIdle,
    LoweringPlatform,
}

/// How long the platform takes each way when the game data has no times for the hangar class (`platform_times`): the small
/// hangar's pad door animation on the client (12.5 s) plus the margins.
/// Medium, large and XL durations are not read; they use these until they are.
pub const PLATFORM_OPEN_TIME: Duration = Duration::from_millis(13_500);
pub const PLATFORM_CLOSE_TIME: Duration = Duration::from_millis(13_000);

/// How long the loading gate's door takes to open when the game data has no times (the small hangar's gate door clip).
pub const GATE_OPEN_TIME: Duration = Duration::from_millis(12_500);

/// How long the pad's door takes to close when the game data has no times (the small hangar's pad door clip).
pub const PAD_CLOSE_TIME: Duration = Duration::from_millis(12_500);

/// How long the lowering state lasts when the pad is down already: only the platform's lights play (the doors stay closed),
/// so it is shorter than the pad door's animation.
pub const PLATFORM_LIGHTS_TIME: Duration = Duration::from_millis(6_000);

/// The hangar's ship platform. The game drops a request in the wrong state and says so in its log; so does this.
#[derive(Debug)]
pub struct Platform {
    state: PlatformState,
    moving_until: Option<Duration>,
    open_time: Duration,
    close_time: Duration,
    /// The next lowering lasts this long instead (the pad is down already).
    close_once: Option<Duration>,
}

impl Default for Platform {
    fn default() -> Self {
        Self {
            state: PlatformState::ClosedIdle,
            moving_until: None,
            open_time: PLATFORM_OPEN_TIME,
            close_time: PLATFORM_CLOSE_TIME,
            close_once: None,
        }
    }
}

impl Platform {
    /// Back to rest (down), for a retrieval that is abandoned (the client is gone, so nothing is sent).
    pub fn reset(&mut self) {
        (self.state, self.moving_until) = (PlatformState::ClosedIdle, None);
    }

    /// The pad is up with a ship on it, as the DGS has it from the last retrieval (it does not move).
    pub fn set_up(&mut self) {
        (self.state, self.moving_until) = (PlatformState::OpenIdle, None);
    }

    /// A platform that takes `times` each way.
    pub fn with_times(times: Times) -> Self {
        Self { open_time: times.open, close_time: times.close, ..Self::default() }
    }

    pub fn open_time(&self) -> Duration {
        self.open_time
    }

    pub fn close_time(&self) -> Duration {
        self.close_time
    }

    /// The next lowering lasts `time` (the pad is down already and only its lights play).
    pub fn close_once_for(&mut self, time: Duration) {
        self.close_once = Some(time);
    }

    pub fn state(&self) -> PlatformState {
        self.state
    }

    /// The platform's open request: only from `ClosedIdle`.
    pub fn request_open(&mut self, now: Duration) -> bool {
        if self.state != PlatformState::ClosedIdle {
            return false;
        }
        (self.state, self.moving_until) =
            (PlatformState::RaisingPlatform, Some(now + self.open_time));
        true
    }

    /// The platform's close request: only from `OpenIdle`.
    pub fn request_close(&mut self, now: Duration) -> bool {
        if self.state != PlatformState::OpenIdle {
            return false;
        }
        (self.state, self.moving_until) =
            (PlatformState::LoweringPlatform, Some(now + self.close_once.take().unwrap_or(self.close_time)));
        true
    }

    /// Finishes a movement whose time has come; the state it arrived in.
    pub fn tick(&mut self, now: Duration) -> Option<PlatformState> {
        let until = self.moving_until.filter(|until| *until <= now)?;
        let _ = until;
        self.moving_until = None;
        self.state = match self.state {
            PlatformState::RaisingPlatform => PlatformState::OpenIdle,
            PlatformState::LoweringPlatform => PlatformState::ClosedIdle,
            idle => idle,
        };
        Some(self.state)
    }
}

/// A request of a player at the kiosk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KioskRequest {
    List,
    Claim { geid: u64, kind: ShipClaimKind },
    Retrieve { geid: u64 },
    Store { geid: u64 },
}

/// What a retrieval asks the world for, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `RequestClose`: lower the pad (and close the gate), with the ship that stands on it.
    LowerPlatform,
    /// The pad is down: take the ship that stood on it away (its entity goes) and store it in the hangar bank.
    StoreShip { geid: u64 },
    /// Create the ship's entity on the lowered pad, from the entity sc-entitygraph keeps for `geid`.
    SpawnShip { geid: u64 },
    /// `RequestOpen`: open the gate and raise the pad with the ship on it.
    RaisePlatform,
    /// The pad is up with the ship on it.
    ShipReady { geid: u64 },
    /// A store request ended: the ship that stood on the pad is stored in the hangar bank and the pad is down and empty.
    ShipStored { geid: u64 },
    /// Put the ship back in the hangar bank: the retrieval failed after it was marked deployed.
    ReleaseShip { geid: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The pad goes down with the ship that was on it.
    Lowering,
    /// The old ship is being stored.
    Storing,
    Spawning,
    Raising,
    Done,
}

/// One retrieval, from the backend's approval to the ship on the raised pad. It is driven by the platform's states and by
/// the store and the spawn being done, and its `Action`s say what is asked next.
#[derive(Debug)]
pub struct Retrieval {
    geid: u64,
    /// The ship on the pad when it began, stored first.
    stored: Option<u64>,
    step: Step,
    /// A store request: the ship on the pad is stored and nothing is spawned after it.
    store_only: bool,
}

impl Retrieval {
    /// A retrieval the backend approved; the first action: the pad is down (spawn now), or up (lower it, with `on_pad` on it
    /// when the DGS knows a ship stands there, and that one is stored first).
    pub fn start(geid: u64, pad_up: bool, on_pad: Option<u64>) -> (Self, Action) {
        let (step, action) = if pad_up {
            (Step::Lowering, Action::LowerPlatform)
        } else {
            (Step::Spawning, Action::SpawnShip { geid })
        };
        (
            Self {
                geid,
                stored: on_pad,
                step,
                store_only: false,
            },
            action,
        )
    }

    /// A store request for the ship `geid` on the raised pad: the pad goes down, the ship is stored, and that is all.
    pub fn start_store(geid: u64) -> (Self, Action) {
        (
            Self {
                geid,
                stored: Some(geid),
                step: Step::Lowering,
                store_only: true,
            },
            Action::LowerPlatform,
        )
    }

    /// Whether this retrieval only stores the ship on the pad.
    pub fn is_store(&self) -> bool {
        self.store_only
    }

    /// Whether the ship `geid` is the one retrieved or the one stored by this retrieval.
    pub fn involves(&self, geid: u64) -> bool {
        self.geid == geid || self.stored == Some(geid)
    }

    pub fn geid(&self) -> u64 {
        self.geid
    }

    pub fn done(&self) -> bool {
        self.step == Step::Done
    }

    /// The platform arrived in `state`.
    pub fn platform(&mut self, state: PlatformState) -> Option<Action> {
        match (self.step, state) {
            (Step::Lowering, PlatformState::ClosedIdle) => match self.stored {
                Some(geid) => {
                    self.step = Step::Storing;
                    Some(Action::StoreShip { geid })
                }
                None => {
                    self.step = Step::Spawning;
                    Some(Action::SpawnShip { geid: self.geid })
                }
            },
            (Step::Raising, PlatformState::OpenIdle) => {
                self.step = Step::Done;
                Some(Action::ShipReady { geid: self.geid })
            }
            _ => None,
        }
    }

    /// The old ship is stored (or could not be: it stays deployed in its record, and the pad is clear all the same).
    pub fn stored(&mut self) -> Option<Action> {
        if self.step != Step::Storing {
            return None;
        }
        if self.store_only {
            self.step = Step::Done;
            return Some(Action::ShipStored { geid: self.geid });
        }
        self.step = Step::Spawning;
        Some(Action::SpawnShip { geid: self.geid })
    }

    /// The ship's entity exists on the pad (`ok`), or could not be made.
    pub fn spawned(&mut self, ok: bool) -> Action {
        if self.step != Step::Spawning {
            return Action::ReleaseShip { geid: self.geid };
        }
        if ok {
            self.step = Step::Raising;
            Action::RaisePlatform
        } else {
            self.step = Step::Done;
            Action::ReleaseShip { geid: self.geid }
        }
    }
}

/// What the kiosk asks of sc-entitygraph.
#[tonic::async_trait]
pub trait ShipBackend: Send + Sync {
    async fn list(&self, request: ListShipsRequest) -> Result<Vec<ShipView>, Status>;
    async fn claim(&self, request: ClaimShipRequest) -> Result<ShipView, Status>;
    async fn retrieve(&self, request: RetrieveShipRequest) -> Result<ShipView, Status>;
    async fn report(&self, request: ReportShipRequest) -> Result<ShipView, Status>;
    async fn deliver(&self, request: DeliverShipRequest) -> Result<ShipView, Status>;
}

#[tonic::async_trait]
impl ShipBackend for ShipsInternalClient<Channel> {
    async fn list(&self, request: ListShipsRequest) -> Result<Vec<ShipView>, Status> {
        Ok(self.clone().list_ships(request).await?.into_inner().ships)
    }
    async fn claim(&self, request: ClaimShipRequest) -> Result<ShipView, Status> {
        self.clone()
            .claim_ship(request)
            .await?
            .into_inner()
            .ship
            .ok_or_else(|| Status::internal("no ship"))
    }
    async fn retrieve(&self, request: RetrieveShipRequest) -> Result<ShipView, Status> {
        self.clone()
            .retrieve_ship(request)
            .await?
            .into_inner()
            .ship
            .ok_or_else(|| Status::internal("no ship"))
    }
    async fn report(&self, request: ReportShipRequest) -> Result<ShipView, Status> {
        self.clone()
            .report_ship(request)
            .await?
            .into_inner()
            .ship
            .ok_or_else(|| Status::internal("no ship"))
    }
    async fn deliver(&self, request: DeliverShipRequest) -> Result<ShipView, Status> {
        self.clone()
            .deliver_ship(request)
            .await?
            .into_inner()
            .ship
            .ok_or_else(|| Status::internal("no ship"))
    }
}

/// The kiosk of one hangar: the ships it lists and spawns are those that fit its size, and its platform is the only
/// way a ship gets in. One retrieval runs at a time (the game logs `RequestVehicle called while already retrieving`).
pub struct Kiosk<B: ShipBackend> {
    backend: B,
    /// The player at the kiosk, whose hangar this is.
    owner: u64,
    location_id: u32,
    size: HangarSize,
    /// Only ships of this hangar's own size (`ListShipsRequest.exact_size`). The hangars of a location differ in size,
    /// so a ship goes to the smallest that fits when the kiosk of that hangar is the one used (assumed).
    exact: bool,
    pub platform: Platform,
    /// Where the platform's changes go: the hangar's client. Without one (no known layout, or a test) the platform runs
    /// on its clock alone.
    link: Option<PlatformLink>,
    retrieval: Option<Retrieval>,
    /// The ship the retrieval in hand is of, as the backend approved it (its class, for the spawn).
    retrieving: Option<ShipView>,
    /// The ship that stands on the raised pad, from the last retrieval that ended with it up.
    on_pad: Option<u64>,
    /// Whether the client's pad has been lowered by a `Close` since the join. The merged manager binds at `ClosedIdle`, which
    /// stops the sequences and leaves the pad and the loading gate at their static pose: coincident with the landing area (all
    /// three sit at the same point in `hangar_smlfrnt_001`), so a ship spawned there is refused as 'colliding with pent
    /// LoadingPlatform...'. The first retrieval lowers the pad first (as the manager's `alwaysLowerPlatformBeforeSpawn`).
    pad_lowered: bool,
    commands: u64,
}

/// `SC_DGS_PLATFORM_LOWER_FIRST=off|0|false|no` keeps the first retrieval from lowering the pad before it spawns the ship.
pub const LOWER_FIRST_SETTING: &str = "SC_DGS_PLATFORM_LOWER_FIRST";

fn lower_first() -> bool {
    !std::env::var(LOWER_FIRST_SETTING).is_ok_and(|value| {
        ["off", "0", "false", "no"].contains(&value.to_ascii_lowercase().as_str())
    })
}

impl<B: ShipBackend> Kiosk<B> {
    pub fn new(backend: B, owner: u64, location_id: u32, hangar_class: &str, exact: bool) -> Self {
        Self::with_times(backend, owner, location_id, hangar_class, exact, platform_times::installed_of(hangar_class))
    }

    /// A kiosk whose platform takes `times` each way, or the fixed times without any.
    pub fn with_times(backend: B, owner: u64, location_id: u32, hangar_class: &str, exact: bool, times: Option<Times>) -> Self {
        let size = hangar_class::hangar_size_of(hangar_class);
        Self {
            backend,
            owner,
            location_id,
            size,
            exact,
            platform: times.map_or_else(Platform::default, Platform::with_times),
            link: None,
            retrieval: None,
            retrieving: None,
            on_pad: None,
            // The tests model a pad that is where the DGS says it is; `the_first_retrieval_lowers...` clears this.
            pad_lowered: cfg!(test),
            commands: 0,
        }
    }

    /// How long this hangar's platform takes to rise and to go down.
    pub fn platform_open_time(&self) -> Duration {
        self.platform.open_time()
    }

    pub fn platform_close_time(&self) -> Duration {
        self.platform.close_time()
    }

    pub fn size(&self) -> HangarSize {
        self.size
    }

    /// Sends the platform's door and manager changes to the hangar's client.
    pub fn with_link(mut self, link: PlatformLink) -> Self {
        self.link = Some(link);
        self
    }

    fn announce(&self, step: PlatformStep) {
        if let Some(link) = &self.link {
            link.announce(step);
        }
    }

    /// `RequestOpen`: lowers the platform and tells the client. `false` when it is not closed and idle.
    fn open_platform(&mut self, now: Duration) -> bool {
        let opened = self.platform.request_open(now);
        if opened {
            self.announce(PlatformStep::Open);
        }
        opened
    }

    /// `RequestClose`: raises the platform and tells the client.
    /// `to_rest`: nothing comes up after it (a ship stored), so the client is told to settle the lights when it is down.
    fn close_platform(&mut self, now: Duration, to_rest: bool) -> bool {
        let closed = self.platform.request_close(now);
        if closed {
            self.announce(if to_rest { PlatformStep::CloseToRest } else { PlatformStep::Close });
        }
        closed
    }

    /// The client is gone, or the retrieval failed past repair: the ship goes back to the bank and the platform to rest.
    /// Nothing is sent: the next join binds the hangar as it is. `false` when nothing was being retrieved.
    pub async fn abort(&mut self, now_ms: i64, request_id: u64) -> bool {
        let Some(retrieval) = self.retrieval.take().filter(|retrieval| !retrieval.done()) else {
            return false;
        };
        let geid = retrieval.geid();
        warn!(
            owner = self.owner,
            geid, "ship retrieval abandoned: stored again"
        );
        let command_id = self.command("abort", request_id);
        let request = ReportShipRequest {
            command_id,
            owner_id: self.owner,
            geid,
            state: ShipState::Stored as i32,
            location_id: self.location_id,
            now_ms,
        };
        if let Err(status) = self.backend.report(request).await {
            warn!(geid, %status, "the ship stays deployed in its record until it is stored");
        }
        self.platform.reset();
        self.on_pad = None;
        true
    }

    /// A command id for a call, unique to this kiosk's use; a repeat of a call (a client's retry) must reuse the id the
    /// first had, so the caller passes `request_id` through.
    fn command(&mut self, what: &str, request_id: u64) -> String {
        self.commands += 1;
        format!("kiosk:{}:{what}:{request_id}", self.owner)
    }

    /// The ships of the player at this location that fit this hangar. An unknown hangar size lists nothing.
    pub async fn list(&self, now_ms: i64) -> Result<Vec<ShipView>, Status> {
        if self.size == HangarSize::Unspecified {
            return Ok(Vec::new());
        }
        self.backend
            .list(ListShipsRequest {
                owner_id: self.owner,
                location_id: self.location_id,
                hangar_size: self.size as i32,
                exact_size: self.exact,
                now_ms,
            })
            .await
    }

    pub async fn claim(
        &mut self,
        geid: u64,
        kind: ShipClaimKind,
        request_id: u64,
        now_ms: i64,
    ) -> Result<ShipView, Status> {
        let command_id = self.command("claim", request_id);
        self.backend
            .claim(ClaimShipRequest {
                command_id,
                owner_id: self.owner,
                geid,
                kind: kind as i32,
                now_ms,
            })
            .await
    }

    /// Retrieve: the backend approves (the ship is stored and fits), then the pad is made ready and the ship comes up on it:
    /// - the pad is **up** with a ship on it: it goes down (`LowerPlatform`), that ship is stored (`StoreShip`), and then as
    ///   below;
    /// - the pad is **down**: the ship spawns on it (`SpawnShip`), the gate opens and the pad rises with it (`RaisePlatform`),
    ///   and at `OpenIdle` it is ready (`ShipReady`).
    ///
    /// The first action is returned; `tick`, `stored` and `spawned` return the rest. The ship already on the pad is refused
    /// (`already_exists`), and so is a request while the pad moves or another retrieval runs.
    pub async fn retrieve(
        &mut self,
        geid: u64,
        request_id: u64,
        now: Duration,
        now_ms: i64,
    ) -> Result<Vec<Action>, Status> {
        if self.busy() {
            return Err(Status::failed_precondition(
                "a ship is being retrieved already",
            ));
        }
        if self.platform.state() == PlatformState::ClosedIdle && (!self.pad_lowered || !cfg!(test)) && lower_first() {
            // The client's pad still stands at its static pose, or is down already: it goes through the lowering before
            // the ship is made (its lights play in that state; the doors, closed, ignore the step).
            self.platform.set_up();
            if self.pad_lowered {
                // Down for real (a store or a retrieval before): only the lights play. A pad the client still has up
                // (the first retrieval after a join) goes down with its doors, for as long as they take.
                self.platform.close_once_for(PLATFORM_LIGHTS_TIME);
            }
        }
        let pad_up = match self.platform.state() {
            PlatformState::ClosedIdle => false,
            PlatformState::OpenIdle => true,
            PlatformState::RaisingPlatform | PlatformState::LoweringPlatform => {
                return Err(Status::failed_precondition("the platform is moving"));
            }
        };
        if pad_up && self.on_pad == Some(geid) {
            return Err(Status::already_exists("the ship is on the platform"));
        }
        let command_id = self.command("retrieve", request_id);
        let request = RetrieveShipRequest {
            command_id,
            owner_id: self.owner,
            geid,
            hangar_size: self.size as i32,
            exact_size: self.exact,
            now_ms,
            ..Default::default()
        };
        let view = self.backend.retrieve(request).await?;
        self.retrieving = Some(view);
        let (retrieval, action) = Retrieval::start(geid, pad_up, self.on_pad);
        if pad_up {
            self.close_platform(now, false);
        }
        self.retrieval = Some(retrieval);
        info!(owner = self.owner, geid, pad_up, on_pad = ?self.on_pad, "ship retrieval started");
        Ok(vec![action])
    }

    /// Store: the ship on the raised pad goes down with the pad and is stored at this location (the ASOP's Store button).
    /// The pad lowers (`LowerPlatform`, the ship unbound as it starts down), then `StoreShip` reports the ship stored, and
    /// `ShipStored` ends it. Refused when a retrieval runs, the pad moves or is down, or `geid` is not the ship on it.
    pub fn store(&mut self, geid: u64, now: Duration) -> Result<Vec<Action>, Status> {
        if self.busy() {
            return Err(Status::failed_precondition("a ship is being retrieved already"));
        }
        match self.platform.state() {
            PlatformState::OpenIdle => {}
            PlatformState::ClosedIdle => {
                return Err(Status::failed_precondition("no ship is on the platform"));
            }
            PlatformState::RaisingPlatform | PlatformState::LoweringPlatform => {
                return Err(Status::failed_precondition("the platform is moving"));
            }
        }
        if self.on_pad != Some(geid) {
            return Err(Status::not_found("the ship is not on the platform"));
        }
        let (retrieval, action) = Retrieval::start_store(geid);
        self.close_platform(now, true);
        self.retrieval = Some(retrieval);
        info!(owner = self.owner, geid, "ship store started");
        Ok(vec![action])
    }

    /// The ship being retrieved, from the approval to the next retrieval.
    pub fn retrieving(&self) -> Option<&ShipView> {
        self.retrieving.as_ref()
    }

    /// The ship on the raised pad, if the DGS knows one.
    pub fn on_pad(&self) -> Option<u64> {
        self.on_pad
    }

    /// The ship moving to storage, while its pad lowers: the ASOP lists it as landed still, so that its row shows "Moving to Storage".
    pub fn storing(&self) -> Option<u64> {
        self.retrieval.as_ref().filter(|retrieval| !retrieval.done() && retrieval.is_store()).map(Retrieval::geid)
    }

    /// Whether the ship `geid` is at this kiosk's hangar bay: on the raised pad, being retrieved to it, or moving to storage.
    /// A delivery of it is refused: it is delivered already, or on its way into storage.
    pub fn engages(&self, geid: u64) -> bool {
        let moving = self.retrieval.as_ref().is_some_and(|retrieval| !retrieval.done() && retrieval.involves(geid));
        moving || self.on_pad == Some(geid)
    }

    /// Whether a retrieval is running (approved and not yet on the raised pad or failed).
    pub fn busy(&self) -> bool {
        self.retrieval
            .as_ref()
            .is_some_and(|retrieval| !retrieval.done())
    }

    /// The platform's clock; what the retrieval asks next.
    pub fn tick(&mut self, now: Duration) -> Vec<Action> {
        let Some(state) = self.platform.tick(now) else {
            return Vec::new();
        };
        self.pad_lowered |= state == PlatformState::ClosedIdle;
        self.announce(if state == PlatformState::OpenIdle {
            PlatformStep::Opened
        } else {
            PlatformStep::Closed
        });
        let Some(retrieval) = self.retrieval.as_mut() else {
            return Vec::new();
        };
        let action = retrieval.platform(state);
        if let Some(Action::ShipReady { geid }) = action {
            self.on_pad = Some(geid);
        }
        action.into_iter().collect()
    }

    /// The pad is down and the ship that stood on it is taken away: its record is stored again at this hangar.
    pub async fn stored(&mut self, geid: u64, now_ms: i64, request_id: u64) -> Vec<Action> {
        let command_id = self.command("store", request_id);
        let request = ReportShipRequest {
            command_id,
            owner_id: self.owner,
            geid,
            state: ShipState::Stored as i32,
            location_id: self.location_id,
            now_ms,
        };
        if let Err(status) = self.backend.report(request).await {
            warn!(geid, %status, "the ship stays deployed in its record until it is stored");
        }
        self.on_pad = None;
        info!(owner = self.owner, geid, "the ship that was on the platform is stored");
        self.retrieval
            .as_mut()
            .and_then(Retrieval::stored)
            .into_iter()
            .collect()
    }

    /// The spawn finished. A failure puts the ship back in the bank, and the pad stays down.
    pub async fn spawned(
        &mut self,
        ok: bool,
        now: Duration,
        now_ms: i64,
        request_id: u64,
    ) -> Vec<Action> {
        let Some(retrieval) = self.retrieval.as_mut() else {
            return Vec::new();
        };
        let action = retrieval.spawned(ok);
        match &action {
            Action::RaisePlatform => {
                self.open_platform(now);
            }
            Action::ReleaseShip { geid } => {
                let geid = *geid;
                warn!(
                    owner = self.owner,
                    geid, "the ship could not be spawned: stored again"
                );
                let command_id = self.command("release", request_id);
                let request = ReportShipRequest {
                    command_id,
                    owner_id: self.owner,
                    geid,
                    state: ShipState::Stored as i32,
                    location_id: self.location_id,
                    now_ms,
                };
                if let Err(status) = self.backend.report(request).await {
                    warn!(geid, %status, "the ship stays deployed in its record until it is stored");
                }
            }
            _ => {}
        }
        vec![action]
    }

    /// A ship put away at this kiosk, or lost: the record and the graph follow.
    pub async fn report(
        &mut self,
        geid: u64,
        state: ShipState,
        request_id: u64,
        now_ms: i64,
    ) -> Result<ShipView, Status> {
        let command_id = self.command("report", request_id);
        self.backend
            .report(ReportShipRequest {
                command_id,
                owner_id: self.owner,
                geid,
                state: state as i32,
                location_id: self.location_id,
                now_ms,
            })
            .await
    }
}

/// The hangar size of a class name, as sc-entitygraph reads it (`Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`).
pub(crate) mod hangar_class {
    use sc_api::internal::HangarSize;

    pub fn hangar_size_of(class_name: &str) -> HangarSize {
        let kind = class_name.strip_prefix("Hangar_").unwrap_or_default();
        [
            ("Small", HangarSize::Small),
            ("Medium", HangarSize::Medium),
            ("Large", HangarSize::Large),
            ("XL", HangarSize::Xlarge),
        ]
        .into_iter()
        .find(|(prefix, _)| kind.starts_with(prefix))
        .map_or(HangarSize::Unspecified, |(_, size)| size)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sc_api::internal::{PadSize, ShipRecord};

    use super::*;

    #[derive(Default)]
    struct Fake {
        ships: Mutex<Vec<ShipRecord>>,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn with(ships: Vec<(u64, PadSize, ShipState)>) -> Self {
            let ships = ships
                .into_iter()
                .map(|(geid, pad, state)| ShipRecord {
                    geid,
                    pad_size: pad as i32,
                    state: state as i32,
                    ..Default::default()
                })
                .collect();
            Self {
                ships: Mutex::new(ships),
                calls: Mutex::default(),
            }
        }
    }

    fn fits(pad: PadSize, size: i32, exact: bool) -> bool {
        if exact {
            (pad as i32).max(3) == size
        } else {
            pad as i32 <= size
        }
    }

    #[tonic::async_trait]
    impl ShipBackend for &Fake {
        async fn list(&self, request: ListShipsRequest) -> Result<Vec<ShipView>, Status> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("list {}", request.hangar_size));
            let ships = self.ships.lock().unwrap();
            Ok(ships
                .iter()
                .filter(|ship| fits(ship.pad_size(), request.hangar_size, request.exact_size))
                .map(|ship| ShipView {
                    ship: Some(ship.clone()),
                    fits: true,
                    ..Default::default()
                })
                .collect())
        }
        async fn claim(&self, request: ClaimShipRequest) -> Result<ShipView, Status> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("claim {} {}", request.geid, request.command_id));
            Ok(ShipView::default())
        }
        async fn retrieve(&self, request: RetrieveShipRequest) -> Result<ShipView, Status> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("retrieve {}", request.geid));
            let mut ships = self.ships.lock().unwrap();
            let ship = ships
                .iter_mut()
                .find(|ship| ship.geid == request.geid)
                .ok_or_else(|| Status::not_found("no ship"))?;
            if ship.state() != ShipState::Stored
                || !fits(ship.pad_size(), request.hangar_size, request.exact_size)
            {
                return Err(Status::failed_precondition("not retrievable here"));
            }
            ship.state = ShipState::Deployed as i32;
            Ok(ShipView {
                ship: Some(ship.clone()),
                ..Default::default()
            })
        }
        async fn report(&self, request: ReportShipRequest) -> Result<ShipView, Status> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("report {} {}", request.geid, request.state));
            let mut ships = self.ships.lock().unwrap();
            if let Some(ship) = ships.iter_mut().find(|ship| ship.geid == request.geid) {
                ship.state = request.state;
            }
            Ok(ShipView::default())
        }
        async fn deliver(&self, request: DeliverShipRequest) -> Result<ShipView, Status> {
            self.calls.lock().unwrap().push(format!(
                "deliver {} {}",
                request.geid, request.destination_location_id
            ));
            Ok(ShipView::default())
        }
    }

    const T0: Duration = Duration::from_secs(100);

    #[test]
    fn the_platform_drops_requests_in_the_wrong_state() {
        let mut platform = Platform::default();
        assert!(!platform.request_close(T0), "down already");
        assert!(platform.request_open(T0));
        assert!(!platform.request_open(T0), "already rising");
        assert_eq!(platform.state(), PlatformState::RaisingPlatform);
        assert_eq!(
            platform.tick(T0 + PLATFORM_OPEN_TIME - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            platform.tick(T0 + PLATFORM_OPEN_TIME),
            Some(PlatformState::OpenIdle)
        );
        assert!(platform.request_close(T0 + PLATFORM_OPEN_TIME));
        assert_eq!(platform.state(), PlatformState::LoweringPlatform);
        assert_eq!(
            platform.tick(T0 + PLATFORM_OPEN_TIME + PLATFORM_CLOSE_TIME),
            Some(PlatformState::ClosedIdle)
        );
    }

    #[test]
    fn a_hangar_whose_class_has_times_takes_them_and_the_others_the_fixed_ones() {
        let fake = Fake::default();
        let table = crate::platform_times::PlatformTimes::parse(
            "Hangar_LargeTop_Orison\t39000\t37667\t15000\t15000\t15000\t15000\n",
        )
        .unwrap();
        let class = "Hangar_LargeTop_Orison";
        let large = Kiosk::with_times(&fake, 7, 9, class, false, table.of(class));
        assert_eq!(large.platform_close_time(), Duration::from_millis(38_167));
        assert_eq!(large.platform_open_time(), Duration::from_millis(39_300));
        let class = "Hangar_SmallFront_Orison";
        let missing = Kiosk::with_times(&fake, 7, 9, class, false, table.of(class));
        assert_eq!(missing.platform_open_time(), PLATFORM_OPEN_TIME);
        assert_eq!(missing.platform_close_time(), PLATFORM_CLOSE_TIME);
        // With no table installed (as in tests) every class has the fixed times.
        assert_eq!(Kiosk::new(&fake, 7, 9, "Hangar_LargeTop_Orison", false).platform_close_time(), PLATFORM_CLOSE_TIME);
    }

    #[test]
    fn hangar_sizes_come_from_the_class() {
        assert_eq!(
            hangar_class::hangar_size_of("Hangar_SmallFront_Orison"),
            HangarSize::Small
        );
        assert_eq!(
            hangar_class::hangar_size_of("Hangar_XLTop_Area18"),
            HangarSize::Xlarge
        );
        assert_eq!(
            hangar_class::hangar_size_of("Elsewhere"),
            HangarSize::Unspecified
        );
    }

    #[tokio::test]
    async fn the_kiosk_lists_only_the_ships_that_fit_its_hangar() {
        let fake = Fake::with(vec![
            (1, PadSize::Xsmall, ShipState::Stored),
            (2, PadSize::Medium, ShipState::Stored),
            (3, PadSize::Large, ShipState::Stored),
        ]);
        let small = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        assert_eq!(
            small
                .list(0)
                .await
                .unwrap()
                .iter()
                .map(|view| view.ship.as_ref().unwrap().geid)
                .collect::<Vec<_>>(),
            [1]
        );
        let large = Kiosk::new(&fake, 7, 9, "Hangar_LargeTop_Orison", false);
        assert_eq!(large.list(0).await.unwrap().len(), 3);
        let exact = Kiosk::new(&fake, 7, 9, "Hangar_LargeTop_Orison", true);
        assert_eq!(exact.list(0).await.unwrap().len(), 1);
        let unknown = Kiosk::new(&fake, 7, 9, "Lobby", false);
        assert!(unknown.list(0).await.unwrap().is_empty());
        assert_eq!(
            fake.calls.lock().unwrap().len(),
            3,
            "an unknown hangar asks nothing"
        );
    }

    #[tokio::test]
    async fn a_retrieval_spawns_the_ship_on_the_lowered_pad_and_raises_it() {
        let fake = Fake::with(vec![
            (1, PadSize::Xsmall, ShipState::Stored),
            (3, PadSize::Large, ShipState::Stored),
        ]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        let refused = kiosk.retrieve(3, 1, T0, 0).await.unwrap_err();
        assert_eq!(
            refused.code(),
            tonic::Code::FailedPrecondition,
            "a large ship does not come out of a small hangar"
        );
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle, "nothing moved");

        assert_eq!(
            kiosk.retrieve(1, 2, T0, 0).await.unwrap(),
            [Action::SpawnShip { geid: 1 }],
            "the pad is down: the ship spawns at once"
        );
        assert_eq!(
            kiosk.retrieve(1, 3, T0, 0).await.unwrap_err().code(),
            tonic::Code::FailedPrecondition,
            "one at a time"
        );
        let raising = kiosk.spawned(true, T0, 0, 4).await;
        assert_eq!(raising, [Action::RaisePlatform]);
        assert_eq!(kiosk.platform.state(), PlatformState::RaisingPlatform);
        assert!(kiosk.tick(T0 + Duration::from_secs(1)).is_empty());
        assert_eq!(
            kiosk.tick(T0 + PLATFORM_OPEN_TIME),
            [Action::ShipReady { geid: 1 }]
        );
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Deployed);
        assert_eq!(kiosk.platform.state(), PlatformState::OpenIdle);
        assert_eq!(kiosk.on_pad(), Some(1));
    }

    #[tokio::test]
    async fn the_first_retrieval_lowers_the_pad_that_the_client_still_has_at_its_static_pose() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.pad_lowered = false;
        assert_eq!(
            kiosk.retrieve(1, 1, T0, 0).await.unwrap(),
            [Action::LowerPlatform],
            "the pad is lowered before the ship is made"
        );
        assert_eq!(kiosk.platform.state(), PlatformState::LoweringPlatform);
        assert!(kiosk.tick(T0 + Duration::from_secs(1)).is_empty());
        assert_eq!(
            kiosk.tick(T0 + PLATFORM_CLOSE_TIME),
            [Action::SpawnShip { geid: 1 }],
            "the ship is made once the pad is down"
        );
        assert!(kiosk.pad_lowered);
    }

    #[tokio::test]
    async fn a_ship_on_the_raised_pad_goes_down_and_is_stored_before_the_next_comes_up() {
        let fake = Fake::with(vec![
            (1, PadSize::Xsmall, ShipState::Stored),
            (2, PadSize::Xsmall, ShipState::Stored),
        ]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        kiosk.spawned(true, T0, 0, 1).await;
        let up = T0 + PLATFORM_OPEN_TIME;
        kiosk.tick(up);
        assert_eq!(
            kiosk.retrieve(1, 2, up, 0).await.unwrap_err().code(),
            tonic::Code::AlreadyExists,
            "the ship that is on the pad is not retrieved again"
        );
        assert_eq!(
            kiosk.retrieve(2, 3, up, 0).await.unwrap(),
            [Action::LowerPlatform]
        );
        assert_eq!(kiosk.platform.state(), PlatformState::LoweringPlatform);
        assert!(kiosk.tick(up + Duration::from_secs(1)).is_empty());
        let down = up + PLATFORM_CLOSE_TIME;
        assert_eq!(kiosk.tick(down), [Action::StoreShip { geid: 1 }]);
        assert_eq!(
            fake.ships.lock().unwrap()[0].state(),
            ShipState::Deployed,
            "stored only when the pad is down"
        );
        assert_eq!(kiosk.stored(1, 0, 1).await, [Action::SpawnShip { geid: 2 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Stored);
        assert_eq!(kiosk.on_pad(), None);
        assert_eq!(kiosk.spawned(true, down, 0, 2).await, [Action::RaisePlatform]);
        assert_eq!(kiosk.tick(down + PLATFORM_OPEN_TIME), [Action::ShipReady { geid: 2 }]);
        assert_eq!(fake.ships.lock().unwrap()[1].state(), ShipState::Deployed);
        assert_eq!(kiosk.on_pad(), Some(2));
    }

    #[tokio::test]
    async fn the_store_request_lowers_the_pad_stores_the_ship_and_leaves_the_pad_down() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        assert_eq!(
            kiosk.store(1, T0).unwrap_err().code(),
            tonic::Code::FailedPrecondition,
            "the pad is down: no ship is on it"
        );
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        kiosk.spawned(true, T0, 0, 1).await;
        assert_eq!(
            kiosk.store(1, T0).unwrap_err().code(),
            tonic::Code::FailedPrecondition,
            "a retrieval is running"
        );
        let up = T0 + PLATFORM_OPEN_TIME;
        assert_eq!(kiosk.tick(up), [Action::ShipReady { geid: 1 }]);
        assert_eq!(kiosk.store(2, up).unwrap_err().code(), tonic::Code::NotFound);
        assert_eq!(kiosk.store(1, up).unwrap(), [Action::LowerPlatform]);
        assert_eq!(kiosk.platform.state(), PlatformState::LoweringPlatform);
        assert_eq!(
            kiosk.store(1, up).unwrap_err().code(),
            tonic::Code::FailedPrecondition,
            "the pad moves"
        );
        assert!(kiosk.busy());
        let down = up + PLATFORM_CLOSE_TIME;
        assert_eq!(kiosk.tick(down), [Action::StoreShip { geid: 1 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Deployed);
        assert_eq!(kiosk.stored(1, 0, 1).await, [Action::ShipStored { geid: 1 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Stored);
        assert_eq!(kiosk.on_pad(), None);
        assert!(!kiosk.busy());
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle);
        assert!(kiosk.retrieve(1, 2, down, 0).await.is_ok(), "it can be retrieved again");
    }

    #[tokio::test]
    async fn a_ship_that_cannot_be_spawned_goes_back_to_the_bank() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        let actions = kiosk.spawned(false, T0, 0, 2).await;
        assert_eq!(actions, [Action::ReleaseShip { geid: 1 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Stored);
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle, "the pad never moved");
        assert!(!kiosk.busy());
    }

    fn linked(
        fake: &Fake,
    ) -> (
        Kiosk<&Fake>,
        tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>,
    ) {
        use crate::platform::{Indices, PlatformIds};
        let outbox = crate::outbox::Outbox::default();
        let receiver = outbox.subscribe((1, 1));
        let link = PlatformLink::new(
            outbox,
            (1, 1),
            PlatformIds {
                manager: 80,
                platform: 84,
            },
        )
        .with_indices(Indices {
            current: Some(1),
            planned: Some(2),
            door: Some(3),
            ..Indices::default()
        });
        (
            Kiosk::new(fake, 7, 9, "Hangar_SmallFront_Orison", false).with_link(link),
            receiver,
        )
    }

    fn drain(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>,
    ) -> Vec<(u64, u32, u8)> {
        use sc_api::internal::session_update::Update;
        let mut sent = Vec::new();
        while let Ok(Ok(update)) = receiver.try_recv() {
            let Some(Update::Variables(variables)) = update.update else {
                panic!("not a variable update")
            };
            sent.push((
                variables.entity_id,
                variables.variables[0].index,
                variables.variables[0].value[0],
            ));
        }
        sent
    }

    #[tokio::test]
    async fn a_retrieval_sends_the_platforms_door_and_manager_states_in_order() {
        let fake = Fake::with(vec![
            (1, PadSize::Xsmall, ShipState::Stored),
            (2, PadSize::Xsmall, ShipState::Stored),
        ]);
        let (mut kiosk, mut receiver) = linked(&fake);
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        assert!(drain(&mut receiver).is_empty(), "the pad is down: nothing moves before the ship spawns");
        kiosk.spawned(true, T0, 0, 1).await;
        assert_eq!(
            drain(&mut receiver),
            [(84, 3, 5), (80, 1, 1), (80, 2, 3)],
            "door opening, manager 1 -> 3"
        );
        assert!(kiosk.tick(T0 + PLATFORM_OPEN_TIME - Duration::from_millis(1)).is_empty());
        assert!(drain(&mut receiver).is_empty(), "nothing before the measured time");
        let up = T0 + PLATFORM_OPEN_TIME;
        kiosk.tick(up);
        assert_eq!(
            drain(&mut receiver),
            [(84, 3, 3), (80, 1, 3)],
            "door opened, manager OpenIdle"
        );
        kiosk.retrieve(2, 2, up, 0).await.unwrap();
        assert_eq!(
            drain(&mut receiver),
            [(84, 3, 2), (80, 1, 4), (80, 2, 7)],
            "door closing, manager 4 -> 7"
        );
        kiosk.tick(up + PLATFORM_CLOSE_TIME);
        assert_eq!(
            drain(&mut receiver),
            [(84, 3, 0), (80, 1, 7)],
            "door closed, manager ClosedIdle"
        );
    }

    #[tokio::test]
    async fn a_disconnect_mid_retrieval_stores_the_ship_again_and_rests_the_platform() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let (mut kiosk, _receiver) = linked(&fake);
        assert!(!kiosk.abort(0, 1).await, "nothing to abandon");
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Deployed);
        assert!(kiosk.abort(0, 2).await);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Stored);
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle);
        kiosk
            .retrieve(1, 3, T0, 0)
            .await
            .expect("a new retrieval may start");
    }

    #[tokio::test]
    async fn a_claim_carries_the_clients_request_id() {
        let fake = Fake::default();
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.claim(5, ShipClaimKind::Insured, 42, 0).await.unwrap();
        kiosk.claim(5, ShipClaimKind::Insured, 42, 0).await.unwrap();
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls[0], calls[1],
            "a retry has the same command id, so sc-entitygraph applies it once"
        );
    }
}

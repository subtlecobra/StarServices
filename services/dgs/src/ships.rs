//! The ASOP kiosk's server side and the retrieval of a ship into the hangar (see docs/SHIPS.md).
//!
//! The kiosk is `CEntityComponentShipInsuranceProvider` on an entity of the hangar; its authority (this DGS) answers the
//! player's list, claim, retrieve and store requests, and keeps the hangar's ship platform (`CSCLoadingPlatformManager`)
//! moving. The ships themselves are sc-entitygraph's (`ShipsInternal`): this module asks it and never reads a database.
//!
//! What is here is the logic, with nothing of the client's wire in it. Which remote methods carry the requests, and their
//! arguments, are not read from the client yet (`KioskRequest` is what they must become); the platform is driven through
//! `Action`s that the join code turns into entity variable changes once their keys are known. Everything marked inferred is
//! to be checked with the game.

use std::time::Duration;

use sc_api::internal::{
    ships_internal_client::ShipsInternalClient, ClaimShipRequest, HangarSize, ListShipsRequest, RetrieveShipRequest, ReportShipRequest,
    ShipClaimKind, ShipState, ShipView,
};
use tonic::{transport::Channel, Status};
use tracing::{info, warn};

use crate::platform::{PlatformLink, Step as PlatformStep};

/// `CSCLoadingPlatformManager`'s states, as its log lines name them. An open platform is lowered to receive a ship
/// (inferred: the names say only that open is idle after lowering, and `LandingAreaComponentParams.alwaysLowerPlatformBeforeSpawn`
/// says a ship spawns on a lowered platform).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformState {
    ClosedIdle,
    LoweringPlatform,
    OpenIdle,
    RaisingPlatform,
}

/// How long the platform takes each way: measured on the small hangars (transit data.md: open 33 s, close 31.6667 s).
/// Medium, large and XL durations are not read; they use these until they are.
pub const PLATFORM_OPEN_TIME: Duration = Duration::from_millis(33_000);
pub const PLATFORM_CLOSE_TIME: Duration = Duration::from_micros(31_666_700);

/// The hangar's ship platform. The game drops a request in the wrong state and says so in its log; so does this.
#[derive(Debug)]
pub struct Platform {
    state: PlatformState,
    moving_until: Option<Duration>,
    open_time: Duration,
    close_time: Duration,
}

impl Default for Platform {
    fn default() -> Self {
        Self { state: PlatformState::ClosedIdle, moving_until: None, open_time: PLATFORM_OPEN_TIME, close_time: PLATFORM_CLOSE_TIME }
    }
}

impl Platform {
    /// Back to rest, for a retrieval that is abandoned (the client is gone, so nothing is sent).
    pub fn reset(&mut self) {
        (self.state, self.moving_until) = (PlatformState::ClosedIdle, None);
    }

    pub fn state(&self) -> PlatformState {
        self.state
    }

    /// `SCEvtNotify_OnPlatformStateRequestOpen`: only from `ClosedIdle`.
    pub fn request_open(&mut self, now: Duration) -> bool {
        if self.state != PlatformState::ClosedIdle {
            return false;
        }
        (self.state, self.moving_until) = (PlatformState::LoweringPlatform, Some(now + self.open_time));
        true
    }

    /// `SCEvtNotify_OnPlatformStateRequestClose`: only from `OpenIdle`.
    pub fn request_close(&mut self, now: Duration) -> bool {
        if self.state != PlatformState::OpenIdle {
            return false;
        }
        (self.state, self.moving_until) = (PlatformState::RaisingPlatform, Some(now + self.close_time));
        true
    }

    /// Finishes a movement whose time has come; the state it arrived in.
    pub fn tick(&mut self, now: Duration) -> Option<PlatformState> {
        let until = self.moving_until.filter(|until| *until <= now)?;
        let _ = until;
        self.moving_until = None;
        self.state = match self.state {
            PlatformState::LoweringPlatform => PlatformState::OpenIdle,
            PlatformState::RaisingPlatform => PlatformState::ClosedIdle,
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
    /// Lower the hangar's platform.
    LowerPlatform,
    /// Create the ship's entity on the lowered platform, from the entity sc-entitygraph keeps for `geid`.
    SpawnShip { geid: u64 },
    /// Raise the platform with the ship on it, into the hangar bay.
    RaisePlatform,
    /// The ship is in the bay.
    ShipReady { geid: u64 },
    /// Put the ship back in the hangar bank: the retrieval failed after it was marked deployed.
    ReleaseShip { geid: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Lowering,
    Spawning,
    Raising,
    Done,
}

/// One retrieval, from the backend's approval to the ship in the bay. It is driven by the platform's states and by the
/// spawn being done, and its `Action`s say what is asked next.
#[derive(Debug)]
pub struct Retrieval {
    geid: u64,
    step: Step,
}

impl Retrieval {
    /// A retrieval the backend approved; the first action.
    pub fn start(geid: u64) -> (Self, Action) {
        (Self { geid, step: Step::Lowering }, Action::LowerPlatform)
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
            (Step::Lowering, PlatformState::OpenIdle) => {
                self.step = Step::Spawning;
                Some(Action::SpawnShip { geid: self.geid })
            }
            (Step::Raising, PlatformState::ClosedIdle) => {
                self.step = Step::Done;
                Some(Action::ShipReady { geid: self.geid })
            }
            _ => None,
        }
    }

    /// The ship's entity exists on the platform (`ok`), or could not be made.
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
}

#[tonic::async_trait]
impl ShipBackend for ShipsInternalClient<Channel> {
    async fn list(&self, request: ListShipsRequest) -> Result<Vec<ShipView>, Status> {
        Ok(self.clone().list_ships(request).await?.into_inner().ships)
    }
    async fn claim(&self, request: ClaimShipRequest) -> Result<ShipView, Status> {
        self.clone().claim_ship(request).await?.into_inner().ship.ok_or_else(|| Status::internal("no ship"))
    }
    async fn retrieve(&self, request: RetrieveShipRequest) -> Result<ShipView, Status> {
        self.clone().retrieve_ship(request).await?.into_inner().ship.ok_or_else(|| Status::internal("no ship"))
    }
    async fn report(&self, request: ReportShipRequest) -> Result<ShipView, Status> {
        self.clone().report_ship(request).await?.into_inner().ship.ok_or_else(|| Status::internal("no ship"))
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
    /// so a ship goes to the smallest that fits when the kiosk of that hangar is the one used (inferred).
    exact: bool,
    pub platform: Platform,
    /// Where the platform's changes go: the hangar's client. Without one (no known layout, or a test) the platform runs
    /// on its clock alone.
    link: Option<PlatformLink>,
    retrieval: Option<Retrieval>,
    commands: u64,
}

impl<B: ShipBackend> Kiosk<B> {
    pub fn new(backend: B, owner: u64, location_id: u32, hangar_class: &str, exact: bool) -> Self {
        let size = hangar_class::hangar_size_of(hangar_class);
        Self { backend, owner, location_id, size, exact, platform: Platform::default(), link: None, retrieval: None, commands: 0 }
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
    fn close_platform(&mut self, now: Duration) -> bool {
        let closed = self.platform.request_close(now);
        if closed {
            self.announce(PlatformStep::Close);
        }
        closed
    }

    /// The client is gone, or the retrieval failed past repair: the ship goes back to the bank and the platform to rest.
    /// Nothing is sent: the next join binds the hangar as it is. `false` when nothing was being retrieved.
    pub async fn abort(&mut self, now_ms: i64, request_id: u64) -> bool {
        let Some(retrieval) = self.retrieval.take().filter(|retrieval| !retrieval.done()) else { return false };
        let geid = retrieval.geid();
        warn!(owner = self.owner, geid, "ship retrieval abandoned: stored again");
        let command_id = self.command("abort", request_id);
        let request = ReportShipRequest { command_id, owner_id: self.owner, geid, state: ShipState::Stored as i32, location_id: self.location_id, now_ms };
        if let Err(status) = self.backend.report(request).await {
            warn!(geid, %status, "the ship stays deployed in its record until it is stored");
        }
        self.platform.reset();
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
            .list(ListShipsRequest { owner_id: self.owner, location_id: self.location_id, hangar_size: self.size as i32, exact_size: self.exact, now_ms })
            .await
    }

    pub async fn claim(&mut self, geid: u64, kind: ShipClaimKind, request_id: u64, now_ms: i64) -> Result<ShipView, Status> {
        let command_id = self.command("claim", request_id);
        self.backend.claim(ClaimShipRequest { command_id, owner_id: self.owner, geid, kind: kind as i32, now_ms }).await
    }

    /// Retrieve: the backend approves (the ship is stored and fits), then the platform lowers, the ship spawns on it and
    /// the platform raises it into the bay. The first action is returned; `platform_arrived` and `spawned` return the rest.
    pub async fn retrieve(&mut self, geid: u64, request_id: u64, now: Duration, now_ms: i64) -> Result<Vec<Action>, Status> {
        if self.retrieval.as_ref().is_some_and(|retrieval| !retrieval.done()) {
            return Err(Status::failed_precondition("a ship is being retrieved already"));
        }
        if self.platform.state() != PlatformState::ClosedIdle {
            return Err(Status::failed_precondition("the platform is moving"));
        }
        let command_id = self.command("retrieve", request_id);
        let request = RetrieveShipRequest { command_id, owner_id: self.owner, geid, hangar_size: self.size as i32, exact_size: self.exact, now_ms, ..Default::default() };
        self.backend.retrieve(request).await?;
        let (retrieval, action) = Retrieval::start(geid);
        self.open_platform(now);
        self.retrieval = Some(retrieval);
        info!(owner = self.owner, geid, "ship retrieval started");
        Ok(vec![action])
    }

    /// The platform's clock; what the retrieval asks next.
    pub fn tick(&mut self, now: Duration) -> Vec<Action> {
        let Some(state) = self.platform.tick(now) else { return Vec::new() };
        self.announce(if state == PlatformState::OpenIdle { PlatformStep::Opened } else { PlatformStep::Closed });
        let Some(retrieval) = self.retrieval.as_mut() else { return Vec::new() };
        let action = retrieval.platform(state);
        if matches!(action, Some(Action::RaisePlatform)) {
            self.close_platform(now);
        }
        action.into_iter().collect()
    }

    /// The spawn finished. A failure puts the ship back in the bank.
    pub async fn spawned(&mut self, ok: bool, now: Duration, now_ms: i64, request_id: u64) -> Vec<Action> {
        let Some(retrieval) = self.retrieval.as_mut() else { return Vec::new() };
        let action = retrieval.spawned(ok);
        match &action {
            Action::RaisePlatform => {
                self.close_platform(now);
            }
            Action::ReleaseShip { geid } => {
                let geid = *geid;
                warn!(owner = self.owner, geid, "the ship could not be spawned: stored again");
                let command_id = self.command("release", request_id);
                let request = ReportShipRequest { command_id, owner_id: self.owner, geid, state: ShipState::Stored as i32, location_id: self.location_id, now_ms };
                if let Err(status) = self.backend.report(request).await {
                    warn!(geid, %status, "the ship stays deployed in its record until it is stored");
                }
                self.close_platform(now);
            }
            _ => {}
        }
        vec![action]
    }

    /// A ship put away at this kiosk, or lost: the record and the graph follow.
    pub async fn report(&mut self, geid: u64, state: ShipState, request_id: u64, now_ms: i64) -> Result<ShipView, Status> {
        let command_id = self.command("report", request_id);
        self.backend.report(ReportShipRequest { command_id, owner_id: self.owner, geid, state: state as i32, location_id: self.location_id, now_ms }).await
    }
}

/// The hangar size of a class name, as sc-entitygraph reads it (`Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`).
mod hangar_class {
    use sc_api::internal::HangarSize;

    pub fn hangar_size_of(class_name: &str) -> HangarSize {
        let kind = class_name.strip_prefix("Hangar_").unwrap_or_default();
        [("Small", HangarSize::Small), ("Medium", HangarSize::Medium), ("Large", HangarSize::Large), ("XL", HangarSize::Xlarge)]
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
            let ships = ships.into_iter().map(|(geid, pad, state)| ShipRecord { geid, pad_size: pad as i32, state: state as i32, ..Default::default() }).collect();
            Self { ships: Mutex::new(ships), calls: Mutex::default() }
        }
    }

    fn fits(pad: PadSize, size: i32, exact: bool) -> bool {
        if exact { (pad as i32).max(3) == size } else { pad as i32 <= size }
    }

    #[tonic::async_trait]
    impl ShipBackend for &Fake {
        async fn list(&self, request: ListShipsRequest) -> Result<Vec<ShipView>, Status> {
            self.calls.lock().unwrap().push(format!("list {}", request.hangar_size));
            let ships = self.ships.lock().unwrap();
            Ok(ships.iter().filter(|ship| fits(ship.pad_size(), request.hangar_size, request.exact_size)).map(|ship| ShipView { ship: Some(ship.clone()), fits: true, ..Default::default() }).collect())
        }
        async fn claim(&self, request: ClaimShipRequest) -> Result<ShipView, Status> {
            self.calls.lock().unwrap().push(format!("claim {} {}", request.geid, request.command_id));
            Ok(ShipView::default())
        }
        async fn retrieve(&self, request: RetrieveShipRequest) -> Result<ShipView, Status> {
            self.calls.lock().unwrap().push(format!("retrieve {}", request.geid));
            let mut ships = self.ships.lock().unwrap();
            let ship = ships.iter_mut().find(|ship| ship.geid == request.geid).ok_or_else(|| Status::not_found("no ship"))?;
            if ship.state() != ShipState::Stored || !fits(ship.pad_size(), request.hangar_size, request.exact_size) {
                return Err(Status::failed_precondition("not retrievable here"));
            }
            ship.state = ShipState::Deployed as i32;
            Ok(ShipView { ship: Some(ship.clone()), ..Default::default() })
        }
        async fn report(&self, request: ReportShipRequest) -> Result<ShipView, Status> {
            self.calls.lock().unwrap().push(format!("report {} {}", request.geid, request.state));
            let mut ships = self.ships.lock().unwrap();
            if let Some(ship) = ships.iter_mut().find(|ship| ship.geid == request.geid) {
                ship.state = request.state;
            }
            Ok(ShipView::default())
        }
    }

    const T0: Duration = Duration::from_secs(100);

    #[test]
    fn the_platform_drops_requests_in_the_wrong_state() {
        let mut platform = Platform::default();
        assert!(!platform.request_close(T0), "closed already");
        assert!(platform.request_open(T0));
        assert!(!platform.request_open(T0), "already lowering");
        assert_eq!(platform.tick(T0 + PLATFORM_OPEN_TIME - Duration::from_millis(1)), None);
        assert_eq!(platform.tick(T0 + PLATFORM_OPEN_TIME), Some(PlatformState::OpenIdle));
        assert!(platform.request_close(T0 + PLATFORM_OPEN_TIME));
        assert_eq!(platform.state(), PlatformState::RaisingPlatform);
        assert_eq!(platform.tick(T0 + PLATFORM_OPEN_TIME + PLATFORM_CLOSE_TIME), Some(PlatformState::ClosedIdle));
    }

    #[test]
    fn hangar_sizes_come_from_the_class() {
        assert_eq!(hangar_class::hangar_size_of("Hangar_SmallFront_Orison"), HangarSize::Small);
        assert_eq!(hangar_class::hangar_size_of("Hangar_XLTop_Area18"), HangarSize::Xlarge);
        assert_eq!(hangar_class::hangar_size_of("Elsewhere"), HangarSize::Unspecified);
    }

    #[tokio::test]
    async fn the_kiosk_lists_only_the_ships_that_fit_its_hangar() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored), (2, PadSize::Medium, ShipState::Stored), (3, PadSize::Large, ShipState::Stored)]);
        let small = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        assert_eq!(small.list(0).await.unwrap().iter().map(|view| view.ship.as_ref().unwrap().geid).collect::<Vec<_>>(), [1]);
        let large = Kiosk::new(&fake, 7, 9, "Hangar_LargeTop_Orison", false);
        assert_eq!(large.list(0).await.unwrap().len(), 3);
        let exact = Kiosk::new(&fake, 7, 9, "Hangar_LargeTop_Orison", true);
        assert_eq!(exact.list(0).await.unwrap().len(), 1);
        let unknown = Kiosk::new(&fake, 7, 9, "Lobby", false);
        assert!(unknown.list(0).await.unwrap().is_empty());
        assert_eq!(fake.calls.lock().unwrap().len(), 3, "an unknown hangar asks nothing");
    }

    #[tokio::test]
    async fn a_retrieval_lowers_the_platform_spawns_the_ship_and_raises_it() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored), (3, PadSize::Large, ShipState::Stored)]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        let refused = kiosk.retrieve(3, 1, T0, 0).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "a large ship does not come out of a small hangar");
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle, "nothing moved");

        assert_eq!(kiosk.retrieve(1, 2, T0, 0).await.unwrap(), [Action::LowerPlatform]);
        assert_eq!(kiosk.platform.state(), PlatformState::LoweringPlatform);
        assert_eq!(kiosk.retrieve(1, 3, T0, 0).await.unwrap_err().code(), tonic::Code::FailedPrecondition, "one at a time");
        assert!(kiosk.tick(T0 + Duration::from_secs(1)).is_empty());
        assert_eq!(kiosk.tick(T0 + PLATFORM_OPEN_TIME), [Action::SpawnShip { geid: 1 }]);
        let raising = kiosk.spawned(true, T0 + PLATFORM_OPEN_TIME, 0, 4).await;
        assert_eq!(raising, [Action::RaisePlatform]);
        assert_eq!(kiosk.platform.state(), PlatformState::RaisingPlatform);
        assert_eq!(kiosk.tick(T0 + PLATFORM_OPEN_TIME + PLATFORM_CLOSE_TIME), [Action::ShipReady { geid: 1 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Deployed);
        assert_eq!(kiosk.platform.state(), PlatformState::ClosedIdle);
    }

    #[tokio::test]
    async fn a_ship_that_cannot_be_spawned_goes_back_to_the_bank() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        kiosk.tick(T0 + PLATFORM_OPEN_TIME);
        let actions = kiosk.spawned(false, T0 + PLATFORM_OPEN_TIME, 0, 2).await;
        assert_eq!(actions, [Action::ReleaseShip { geid: 1 }]);
        assert_eq!(fake.ships.lock().unwrap()[0].state(), ShipState::Stored);
        assert!(kiosk.platform.request_close(T0 + PLATFORM_OPEN_TIME) || kiosk.platform.state() == PlatformState::RaisingPlatform);
    }

    fn linked(fake: &Fake) -> (Kiosk<&Fake>, tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>) {
        use crate::platform::{Indices, PlatformIds};
        let outbox = crate::outbox::Outbox::default();
        let receiver = outbox.subscribe((1, 1));
        let link = PlatformLink::new(outbox, (1, 1), PlatformIds { manager: 80, platform: 84 }).with_indices(Indices { current: Some(1), planned: Some(2), door: Some(3) });
        (Kiosk::new(fake, 7, 9, "Hangar_SmallFront_Orison", false).with_link(link), receiver)
    }

    fn drain(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>) -> Vec<(u64, u32, u8)> {
        use sc_api::internal::session_update::Update;
        let mut sent = Vec::new();
        while let Ok(Ok(update)) = receiver.try_recv() {
            let Some(Update::Variables(variables)) = update.update else { panic!("not a variable update") };
            sent.push((variables.entity_id, variables.variables[0].index, variables.variables[0].value[0]));
        }
        sent
    }

    #[tokio::test]
    async fn a_retrieval_sends_the_platforms_door_and_manager_states_in_order() {
        let fake = Fake::with(vec![(1, PadSize::Xsmall, ShipState::Stored)]);
        let (mut kiosk, mut receiver) = linked(&fake);
        kiosk.retrieve(1, 1, T0, 0).await.unwrap();
        assert_eq!(drain(&mut receiver), [(84, 3, 5), (80, 1, 2), (80, 2, 3)], "door opening, manager 2 -> 3");
        assert!(kiosk.tick(T0 + PLATFORM_OPEN_TIME - Duration::from_millis(1)).is_empty());
        assert!(drain(&mut receiver).is_empty(), "nothing before the measured time");
        kiosk.tick(T0 + PLATFORM_OPEN_TIME);
        assert_eq!(drain(&mut receiver), [(84, 3, 3), (80, 1, 3)], "door opened, manager OpenIdle");
        kiosk.spawned(true, T0 + PLATFORM_OPEN_TIME, 0, 2).await;
        assert_eq!(drain(&mut receiver), [(84, 3, 2), (80, 1, 6), (80, 2, 7)], "door closing, manager 6 -> 7");
        kiosk.tick(T0 + PLATFORM_OPEN_TIME + PLATFORM_CLOSE_TIME);
        assert_eq!(drain(&mut receiver), [(84, 3, 0), (80, 1, 7)], "door closed, manager ClosedIdle");
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
        kiosk.retrieve(1, 3, T0, 0).await.expect("a new retrieval may start");
    }

    #[tokio::test]
    async fn a_claim_carries_the_clients_request_id() {
        let fake = Fake::default();
        let mut kiosk = Kiosk::new(&fake, 7, 9, "Hangar_SmallFront_Orison", false);
        kiosk.claim(5, ShipClaimKind::Insured, 42, 0).await.unwrap();
        kiosk.claim(5, ShipClaimKind::Insured, 42, 0).await.unwrap();
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0], calls[1], "a retry has the same command id, so sc-entitygraph applies it once");
    }
}

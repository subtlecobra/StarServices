//! The hangar elevators this DGS runs: one `sim::Line` per Transit manager that serves a hangar of a player it holds,
//! and what the line's commands become for the clients.
//!
//! A player's hangar is a stop of its manager's line from its join to its leave. A press of "Call elevator" on one of
//! its call panels is the line's `Call`. The line decides the carriages; this module binds a carriage to the client of
//! the hangar it is at or going to, moves it between the gateway and the limbo node below it, and opens and closes the
//! gateway's door. A carriage at a stop no client of this DGS has, such as the spaceport's lobby, which is not bound
//! yet, is unbound. How each of those is written on the wire is `super::wire`'s.
//!
//! Not done yet, and logged where it would happen: boarding (`Board` and `Exit` need the player's zone), rides to the
//! lobby (`Load` of a persistent location), moving a rider between carriages (`MoveRider`) and saving a rider who left
//! (`Save`).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, Instant},
};

use sc_api::internal::{
    session_update::Update, EntityBind, EntityBinds, EntityUnbinds, EntityVariables,
};
use tracing::{debug, info, warn};

use super::{
    sim::{
        CarriageId, CarriageState, Command, Event, GatewayRef, Line, LineSpec, Location, Place,
        PlayerId, Settings, Stop,
    },
    tables, wire, Owner, PrefabEntity, System,
};
use crate::{
    entity::Transform,
    location::{GatewayPrefab, HomeHangar},
    outbox::{Outbox, SessionKey},
};

/// Where a hangar's gateway is matched with its prefab's entities: the gateway entity of the table and of the prefab are
/// the same entity, so they are within rounding of each other.
const SAME_PLACE: f64 = 0.05;
/// Carriage entities take ids from here, above the hangars, a block of `CARRIAGES_PER_LINE` per line, each carriage
/// followed by the ids of its cab's content.
const FIRST_CARRIAGE_ID: u64 = 230_000_000_000;
const CARRIAGES_PER_LINE: u64 = 64;
const CARRIAGE_STRIDE: u64 = 1 + wire::CAB_IDS;

/// A gateway of a hangar, with the entities of its prefab the DGS binds.
#[derive(Clone, Debug)]
struct HangarGateway {
    transform: Transform,
    door: Option<u64>,
    /// The door's bind as the join sent it, to bind it again with another state.
    door_bind: Option<EntityBind>,
    panel: Option<u64>,
    /// Where a carriage waits below the door.
    limbo: Transform,
}

/// A player's hangar as a stop of its manager's line.
struct HangarStop {
    /// The account whose hangar it is; the stop is keyed by `owner` for the home hangar, by the instance's entity id for any other.
    owner: PlayerId,
    location_id: u32,
    /// The player's home hangar, which a leave closes along with the player's place on the line.
    home: bool,
    manager: Owner,
    hangar_id: u64,
    session: SessionKey,
    gateways: Vec<HangarGateway>,
    /// The hangar's class name, for the size its panel entry is labelled with.
    class: String,
    /// The players besides the owner whose panels offer this stop and who may ride to it: the instance manager's grant,
    /// after its relation and access rules (`allow_guest`).
    guests: BTreeSet<PlayerId>,
}

/// A hangar a cab panel offers a rider: its stop, the entry's label and the name of the hangar's owner.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PanelHangar {
    stop: PlayerId,
    label: String,
    owner: String,
}

/// A carriage's entity, and the hangar whose client has it bound.
struct CarriageEntity {
    entity_id: u64,
    shown: Option<PlayerId>,
    /// Where it was last placed for that client.
    transform: Option<Transform>,
    /// Its open doors: its cab's and the gateway's.
    opened: Vec<u64>,
    /// What its cab panel's points mean, as the panel was last bound: a stop to pick, or `None` for the doors.
    points: Vec<(u32, Option<Stop>)>,
}

struct LineRun {
    line: Line,
    name: String,
    kind: wire::CarriageKind,
    /// The entities of its carriages' cab, which the DGS binds after each carriage's id, in index order (the client makes
    /// them itself with `CabStatics::Client`, see `bound`).
    cab: Vec<PrefabEntity>,
    cab_statics: wire::CabStatics,
    /// The manager's destinations a rider can pick, by label (its gateways' `nameIdentifier`) and index.
    destinations: Vec<Destination>,
    first_id: u64,
    carriages: BTreeMap<CarriageId, CarriageEntity>,
}

impl LineRun {
    /// The cab entities the DGS binds itself, with their ids for the carriage `carriage_id`. With the client making the
    /// cab from the carriage's `ocIdRange`, only the cab panel, at `carriage_id + CAB_IDS`: an id past the range (and the
    /// next carriage's own is `CARRIAGE_STRIDE` away), as one inside it would collide with the client's copy.
    fn bound(&self, carriage_id: u64) -> Vec<(u64, &PrefabEntity)> {
        match self.cab_statics {
            wire::CabStatics::Server => self
                .cab
                .iter()
                .map(|cab| (carriage_id + 1 + cab.index, cab))
                .collect(),
            wire::CabStatics::Merge => self
                .cab
                .iter()
                .filter(|cab| cab.class_crc == CAB_PANEL_CLASS)
                .map(|cab| (carriage_id + 1 + cab.index, cab))
                .take(1)
                .collect(),
            wire::CabStatics::Client => self
                .cab
                .iter()
                .filter(|cab| cab.class_crc == CAB_PANEL_CLASS)
                .map(|cab| (carriage_id + wire::CAB_IDS, cab))
                .take(1)
                .collect(),
        }
    }

    /// A bound cab entity as the mode binds it: merged into the client's statics, or on its own.
    fn cab_bind(
        &self,
        id: u64,
        carriage_id: u64,
        cab: &PrefabEntity,
        extra: Vec<crate::entity::Snapshot>,
    ) -> crate::entity::Bind {
        match self.cab_statics {
            wire::CabStatics::Merge => wire::cab_merged_bind(id, carriage_id, cab, extra),
            _ => wire::cab_entity_bind(id, carriage_id, cab, extra),
        }
    }

    /// The id of a carriage's cab panel as the DGS binds it.
    fn panel_id(&self, carriage_id: u64) -> Option<u64> {
        self.bound(carriage_id)
            .into_iter()
            .find(|(_, cab)| cab.class_crc == CAB_PANEL_CLASS)
            .map(|(id, _)| id)
    }

    /// How many statics the client makes from the carriage's `ocIdRange`.
    fn client_statics(&self) -> u64 {
        if self.cab_statics == wire::CabStatics::Server {
            0
        } else {
            self.cab.len() as u64
        }
    }
}

pub struct Elevators {
    lines: HashMap<Owner, LineRun>,
    /// By owner, the player whose hangar it is.
    hangars: HashMap<PlayerId, HangarStop>,
    outbox: Outbox,
    /// When the server started, for the clients' times.
    started: Instant,
    /// The players whose zone is a carriage, and that carriage.
    aboard: HashMap<PlayerId, (Owner, CarriageId)>,
    /// The in-place variable indices set, see `wire::Indices`; none by default.
    indices: wire::Indices,
    cab_statics: wire::CabStatics,
    /// In-place door states still to send: the echo that follows an animation's first state.
    pending: Vec<PendingDoor>,
    /// The names panels show for the owners of the hangars they list, by player.
    names: HashMap<PlayerId, String>,
}

/// How long a cab door's animation takes before its end state is sent as an echo.
const DOOR_ANIMATION: Duration = Duration::from_secs(5);

/// The end state of a cab door's animation, sent once it ran.
struct PendingDoor {
    due: Instant,
    owner: PlayerId,
    carriage_entity: u64,
    door: u64,
    state: u8,
}

fn distance(a: &Transform, b: &Transform) -> f64 {
    a.position
        .iter()
        .zip(b.position)
        .map(|(a, b)| (a - b) * (a - b))
        .sum::<f64>()
        .sqrt()
}

/// Below a gateway by the depth of the prefabs' limbo nodes, for a gateway whose prefab has none.
fn below(gateway: &Transform) -> Transform {
    let [x, y, z] = gateway.position;
    Transform {
        position: [x, y, z - 6.0],
        rotation: gateway.rotation,
    }
}

impl Elevators {
    /// Elevators with the in-place indices `SC_DGS_TRANSIT_INDICES` sets, none by default.
    pub fn new(outbox: Outbox) -> Self {
        Self::with_indices(outbox, wire::Indices::from_environment())
            .with_cab_statics(wire::CabStatics::from_environment())
    }

    /// Who makes the cab's entities, `SC_DGS_TRANSIT_CAB_STATICS`: the client by default (`new`), the DGS (`server`) with `with_indices` alone.
    pub fn with_cab_statics(mut self, cab_statics: wire::CabStatics) -> Self {
        self.cab_statics = cab_statics;
        self
    }

    pub fn with_indices(outbox: Outbox, indices: wire::Indices) -> Self {
        Self {
            lines: HashMap::new(),
            hangars: HashMap::new(),
            outbox,
            started: Instant::now(),
            aboard: HashMap::new(),
            indices,
            cab_statics: wire::CabStatics::Server,
            pending: Vec::new(),
            names: HashMap::new(),
        }
    }

    /// A held player's hangar becomes a stop of the line of the manager that serves it, if it is a Transit one. The stop
    /// is keyed by `owner`.
    pub fn open_hangar(&mut self, owner: PlayerId, home: &HomeHangar, session: SessionKey) {
        self.open_stop(owner, owner, home, session, true);
    }

    /// A hangar instance other than the home one (`hangar_slots`) becomes a stop too, for the owner's `session`. Its stop is
    /// keyed by the instance's entity id (from `hangar_slots::POOL_FIRST_ID`, above any home hangar and below the
    /// carriages), which is returned for `call` and `close_hangar`; `None` when the hangar has no Transit elevator.
    pub fn open_instance(
        &mut self,
        owner: PlayerId,
        instance: &HomeHangar,
        session: SessionKey,
    ) -> Option<PlayerId> {
        let stop = instance.entity_id;
        self.open_stop(stop, owner, instance, session, false)
            .then_some(stop)
    }

    /// The stop of `owner`'s hangar at a location, home or instance.
    pub fn stop_of(&self, owner: PlayerId, location_id: u32) -> Option<PlayerId> {
        self.hangars
            .iter()
            .find(|(_, hangar)| hangar.owner == owner && hangar.location_id == location_id)
            .map(|(stop, _)| *stop)
    }

    /// The stop a call panel belongs to.
    pub fn stop_of_panel(&self, panel: u64) -> Option<PlayerId> {
        self.hangars
            .iter()
            .find(|(_, hangar)| {
                hangar
                    .gateways
                    .iter()
                    .any(|gateway| gateway.panel == Some(panel))
            })
            .map(|(stop, _)| *stop)
    }

    /// The stops `session` holds: the home one and the owner's instances.
    pub fn stops_of(&self, session: SessionKey) -> Vec<PlayerId> {
        let mut stops: Vec<PlayerId> = self
            .hangars
            .iter()
            .filter(|(_, hangar)| hangar.session == session)
            .map(|(stop, _)| *stop)
            .collect();
        stops.sort_unstable();
        stops
    }

    /// The name the panels show for `player`'s hangars (the handle the client shows as the player's name).
    pub fn set_player_name(&mut self, player: PlayerId, name: &str) {
        if !name.is_empty() {
            self.names.insert(player, name.to_string());
        }
    }

    /// The instance manager lets `guest` ride to `stop`: its panels list the stop, and a pick of it is granted. The caller has
    /// applied the access rules (`hangar_access`, `hangar_relation`); the owner needs no grant. `false` for no such stop.
    pub fn allow_guest(&mut self, stop: PlayerId, guest: PlayerId) -> bool {
        let Some(hangar) = self.hangars.get_mut(&stop) else {
            return false;
        };
        hangar.guests.insert(guest);
        true
    }

    /// `guest` may no longer ride to `stop`.
    pub fn deny_guest(&mut self, stop: PlayerId, guest: PlayerId) {
        if let Some(hangar) = self.hangars.get_mut(&stop) {
            hangar.guests.remove(&guest);
        }
    }

    /// What `viewer`'s cab panel lists of the hangars on `manager`'s line (see `panel_hangars`).
    #[cfg(test)]
    fn listed_for(&self, manager: &Owner, viewer: PlayerId) -> Vec<PanelHangar> {
        panel_hangars(&self.hangars, &self.names, manager, viewer)
    }

    fn open_stop(
        &mut self,
        stop: PlayerId,
        owner: PlayerId,
        home: &HomeHangar,
        session: SessionKey,
        is_home: bool,
    ) -> bool {
        self.close_hangar(stop);
        let tables = tables();
        let Some(hangar) = tables.hangar(home.location.location_id, home.location.hangar_class_crc)
        else {
            return false;
        };
        if hangar.system != System::Transit {
            debug!(owner, system = ?hangar.system, "a hangar with no Transit elevator");
            return false;
        }
        let Some(manager) = tables.manager(&hangar.manager) else {
            return false;
        };
        let prefabs = home.gateway_prefabs();
        let mut gateways: Vec<HangarGateway> = tables
            .stop_gateways(hangar)
            .iter()
            .map(|gateway| match_prefab(&gateway.transform, &prefabs))
            .collect();
        let content: Vec<EntityBind> = home
            .content_binds()
            .into_iter()
            .map(EntityBind::from)
            .collect();
        for gateway in &mut gateways {
            gateway.door_bind = gateway
                .door
                .and_then(|door| content.iter().find(|bind| bind.entity_id == door))
                .cloned();
        }
        let doors = gateways
            .iter()
            .filter(|gateway| gateway.door.is_some())
            .count();
        info!(owner, hangar = home.entity_id, manager = %manager.name, gateways = gateways.len(), doors, "hangar elevator stop opened");
        let count = gateways.len();
        let line_count = self.lines.len() as u64;
        self.lines
            .entry(manager.owner.clone())
            .or_insert_with(|| LineRun {
                line: Line::new(
                    LineSpec::from_manager(manager),
                    Settings {
                        door_open: DOOR_ANIMATION,
                        ..Settings::default()
                    }
                    .with_manager(manager),
                ),
                name: manager.name.clone(),
                cab: tables.prefab_entities(manager.cab_socpak_hash).to_vec(),
                cab_statics: self.cab_statics,
                destinations: destinations(manager),
                kind: wire::CarriageKind {
                    class_crc: manager.carriage_class_crc,
                    cab_socpak_hash: manager.cab_socpak_hash,
                    manager_name: manager.name.clone(),
                },
                first_id: FIRST_CARRIAGE_ID + line_count * CARRIAGES_PER_LINE * CARRIAGE_STRIDE,
                carriages: BTreeMap::new(),
            });
        self.hangars.insert(
            stop,
            HangarStop {
                owner,
                location_id: home.location.location_id,
                home: is_home,
                manager: manager.owner.clone(),
                hangar_id: home.entity_id,
                session,
                gateways,
                class: home.location.hangar_class.clone(),
                guests: BTreeSet::new(),
            },
        );
        self.handle(
            stop,
            Event::HangarOpened {
                owner: stop,
                gateways: count,
            },
        );
        self.handle(
            stop,
            Event::Ready {
                player: owner,
                location: Location::Hangar(stop),
            },
        );
        true
    }

    /// A hangar stop closes: its carriages go. A player's home hangar closes when the player leaves, and so does the
    /// player as a rider; an instance leaves the owner on the line (they may be elsewhere on it).
    pub fn close_hangar(&mut self, stop: PlayerId) {
        let Some(home) = self
            .hangars
            .get(&stop)
            .map(|hangar| (hangar.home, hangar.owner))
        else {
            return;
        };
        self.handle(stop, Event::HangarClosed { owner: stop });
        if home.0 {
            self.handle(stop, Event::Disconnected { player: home.1 });
        }
        let manager = self.hangars.remove(&stop).map(|hangar| hangar.manager);
        if let Some(run) = manager.and_then(|manager| self.lines.get_mut(&manager)) {
            for carriage in run
                .carriages
                .values_mut()
                .filter(|carriage| carriage.shown == Some(stop))
            {
                carriage.shown = None;
                carriage.transform = None;
            }
        }
        info!(stop, "hangar elevator stop closed");
    }

    /// The home hangar a session holds, if any.
    pub fn owner_of(&self, session: SessionKey) -> Option<PlayerId> {
        self.hangars
            .iter()
            .find(|(_, hangar)| hangar.home && hangar.session == session)
            .map(|(owner, _)| *owner)
    }

    /// A press of "Call elevator" on a call panel of `owner`'s hangar. `false` for a panel of no gateway of it.
    pub fn call(&mut self, owner: PlayerId, player: PlayerId, panel: u64) -> bool {
        let Some(index) = self.hangars.get(&owner).and_then(|hangar| {
            hangar
                .gateways
                .iter()
                .position(|gateway| gateway.panel == Some(panel))
        }) else {
            warn!(
                owner,
                panel, "a call from a panel of no gateway of the hangar"
            );
            return false;
        };
        self.handle(
            owner,
            Event::Call {
                player,
                gateway: GatewayRef {
                    stop: Stop::Hangar(owner),
                    gateway: index,
                },
            },
        );
        true
    }

    /// Everything due by now, on every line.
    pub fn step(&mut self, now: Instant) {
        self.send_due_doors(now);
        let managers: Vec<Owner> = self.lines.keys().cloned().collect();
        for manager in managers {
            let Some(run) = self.lines.get_mut(&manager) else {
                continue;
            };
            let commands = run.line.tick(now);
            self.apply(&manager, now, commands);
        }
    }

    /// An event of the line that serves `owner`'s hangar, and the commands it leads to.
    fn handle(&mut self, owner: PlayerId, event: Event) {
        let Some(manager) = self
            .hangars
            .get(&owner)
            .map(|hangar| hangar.manager.clone())
        else {
            return;
        };
        self.handle_line(&manager, event);
    }

    fn handle_line(&mut self, manager: &Owner, event: Event) {
        let now = Instant::now();
        let Some(run) = self.lines.get_mut(manager) else {
            return;
        };
        let commands = run.line.handle(now, event);
        self.apply(manager, now, commands);
    }

    /// The carriages bound to a client, which are zones its player may step into.
    pub fn carriage_zones(&self) -> Vec<u64> {
        self.lines
            .values()
            .flat_map(|run| {
                run.carriages
                    .values()
                    .filter(|entity| entity.shown.is_some())
                    .map(|entity| entity.entity_id)
            })
            .collect()
    }

    /// The players' zones, as their movement has them: a player whose zone is a carriage has boarded it, and one whose
    /// zone no longer is has left it. Riders keep their carriage's doors and the carriage itself.
    pub fn observe(&mut self, zones: &[(PlayerId, u64)]) {
        let carriages: HashMap<u64, (Owner, CarriageId)> = self
            .lines
            .iter()
            .flat_map(|(manager, run)| {
                run.carriages
                    .iter()
                    .map(move |(id, entity)| (entity.entity_id, (manager.clone(), *id)))
            })
            .collect();
        for (player, zone) in zones {
            let now_in = carriages.get(zone).cloned();
            let was = self.aboard.get(player).cloned();
            if now_in == was {
                continue;
            }
            if let Some((manager, carriage)) = was {
                self.aboard.remove(player);
                info!(player, carriage = carriage.0, "rider left the carriage");
                self.handle_line(
                    &manager,
                    Event::Exit {
                        player: *player,
                        carriage,
                    },
                );
            }
            if let Some((manager, carriage)) = now_in {
                self.aboard.insert(*player, (manager.clone(), carriage));
                info!(player, carriage = carriage.0, "rider boarded the carriage");
                self.handle_line(
                    &manager,
                    Event::Board {
                        player: *player,
                        carriage,
                    },
                );
            }
        }
    }

    /// Whether any player is aboard the carriage: such a carriage is never bound again, which would take it from under
    /// them.
    fn has_riders(&self, manager: &Owner, carriage: CarriageId) -> bool {
        self.aboard
            .values()
            .any(|(line, id)| line == manager && *id == carriage)
    }

    fn apply(&mut self, manager: &Owner, now: Instant, commands: Vec<Command>) {
        let mut follow_up = Vec::new();
        for command in commands {
            debug!(?command, "elevator command");
            match command {
                Command::Create { carriage, place } => {
                    let Some(run) = self.lines.get_mut(manager) else {
                        continue;
                    };
                    let entity_id = run.first_id + carriage.0 as u64 * CARRIAGE_STRIDE;
                    run.carriages.insert(
                        carriage,
                        CarriageEntity {
                            entity_id,
                            shown: None,
                            transform: None,
                            opened: Vec::new(),
                            points: Vec::new(),
                        },
                    );
                    info!(line = %run.name, carriage = carriage.0, entity_id, ?place, "carriage created");
                    let (gateway, present) = match place {
                        Place::Gateway(gateway) => (Some(gateway), true),
                        Place::Limbo(stop) => (Some(GatewayRef { stop, gateway: 0 }), false),
                    };
                    self.show(manager, carriage, gateway, present);
                }
                Command::State {
                    carriage,
                    state,
                    at,
                    target,
                    ..
                } => {
                    info!(
                        carriage = carriage.0,
                        ?state,
                        ?at,
                        ?target,
                        "carriage state"
                    );
                    let present = at.is_some()
                        && matches!(state, CarriageState::Present | CarriageState::Idle);
                    self.show(manager, carriage, at.or(target), present);
                    if present {
                        self.arrived(manager, carriage);
                    }
                    if let Some(entity_id) = self
                        .lines
                        .get(manager)
                        .and_then(|run| run.carriages.get(&carriage))
                        .map(|carriage| carriage.entity_id)
                    {
                        let in_transit = matches!(
                            state,
                            CarriageState::Travelling | CarriageState::WaitingInLimbo
                        );
                        self.to_viewer(manager, carriage, |owner| {
                            (owner, wire::carriage_state(entity_id, in_transit))
                        });
                    }
                }
                Command::OpenDoors { carriage, gateway } => {
                    self.doors(manager, carriage, gateway, true)
                }
                Command::CloseDoors { carriage, gateway } => {
                    self.doors(manager, carriage, gateway, false)
                }
                Command::Remove { carriage } => {
                    self.show(manager, carriage, None, false);
                    self.aboard
                        .retain(|_, (line, id)| !(line == manager && *id == carriage));
                    if let Some(run) = self.lines.get_mut(manager) {
                        run.carriages.remove(&carriage);
                        info!(line = %run.name, carriage = carriage.0, "carriage removed");
                    }
                }
                Command::Load {
                    player,
                    location,
                    resend,
                } => match location {
                    // The owner rides to their own stops; a guest rides to those the instance manager granted (`allow_guest`).
                    Location::Hangar(stop)
                        if self.hangars.get(&stop).is_some_and(|hangar| {
                            hangar.owner == player || hangar.guests.contains(&player)
                        }) =>
                    {
                        follow_up.push((stop, Event::Ready { player, location }));
                    }
                    _ => warn!(
                        player,
                        ?location,
                        resend,
                        "a rider needs a location this DGS does not send yet"
                    ),
                },
                Command::Unload { player, location } => {
                    debug!(player, ?location, "location no longer needed")
                }
                Command::MoveRider { player, from, to } => warn!(
                    player,
                    from = from.0,
                    to = to.0,
                    "moving a rider between carriages is not done yet"
                ),
                // This DGS keeps no position of a player: they spawn in their hangar at every join, so a rider who leaves
                // aboard is already "saved" outside the gateway's door. A DGS that saves positions saves `stop` here.
                Command::Save {
                    player,
                    stop,
                    gateway,
                } => debug!(
                    player,
                    ?stop,
                    ?gateway,
                    "a rider left aboard, spawns in the hangar next time"
                ),
            }
        }
        let _ = now;
        for (owner, event) in follow_up {
            self.handle(owner, event);
        }
    }

    /// A carriage at rest at a stop of the client it is bound to: its cab panel's current floor is that stop, written in
    /// place when `tdpCurrentStop`'s index is set (`SC_DGS_TRANSIT_INDICES`); a bind carries it otherwise (`carriage_binds`).
    fn arrived(&self, manager: &Owner, carriage: CarriageId) {
        let Some(run) = self.lines.get(manager) else {
            return;
        };
        let Some((entity, stop)) = run
            .carriages
            .get(&carriage)
            .and_then(|entity| entity.shown.map(|stop| (entity, stop)))
        else {
            return;
        };
        let (Some(panel), Some(hangar)) = (run.panel_id(entity.entity_id), self.hangars.get(&stop))
        else {
            return;
        };
        let listed = panel_hangars(&self.hangars, &self.names, manager, hangar.owner);
        let index = panel_entries(run, &listed, Some(stop)).current;
        if let Some(variables) = wire::current_stop_update(panel, index, &self.indices) {
            self.outbox
                .send(hangar.session, Update::Variables(variables));
        }
    }

    /// Sends `make(owner)`'s update to the client of the hangar the carriage is bound to, if any.
    fn to_viewer(
        &self,
        manager: &Owner,
        carriage: CarriageId,
        make: impl FnOnce(PlayerId) -> (PlayerId, Option<EntityVariables>),
    ) {
        let Some(owner) = self
            .lines
            .get(manager)
            .and_then(|run| run.carriages.get(&carriage))
            .and_then(|carriage| carriage.shown)
        else {
            return;
        };
        let (owner, variables) = make(owner);
        if let (Some(variables), Some(hangar)) = (variables, self.hangars.get(&owner)) {
            self.outbox
                .send(hangar.session, Update::Variables(variables));
        }
    }

    /// Binds the carriage, with its cab's entities, to the client of the hangar `gateway` is of, at the gateway or in limbo
    /// below it, moves it there if it is bound already, or unbinds it from the client that has it when it goes where no
    /// client of this DGS is.
    fn show(
        &mut self,
        manager: &Owner,
        carriage: CarriageId,
        gateway: Option<GatewayRef>,
        present: bool,
    ) {
        let place = gateway.and_then(|gateway| match gateway.stop {
            Stop::Hangar(owner) => {
                let hangar = self.hangars.get(&owner)?;
                let at = hangar.gateways.get(gateway.gateway)?;
                Some((
                    owner,
                    hangar.hangar_id,
                    hangar.session,
                    if present { at.transform } else { at.limbo },
                ))
            }
            Stop::Destination(_) => None,
        });
        let since = self.started;
        let riders = self.has_riders(manager, carriage);
        let listed = place
            .map(|(stop, ..)| {
                panel_hangars(
                    &self.hangars,
                    &self.names,
                    manager,
                    self.hangars.get(&stop).map_or(stop, |hangar| hangar.owner),
                )
            })
            .unwrap_or_default();
        let Some(run) = self.lines.get_mut(manager) else {
            return;
        };
        let time = Instant::now().duration_since(since);
        let Some(entity) = run.carriages.get(&carriage) else {
            return;
        };
        let ids = carriage_ids(entity.entity_id, &run);
        if let Some(old) = entity
            .shown
            .filter(|old| place.is_none_or(|(owner, ..)| owner != *old))
        {
            if let Some(hangar) = self.hangars.get(&old) {
                self.outbox.send(
                    hangar.session,
                    Update::Unbinds(EntityUnbinds {
                        entity_ids: ids.clone(),
                    }),
                );
            }
            let entity = run.carriages.get_mut(&carriage).expect("present");
            entity.shown = None;
            entity.transform = None;
        }
        let Some((owner, hangar_id, session, transform)) = place else {
            return;
        };
        let entity = run.carriages.get(&carriage).expect("present");
        let at = wire::CarriageAt {
            hangar_id,
            transform,
            time,
            opened_doors: entity.opened.clone(),
        };
        let (shown, last) = (entity.shown, entity.transform);
        if shown.is_none() {
            let (binds, points) = carriage_binds(run, carriage, &listed, Some(owner), &at);
            self.outbox.send(
                session,
                Update::Binds(EntityBinds {
                    key: format!("carriage:{}", binds[0].entity_id),
                    binds,
                }),
            );
            Self::resend_panel(
                &self.outbox,
                &self.indices,
                run,
                run.carriages[&carriage].entity_id,
                &listed,
                Some(owner),
                session,
            );
            let entity = run.carriages.get_mut(&carriage).expect("present");
            entity.shown = Some(owner);
            entity.points = points;
            info!(
                entity = entity.entity_id,
                owner,
                present,
                cab = run.cab.len(),
                "carriage bound to the hangar's client"
            );
        } else if last != Some(transform) {
            match wire::carriage_moved(entity.entity_id, hangar_id, transform, time) {
                Some(variables) => self.outbox.send(session, Update::Variables(variables)),
                None if riders => warn!(
                    entity = entity.entity_id,
                    "a carriage with riders cannot move until its position's index is known"
                ),
                None => {
                    // With no index for its position, the carriage is bound again where it now is, with its cab.
                    self.outbox
                        .send(session, Update::Unbinds(EntityUnbinds { entity_ids: ids }));
                    let (binds, points) = carriage_binds(run, carriage, &listed, Some(owner), &at);
                    self.outbox.send(
                        session,
                        Update::Binds(EntityBinds {
                            key: format!("carriage:{}", binds[0].entity_id),
                            binds,
                        }),
                    );
                    run.carriages.get_mut(&carriage).expect("present").points = points;
                }
            }
        }
        run.carriages.get_mut(&carriage).expect("present").transform = Some(transform);
    }

    /// The doors of a carriage at a hangar gateway: its open doors, the cab's and the gateway's, go in the carriage's
    /// `tcOpenedDoors`, ascending, which every client opens or closes locally (the gateway's door is bound with
    /// `autoSerializeState` off, so it follows nothing else). Written in place when the index is set; otherwise the
    /// carriage is bound again with the list, which a carriage with riders cannot take. The cab's doors also get their
    /// `Door_OpenState`: in place as an animation when the index is set, otherwise by binding each again with it.
    fn doors(&mut self, manager: &Owner, carriage: CarriageId, gateway: GatewayRef, open: bool) {
        info!(carriage = carriage.0, ?gateway, open, "carriage doors");
        let Stop::Hangar(owner) = gateway.stop else {
            return;
        };
        let Some(hangar) = self.hangars.get(&owner) else {
            return;
        };
        let Some(at) = hangar.gateways.get(gateway.gateway) else {
            return;
        };
        let (Some(door), session) = (at.door, hangar.session) else {
            warn!(?gateway, "a gateway with no door to open");
            return;
        };
        // The gateway door's own state first, as the client keeps the authoritative one it was bound with and applies it
        // at the door's first change (it would snap the door shut half a second after the open): it is bound again with the state the doors now have, before the carriage's list.
        if let Some(bind) = &at.door_bind {
            self.outbox.send(
                session,
                Update::Unbinds(EntityUnbinds {
                    entity_ids: vec![door],
                }),
            );
            self.outbox.send(
                session,
                Update::Binds(EntityBinds {
                    key: format!("door:{door}"),
                    binds: vec![wire::door_rebind(bind, open)],
                }),
            );
        }
        let riders = self.has_riders(manager, carriage);
        let indices = self.indices;
        let listed = panel_hangars(
            &self.hangars,
            &self.names,
            manager,
            self.hangars
                .get(&owner)
                .map_or(owner, |hangar| hangar.owner),
        );
        let Some(run) = self.lines.get_mut(manager) else {
            return;
        };
        let Some(entity) = run.carriages.get_mut(&carriage) else {
            return;
        };
        let carriage_id = entity.entity_id;
        let cab_doors: Vec<u64> = run
            .cab
            .iter()
            .filter(|cab| crate::location::DOOR_CLASSES.contains(&cab.class_crc))
            .map(|cab| carriage_id + 1 + cab.index)
            .collect();
        let mut opened = cab_doors.clone();
        opened.push(door);
        opened.sort_unstable();
        entity.opened = if open { opened } else { Vec::new() };
        if entity.shown != Some(owner) {
            return;
        }
        let at = wire::CarriageAt {
            hangar_id: hangar.hangar_id,
            transform: entity.transform.unwrap_or(at.transform),
            time: Instant::now().duration_since(self.started),
            opened_doors: entity.opened.clone(),
        };
        match wire::carriage_opened_doors(carriage_id, &entity.opened, &indices) {
            Some(variables) => self.outbox.send(session, Update::Variables(variables)),
            None if riders => warn!(carriage_id, "a carriage with riders cannot take its open doors until their index is set (SC_DGS_TRANSIT_INDICES)"),
            None => {
                // With no index for `tcOpenedDoors`, the carriage is bound again with the list, and its cab with it.
                self.outbox.send(session, Update::Unbinds(EntityUnbinds { entity_ids: carriage_ids(carriage_id, run) }));
                let (binds, points) = carriage_binds(run, carriage, &listed, Some(owner), &at);
                self.outbox.send(session, Update::Binds(EntityBinds { key: format!("carriage:{}", binds[0].entity_id), binds }));
                Self::resend_panel(&self.outbox, &indices, run, carriage_id, &listed, Some(owner), session);
                run.carriages.get_mut(&carriage).expect("present").points = points;
            }
        }
        for id in cab_doors {
            if indices.door_state.is_some() {
                let (first, last) = if open {
                    (wire::DOOR_OPENING, wire::DOOR_OPENED)
                } else {
                    (wire::DOOR_CLOSING, wire::DOOR_CLOSED)
                };
                if let Some(variables) = wire::door_state(id, first, &indices) {
                    self.outbox.send(session, Update::Variables(variables));
                }
                self.pending.push(PendingDoor {
                    due: Instant::now() + DOOR_ANIMATION,
                    owner,
                    carriage_entity: carriage_id,
                    door: id,
                    state: last,
                });
            } else if let Some((_, cab)) = run
                .bound(carriage_id)
                .into_iter()
                .find(|(bound, _)| *bound == id)
            {
                let bind = EntityBind::from(run.cab_bind(id, carriage_id, cab, Vec::new()));
                self.outbox.send(
                    session,
                    Update::Unbinds(EntityUnbinds {
                        entity_ids: vec![id],
                    }),
                );
                self.outbox.send(
                    session,
                    Update::Binds(EntityBinds {
                        key: format!("door:{id}"),
                        binds: vec![wire::door_rebind(&bind, open)],
                    }),
                );
            }
        }
    }

    /// With the panel's index set, its list sent again in place after the carriage was bound: a client that does not run the
    /// variable's change callback for a bind's snapshot (unconfirmed) only lists what an update gives it.
    #[allow(clippy::too_many_arguments)]
    fn resend_panel(
        outbox: &Outbox,
        indices: &wire::Indices,
        run: &LineRun,
        entity_id: u64,
        listed: &[PanelHangar],
        current: Option<PlayerId>,
        session: SessionKey,
    ) {
        let Some(panel) = run.panel_id(entity_id) else {
            return;
        };
        let list = panel_entries(run, listed, current);
        if let Some(variables) = wire::panel_update(panel, &list.entries, indices) {
            outbox.send(session, Update::Variables(variables));
        }
    }

    /// The end states of the cab doors whose animation has run, for the carriages still bound to their client.
    fn send_due_doors(&mut self, now: Instant) {
        if self.pending.is_empty() {
            return;
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|door| door.due <= now);
        self.pending = later;
        for door in due {
            let bound = self.lines.values().any(|run| {
                run.carriages.values().any(|entity| {
                    entity.entity_id == door.carriage_entity && entity.shown == Some(door.owner)
                })
            });
            let Some(hangar) = self.hangars.get(&door.owner).filter(|_| bound) else {
                continue;
            };
            if let Some(variables) = wire::door_state(door.door, door.state, &self.indices) {
                self.outbox
                    .send(hangar.session, Update::Variables(variables));
            }
        }
    }

    /// A press on a carriage's cab panel: the stop of its point is the rider's pick, or the doors. `false` for no panel
    /// of a carriage bound to the player's client, or a point it does not list.
    pub fn cab_panel_press(&mut self, player: PlayerId, panel: u64, point: u32) -> bool {
        let found = self.lines.iter().find_map(|(manager, run)| {
            run.carriages.values().find_map(|entity| {
                let id = run.panel_id(entity.entity_id)?;
                (id == panel && entity.shown.is_some()).then(|| {
                    (
                        manager.clone(),
                        entity
                            .points
                            .iter()
                            .find(|(known, _)| *known == point)
                            .map(|(_, stop)| *stop),
                    )
                })
            })
        });
        let Some((manager, meaning)) = found else {
            return false;
        };
        match meaning {
            // A fixed stop is listed as the official service lists it, but this DGS cannot load its location for a rider yet: a pick
            // would hold the carriage for a location that never loads, so it is dropped (the rider keeps the panel).
            Some(Some(stop @ Stop::Destination(_))) => warn!(
                player,
                ?stop,
                "a fixed stop is not served by this DGS yet: the pick is dropped"
            ),
            Some(Some(stop)) => {
                info!(player, ?stop, "rider picked a stop");
                self.handle_line(&manager, Event::Pick { player, stop });
            }
            Some(None) => {
                info!(player, "rider asked for the doors");
                self.handle_line(&manager, Event::OpenDoors { player });
            }
            None => {
                warn!(
                    player,
                    point = format_args!("{point:#010x}"),
                    "a cab panel point it does not list"
                );
                return false;
            }
        }
        true
    }

    /// The carriages of every line, for the logs and tests: line name, slot, entity and the hangar it is bound to.
    pub fn carriages(&self) -> Vec<(String, usize, u64, Option<PlayerId>)> {
        let mut out: Vec<_> = self
            .lines
            .values()
            .flat_map(|run| {
                run.carriages
                    .iter()
                    .map(|(id, entity)| (run.name.clone(), id.0, entity.entity_id, entity.shown))
            })
            .collect();
        out.sort();
        out
    }
}

/// `TransitInteractionUIPanel_Elevator_Int`, the class of the panel inside a cab.
const CAB_PANEL_CLASS: u32 = 1_611_847_238;
/// The label of a hangar's entry on a cab panel, by the hangar's size (`@ui_interactor_hangar_<size>`: the official service
/// sent `small`... `large` and `xlarge`), from the prefix of its class name `Hangar_<Small|Medium|Large|XL>...`.
const HANGAR_LABEL_PREFIX: &str = "@ui_interactor_hangar_";
const HANGAR_CLASS_PREFIX: &str = "Hangar_";
const HANGAR_SIZES: [(&str, &str); 4] = [
    ("Small", "small"),
    ("Medium", "medium"),
    ("Large", "large"),
    ("XL", "xlarge"),
];
/// A hangar whose class names no known size is labelled as a small one, which is every hangar the DGS holds so far.
const DEFAULT_HANGAR_SIZE: &str = "small";
/// The environment variable that takes the location's fixed stops (lobbies, docking ports, shops...) off the cab panel:
/// `off`, any case. They are listed by default, as the official service lists them.
const FIXED_STOPS_ENV: &str = "SC_DGS_TRANSIT_FIXED_STOPS";
const FIXED_STOPS_OFF: &str = "off";
/// The label of the doors on a cab panel.
const DOORS_LABEL: &str = "@innerthought_open";

/// Whether the fixed stops of the manager are listed.
fn fixed_stops_listed() -> bool {
    !std::env::var(FIXED_STOPS_ENV)
        .is_ok_and(|setting| setting.trim().eq_ignore_ascii_case(FIXED_STOPS_OFF))
}

/// A fixed stop of a manager's location: the label of its gateway, its index in the manager and whether the data marks it
/// important, as the lobby of a location is (the panel lists those apart, at the bottom).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Destination {
    label: String,
    index: usize,
    important: bool,
}

/// The manager's destinations with an enabled gateway, each once, by the label of its first such gateway; important if any
/// of its gateways is.
fn destinations(manager: &super::Manager) -> Vec<Destination> {
    let mut out: Vec<Destination> = Vec::new();
    for gateway in manager
        .gateways
        .iter()
        .filter(|gateway| gateway.enabled && !gateway.name.is_empty())
    {
        match out
            .iter_mut()
            .find(|destination| destination.index == gateway.destination)
        {
            Some(destination) => destination.important |= gateway.important,
            None => out.push(Destination {
                label: gateway.name.clone(),
                index: gateway.destination,
                important: gateway.important,
            }),
        }
    }
    out
}

/// The environment variable that takes the display variables (current floor, style...) off the cab panel: `off`, any case.
/// They are bound by default, as the official service binds them.
const DISPLAY_ENV: &str = "SC_DGS_TRANSIT_DISPLAY";
const DISPLAY_OFF: &str = "off";

/// The carriage's id and its cab's, as they are bound and unbound together.
fn carriage_ids(entity_id: u64, run: &LineRun) -> Vec<u64> {
    std::iter::once(entity_id)
        .chain(run.bound(entity_id).into_iter().map(|(id, _)| id))
        .collect()
}

/// The carriage's bind and its cab's, the carriage first, for the client of a hangar whose rider's panel offers `listed`:
/// the cab panel lists the line's destinations, those hangars and the doors. Also what the panel's points mean.
fn carriage_binds(
    run: &LineRun,
    carriage: CarriageId,
    listed: &[PanelHangar],
    current: Option<PlayerId>,
    at: &wire::CarriageAt,
) -> (Vec<EntityBind>, Vec<(u32, Option<Stop>)>) {
    let entity_id = run.carriages[&carriage].entity_id;
    let list = panel_entries(run, listed, current);
    let mut binds = vec![EntityBind::from(wire::carriage_bind(
        entity_id,
        &run.kind,
        carriage.0,
        at,
        run.client_statics(),
    ))];
    for (id, cab) in run.bound(entity_id) {
        let extra = if cab.class_crc == CAB_PANEL_CLASS {
            list.snapshots(&run.name)
        } else {
            Vec::new()
        };
        binds.push(EntityBind::from(run.cab_bind(id, entity_id, cab, extra)));
    }
    (binds, list.points)
}

/// What a cab panel lists: the entries, the names its display shows, and what each point means.
struct PanelList {
    entries: Vec<wire::PanelEntry>,
    names: Vec<wire::StopName>,
    points: Vec<(u32, Option<Stop>)>,
    /// The index among `names` of the stop the carriage is at.
    current: u32,
}

impl PanelList {
    /// The snapshots a panel is bound with: its list, the display's names and its manager's name, as the official service binds it.
    fn snapshots(&self, manager_name: &str) -> Vec<crate::entity::Snapshot> {
        let mut snapshots = vec![
            wire::panel_interactions(&self.entries),
            wire::stop_names(&self.names),
            wire::panel_manager_name(manager_name),
        ];
        if !std::env::var(DISPLAY_ENV)
            .is_ok_and(|setting| setting.trim().eq_ignore_ascii_case(DISPLAY_OFF))
        {
            snapshots.extend(wire::panel_display(
                manager_name,
                self.current,
                self.names.len() as u32,
            ));
        }
        snapshots
    }
}

/// What a cab panel lists, in the official service's order: the manager's fixed stops, each `requestStop` at the point
/// `destination_<index>`; then the hangars the rider may ride to (their own first), at `dynamic_interaction_<k>` and
/// primary; then the doors' button (`requestDoorOpen`), all enabled. The client adds `callTransitCarriage` to the first
/// entry. The display's names follow the stops, the owner's name on a hangar.
fn panel_entries(run: &LineRun, hangars: &[PanelHangar], current: Option<PlayerId>) -> PanelList {
    let mut list = PanelList {
        entries: Vec::new(),
        names: Vec::new(),
        points: Vec::new(),
        current: 0,
    };
    if fixed_stops_listed() {
        for destination in &run.destinations {
            let point = wire::destination_point(destination.index);
            let flags = if destination.important {
                wire::PANEL_IMPORTANT
            } else {
                0
            } | wire::PANEL_ENABLED;
            list.entries.push(wire::PanelEntry {
                label: destination.label.clone(),
                point,
                flags,
            });
            list.names.push(wire::StopName {
                label: destination.label.clone(),
                owner: String::new(),
                important: destination.important,
            });
            list.points
                .push((point, Some(Stop::Destination(destination.index))));
        }
    }
    for (index, hangar) in hangars.iter().enumerate() {
        if current == Some(hangar.stop) {
            list.current = list.names.len() as u32;
        }
        let point = wire::stop_point(index);
        list.entries.push(wire::PanelEntry {
            label: hangar.label.clone(),
            point,
            flags: wire::PANEL_PRIMARY | wire::PANEL_ENABLED,
        });
        list.names.push(wire::StopName {
            label: hangar.label.clone(),
            owner: hangar.owner.clone(),
            important: false,
        });
        list.points.push((point, Some(Stop::Hangar(hangar.stop))));
    }
    list.entries.push(wire::PanelEntry {
        label: DOORS_LABEL.to_string(),
        point: wire::OPEN_DOORS_POINT,
        flags: wire::PANEL_ENABLED,
    });
    list.points.push((wire::OPEN_DOORS_POINT, None));
    list
}

/// The label of a hangar's panel entry from its class name.
fn hangar_label(class: &str) -> String {
    let kind = class.strip_prefix(HANGAR_CLASS_PREFIX).unwrap_or_default();
    let size = HANGAR_SIZES
        .iter()
        .find(|(prefix, _)| kind.starts_with(prefix))
        .map_or(DEFAULT_HANGAR_SIZE, |(_, size)| size);
    format!("{HANGAR_LABEL_PREFIX}{size}")
}

/// The hangars of `manager`'s line that `viewer`'s panel offers: those they own, their home first, then those the instance
/// manager granted them (`allow_guest`), each labelled with its size and its owner's name. A stranger's is never offered.
fn panel_hangars(
    hangars: &HashMap<PlayerId, HangarStop>,
    names: &HashMap<PlayerId, String>,
    manager: &Owner,
    viewer: PlayerId,
) -> Vec<PanelHangar> {
    let mut offered: Vec<(&PlayerId, &HangarStop)> = hangars
        .iter()
        .filter(|(_, hangar)| {
            hangar.manager == *manager
                && (hangar.owner == viewer || hangar.guests.contains(&viewer))
        })
        .collect();
    offered.sort_by_key(|(stop, hangar)| (hangar.owner != viewer, !hangar.home, **stop));
    offered
        .into_iter()
        .map(|(stop, hangar)| PanelHangar {
            stop: *stop,
            label: hangar_label(&hangar.class),
            owner: names
                .get(&hangar.owner)
                .cloned()
                .unwrap_or_else(|| wire::UNKNOWN_OWNER.to_string()),
        })
        .collect()
}

/// A gateway of the table with its prefab's entities, the prefab whose gateway entity is at the same place.
fn match_prefab(transform: &Transform, prefabs: &[GatewayPrefab]) -> HangarGateway {
    let prefab = prefabs.iter().find(|prefab| {
        prefab
            .gateway
            .is_some_and(|gateway| distance(&gateway.transform, transform) < SAME_PLACE)
    });
    let limbo = prefab
        .and_then(|prefab| prefab.limbo)
        .map_or_else(|| below(transform), |limbo| limbo.transform);
    HangarGateway {
        transform: *transform,
        door: prefab
            .and_then(|prefab| prefab.door)
            .map(|door| door.entity_id),
        door_bind: None,
        panel: prefab
            .and_then(|prefab| prefab.panel)
            .map(|panel| panel.entity_id),
        limbo,
    }
}

#[cfg(test)]
mod tests;

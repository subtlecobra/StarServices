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
    collections::{BTreeMap, HashMap},
    time::Instant,
};

use sc_api::internal::{session_update::Update, EntityBinds, EntityBind, EntityUnbinds, EntityVariables};
use tracing::{debug, info, warn};

use super::{
    PrefabEntity,
    sim::{CarriageId, CarriageState, Command, Event, GatewayRef, Line, LineSpec, Location, Place, PlayerId, Settings, Stop},
    tables, wire, Owner, System,
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
    manager: Owner,
    hangar_id: u64,
    session: SessionKey,
    gateways: Vec<HangarGateway>,
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
    /// The entities of its carriages' cab, which the DGS binds after each carriage's id, in index order.
    cab: Vec<PrefabEntity>,
    /// The manager's destinations a rider can pick, by label (its gateways' `nameIdentifier`) and index.
    destinations: Vec<(String, usize)>,
    first_id: u64,
    carriages: BTreeMap<CarriageId, CarriageEntity>,
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
}

fn distance(a: &Transform, b: &Transform) -> f64 {
    a.position.iter().zip(b.position).map(|(a, b)| (a - b) * (a - b)).sum::<f64>().sqrt()
}

/// Below a gateway by the depth of the prefabs' limbo nodes, for a gateway whose prefab has none.
fn below(gateway: &Transform) -> Transform {
    let [x, y, z] = gateway.position;
    Transform { position: [x, y, z - 6.0], rotation: gateway.rotation }
}

impl Elevators {
    pub fn new(outbox: Outbox) -> Self {
        Self { lines: HashMap::new(), hangars: HashMap::new(), outbox, started: Instant::now(), aboard: HashMap::new() }
    }

    /// A held player's hangar becomes a stop of the line of the manager that serves it, if it is a Transit one.
    pub fn open_hangar(&mut self, owner: PlayerId, home: &HomeHangar, session: SessionKey) {
        self.close_hangar(owner);
        let tables = tables();
        let Some(hangar) = tables.hangar(home.location.location_id, home.location.hangar_class_crc) else { return };
        if hangar.system != System::Transit {
            debug!(owner, system = ?hangar.system, "a hangar with no Transit elevator");
            return;
        }
        let Some(manager) = tables.manager(&hangar.manager) else { return };
        let prefabs = home.gateway_prefabs();
        let mut gateways: Vec<HangarGateway> = tables.stop_gateways(hangar).iter().map(|gateway| match_prefab(&gateway.transform, &prefabs)).collect();
        let content: Vec<EntityBind> = home.content_binds().into_iter().map(EntityBind::from).collect();
        for gateway in &mut gateways {
            gateway.door_bind = gateway.door.and_then(|door| content.iter().find(|bind| bind.entity_id == door)).cloned();
        }
        let doors = gateways.iter().filter(|gateway| gateway.door.is_some()).count();
        info!(owner, hangar = home.entity_id, manager = %manager.name, gateways = gateways.len(), doors, "hangar elevator stop opened");
        let count = gateways.len();
        let line_count = self.lines.len() as u64;
        self.lines.entry(manager.owner.clone()).or_insert_with(|| LineRun {
            line: Line::new(LineSpec::from_manager(manager), Settings::default().with_manager(manager)),
            name: manager.name.clone(),
            cab: tables.prefab_entities(manager.cab_socpak_hash).to_vec(),
            destinations: destinations(manager),
            kind: wire::CarriageKind { class_crc: manager.carriage_class_crc, cab_socpak_hash: manager.cab_socpak_hash, manager_name: manager.name.clone() },
            first_id: FIRST_CARRIAGE_ID + line_count * CARRIAGES_PER_LINE * CARRIAGE_STRIDE,
            carriages: BTreeMap::new(),
        });
        self.hangars.insert(owner, HangarStop { manager: manager.owner.clone(), hangar_id: home.entity_id, session, gateways });
        self.handle(owner, Event::HangarOpened { owner, gateways: count });
        self.handle(owner, Event::Ready { player: owner, location: Location::Hangar(owner) });
    }

    /// A player's hangar closes when the player leaves: its carriages go, and so does the player as a rider.
    pub fn close_hangar(&mut self, owner: PlayerId) {
        if !self.hangars.contains_key(&owner) {
            return;
        }
        self.handle(owner, Event::HangarClosed { owner });
        self.handle(owner, Event::Disconnected { player: owner });
        let manager = self.hangars.remove(&owner).map(|hangar| hangar.manager);
        if let Some(run) = manager.and_then(|manager| self.lines.get_mut(&manager)) {
            for carriage in run.carriages.values_mut().filter(|carriage| carriage.shown == Some(owner)) {
                carriage.shown = None;
                carriage.transform = None;
            }
        }
        info!(owner, "hangar elevator stop closed");
    }

    /// The hangar a session holds, if any.
    pub fn owner_of(&self, session: SessionKey) -> Option<PlayerId> {
        self.hangars.iter().find(|(_, hangar)| hangar.session == session).map(|(owner, _)| *owner)
    }

    /// A press of "Call elevator" on a call panel of `owner`'s hangar. `false` for a panel of no gateway of it.
    pub fn call(&mut self, owner: PlayerId, player: PlayerId, panel: u64) -> bool {
        let Some(index) = self.hangars.get(&owner).and_then(|hangar| hangar.gateways.iter().position(|gateway| gateway.panel == Some(panel))) else {
            warn!(owner, panel, "a call from a panel of no gateway of the hangar");
            return false;
        };
        self.handle(owner, Event::Call { player, gateway: GatewayRef { stop: Stop::Hangar(owner), gateway: index } });
        true
    }

    /// Everything due by now, on every line.
    pub fn step(&mut self, now: Instant) {
        let managers: Vec<Owner> = self.lines.keys().cloned().collect();
        for manager in managers {
            let Some(run) = self.lines.get_mut(&manager) else { continue };
            let commands = run.line.tick(now);
            self.apply(&manager, now, commands);
        }
    }

    /// An event of the line that serves `owner`'s hangar, and the commands it leads to.
    fn handle(&mut self, owner: PlayerId, event: Event) {
        let Some(manager) = self.hangars.get(&owner).map(|hangar| hangar.manager.clone()) else { return };
        self.handle_line(&manager, event);
    }

    fn handle_line(&mut self, manager: &Owner, event: Event) {
        let now = Instant::now();
        let Some(run) = self.lines.get_mut(manager) else { return };
        let commands = run.line.handle(now, event);
        self.apply(manager, now, commands);
    }

    /// The carriages bound to a client, which are zones its player may step into.
    pub fn carriage_zones(&self) -> Vec<u64> {
        self.lines.values().flat_map(|run| run.carriages.values().filter(|entity| entity.shown.is_some()).map(|entity| entity.entity_id)).collect()
    }

    /// The players' zones, as their movement has them: a player whose zone is a carriage has boarded it, and one whose
    /// zone no longer is has left it. Riders keep their carriage's doors and the carriage itself.
    pub fn observe(&mut self, zones: &[(PlayerId, u64)]) {
        let carriages: HashMap<u64, (Owner, CarriageId)> = self
            .lines
            .iter()
            .flat_map(|(manager, run)| run.carriages.iter().map(move |(id, entity)| (entity.entity_id, (manager.clone(), *id))))
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
                self.handle_line(&manager, Event::Exit { player: *player, carriage });
            }
            if let Some((manager, carriage)) = now_in {
                self.aboard.insert(*player, (manager.clone(), carriage));
                info!(player, carriage = carriage.0, "rider boarded the carriage");
                self.handle_line(&manager, Event::Board { player: *player, carriage });
            }
        }
    }

    /// Whether any player is aboard the carriage: such a carriage is never bound again, which would take it from under
    /// them.
    fn has_riders(&self, manager: &Owner, carriage: CarriageId) -> bool {
        self.aboard.values().any(|(line, id)| line == manager && *id == carriage)
    }

    fn apply(&mut self, manager: &Owner, now: Instant, commands: Vec<Command>) {
        let mut follow_up = Vec::new();
        for command in commands {
            debug!(?command, "elevator command");
            match command {
                Command::Create { carriage, place } => {
                    let Some(run) = self.lines.get_mut(manager) else { continue };
                    let entity_id = run.first_id + carriage.0 as u64 * CARRIAGE_STRIDE;
                    run.carriages.insert(carriage, CarriageEntity { entity_id, shown: None, transform: None, opened: Vec::new(), points: Vec::new() });
                    info!(line = %run.name, carriage = carriage.0, entity_id, ?place, "carriage created");
                    let (gateway, present) = match place {
                        Place::Gateway(gateway) => (Some(gateway), true),
                        Place::Limbo(stop) => (Some(GatewayRef { stop, gateway: 0 }), false),
                    };
                    self.show(manager, carriage, gateway, present);
                }
                Command::State { carriage, state, at, target, .. } => {
                    info!(carriage = carriage.0, ?state, ?at, ?target, "carriage state");
                    let present = at.is_some() && matches!(state, CarriageState::Present | CarriageState::Idle);
                    self.show(manager, carriage, at.or(target), present);
                    if let Some(entity_id) = self.lines.get(manager).and_then(|run| run.carriages.get(&carriage)).map(|carriage| carriage.entity_id) {
                        let in_transit = matches!(state, CarriageState::Travelling | CarriageState::WaitingInLimbo);
                        self.to_viewer(manager, carriage, |owner| (owner, wire::carriage_state(entity_id, in_transit)));
                    }
                }
                Command::OpenDoors { carriage, gateway } => self.doors(manager, carriage, gateway, true),
                Command::CloseDoors { carriage, gateway } => self.doors(manager, carriage, gateway, false),
                Command::Remove { carriage } => {
                    self.show(manager, carriage, None, false);
                    self.aboard.retain(|_, (line, id)| !(line == manager && *id == carriage));
                    if let Some(run) = self.lines.get_mut(manager) {
                        run.carriages.remove(&carriage);
                        info!(line = %run.name, carriage = carriage.0, "carriage removed");
                    }
                }
                Command::Load { player, location, resend } => match location {
                    Location::Hangar(owner) if owner == player && self.hangars.contains_key(&owner) => {
                        follow_up.push((owner, Event::Ready { player, location }));
                    }
                    _ => warn!(player, ?location, resend, "a rider needs a location this DGS does not send yet"),
                },
                Command::Unload { player, location } => debug!(player, ?location, "location no longer needed"),
                Command::MoveRider { player, from, to } => warn!(player, from = from.0, to = to.0, "moving a rider between carriages is not done yet"),
                // This DGS keeps no position of a player: they spawn in their hangar at every join, so a rider who leaves
                // aboard is already "saved" outside the gateway's door. A DGS that saves positions saves `stop` here.
                Command::Save { player, stop, gateway } => debug!(player, ?stop, ?gateway, "a rider left aboard, spawns in the hangar next time"),
            }
        }
        let _ = now;
        for (owner, event) in follow_up {
            self.handle(owner, event);
        }
    }

    /// Sends `make(owner)`'s update to the client of the hangar the carriage is bound to, if any.
    fn to_viewer(&self, manager: &Owner, carriage: CarriageId, make: impl FnOnce(PlayerId) -> (PlayerId, Option<EntityVariables>)) {
        let Some(owner) = self.lines.get(manager).and_then(|run| run.carriages.get(&carriage)).and_then(|carriage| carriage.shown) else { return };
        let (owner, variables) = make(owner);
        if let (Some(variables), Some(hangar)) = (variables, self.hangars.get(&owner)) {
            self.outbox.send(hangar.session, Update::Variables(variables));
        }
    }

    /// Binds the carriage, with its cab's entities, to the client of the hangar `gateway` is of, at the gateway or in limbo
    /// below it, moves it there if it is bound already, or unbinds it from the client that has it when it goes where no
    /// client of this DGS is.
    fn show(&mut self, manager: &Owner, carriage: CarriageId, gateway: Option<GatewayRef>, present: bool) {
        let place = gateway.and_then(|gateway| match gateway.stop {
            Stop::Hangar(owner) => {
                let hangar = self.hangars.get(&owner)?;
                let at = hangar.gateways.get(gateway.gateway)?;
                Some((owner, hangar.hangar_id, hangar.session, if present { at.transform } else { at.limbo }))
            }
            Stop::Destination(_) => None,
        });
        let since = self.started;
        let riders = self.has_riders(manager, carriage);
        let Some(run) = self.lines.get_mut(manager) else { return };
        let time = Instant::now().duration_since(since);
        let Some(entity) = run.carriages.get(&carriage) else { return };
        let ids = carriage_ids(entity.entity_id, &run.cab);
        if let Some(old) = entity.shown.filter(|old| place.is_none_or(|(owner, ..)| owner != *old)) {
            if let Some(hangar) = self.hangars.get(&old) {
                self.outbox.send(hangar.session, Update::Unbinds(EntityUnbinds { entity_ids: ids.clone() }));
            }
            let entity = run.carriages.get_mut(&carriage).expect("present");
            entity.shown = None;
            entity.transform = None;
        }
        let Some((owner, hangar_id, session, transform)) = place else { return };
        let entity = run.carriages.get(&carriage).expect("present");
        let at = wire::CarriageAt { hangar_id, transform, time, opened_doors: entity.opened.clone() };
        let (shown, last) = (entity.shown, entity.transform);
        if shown.is_none() {
            let (binds, points) = carriage_binds(run, carriage, owner, &at);
            self.outbox.send(session, Update::Binds(EntityBinds { key: format!("carriage:{}", binds[0].entity_id), binds }));
            let entity = run.carriages.get_mut(&carriage).expect("present");
            entity.shown = Some(owner);
            entity.points = points;
            info!(entity = entity.entity_id, owner, present, cab = run.cab.len(), "carriage bound to the hangar's client");
        } else if last != Some(transform) {
            match wire::carriage_moved(entity.entity_id, hangar_id, transform, time) {
                Some(variables) => self.outbox.send(session, Update::Variables(variables)),
                None if riders => warn!(entity = entity.entity_id, "a carriage with riders cannot move until its position's index is known"),
                None => {
                    // With no index for its position, the carriage is bound again where it now is, with its cab.
                    self.outbox.send(session, Update::Unbinds(EntityUnbinds { entity_ids: ids }));
                    let (binds, points) = carriage_binds(run, carriage, owner, &at);
                    self.outbox.send(session, Update::Binds(EntityBinds { key: format!("carriage:{}", binds[0].entity_id), binds }));
                    run.carriages.get_mut(&carriage).expect("present").points = points;
                }
            }
        }
        run.carriages.get_mut(&carriage).expect("present").transform = Some(transform);
    }

    /// The door of a hangar gateway, and the carriage's open doors.
    fn doors(&mut self, manager: &Owner, carriage: CarriageId, gateway: GatewayRef, open: bool) {
        info!(carriage = carriage.0, ?gateway, open, "carriage doors");
        let Stop::Hangar(owner) = gateway.stop else { return };
        let Some(hangar) = self.hangars.get(&owner) else { return };
        let Some(at) = hangar.gateways.get(gateway.gateway) else { return };
        let (Some(door), session) = (at.door, hangar.session) else { return };
        match (wire::door(door, open), &at.door_bind) {
            (Some(variables), _) => self.outbox.send(session, Update::Variables(variables)),
            (None, Some(bind)) => {
                // With no index for the door's state, the door is bound again with it.
                self.outbox.send(session, Update::Unbinds(EntityUnbinds { entity_ids: vec![door] }));
                self.outbox.send(session, Update::Binds(EntityBinds { key: format!("door:{door}"), binds: vec![wire::door_rebind(bind, open)] }));
            }
            (None, None) => warn!(door, "a door with no bind to open"),
        }
        // The cab's doors, bound by the server as the gateway's are: bound again with their state.
        let Some(run) = self.lines.get_mut(manager) else { return };
        let Some(entity) = run.carriages.get_mut(&carriage) else { return };
        let carriage_id = entity.entity_id;
        let mut opened: Vec<u64> = Vec::new();
        for cab in run.cab.iter().filter(|cab| crate::location::DOOR_CLASSES.contains(&cab.class_crc)) {
            let id = carriage_id + 1 + cab.index;
            opened.push(id);
            if entity.shown == Some(owner) {
                let bind = EntityBind::from(wire::cab_entity_bind(id, carriage_id, cab, Vec::new()));
                self.outbox.send(session, Update::Unbinds(EntityUnbinds { entity_ids: vec![id] }));
                self.outbox.send(session, Update::Binds(EntityBinds { key: format!("door:{id}"), binds: vec![wire::door_rebind(&bind, open)] }));
            }
        }
        opened.push(door);
        entity.opened = if open { opened } else { Vec::new() };
    }

    /// A press on a carriage's cab panel: the stop of its point is the rider's pick, or the doors. `false` for no panel
    /// of a carriage bound to the player's client, or a point it does not list.
    pub fn cab_panel_press(&mut self, player: PlayerId, panel: u64, point: u32) -> bool {
        let found = self.lines.iter().find_map(|(manager, run)| {
            run.carriages.values().find_map(|entity| {
                let id = run.cab.iter().find(|cab| cab.class_crc == CAB_PANEL_CLASS).map(|cab| entity.entity_id + 1 + cab.index)?;
                (id == panel && entity.shown.is_some()).then(|| (manager.clone(), entity.points.iter().find(|(known, _)| *known == point).map(|(_, stop)| *stop)))
            })
        });
        let Some((manager, meaning)) = found else { return false };
        match meaning {
            Some(Some(stop)) => {
                info!(player, ?stop, "rider picked a stop");
                self.handle_line(&manager, Event::Pick { player, stop });
            }
            Some(None) => {
                info!(player, "rider asked for the doors");
                self.handle_line(&manager, Event::OpenDoors { player });
            }
            None => {
                warn!(player, point = format_args!("{point:#010x}"), "a cab panel point it does not list");
                return false;
            }
        }
        true
    }

    /// The carriages of every line, for the logs and tests: line name, slot, entity and the hangar it is bound to.
    pub fn carriages(&self) -> Vec<(String, usize, u64, Option<PlayerId>)> {
        let mut out: Vec<_> = self.lines.values().flat_map(|run| run.carriages.iter().map(|(id, entity)| (run.name.clone(), id.0, entity.entity_id, entity.shown))).collect();
        out.sort();
        out
    }
}

/// `TransitInteractionUIPanel_Elevator_Int`, the class of the panel inside a cab.
const CAB_PANEL_CLASS: u32 = 1_611_847_238;
/// The label of the hangar's own stop on a cab panel.
const HANGAR_LABEL: &str = "@ui_interactor_hangar_small";
/// Whether the lobbies of a line's destinations are bound for riders by this DGS.
const LOBBIES_SERVED: bool = false;
/// The label of the doors on a cab panel.
const DOORS_LABEL: &str = "@innerthought_open";

/// The manager's destinations with an enabled gateway, each once, by the label of its first such gateway.
fn destinations(manager: &super::Manager) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for gateway in manager.gateways.iter().filter(|gateway| gateway.enabled && !gateway.name.is_empty()) {
        if !out.iter().any(|(_, destination)| *destination == gateway.destination) {
            out.push((gateway.name.clone(), gateway.destination));
        }
    }
    out
}

/// The carriage's id and its cab's, as they are bound and unbound together.
fn carriage_ids(entity_id: u64, cab: &[PrefabEntity]) -> Vec<u64> {
    std::iter::once(entity_id).chain(cab.iter().map(|entity| entity_id + 1 + entity.index)).collect()
}

/// The carriage's bind and its cab's, the carriage first, for the client of `owner`'s hangar: the cab panel lists the
/// line's destinations, the hangar's own stop and the doors. Also what the panel's points mean.
fn carriage_binds(run: &LineRun, carriage: CarriageId, owner: PlayerId, at: &wire::CarriageAt) -> (Vec<EntityBind>, Vec<(u32, Option<Stop>)>) {
    let entity_id = run.carriages[&carriage].entity_id;
    // A destination is listed only when this DGS can load its location for a rider, which it cannot yet: a pick would
    // leave the rider waiting for a location that never loads.
    let mut stops: Vec<(String, Stop)> = if LOBBIES_SERVED {
        run.destinations.iter().map(|(label, index)| (label.clone(), Stop::Destination(*index))).collect()
    } else {
        Vec::new()
    };
    stops.push((HANGAR_LABEL.to_string(), Stop::Hangar(owner)));
    let mut entries: Vec<wire::PanelEntry> = Vec::new();
    let mut points: Vec<(u32, Option<Stop>)> = Vec::new();
    for (index, (label, stop)) in stops.into_iter().enumerate() {
        let point = wire::stop_point(index);
        entries.push(wire::PanelEntry { label, point, flags: wire::PANEL_ENABLED });
        points.push((point, Some(stop)));
    }
    entries.push(wire::PanelEntry { label: DOORS_LABEL.to_string(), point: wire::OPEN_DOORS_POINT, flags: wire::PANEL_ENABLED });
    points.push((wire::OPEN_DOORS_POINT, None));
    let mut binds = vec![EntityBind::from(wire::carriage_bind(entity_id, &run.kind, carriage.0, at))];
    for cab in &run.cab {
        let extra = if cab.class_crc == CAB_PANEL_CLASS { vec![wire::panel_interactions(&entries)] } else { Vec::new() };
        binds.push(EntityBind::from(wire::cab_entity_bind(entity_id + 1 + cab.index, entity_id, cab, extra)));
    }
    (binds, points)
}

/// A gateway of the table with its prefab's entities, the prefab whose gateway entity is at the same place.
fn match_prefab(transform: &Transform, prefabs: &[GatewayPrefab]) -> HangarGateway {
    let prefab = prefabs.iter().find(|prefab| prefab.gateway.is_some_and(|gateway| distance(&gateway.transform, transform) < SAME_PLACE));
    let limbo = prefab.and_then(|prefab| prefab.limbo).map_or_else(|| below(transform), |limbo| limbo.transform);
    HangarGateway {
        transform: *transform,
        door: prefab.and_then(|prefab| prefab.door).map(|door| door.entity_id),
        door_bind: None,
        panel: prefab.and_then(|prefab| prefab.panel).map(|panel| panel.entity_id),
        limbo,
    }
}

#[cfg(test)]
mod tests;

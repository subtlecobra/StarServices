//! What a DGS answers to the replicant: which entities it owns and holds. The replicant writes them to clients, and decides
//! nothing about them. A client never owns any of this. How a running client marks authority has not been read, so this is
//! ownership on the server's side and nothing is sent to a client about it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use sc_api::internal::{
    dgs_control_server::DgsControl, DgsStatusRequest, DgsStatusResponse, EntityBind, EntityMethodCall, EntityMethodRequest, EntityMethodResponse,
    InputFrame, JoinPlayerRequest, JoinPlayerResponse, SessionUpdatesRequest,
    LeavePlayerRequest, LeavePlayerResponse, LocateHomeRequest, LocateHomeResponse, PlayerInputRequest, PlayerInputResponse, PlayerMovement,
    Rotation, ShardEntitiesRequest, ShardEntitiesResponse,
};
use sc_api::internal::character_customizer_internal_client::CharacterCustomizerInternalClient;
use sc_api::internal::{
    entity_graph_internal_client::EntityGraphInternalClient, identity_internal_client::IdentityInternalClient, EnsureLoadoutRequest,
    GetAccountRequest,
};
use tonic::{transport::Channel, Request, Response, Status};
use tracing::{info, warn};

use crate::{
    appearance::{self, Appearance},
    entity::Bind,
    frames::FrameMeter,
    holdings::Holdings,
    interaction::{Interaction, CALL_ELEVATOR, PANEL_ON_INTERACT, PANEL_REQUEST_INTERACT},
    location::{fallback_placement, governed_place, planets, player_home, CallPanel, Governed, HomeHangar},
    movement::{Bounds, ClientPosition, Input, Movement, Movements, SMALL_FRONT_HANGAR_GRID},
    outbox::{Outbox, Sent},
    transit::elevators::Elevators,
    loadout::{self, loadout_binds, loadout_binds_with, DEFAULT_LOADOUT, Item},
    player::{badges_snapshot, player_geid, player_index, player_record},
    solar_system::{planet_id, quantum_grid_bind, solar_system_bind, SOLAR_SYSTEMS},
    universe::first_binds,
};

/// What this DGS is, from the operator and its area.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub shard_id: String,
    pub name: String,
    pub system: String,
    pub roots: Vec<String>,
    pub governed_containers: u64,
    pub governed_entities: u64,
    /// Bind the home planet and hang the hangar from it. `false` puts the hangar at the origin of its system.
    pub planet_zone: bool,
}

/// A player's entities, as `JoinPlayer` builds them.
pub struct PlayerEntities {
    pub planet: Option<Bind>,
    pub hangar: Option<Bind>,
    /// The containers inside the hangar, such as the one whose brushes are its floor.
    pub hangar_content: Vec<Bind>,
    pub player: Bind,
    /// What the player wears, its body first, each under the item whose port holds it. Bound after the player.
    pub items: Vec<Bind>,
    /// The table `items` were made from.
    pub loadout: Vec<Item>,
    /// Where the player stands, which the DGS moves from here on.
    pub movement: Movement,
    /// The call panels of the hangar's elevators, among `hangar_content`.
    pub call_panels: Vec<CallPanel>,
    /// The hangar, whose elevator stop opens while the player is held.
    pub home: Option<HomeHangar>,
}

/// The socpak of `hangar_smlfrnt_002_rest_rund`, the rest stops' small front hangar.
const SMALL_FRONT_HANGAR_SOCPAK: u32 = 1_200_814_982;

/// What keeps a player inside its hangar's zone. Only the grid of the rest stops' hangar has been read.
fn hangar_bounds(socpak_hash: u32) -> Option<Bounds> {
    (socpak_hash == SMALL_FRONT_HANGAR_SOCPAK).then_some(SMALL_FRONT_HANGAR_GRID)
}

impl PlayerEntities {
    /// What goes when the player does. The planet stays, as it is the shard's.
    fn held(&self) -> Vec<u64> {
        let content = self.hangar_content.iter().map(|content| content.entity_id);
        let items = self.items.iter().map(|item| item.entity_id);
        self.hangar.iter().map(|hangar| hangar.entity_id).chain(content).chain([self.player.entity_id]).chain(items).collect()
    }
}

/// `None` for an account with no character. With no hangar the player waits at the origin of a system. The player wears
/// the game's default character.
pub fn player_entities(account: u64, location: Option<u32>, planet_zone: bool) -> Option<PlayerEntities> {
    player_entities_wearing(account, location, planet_zone, None)
}

/// As `player_entities`, with the character the player saved, `None` for the default.
pub fn player_entities_wearing(account: u64, location: Option<u32>, planet_zone: bool, appearance: Option<&Appearance>) -> Option<PlayerEntities> {
    let geid = player_geid(account)?;
    let home = player_home(account, location, planet_zone);
    let placement = match &home {
        Some(home) => home.placement(),
        None => {
            warn!(geid, "no home hangar, placing the player at the origin of its system");
            fallback_placement()?
        }
    };
    let location_id = home.as_ref().map(|home| home.location.location_id);
    let mut player = player_record(geid, account, placement, location_id).inspect_err(|error| warn!(?error, "no player bind")).ok()?;
    // No account has a stored inventory yet, so everyone wears the game's default items, with the head's that the player
    // chose in the character creator.
    let loadout = match appearance {
        Some(appearance) => {
            player.snapshots.extend(appearance.player_snapshots());
            appearance.items(&DEFAULT_LOADOUT)
        }
        None => DEFAULT_LOADOUT.to_vec(),
    };
    let items = loadout_binds(geid, player_index(account)?, &loadout);
    let bounds = home.as_ref().and_then(|home| hangar_bounds(home.location.socpak_hash));
    let movement = Movement::new(&placement, bounds);
    // Against the hangar's collision once it is built (at startup, see `main`); never built here, on the join. The
    // collision checks what the client reports, and moves the player only when it stops reporting.
    let movement = match &home {
        Some(home) => movement.with_collision(home.location.socpak_hash),
        None => movement,
    };
    Some(PlayerEntities {
        movement,
        planet: home.as_ref().and_then(|home| home.planet_bind()),
        hangar: home.as_ref().map(|home| home.bind()),
        hangar_content: home.as_ref().map(|home| home.content_binds()).unwrap_or_default(),
        call_panels: home.as_ref().map(|home| home.call_panels()).unwrap_or_default(),
        player,
        items,
        loadout,
        home,
    })
}

/// How long a join waits for sc-entitygraph to say what the player wears.
const ITEMS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub struct Control {
    settings: Settings,
    boot_id: u64,
    /// The place in `SOLAR_SYSTEMS` of this DGS's system, when it has the `<system>system` root.
    solar_system: Option<usize>,
    shard_entities: u64,
    frames: Arc<FrameMeter>,
    holdings: Mutex<Holdings>,
    movements: Arc<Mutex<Movements>>,
    /// The call panels of the hangars of the players held, by entity, with the replicant and session holding them.
    panels: Mutex<HashMap<u64, HeldPanel>>,
    /// What the clients of the sessions held are sent of this DGS's own accord.
    outbox: Outbox,
    /// The hangar elevators of the players held.
    elevators: Arc<Mutex<Elevators>>,
    /// `sc-character-customizer`, which owns saved characters. With none every player wears the default.
    customizer: Option<CharacterCustomizerInternalClient<Channel>>,
    /// `sc-identity`, which owns accounts and their in-game badges. With none no player has any.
    identity: Option<IdentityInternalClient<Channel>>,
    /// `sc-entitygraph`, which owns the items players wear. With none, or while it is away, they wear ids made up here.
    graph: Option<EntityGraphInternalClient<Channel>>,
}

struct HeldPanel {
    replicant: u64,
    session: u64,
    panel: CallPanel,
}

fn rotation([x, y, z, w]: [f64; 4]) -> Rotation {
    Rotation { x: x as f32, y: y as f32, z: z as f32, w: w as f32 }
}

impl From<&InputFrame> for Input {
    fn from(frame: &InputFrame) -> Self {
        let quaternion = |turn: &Rotation| [turn.x, turn.y, turn.z, turn.w].map(f64::from);
        let client_position = frame.client_position.as_ref().map(|client| ClientPosition {
            zone_host: client.zone_host_id,
            position: [client.x, client.y, client.z],
            base_rotation: client.base_rotation.as_ref().map_or([0.0, 0.0, 0.0, 1.0], quaternion),
        });
        Self {
            time: frame.time,
            move_buttons: frame.move_buttons,
            sprint: frame.sprint,
            aim_rotation: frame.aim_rotation.as_ref().map(quaternion),
            client_position,
            partial: frame.partial,
            jump: frame.jump,
            stance: frame.stance,
        }
    }
}

fn player_movement(player: u64, movement: &Movement) -> PlayerMovement {
    let [x, y, z] = movement.position;
    PlayerMovement {
        entity_id: player,
        x,
        y,
        z,
        base_rotation: Some(rotation(movement.base_rotation())),
        aim_rotation: Some(rotation(movement.aim_rotation())),
        zone_host_id: movement.zone_host,
        parent_id: movement.parent,
        revision: movement.revision,
        frame_time: movement.frame_time(),
    }
}

impl Control {
    /// A call on a carriage's cab panel. Its press is read as an interaction, whichever method it came on (the panel's
    /// method indices are not known yet, so the first press's are logged), and answered with the next method, which is
    /// `ClOnInteract` for `SvRequestInteract`.
    fn cab_panel_method(&self, request: EntityMethodRequest) -> Result<Response<EntityMethodResponse>, Status> {
        info!(entity = request.entity_id, method = request.method, args = request.args.len(), "a call on an entity that is not a call panel, read as a cab panel press");
        let Some(interaction) = Interaction::parse(&request.args).filter(|interaction| interaction.user == request.player_id && interaction.interactable == request.entity_id) else {
            return Err(Status::not_found(format!("{} holds no panel {} for this session", self.settings.name, request.entity_id)));
        };
        if !self.elevators.lock().unwrap().cab_panel_press(request.player_id, request.entity_id, interaction.point) {
            return Err(Status::not_found(format!("{} has no cab panel {} with that point", self.settings.name, request.entity_id)));
        }
        info!(panel = request.entity_id, method = request.method, point = format_args!("{:#010x}", interaction.point), "cab panel pressed");
        let answer = EntityMethodCall { entity_id: request.entity_id, method: request.method + 1, args: request.args };
        Ok(Response::new(EntityMethodResponse { to_caller: vec![answer] }))
    }

    pub fn new(settings: Settings) -> Self {
        let system_root = format!("{}system", settings.system.to_ascii_lowercase());
        let owned = settings.roots.contains(&system_root);
        let solar_system = SOLAR_SYSTEMS.iter().position(|system| owned && system.name.eq_ignore_ascii_case(&settings.system));
        let planets = planets().into_iter().filter(|planet| settings.roots.contains(&planet.name) && planet_id(&planet.name).is_some());
        let planets = if settings.planet_zone { planets.count() as u64 } else { 0 };
        let systems = solar_system.map_or(0, |_| 2);
        let boot_id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
        let outbox = Outbox::default();
        Self {
            settings,
            boot_id,
            solar_system,
            shard_entities: systems + planets,
            frames: Arc::default(),
            holdings: Mutex::default(),
            movements: Arc::default(),
            panels: Mutex::default(),
            outbox: outbox.clone(),
            elevators: Arc::new(Mutex::new(Elevators::new(outbox))),
            customizer: None,
            identity: None,
            graph: None,
        }
    }

    /// Asks `customizer` for the characters players saved.
    pub fn with_customizer(mut self, customizer: CharacterCustomizerInternalClient<Channel>) -> Self {
        self.customizer = Some(customizer);
        self
    }

    /// Has `graph` keep the items players wear.
    pub fn with_graph(mut self, graph: EntityGraphInternalClient<Channel>) -> Self {
        self.graph = Some(graph);
        self
    }

    /// The ids sc-entitygraph keeps the player's items under, one per item of `entities.loadout`: made on the first join
    /// with this loadout and the same afterwards. `None` without sc-entitygraph or while it cannot say.
    async fn item_ids(&self, entities: &PlayerEntities) -> Option<Vec<u64>> {
        let mut graph = self.graph.clone()?;
        let player_id = entities.player.entity_id;
        let request = EnsureLoadoutRequest {
            command_id: loadout::command_id(player_id, &entities.loadout),
            player_id,
            items: loadout::request_items(&entities.loadout),
        };
        // A join waits for this, so a slow sc-entitygraph costs the player the made up ids rather than the join.
        match tokio::time::timeout(ITEMS_TIMEOUT, graph.ensure_loadout(request)).await.unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("no answer"))) {
            Ok(reply) => {
                let reply = reply.into_inner();
                (reply.geids.len() == entities.loadout.len()).then_some(reply.geids)
            }
            Err(status) => {
                warn!(player_id, %status, "no stored items from sc-entitygraph, the player wears made up ones");
                None
            }
        }
    }

    /// Asks `identity` for the players' in-game badges.
    pub fn with_identity(mut self, identity: IdentityInternalClient<Channel>) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The in-game badges of `account`, by id, as identity has them. None when it cannot say.
    async fn badges(&self, account: u64) -> Vec<u32> {
        let (Some(mut identity), Ok(account_id)) = (self.identity.clone(), u32::try_from(account)) else { return Vec::new() };
        match identity.get_account(GetAccountRequest { account_id }).await {
            Ok(reply) => reply.into_inner().account.map(|account| account.game_badges.iter().map(|badge| badge.id).collect()).unwrap_or_default(),
            Err(status) => {
                warn!(account, %status, "no in-game badges from identity");
                Vec::new()
            }
        }
    }

    /// The character `account` saved, `None` for none or when it cannot be had.
    async fn appearance(&self, account: u64) -> Option<Appearance> {
        let mut customizer = self.customizer.clone()?;
        appearance::load(&mut customizer, player_geid(account)?).await
    }

    /// What the tick moves.
    pub fn movements(&self) -> Arc<Mutex<Movements>> {
        self.movements.clone()
    }

    /// Movement of players no longer held goes with them.
    fn forget_movements(&self, holdings: &Holdings) {
        self.movements.lock().unwrap().retain(|player| holdings.holds(player));
    }

    /// What the tick steps, with the movements.
    pub fn elevators(&self) -> Arc<Mutex<Elevators>> {
        self.elevators.clone()
    }

    /// What the tick writes to.
    pub fn frame_meter(&self) -> Arc<FrameMeter> {
        self.frames.clone()
    }

    fn status(&self) -> DgsStatusResponse {
        let holdings = self.holdings.lock().unwrap();
        let settings = &self.settings;
        DgsStatusResponse {
            shard_id: settings.shard_id.clone(),
            name: settings.name.clone(),
            system: settings.system.clone(),
            roots: settings.roots.clone(),
            governed_containers: settings.governed_containers,
            governed_entities: settings.governed_entities,
            frames: Some(self.frames.frames()),
            players: holdings.players(),
            player_entities: holdings.entities(),
            shard_entities: self.shard_entities,
            boot_id: self.boot_id,
        }
    }

    fn governs(&self, place: &Governed) -> bool {
        self.settings.system.eq_ignore_ascii_case(&place.system) && self.settings.roots.contains(&place.root())
    }
}

#[tonic::async_trait]
impl DgsControl for Control {
    type SessionUpdatesStream = tokio_stream::wrappers::UnboundedReceiverStream<Sent>;

    async fn session_updates(&self, request: Request<SessionUpdatesRequest>) -> Result<Response<Self::SessionUpdatesStream>, Status> {
        let request = request.into_inner();
        info!(replicant = request.replicant, session = request.session, "session subscribed to its updates");
        let receiver = self.outbox.subscribe((request.replicant, request.session));
        Ok(Response::new(tokio_stream::wrappers::UnboundedReceiverStream::new(receiver)))
    }

    async fn status(&self, _: Request<DgsStatusRequest>) -> Result<Response<DgsStatusResponse>, Status> {
        Ok(Response::new(self.status()))
    }

    async fn locate_home(&self, request: Request<LocateHomeRequest>) -> Result<Response<LocateHomeResponse>, Status> {
        let place = governed_place(request.into_inner().location_id);
        Ok(Response::new(LocateHomeResponse { system: place.system, planet: place.planet.unwrap_or_default() }))
    }

    async fn shard_entities(&self, _: Request<ShardEntitiesRequest>) -> Result<Response<ShardEntitiesResponse>, Status> {
        let Some(order) = self.solar_system else { return Ok(Response::new(ShardEntitiesResponse::default())) };
        let system = &SOLAR_SYSTEMS[order];
        Ok(Response::new(ShardEntitiesResponse {
            universe: first_binds().into_iter().map(Into::into).collect(),
            solar_system: Some(solar_system_bind(order, system).into()),
            quantum_grid: Some(quantum_grid_bind(order, system).into()),
            order: order as u32,
        }))
    }

    async fn join_player(&self, request: Request<JoinPlayerRequest>) -> Result<Response<JoinPlayerResponse>, Status> {
        let request = request.into_inner();
        let place = governed_place(request.location_id);
        if !self.governs(&place) {
            return Err(Status::failed_precondition(format!("{} does not govern {}", self.settings.name, place.root())));
        }
        let appearance = self.appearance(request.account_id).await;
        let wearing = appearance.as_ref();
        let Some(mut entities) = player_entities_wearing(request.account_id, request.location_id, self.settings.planet_zone, wearing) else {
            return Err(Status::invalid_argument(format!("account {} has no character", request.account_id)));
        };
        if let Some(ids) = self.item_ids(&entities).await {
            entities.items = loadout_binds_with(entities.player.entity_id, &entities.loadout, &ids);
        }
        let badges = self.badges(request.account_id).await;
        if !badges.is_empty() {
            entities.player.snapshots.push(badges_snapshot(&badges));
        }
        let held = entities.held();
        info!(
            account = request.account_id, session = request.session, root = %place.root(), customized = appearance.is_some(), ?held,
            "holding a player's entities",
        );
        let mut holdings = self.holdings.lock().unwrap();
        holdings.join(request.replicant, request.session, request.account_id, held);
        self.forget_movements(&holdings);
        drop(holdings);
        let mut panels = self.panels.lock().unwrap();
        panels.retain(|_, held| (held.replicant, held.session) != (request.replicant, request.session));
        for panel in entities.call_panels {
            panels.insert(panel.entity_id, HeldPanel { replicant: request.replicant, session: request.session, panel });
        }
        drop(panels);
        if let Some(home) = &entities.home {
            self.elevators.lock().unwrap().open_hangar(entities.player.entity_id, home, (request.replicant, request.session));
        }
        // A join is a spawn: the player stands at its spawn point again.
        self.movements.lock().unwrap().join(entities.player.entity_id, entities.movement);
        Ok(Response::new(JoinPlayerResponse {
            planet: entities.planet.map(EntityBind::from),
            hangar: entities.hangar.map(EntityBind::from),
            player: Some(entities.player.into()),
            status: Some(self.status()),
            hangar_content: entities.hangar_content.into_iter().map(EntityBind::from).collect(),
            player_items: entities.items.into_iter().map(EntityBind::from).collect(),
        }))
    }

    async fn leave_player(&self, request: Request<LeavePlayerRequest>) -> Result<Response<LeavePlayerResponse>, Status> {
        let request = request.into_inner();
        let mut holdings = self.holdings.lock().unwrap();
        let released = holdings.leave(request.replicant, request.session);
        self.forget_movements(&holdings);
        drop(holdings);
        self.panels.lock().unwrap().retain(|_, held| (held.replicant, held.session) != (request.replicant, request.session));
        let mut elevators = self.elevators.lock().unwrap();
        if let Some(owner) = elevators.owner_of((request.replicant, request.session)) {
            elevators.close_hangar(owner);
        }
        drop(elevators);
        self.outbox.close((request.replicant, request.session));
        info!(session = request.session, released, "player left");
        Ok(Response::new(LeavePlayerResponse { released }))
    }

    async fn player_input(&self, request: Request<PlayerInputRequest>) -> Result<Response<PlayerInputResponse>, Status> {
        let request = request.into_inner();
        let mut movements = self.movements.lock().unwrap();
        let Some(movement) = movements.get_mut(request.entity_id) else {
            return Err(Status::not_found(format!("{} holds no player {}", self.settings.name, request.entity_id)));
        };
        for frame in &request.frames {
            movement.input(&Input::from(frame));
        }
        Ok(Response::new(PlayerInputResponse { movement: Some(player_movement(request.entity_id, movement)) }))
    }

    /// A press of a call panel: "Call elevator" by the panel's own player is taken, and answered with `ClOnInteract` so
    /// that the client runs the panel's side of it. Anything else on a panel is refused, with no answer, as the game does.
    async fn entity_method(&self, request: Request<EntityMethodRequest>) -> Result<Response<EntityMethodResponse>, Status> {
        let request = request.into_inner();
        let panels = self.panels.lock().unwrap();
        let Some(held) = panels.get(&request.entity_id).filter(|held| (held.replicant, held.session) == (request.replicant, request.session)) else {
            drop(panels);
            return self.cab_panel_method(request);
        };
        let refused = Ok(Response::new(EntityMethodResponse::default()));
        if request.method != PANEL_REQUEST_INTERACT {
            info!(entity = request.entity_id, method = request.method, "a call panel method that is not an interaction");
            return refused;
        }
        let Some(interaction) = Interaction::parse(&request.args).filter(|interaction| interaction.user == request.player_id && interaction.interactable == request.entity_id) else {
            warn!(entity = request.entity_id, player = request.player_id, "an interaction with a call panel that does not read as one of this player's");
            return refused;
        };
        if interaction.interaction != CALL_ELEVATOR {
            info!(entity = request.entity_id, interaction = format_args!("{:#010x}", interaction.interaction), "a call panel interaction that is not handled");
            return refused;
        }
        let panel = &held.panel;
        info!(panel = panel.entity_id, hangar = panel.hangar_id, gateway = %panel.gateway, player = request.player_id, request_id = interaction.request_id, "elevator called");
        let (player, panel) = (request.player_id, request.entity_id);
        drop(panels);
        if !self.elevators.lock().unwrap().call(player, player, panel) {
            return refused;
        }
        let answer = EntityMethodCall { entity_id: request.entity_id, method: PANEL_ON_INTERACT, args: request.args };
        Ok(Response::new(EntityMethodResponse { to_caller: vec![answer] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ORBITUARY;
    use tonic::Code;

    fn control(name: &str, system: &str, roots: &[&str]) -> Control {
        let roots = roots.iter().map(ToString::to_string).collect();
        Control::new(Settings { shard_id: "s".into(), name: name.into(), system: system.into(), roots, planet_zone: true, ..Settings::default() })
    }

    fn pyro_1() -> Control {
        control("pyro-1", "pyro", &["pyrosystem", "pyro1", "pyro2", "pyro3"])
    }

    fn join(account: u64, location: Option<u32>, session: u64) -> Request<JoinPlayerRequest> {
        Request::new(JoinPlayerRequest { replicant: 1, session, account_id: account, location_id: location })
    }

    fn leave(session: u64) -> Request<LeavePlayerRequest> {
        Request::new(LeavePlayerRequest { replicant: 1, session })
    }

    #[tokio::test]
    async fn the_dgs_with_the_system_root_owns_the_system_and_its_grid() {
        let reply = pyro_1().shard_entities(Request::new(ShardEntitiesRequest {})).await.unwrap().into_inner();
        assert_eq!(reply.order, 1);
        assert_eq!(reply.universe.len(), 2);
        assert_eq!(reply.solar_system.unwrap().entity_id, 200_001_275_310);
        assert_eq!(reply.quantum_grid.unwrap().parent_id, 200_001_275_310);
    }

    #[tokio::test]
    async fn a_dgs_with_no_system_root_owns_nothing_every_client_gets() {
        let reply = control("pyro-2", "pyro", &["pyro5", "pyro6"]).shard_entities(Request::new(ShardEntitiesRequest {})).await.unwrap().into_inner();
        assert_eq!(reply, ShardEntitiesResponse::default());
    }

    #[test]
    fn a_dgs_counts_what_it_holds_for_the_shard() {
        assert_eq!(pyro_1().status().shard_entities, 3, "the system, its grid and pyro3");
        assert_eq!(control("pyro-2", "pyro", &["pyro6"]).status().shard_entities, 1);
        assert_eq!(control("stanton-2", "stanton", &["stanton1a"]).status().shard_entities, 0);
    }

    /// sc-entitygraph being away costs the player nothing: the items are bound under the ids made up here.
    #[tokio::test]
    async fn a_player_wears_made_up_items_while_sc_entitygraph_is_away() {
        let endpoint = tonic::transport::Endpoint::from_static("http://127.0.0.1:1");
        let dgs = pyro_1().with_graph(EntityGraphInternalClient::new(endpoint.connect_lazy()));
        let reply = dgs.join_player(join(1_000_001, Some(ORBITUARY), 7)).await.unwrap().into_inner();
        assert_eq!(reply.player_items.len(), 23);
        assert_eq!(reply.player_items[0].entity_id, loadout::item_id(1, 0));
    }

    #[tokio::test]
    async fn the_governing_dgs_builds_the_player_and_records_it() {
        let dgs = pyro_1();
        let reply = dgs.join_player(join(1_000_001, Some(ORBITUARY), 7)).await.unwrap().into_inner();
        assert_eq!(reply.player.unwrap().entity_id, 300_000_000_001);
        assert_eq!(reply.hangar.as_ref().unwrap().entity_id, 220_000_065_536);
        assert_eq!(reply.planet.unwrap().entity_id, 200_002_185_940);
        assert_eq!(reply.hangar_content.len(), 11, "7 containers and the 4 entities of the elevator's gateway");
        let gateway: Vec<u64> = reply.hangar_content[7..].iter().map(|bind| bind.entity_id).collect();
        let status = reply.status.unwrap();
        assert_eq!(reply.player_items.len(), 23);
        assert_eq!(reply.player_items[0].parent_id, 300_000_000_001, "the body hangs from the player");
        assert_eq!((status.players, status.player_entities), (1, 36), "the hangar, its 7 containers, the gateway's 4 entities, the player and its 23 items");
        let held: Vec<u64> = [220_000_065_536]
            .into_iter()
            .chain(220_000_065_581..220_000_065_588)
            .chain(gateway)
            .chain([300_000_000_001])
            .chain(310_000_000_256..310_000_000_279)
            .collect();
        assert_eq!(dgs.holdings.lock().unwrap().of(1_000_001), Some(held.as_slice()));
    }

    #[tokio::test]
    async fn leaving_frees_the_players_entities_and_not_the_planet() {
        let dgs = pyro_1();
        dgs.join_player(join(1_000_001, None, 7)).await.unwrap();
        let reply = dgs.leave_player(leave(7)).await.unwrap().into_inner();
        assert_eq!(reply.released, 36);
        let status = dgs.status();
        assert_eq!((status.players, status.player_entities, status.shard_entities), (0, 0, 3));
    }

    #[tokio::test]
    async fn joining_again_gives_the_same_entities_and_counts_once() {
        let dgs = pyro_1();
        let first = dgs.join_player(join(1_000_001, None, 7)).await.unwrap().into_inner();
        let second = dgs.join_player(join(1_000_001, None, 7)).await.unwrap().into_inner();
        assert_eq!((first.hangar, first.player), (second.hangar, second.player));
        assert_eq!(dgs.status().players, 1);
    }

    #[tokio::test]
    async fn another_dgs_of_the_system_refuses_a_home_it_does_not_govern() {
        let dgs = control("pyro-2", "pyro", &["pyro5", "pyro6"]);
        let error = dgs.join_player(join(1_000_001, Some(ORBITUARY), 7)).await.unwrap_err();
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert_eq!(dgs.status().players, 0);
        assert!(dgs.join_player(join(1_000_001, Some(2_026_442_305), 7)).await.is_ok());
    }

    #[tokio::test]
    async fn an_account_with_no_character_is_refused_for_good() {
        let error = pyro_1().join_player(join(5, None, 7)).await.unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_home_with_no_hangar_leaves_the_player_with_the_system_root() {
        let reply = pyro_1().join_player(join(1_000_001, Some(1), 7)).await.unwrap().into_inner();
        assert_eq!((reply.hangar, reply.planet), (None, None));
        assert_eq!(reply.player.unwrap().parent_id, 200_001_275_310);
    }

    #[tokio::test]
    async fn any_dgs_locates_a_home() {
        let dgs = control("nyx-1", "nyx", &["nyxsystem"]);
        let located = dgs.locate_home(Request::new(LocateHomeRequest { location_id: Some(2_026_442_305) })).await.unwrap().into_inner();
        assert_eq!((located.system.as_str(), located.planet.as_str()), ("Pyro", "pyro6"));
    }

    #[test]
    fn a_new_process_has_a_new_boot_id() {
        assert_ne!(pyro_1().status().boot_id, 0);
    }

    /// "Call elevator" as a running client sent it, with this player and panel in it.
    fn press(player: u64, panel: u64, interaction: u32) -> Vec<u8> {
        let mut args = 0x003f_bdc1_4d2c_a50d_u64.to_le_bytes().to_vec();
        args.extend(1u64.to_le_bytes());
        args.extend(0x42bf_347e_u32.to_le_bytes());
        args.extend(interaction.to_le_bytes());
        args.extend(player.to_le_bytes());
        args.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        args.extend(panel.to_le_bytes());
        args
    }

    fn method(session: u64, player: u64, entity: u64, method: u32, args: Vec<u8>) -> Request<EntityMethodRequest> {
        Request::new(EntityMethodRequest { replicant: 1, session, player_id: player, entity_id: entity, method, args })
    }

    #[tokio::test]
    async fn a_call_panel_press_is_answered_to_its_player_alone() {
        let dgs = pyro_1();
        let reply = dgs.join_player(join(1_000_001, Some(ORBITUARY), 7)).await.unwrap().into_inner();
        let player = reply.player.unwrap().entity_id;
        let panel = reply.hangar_content.iter().find(|bind| bind.class_crc == crate::location::CALL_PANEL_CLASS).unwrap().entity_id;
        let args = press(player, panel, CALL_ELEVATOR);
        let answer = dgs.entity_method(method(7, player, panel, 10, args.clone())).await.unwrap().into_inner();
        assert_eq!(answer.to_caller, [EntityMethodCall { entity_id: panel, method: 11, args: args.clone() }], "ClOnInteract, with the request's arguments");
        // Another interaction, another method, someone else's press: refused, with no answer.
        let other = dgs.entity_method(method(7, player, panel, 10, press(player, panel, 1))).await.unwrap().into_inner();
        assert!(other.to_caller.is_empty());
        assert!(dgs.entity_method(method(7, player, panel, 12, args.clone())).await.unwrap().into_inner().to_caller.is_empty());
        assert!(dgs.entity_method(method(7, player, panel, 10, press(player + 1, panel, CALL_ELEVATOR))).await.unwrap().into_inner().to_caller.is_empty());
        // Not this session's panel, and none once the player has left.
        assert_eq!(dgs.entity_method(method(8, player, panel, 10, args.clone())).await.unwrap_err().code(), Code::NotFound);
        dgs.leave_player(leave(7)).await.unwrap();
        assert_eq!(dgs.entity_method(method(7, player, panel, 10, args)).await.unwrap_err().code(), Code::NotFound);
    }
}

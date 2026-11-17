//! What a DGS answers to the replicant: which entities it owns and holds. The replicant writes them to clients, and decides
//! nothing about them. A client never owns any of this. How a client marks authority is not known, so this is
//! ownership on the server's side and nothing is sent to a client about it.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use sc_api::internal::{
    character_customizer_internal_client::CharacterCustomizerInternalClient,
    ledger_internal_client::LedgerInternalClient,
};
use sc_api::internal::{
    dgs_control_server::DgsControl, session_update::Update, DgsStatusRequest, DgsStatusResponse,
    EntityBind, EntityBinds, EntityMethodCall, EntityMethodRequest, EntityMethodResponse,
    EntityUnbinds, InputFrame, JoinPlayerRequest, JoinPlayerResponse, LeavePlayerRequest,
    LeavePlayerResponse, LocateHomeRequest, LocateHomeResponse, PlayerInputRequest,
    PlayerInputResponse, PlayerMovement, Rotation, SessionUpdatesRequest, ShardEntitiesRequest,
    ShardEntitiesResponse,
};
use sc_api::internal::{
    entity_graph_internal_client::EntityGraphInternalClient,
    identity_internal_client::IdentityInternalClient, ships_internal_client::ShipsInternalClient,
    DeliverShipRequest, EnsureLoadoutRequest, GetAccountRequest, HangarSize, ListShipsRequest,
    ReportShipRequest, ShipState, ShipView,
};
use tonic::{transport::Channel, Request, Response, Status};
use tracing::{info, warn};

use crate::{
    appearance::{self, Appearance},
    departure,
    entity::Bind,
    equip::{self, GraphItems, Origin},
    frames::FrameMeter,
    holdings::Holdings,
    interaction::{
        Interaction, CALL_ELEVATOR, KIOSK_ON_INTERACT, KIOSK_REQUEST_INTERACT, PANEL_ON_INTERACT,
        PANEL_REQUEST_INTERACT,
    },
    inventory::{completed_args, Request as InventoryRequest, COMPLETED_AFTER_PROCESS},
    kiosk_calls::{
        claim_response, class_prices, insurance_response_method, interaction_state_index,
        interaction_state_update, landed_records, prices_reply, server_infos, server_infos_landed, ship_lists, use_update, KioskCall,
        CLAIM_RESPONSE_ENV, CLAIM_RESPONSE_OFFSET, GUID_LEN, INSURANCE_RESPONSE_METHOD_ENV,
        INSURANCE_RESPONSE_OFF, KIOSK_ON_FETCH_PRICES,
    },
    loadout::{self, equipped_binds, loadout_binds, loadout_binds_with, Item, DEFAULT_LOADOUT},
    location::{
        fallback_placement, governed_place, planets, player_home, CallPanel, Governed, HomeHangar,
    },
    movement::{Bounds, ClientPosition, Input, Movement, Movements, SMALL_FRONT_HANGAR_GRID},
    outbox::{Outbox, Sent},
    player::{badges_snapshot, player_geid, player_index, player_record},
    ships::{hangar_class::hangar_size_of, ShipBackend},
    solar_system::{planet_id, quantum_grid_bind, solar_system_bind, SOLAR_SYSTEMS},
    transit::elevators::Elevators,
    universe::first_binds,
    worn::Worn,
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
    /// The ids of the hangar's ASOP kiosks.
    pub kiosks: Vec<u64>,
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
        self.hangar
            .iter()
            .map(|hangar| hangar.entity_id)
            .chain(content)
            .chain([self.player.entity_id])
            .chain(items)
            .collect()
    }
}

/// `None` for an account with no character. With no hangar the player waits at the origin of a system. The player wears
/// the game's default character.
pub fn player_entities(
    account: u64,
    location: Option<u32>,
    planet_zone: bool,
) -> Option<PlayerEntities> {
    // A small hangar, the first of the location's table, which the tests and the golden binds were made with.
    player_entities_wearing(account, location, HangarSize::Small, planet_zone, None)
}

/// As `player_entities`, with the character the player saved, `None` for the default.
pub fn player_entities_wearing(
    account: u64,
    location: Option<u32>,
    hangar_size: HangarSize,
    planet_zone: bool,
    appearance: Option<&Appearance>,
) -> Option<PlayerEntities> {
    let geid = player_geid(account)?;
    let home = player_home(account, location, hangar_size, planet_zone);
    let placement = match &home {
        Some(home) => home.placement(),
        None => {
            warn!(
                geid,
                "no home hangar, placing the player at the origin of its system"
            );
            fallback_placement()?
        }
    };
    let location_id = home.as_ref().map(|home| home.location.location_id);
    let mut player = player_record(geid, account, placement, location_id)
        .inspect_err(|error| warn!(?error, "no player bind"))
        .ok()?;
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
    let bounds = home
        .as_ref()
        .and_then(|home| hangar_bounds(home.location.socpak_hash));
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
        hangar_content: home
            .as_ref()
            .map(|home| {
                let mut binds = home.content_binds();
                binds.extend(crate::atc::binds(home));
                binds
            })
            .unwrap_or_default(),
        call_panels: home
            .as_ref()
            .map(|home| home.call_panels())
            .unwrap_or_default(),
        kiosks: home
            .as_ref()
            .map(|home| home.kiosk_ids())
            .unwrap_or_default(),
        player,
        items,
        loadout,
        home,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// How long a join waits for sc-entitygraph to say what the player wears.
/// `ListShipsRequest.location_id` for the ships of every location, and `now_ms` for the clock of the service.
const ALL_LOCATIONS: u32 = 0;
const CLOCK_OF_THE_SERVICE: i64 = 0;
const ITEMS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
/// The prefix of a ship delivery's command id, then the claim request's id: a repeated request moves nothing twice.
const DELIVERY_COMMAND_PREFIX: &str = "kiosk-deliver:";
/// What the store of a ship the record held live adds to that command id, so that the two commands differ.
const STALE_COMMAND_SUFFIX: &str = ":stale";

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
    /// The ASOP kiosks of the hangars of the players held, by entity, with the replicant and session holding them.
    kiosks: Mutex<HashMap<u64, (u64, u64)>>,
    /// The hangar class (`Hangar_<size><Front|Top>_<place>`) of each kiosk in `kiosks`, which decides the ships that fit it.
    kiosk_hangars: Mutex<HashMap<u64, String>>,
    /// The location each kiosk stands at: where the client looks for the ships that are here, and where a delivery takes them.
    kiosk_locations: Mutex<HashMap<u64, u32>>,
    /// What the client's fire-and-forget calls on its player entity set (`player_calls`).
    player_calls: crate::player_calls::PlayerCalls,
    /// The item each player last took in hand: the client does it as a put in a pocket, so the item the next `SDrop`
    /// drops is the one last put in a pocket.
    pocketed: Mutex<HashMap<u64, u64>>,
    /// What the clients of the sessions held are sent of this DGS's own accord.
    outbox: Outbox,
    /// The hangar elevators of the players held.
    elevators: Arc<Mutex<Elevators>>,
    /// The hangar instances beyond players' home hangars (docs/HANGARS.md). Locked after `elevators`, never before.
    instances: Arc<Mutex<crate::hangar_instance::HangarInstances>>,
    /// The game data the hangars' collision is fetched from when a player first lives in a hangar that was not loaded at
    /// startup (the sizes other than a location's first). With none only what was loaded at startup is there.
    gamedata: Option<crate::gamedata::GameData>,
    /// `sc-character-customizer`, which owns saved characters. With none every player wears the default.
    customizer: Option<CharacterCustomizerInternalClient<Channel>>,
    /// `sc-identity`, which owns accounts and their in-game badges. With none no player has any.
    identity: Option<IdentityInternalClient<Channel>>,
    /// `sc-ledger`, which owns the money. With none every wallet is empty.
    ledger: Option<LedgerInternalClient<Channel>>,
    /// `sc-entitygraph`, which owns the items players wear. With none, or while it is away, they wear ids made up here.
    graph: Option<EntityGraphInternalClient<Channel>>,
    /// `sc-entitygraph`'s ships, whose records answer an ASOP kiosk's ship list. With none the list names no ship.
    ships: Option<ShipsInternalClient<Channel>>,
    /// The ships retrieved at the hangars' ASOP kiosks, through their ATCs (see `retrieve`).
    retrievals: Arc<crate::retrieve::Retrievals<ShipsInternalClient<Channel>>>,
    /// What the players held wear, by player entity, with the session holding it. A client's inventory requests change it.
    worn: Mutex<HashMap<u64, HeldWorn>>,
}

struct HeldWorn {
    replicant: u64,
    session: u64,
    worn: Worn,
    /// The weapon in a hand and where it was holstered, while one is drawn (see `draw`).
    drawn: Option<(u64, (u64, u32))>,
    /// The rounds of the magazines (see `reload`).
    ammo: crate::reload::Ammo,
    /// The trigger and the health (see `combat`).
    combat: crate::combat::Combat,
    /// The weapon and the magazine the client was last told to fire from (see `weapon_command`).
    container_sent: Option<(u64, u64)>,
    /// The tick of the client's last weapon command and when it arrived, which the state reply is stamped after (see `weapon_command`).
    last_weapon_tick: Option<(u32, std::time::Instant)>,
    /// The drawn weapon whose state request came before any command of the client (no tick known): it is answered with the
    /// first command, stamped after its tick.
    state_pending: Option<u64>,
}

impl HeldWorn {
    /// The weapon the player holds: the one drawn from a holster, else an item carried in a hand (a weapon the client took
    /// out of a pocket or an inventory arrives in the hand without a draw, see `carry`).
    fn weapon_in_hand(&self) -> Option<u64> {
        self.drawn
            .map(|(weapon, _)| weapon)
            .or_else(|| crate::carry::held_items(&self.worn).into_iter().next())
    }
}

mod fighting;

struct HeldPanel {
    replicant: u64,
    session: u64,
    panel: CallPanel,
}

fn rotation([x, y, z, w]: [f64; 4]) -> Rotation {
    Rotation {
        x: x as f32,
        y: y as f32,
        z: z as f32,
        w: w as f32,
    }
}

impl From<&InputFrame> for Input {
    fn from(frame: &InputFrame) -> Self {
        let quaternion = |turn: &Rotation| [turn.x, turn.y, turn.z, turn.w].map(f64::from);
        let client_position = frame.client_position.as_ref().map(|client| ClientPosition {
            zone_host: client.zone_host_id,
            position: [client.x, client.y, client.z],
            base_rotation: client
                .base_rotation
                .as_ref()
                .map_or([0.0, 0.0, 0.0, 1.0], quaternion),
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
            mobiglas: frame.mobiglas,
            fly_mode: frame.fly_mode.filter(|_| fly_allowed()),
            pose: frame.pose,
        }
    }
}

/// `SC_DGS_FLY_MODE=on|1|true|yes` lets a client's `SFlyMode` request (F3, the dev fly and no clip mode) take effect. Off by
/// default: the request is ignored, as before. A setting of the shard's DGS, so every player of that shard may fly.
pub const FLY_MODE_SETTING: &str = "SC_DGS_FLY_MODE";

fn fly_allowed() -> bool {
    std::env::var(FLY_MODE_SETTING)
        .is_ok_and(|setting| ["1", "on", "true", "yes"].contains(&setting.to_ascii_lowercase().as_str()))
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
        mobiglas_open: movement.mobiglas_open,
        pose: movement.pose,
        fly_mode: movement.fly_mode,
    }
}

impl Control {
    /// A press on an ASOP kiosk of the player's hangar: `SvRequestInteract`, answered with `ClOnInteract` carrying the
    /// request's own arguments, to the requester alone, which opens the kiosk's interface on the client (see
    /// `interaction`). The kiosk's contents are the client's own queries; nothing else is needed from the DGS.
    fn kiosk_interact(&self, request: &EntityMethodRequest) -> EntityMethodResponse {
        // The cab panel's press has no interaction-name hash and leaves the interactable's id 0 (`Interaction::parse_cab`).
        let Some(interaction) = Interaction::parse_cab(&request.args)
            .filter(|interaction| interaction.user == request.player_id)
            .or_else(|| {
                Interaction::parse(&request.args).filter(|interaction| {
                    interaction.user == request.player_id
                        && interaction.interactable == request.entity_id
                })
            })
        else {
            warn!(entity = request.entity_id, player = request.player_id, args_hex = %crate::atc::hex(&request.args), "an interaction with a kiosk that does not read as one of this player's");
            return EntityMethodResponse::default();
        };
        info!(
            kiosk = request.entity_id,
            player = request.player_id,
            request_id = interaction.request_id,
            interaction = format_args!("{:#010x}", interaction.interaction),
            "kiosk used"
        );
        // As the official service does at a press: the kiosk's user is this player.
        self.outbox.send(
            (request.replicant, request.session),
            Update::Variables(use_update(
                request.entity_id,
                request.player_id,
                self.kiosk_locations.lock().unwrap().get(&request.entity_id).copied(),
                interaction_state_index(),
            )),
        );
        EntityMethodResponse {
            to_caller: vec![EntityMethodCall {
                entity_id: request.entity_id,
                method: KIOSK_ON_INTERACT,
                args: request.args.clone(),
            }],
        }
    }

    /// The other remote methods of an ASOP kiosk the player's session holds (see `kiosk_calls`). Only the ship list request
    /// is answered: on the player, with `RmTokenResponseFetchServerInfos` carrying the request's id and no landed ships, which
    /// the client's insurance component waits for before it shows the list. The rest are the client's state notes.
    async fn kiosk_method(&self, request: &EntityMethodRequest) -> EntityMethodResponse {
        let args_hex = crate::atc::hex(&request.args);
        let Some(call) = KioskCall::parse(request.method, &request.args) else {
            warn!(kiosk = request.entity_id, player = request.player_id, method = request.method, args = request.args.len(), args_hex = %args_hex, "a kiosk method that is not read");
            return EntityMethodResponse::default();
        };
        info!(kiosk = request.entity_id, player = request.player_id, call = ?call, "kiosk method");
        let (request_id, atc) = match call {
            KioskCall::RequestVehicleInfos { request_id, atc } => (request_id, atc),
            KioskCall::FetchPrices {
                request: number,
                classes,
            } => {
                let prices =
                    class_prices(&self.player_ships(request, HangarSize::Unspecified).await);
                info!(
                    kiosk = request.entity_id,
                    player = request.player_id,
                    number,
                    classes = classes.len(),
                    "ship prices answered"
                );
                let answer = EntityMethodCall {
                    entity_id: request.entity_id,
                    method: KIOSK_ON_FETCH_PRICES,
                    args: prices_reply(number, &classes, &prices),
                };
                return EntityMethodResponse {
                    to_caller: vec![answer],
                };
            }
            KioskCall::SetInteractionState { state, .. } | KioskCall::UserState { state, .. } => {
                // The authority sets the state variable the client's own value goes to, as the official service does.
                if let Some(index) = interaction_state_index() {
                    self.outbox.send(
                        (request.replicant, request.session),
                        Update::Variables(interaction_state_update(
                            request.entity_id,
                            index,
                            state,
                        )),
                    );
                }
                return EntityMethodResponse::default();
            }
            KioskCall::RequestClaim {
                request_id,
                urn,
                kind,
                ..
            } => {
                let off = std::env::var(CLAIM_RESPONSE_ENV).is_ok_and(|setting| {
                    setting.trim().eq_ignore_ascii_case(INSURANCE_RESPONSE_OFF)
                });
                let Some(base) = insurance_response_method().filter(|_| !off) else {
                    return EntityMethodResponse::default();
                };
                let ships = self.player_ships(request, HangarSize::Unspecified).await;
                let Some(ship) = ships
                    .iter()
                    .filter_map(|view| view.ship.as_ref())
                    .find(|ship| ship.entitlement_urn == urn)
                else {
                    warn!(kiosk = request.entity_id, player = request.player_id, urn = %urn, "a claim for an entitlement that is none of the player's ships: answered as failed");
                    return self.claim_failed(request, base, &request_id);
                };
                let vehicle = ship.geid;
                let session = (request.replicant, request.session);
                if self.retrievals.refuses_delivery(session, request.player_id, vehicle).await {
                    warn!(kiosk = request.entity_id, player = request.player_id, vehicle, "a delivery of a ship that is on the pad or moving to storage: refused");
                    return self.claim_failed(request, base, &request_id);
                }
                if !self.deliver_to_kiosk(request, ship, &request_id).await {
                    return self.claim_failed(request, base, &request_id);
                }
                info!(kiosk = request.entity_id, player = request.player_id, urn = %urn, kind, vehicle, "claim answered (RmTokenResponseClaim on the player; no ship is spawned yet)");
                let answer = EntityMethodCall {
                    entity_id: request.player_id,
                    method: base + CLAIM_RESPONSE_OFFSET,
                    args: claim_response(&request_id, vehicle, request.player_id, self.kiosk_locations.lock().unwrap().get(&request.entity_id).copied().unwrap_or_default()),
                };
                return EntityMethodResponse {
                    to_caller: vec![answer],
                };
            }
            KioskCall::StoreVehicle { vehicle } => {
                let session = (request.replicant, request.session);
                match self.retrievals.store(session, request.player_id, vehicle).await {
                    Ok(()) => info!(kiosk = request.entity_id, player = request.player_id, vehicle, "ship store started: the pad goes down with it"),
                    Err(status) => warn!(kiosk = request.entity_id, player = request.player_id, vehicle, %status, "ship store refused"),
                }
                return EntityMethodResponse::default();
            }
            _ => return EntityMethodResponse::default(),
        };
        let Some(method) = insurance_response_method() else {
            warn!(kiosk = request.entity_id, atc, env = INSURANCE_RESPONSE_METHOD_ENV, "the ship list request is not answered: the reply is switched off by SC_DGS_INSURANCE_RESPONSE_METHOD");
            return EntityMethodResponse::default();
        };
        let (can_dock, destroyed, landed) = self.kiosk_ships(request).await;
        let answer = EntityMethodCall {
            entity_id: request.player_id,
            method,
            args: server_infos_landed(&request_id, &can_dock, &destroyed, &landed),
        };
        info!(
            kiosk = request.entity_id, player = request.player_id, method, request_guid = %crate::atc::hex(&request_id), can_dock = can_dock.len(), destroyed = destroyed.len(),
            "ship list request answered (RmTokenResponseFetchServerInfos on the player)",
        );
        EntityMethodResponse {
            to_caller: vec![answer],
        }
    }

    /// The mobiGlas Vehicle Manager's ship list request: `RmAuthorityRequestVehicleInfos` on the player's `PersonalMobiGlas_PU`
    /// (the client's `OnRequestFetchVehicles`, as at a kiosk), answered as at a kiosk with `RmTokenResponseFetchServerInfos` on
    /// the player: the client's insurance component holds the list until it arrives. `None` for any other call.
    async fn mobiglas_vehicle_infos(&self, request: &EntityMethodRequest) -> Option<EntityMethodResponse> {
        use crate::kiosk_calls::{MOBIGLAS_CLASS_CRC, MOBIGLAS_REQUEST_VEHICLE_INFOS, REQUEST_VEHICLE_INFOS_LEN};
        if request.method != MOBIGLAS_REQUEST_VEHICLE_INFOS || request.args.len() != REQUEST_VEHICLE_INFOS_LEN {
            return None;
        }
        let is_mobiglas = self
            .worn
            .lock()
            .unwrap()
            .get(&request.player_id)
            .and_then(|held| held.worn.get(request.entity_id).map(|item| item.class_crc))
            == Some(MOBIGLAS_CLASS_CRC);
        if !is_mobiglas {
            return None;
        }
        let request_id: [u8; GUID_LEN] = request.args[..GUID_LEN].try_into().ok()?;
        let Some(method) = insurance_response_method() else {
            warn!(entity = request.entity_id, player = request.player_id, env = INSURANCE_RESPONSE_METHOD_ENV, "the mobiGlas ship list request is not answered: the reply is switched off");
            return Some(EntityMethodResponse::default());
        };
        // The list is the player's own ships, none marked as fitting a hangar: there is no kiosk (an ATC id of 0).
        let (can_dock, destroyed) = ship_lists(&self.player_ships(request, HangarSize::Unspecified).await);
        info!(
            mobiglas = request.entity_id, player = request.player_id, method, request_guid = %crate::atc::hex(&request_id), can_dock = can_dock.len(), destroyed = destroyed.len(),
            "mobiGlas ship list request answered (RmTokenResponseFetchServerInfos on the player)",
        );
        Some(EntityMethodResponse {
            to_caller: vec![EntityMethodCall {
                entity_id: request.player_id,
                method,
                args: server_infos(&request_id, &can_dock, &destroyed),
            }],
        })
    }

    /// The failed answer of a claim: the client keeps a request that gets none pending for good, and the ship's row then reads
    /// "awaiting delivery" with its buttons locked.
    fn claim_failed(&self, request: &EntityMethodRequest, base: u32, claim: &[u8; GUID_LEN]) -> EntityMethodResponse {
        let location = self.kiosk_locations.lock().unwrap().get(&request.entity_id).copied().unwrap_or_default();
        EntityMethodResponse {
            to_caller: vec![EntityMethodCall {
                entity_id: request.player_id,
                method: base + CLAIM_RESPONSE_OFFSET,
                args: crate::kiosk_calls::claim_failure(claim, request.player_id, location),
            }],
        }
    }

    /// Moves `ship` to the location of the kiosk the player claims at, as the delivery of a ship stored elsewhere: saved in the
    /// ship's record and in the graph by sc-entitygraph, so the next list finds it here. `true` when the ship is there after
    /// the call (it was, or now is); `false` when it could not be moved, which is logged.
    async fn deliver_to_kiosk(
        &self,
        request: &EntityMethodRequest,
        ship: &sc_api::internal::ShipRecord,
        claim: &[u8; GUID_LEN],
    ) -> bool {
        let Some(location) = self
            .kiosk_locations
            .lock()
            .unwrap()
            .get(&request.entity_id)
            .copied()
        else {
            warn!(
                kiosk = request.entity_id,
                "the kiosk's location is not known: no delivery"
            );
            return false;
        };
        let Some(ships) = self.ships.clone() else {
            return false;
        };
        match ship.state() {
            // The record says the ship is live, and it is not on this hangar's pad (`refuses_delivery` saw to that): the shard
            // that had it is gone. It is put away at the kiosk's location, which is what the claim asks for.
            ShipState::Deployed => {
                let ask = ReportShipRequest {
                    command_id: format!("{DELIVERY_COMMAND_PREFIX}{}{STALE_COMMAND_SUFFIX}", crate::atc::hex(claim)),
                    owner_id: request.player_id,
                    geid: ship.geid,
                    state: ShipState::Stored as i32,
                    location_id: location,
                    now_ms: CLOCK_OF_THE_SERVICE,
                };
                return match tokio::time::timeout(ITEMS_TIMEOUT, ShipBackend::report(&ships, ask)).await {
                    Ok(Ok(_)) => {
                        info!(player = request.player_id, ship = ship.geid, location, "a ship the record held live is stored at the kiosk's location");
                        true
                    }
                    Ok(Err(status)) => {
                        warn!(player = request.player_id, ship = ship.geid, %status, "a ship the record held live could not be stored at the kiosk's location");
                        false
                    }
                    Err(_) => {
                        warn!(player = request.player_id, ship = ship.geid, "storing the ship at the kiosk's location timed out");
                        false
                    }
                };
            }
            ShipState::Stored if ship.location_id == location => {
                // Delivered here already: nothing moves, and the answer lets the client's request end.
                info!(player = request.player_id, ship = ship.geid, location, "the ship is at the kiosk's location already: nothing to deliver");
                return true;
            }
            ShipState::Stored => {}
            state => {
                warn!(player = request.player_id, ship = ship.geid, ?state, "a ship that is not stored cannot be delivered");
                return false;
            }
        }
        let ask = DeliverShipRequest {
            command_id: format!("{DELIVERY_COMMAND_PREFIX}{}", crate::atc::hex(claim)),
            owner_id: request.player_id,
            geid: ship.geid,
            destination_location_id: location,
            now_ms: CLOCK_OF_THE_SERVICE,
        };
        match tokio::time::timeout(ITEMS_TIMEOUT, ShipBackend::deliver(&ships, ask)).await {
            Ok(Ok(_)) => {
                info!(
                    player = request.player_id,
                    ship = ship.geid,
                    from = ship.location_id,
                    to = location,
                    "ship delivered to the kiosk's location"
                );
                true
            }
            Ok(Err(status)) => {
                warn!(player = request.player_id, ship = ship.geid, %status, "the ship could not be delivered to the kiosk's location");
                false
            }
            Err(_) => {
                warn!(
                    player = request.player_id,
                    ship = ship.geid,
                    "the delivery of the ship timed out"
                );
                false
            }
        }
    }

    /// The classes of the player's ships that fit the hangar of `request`'s kiosk, and the ships destroyed (see
    /// `kiosk_calls::ship_lists`). Both empty without sc-entitygraph, while it cannot say, or for a kiosk of no known hangar.
    async fn kiosk_ships(
        &self,
        request: &EntityMethodRequest,
    ) -> (Vec<[u8; GUID_LEN]>, Vec<u64>, Vec<crate::kiosk_calls::LandedRecord>) {
        let size = self
            .kiosk_hangars
            .lock()
            .unwrap()
            .get(&request.entity_id)
            .map_or(HangarSize::Unspecified, |class| hangar_size_of(class));
        // A kiosk of no known hangar size lists no ship that fits.
        if size == HangarSize::Unspecified {
            return (Vec::new(), Vec::new(), Vec::new());
        }
        let ships = self.player_ships(request, size).await;
        let (can_dock, destroyed) = ship_lists(&ships);
        for view in &ships {
            let Some(ship) = view.ship.as_ref() else {
                continue;
            };
            info!(
                kiosk = request.entity_id,
                player = request.player_id,
                ship = ship.geid,
                class = %ship.entity_class_guid,
                wire = %crate::kiosk_calls::class_guid_wire(&ship.entity_class_guid).map_or_else(String::new, |wire| crate::atc::hex(&wire)),
                listed = crate::kiosk_calls::class_guid_wire(&ship.entity_class_guid).is_some_and(|wire| can_dock.contains(&wire)),
                state = ship.state,
                pad = ship.pad_size,
                fits = view.fits,
                urn = %ship.entitlement_urn,
                "the kiosk's list holds a ship"
            );
        }
        // The ship on the raised pad is listed as landed at this ATC: the ASOP shows it retrieved, with Store.
        let landed = self
            .retrievals
            .landed((request.replicant, request.session), request.player_id)
            .await;
        let held = landed.is_some() || self.retrievals.holds((request.replicant, request.session), request.player_id);
        let records = landed_records(&ships, landed, request.player_id);
        for record in &records {
            info!(
                kiosk = request.entity_id,
                player = request.player_id,
                ship = record.entity_id,
                atc = record.atc,
                area = record.area,
                flags = crate::kiosk_calls::LANDED_FLAGS,
                "the kiosk's ship list holds a landed record"
            );
        }
        if held && records.is_empty() {
            info!(
                kiosk = request.entity_id,
                player = request.player_id,
                "a retrieval is held but no ship stands landed for this request"
            );
        }
        info!(
            kiosk = request.entity_id,
            player = request.player_id,
            can_dock = can_dock.len(),
            destroyed = destroyed.len(),
            landed = records.len(),
            "the kiosk's ship list answered"
        );
        (can_dock, destroyed, records)
    }

    /// The ships of the player at `request`, those that fit a hangar of `size` marked (`Unspecified`: every ship, none marked).
    /// Empty without sc-entitygraph or while it cannot say.
    async fn player_ships(&self, request: &EntityMethodRequest, size: HangarSize) -> Vec<ShipView> {
        let Some(ships) = self.ships.clone() else {
            return Vec::new();
        };
        let ask = ListShipsRequest {
            owner_id: request.player_id,
            location_id: ALL_LOCATIONS,
            hangar_size: size as i32,
            exact_size: false,
            now_ms: CLOCK_OF_THE_SERVICE,
        };
        match tokio::time::timeout(ITEMS_TIMEOUT, ShipBackend::list(&ships, ask)).await {
            Ok(Ok(views)) => views,
            Ok(Err(status)) => {
                warn!(player = request.player_id, %status, "the player's ships could not be listed for the kiosk");
                Vec::new()
            }
            Err(_) => {
                warn!(
                    player = request.player_id,
                    "the player's ships were not listed in time for the kiosk"
                );
                Vec::new()
            }
        }
    }

    /// `RmAuthorityRequestCommand` of the weapon the player has drawn (see `weapon_command`): the trigger's press and release
    /// are answered with the multicast that lets the client's weapon shoot. `None` for any other call.
    fn weapon_command(&self, request: &EntityMethodRequest) -> Option<EntityMethodResponse> {
        use crate::weapon_command::{
            aim_answer, answer, echo_answer, echo_unknown_commands, is_known_command, set_ammo_container, CommandRequest,
            CONTAINER_TICK_LEAD, RECEIVE_COMMAND_METHOD, REQUEST_COMMAND_METHOD,
        };
        use crate::{loadout::port_attach_id, reload::MAGAZINE_PORT};
        if request.method == crate::weapon_command::STATE_REQUEST_METHOD {
            return self.weapon_state_reply(request);
        }
        if request.method != REQUEST_COMMAND_METHOD {
            let in_hand = self.worn.lock().unwrap().get(&request.player_id).and_then(HeldWorn::weapon_in_hand);
            if in_hand == Some(request.entity_id) {
                info!(
                    player = request.player_id,
                    weapon = request.entity_id,
                    method = request.method,
                    args = %crate::atc::hex(&request.args),
                    "a call on the drawn weapon the DGS does not handle"
                );
            }
            return None;
        }
        let command = CommandRequest::parse(&request.args)?;
        let mut calls = Vec::new();
        {
            let mut all = self.worn.lock().unwrap();
            let held = all.get_mut(&request.player_id).filter(|held| {
                (held.replicant, held.session) == (request.replicant, request.session)
            })?;
            let weapon = held
                .weapon_in_hand()
                .filter(|weapon| *weapon == request.entity_id)?;
            held.last_weapon_tick = Some((command.tick, std::time::Instant::now()));
            // The magazine in the weapon is the container the client fires from; it is told once for each magazine, with the
            // rounds the session knows (else a full one), at a tick just ahead of the client's.
            let magazine = held
                .worn
                .child_at(weapon, port_attach_id(MAGAZINE_PORT))
                .map(|item| item.geid);
            if let Some(magazine) =
                magazine.filter(|magazine| held.container_sent != Some((weapon, *magazine)))
            {
                let rounds = held
                    .ammo
                    .get(magazine)
                    .unwrap_or_else(|| held.combat.capacities.get(magazine));
                calls.push(set_ammo_container(
                    weapon,
                    command.tick.wrapping_add(CONTAINER_TICK_LEAD),
                    magazine,
                    rounds,
                ));
                held.container_sent = Some((weapon, magazine));
                info!(
                    player = request.player_id,
                    weapon,
                    magazine,
                    rounds,
                    tick = command.tick,
                    "the weapon's ammo container set"
                );
            }
        }
        calls.extend(answer(request.entity_id, &command));
        calls.extend(aim_answer(request.entity_id, &command, &request.args));
        if !is_known_command(&command) {
            // A command the DGS does not know (its bytes are logged to name it): acknowledged with its own bytes only when
            // `SC_DGS_WEAPON_ECHO_COMMANDS` is on (not verified).
            warn!(
                player = request.player_id,
                weapon = request.entity_id,
                tick = command.tick,
                command = command.command,
                trigger = command.trigger,
                args = %crate::atc::hex(&request.args),
                echoed = echo_unknown_commands(),
                "a weapon command the DGS does not know"
            );
            calls.extend(echo_answer(request.entity_id, &request.args).filter(|_| echo_unknown_commands()));
        }
        if let Some(reply) = self.pending_state_reply(request, command.tick) {
            calls.extend(reply);
        }
        info!(
            player = request.player_id,
            weapon = request.entity_id,
            tick = command.tick,
            command = command.command,
            trigger = command.trigger,
            calls = calls.len(),
            "a weapon command"
        );
        Some(EntityMethodResponse { to_caller: calls })
    }

    /// `RmAuthorityRequestState` of the weapon the player has drawn (method 8 on a handheld): the client asks for the weapon's state
    /// when it is drawn, and the official server answers with the state batch (`weapon_command::state_reply`), stamped just after the
    /// client's last command. `None` when the call is not for the drawn weapon, there is no known tick, or the reply is switched off
    /// (`SC_DGS_WEAPON_STATE_REPLY=off`).
    fn weapon_state_reply(&self, request: &EntityMethodRequest) -> Option<EntityMethodResponse> {
        use crate::weapon_command::{estimate_tick, state_reply_enabled, STATE_TICK_LEAD};
        if !state_reply_enabled() {
            return None;
        }
        let mut all = self.worn.lock().unwrap();
        let held = all
            .get_mut(&request.player_id)
            .filter(|held| (held.replicant, held.session) == (request.replicant, request.session))?;
        let weapon = held.weapon_in_hand().filter(|weapon| *weapon == request.entity_id)?;
        let Some((last, at)) = held.last_weapon_tick else {
            // The first draw: the client asks for the state before it has sent any command (the first command comes later), so the reply waits for that command's tick.
            held.state_pending = Some(weapon);
            info!(player = request.player_id, weapon, "a weapon state request before any weapon command: the reply waits for the first command's tick");
            return None;
        };
        let tick = estimate_tick(last, at.elapsed()).wrapping_add(STATE_TICK_LEAD);
        let calls = Self::state_calls(held, weapon, tick, request.player_id)?;
        Some(EntityMethodResponse { to_caller: calls })
    }

    /// The state reply that waited for a tick (see `weapon_state_reply`), stamped after the command that gave it.
    fn pending_state_reply(&self, request: &EntityMethodRequest, tick: u32) -> Option<Vec<EntityMethodCall>> {
        let mut all = self.worn.lock().unwrap();
        let held = all.get_mut(&request.player_id)?;
        if held.state_pending != Some(request.entity_id) {
            return None;
        }
        held.state_pending = None;
        Self::state_calls(held, request.entity_id, tick.wrapping_add(crate::weapon_command::STATE_TICK_LEAD), request.player_id)
    }

    fn state_calls(held: &HeldWorn, weapon: u64, tick: u32, player: u64) -> Option<Vec<EntityMethodCall>> {
        use crate::weapon_command::{state_reply, WEAPON_STATE_FLAGS};
        use crate::{loadout::port_attach_id, reload::MAGAZINE_PORT};
        let Some(magazine) = held.worn.child_at(weapon, port_attach_id(MAGAZINE_PORT)).map(|item| item.geid) else {
            warn!(player, weapon, "a weapon state request of a weapon with no magazine bound: not answered");
            return None;
        };
        let capacity = held.combat.capacities.get(magazine);
        let rounds = held.ammo.get(magazine).unwrap_or(capacity);
        let calls = state_reply(weapon, tick, magazine, rounds, capacity, WEAPON_STATE_FLAGS);
        info!(player, weapon, magazine, rounds, capacity, tick, calls = calls.len(), "the weapon's state sent");
        Some(calls)
    }

    /// A call on a carriage's cab panel. Its press is read as an interaction, whichever method it came on (the panel's
    /// method indices are not known yet, so the first press's are logged), and answered with the next method, which is
    /// `ClOnInteract` for `SvRequestInteract`.
    fn cab_panel_method(
        &self,
        request: EntityMethodRequest,
    ) -> Result<Response<EntityMethodResponse>, Status> {
        info!(
            entity = request.entity_id, method = request.method, player = request.player_id, args = request.args.len(), args_hex = %crate::atc::hex(&request.args),
            "a call on an entity that is not a call panel (an ASOP kiosk or ATC when its id is one the join bound), read as a cab panel press",
        );
        let Some(interaction) = Interaction::parse(&request.args).filter(|interaction| {
            interaction.user == request.player_id && interaction.interactable == request.entity_id
        }) else {
            return Err(Status::not_found(format!(
                "{} holds no panel {} for this session",
                self.settings.name, request.entity_id
            )));
        };
        if !self.elevators.lock().unwrap().cab_panel_press(
            request.player_id,
            request.entity_id,
            interaction.point,
        ) {
            return Err(Status::not_found(format!(
                "{} has no cab panel {} with that point",
                self.settings.name, request.entity_id
            )));
        }
        info!(
            panel = request.entity_id,
            method = request.method,
            point = format_args!("{:#010x}", interaction.point),
            "cab panel pressed"
        );
        let answer = EntityMethodCall {
            entity_id: request.entity_id,
            method: request.method + 1,
            args: request.args,
        };
        Ok(Response::new(EntityMethodResponse {
            to_caller: vec![answer],
        }))
    }

    /// A draw moves the weapon to a hand port, a holster puts it back (see `draw`); the client is sent the rebind. A draw
    /// while a weapon is in the hand puts that one back first: the client swaps weapons with a single draw.
    fn draw_or_holster(&self, player: u64, replicant: u64, session_id: u64, frame: &InputFrame) {
        let session = (replicant, session_id);
        let mut moves = Vec::new();
        {
            let mut all = self.worn.lock().unwrap();
            let Some(held) = all
                .get_mut(&player)
                .filter(|held| (held.replicant, held.session) == session)
            else {
                return;
            };
            let putting_away = frame.pose == Some(crate::draw::POSE_NO_WEAPON);
            let drawing =
                frame.pose == Some(crate::draw::POSE_WEAPON) && frame.unholster_owner != 0;
            if putting_away || drawing {
                if let Some((weapon, from)) = held.drawn.take() {
                    match crate::draw::holster(&held.worn, weapon, from) {
                        Some(back) => {
                            held.worn = back.worn.clone();
                            moves.push(back);
                        }
                        None => held.drawn = Some((weapon, from)),
                    }
                }
            }
            if drawing && held.drawn.is_none() {
                if let Some(draw) = crate::draw::unholster(
                    &held.worn,
                    player,
                    frame.unholster_owner,
                    frame.unholster_port,
                ) {
                    held.drawn = Some((draw.weapon, draw.from));
                    held.worn = draw.worn.clone();
                    moves.push(draw);
                }
            }
        }
        for (index, moved) in moves.into_iter().enumerate() {
            info!(player = player, weapon = moved.weapon, pose = ?frame.pose, owner = frame.unholster_owner, port = frame.unholster_port, "a weapon moved between holster and hand");
            self.outbox.send(
                session,
                Update::Unbinds(EntityUnbinds {
                    entity_ids: moved.unbinds,
                }),
            );
            let key = format!("draw-{}-{}-{index}", moved.weapon, frame.time);
            self.outbox.send(
                session,
                Update::Binds(EntityBinds {
                    key,
                    binds: moved.binds.into_iter().map(EntityBind::from).collect(),
                }),
            );
        }
    }

    /// A reload of the weapon in the player's hand puts the magazine the client chose in it (see `reload`).
    fn reload_weapon(&self, player: u64, replicant: u64, session_id: u64, frame: &InputFrame) {
        let session = (replicant, session_id);
        let done = {
            let mut all = self.worn.lock().unwrap();
            let Some(held) = all
                .get_mut(&player)
                .filter(|held| (held.replicant, held.session) == session)
            else {
                return;
            };
            let Some(weapon) = held.weapon_in_hand() else {
                info!(player = player, "a reload with no weapon drawn");
                return;
            };
            // What the client reports of the magazine it chose (rounds and capacity) is the count to keep for it.
            if let (Some(rounds), true) = (frame.reload_ammo_count, frame.reload_ammo_capacity > 0)
            {
                held.ammo.set(frame.reload_magazine, rounds.max(0) as u32);
            }
            match crate::reload::reload(
                &held.worn,
                weapon,
                frame.reload.unwrap_or_default(),
                frame.reload_magazine,
            ) {
                Ok(mut done) => {
                    done.loaded_rounds = held.ammo.get(done.loaded);
                    done.spent_rounds = done.spent.and_then(|spent| held.ammo.get(spent));
                    held.worn = done.worn.clone();
                    done
                }
                Err(refusal) => {
                    info!(
                        player = player,
                        weapon,
                        magazine = frame.reload_magazine,
                        ?refusal,
                        "a reload that was not done"
                    );
                    return;
                }
            }
        };
        info!(player = player, loaded = done.loaded, spent = ?done.spent, loaded_rounds = ?done.loaded_rounds, spent_rounds = ?done.spent_rounds, "a weapon reloaded");
        self.outbox.send(
            session,
            Update::Unbinds(EntityUnbinds {
                entity_ids: done.unbinds,
            }),
        );
        let key = format!("reload-{}-{}", done.loaded, frame.time);
        self.outbox.send(
            session,
            Update::Binds(EntityBinds {
                key,
                binds: done.binds.into_iter().map(EntityBind::from).collect(),
            }),
        );
        // A count the session knows goes to the client with the rebind, as the replicated `ac.ammoCount` of the magazine, for the classes whose index is verified.
        for (magazine, rounds) in [
            (done.loaded, done.loaded_rounds),
            (done.spent.unwrap_or_default(), done.spent_rounds),
        ] {
            let class = done.worn.get(magazine).map_or(0, |item| item.class_crc);
            let update = rounds
                .filter(|_| magazine != 0)
                .and_then(|rounds| crate::reload::ammo_update(magazine, class, rounds));
            if let Some(update) = update {
                self.outbox.send(session, Update::Variables(update));
            }
        }
    }

    /// Applies an inventory request of a player this DGS holds for the session, and sends the binds and unbinds it made.
    async fn run_inventory(
        &self,
        player: u64,
        replicant: u64,
        session_id: u64,
        inventory: &InventoryRequest,
    ) -> (bool, u64) {
        let session = (replicant, session_id);
        let held = self
            .worn
            .lock()
            .unwrap()
            .get(&player)
            .filter(|held| (held.replicant, held.session) == session)
            .map(|held| held.worn.clone());
        let outcome = match held {
            Some(worn) => {
                let spot = self
                    .movements
                    .lock()
                    .unwrap()
                    .get(player)
                    .map(|movement| equip::Spot {
                        zone: movement.parent,
                        position: movement.position,
                        rotation: movement.base_rotation(),
                    });
                let origin = Origin {
                    replicant: replicant,
                    session: session_id,
                    shard: &self.settings.shard_id,
                    spot,
                };
                equip::apply(&GraphItems(self.graph.clone()), &worn, origin, inventory).await
            }
            None => {
                warn!(player = player, "an inventory request of a player this DGS does not hold for the session, failed");
                equip::Outcome::default()
            }
        };
        let mut outcome = outcome;
        if let Some(worn) = outcome.worn.as_mut() {
            // A helmet put on gets the visor the one taken off took with it.
            let index = crate::player::geid_index(player);
            if let Some(visor) = index.and_then(|index| loadout::helmet_visor_bind(worn, index)) {
                worn.attach(crate::worn::WornItem {
                    geid: visor.entity_id,
                    class_crc: visor.class_crc,
                    parent: visor.parent_id,
                    port_crc: visor.parent_attach_id,
                });
                outcome.binds.push(visor);
            }
        }
        if let Some(worn) = outcome.worn {
            if let Some(held) = self
                .worn
                .lock()
                .unwrap()
                .get_mut(&player)
                .filter(|held| (held.replicant, held.session) == session)
            {
                held.worn = worn;
            }
        }
        if !outcome.unbinds.is_empty() {
            self.outbox.send(
                session,
                Update::Unbinds(EntityUnbinds {
                    entity_ids: outcome.unbinds.clone(),
                }),
            );
        }
        if !outcome.binds.is_empty() {
            let key = format!("equip-{}", inventory.request_id);
            let bound: Vec<(u64, u64, u32)> = outcome
                .binds
                .iter()
                .map(|bind| (bind.entity_id, bind.parent_id, bind.parent_attach_id))
                .collect();
            info!(
                player = player,
                request_id = inventory.request_id,
                ?bound,
                "binding the equipped item (entity, parent, port key)"
            );
            self.outbox.send(
                session,
                Update::Binds(EntityBinds {
                    key,
                    binds: outcome.binds.into_iter().map(EntityBind::from).collect(),
                }),
            );
        }
        info!(
            player = player, request_id = inventory.request_id, kind = inventory.kind, result = outcome.result, item = outcome.item,
            unbound = ?outcome.unbinds, "inventory request completed",
        );
        let pocket = inventory.detail.as_ref().is_some_and(|detail| {
            equip::is_pocket(detail.port_path.rsplit(':').next().unwrap_or_default())
        });
        if pocket && outcome.result && outcome.item != 0 {
            self.pocketed.lock().unwrap().insert(player, outcome.item);
        }
        (outcome.result, outcome.item)
    }

    /// A call on the client's own player other than its action frames: an inventory request is applied (an equip attaches
    /// the item to its port and has the client shown it, a store takes it off, a move changes its inventory, see `equip`)
    /// and completed with the result, so that the client goes on to the next one, whatever happened to this. Anything
    /// else gets no answer.
    async fn player_method(&self, request: &EntityMethodRequest) -> EntityMethodResponse {
        // The calls the official server never answers (open and close inventory, spectator mode, AI signals...): read, kept, no reply.
        if self.player_calls.handle(request.player_id, request.method, &request.args) {
            return EntityMethodResponse::default();
        }
        let Some(inventory) = InventoryRequest::parse(&request.args, request.player_id) else {
            info!(player = request.player_id, method = request.method, args = request.args.len(), args_hex = %crate::atc::hex(&request.args), "a call on the player that is not an inventory request");
            return EntityMethodResponse::default();
        };
        info!(
            player = request.player_id, method = request.method, request_id = inventory.request_id, kind = inventory.kind,
            source = ?inventory.source.id(), target = ?inventory.target.id(), detail = ?inventory.detail, "inventory request",
        );
        if inventory.detail.is_none() {
            info!(player = request.player_id, request_id = inventory.request_id, args_hex = %crate::atc::hex(&request.args), "an inventory request whose detail did not parse");
        }
        let (result, item) = self
            .run_inventory(
                request.player_id,
                request.replicant,
                request.session,
                &inventory,
            )
            .await;
        let answer = EntityMethodCall {
            entity_id: request.entity_id,
            method: request.method + COMPLETED_AFTER_PROCESS,
            args: completed_args(result, inventory.request_id, item),
        };
        EntityMethodResponse {
            to_caller: vec![answer],
        }
    }

    pub fn new(settings: Settings) -> Self {
        let system_root = format!("{}system", settings.system.to_ascii_lowercase());
        let owned = settings.roots.contains(&system_root);
        let solar_system = SOLAR_SYSTEMS
            .iter()
            .position(|system| owned && system.name.eq_ignore_ascii_case(&settings.system));
        let planets = planets().into_iter().filter(|planet| {
            settings.roots.contains(&planet.name) && planet_id(&planet.name).is_some()
        });
        let planet_zone = settings.planet_zone;
        let planets = if settings.planet_zone {
            planets.count() as u64
        } else {
            0
        };
        let systems = solar_system.map_or(0, |_| 2);
        let boot_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
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
            kiosks: Mutex::default(),
            kiosk_hangars: Mutex::default(),
            kiosk_locations: Mutex::default(),
            player_calls: Default::default(),
            pocketed: Mutex::default(),
            outbox: outbox.clone(),
            instances: Arc::new(Mutex::new(crate::hangar_instance::HangarInstances::new(
                outbox.clone(),
                planet_zone,
            ))),
            retrievals: Arc::new(crate::retrieve::Retrievals::new(outbox.clone())),
            elevators: Arc::new(Mutex::new(Elevators::new(outbox))),
            gamedata: None,
            customizer: None,
            identity: None,
            ledger: None,
            graph: None,
            ships: None,
            worn: Mutex::default(),
        }
    }

    /// Has `gamedata` give the collision of a hangar the first time a player joins in it.
    pub fn with_gamedata(mut self, gamedata: crate::gamedata::GameData) -> Self {
        self.gamedata = Some(gamedata);
        self
    }

    /// Builds the collision of the hangar `request` puts the player in, if it is not built yet, so the player is never placed
    /// in a hangar without its walls. A hangar's collision is over a million triangles, which is why only a location's first
    /// is loaded at startup. A failure is logged: the player then moves with no collision, as with a hangar the build lacks.
    async fn load_collision(&self, request: &JoinPlayerRequest) {
        let Some(gamedata) = &self.gamedata else { return };
        let size = HangarSize::try_from(request.hangar_size).unwrap_or_default();
        let Some(home) = player_home(request.account_id, request.location_id, size, self.settings.planet_zone) else { return };
        let hash = home.location.socpak_hash;
        if crate::physics::world::World::loaded(hash).is_some() {
            return;
        }
        let started = std::time::Instant::now();
        match gamedata.hangar_collision(hash).await {
            Ok(world) => info!(hash, triangles = world.triangles(), elapsed = ?started.elapsed(), "hangar collision loaded for a join"),
            Err(error) => warn!(hash, %error, "no collision for the player's hangar"),
        }
    }

    /// Asks `customizer` for the characters players saved.
    pub fn with_customizer(
        mut self,
        customizer: CharacterCustomizerInternalClient<Channel>,
    ) -> Self {
        self.customizer = Some(customizer);
        self
    }

    /// Has `graph` keep the items players wear.
    pub fn with_graph(mut self, graph: EntityGraphInternalClient<Channel>) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Has `ships` answer the ship lists of the ASOP kiosks.
    pub fn with_ships(mut self, ships: ShipsInternalClient<Channel>) -> Self {
        self.ships = Some(ships);
        self
    }

    /// The ids sc-entitygraph keeps the player's items under, one per item of `entities.loadout`: made on the first join
    /// with this loadout and the same afterwards. `None` without sc-entitygraph or while it cannot say.
    async fn item_ids(
        &self,
        entities: &PlayerEntities,
    ) -> Option<(Vec<u64>, Vec<sc_api::internal::EquippedItem>)> {
        let mut graph = self.graph.clone()?;
        let player_id = entities.player.entity_id;
        let request = EnsureLoadoutRequest {
            command_id: loadout::command_id(player_id, &entities.loadout),
            player_id,
            items: loadout::request_items(&entities.loadout),
        };
        // A join waits for this, so a slow sc-entitygraph costs the player the made up ids rather than the join.
        match tokio::time::timeout(ITEMS_TIMEOUT, graph.ensure_loadout(request))
            .await
            .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("no answer")))
        {
            Ok(reply) => {
                let reply = reply.into_inner();
                (reply.geids.len() == entities.loadout.len())
                    .then_some((reply.geids, reply.equipped))
            }
            Err(status) => {
                warn!(player_id, %status, "no stored items from sc-entitygraph, the player wears made up ones");
                None
            }
        }
    }

    /// Asks `ledger` for the balances of the players' wallets.
    pub fn with_ledger(mut self, ledger: LedgerInternalClient<Channel>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// The variables of the wallet of `account`'s character: its balances, or empty ones when the ledger cannot say.
    async fn wallet(&self, account: u64) -> Vec<crate::entity::Snapshot> {
        match (
            self.ledger.clone(),
            player_geid(account),
            u32::try_from(account),
        ) {
            (Some(mut ledger), Some(geid), Ok(account)) => {
                crate::wallet::load(&mut ledger, geid, account).await
            }
            _ => crate::wallet::snapshots(&[]),
        }
    }

    /// Asks `identity` for the players' in-game badges.
    pub fn with_identity(mut self, identity: IdentityInternalClient<Channel>) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The in-game badges of `account`, by id, as identity has them. None when it cannot say.
    async fn badges(&self, account: u64) -> Vec<u32> {
        let (Some(mut identity), Ok(account_id)) = (self.identity.clone(), u32::try_from(account))
        else {
            return Vec::new();
        };
        match identity.get_account(GetAccountRequest { account_id }).await {
            Ok(reply) => reply
                .into_inner()
                .account
                .map(|account| account.game_badges.iter().map(|badge| badge.id).collect())
                .unwrap_or_default(),
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
        self.movements
            .lock()
            .unwrap()
            .retain(|player| holdings.holds(player));
    }

    /// What the tick steps, with the movements.
    pub fn instances(&self) -> Arc<Mutex<crate::hangar_instance::HangarInstances>> {
        self.instances.clone()
    }

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
        self.settings.system.eq_ignore_ascii_case(&place.system)
            && self.settings.roots.contains(&place.root())
    }
}

#[tonic::async_trait]
impl DgsControl for Control {
    type SessionUpdatesStream = tokio_stream::wrappers::UnboundedReceiverStream<Sent>;

    async fn session_updates(
        &self,
        request: Request<SessionUpdatesRequest>,
    ) -> Result<Response<Self::SessionUpdatesStream>, Status> {
        let request = request.into_inner();
        info!(
            replicant = request.replicant,
            session = request.session,
            "session subscribed to its updates"
        );
        let receiver = self.outbox.subscribe((request.replicant, request.session));
        Ok(Response::new(
            tokio_stream::wrappers::UnboundedReceiverStream::new(receiver),
        ))
    }

    async fn status(
        &self,
        _: Request<DgsStatusRequest>,
    ) -> Result<Response<DgsStatusResponse>, Status> {
        Ok(Response::new(self.status()))
    }

    async fn locate_home(
        &self,
        request: Request<LocateHomeRequest>,
    ) -> Result<Response<LocateHomeResponse>, Status> {
        let place = governed_place(request.into_inner().location_id);
        Ok(Response::new(LocateHomeResponse {
            system: place.system,
            planet: place.planet.unwrap_or_default(),
        }))
    }

    async fn shard_entities(
        &self,
        _: Request<ShardEntitiesRequest>,
    ) -> Result<Response<ShardEntitiesResponse>, Status> {
        let Some(order) = self.solar_system else {
            return Ok(Response::new(ShardEntitiesResponse::default()));
        };
        let system = &SOLAR_SYSTEMS[order];
        Ok(Response::new(ShardEntitiesResponse {
            universe: first_binds().into_iter().map(Into::into).collect(),
            solar_system: Some(solar_system_bind(order, system).into()),
            quantum_grid: Some(quantum_grid_bind(order, system).into()),
            order: order as u32,
        }))
    }

    async fn join_player(
        &self,
        request: Request<JoinPlayerRequest>,
    ) -> Result<Response<JoinPlayerResponse>, Status> {
        let request = request.into_inner();
        let place = governed_place(request.location_id);
        if !self.governs(&place) {
            return Err(Status::failed_precondition(format!(
                "{} does not govern {}",
                self.settings.name,
                place.root()
            )));
        }
        self.load_collision(&request).await;
        let appearance = self.appearance(request.account_id).await;
        let wearing = appearance.as_ref();
        let Some(mut entities) = player_entities_wearing(
            request.account_id,
            request.location_id,
            HangarSize::try_from(request.hangar_size).unwrap_or_default(),
            self.settings.planet_zone,
            wearing,
        ) else {
            return Err(Status::invalid_argument(format!(
                "account {} has no character",
                request.account_id
            )));
        };
        if !request.nickname.is_empty() {
            entities
                .player
                .snapshots
                .push(crate::player::player_name_snapshot(&request.nickname));
        }
        if let Some((ids, equipped)) = self.item_ids(&entities).await {
            entities.items = loadout_binds_with(entities.player.entity_id, &entities.loadout, &ids);
            entities.items.extend(equipped_binds(&equipped));
        }
        // A helmet the player wears instead of the default one has no visor, and the HUD is drawn on it.
        if let Some(visor) = crate::player::player_index(request.account_id).and_then(|index| {
            loadout::helmet_visor_bind(
                &Worn::from_binds(entities.player.entity_id, &entities.items),
                index,
            )
        }) {
            entities.items.push(visor);
        }
        entities
            .player
            .snapshots
            .extend(self.wallet(request.account_id).await);
        let badges = self.badges(request.account_id).await;
        if !badges.is_empty() {
            entities.player.snapshots.push(badges_snapshot(&badges));
        }
        let held = entities.held();
        self.worn.lock().unwrap().insert(
            entities.player.entity_id,
            HeldWorn {
                replicant: request.replicant,
                session: request.session,
                worn: Worn::from_binds(entities.player.entity_id, &entities.items),
                drawn: None,
                ammo: Default::default(),
                combat: Default::default(),
                container_sent: None,
                last_weapon_tick: None,
                state_pending: None,
            },
        );
        info!(
            account = request.account_id, session = request.session, root = %place.root(), customized = appearance.is_some(), ?held,
            "holding a player's entities",
        );
        let mut holdings = self.holdings.lock().unwrap();
        holdings.join(request.replicant, request.session, request.account_id, held);
        self.forget_movements(&holdings);
        drop(holdings);
        let mut panels = self.panels.lock().unwrap();
        panels.retain(|_, held| {
            (held.replicant, held.session) != (request.replicant, request.session)
        });
        for panel in entities.call_panels {
            panels.insert(
                panel.entity_id,
                HeldPanel {
                    replicant: request.replicant,
                    session: request.session,
                    panel,
                },
            );
        }
        drop(panels);
        let mut kiosks = self.kiosks.lock().unwrap();
        kiosks.retain(|_, held| *held != (request.replicant, request.session));
        let mut hangars = self.kiosk_hangars.lock().unwrap();
        let mut locations = self.kiosk_locations.lock().unwrap();
        for kiosk in entities.kiosks {
            kiosks.insert(kiosk, (request.replicant, request.session));
            if let Some(home) = &entities.home {
                hangars.insert(kiosk, home.location.hangar_class.clone());
                locations.insert(kiosk, home.location.location_id);
            }
        }
        hangars.retain(|kiosk, _| kiosks.contains_key(kiosk));
        locations.retain(|kiosk, _| kiosks.contains_key(kiosk));
        drop(locations);
        drop(hangars);
        drop(kiosks);
        if let (Some(home), Some(ships)) = (&entities.home, self.ships.clone()) {
            self.retrievals.hold(
                (request.replicant, request.session),
                entities.player.entity_id,
                home,
                ships,
            );
        }
        if let Some(home) = &entities.home {
            let mut elevators = self.elevators.lock().unwrap();
            elevators.set_player_name(entities.player.entity_id, &request.nickname);
            elevators.open_hangar(
                entities.player.entity_id,
                home,
                (request.replicant, request.session),
            );
        }
        // A join is a spawn: the player stands at its spawn point again.
        self.movements
            .lock()
            .unwrap()
            .join(entities.player.entity_id, entities.movement);
        Ok(Response::new(JoinPlayerResponse {
            planet: entities.planet.map(EntityBind::from),
            hangar: entities.hangar.map(EntityBind::from),
            player: Some(entities.player.into()),
            status: Some(self.status()),
            hangar_content: entities
                .hangar_content
                .into_iter()
                .map(EntityBind::from)
                .collect(),
            player_items: entities.items.into_iter().map(EntityBind::from).collect(),
        }))
    }

    async fn leave_player(
        &self,
        request: Request<LeavePlayerRequest>,
    ) -> Result<Response<LeavePlayerResponse>, Status> {
        let request = request.into_inner();
        let mut holdings = self.holdings.lock().unwrap();
        let left = holdings.leave_account(request.replicant, request.session);
        let released = left.map_or(0, |(_, entities)| entities);
        if let Some((account, _)) = left {
            self.player_calls.forget(account);
        }
        self.forget_movements(&holdings);
        drop(holdings);
        self.panels.lock().unwrap().retain(|_, held| {
            (held.replicant, held.session) != (request.replicant, request.session)
        });
        self.worn.lock().unwrap().retain(|_, held| {
            (held.replicant, held.session) != (request.replicant, request.session)
        });
        let mut elevators = self.elevators.lock().unwrap();
        if let Some(owner) = elevators.owner_of((request.replicant, request.session)) {
            elevators.close_hangar(owner);
        }
        self.instances.lock().unwrap().session_ended(
            (request.replicant, request.session),
            now_ms(),
            &mut elevators,
        );
        drop(elevators);
        // A retrieval in hand stores its ship again; the leave does not wait for it.
        let retrievals = self.retrievals.clone();
        let session = (request.replicant, request.session);
        tokio::spawn(async move { retrievals.release(session).await });
        self.outbox.close((request.replicant, request.session));
        info!(
            session = request.session,
            released,
            reason = request.reason,
            client_code = request.client_code,
            "player left"
        );
        // The entities this DGS held live go back to sc-entitygraph. Only a session this DGS held has any, and a replaced one
        // belongs to the connection that took over.
        if let (Some((account, _)), Some(graph)) = (left, &self.graph) {
            if let (true, Some(geid)) = (departure::releases(request.reason), player_geid(account))
            {
                let at_ms = if request.at_ms == 0 {
                    now_ms()
                } else {
                    request.at_ms
                };
                let event = departure::event(
                    &self.settings.shard_id,
                    request.replicant,
                    request.session,
                    account,
                    geid,
                    request.reason,
                    request.client_code,
                    at_ms,
                );
                departure::release(graph.clone(), event);
            }
        }
        Ok(Response::new(LeavePlayerResponse { released }))
    }

    async fn player_input(
        &self,
        request: Request<PlayerInputRequest>,
    ) -> Result<Response<PlayerInputResponse>, Status> {
        let request = request.into_inner();
        let answer = {
            let mut movements = self.movements.lock().unwrap();
            let Some(movement) = movements.get_mut(request.entity_id) else {
                return Err(Status::not_found(format!(
                    "{} holds no player {}",
                    self.settings.name, request.entity_id
                )));
            };
            for frame in &request.frames {
                movement.input(&Input::from(frame));
            }
            PlayerInputResponse {
                movement: Some(player_movement(request.entity_id, movement)),
            }
        };
        for frame in &request.frames {
            self.draw_or_holster(request.entity_id, request.replicant, request.session, frame);
        }
        for frame in request.frames.iter().filter(|frame| frame.reload.is_some()) {
            self.reload_weapon(request.entity_id, request.replicant, request.session, frame);
        }
        for frame in &request.frames {
            self.combat_frame(request.entity_id, request.replicant, request.session, frame);
        }
        for action in request.frames.iter().flat_map(|frame| &frame.item_actions) {
            let held = self
                .worn
                .lock()
                .unwrap()
                .get(&request.entity_id)
                .filter(|held| {
                    (held.replicant, held.session) == (request.replicant, request.session)
                })
                .map(|held| held.worn.clone());
            let Some(worn) = held else { break };
            info!(
                player = request.entity_id,
                kind = format_args!("{:#04x}", action.kind),
                item = action.item,
                action = action.action,
                hand = action.hand,
                "an item action"
            );
            let pocketed = self.pocketed.lock().unwrap().remove(&request.entity_id);
            for inventory in crate::carry::requests(
                action.kind,
                action.item,
                action.action,
                &worn,
                request.entity_id,
                pocketed,
            ) {
                self.run_inventory(
                    request.entity_id,
                    request.replicant,
                    request.session,
                    &inventory,
                )
                .await;
            }
        }
        Ok(Response::new(answer))
    }

    /// A client's request for a token of one of its entities: the access token of an inventory container is granted for
    /// the player's own items and denied for another's (see `token`). Anything else stays unanswered.
    async fn entity_token(
        &self,
        request: Request<sc_api::internal::EntityTokenRequest>,
    ) -> Result<Response<sc_api::internal::EntityTokenResponse>, Status> {
        let request = request.into_inner();
        let session = (request.replicant, request.session);
        let worn = self
            .worn
            .lock()
            .unwrap()
            .get(&request.player_id)
            .filter(|held| (held.replicant, held.session) == session)
            .map(|held| held.worn.clone());
        let decision = crate::token::decide(
            &crate::token::GraphOwners(self.graph.clone()),
            worn.as_ref(),
            &request,
        )
        .await;
        Ok(Response::new(sc_api::internal::EntityTokenResponse {
            decision: decision as i32,
        }))
    }

    /// A press of a call panel: "Call elevator" by the panel's own player is taken, and answered with `ClOnInteract` so
    /// that the client runs the panel's side of it. Anything else on a panel is refused, with no answer, as the game does.
    async fn entity_method(
        &self,
        request: Request<EntityMethodRequest>,
    ) -> Result<Response<EntityMethodResponse>, Status> {
        let request = request.into_inner();
        if request.entity_id == request.player_id {
            return Ok(Response::new(self.player_method(&request).await));
        }
        if let Some(calls) = self.retrievals.request(&request).await {
            return Ok(Response::new(EntityMethodResponse { to_caller: calls }));
        }
        if request.method == KIOSK_REQUEST_INTERACT
            && self.kiosks.lock().unwrap().get(&request.entity_id)
                == Some(&(request.replicant, request.session))
        {
            return Ok(Response::new(self.kiosk_interact(&request)));
        }
        if self.kiosks.lock().unwrap().get(&request.entity_id)
            == Some(&(request.replicant, request.session))
        {
            return Ok(Response::new(self.kiosk_method(&request).await));
        }
        if let Some(response) = self.weapon_command(&request) {
            return Ok(Response::new(response));
        }
        if let Some(response) = self.mobiglas_vehicle_infos(&request).await {
            return Ok(Response::new(response));
        }
        let panels = self.panels.lock().unwrap();
        let Some(held) = panels
            .get(&request.entity_id)
            .filter(|held| (held.replicant, held.session) == (request.replicant, request.session))
        else {
            drop(panels);
            return self.cab_panel_method(request);
        };
        let refused = Ok(Response::new(EntityMethodResponse::default()));
        if request.method != PANEL_REQUEST_INTERACT {
            info!(
                entity = request.entity_id,
                method = request.method,
                "a call panel method that is not an interaction"
            );
            return refused;
        }
        let Some(interaction) = Interaction::parse(&request.args).filter(|interaction| {
            interaction.user == request.player_id && interaction.interactable == request.entity_id
        }) else {
            warn!(
                entity = request.entity_id,
                player = request.player_id,
                "an interaction with a call panel that does not read as one of this player's"
            );
            return refused;
        };
        if interaction.interaction != CALL_ELEVATOR {
            info!(
                entity = request.entity_id,
                interaction = format_args!("{:#010x}", interaction.interaction),
                "a call panel interaction that is not handled"
            );
            return refused;
        }
        let panel = &held.panel;
        info!(panel = panel.entity_id, hangar = panel.hangar_id, gateway = %panel.gateway, player = request.player_id, request_id = interaction.request_id, "elevator called");
        let (player, panel) = (request.player_id, request.entity_id);
        drop(panels);
        if !self.elevators.lock().unwrap().call(player, player, panel) {
            return refused;
        }
        let answer = EntityMethodCall {
            entity_id: request.entity_id,
            method: PANEL_ON_INTERACT,
            args: request.args,
        };
        Ok(Response::new(EntityMethodResponse {
            to_caller: vec![answer],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::ORBITUARY;
    use tonic::Code;

    fn control(name: &str, system: &str, roots: &[&str]) -> Control {
        let roots = roots.iter().map(ToString::to_string).collect();
        Control::new(Settings {
            shard_id: "s".into(),
            name: name.into(),
            system: system.into(),
            roots,
            planet_zone: true,
            ..Settings::default()
        })
    }

    #[test]
    fn a_kiosk_press_is_answered_with_its_own_arguments_as_cl_on_interact() {
        // A press of a client (51 bytes), by player 0x45_d966_3f05 on the interactable 0x5d_21db_a063.
        let hex = "a71ffe86b1be3f0001000000000000005b09ecd21c884457053f66d945000000010000000000000000000063a0db215d000000";
        let args: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect();
        let request = EntityMethodRequest {
            entity_id: 0x5d_21db_a063,
            method: KIOSK_REQUEST_INTERACT,
            player_id: 0x45_d966_3f05,
            args: args.clone(),
            ..Default::default()
        };
        let answer = pyro_1().kiosk_interact(&request);
        assert_eq!(
            answer.to_caller,
            vec![EntityMethodCall {
                entity_id: 0x5d_21db_a063,
                method: KIOSK_ON_INTERACT,
                args
            }]
        );
        let other = EntityMethodRequest {
            player_id: 7,
            ..request
        };
        assert!(
            pyro_1().kiosk_interact(&other).to_caller.is_empty(),
            "another player's press is not answered"
        );
    }

    fn pyro_1() -> Control {
        control("pyro-1", "pyro", &["pyrosystem", "pyro1", "pyro2", "pyro3"])
    }

    fn join(account: u64, location: Option<u32>, session: u64) -> Request<JoinPlayerRequest> {
        Request::new(JoinPlayerRequest {
            replicant: 1,
            session,
            account_id: account,
            location_id: location,
            nickname: String::new(),
            hangar_size: HangarSize::Small as i32,
        })
    }

    fn leave(session: u64) -> Request<LeavePlayerRequest> {
        Request::new(LeavePlayerRequest {
            replicant: 1,
            session,
            ..Default::default()
        })
    }

    fn inventory_call(player: u64, kind: u8, session: u64) -> EntityMethodRequest {
        let mut args = 5u32.to_le_bytes().to_vec();
        args.push(kind);
        args.extend(player.to_le_bytes());
        args.extend([0u8; 40]);
        EntityMethodRequest {
            replicant: 1,
            session,
            player_id: player,
            entity_id: player,
            method: 40,
            args,
        }
    }

    #[tokio::test]
    async fn an_inventory_request_of_the_player_is_completed_ten_methods_on() {
        let dgs = pyro_1();
        dgs.join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap();
        let player = player_geid(1_000_001).unwrap();
        // A sort changes nothing: completed.
        let reply = dgs
            .entity_method(Request::new(inventory_call(player, 3, 7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.to_caller.len(), 1);
        assert_eq!(
            (reply.to_caller[0].entity_id, reply.to_caller[0].method),
            (player, 50)
        );
        assert_eq!(reply.to_caller[0].args, completed_args(true, 5, 0));
        // A store with nobody to keep the item fails, but is answered, so the client is not left waiting.
        let reply = dgs
            .entity_method(Request::new(inventory_call(player, 7, 7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.to_caller[0].args, completed_args(false, 5, 0));
        // A call that is not an inventory request is not answered.
        let other = EntityMethodRequest {
            replicant: 1,
            session: 7,
            player_id: player,
            entity_id: player,
            method: 40,
            args: vec![1, 2, 3],
        };
        assert!(dgs
            .entity_method(Request::new(other))
            .await
            .unwrap()
            .into_inner()
            .to_caller
            .is_empty());
    }

    #[tokio::test]
    async fn an_inventory_request_of_a_player_not_held_for_the_session_fails_but_is_answered() {
        let dgs = pyro_1();
        let player = player_geid(1_000_001).unwrap();
        let reply = dgs
            .entity_method(Request::new(inventory_call(player, 3, 7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.to_caller[0].args, completed_args(false, 5, 0));
        dgs.join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap();
        let reply = dgs
            .entity_method(Request::new(inventory_call(player, 3, 8)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            reply.to_caller[0].args,
            completed_args(false, 5, 0),
            "another session does not hold it"
        );
        dgs.leave_player(leave(7)).await.unwrap();
        let reply = dgs
            .entity_method(Request::new(inventory_call(player, 3, 7)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            reply.to_caller[0].args,
            completed_args(false, 5, 0),
            "a player that left wears nothing here"
        );
    }

    #[tokio::test]
    async fn a_player_holds_its_loadout_as_a_tree_for_the_session() {
        let dgs = pyro_1();
        dgs.join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap();
        let player = player_geid(1_000_001).unwrap();
        let held = dgs.worn.lock().unwrap();
        let worn = &held.get(&player).unwrap().worn;
        assert_eq!(worn.len(), 23);
        assert!(worn
            .resolve("Body_ItemPort:Armor_Undersuit:Armor_Helmet")
            .is_ok());
    }

    #[tokio::test]
    async fn the_dgs_with_the_system_root_owns_the_system_and_its_grid() {
        let reply = pyro_1()
            .shard_entities(Request::new(ShardEntitiesRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.order, 1);
        assert_eq!(reply.universe.len(), 2);
        assert_eq!(reply.solar_system.unwrap().entity_id, 200_001_275_310);
        assert_eq!(reply.quantum_grid.unwrap().parent_id, 200_001_275_310);
    }

    #[tokio::test]
    async fn a_dgs_with_no_system_root_owns_nothing_every_client_gets() {
        let reply = control("pyro-2", "pyro", &["pyro5", "pyro6"])
            .shard_entities(Request::new(ShardEntitiesRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply, ShardEntitiesResponse::default());
    }

    #[test]
    fn a_dgs_counts_what_it_holds_for_the_shard() {
        assert_eq!(
            pyro_1().status().shard_entities,
            3,
            "the system, its grid and pyro3"
        );
        assert_eq!(
            control("pyro-2", "pyro", &["pyro6"])
                .status()
                .shard_entities,
            1
        );
        assert_eq!(
            control("stanton-2", "stanton", &["stanton1a"])
                .status()
                .shard_entities,
            0
        );
    }

    /// sc-entitygraph being away costs the player nothing: the items are bound under the ids made up here.
    #[tokio::test]
    async fn a_player_wears_made_up_items_while_sc_entitygraph_is_away() {
        let endpoint = tonic::transport::Endpoint::from_static("http://127.0.0.1:1");
        let dgs = pyro_1().with_graph(EntityGraphInternalClient::new(endpoint.connect_lazy()));
        let reply = dgs
            .join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.player_items.len(), 23);
        assert_eq!(reply.player_items[0].entity_id, loadout::item_id(1, 0));
    }

    #[tokio::test]
    async fn the_governing_dgs_builds_the_player_and_records_it() {
        let dgs = pyro_1();
        let reply = dgs
            .join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(reply.player.unwrap().entity_id, 300_000_000_001);
        assert_eq!(reply.hangar.as_ref().unwrap().entity_id, 220_000_065_536);
        assert_eq!(reply.planet.unwrap().entity_id, 200_002_185_940);
        assert_eq!(reply.hangar_content.len(), 17, "7 containers, the 4 entities of the elevator's gateway, the 2 kiosks and the platform's 3 statics merged into the shell and the ATC");
        let gateway: Vec<u64> = reply.hangar_content[7..]
            .iter()
            .map(|bind| bind.entity_id)
            .collect();
        let status = reply.status.unwrap();
        assert_eq!(reply.player_items.len(), 23);
        assert_eq!(
            reply.player_items[0].parent_id, 300_000_000_001,
            "the body hangs from the player"
        );
        assert_eq!((status.players, status.player_entities), (1, 42), "the hangar, its 7 containers, the gateway's 4 entities, the 2 kiosks, the platform's 3 statics, the ATC, the player and its 23 items");
        let held: Vec<u64> = [220_000_065_536]
            .into_iter()
            .chain(220_000_065_547..220_000_065_554)
            .chain(gateway)
            .chain([300_000_000_001])
            .chain(310_000_000_256..310_000_000_279)
            .collect();
        assert_eq!(
            dgs.holdings.lock().unwrap().of(1_000_001),
            Some(held.as_slice())
        );
    }

    #[tokio::test]
    async fn leaving_frees_the_players_entities_and_not_the_planet() {
        let dgs = pyro_1();
        dgs.join_player(join(1_000_001, None, 7)).await.unwrap();
        let reply = dgs.leave_player(leave(7)).await.unwrap().into_inner();
        assert_eq!(reply.released, 42);
        let status = dgs.status();
        assert_eq!(
            (
                status.players,
                status.player_entities,
                status.shard_entities
            ),
            (0, 0, 3)
        );
    }

    /// The release to sc-entitygraph runs on its own task: a graph that is not there costs the leave nothing, and a leave
    /// of a session the DGS does not hold, or of a replaced one, frees nothing and sends nothing.
    #[tokio::test]
    async fn a_leave_does_not_wait_for_sc_entitygraph_and_a_replaced_one_changes_nothing() {
        let endpoint = tonic::transport::Endpoint::from_static("http://127.0.0.1:1");
        let dgs = pyro_1().with_graph(EntityGraphInternalClient::new(endpoint.connect_lazy()));
        dgs.join_player(join(1_000_001, None, 7)).await.unwrap();
        let reason = sc_api::internal::DisconnectReason::Clean as i32;
        let started = std::time::Instant::now();
        let reply = dgs
            .leave_player(Request::new(LeavePlayerRequest {
                replicant: 1,
                session: 7,
                reason,
                client_code: 30016,
                at_ms: 5,
            }))
            .await
            .unwrap();
        assert_eq!(reply.into_inner().released, 42);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let late = dgs
            .leave_player(Request::new(LeavePlayerRequest {
                replicant: 1,
                session: 7,
                reason,
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_eq!(late.into_inner().released, 0, "a repeat finds nothing");
        dgs.join_player(join(1_000_001, None, 8)).await.unwrap();
        let stale = dgs
            .leave_player(Request::new(LeavePlayerRequest {
                replicant: 1,
                session: 7,
                reason,
                ..Default::default()
            }))
            .await
            .unwrap();
        assert_eq!(
            stale.into_inner().released,
            0,
            "the old connection's late leave keeps the new one"
        );
        assert_eq!(dgs.status().players, 1);
    }

    #[tokio::test]
    async fn joining_again_gives_the_same_entities_and_counts_once() {
        let dgs = pyro_1();
        let first = dgs
            .join_player(join(1_000_001, None, 7))
            .await
            .unwrap()
            .into_inner();
        let second = dgs
            .join_player(join(1_000_001, None, 7))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((first.hangar, first.player), (second.hangar, second.player));
        assert_eq!(dgs.status().players, 1);
    }

    #[tokio::test]
    async fn another_dgs_of_the_system_refuses_a_home_it_does_not_govern() {
        let dgs = control("pyro-2", "pyro", &["pyro5", "pyro6"]);
        let error = dgs
            .join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Code::FailedPrecondition);
        assert_eq!(dgs.status().players, 0);
        assert!(dgs
            .join_player(join(1_000_001, Some(2_026_442_305), 7))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn an_account_with_no_character_is_refused_for_good() {
        let error = pyro_1().join_player(join(5, None, 7)).await.unwrap_err();
        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_home_with_no_hangar_leaves_the_player_with_the_system_root() {
        let reply = pyro_1()
            .join_player(join(1_000_001, Some(1), 7))
            .await
            .unwrap()
            .into_inner();
        assert_eq!((reply.hangar, reply.planet), (None, None));
        assert_eq!(reply.player.unwrap().parent_id, 200_001_275_310);
    }

    #[tokio::test]
    async fn any_dgs_locates_a_home() {
        let dgs = control("nyx-1", "nyx", &["nyxsystem"]);
        let located = dgs
            .locate_home(Request::new(LocateHomeRequest {
                location_id: Some(2_026_442_305),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            (located.system.as_str(), located.planet.as_str()),
            ("Pyro", "pyro6")
        );
    }

    #[test]
    fn a_new_process_has_a_new_boot_id() {
        assert_ne!(pyro_1().status().boot_id, 0);
    }

    /// "Call elevator" as a client sends it, with this player and panel in it.
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

    fn method(
        session: u64,
        player: u64,
        entity: u64,
        method: u32,
        args: Vec<u8>,
    ) -> Request<EntityMethodRequest> {
        Request::new(EntityMethodRequest {
            replicant: 1,
            session,
            player_id: player,
            entity_id: entity,
            method,
            args,
        })
    }

    #[tokio::test]
    async fn a_call_panel_press_is_answered_to_its_player_alone() {
        let dgs = pyro_1();
        let reply = dgs
            .join_player(join(1_000_001, Some(ORBITUARY), 7))
            .await
            .unwrap()
            .into_inner();
        let player = reply.player.unwrap().entity_id;
        let panel = reply
            .hangar_content
            .iter()
            .find(|bind| bind.class_crc == crate::location::CALL_PANEL_CLASS)
            .unwrap()
            .entity_id;
        let args = press(player, panel, CALL_ELEVATOR);
        let answer = dgs
            .entity_method(method(7, player, panel, 10, args.clone()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            answer.to_caller,
            [EntityMethodCall {
                entity_id: panel,
                method: 11,
                args: args.clone()
            }],
            "ClOnInteract, with the request's arguments"
        );
        // Another interaction, another method, someone else's press: refused, with no answer.
        let other = dgs
            .entity_method(method(7, player, panel, 10, press(player, panel, 1)))
            .await
            .unwrap()
            .into_inner();
        assert!(other.to_caller.is_empty());
        assert!(dgs
            .entity_method(method(7, player, panel, 12, args.clone()))
            .await
            .unwrap()
            .into_inner()
            .to_caller
            .is_empty());
        assert!(dgs
            .entity_method(method(
                7,
                player,
                panel,
                10,
                press(player + 1, panel, CALL_ELEVATOR)
            ))
            .await
            .unwrap()
            .into_inner()
            .to_caller
            .is_empty());
        // Not this session's panel, and none once the player has left.
        assert_eq!(
            dgs.entity_method(method(8, player, panel, 10, args.clone()))
                .await
                .unwrap_err()
                .code(),
            Code::NotFound
        );
        dgs.leave_player(leave(7)).await.unwrap();
        assert_eq!(
            dgs.entity_method(method(7, player, panel, 10, args))
                .await
                .unwrap_err()
                .code(),
            Code::NotFound
        );
    }
}

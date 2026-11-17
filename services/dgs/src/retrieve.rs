//! A ship retrieved at the ASOP kiosk: the client's request to the hangar's ATC, the answer that lets the kiosk go on,
//! the platform that lowers, the ship's entity bound on it, and the platform that raises it into the bay.
//!
//! The kiosk's `RequestVehicle` sends **one** call, `RmAuthorityRequestPermission` (rel 3 of the ATC's methods) with a
//! take-off `SATCActionRequest` whose `isSpawn` bit is set and whose `vehicle` is the ship's id (the one the claim answer
//! and the list carry: its geid). The authority answers it with `RmMulticastNotifyPlayer` (rel 5) of type 3,
//! `TakingOffAreaGranted`, or a refusal type, and creates the ship itself (a replicated entity at the landing area);
//! `RmMulticastChangePermissionToken` (rel 9) takes the token to occupied.
//!
//! What is unknown, and how it is handled:
//! - The ATC's absolute method indices (not known yet). The call is recognised by its arguments, not by its
//!   index: a call on the ATC entity that reads as a take-off with `isSpawn` is the request, and the answers go at the
//!   request's own index plus the relative distances (`NOTIFY_PLAYER_OFFSET`, `CHANGE_TOKEN_OFFSET`), which holds if the
//!   ATC's methods are one run, as the registrar's order says.
//! - The ship's entity: its class is the ship's class (CRC-32C of the GUID), at the landing area's pose in the hangar's
//!   frame, with no variables and no loadout. What the official service binds with it (owner, loadout items, state) is
//!   not known, so the ship is the bare class; the loadout from the entitlement snapshot is not built.
//! - The landing area entity is the shell's `LandingArea_ShipElevator_*` static (`transit_prefab_entities.tsv`); it is
//!   the token the notification names (`m_assignedTokenId`).
//!
//! The platform runs on `ships::Kiosk`'s clock (33 s down, 31.7 s up), and the DGS sends the door and manager changes
//! of `platform.rs`, which go to the client only with `SC_DGS_PLATFORM_INDICES` set.

use sc_api::internal::{HangarSize, PadSize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sc_api::internal::{
    session_update::Update, EntityBinds, EntityMethodCall, EntityMethodRequest, EntityUnbinds, EntityVariable, EntityVariables,
    ShipView,
};
use tonic::Code;
use tracing::{info, warn};

use crate::{
    atc::{atc_id, hex},
    atc_wire::{
        change_permission_token_args, notify_player_args, read_request_permission, ActionRequest,
        Notification, NOTIFICATION_NO_VALID_AREA_SIZE, NOTIFICATION_NO_VALID_AREA_TYPE,
        NOTIFICATION_TAKE_OFF_AREA_GRANTED, NOTIFICATION_WAITING_IN_QUEUE, REQUEST_TAKE_OFF,
        TOKEN_FREE, TOKEN_OCCUPIED,
    },
    entity::{Bind, Snapshot, Transform},
    location::HomeHangar,
    outbox::{Outbox, SessionKey},
    platform::{PlatformIds, PlatformLink},
    ride::{self, Ride, RIDE_TICK},
    ships::{Action, Kiosk, ShipBackend},
    solar_system::ZONE_ATTACHMENT,
};

/// The distance, in the ATC's method list, from `RmAuthorityRequestPermission` (3) to `RmMulticastNotifyPlayer` (5) and to
/// `RmMulticastChangePermissionToken` (9).
pub const NOTIFY_PLAYER_OFFSET: u32 = 2;
/// `RmAuthorityRequestPermission`'s relative index in the ATC's method list.
pub const REQUEST_PERMISSION_INDEX: u32 = 3;
pub const CHANGE_TOKEN_OFFSET: u32 = 6;
/// `EATCNotificationType` 9, `AlreadyGranted`: the kiosk takes it as success too.
pub const NOTIFICATION_ALREADY_GRANTED: i64 = 9;
/// How often a retrieval's clock is looked at.
pub const TICK: Duration = Duration::from_millis(250);
/// The flags of every root ship bind of the official service (the Cutlass, Cyclone, Vulture,
/// Skylark, Stinger and Liberator, each under its hangar with the zone attachment): replicated (0x1000), 0x400 as the
/// hangar's own, and the quantum grid bits `0x84a0_0000`. The first try bound a ship with the content's `0x1000` alone.
pub const SHIP_FLAGS: u32 = 0x84a0_1400;
/// The one component every root ship bind adds to its class's (the ones with an ATC data manager add
/// `0x1988_ff35` as well, not bound here). Its name is not known.
pub const SHIP_COMPONENT: u32 = 0x1cad_fa78;
/// The name every hangar's landing area static starts with.
pub const LANDING_AREA_PREFIX: &str = "LandingArea_ShipElevator";

/// The class CRC of an `EntityClassDefinition` GUID, as the seed's `guidHashCrc` and every class CRC of a bind: CRC-32C of
/// the GUID's 16 bytes, each half of 8 taken from its last byte to its first (`loadout::class_crc`). The CRC-32C of the
/// GUID's text, which this function took before, is a class the client does not know (`No Entity Class`). 0 for text that is not a GUID.
pub fn class_crc(guid: &str) -> u32 {
    crate::loadout::class_crc(guid).unwrap_or(0)
}

/// A hangar's landing area: the entity the token names and where a ship stands, in the hangar's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LandingArea {
    pub entity_id: u64,
    pub pose: Transform,
    /// The area's pose in the frame of the `LoadingPlatform` entity (both prefab poses are in the shell's frame, and the
    /// platform has an identity rotation): where a ship bound under the platform stands.
    pub on_platform: Transform,
}

/// The landing area of the hangar's shell: the container whose statics are as many as the platform layout says
/// (`PlatformIds::for_hangar`), its first `LandingArea_ShipElevator` entity. `None` for a hangar with no known shell or none
/// in the tables.
pub fn landing_area(hangar: &HomeHangar) -> Option<LandingArea> {
    let layout = crate::platform::layout_of_class(&hangar.location.hangar_class)?;
    let content = crate::location::hangar_content(hangar.location.socpak_hash);
    let ids = hangar.ids();
    let (position, (_, shell)) = ids
        .containers
        .iter()
        .enumerate()
        .find(|(_, (_, ids))| ids.statics.1 + 1 - ids.statics.0 == layout.statics)?;
    let area = crate::transit::tables()
        .prefab_entities(content.get(position)?.socpak_hash)
        .iter()
        .find(|entity| entity.name.starts_with(LANDING_AREA_PREFIX))?;
    let platform = crate::transit::tables()
        .prefab_entities(content.get(position)?.socpak_hash)
        .iter()
        .find(|entity| entity.index == layout.platform);
    let mut on_platform = area.pose;
    if let Some(platform) = platform {
        for (axis, base) in on_platform.position.iter_mut().zip(platform.pose.position) {
            *axis -= base;
        }
    }
    Some(LandingArea {
        entity_id: shell.statics.0 + area.index,
        pose: content.get(position)?.transform.then(&area.pose),
        on_platform,
    })
}

/// `SC_DGS_SHIP_PARENT=area`: the ship is bound as a child of the landing area static, with an identity pose, instead of under the
/// hangar at the area's static pose. The landing area is attached to the platform's bone `pad_helper_01` and the platform moves as a door animation of its `.cga`, so a child of the area should stand on the lowered pad
/// and rise with it. Not verified: the official service binds no ship under a landing area, and the attachment type for a static parent
/// is not known (the zone attachment is sent). The static pose of the area, (0, -45, 0) for `hangar_smlfrnt_001`, is the pad's UP
/// pose (the hangar floor, the `dooropen_padup` sequence ends there), so a ship bound at it while the pad is down is in the air
/// and falls.
pub const SHIP_PARENT_SETTING: &str = "SC_DGS_SHIP_PARENT";
pub const SHIP_PARENT_AREA: &str = "area";
/// `SC_DGS_SHIP_PARENT=static`: the old binding, under the hangar at the area's static pose.
pub const SHIP_PARENT_STATIC: &str = "static";
/// `SC_DGS_SHIP_PARENT=platform`: as `area`, under the `LoadingPlatform` entity itself (an entity attached to the platform
/// gets its physics as moving ground, `SetPlatformPhysicsParent`, so it rides up; the real state is a child of the
/// landing area with `CLandingArea::m_usingVehicle` set, whose variable is not identified). Not verified either.
pub const SHIP_PARENT_PLATFORM: &str = "platform";
/// `SC_DGS_PAD_LOWERED_OFFSET=x,y,z`: added to the area's static pose (hangar frame) when the ship is bound under the hangar, the
/// pad's lowered pose relative to its static (up) pose. The travel of the `doorclose_paddown` animation is not in our data
/// (it is in the `.cga` animation), so there is no default.
pub const PAD_LOWERED_OFFSET_SETTING: &str = "SC_DGS_PAD_LOWERED_OFFSET";

/// How far the ship-lift pad goes down from its up pose (the static pose of the landing area), in metres along Z, by hangar size:
/// the pad bone moves only along Z, with no rotation; `door_close` runs 0 -> -32 for the small hangar (`hangar_smlfrnt_001`,
/// `ht_hangar_ship_elev_sml_door_close.caf`, 450 frames); medium -36, large -64, extra large -104 (the platform has an identity rotation, so the offset is the same in the hangar's frame). The ship is spawned
/// after the pad has been lowered, so it is bound at that pose.
pub const PAD_TRAVEL: [(HangarSize, f64); 4] = [
    (HangarSize::Small, 32.0),
    (HangarSize::Medium, 36.0),
    (HangarSize::Large, 64.0),
    (HangarSize::Xlarge, 104.0),
];

/// The pad's lowered offset for a hangar of `hangar_class`: down along Z; none when the size is not known.
pub fn pad_lowered_offset(hangar_class: &str) -> [f64; 3] {
    let size = crate::ships::hangar_class::hangar_size_of(hangar_class);
    PAD_TRAVEL
        .iter()
        .find(|(known, _)| *known == size)
        .map_or([0.0; 3], |(_, travel)| [0.0, 0.0, -travel])
}

/// The lowered offset from its setting text: three numbers, or `None`.
pub fn parse_offset(setting: Option<&str>) -> Option<[f64; 3]> {
    let mut parts = setting?.split(',').map(|part| part.trim().parse::<f64>().ok());
    let offset = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(offset)
}

/// The ship's bind and, in the same batch right after it, its parts (`retrieve_loadout`).
fn ship_with_parts(ship: Bind) -> Vec<sc_api::internal::EntityBind> {
    let parts = crate::retrieve_loadout::part_binds(ship.entity_id, ship.class_crc);
    std::iter::once(ship).chain(parts).map(Into::into).collect()
}

/// The ship's entity on the landing area: its class, in the hangar's frame at the area's pose, or under the area
/// (`SC_DGS_SHIP_PARENT=area`), or shifted by `SC_DGS_PAD_LOWERED_OFFSET`.
pub fn ship_bind(geid: u64, class_guid: &str, hangar: &HomeHangar, area: &LandingArea, rest_height: f64) -> Bind {
    let setting = std::env::var(SHIP_PARENT_SETTING).unwrap_or_default();
    // By default the ship is bound as the official service binds a parked hangar ship (VNCL_Stinger, ANVL_Liberator,
    // parent = the hangar container, zone attachment 0x46c01eb2, identity rotation, the pad's `Door_OpenState` Opened): under
    // the hangar, in its frame, above the landing area's point by the ship's rest height (`SHIP_REST_HEIGHT`). A ship bound
    // under the landing area (`area`) or the platform (`platform`) has no official bind and stays opt-in.
    // Default (the binding that put the ship in the pit): under the hangar. `platform` binds it under the
    // `LoadingPlatform` entity instead, the one bound parent that is not the hangar: under the landing area (a static the
    // client makes itself) disconnected the client with 30008 "Parent Entity Missing", under the platform spawned
    // nothing visible. Not verified, opt-in.
    let parent = if setting.eq_ignore_ascii_case(SHIP_PARENT_PLATFORM) {
        PlatformIds::for_hangar(hangar).map(|ids| ids.platform)
    } else if setting.eq_ignore_ascii_case(SHIP_PARENT_AREA) {
        Some(area.entity_id)
    } else {
        None
    };
    // The platform entity does not follow the pad's animation (a ship at the rest height only
    // collides with the hangar door), so the pad's lowered offset applies under the platform too.
    let lowered = parse_offset(std::env::var(PAD_LOWERED_OFFSET_SETTING).ok().as_deref())
        .unwrap_or_else(|| pad_lowered_offset(&hangar.location.hangar_class));
    let offset = Some([lowered[0], lowered[1], lowered[2] + rest_height]);
    ship_bind_with(geid, class_guid, hangar, area, parent, offset)
}

/// How far a ship's origin sits above the landing area's point when it rests on the pad with its gear down, in the hangar's
/// frame (z up). Measured (ship pos minus platform + landing area pos through the hangar's container
/// modifier): VNCL_Stinger 9.78 m (hangar MediumFront), ANVL_Liberator 11.86 m (XLTop); identity rotation. It depends on the
/// ship (its gear height), so the value is the lower measured one: a ship bound at the area's point itself has its hull inside
/// the pad (`Bad Spawn Position ... deep penetration`).
pub const SHIP_REST_HEIGHT: f64 = 9.78;

/// The ship's origin above the pad's point for each pad class, where a ship bound lower than its keel has its hull inside the
/// pit's floor (`INVALID SPAWN LOCATION ... colliding with designer_279.cgf`, and the engine then moves it hundreds of
/// metres away). A ship that is bound above the pad settles onto it. The measured origin-to-keel offsets are the Sabre's 2.53 m,
/// the Stinger's 9.78 and the Liberator's 11.86 (official XL hangar), and the Idris M's about 25 (its gear spring lengths, 24.1
/// and 26.2); the Javelin has none, its hull reaching about 22.4 m below its origin. So the large classes get a margin over
/// that for a ship with no keel of its own (`ship_rest_height`).
pub const SHIP_REST_HEIGHTS: [(PadSize, f64); 2] = [(PadSize::Large, 20.0), (PadSize::Xlarge, 26.0)];

/// `tables/ship_keels.tsv`: the class CRC and the origin-to-keel distance in metres of each ship whose XML gives one.
pub const SHIP_KEELS_PATH: &str = "tables/ship_keels.tsv";
/// How far the landing area's pose is below the pad's top surface (the area is attached to the pad bone at -0.796), which a
/// keel touches.
pub const AREA_BELOW_PAD_TOP: f64 = 0.796;
/// A ship is bound this far above where its keel touches the pad, so it never starts inside it and settles onto it.
pub const SETTLING_CLEARANCE: f64 = 0.3;

static KEELS: std::sync::OnceLock<std::collections::HashMap<u32, f64>> = std::sync::OnceLock::new();

/// The keels of the build, once, at startup.
pub fn install_keels(keels: std::collections::HashMap<u32, f64>) -> bool {
    KEELS.set(keels).is_ok()
}

/// The table's rows: a class CRC and a distance each.
pub fn parse_keels(text: &str) -> Result<std::collections::HashMap<u32, f64>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (class_crc, keel) = line.split_once('\t').ok_or_else(|| format!("{SHIP_KEELS_PATH}: {line:?} has no tab"))?;
            let row = (class_crc.parse::<u32>().ok(), keel.trim().parse::<f64>().ok());
            match row {
                (Some(class_crc), Some(keel)) => Ok((class_crc, keel)),
                _ => Err(format!("{SHIP_KEELS_PATH}: {line:?} does not parse")),
            }
        })
        .collect()
}

/// How high above the landing area's pose a ship of `class_crc` is bound: its own keel offset when the build has it, over the
/// pad's top and with a little clearance; never less than `rest_height(pad)`'s for its pad class, so a ship that does not have
/// a keel (the Javelin has none) is bound at its class's margin, and a small ship keeps the height that has always worked.
pub fn ship_rest_height(class_crc: u32, pad: PadSize) -> f64 {
    let own = KEELS
        .get()
        .and_then(|keels| keels.get(&class_crc))
        .map(|keel| keel + AREA_BELOW_PAD_TOP + SETTLING_CLEARANCE);
    own.map_or(rest_height(pad), |own| own.max(SHIP_REST_HEIGHT))
}

/// The rest height of a ship of pad class `pad`: `SHIP_REST_HEIGHT`, or more for a class larger than the measured ships.
pub fn rest_height(pad: PadSize) -> f64 {
    SHIP_REST_HEIGHTS
        .iter()
        .find(|(known, _)| *known == pad)
        .map_or(SHIP_REST_HEIGHT, |(_, height)| height.max(SHIP_REST_HEIGHT))
}

fn ship_bind_with(
    geid: u64,
    class_guid: &str,
    hangar: &HomeHangar,
    area: &LandingArea,
    parent: Option<u64>,
    offset: Option<[f64; 3]>,
) -> Bind {
    let (parent_id, mut transform) = match parent {
        Some(parent) if parent == area.entity_id => (parent, Transform::IDENTITY),
        Some(parent) => (parent, area.on_platform),
        None => (hangar.entity_id, area.pose),
    };
    // The offset is in the parent's frame (the hangar's, or the area's: both with the rest of the pose identity).
    if let Some(offset) = offset {
        for (axis, shift) in transform.position.iter_mut().zip(offset) {
            *axis += shift;
        }
    }
    Bind {
        entity_id: geid,
        class_crc: class_crc(class_guid),
        flags: SHIP_FLAGS,
        transform,
        parent_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components: vec![SHIP_COMPONENT],
        snapshots: landing_gear_down(area.entity_id),
    }
}

/// `Server.LandingGearExtract` (CRC-32 of the lowercased name), a bool on the ship's root: 1 = gear down. The key that
/// separates the two states in the official ship binds (the ships bound with it 1 are parked on
/// plinths or in hangar bays).
pub const LANDING_GEAR_EXTRACT_KEY: u32 = 0x8822_024c;
/// The serializer type of the bool, as `transit::wire`'s door states carry it.
pub const BOOL_TYPE: u32 = 0x0b9f_c026;
/// `TokenGearsState` (`CIGLandingSystem::SGearsState`, one byte): 3 on the official ships bound with the gear down and the
/// token reporting, only ever together with the extract bool at 1.
pub const TOKEN_GEARS_STATE_KEY: u32 = 0x70ad_a460;
pub const GEARS_STATE_TYPE: u32 = 0x5dc8_0105;
/// `SGearsState { m_bIsLanded, m_bAllContactsTouching }`, two bytes: a ship resting on a
/// pad is landed with every contact touching.
pub const GEARS_STATE_LANDED: [u8; 2] = [1, 1];
/// `LandingAreaId` (an entity id, flags 1): the pad's landing area entity the ship rests on.
pub const LANDING_AREA_ID_KEY: u32 = 0x7263_57b0;
pub const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
/// `ServerLandingGearSnap`, a bool on the roots of some official ships: the gear is already in position (no animation).
pub const LANDING_GEAR_SNAP_KEY: u32 = 0x436d_abbd;
const GEAR_DOWN: u8 = 1;

/// The variables that bind a ship with its landing gear deployed: every retrieved ship rests on the pad with it down.
/// (The landing system child's own `LandingGearExtract` stays 0 on the hangar-bay ships, so only the
/// root is set.)
pub fn landing_gear_down(area_id: u64) -> Vec<Snapshot> {
    vec![
        Snapshot {
            key: LANDING_GEAR_EXTRACT_KEY,
            type_id: BOOL_TYPE,
            value: vec![GEAR_DOWN],
        },
        Snapshot {
            key: LANDING_GEAR_SNAP_KEY,
            type_id: BOOL_TYPE,
            value: vec![GEAR_DOWN],
        },
        Snapshot {
            key: TOKEN_GEARS_STATE_KEY,
            type_id: GEARS_STATE_TYPE,
            value: GEARS_STATE_LANDED.to_vec(),
        },
        Snapshot {
            key: LANDING_AREA_ID_KEY,
            type_id: ENTITY_ID_TYPE,
            value: area_id.to_le_bytes().to_vec(),
        },
    ]
}

/// The request of a call on the ATC, when it is a ship's retrieval (a take-off with `isSpawn`).
pub fn retrieval_request(args: &[u8]) -> Option<ActionRequest> {
    let (request, _) = read_request_permission(args)?;
    (request.mode == REQUEST_TAKE_OFF && request.is_spawn).then_some(request)
}

/// `ATCDataManager_VehiclesBeingStored` of the ATC entity (key 0xae120d6d, a sequence of entity ids): the ships moving to storage. The
/// client's ASOP lists a ship in it as "Moving to Storage" with no action. Its slot in the sorted variables of the ATC entity (10: 6 crashed the client, which named slot 6 as `smlc.allowedForMissionRestrictedDeliveries`; that variable is third in the key order of the
/// component set, so the entity carries three variables below it that the set alone does not, and the one that sits below the variable too, the fourth):
/// `BEING_STORED_SETTING` replaces it for a test, and the client reads the variable only when it builds the list, so it is sent at the Store.
pub const BEING_STORED_INDEX: Option<u32> = Some(10);
pub const BEING_STORED_SETTING: &str = "SC_DGS_BEING_STORED_INDEX";

fn being_stored_index() -> Option<u32> {
    std::env::var(BEING_STORED_SETTING).ok().and_then(|text| text.trim().parse().ok()).or(BEING_STORED_INDEX)
}

/// The value of the variable for `ids`: a varint count, then each id as a u64 (little endian).
pub fn being_stored_value(ids: &[u64]) -> Vec<u8> {
    let mut value = Vec::new();
    crate::kiosk_calls::push_varint(&mut value, ids.len() as u64);
    for id in ids {
        value.extend_from_slice(&id.to_le_bytes());
    }
    value
}

/// The localization keys of the hangar names the ASOP shows on the retrieved screen, by size. Each text holds one `%ls`, which the
/// client fills with the player's name ("<name>'s Large Hangar"). The `@` is the form the client resolves.
pub const HANGAR_NAME_SMALL: &str = "@ui_interactor_hangar_small";
pub const HANGAR_NAME_MEDIUM: &str = "@ui_interactor_hangar_medium";
pub const HANGAR_NAME_LARGE: &str = "@ui_interactor_hangar_large";
pub const HANGAR_NAME_XLARGE: &str = "@ui_interactor_hangar_xlarge";

/// The key of the name of a hangar of `hangar_class`, empty when its size is not known.
pub fn hangar_name_key(hangar_class: &str) -> &'static str {
    match crate::ships::hangar_class::hangar_size_of(hangar_class) {
        HangarSize::Small => HANGAR_NAME_SMALL,
        HangarSize::Medium => HANGAR_NAME_MEDIUM,
        HangarSize::Large => HANGAR_NAME_LARGE,
        HangarSize::Xlarge => HANGAR_NAME_XLARGE,
        _ => "",
    }
}

/// `RmMulticastNotifyPlayer` for `request`, at `method` of the ATC `atc`.
pub fn notify_call(
    atc: u64,
    method: u32,
    request: &ActionRequest,
    kind: i64,
    area: u64,
) -> EntityMethodCall {
    notify_call_named(atc, method, request, kind, area, "")
}

/// `RmMulticastNotifyPlayer` whose notification carries `identifier`: for a retrieval it is the key of the hangar's name, which the
/// ASOP shows as "Your vehicle will be waiting at" (empty, the client logs an invalid landing area).
pub fn notify_call_named(
    atc: u64,
    method: u32,
    request: &ActionRequest,
    kind: i64,
    area: u64,
    identifier: &str,
) -> EntityMethodCall {
    let mut request = request.clone();
    request.token = area;
    let notification = Notification {
        kind,
        operator: atc,
        token: area,
        identifier: identifier.to_string(),
        ..Notification::default()
    };
    EntityMethodCall {
        entity_id: atc,
        method,
        args: notify_player_args(&request, &notification),
    }
}

/// The notification a refused retrieval gets: the kiosk shows the matching text.
pub fn refusal_kind(code: Code, platform_busy: bool) -> i64 {
    match code {
        Code::AlreadyExists => NOTIFICATION_ALREADY_GRANTED,
        _ if platform_busy => NOTIFICATION_WAITING_IN_QUEUE,
        Code::FailedPrecondition => NOTIFICATION_NO_VALID_AREA_SIZE,
        _ => NOTIFICATION_NO_VALID_AREA_TYPE,
    }
}

impl<B: ShipBackend> Held<B> {
    /// Whether the ship is moving with the pad, which asks for the finer clock.
    fn riding(&self) -> bool {
        self.ride.lock().unwrap().as_ref().is_some_and(Ride::moving)
    }

    /// The pad starts to rise or lower: the ship on it moves with it.
    fn start_ride(&self, up: bool, now: Duration, duration: Duration) {
        if let Some(ride) = self.ride.lock().unwrap().as_mut() {
            info!(ship = ride.geid(), up, ?duration, "the ship rides the platform");
            ride.start(up, now, duration);
        }
    }
}

/// One hangar's retrieval: the kiosk machine and what binds it to the ATC and the client.
struct Held<B: ShipBackend> {
    session: SessionKey,
    owner: u64,
    atc: u64,
    hangar: HomeHangar,
    area: LandingArea,
    /// The ATC's `RmAuthorityRequestPermission` index, as the request came.
    request_method: Mutex<Option<u32>>,
    /// The ship on the pad and where it is while the pad moves (`SC_DGS_SHIP_RIDE`), which the DGS moves itself.
    ride: Mutex<Option<Ride>>,
    /// The ship's bind at the raised pose, kept until the pad has risen (`SC_DGS_SHIP_POP_IN`).
    pop_in: Mutex<Option<Bind>>,
    kiosk: tokio::sync::Mutex<Kiosk<B>>,
}

/// The retrievals of the hangars of the players held, by ATC.
pub struct Retrievals<B: ShipBackend + 'static> {
    outbox: Outbox,
    held: Mutex<HashMap<u64, Arc<Held<B>>>>,
    started: Instant,
}

impl<B: ShipBackend + 'static> Retrievals<B> {
    pub fn new(outbox: Outbox) -> Self {
        Self {
            outbox,
            held: Mutex::default(),
            started: Instant::now(),
        }
    }

    /// The hangar of `owner`, for its session. Without a known landing area or platform nothing is held (the kiosk lists
    /// ships and cannot spawn them).
    pub fn hold(&self, session: SessionKey, owner: u64, hangar: &HomeHangar, backend: B) {
        let (Some(area), Some(ids)) = (landing_area(hangar), PlatformIds::for_hangar(hangar))
        else {
            warn!(
                hangar = hangar.entity_id,
                class = hangar.location.hangar_class,
                "the hangar has no landing area or platform in the tables: no ship is retrieved here"
            );
            return;
        };
        let mut link = PlatformLink::new(self.outbox.clone(), session, ids);
        if let Some(layout) = crate::platform::layout_of_class(&hangar.location.hangar_class)
            .filter(|_| crate::platform::gate_enabled())
        {
            let times = crate::platform_times::installed_of(&hangar.location.hangar_class);
            let gate_open = times.map_or(crate::ships::GATE_OPEN_TIME, |times| times.gate_open);
            let pad_close = times.map_or(crate::ships::PAD_CLOSE_TIME, |times| times.pad_close);
            link = link.with_gate(ids.platform - layout.platform + layout.gate, gate_open, pad_close);
        }
        let kiosk = Kiosk::new(
            backend,
            owner,
            hangar.location.location_id,
            &hangar.location.hangar_class,
            false,
        )
        .with_link(link);
        let atc = atc_id(hangar);
        self.held.lock().unwrap().insert(
            atc,
            Arc::new(Held {
                session,
                owner,
                atc,
                hangar: hangar.clone(),
                area,
                request_method: Mutex::new(None),
                ride: Mutex::new(None),
                pop_in: Mutex::new(None),
                kiosk: tokio::sync::Mutex::new(kiosk),
            }),
        );
    }

    /// The session left: a retrieval in hand is abandoned (the ship is stored again).
    pub async fn release(&self, session: SessionKey) {
        let gone: Vec<Arc<Held<B>>> = {
            let mut held = self.held.lock().unwrap();
            let atcs: Vec<u64> = held
                .iter()
                .filter(|(_, held)| held.session == session)
                .map(|(atc, _)| *atc)
                .collect();
            atcs.iter().filter_map(|atc| held.remove(atc)).collect()
        };
        for held in gone {
            held.kiosk.lock().await.abort(0, 0).await;
        }
    }

    /// A call on an entity that may be a hangar's ATC. `None` when it is not one held for this session or does not read as
    /// the retrieval request (the call then falls to the other handlers); else what the caller is answered.
    pub async fn request(
        self: &Arc<Self>,
        request: &EntityMethodRequest,
    ) -> Option<Vec<EntityMethodCall>> {
        let held = self
            .held
            .lock()
            .unwrap()
            .get(&request.entity_id)
            .filter(|held| held.session == (request.replicant, request.session))
            .cloned()?;
        let Some(ask) = retrieval_request(&request.args) else {
            info!(
                atc = request.entity_id,
                method = request.method,
                args = hex(&request.args),
                "a call on the ATC that is not a ship's retrieval"
            );
            return None;
        };
        if ask.player != request.player_id {
            warn!(
                atc = request.entity_id,
                player = request.player_id,
                asked = ask.player,
                "a retrieval for another player: refused"
            );
            return None;
        }
        *held.request_method.lock().unwrap() = Some(request.method);
        let reply_at = request.method + NOTIFY_PLAYER_OFFSET;
        let now = self.started.elapsed();
        let mut kiosk = held.kiosk.lock().await;
        let busy = kiosk.busy();
        let result = kiosk.retrieve(ask.vehicle, ask.vehicle, now, 0).await;
        if let Ok(actions) = &result {
            self.run(&held, &mut kiosk, actions.clone(), now).await;
        }
        drop(kiosk);
        let call = match result {
            Ok(_) => {
                info!(
                    atc = held.atc,
                    ship = ask.vehicle,
                    area = held.area.entity_id,
                    player = request.player_id,
                    "ship retrieval granted"
                );
                self.clone().drive(held.clone());
                notify_call_named(
                    held.atc,
                    reply_at,
                    &ask,
                    NOTIFICATION_TAKE_OFF_AREA_GRANTED,
                    held.area.entity_id,
                    hangar_name_key(&held.hangar.location.hangar_class),
                )
            }
            Err(status) => {
                warn!(atc = held.atc, ship = ask.vehicle, %status, "ship retrieval refused");
                notify_call(
                    held.atc,
                    reply_at,
                    &ask,
                    refusal_kind(status.code(), busy),
                    held.area.entity_id,
                )
            }
        };
        Some(vec![call])
    }

    /// The ship on the raised pad of the hangar of `session`'s `owner`, with its ATC and landing area: what the ASOP lists as
    /// retrieved (`kiosk_calls::LandedShip`). `None` when no ship stands there or the pad is not up.
    pub async fn landed(&self, session: SessionKey, owner: u64) -> Option<LandedShip> {
        let held = self.held_of(session, owner)?;
        let kiosk = held.kiosk.lock().await;
        let geid = kiosk.storing().or_else(|| kiosk.on_pad().filter(|_| !kiosk.busy()))?;
        Some(LandedShip {
            geid,
            atc: held.atc,
            area: held.area.entity_id,
            location_id: held.hangar.location.location_id,
        })
    }

    /// Whether `owner` holds a retrieval in `session`, whether or not a ship stands on its pad.
    pub fn holds(&self, session: SessionKey, owner: u64) -> bool {
        self.held_of(session, owner).is_some()
    }

    fn held_of(&self, session: SessionKey, owner: u64) -> Option<Arc<Held<B>>> {
        self.held
            .lock()
            .unwrap()
            .values()
            .find(|held| held.session == session && held.owner == owner)
            .cloned()
    }

    /// Sets the ships moving to storage on the hangar's ATC, which the client's ASOP shows as "Moving to Storage".
    fn being_stored(&self, held: &Held<B>, ids: &[u64]) {
        let Some(index) = being_stored_index() else {
            info!(atc = held.atc, ships = ids.len(), setting = BEING_STORED_SETTING, "the ships being stored are not sent: the variable's index is not known");
            return;
        };
        let variable = EntityVariable { index, value: being_stored_value(ids) };
        info!(atc = held.atc, index, ships = ids.len(), "the ships being stored sent");
        self.outbox.send(held.session, Update::Variables(EntityVariables { entity_id: held.atc, variables: vec![variable] }));
    }

    /// Whether a delivery of the ship `geid` is refused at `owner`'s kiosk: it is on the pad, being retrieved, or moving to storage.
    pub async fn refuses_delivery(&self, session: SessionKey, owner: u64, geid: u64) -> bool {
        let Some(held) = self.held_of(session, owner) else {
            return false;
        };
        let kiosk = held.kiosk.lock().await;
        kiosk.engages(geid)
    }

    /// The ASOP's Store button for the ship `geid` on the pad of `owner`'s hangar: the pad goes down, the ship is stored in
    /// the bank and the pad stays down and empty. The refusal says why not.
    pub async fn store(
        self: &Arc<Self>,
        session: SessionKey,
        owner: u64,
        geid: u64,
    ) -> Result<(), tonic::Status> {
        let held = self
            .held_of(session, owner)
            .ok_or_else(|| tonic::Status::not_found("no hangar kiosk is held"))?;
        let now = self.started.elapsed();
        let mut kiosk = held.kiosk.lock().await;
        let actions = kiosk.store(geid, now)?;
        self.being_stored(&held, &[geid]);
        self.run(&held, &mut kiosk, actions, now).await;
        drop(kiosk);
        self.clone().drive(held);
        Ok(())
    }

    /// Runs the retrieval's clock until the ship is in the bay or it fails.
    fn drive(self: Arc<Self>, held: Arc<Held<B>>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(if held.riding() { RIDE_TICK } else { TICK }).await;
                if !self.step(&held, self.started.elapsed()).await {
                    break;
                }
            }
        });
    }

    /// One look at the clock. `false` when nothing is retrieved any more.
    async fn step(&self, held: &Held<B>, now: Duration) -> bool {
        let mut kiosk = held.kiosk.lock().await;
        // The ship goes first, so its last position is sent before the pad's arrival unbinds it.
        self.ride(held, now);
        let actions = kiosk.tick(now);
        self.run(held, &mut kiosk, actions, now).await;
        kiosk.busy()
    }

    /// Does what a retrieval asked, and what that asks in turn.
    async fn run(&self, held: &Held<B>, kiosk: &mut Kiosk<B>, mut actions: Vec<Action>, now: Duration) {
        while let Some(action) = actions.pop() {
            match action {
                Action::SpawnShip { geid } => {
                    let spawned = self.spawn(held, kiosk, geid);
                    actions.extend(kiosk.spawned(spawned, now, 0, geid).await);
                }
                Action::StoreShip { geid } => {
                    self.unbind(held, geid);
                    actions.extend(kiosk.stored(geid, 0, geid).await);
                }
                Action::ShipReady { geid } => {
                    self.pop_in(held, geid);
                    self.ready(held, geid);
                }
                Action::ShipStored { geid } => {
                    info!(ship = geid, "the ship on the platform is stored: the pad stays down");
                    self.free(held);
                    self.being_stored(held, &[]);
                    self.notify_stored(held, geid);
                }
                Action::ReleaseShip { geid } => {
                    warn!(ship = geid, "the retrieval ended with the ship stored again");
                }
                Action::RaisePlatform => held.start_ride(true, now, kiosk.platform_open_time()),
                Action::LowerPlatform => {
                    // The ship rests on the pad and goes down with it; it leaves when the platform has closed
                    // (`Action::StoreShip`).
                    held.start_ride(false, now, kiosk.platform_close_time());
                }
            }
        }
    }

    /// Sends the position of the ship that rides the pad, when the pad moves.
    fn ride(&self, held: &Held<B>, now: Duration) {
        let moved = held.ride.lock().unwrap().as_mut().and_then(|ride| ride.tick(now));
        if let Some(moved) = moved {
            self.outbox.send(held.session, Update::Moved(moved));
        }
    }

    /// The ship that stood on the pad leaves the client's world.
    fn unbind(&self, held: &Held<B>, geid: u64) {
        *held.ride.lock().unwrap() = None;
        info!(ship = geid, "the ship on the lowered platform is unbound");
        self.outbox.send(
            held.session,
            Update::Unbinds(EntityUnbinds {
                entity_ids: std::iter::once(geid)
                    .chain(crate::retrieve_loadout::take_part_ids(geid))
                    .collect(),
            }),
        );
    }

    /// Binds the ship on the landing area. `false` when its class is not known.
    fn spawn(&self, held: &Held<B>, kiosk: &Kiosk<B>, geid: u64) -> bool {
        let pad = kiosk.retrieving().and_then(|view| view.ship.as_ref()).map_or(PadSize::Unspecified, |ship| ship.pad_size());
        let Some(class) = kiosk
            .retrieving()
            .and_then(|view| view.ship.as_ref())
            .map(|ship| ship.entity_class_guid.clone())
            .filter(|class| crate::loadout::class_crc(class).is_some())
        else {
            warn!(ship = geid, "the ship has no class: not spawned");
            return false;
        };
        // The parts of the ship's loadout tree in the graph, bound with the graph's ids (what the Vehicle Manager lists).
        if let Some(view) = kiosk.retrieving() {
            crate::retrieve_loadout::remember(geid, &view.parts);
        }
        let bind = ship_bind(geid, &class, &held.hangar, &held.area, ship_rest_height(crate::loadout::class_crc(&class).unwrap_or_default(), pad));
        info!(
            ship = geid,
            class,
            class_crc = format_args!("{:#010x}", bind.class_crc),
            area = held.area.entity_id,
            "the ship is bound on the landing area"
        );
        if ride::pop_in() {
            // The ship is made at the bottom when the platform has closed (the door and the gate are shut) and stays
            // where it is bound: the pad's own animation carries it up (it rests on the pad), so it is not moved again.
            info!(ship = geid, class, "the ship is bound at the bottom as the platform closes");
        } else if ride::enabled() {
            // The up pose is the pit pose without the pad's lowered offset (the offset is in the parent's frame, added to the pose).
            let lowered = parse_offset(std::env::var(PAD_LOWERED_OFFSET_SETTING).ok().as_deref())
                .unwrap_or_else(|| pad_lowered_offset(&held.hangar.location.hangar_class));
            let pit = bind.transform.position;
            let up = [0, 1, 2].map(|axis| pit[axis] - lowered[axis]);
            *held.ride.lock().unwrap() = Some(Ride::at_pit(geid, pit, up));
        }
        self.outbox.send(
            held.session,
            Update::Binds(EntityBinds {
                key: format!("ship:{geid}"),
                binds: ship_with_parts(bind),
            }),
        );
        true
    }

    /// The pad has risen: the ship is bound on it (`SC_DGS_SHIP_POP_IN`).
    fn pop_in(&self, held: &Held<B>, geid: u64) {
        let Some(bind) = held.pop_in.lock().unwrap().take() else {
            return;
        };
        info!(ship = geid, "the ship is moved to the raised pad");
        self.outbox.send(
            held.session,
            Update::Unbinds(EntityUnbinds {
                entity_ids: std::iter::once(geid)
                    .chain(crate::retrieve_loadout::take_part_ids(geid))
                    .collect(),
            }),
        );
        self.outbox.send(
            held.session,
            Update::Binds(EntityBinds {
                key: format!("ship:{geid}"),
                binds: ship_with_parts(bind),
            }),
        );
    }

    /// The pad is down and empty after a store: the landing area's token is free.
    fn free(&self, held: &Held<B>) {
        let Some(method) = *held.request_method.lock().unwrap() else {
            return;
        };
        self.outbox.send(
            held.session,
            Update::Call(EntityMethodCall {
                entity_id: held.atc,
                method: method + CHANGE_TOKEN_OFFSET,
                args: change_permission_token_args(held.area.entity_id, TOKEN_FREE, 0, 0),
            }),
        );
    }

    /// `RmMulticastNotifyVehicleStored`: the kiosk's `OnVehicleStored` marks the ship stored with its location inventory.
    fn notify_stored(&self, held: &Held<B>, geid: u64) {
        let Some(method) = *held.request_method.lock().unwrap() else {
            return;
        };
        self.outbox.send(
            held.session,
            Update::Call(EntityMethodCall {
                entity_id: held.atc,
                method: method + crate::kiosk_calls::NOTIFY_VEHICLE_STORED_OFFSET - REQUEST_PERMISSION_INDEX,
                args: crate::kiosk_calls::vehicle_stored_args(geid, held.owner, held.hangar.location.location_id),
            }),
        );
    }

    /// The ship is in the bay: its token is occupied by it and its owner.
    fn ready(&self, held: &Held<B>, geid: u64) {
        let Some(method) = *held.request_method.lock().unwrap() else {
            return;
        };
        info!(ship = geid, owner = held.owner, "the ship is in the bay");
        self.outbox.send(
            held.session,
            Update::Call(EntityMethodCall {
                entity_id: held.atc,
                method: method + CHANGE_TOKEN_OFFSET,
                args: change_permission_token_args(
                    held.area.entity_id,
                    TOKEN_OCCUPIED,
                    held.owner,
                    geid,
                ),
            }),
        );
    }
}

/// The ship that stands on a hangar's raised pad, as the ASOP's list shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LandedShip {
    pub geid: u64,
    pub atc: u64,
    pub area: u64,
    pub location_id: u32,
}

/// The class of the ship a retrieval approved, as the backend has it.
pub fn class_of(view: &ShipView) -> Option<&str> {
    view.ship
        .as_ref()
        .map(|ship| ship.entity_class_guid.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ships::{PLATFORM_CLOSE_TIME, PLATFORM_OPEN_TIME};

    /// A worked example of a take off of an Arrow from the ASOP kiosk, 102 bytes (as in `atc_wire`'s tests).
    const REQUEST_PERMISSION: &str = concat!(
        "053f66d945000000",
        "8ca0db215d000000",
        "0000000000000000",
        "02",
        "2d",
        "00",
        "8877665544332211",
        "00000000",
        "053f66d945000000",
        "03",
        "d204000000000000",
        "0a414e564c5f4172726f77",
        "0e48616e6761725f536d6c46726e74",
        "01",
        "01",
        "053f66d945000000",
        "03",
        "d204000000000000",
        "00",
    );
    const PLAYER: u64 = 0x45_d966_3f05;
    const SHIP: u64 = 0x5d_21db_a08c;
    const ARROW: &str = "467cdeca-a5bc-4b29-ad21-44aed9bfce35";
    const SESSION: SessionKey = (1, 7);
    /// `Stanton2_Orison`, whose home hangar is the small front one.
    const ORISON: u32 = 1_105_811_199;

    fn bytes(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
    }

    #[tonic::async_trait]
    impl ShipBackend for Arc<Fake> {
        async fn list(&self, _: sc_api::internal::ListShipsRequest) -> Result<Vec<ShipView>, tonic::Status> {
            Ok(Vec::new())
        }
        async fn claim(&self, _: sc_api::internal::ClaimShipRequest) -> Result<ShipView, tonic::Status> {
            Err(tonic::Status::unimplemented("claim"))
        }
        async fn retrieve(&self, request: sc_api::internal::RetrieveShipRequest) -> Result<ShipView, tonic::Status> {
            self.calls.lock().unwrap().push(format!("retrieve {}", request.geid));
            Ok(ShipView {
                ship: Some(sc_api::internal::ShipRecord {
                    geid: request.geid,
                    entity_class_guid: ARROW.into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
        }
        async fn report(&self, request: sc_api::internal::ReportShipRequest) -> Result<ShipView, tonic::Status> {
            self.calls.lock().unwrap().push(format!("report {}", request.geid));
            Ok(ShipView::default())
        }
        async fn deliver(&self, _: sc_api::internal::DeliverShipRequest) -> Result<ShipView, tonic::Status> {
            Err(tonic::Status::unimplemented("deliver"))
        }
    }

    fn call(entity_id: u64, method: u32, args: Vec<u8>) -> EntityMethodRequest {
        EntityMethodRequest {
            replicant: SESSION.0,
            session: SESSION.1,
            player_id: PLAYER,
            entity_id,
            method,
            args,
        }
    }

    fn drain(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>,
    ) -> Vec<Update> {
        let mut all = Vec::new();
        while let Ok(Ok(update)) = updates.try_recv() {
            all.extend(update.update);
        }
        all
    }

    fn ready(backend: &Arc<Fake>, hangar: &HomeHangar) -> (Arc<Retrievals<Arc<Fake>>>, tokio::sync::mpsc::UnboundedReceiver<crate::outbox::Sent>) {
        let outbox = Outbox::default();
        let updates = outbox.subscribe(SESSION);
        let retrievals = Arc::new(Retrievals::new(outbox));
        retrievals.hold(SESSION, PLAYER, hangar, backend.clone());
        (retrievals, updates)
    }

    /// The second ship's request: the worked example with another vehicle id (its second u64).
    fn request_of(ship: u64) -> Vec<u8> {
        let mut args = bytes(REQUEST_PERMISSION);
        args[8..16].copy_from_slice(&ship.to_le_bytes());
        args
    }

    #[tokio::test]
    async fn a_retrieval_on_a_lowered_pad_spawns_the_ship_and_raises_it() {
        let hangar = HomeHangar::new(ORISON, 0, false).expect("a home hangar");
        let backend = Arc::new(Fake::default());
        let (retrievals, mut updates) = ready(&backend, &hangar);
        let atc = atc_id(&hangar);
        let held = retrievals.held.lock().unwrap().get(&atc).cloned().unwrap();
        // Another entity, or a call that is not a retrieval, is not taken.
        assert!(retrievals.request(&call(atc + 1, 40, bytes(REQUEST_PERMISSION))).await.is_none());
        assert!(retrievals.request(&call(atc, 40, vec![1, 2, 3])).await.is_none());
        let answer = retrievals
            .request(&call(atc, 40, bytes(REQUEST_PERMISSION)))
            .await
            .expect("the retrieval is answered");
        assert_eq!(answer.len(), 1);
        assert_eq!((answer[0].entity_id, answer[0].method), (atc, 42));
        assert_eq!(*backend.calls.lock().unwrap(), [format!("retrieve {SHIP}")]);
        // The ship is bound on the raised pad as soon as the platform has closed; the token is occupied when it is up.
        let sent = drain(&mut updates);
        let Some(Update::Binds(binds)) = sent.iter().find(|u| matches!(u, Update::Binds(_))) else {
            panic!("the ship is bound: {sent:?}")
        };
        assert!(retrievals.step(&held, Duration::from_secs(10)).await);
        assert!(drain(&mut updates).iter().all(|u| !matches!(u, Update::Call(_))));
        assert!(!retrievals.step(&held, Duration::from_secs(40)).await);
        let sent = drain(&mut updates);
        assert!(sent.iter().all(|u| !matches!(u, Update::Binds(_) | Update::Unbinds(_))), "the ship is bound once: {sent:?}");
        assert_eq!(
            (binds.binds[0].entity_id, binds.binds[0].class_crc, binds.binds[0].parent_id),
            (SHIP, class_crc(ARROW), hangar.entity_id)
        );
        let z = binds.binds[0].transform.as_ref().unwrap().z;
        assert!(z < held.area.pose.position[2] + SHIP_REST_HEIGHT, "the ship stands at the bottom: {z}");
        let calls: Vec<EntityMethodCall> = sent
            .into_iter()
            .filter_map(|u| if let Update::Call(call) = u { Some(call) } else { None })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].entity_id, calls[0].method), (atc, 40 + CHANGE_TOKEN_OFFSET));
        assert_eq!(
            calls[0].args,
            change_permission_token_args(held.area.entity_id, TOKEN_OCCUPIED, PLAYER, SHIP)
        );
    }

    #[tokio::test]
    async fn a_ship_on_the_raised_pad_is_lowered_and_stored_before_the_next_rises() {
        const NEXT: u64 = SHIP + 1;
        let hangar = HomeHangar::new(ORISON, 0, false).expect("a home hangar");
        let backend = Arc::new(Fake::default());
        let (retrievals, mut updates) = ready(&backend, &hangar);
        let atc = atc_id(&hangar);
        let held = retrievals.held.lock().unwrap().get(&atc).cloned().unwrap();
        retrievals.request(&call(atc, 40, request_of(SHIP))).await.unwrap();
        retrievals.step(&held, Duration::from_secs(40)).await;
        drain(&mut updates);
        // The same ship again is told it is granted already, and nothing moves.
        let again = retrievals.request(&call(atc, 40, request_of(SHIP))).await.unwrap();
        let asked = retrieval_request(&request_of(SHIP)).unwrap();
        assert_eq!(
            again[0],
            notify_call(atc, 42, &asked, NOTIFICATION_ALREADY_GRANTED, held.area.entity_id)
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1, "the backend is not asked for it");
        // Another one: the pad goes down with the first, which is unbound and stored, and then the second is bound and rises.
        retrievals.request(&call(atc, 40, request_of(NEXT))).await.unwrap();
        assert!(retrievals.step(&held, Duration::from_secs(80)).await, "the pad is down");
        let sent = drain(&mut updates);
        let unbound = sent.iter().position(|u| matches!(u, Update::Unbinds(_)));
        assert!(unbound.is_some(), "{sent:?}");
        let Some(Update::Unbinds(gone)) = sent.get(unbound.unwrap()) else { panic!() };
        assert_eq!(gone.entity_ids, [SHIP]);
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                format!("retrieve {SHIP}"),
                format!("retrieve {NEXT}"),
                format!("report {SHIP}"),
            ],
            "the first is stored, the second retrieved"
        );
        assert!(!retrievals.step(&held, Duration::from_secs(130)).await);
        let sent = drain(&mut updates);
        assert!(sent.iter().all(|u| !matches!(u, Update::Binds(_))), "the next ship was bound when the platform closed: {sent:?}");
    }

    #[tokio::test]
    async fn the_ship_rides_the_pad_up_and_down() {
        ride::POP_IN_FORCED.with(|forced| forced.set(Some(false)));
        const NEXT: u64 = SHIP + 1;
        let hangar = HomeHangar::new(ORISON, 0, false).expect("a home hangar");
        let backend = Arc::new(Fake::default());
        let (retrievals, mut updates) = ready(&backend, &hangar);
        let atc = atc_id(&hangar);
        let held = retrievals.held.lock().unwrap().get(&atc).cloned().unwrap();
        retrievals.request(&call(atc, 40, request_of(SHIP))).await.unwrap();
        let bound = drain(&mut updates)
            .into_iter()
            .find_map(|update| match update {
                Update::Binds(binds) => binds.binds.first().and_then(|bind| bind.transform.clone()),
                _ => None,
            })
            .expect("the ship's bind");
        let moves = |updates: Vec<Update>| -> Vec<f64> {
            updates
                .into_iter()
                .filter_map(|update| match update {
                    Update::Moved(moved) => Some(moved.position.unwrap().z),
                    _ => None,
                })
                .collect()
        };
        let travel = pad_lowered_offset(&hangar.location.hangar_class)[2];
        // Rising: from the bind's pose (the pit) to the up pose, one position a tick, never past it.
        let start = retrievals.started.elapsed();
        for tenth in 1..=10 {
            retrievals.step(&held, start + PLATFORM_OPEN_TIME * tenth / 10).await;
        }
        let rising = moves(drain(&mut updates));
        assert_eq!(rising.len(), 10);
        assert!(rising.windows(2).all(|pair| pair[0] < pair[1]), "{rising:?}");
        assert!(rising[0] > bound.z && rising[0] < bound.z - travel);
        assert!((rising[9] - (bound.z - travel)).abs() < 1e-9, "{} against {}", rising[9], bound.z - travel);
        // Lowering with the next retrieval: down to the pit before the ship is unbound.
        retrievals.request(&call(atc, 40, request_of(NEXT))).await.unwrap();
        let start = retrievals.started.elapsed();
        retrievals.step(&held, start + PLATFORM_CLOSE_TIME / 2).await;
        retrievals.step(&held, start + PLATFORM_CLOSE_TIME).await;
        let sent = drain(&mut updates);
        let unbound = sent.iter().position(|update| matches!(update, Update::Unbinds(_))).expect("unbound");
        let last_move = sent.iter().rposition(|update| matches!(update, Update::Moved(_))).expect("moved");
        assert!(last_move < unbound, "the ship is at the pit before it is unbound");
        let lowering = moves(sent);
        assert!(lowering.windows(2).all(|pair| pair[0] > pair[1]), "{lowering:?}");
        assert!((lowering.last().unwrap() - bound.z).abs() < 1e-9);
    }

    #[test]
    fn the_class_crc_is_the_seeds_guid_hash() {
        // DRAK_Marauder's `guidHashCrc` in the seed (entity_classes.pb, build 13165946); the text's CRC-32C, 0x0c612965,
        // was what the first try sent and the client did not know it.
        let marauder = "16099337-179f-473e-8bc9-545e38ef0a32";
        assert_eq!(class_crc(marauder), 0x34c5_df90);
        assert_eq!(class_crc(&marauder.to_uppercase()), 0x34c5_df90);
        assert_eq!(class_crc("not a guid"), 0);
    }

    /// The ship minus the landing area's point (container modifier + platform + area, in the hangar frame)
    /// of the official hangar ships, which the rest height is the lower of.
    #[test]
    fn the_rest_height_is_the_ships_origin_above_the_landing_area() {
        // VNCL_Stinger in Hangar_MediumFront: ship z 16.3138322; modifier 0.0 + platform 6.5 + area 0.038.
        let stinger = 16.3138322 - (0.0 + 6.5 + 0.038);
        // ANVL_Liberator in Hangar_XLTop: ship z -103.52; area -115.38.
        let liberator = -103.52 - (-115.38);
        assert!((stinger - SHIP_REST_HEIGHT).abs() < 0.01, "{stinger}");
        assert!(liberator > SHIP_REST_HEIGHT);
    }

    #[test]
    fn a_ship_with_a_keel_is_bound_above_it() {
        let keels = parse_keels("100\t25.645\n200\t2.574\n").unwrap();
        assert_eq!(keels[&100], 25.645);
        assert!(parse_keels("100 25").is_err());
        install_keels(keels);
        // The Idris M: its keel over the pad's top, with the clearance.
        let idris = ship_rest_height(100, PadSize::Xlarge);
        assert!((idris - (25.645 + AREA_BELOW_PAD_TOP + SETTLING_CLEARANCE)).abs() < 1e-9, "{idris}");
        // A small ship keeps the old height, and a ship with no keel gets its class's margin.
        assert_eq!(ship_rest_height(200, PadSize::Small), SHIP_REST_HEIGHT);
        assert_eq!(ship_rest_height(300, PadSize::Xlarge), rest_height(PadSize::Xlarge));
    }

    #[test]
    fn a_large_ship_is_bound_above_its_keel() {
        // A ship with no keel of its own (the Javelin: no gear, its hull reaches about 22.4 m below its origin) gets a margin over
        // that; bound at 9.78 its keel went through the pit floor.
        assert_eq!(rest_height(PadSize::Small), SHIP_REST_HEIGHT);
        assert!(rest_height(PadSize::Xlarge) > 22.39 + AREA_BELOW_PAD_TOP + SETTLING_CLEARANCE);
        assert!(rest_height(PadSize::Large) > SHIP_REST_HEIGHT);
        assert_eq!(rest_height(PadSize::Unspecified), SHIP_REST_HEIGHT);
    }

    #[test]
    fn a_hangars_landing_area_is_its_shells_static() {
        let hangar =
            HomeHangar::new(ORISON, 0, false).expect("a home hangar");
        let area = landing_area(&hangar).expect("the shell has a landing area");
        let platform = PlatformIds::for_hangar(&hangar).expect("a platform");
        assert!(area.entity_id > hangar.entity_id && area.entity_id < atc_id(&hangar));
        assert_ne!(area.entity_id, platform.platform);
        let lowered = pad_lowered_offset(&hangar.location.hangar_class);
        let bind = ship_bind_with(
            77,
            "467cdeca-a5bc-4b29-ad21-44aed9bfce35",
            &hangar,
            &area,
            None,
            Some([lowered[0], lowered[1], lowered[2] + SHIP_REST_HEIGHT]),
        );
        let default = ship_bind(77, "467cdeca-a5bc-4b29-ad21-44aed9bfce35", &hangar, &area, SHIP_REST_HEIGHT);
        assert_eq!(default.parent_id, hangar.entity_id);
        assert_eq!(default.transform, bind.transform);
        assert_ne!(default.parent_id, platform.platform);
        assert_eq!(
            (bind.entity_id, bind.parent_id, bind.flags, bind.transform),
            (77, hangar.entity_id, SHIP_FLAGS, {
                let mut pose = area.pose;
                pose.position[2] += SHIP_REST_HEIGHT + pad_lowered_offset(&hangar.location.hangar_class)[2];
                pose
            })
        );
        // The pad of a small hangar goes down 32 m; the other sizes more.
        assert_eq!(
            pad_lowered_offset(&hangar.location.hangar_class),
            [0.0, 0.0, -32.0],
            "Orison's hangar is a small one"
        );
        // The official ship binds (VNCL_Stinger and ANVL_Liberator, both under
        // their hangar with the zone attachment): flags 0x84a01400, one added component 0x1cadfa78.
        assert_eq!(bind.flags, 0x84a0_1400);
        assert_eq!(bind.components, [0x1cad_fa78]);
        assert_eq!(bind.parent_attach_id, ZONE_ATTACHMENT);
    }

    #[test]
    fn the_ship_can_be_bound_under_the_area_or_shifted_to_the_lowered_pad() {
        let hangar = HomeHangar::new(ORISON, 0, false).expect("a home hangar");
        let area = landing_area(&hangar).expect("the shell has a landing area");
        let guid = "467cdeca-a5bc-4b29-ad21-44aed9bfce35";
        let under = ship_bind_with(77, guid, &hangar, &area, Some(area.entity_id), None);
        assert_eq!(
            (under.parent_id, under.transform),
            (area.entity_id, Transform::IDENTITY)
        );
        let shifted = ship_bind_with(77, guid, &hangar, &area, None, Some([0.0, 0.0, -7.5]));
        assert_eq!(shifted.parent_id, hangar.entity_id);
        assert_eq!(shifted.transform.position[2], area.pose.position[2] - 7.5);
        assert_eq!(shifted.transform.position[..2], area.pose.position[..2]);
        assert_eq!(parse_offset(Some("0, 0,-7.5")), Some([0.0, 0.0, -7.5]));
        assert_eq!(parse_offset(Some("1,2")), None);
        assert_eq!(parse_offset(None), None);
    }

    #[test]
    fn only_a_take_off_with_is_spawn_is_a_retrieval() {
        let args = bytes(REQUEST_PERMISSION);
        let request = retrieval_request(&args).expect("the worked example is a retrieval");
        assert_eq!((request.player, request.vehicle), (PLAYER, SHIP));
        assert!(retrieval_request(&args[..args.len() - 1]).is_none());
    }

    #[test]
    fn the_granted_answer_names_the_area_and_keeps_the_request() {
        let request = ActionRequest {
            player: 5,
            vehicle: 9,
            mode: REQUEST_TAKE_OFF,
            notification: crate::atc_wire::NOTIFICATION_NONE,
            is_spawn: true,
            ..ActionRequest::default()
        };
        let call = notify_call(
            100,
            5,
            &request,
            NOTIFICATION_TAKE_OFF_AREA_GRANTED,
            777,
        );
        assert_eq!((call.entity_id, call.method), (100, 5));
        let mut named = request.clone();
        named.token = 777;
        let notification = Notification {
            kind: NOTIFICATION_TAKE_OFF_AREA_GRANTED,
            operator: 100,
            token: 777,
            ..Notification::default()
        };
        assert_eq!(call.args, notify_player_args(&named, &notification));
    }

    #[test]
    fn the_ships_being_stored_are_a_count_and_their_ids() {
        assert_eq!(being_stored_value(&[]), [0]);
        assert_eq!(being_stored_value(&[0x0102]), [1, 2, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_grant_names_the_hangar_for_the_retrieved_screen() {
        assert_eq!(hangar_name_key("Hangar_SmallFront_Standard"), HANGAR_NAME_SMALL);
        assert_eq!(hangar_name_key("Hangar_Large_Standard"), HANGAR_NAME_LARGE);
        assert_eq!(hangar_name_key("Elsewhere"), "");
    }

    #[test]
    fn a_busy_platform_queues_and_a_ship_that_does_not_fit_says_size() {
        assert_eq!(
            refusal_kind(Code::FailedPrecondition, true),
            NOTIFICATION_WAITING_IN_QUEUE
        );
        assert_eq!(
            refusal_kind(Code::FailedPrecondition, false),
            NOTIFICATION_NO_VALID_AREA_SIZE
        );
        assert_eq!(
            refusal_kind(Code::NotFound, false),
            NOTIFICATION_NO_VALID_AREA_TYPE
        );
    }
}

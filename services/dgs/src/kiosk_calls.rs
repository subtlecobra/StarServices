//! The remote methods a client calls on an ASOP kiosk the DGS holds, other than the interaction (`interaction.rs`), and the
//! answer to the one that fetches the ship list.
//!
//! The indices are positions in
//! the kiosk entity's one method vector, so they hold for the class they were taken from
//! (`ShipSelectorKiosk_Hangar_LowTech_1_a`).

use sc_api::internal::{ShipState, ShipView};

/// `RmAuthorityRequestVehicleInfos(CryGUID requestId, EntityId atcId)`: the request for the
/// ships of the player at this ATC. Answered on the player (below).
pub const KIOSK_REQUEST_VEHICLE_INFOS: u32 = 0;
/// The class CRC of `PersonalMobiGlas_PU`, the item whose ship list provider the mobiGlas Vehicle Manager uses (`loadout.rs`).
pub const MOBIGLAS_CLASS_CRC: u32 = 2_758_675_951;
/// The method of the same `RmAuthorityRequestVehicleInfos` on the mobiGlas entity: seven methods of earlier components come
/// first (derived: the request is 24 bytes, the GUID and an ATC id of 0). The answer is the same one the kiosk
/// gets, on the player (`INSURANCE_RESPONSE_METHOD`), whoever asked.
pub const MOBIGLAS_REQUEST_VEHICLE_INFOS: u32 = 7;
/// The byte length of the arguments of `RmAuthorityRequestVehicleInfos`: the request GUID and the ATC's entity id.
pub const REQUEST_VEHICLE_INFOS_LEN: usize = 24;

/// `RmAuthorityFetchPrices(u32 requestId, varint count, CryGUID[] classes)`: the price
/// of each class the list shows. Answered on the kiosk with `KIOSK_ON_FETCH_PRICES`.
pub const KIOSK_FETCH_PRICES: u32 = 4;
/// `RmOnFetchPrices(u32 requestId, varint count, (CryGUID class, f64 price)[])`, to the caller.
pub const KIOSK_ON_FETCH_PRICES: u32 = 13;
/// `RmAuthoritySetInteractionState(u32, u32)`: the UI state the user is in. No answer.
pub const KIOSK_SET_INTERACTION_STATE: u32 = 9;
/// `RmAuthorityRequestDeliver(CryGUID requestId, string urn, varint u32)`. Read from
/// the position in the component's list; not verified.
pub const KIOSK_REQUEST_DELIVER: u32 = 1;
/// `RmAuthorityRequestClaim(CryGUID requestId, string urn, varint u32, u64, bool)`: the claim button. The
/// client sends it with the ship's entitlement URN, 2, 0 and false, 1.9 s after it opened the claim window.
pub const KIOSK_REQUEST_CLAIM: u32 = 2;
/// `RmAuthorityClaimWindowOpened(string urn)`: the user picked a ship in the list.
pub const KIOSK_CLAIM_WINDOW_OPENED: u32 = 11;
/// `RmAuthorityStoreVehicle(EntityId vehicle)`: the Store button for a ship that
/// stands on the pad. Derived: the component's 6th method (5 in the component, 0 is the ship list
/// provider's one before it, so 6 on the entity, as `FetchPrices` 4 and `ClaimWindowOpened` 11); not verified.
pub const KIOSK_STORE_VEHICLE: u32 = 6;
/// A kiosk call of the interaction component that carries (`EntityId user`, `u32 type`, `u32 state`): the ship pick. The
/// official service answers it as `KIOSK_SET_INTERACTION_STATE`, with the index 3 variable set to the state.
pub const KIOSK_USER_STATE: u32 = 21;
/// `RmAuthorityCleanUserClaimChains()`. No answer.
pub const KIOSK_CLEAN_USER_CLAIM_CHAINS: u32 = 10;
/// `RmAuthoritySetUserId(EntityId)`: the user opening the kiosk. No answer.
pub const KIOSK_SET_USER_ID: u32 = 37;
/// `RmAuthorityAckClientConnection()`. No answer.
pub const KIOSK_ACK_CLIENT_CONNECTION: u32 = 38;

/// The environment variable that replaces the layout index of the kiosk's interaction state variable (below); `off`
/// (any case) or `0`... turns the update off.
pub const INTERACTION_STATE_INDEX_ENV: &str = "SC_DGS_KIOSK_STATE_INDEX";
/// The index, in the kiosk's layout, of the `uint32` variable the authority sets to the state of each
/// `KIOSK_SET_INTERACTION_STATE` the client sends. After the client's
/// 9 with `64cd28bf 2438f793` the official service sends an `EntitySnapshotUpdate` of that kiosk with variable 3 (`04` is the index
/// plus one) and the value `2438f793`, for each of the three states. Verified on one kiosk class only; a wrong
/// index writes another variable, so the update can be turned off with `SC_DGS_KIOSK_STATE_INDEX=off`.
pub const INTERACTION_STATE_INDEX: u32 = 3;
/// The value of `INTERACTION_STATE_INDEX_ENV` that turns the update off.
pub const INTERACTION_STATE_OFF: &str = "off";

/// The kiosk's layout index of the interaction state variable (`None`: no update).
pub fn interaction_state_index() -> Option<u32> {
    match std::env::var(INTERACTION_STATE_INDEX_ENV) {
        Ok(setting) if setting.eq_ignore_ascii_case(INTERACTION_STATE_OFF) => None,
        Ok(setting) => setting.parse().ok().or(Some(INTERACTION_STATE_INDEX)),
        Err(_) => Some(INTERACTION_STATE_INDEX),
    }
}

/// The variable update that confirms a `SetInteractionState` of `state` to the client: its value is the state as sent.
pub fn interaction_state_update(
    kiosk: u64,
    index: u32,
    state: u32,
) -> sc_api::internal::EntityVariables {
    sc_api::internal::EntityVariables {
        entity_id: kiosk,
        variables: vec![sc_api::internal::EntityVariable {
            index,
            value: state.to_le_bytes().to_vec(),
        }],
    }
}

/// The kiosk's layout index of its `LocationId` (a `uint32`) and of `kioskInteraction_kioskUser` (an entity id), and the
/// values the official service sets at the press: right after the client's use request (method 39) one `EntitySnapshotUpdate`
/// of the kiosk carries variable 0, the interaction state (variable 3) = `0x21343459` and the user (variable 21) = the
/// player's id. Variable 0, `0x3298b64a`, is 848868938, `StarMapObject.RR_P3_LEO`: the location of the
/// Pyro hangar, so it is the kiosk's own location and never a constant (copying it made the client ask for
/// the ships of Pyro in an Orison hangar). Without the user the client's interaction
/// ends by itself less than a second after it opens.
pub const KIOSK_LOCATION_INDEX: u32 = 0;
pub const KIOSK_IDLE_STATE: u32 = 0x2122_3459;
pub const KIOSK_USER_INDEX: u32 = 21;

/// The update the official service sends at a press: the kiosk's location (left out when it is not known: never an assumption), its
/// user the player and its state the idle one.
pub fn use_update(
    kiosk: u64,
    player: u64,
    location: Option<u32>,
    state_index: Option<u32>,
) -> sc_api::internal::EntityVariables {
    use sc_api::internal::EntityVariable;
    let mut variables = Vec::new();
    if let Some(location) = location {
        variables.push(EntityVariable {
            index: KIOSK_LOCATION_INDEX,
            value: location.to_le_bytes().to_vec(),
        });
    }
    variables.push(EntityVariable {
        index: KIOSK_USER_INDEX,
        value: player.to_le_bytes().to_vec(),
    });
    if let Some(index) = state_index {
        variables.push(EntityVariable {
            index,
            value: KIOSK_IDLE_STATE.to_le_bytes().to_vec(),
        });
    }
    sc_api::internal::EntityVariables {
        entity_id: kiosk,
        variables,
    }
}

/// The environment variable that overrides the player's method index of
/// `RmTokenResponseFetchServerInfos`, the answer to `KIOSK_REQUEST_VEHICLE_INFOS`. `0` or `off` (any
/// case) disables the reply; a number replaces `INSURANCE_RESPONSE_DEFAULT`.
pub const INSURANCE_RESPONSE_METHOD_ENV: &str = "SC_DGS_INSURANCE_RESPONSE_METHOD";
/// The value of the environment variable that disables the reply.
pub const INSURANCE_RESPONSE_OFF: &str = "off";

/// The player entity's method index of `RmTokenResponseFetchServerInfos`: 156. The answer to the kiosk's method 0 is an `EntityRemoteMethodFrom` to
/// the player entity with method 156 and the request's GUID first. The component's methods follow in registration order, so
/// `RmTokenResponseDeliver` is 157 and the claim's answer 158 (the answer to a claim is a 158).
pub const INSURANCE_RESPONSE_METHOD: u32 = 156;
/// The default method: the official index; `SC_DGS_INSURANCE_RESPONSE_METHOD` replaces it and `off` disables the reply.
pub const INSURANCE_RESPONSE_DEFAULT: Option<u32> = Some(INSURANCE_RESPONSE_METHOD);

/// The environment variable that turns off the answer to a claim request (`off`, any case).
pub const CLAIM_RESPONSE_ENV: &str = "SC_DGS_CLAIM_RESPONSE";
/// `RmTokenResponseClaim` follows `RmTokenResponseFetchServerInfos` and `RmTokenResponseDeliver` on the player.
pub const CLAIM_RESPONSE_OFFSET: u32 = 2;
/// The claim answer's fields after the request's GUID, as the official service sends them: an
/// opaque `u64` (none known, sent as 0), the player's entity id, the varint 3 (assumed: the
/// outcome), the `u32` of the kiosk's location (0x3298b64a is the Pyro hangar's, 848868938), a `u32` 0 and ten bytes that are copied as they
/// were. The `u64` is the vehicle's entity id: the client's claim handler logged "Invalid vehicle id received when claiming
/// vehicle" for 0. The rest is not decoded: a copy of the official bytes, to be checked in game.
pub const CLAIM_OUTCOME: u8 = 3;
pub const CLAIM_TAIL: [u8; 10] = [0x20, 0x01, 0x0a, 0xd9, 0x09, 0x3e, 0xd7, 0x00, 0x00, 0x00];

/// The arguments of `RmTokenResponseClaim` for a claim request of `player`.
pub fn claim_response(request_id: &[u8; GUID_LEN], vehicle: u64, player: u64, location: u32) -> Vec<u8> {
    let mut args = request_id.to_vec();
    args.extend_from_slice(&vehicle.to_le_bytes());
    args.extend_from_slice(&player.to_le_bytes());
    args.push(CLAIM_OUTCOME);
    args.extend_from_slice(&location.to_le_bytes());
    args.extend_from_slice(&0u32.to_le_bytes());
    args.extend_from_slice(&CLAIM_TAIL);
    args
}

/// The status of a claim answer that failed (the success one is the `0x20` that opens `CLAIM_TAIL`): the client turns any
/// other status into an error and ends the request, which it keeps pending, with its buttons locked, when it gets no answer.
pub const CLAIM_FAILED_STATUS: u8 = 4;

/// The arguments of `RmTokenResponseClaim` for a claim that failed: no vehicle, and the failed status.
pub fn claim_failure(request_id: &[u8; GUID_LEN], player: u64, location: u32) -> Vec<u8> {
    let mut args = claim_response(request_id, 0, player, location);
    let status = args.len() - CLAIM_TAIL.len();
    args[status] = CLAIM_FAILED_STATUS;
    args
}

/// A `CryGUID`: the request id, written as two little-endian `u64`.
pub const GUID_LEN: usize = 16;
/// An entity id.
pub const ENTITY_ID_LEN: usize = 8;
/// The varint of a count of none.
pub const EMPTY_COUNT: u8 = 0;
/// The answer's lists after the request id: `canDockHereList`, `destroyedVehicles` and `landedVehiclesInfos`, each a varint
/// count (`SQueryServerInfos`). An empty list is a valid answer: the player's stored ships come
/// to the client from its own entity graph and entitlement queries, and only landed ones are listed here.
pub const ANSWER_LISTS: usize = 3;
/// The varint that ends the answer. The official service sends 32 (`0x20`) after a list with one class and no ship; the client's reader names it only as a `u32`.
pub const ANSWER_TRAILER: u8 = 32;

/// A kiosk method as the DGS reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KioskCall {
    RequestVehicleInfos {
        request_id: [u8; GUID_LEN],
        atc: u64,
    },
    FetchPrices {
        request: u32,
        classes: Vec<[u8; GUID_LEN]>,
    },
    SetInteractionState {
        state_type: u32,
        state: u32,
    },
    CleanUserClaimChains,
    UserState {
        user: u64,
        state_type: u32,
        state: u32,
    },
    ClaimWindowOpened {
        urn: String,
    },
    RequestClaim {
        request_id: [u8; GUID_LEN],
        urn: String,
        kind: u64,
        amount: u64,
        flag: bool,
    },
    RequestDeliver {
        request_id: [u8; GUID_LEN],
        urn: String,
        kind: u64,
    },
    SetUserId {
        user: u64,
    },
    AckClientConnection,
    StoreVehicle {
        vehicle: u64,
    },
}

impl KioskCall {
    /// `None` for a method that is not one of these or whose arguments are not the length it has.
    pub fn parse(method: u32, args: &[u8]) -> Option<Self> {
        let u32_at = |at: usize| Some(u32::from_le_bytes(args.get(at..at + 4)?.try_into().ok()?));
        match method {
            KIOSK_REQUEST_VEHICLE_INFOS if args.len() == GUID_LEN + ENTITY_ID_LEN => {
                Some(Self::RequestVehicleInfos {
                    request_id: args[..GUID_LEN].try_into().ok()?,
                    atc: u64::from_le_bytes(args[GUID_LEN..].try_into().ok()?),
                })
            }
            KIOSK_FETCH_PRICES => {
                let request = u32_at(0)?;
                let (count, used) = read_varint(args.get(U32_LEN..)?)?;
                let body = args.get(U32_LEN + used..)?;
                let count = usize::try_from(count)
                    .ok()
                    .filter(|count| count.checked_mul(GUID_LEN) == Some(body.len()))?;
                let classes = body
                    .chunks_exact(GUID_LEN)
                    .take(count)
                    .map(|class| class.try_into().ok())
                    .collect::<Option<Vec<_>>>()?;
                Some(Self::FetchPrices { request, classes })
            }
            KIOSK_SET_INTERACTION_STATE if args.len() == 8 => Some(Self::SetInteractionState {
                state_type: u32_at(0)?,
                state: u32_at(4)?,
            }),
            KIOSK_USER_STATE if args.len() == ENTITY_ID_LEN + 2 * U32_LEN => {
                Some(Self::UserState {
                    user: u64::from_le_bytes(args[..ENTITY_ID_LEN].try_into().ok()?),
                    state_type: u32_at(ENTITY_ID_LEN)?,
                    state: u32_at(ENTITY_ID_LEN + U32_LEN)?,
                })
            }
            KIOSK_CLAIM_WINDOW_OPENED => {
                let (urn, used) = read_string(args)?;
                (used == args.len()).then_some(Self::ClaimWindowOpened { urn })
            }
            KIOSK_REQUEST_CLAIM | KIOSK_REQUEST_DELIVER => {
                let request_id: [u8; GUID_LEN] = args.get(..GUID_LEN)?.try_into().ok()?;
                let (urn, used) = read_string(&args[GUID_LEN..])?;
                let rest = &args[GUID_LEN + used..];
                let (kind, kind_len) = read_varint(rest)?;
                let rest = &rest[kind_len..];
                if method == KIOSK_REQUEST_DELIVER {
                    return rest.is_empty().then_some(Self::RequestDeliver {
                        request_id,
                        urn,
                        kind,
                    });
                }
                (rest.len() == ENTITY_ID_LEN + 1).then(|| Self::RequestClaim {
                    request_id,
                    urn,
                    kind,
                    amount: u64::from_le_bytes(
                        rest[..ENTITY_ID_LEN].try_into().unwrap_or_default(),
                    ),
                    flag: rest[ENTITY_ID_LEN] != 0,
                })
            }
            KIOSK_CLEAN_USER_CLAIM_CHAINS if args.is_empty() => Some(Self::CleanUserClaimChains),
            KIOSK_SET_USER_ID if args.len() == ENTITY_ID_LEN => Some(Self::SetUserId {
                user: u64::from_le_bytes(args.try_into().ok()?),
            }),
            KIOSK_ACK_CLIENT_CONNECTION if args.is_empty() => Some(Self::AckClientConnection),
            KIOSK_STORE_VEHICLE if args.len() == ENTITY_ID_LEN => Some(Self::StoreVehicle {
                vehicle: u64::from_le_bytes(args.try_into().ok()?),
            }),
            _ => None,
        }
    }
}

/// The bytes of a `u32` argument.
const U32_LEN: usize = 4;

/// The radix of the hex text of a GUID.
const GUID_HEX_RADIX: u32 = 16;
/// The bytes of one half of a `CryGUID`; each half goes on the wire in little endian.
const GUID_HALF_LEN: usize = GUID_LEN / 2;
/// The bits of a varint byte that carry the value, and the bit that says another byte follows.
const VARINT_VALUE_BITS: u32 = 7;
const VARINT_MORE: u8 = 0x80;

/// The wire form of a class GUID written as text (`a6e1bb18-8a11-4bbc-865e-45fd84e7b469`): its digits as bytes, each half of
/// eight reversed. `None` unless it holds exactly 32 hexadecimal digits.
pub fn class_guid_wire(text: &str) -> Option<[u8; GUID_LEN]> {
    let digits: Vec<u8> = text.bytes().filter(|byte| *byte != b'-').collect();
    if digits.len() != GUID_LEN * 2 {
        return None;
    }
    let mut bytes = [0u8; GUID_LEN];
    for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, GUID_HEX_RADIX).ok()?;
    }
    for half in bytes.chunks_exact_mut(GUID_HALF_LEN) {
        half.reverse();
    }
    Some(bytes)
}

/// The string (a varint length, then the bytes) at the start of `bytes` and the bytes it took.
fn read_string(bytes: &[u8]) -> Option<(String, usize)> {
    let (length, used) = read_varint(bytes)?;
    let end = used.checked_add(usize::try_from(length).ok()?)?;
    Some((String::from_utf8(bytes.get(used..end)?.to_vec()).ok()?, end))
}

/// The value of the varint at the start of `bytes` and the bytes it took.
fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (at, byte) in bytes.iter().enumerate() {
        let shift = VARINT_VALUE_BITS * u32::try_from(at).ok()?;
        value |= u64::from(byte & (VARINT_MORE - 1)).checked_shl(shift)?;
        if byte & VARINT_MORE == 0 {
            return Some((value, at + 1));
        }
    }
    None
}

/// The price of each class a player's ships hold, by the class's wire GUID: what the ship was bought for
/// (`ShipRecord.price_auec`), the first of a class met.
pub fn class_prices(ships: &[ShipView]) -> Vec<([u8; GUID_LEN], f64)> {
    let mut prices: Vec<([u8; GUID_LEN], f64)> = Vec::new();
    for ship in ships.iter().filter_map(|view| view.ship.as_ref()) {
        if let Some(class) = class_guid_wire(&ship.entity_class_guid)
            .filter(|class| prices.iter().all(|(known, _)| known != class))
        {
            prices.push((class, ship.price_auec as f64));
        }
    }
    prices
}

/// The arguments of `RmOnFetchPrices`: the request's number echoed and, for every class asked (in that order), its price,
/// 0 for a class that is none of the player's ships (the official service answers every class asked, unknown ones with 0).
pub fn prices_reply(
    request: u32,
    classes: &[[u8; GUID_LEN]],
    prices: &[([u8; GUID_LEN], f64)],
) -> Vec<u8> {
    let mut args = request.to_le_bytes().to_vec();
    push_varint(&mut args, classes.len() as u64);
    for class in classes {
        let price = prices
            .iter()
            .find(|(known, _)| known == class)
            .map_or(NO_PRICE, |(_, price)| *price);
        args.extend_from_slice(class);
        args.extend_from_slice(&price.to_le_bytes());
    }
    args
}

/// The price of a class that is none of the player's.
const NO_PRICE: f64 = 0.0;

pub fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= u64::from(VARINT_MORE) {
        out.push((value as u8 & (VARINT_MORE - 1)) | VARINT_MORE);
        value >>= VARINT_VALUE_BITS;
    }
    out.push(value as u8);
}

/// The ships of a player's list as the answer holds them (see `server_infos`): the classes that may dock at the kiosk's
/// hangar, each once in the order met, and the entities of the ships that were destroyed. A ship may dock when it is
/// stored or deployed and fits the hangar; a ship whose class GUID is not one is left out.
pub fn ship_lists(ships: &[ShipView]) -> (Vec<[u8; GUID_LEN]>, Vec<u64>) {
    let mut can_dock: Vec<[u8; GUID_LEN]> = Vec::new();
    let mut destroyed = Vec::new();
    for view in ships {
        let Some(ship) = view.ship.as_ref() else {
            continue;
        };
        match ShipState::try_from(ship.state) {
            Ok(ShipState::Destroyed) if ship.geid != 0 => destroyed.push(ship.geid),
            Ok(ShipState::Stored | ShipState::Deployed) if view.fits => {
                if let Some(class) = class_guid_wire(&ship.entity_class_guid)
                    .filter(|class| !can_dock.contains(class))
                {
                    can_dock.push(class);
                }
            }
            _ => {}
        }
    }
    (can_dock, destroyed)
}

/// `EInventoryContext::Location`: the inventory a ship stowed at a location is in.
pub const INVENTORY_CONTEXT_LOCATION: u64 = 3;
/// The three bools of `SLandedVehicleInfos`, bits LSB first (4.10.3): bit 0 IsBricked, bit 1
/// IsRoot, bit 2 'can store'. A ship on the pad can be stored and is not a root, so Retrieve (which needs IsRoot) does not win.
pub const LANDED_FLAGS: u8 = 0b100;
/// `RmMulticastNotifyVehicleStored` is the ATC's method 16 (relative to the list start; the request is 3).
pub const NOTIFY_VEHICLE_STORED_OFFSET: u32 = 16;

/// The arguments of `RmMulticastNotifyVehicleStored` (4.10.3): the vehicle, the player, the
/// location (u32, only logged), the ship's stow inventory (`<player>:Location:<location>`: holder, context varint, subject) and
/// `ok` as one bit. The kiosk's `OnVehicleStored` writes that inventory as the ship's stow context.
pub fn vehicle_stored_args(vehicle: u64, player: u64, location_id: u32) -> Vec<u8> {
    let mut args = Vec::new();
    args.extend_from_slice(&vehicle.to_le_bytes());
    args.extend_from_slice(&player.to_le_bytes());
    args.extend_from_slice(&location_id.to_le_bytes());
    args.extend_from_slice(&player.to_le_bytes());
    push_varint(&mut args, INVENTORY_CONTEXT_LOCATION);
    args.extend_from_slice(&u64::from(location_id).to_le_bytes());
    args.push(1);
    args
}

/// One `SLandedVehicleInfos`: a ship of the player that stands landed at an ATC (the ship on the pad).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedRecord {
    pub entity_id: u64,
    pub class: [u8; GUID_LEN],
    pub owner: u64,
    pub location_id: u32,
    pub atc: u64,
    pub area: u64,
}

/// The ship on the pad of a hangar as a landed record, from the player's ships. `None` when the list does not hold it or its
/// class is not a GUID.
pub fn landed_records(
    ships: &[ShipView],
    landed: Option<crate::retrieve::LandedShip>,
    owner: u64,
) -> Vec<LandedRecord> {
    let Some(landed) = landed else {
        return Vec::new();
    };
    ships
        .iter()
        .filter_map(|view| view.ship.as_ref())
        .find(|ship| ship.geid == landed.geid)
        .and_then(|ship| class_guid_wire(&ship.entity_class_guid))
        .map(|class| LandedRecord {
            entity_id: landed.geid,
            class,
            owner,
            location_id: landed.location_id,
            atc: landed.atc,
            area: landed.area,
        })
        .into_iter()
        .collect()
}

/// The wire form of a landed record, in this order (the inventory id as holder, context varint,
/// subject; the area's name an empty string) (4.10.3).
fn push_landed(out: &mut Vec<u8>, record: &LandedRecord) {
    out.extend_from_slice(&record.entity_id.to_le_bytes());
    out.extend_from_slice(&record.class);
    out.extend_from_slice(&record.owner.to_le_bytes());
    out.extend_from_slice(&record.owner.to_le_bytes());
    push_varint(out, INVENTORY_CONTEXT_LOCATION);
    out.extend_from_slice(&u64::from(record.location_id).to_le_bytes());
    out.extend_from_slice(&NO_TIMESTAMP.to_le_bytes());
    out.extend_from_slice(&record.atc.to_le_bytes());
    out.extend_from_slice(&record.area.to_le_bytes());
    push_varint(out, 0);
    out.push(LANDED_FLAGS);
}

/// The ownership timestamp of a landed record when none is known.
const NO_TIMESTAMP: u64 = 0;

/// The arguments of `RmTokenResponseFetchServerInfos` (`SQueryServerInfos`): the request's own id, the classes that may dock,
/// the destroyed ships, no landed ship (the official service lists none for a player whose ships are not landed) and the
/// trailer.
pub fn server_infos(
    request_id: &[u8; GUID_LEN],
    can_dock: &[[u8; GUID_LEN]],
    destroyed: &[u64],
) -> Vec<u8> {
    server_infos_landed(request_id, can_dock, destroyed, &[])
}

/// `server_infos` with the ships that stand landed at the kiosk's ATC.
pub fn server_infos_landed(
    request_id: &[u8; GUID_LEN],
    can_dock: &[[u8; GUID_LEN]],
    destroyed: &[u64],
    landed: &[LandedRecord],
) -> Vec<u8> {
    let mut args = request_id.to_vec();
    push_varint(&mut args, can_dock.len() as u64);
    can_dock
        .iter()
        .for_each(|class| args.extend_from_slice(class));
    push_varint(&mut args, destroyed.len() as u64);
    destroyed
        .iter()
        .for_each(|ship| args.extend_from_slice(&ship.to_le_bytes()));
    push_varint(&mut args, landed.len() as u64);
    landed.iter().for_each(|record| push_landed(&mut args, record));
    args.push(ANSWER_TRAILER);
    args
}

/// The arguments of `RmTokenResponseFetchServerInfos` that list no ship: the request's own id, then empty lists.
pub fn empty_server_infos(request_id: &[u8; GUID_LEN]) -> Vec<u8> {
    server_infos(request_id, &[], &[])
}

/// The method index of the answer: `INSURANCE_RESPONSE_DEFAULT` (none) unless the environment gives a number.
pub fn insurance_response_method() -> Option<u32> {
    response_method(std::env::var(INSURANCE_RESPONSE_METHOD_ENV).ok().as_deref())
}

/// `insurance_response_method` for a given value of the environment variable.
pub fn response_method(setting: Option<&str>) -> Option<u32> {
    let Some(setting) = setting.map(str::trim).filter(|setting| !setting.is_empty()) else {
        return INSURANCE_RESPONSE_DEFAULT;
    };
    if setting.eq_ignore_ascii_case(INSURANCE_RESPONSE_OFF) {
        return None;
    }
    match setting.parse::<u32>() {
        Ok(0) => None,
        Ok(method) => Some(method),
        Err(_) => INSURANCE_RESPONSE_DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_failed_claim_answer_differs_from_the_success_only_in_vehicle_and_status() {
        let id = [7u8; GUID_LEN];
        let good = claim_response(&id, 42, 9, 5);
        let bad = claim_failure(&id, 9, 5);
        assert_eq!(good.len(), bad.len());
        let status = good.len() - CLAIM_TAIL.len();
        assert_eq!((good[status], bad[status]), (CLAIM_TAIL[0], CLAIM_FAILED_STATUS));
        assert_eq!(&bad[GUID_LEN..GUID_LEN + 8], &0u64.to_le_bytes());
    }

    use super::*;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    /// The five calls of a press on a kiosk.
    #[test]
    fn the_claim_answer_is_the_official_one_with_its_own_ids() {
        let guid: [u8; GUID_LEN] = bytes("b11fe79fdab921429f9c9fdb917a658e")
            .try_into()
            .unwrap();
        let answer = claim_response(&guid, 0xd8_2678_799b, 0x4d_5aed_851d, 0x3298_b64a);
        assert_eq!(answer.len(), 51);
        assert_eq!(&answer[..16], &guid);
        assert_eq!(hex_of(&answer[16..24]), "9b797826d8000000");
        assert_eq!(
            hex_of(&answer[24..]),
            "1d85ed5a4d000000034ab698320000000020010ad9093ed7000000"
        );
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_claim_calls_read_as_the_client_sent_them() {
        let urn = "urn:sc:platform:entitlement:uuid:679b830c-884e-5367-a690-587450321967";
        let window = bytes("4575726e3a73633a706c6174666f726d3a656e7469746c656d656e743a757569643a36373962383330632d383834652d353336372d613639302d353837343530333231393637");
        assert_eq!(
            KioskCall::parse(KIOSK_CLAIM_WINDOW_OPENED, &window),
            Some(KioskCall::ClaimWindowOpened { urn: urn.into() })
        );
        let mut args = bytes("b11fe79fdab921429f9c9fdb917a658e");
        args.extend_from_slice(&window);
        args.extend_from_slice(&bytes("02000000000000000000"));
        match KioskCall::parse(KIOSK_REQUEST_CLAIM, &args) {
            Some(KioskCall::RequestClaim {
                urn: read,
                kind,
                amount,
                flag,
                ..
            }) => assert_eq!((read.as_str(), kind, amount, flag), (urn, 2, 0, false)),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            KioskCall::parse(KIOSK_USER_STATE, &bytes("1d85ed5a4d00000064cd28bf4baa307d")),
            Some(KioskCall::UserState {
                user: 0x4d_5aed_851d,
                state_type: 0xbf28_cd64,
                state: 0x7d30_aa4b
            })
        );
    }

    #[test]
    fn the_calls_of_a_press_are_read() {
        assert_eq!(
            KioskCall::parse(37, &bytes("053f66d945000000")),
            Some(KioskCall::SetUserId {
                user: 0x45_d966_3f05
            })
        );
        assert_eq!(
            KioskCall::parse(10, &[]),
            Some(KioskCall::CleanUserClaimChains)
        );
        let press = use_update(
            0xd7_e5ce_14a7,
            0x4d_5aed_851d,
            Some(0x3298_b64a),
            Some(INTERACTION_STATE_INDEX),
        );
        let indexed: Vec<(u32, Vec<u8>)> = press
            .variables
            .iter()
            .map(|variable| (variable.index, variable.value.clone()))
            .collect();
        assert_eq!(
            indexed,
            [
                (0, bytes("4ab69832")),
                (21, bytes("1d85ed5a4d000000")),
                (3, bytes("59342221"))
            ],
            "the press"
        );
        let update = interaction_state_update(0xd7_e5ce_14a7, INTERACTION_STATE_INDEX, 0x93f7_3824);
        assert_eq!(
            (
                update.entity_id,
                update.variables[0].index,
                update.variables[0].value.clone()
            ),
            (0xd7_e5ce_14a7, 3, bytes("2438f793"))
        );
        assert_eq!(
            KioskCall::parse(9, &bytes("64cd28bfb6d5d7bd")),
            Some(KioskCall::SetInteractionState {
                state_type: 0xbf28_cd64,
                state: 0xbdd7_d5b6
            })
        );
        let call = KioskCall::parse(
            0,
            &bytes("641f74e6a6062f4e8fcb4f89014639cc74990ac034000000"),
        )
        .unwrap();
        let KioskCall::RequestVehicleInfos { request_id, atc } = call else {
            panic!("not a request")
        };
        assert_eq!(atc, 226_560_219_508);
        assert_eq!(
            empty_server_infos(&request_id)[..GUID_LEN],
            bytes("641f74e6a6062f4e8fcb4f89014639cc")[..]
        );
    }

    #[test]
    fn a_wrong_length_is_not_a_kiosk_call() {
        assert_eq!(KioskCall::parse(0, &[0; 8]), None);
        assert_eq!(KioskCall::parse(37, &[]), None);
        assert_eq!(KioskCall::parse(11, &[]), None);
    }

    #[test]
    fn the_empty_answer_is_the_id_three_empty_counts_and_the_trailer() {
        let answer = empty_server_infos(&[7; GUID_LEN]);
        assert_eq!(answer.len(), GUID_LEN + ANSWER_LISTS + 1);
        assert!(answer[GUID_LEN..GUID_LEN + ANSWER_LISTS]
            .iter()
            .all(|byte| *byte == EMPTY_COUNT));
        assert_eq!(answer[GUID_LEN + ANSWER_LISTS], ANSWER_TRAILER);
    }

    /// The official service's answer to a request: the GUID, one class in
    /// `canDockHereList`, no destroyed or landed ship and the trailer. Ours differs only in the class list (empty).
    #[test]
    fn the_answer_has_our_layout() {
        let official =
            bytes("7f152a253e6e8e43bf941a8904d4df9401bc4b118a18bbe1a669b4e784fd455e86000020");
        let ours = empty_server_infos(official[..GUID_LEN].try_into().unwrap());
        assert_eq!(
            ours,
            bytes("7f152a253e6e8e43bf941a8904d4df94000000")
                .into_iter()
                .chain([ANSWER_TRAILER])
                .collect::<Vec<u8>>()
        );
        assert_eq!(official.len(), GUID_LEN + 1 + GUID_LEN + 2 + 1);
        assert_eq!(*official.last().unwrap(), ANSWER_TRAILER);
    }

    /// The kiosk's price request and answer, cut to three of its twelve classes.
    #[test]
    fn the_prices_asked_are_all_answered_in_order_with_zero_for_unknown_classes() {
        let aurora = class_guid_wire("a6e1bb18-8a11-4bbc-865e-45fd84e7b469").unwrap();
        let unknown = [9u8; GUID_LEN];
        let mut asked = 1u32.to_le_bytes().to_vec();
        asked.extend([2]);
        asked.extend(unknown);
        asked.extend(aurora);
        assert_eq!(
            KioskCall::parse(KIOSK_FETCH_PRICES, &asked),
            Some(KioskCall::FetchPrices {
                request: 1,
                classes: vec![unknown, aurora]
            })
        );
        assert_eq!(
            KioskCall::parse(KIOSK_FETCH_PRICES, &asked[..asked.len() - 1]),
            None
        );
        let reply = prices_reply(1, &[unknown, aurora], &[(aurora, 680_400.0)]);
        let mut official = asked[..U32_LEN + 1].to_vec();
        official.extend(unknown);
        official.extend(0f64.to_le_bytes());
        official.extend(bytes("bc4b118a18bbe1a669b4e784fd455e8600000000a0c32441"));
        assert_eq!(reply, official);
    }

    #[test]
    fn a_varint_is_read_back() {
        assert_eq!(read_varint(&[0xa5, 0x02]), Some((293, 2)));
        assert_eq!(read_varint(&[0xa5]), None);
    }

    #[test]
    fn a_class_guid_goes_on_the_wire_with_each_half_reversed() {
        let official = bytes("bc4b118a18bbe1a669b4e784fd455e86");
        assert_eq!(
            class_guid_wire("a6e1bb18-8a11-4bbc-865e-45fd84e7b469")
                .unwrap()
                .to_vec(),
            official
        );
        assert_eq!(
            class_guid_wire("a6e1bb188a114bbc865e45fd84e7b469"),
            class_guid_wire("a6e1bb18-8a11-4bbc-865e-45fd84e7b469")
        );
        assert_eq!(class_guid_wire("a6e1bb18"), None);
    }

    #[test]
    fn the_answer_for_the_ship_is_the_official_answer() {
        let id: [u8; GUID_LEN] = bytes("7f152a253e6e8e43bf941a8904d4df94")
            .try_into()
            .unwrap();
        let class = class_guid_wire("a6e1bb18-8a11-4bbc-865e-45fd84e7b469").unwrap();
        assert_eq!(
            server_infos(&id, &[class], &[]),
            bytes("7f152a253e6e8e43bf941a8904d4df9401bc4b118a18bbe1a669b4e784fd455e86000020")
        );
    }

    #[test]
    fn the_store_call_and_the_landed_record_of_the_ship_on_the_pad() {
        use crate::retrieve::LandedShip;
        use sc_api::internal::ShipRecord;
        const AURORA: &str = "a6e1bb18-8a11-4bbc-865e-45fd84e7b469";
        assert_eq!(
            KioskCall::parse(KIOSK_STORE_VEHICLE, &42u64.to_le_bytes()),
            Some(KioskCall::StoreVehicle { vehicle: 42 })
        );
        assert_eq!(KioskCall::parse(KIOSK_STORE_VEHICLE, &[0; 4]), None);
        let ships = [ShipView {
            ship: Some(ShipRecord {
                geid: 42,
                entity_class_guid: AURORA.into(),
                state: ShipState::Deployed as i32,
                ..Default::default()
            }),
            fits: true,
            ..Default::default()
        }];
        let landed = LandedShip { geid: 42, atc: 500, area: 600, location_id: 9 };
        assert!(landed_records(&ships, None, 7).is_empty());
        assert!(landed_records(&ships, Some(LandedShip { geid: 43, ..landed }), 7).is_empty());
        let records = landed_records(&ships, Some(landed), 7);
        assert_eq!(records.len(), 1);
        let id = [1u8; GUID_LEN];
        let answer = server_infos_landed(&id, &[], &[], &records);
        let class = class_guid_wire(AURORA).unwrap();
        let expected = [
            &id[..],
            &[0, 0, 1],
            &42u64.to_le_bytes(),
            &class,
            &7u64.to_le_bytes(),
            &7u64.to_le_bytes(),
            &[INVENTORY_CONTEXT_LOCATION as u8],
            &9u64.to_le_bytes(),
            &0u64.to_le_bytes(),
            &500u64.to_le_bytes(),
            &600u64.to_le_bytes(),
            &[0, LANDED_FLAGS, ANSWER_TRAILER],
        ]
        .concat();
        assert_eq!(answer, expected);
        assert_eq!(server_infos(&id, &[], &[]), server_infos_landed(&id, &[], &[], &[]));
    }

    #[test]
    fn the_stored_notice_carries_the_location_inventory() {
        let args = vehicle_stored_args(42, 7, 9);
        assert_eq!(
            args,
            [
                &42u64.to_le_bytes()[..],
                &7u64.to_le_bytes(),
                &9u32.to_le_bytes(),
                &7u64.to_le_bytes(),
                &[INVENTORY_CONTEXT_LOCATION as u8],
                &9u64.to_le_bytes(),
                &[1],
            ]
            .concat()
        );
    }

    #[test]
    fn destroyed_ships_are_listed_by_their_entity_and_counts_are_varints() {
        let id = [0u8; GUID_LEN];
        let answer = server_infos(&id, &[], &[7, 9]);
        assert_eq!(
            &answer[GUID_LEN..],
            [
                &[0, 2][..],
                &7u64.to_le_bytes(),
                &9u64.to_le_bytes(),
                &[EMPTY_COUNT, ANSWER_TRAILER]
            ]
            .concat()
        );
        let mut out = Vec::new();
        push_varint(&mut out, 300);
        assert_eq!(out, [0xac, 0x02]);
    }

    #[test]
    fn only_ships_that_fit_and_are_in_use_may_dock_and_each_class_counts_once() {
        use sc_api::internal::ShipRecord;
        const AURORA: &str = "a6e1bb18-8a11-4bbc-865e-45fd84e7b469";
        const OTHER: &str = "00000000-0000-0000-0000-000000000001";
        const LOST: u64 = 77;
        let view = |class: &str, state: ShipState, fits: bool, geid: u64| ShipView {
            ship: Some(ShipRecord {
                entity_class_guid: class.into(),
                state: state as i32,
                geid,
                ..ShipRecord::default()
            }),
            fits,
            ..ShipView::default()
        };
        let ships = [
            view(AURORA, ShipState::Stored, true, 1),
            view(AURORA, ShipState::Deployed, true, 2),
            view(OTHER, ShipState::Stored, false, 3),
            view(OTHER, ShipState::Expired, true, 4),
            view(OTHER, ShipState::Destroyed, true, LOST),
        ];
        let (can_dock, destroyed) = ship_lists(&ships);
        assert_eq!(can_dock, [class_guid_wire(AURORA).unwrap()]);
        assert_eq!(destroyed, [LOST]);
    }

    /// The reply for a request (GUID 641f74e6a6062f4e8fcb4f89014639cc): the GUID echoed, three empty counts
    /// and the trailer. A golden of the bytes (`SQueryServerInfos`).
    #[test]
    fn the_empty_reply_is_golden() {
        let request = bytes("641f74e6a6062f4e8fcb4f89014639cc");
        let reply = empty_server_infos(&request.try_into().unwrap());
        assert_eq!(reply, bytes("641f74e6a6062f4e8fcb4f89014639cc00000020"));
    }

    #[test]
    fn the_reply_goes_to_the_official_method_unless_the_environment_says_otherwise() {
        assert_eq!(response_method(None), Some(INSURANCE_RESPONSE_METHOD));
        assert_eq!(response_method(Some("")), Some(INSURANCE_RESPONSE_METHOD));
        assert_eq!(response_method(Some("257")), Some(257));
        assert_eq!(response_method(Some(" 191 ")), Some(191));
        assert_eq!(response_method(Some("0")), None);
        assert_eq!(response_method(Some("OFF")), None);
        assert_eq!(
            response_method(Some("nonsense")),
            Some(INSURANCE_RESPONSE_METHOD)
        );
    }
}

//! The hangar's ship platform on the wire: the loading platform manager and the door that is the platform.
//!
//! The manager (4.10.2) has two replicated
//! variables, `currentPlatformState` (key 0xdf5419be) and `plannedEndPlatformState` (key 0xe90238ce), of the enum
//! `ELoadingPlatform_State` (type hash 0xc8ab346b, one signed varint). Its authority logic: `RequestOpen` needs
//! `ClosedIdle` and sets current 2, planned 3; `RequestClose` sets current 6, planned 7. It follows the platform entity's
//! door: `Door_OpenState` (key 0x7895447b, type 0x2cf99cab) `Opened` (3) makes it `OpenIdle`, `Closed` (0) `ClosedIdle`;
//! `Opening` (5) and `Closing` (2) are ignored. So the DGS drives the door and writes the manager's variables.
//!
//! The entity ids are the hangar shell's first static id plus an index that depends on the hangar's size (the table
//! `LAYOUTS`; Orison's small front hangar has its shell at the hangar's id + 10, so manager + 80 and platform + 84).
//!
//! Unknown, and kept here, each behind a name so a game test can settle it:
//! - the absolute variable indices for an `EntitySnapshotUpdate` (`Indices`; none known, so
//!   `SC_DGS_PLATFORM_INDICES=current,planned,door` sets them for a test, and without them the updates are logged and not
//!   sent);
//! - whether the manager runs authority on a replicated entity, or the client computes it from the door (`ANNOUNCE_MANAGER`
//!   sends the manager's variables as well as the door's);
//! - the platform door's own class and whether an update reaches a client-made static (it does not: `Snapshot Update For
//!   Missing Entity`, see `atc.rs`), which is what the game test shows with the log below.

use sc_api::internal::{session_update::Update, EntityVariable, EntityVariables};
use std::time::Duration;

use tracing::{info, warn};

use crate::{
    entity::Snapshot,
    location::HomeHangar,
    outbox::{Outbox, SessionKey},
};

/// CRC-32 of `currentplatformstate` and `plannedendplatformstate`.
pub const CURRENT_STATE_KEY: u32 = 0xdf54_19be;
pub const PLANNED_STATE_KEY: u32 = 0xe902_38ce;
/// `ELoadingPlatform_State`, FNV-1a 64 folded to 32 bits.
pub const STATE_TYPE: u32 = 0xc8ab_346b;
/// `Door_OpenState` and `EDoorOpenState`, as `transit::wire` has them.
pub const DOOR_STATE_KEY: u32 = 0x7895_447b;
pub const DOOR_STATE_TYPE: u32 = 0x2cf9_9cab;
/// `autoSerializeState` (CRC-32 of the lowercased name), a bool: the client ignores a replicated `Door_OpenState` unless it is
/// true (`OnReplicatedOpenState`), and nothing on a client sets it.
pub const AUTO_SERIALIZE_KEY: u32 = 0xba05_30e2;
/// The serializer type of the bool, as the ship's gear bools carry it.
pub const AUTO_SERIALIZE_TYPE: u32 = 0x0b9f_c026;
const AUTO_SERIALIZE_ON: u8 = 1;

/// `ELoadingPlatform_State`.
pub mod state {
    pub const OPENING_LOADING_GATE: u8 = 1;
    pub const RAISING_PLATFORM: u8 = 2;
    pub const OPEN_IDLE: u8 = 3;
    pub const CLOSING_FRONT_GATE: u8 = 4;
    pub const LOWERING_PLATFORM: u8 = 6;
    pub const CLOSED_IDLE: u8 = 7;
}

/// `EDoorOpenState`.
pub mod door {
    pub const CLOSED: u8 = 0;
    pub const CLOSING: u8 = 2;
    pub const OPENED: u8 = 3;
    pub const OPENING: u8 = 5;
}

/// `EntityClassDefinition.LoadingPlatformManager` (0x199fe33b is the class's own CRC in the replication; this is the CRC the
/// prefab table carries for the entity), `LoadingPlatform` and `LoadingPlatformLoadingGate`: the three statics of a hangar
/// shell that the platform's state needs to reach (the table `transit_prefab_entities.tsv`).
pub const MANAGER_CLASS: u32 = 429_908_795;
pub const PLATFORM_CLASS: u32 = 40_450_539;
pub const LOADING_GATE_CLASS: u32 = 3_474_110_199;
/// The flags of the manager as the official service binds it: a static with no geometry (`location::MERGED_STATIC_FLAGS` gives the geometry statics' `0x80300840`).
pub const MANAGER_FLAGS: u32 = 0x0010_0840;
/// `SC_DGS_PLATFORM_BIND=off` keeps the platform's statics client-made (the manager, the platform and the loading gate are
/// then local entities and the DGS's updates to them are rejected, `Snapshot Update For Missing Entity`).
pub const PLATFORM_BIND_SETTING: &str = "SC_DGS_PLATFORM_BIND";
pub const PLATFORM_BIND_OFF: [&str; 3] = ["off", "0", "false"];

/// Whether the platform's statics are bound as replicated entities merged into the container (the default).
pub fn bind_enabled() -> bool {
    std::env::var(PLATFORM_BIND_SETTING)
        .map_or(true, |value| !PLATFORM_BIND_OFF.contains(&value.to_ascii_lowercase().as_str()))
}

/// Whether `class_crc` is one of the platform's statics.
pub fn is_platform_static(class_crc: u32) -> bool {
    [MANAGER_CLASS, PLATFORM_CLASS, LOADING_GATE_CLASS].contains(&class_crc)
}

/// `SC_DGS_PLATFORM_GATE_OPEN`: unless `off`/`0`/`false`, the loading gate is bound `Opened`. Its `Door_OpenState` has no verified
/// index, so the gate cannot be opened with an update; a gate bound closed collides with the ship that is spawned on the
/// lowered pad (the platform's own physics entity moves with its
/// trackview but the gate's does not).
pub const GATE_OPEN_SETTING: &str = "SC_DGS_PLATFORM_GATE_OPEN";

fn gate_bound_open() -> bool {
    #[cfg(test)]
    return true;
    #[cfg(not(test))]
    !std::env::var(GATE_OPEN_SETTING)
        .is_ok_and(|value| ["off", "0", "false"].contains(&value.to_ascii_lowercase().as_str()))
}

/// The variables a platform static is bound with: a platform that rests up and empty (`OpenIdle`, door `Opened`), the
/// state the DGS holds until a retrieval lowers it. Bound by key, so no index is needed.
pub fn resting_snapshots(class_crc: u32) -> Vec<Snapshot> {
    let variable = |key, type_id, value: u8| Snapshot {
        key,
        type_id,
        value: vec![value],
    };
    match class_crc {
        MANAGER_CLASS => vec![
            variable(CURRENT_STATE_KEY, STATE_TYPE, state::OPEN_IDLE),
            variable(PLANNED_STATE_KEY, STATE_TYPE, state::OPEN_IDLE),
        ],
        LOADING_GATE_CLASS if gate_bound_open() => vec![
            variable(AUTO_SERIALIZE_KEY, AUTO_SERIALIZE_TYPE, AUTO_SERIALIZE_ON),
            Snapshot { key: STAMP_KEY, type_id: STAMP_TYPE, value: stamp_settled() },
            variable(DOOR_STATE_KEY, DOOR_STATE_TYPE, door::OPENED),
        ],
        PLATFORM_CLASS => vec![
            variable(AUTO_SERIALIZE_KEY, AUTO_SERIALIZE_TYPE, AUTO_SERIALIZE_ON),
            Snapshot { key: STAMP_KEY, type_id: STAMP_TYPE, value: stamp_settled() },
            variable(DOOR_STATE_KEY, DOOR_STATE_TYPE, door::OPENED),
        ],
        LOADING_GATE_CLASS => vec![
            variable(AUTO_SERIALIZE_KEY, AUTO_SERIALIZE_TYPE, AUTO_SERIALIZE_ON),
            Snapshot { key: STAMP_KEY, type_id: STAMP_TYPE, value: stamp_settled() },
            variable(DOOR_STATE_KEY, DOOR_STATE_TYPE, door::CLOSED),
        ],
        _ => Vec::new(),
    }
}

/// Whether the manager's variables are sent as well as the door's. On a replicated manager with no authority on the
/// client only the door matters; where the client runs the manager as authority these are what it reads.
pub const ANNOUNCE_MANAGER: bool = true;

/// The shapes of the hangars: how many statics the shell has (to find it among a hangar's containers) and the indices,
/// among its statics, of the loading platform manager and of the platform. Taken from the `.soc` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellLayout {
    pub name: &'static str,
    pub statics: u64,
    pub manager: u64,
    pub platform: u64,
    /// The loading gate, a second door (`SC_DGS_PLATFORM_GATE`).
    pub gate: u64,
}

pub const LAYOUTS: [ShellLayout; 7] = [
    ShellLayout {
        name: "smlfrnt",
        statics: 209,
        manager: 70,
        platform: 74,
        gate: 72,
    },
    ShellLayout {
        name: "smltop",
        statics: 214,
        manager: 58,
        platform: 57,
        gate: 60,
    },
    ShellLayout {
        name: "medfrnt",
        statics: 239,
        manager: 61,
        platform: 64,
        gate: 69,
    },
    ShellLayout {
        name: "medtop",
        statics: 285,
        manager: 66,
        platform: 69,
        gate: 73,
    },
    ShellLayout {
        name: "lrgfront",
        statics: 303,
        manager: 63,
        platform: 68,
        gate: 62,
    },
    ShellLayout {
        name: "lrgtop",
        statics: 378,
        manager: 71,
        platform: 70,
        gate: 69,
    },
    ShellLayout {
        name: "xltop",
        statics: 615,
        manager: 106,
        platform: 100,
        gate: 95,
    },
];

/// The layout of a hangar class name (`Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`).
pub fn layout_of_class(class_name: &str) -> Option<&'static ShellLayout> {
    let kind = class_name.strip_prefix("Hangar_")?;
    let name = [
        ("SmallFront", "smlfrnt"),
        ("SmallTop", "smltop"),
        ("MediumFront", "medfrnt"),
        ("MediumTop", "medtop"),
        ("LargeFront", "lrgfront"),
        ("LargeTop", "lrgtop"),
        ("XLTop", "xltop"),
    ]
    .into_iter()
    .find(|(prefix, _)| kind.starts_with(prefix))?
    .1;
    LAYOUTS.iter().find(|layout| layout.name == name)
}

/// The two entities of a hangar's platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlatformIds {
    pub manager: u64,
    pub platform: u64,
}

impl PlatformIds {
    /// From the id of the shell's first static.
    pub fn from_shell(layout: &ShellLayout, first_static: u64) -> Self {
        Self {
            manager: first_static + layout.manager,
            platform: first_static + layout.platform,
        }
    }

    /// The ids of a home hangar's platform: its shell is the container whose statics are as many as its layout says.
    /// `None` for a hangar of no known layout, or whose content has no such shell.
    pub fn for_hangar(hangar: &HomeHangar) -> Option<Self> {
        let layout = layout_of_class(&hangar.location.hangar_class)?;
        let ids = hangar.ids();
        let (_, shell) = ids
            .containers
            .iter()
            .find(|(_, content)| content.statics.1 + 1 - content.statics.0 == layout.statics)?;
        Some(Self::from_shell(layout, shell.statics.0))
    }
}

/// The manager's `currentPlatformState` and `plannedEndPlatformState` slots (0-based, the wire has index+1): the class `LoadingPlatformManager` has exactly three
/// variables sorted by key (`retrievingItems` 0x7a45f0ab, `currentPlatformState` 0xdf5419be, `plannedEndPlatformState` 0xe90238ce).
/// The doors' slots: the pad (`LoadingPlatform`, 111
/// variables) `Door_OpenState` 39 and `autoSerializeState` 69, the loading gate (129) 48 and 80.
pub const MANAGER_CURRENT_INDEX: Option<u32> = Some(1);
/// `retrievingItems` (key 0x7a45f0ab, `ERequestClose_Context`, type hash 0x90bd8a97) is slot 0 of the manager.
pub const MANAGER_RETRIEVING_INDEX: Option<u32> = Some(0);
pub const RETRIEVING_KEY: u32 = 0x7a45_f0ab;
pub const RETRIEVING_TYPE: u32 = 0x90bd_8a97;
/// The values of `retrievingItems` the platform sets: a close that takes the ship's place sets `RETRIEVING`, which makes the
/// client stop the close sequence (the lights) as the manager arrives in `ClosedIdle`; an open sets it back to `IDLE`.
pub const RETRIEVING_IDLE: u8 = 0;
pub const RETRIEVING: u8 = 1;
pub const MANAGER_PLANNED_INDEX: Option<u32> = Some(2);
pub const PAD_DOOR_INDEX: Option<u32> = Some(39);
pub const PAD_AUTO_SERIALIZE_INDEX: Option<u32> = Some(69);
pub const GATE_DOOR_INDEX: Option<u32> = Some(48);
pub const GATE_AUTO_SERIALIZE_INDEX: Option<u32> = Some(80);
/// `Door_OpenState_timestamp` (key 0xdd7a4f9e, `CTimeValue`, flags 3): pad 85, loading gate 99 (0-based; wire +1). The
/// client fast-forwards a door's animation by the network time minus this, so a door that gets none jumps to its end.
pub const PAD_STAMP_INDEX: Option<u32> = Some(85);
pub const GATE_STAMP_INDEX: Option<u32> = Some(99);
pub const STAMP_KEY: u32 = 0xdd7a_4f9e;
/// The type hash of `CTimeValue`.
pub const STAMP_TYPE: u32 = 0xfee2_9f0d;
/// `CTimeValue` counts 100 ns ticks.
const TICKS_PER_SECOND: u128 = 10_000_000;

/// The stamp of a step: a movement starts now, the end of one is settled.
fn step_stamp(step: Step) -> Vec<u8> {
    match step {
        Step::Open | Step::OpenGate | Step::Raise | Step::Close | Step::CloseToRest | Step::CloseGate => stamp_now(),
        Step::Opened | Step::Closed => stamp_settled(),
    }
}

/// The server's time now as a `CTimeValue`: 100 ns ticks since the Unix epoch, which is the base the client's network time
/// turned out to count in (see `transit::wire`).
pub fn stamp_now() -> Vec<u8> {
    stamp_ago(Duration::ZERO)
}

/// How long ago a door that has settled changed: far more than any animation lasts, so the client shows its end.
const SETTLED_AGE: Duration = Duration::from_secs(3600);

/// The stamp of a door at rest (bound open or closed, or a step that ends a movement): the animation is at its end.
pub fn stamp_settled() -> Vec<u8> {
    PINNED_SETTLED.get().cloned().unwrap_or_else(|| stamp_ago(SETTLED_AGE))
}

static PINNED_SETTLED: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Makes every settled stamp `ticks` for the life of the process, so that tests of the bytes a bind carries do not change
/// with the clock. Nothing but tests calls it.
pub fn pin_settled_stamp(ticks: i64) {
    let _ = PINNED_SETTLED.set(ticks.to_le_bytes().to_vec());
}

fn stamp_ago(age: Duration) -> Vec<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(age);
    ((now.as_nanos() * TICKS_PER_SECOND / 1_000_000_000) as i64).to_le_bytes().to_vec()
}
/// `SC_DGS_PLATFORM_DOOR=off` sends the platform's door variables no more (the manager's still go).
pub const DOOR_SETTING: &str = "SC_DGS_PLATFORM_DOOR";

/// The absolute indices of the variables, which `EntitySnapshotUpdate` needs: not read yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Indices {
    pub current: Option<u32>,
    pub planned: Option<u32>,
    pub door: Option<u32>,
    /// The loading gate's `Door_OpenState` (its own class, so its own index).
    pub gate: Option<u32>,
    /// `autoSerializeState` of the pad and of the gate.
    pub door_auto: Option<u32>,
    pub gate_auto: Option<u32>,
    /// `Door_OpenState_timestamp` of the pad and of the gate.
    pub door_stamp: Option<u32>,
    pub gate_stamp: Option<u32>,
    /// The manager's `retrievingItems`.
    pub retrieving: Option<u32>,
}

impl Indices {
    /// `SC_DGS_PLATFORM_INDICES=current,planned,door[,gate[,door_auto[,gate_auto[,door_stamp[,gate_stamp]]]]]`, each a number or empty, for a game test.
    pub fn from_environment() -> Self {
        let mut indices = Self::parse(std::env::var("SC_DGS_PLATFORM_INDICES").ok().as_deref());
        if std::env::var(DOOR_SETTING)
            .is_ok_and(|value| ["off", "0", "false"].contains(&value.to_ascii_lowercase().as_str()))
        {
            indices.door = None;
            indices.gate = None;
        }
        indices
    }

    fn parse(setting: Option<&str>) -> Self {
        let mut parts = setting
            .unwrap_or_default()
            .split(',')
            .map(|part| part.trim().parse().ok());
        Self {
            current: parts.next().flatten().or(MANAGER_CURRENT_INDEX),
            planned: parts.next().flatten().or(MANAGER_PLANNED_INDEX),
            door: parts.next().flatten().or(PAD_DOOR_INDEX),
            gate: parts.next().flatten().or(GATE_DOOR_INDEX),
            door_auto: parts.next().flatten().or(PAD_AUTO_SERIALIZE_INDEX),
            gate_auto: parts.next().flatten().or(GATE_AUTO_SERIALIZE_INDEX),
            door_stamp: parts.next().flatten().or(PAD_STAMP_INDEX),
            gate_stamp: parts.next().flatten().or(GATE_STAMP_INDEX),
            retrieving: parts.next().flatten().or(MANAGER_RETRIEVING_INDEX),
        }
    }
}

/// What the platform does, in the order it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `RequestOpen`: the loading gate opens and the platform starts to rise (manager `RaisingPlatform` 2).
    Open,
    /// `RequestOpen` of a hangar whose loading gate is driven: the gate opens first (manager `OpeningLoadingGate` 1), and the
    /// pad stays down until `Raise`.
    OpenGate,
    /// The gate is fully open: the pad's door opens and the platform rises (manager `RaisingPlatform` 2).
    Raise,
    /// The platform is fully down: the gate's door closes (the pad's door closed with `Close`).
    CloseGate,
    /// The rising is done: `OpenIdle`, the ship stands at the hangar's floor (the door is `Opened`).
    Opened,
    /// `RequestClose`: the platform starts to lower and the gate closes (manager `LoweringPlatform` 6).
    Close,
    /// `RequestClose` of a ship that is stored: nothing comes up after it, so `retrievingItems` stays idle and the client sets
    /// the idle-closed lights as the manager arrives in `ClosedIdle`.
    CloseToRest,
    /// The lowering is done: `ClosedIdle`, the platform is down (the door is `Closed`).
    Closed,
}

/// One variable to change, with the index an update needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub entity_id: u64,
    pub name: &'static str,
    pub key: u32,
    pub type_id: u32,
    pub value: Vec<u8>,
    pub index: Option<u32>,
}

impl Change {
    /// As the bind would carry it, for the log and for a rebind.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            key: self.key,
            type_id: self.type_id,
            value: self.value.clone(),
        }
    }

    /// The update, `None` while the variable's index is not known.
    pub fn update(&self) -> Option<EntityVariables> {
        let index = self.index?;
        Some(EntityVariables {
            entity_id: self.entity_id,
            variables: vec![EntityVariable {
                index,
                value: self.value.clone(),
            }],
        })
    }
}

/// The changes of the loading gate for a step, the gate door following the platform's own: `[]` unless `gate` is set. The gate
/// is an door like the platform; whether the client's manager drives it on its own, or the server must, is not
/// known, so `SC_DGS_PLATFORM_GATE` (`gate_enabled`) is off by default.
pub fn gate_changes(gate: Option<u64>, indices: &Indices, step: Step) -> Vec<Change> {
    let Some(gate) = gate else {
        return Vec::new();
    };
    let value = match step {
        Step::Open | Step::OpenGate => door::OPENING,
        Step::Opened | Step::Raise => door::OPENED,
        Step::Close | Step::CloseToRest | Step::CloseGate => door::CLOSING,
        Step::Closed => door::CLOSED,
    };
    vec![
        Change {
            entity_id: gate,
            name: "autoSerializeState (gate)",
            key: AUTO_SERIALIZE_KEY,
            type_id: AUTO_SERIALIZE_TYPE,
            value: vec![AUTO_SERIALIZE_ON],
            index: indices.gate.and(indices.gate_auto),
        },
        Change {
            entity_id: gate,
            name: "Door_OpenState_timestamp (gate)",
            key: STAMP_KEY,
            type_id: STAMP_TYPE,
            value: step_stamp(step),
            index: indices.gate.and(indices.gate_stamp),
        },
        Change {
            entity_id: gate,
            name: "Door_OpenState (gate)",
            key: DOOR_STATE_KEY,
            type_id: DOOR_STATE_TYPE,
            value: vec![value],
            index: indices.gate,
        },
    ]
}

/// Whether the loading gate's door follows the platform's: unless `SC_DGS_PLATFORM_GATE` is `off`, `0`, `false` or `no`.
pub fn gate_enabled() -> bool {
    !std::env::var("SC_DGS_PLATFORM_GATE")
        .is_ok_and(|setting| ["0", "off", "false", "no"].contains(&setting.to_ascii_lowercase().as_str()))
}

/// `SC_DGS_PLATFORM_GATE_STATES=off|0|false|no` sends `RaisingPlatform` and `LoweringPlatform` for the two movements, as before.
/// By default the movements go through `OpeningLoadingGate` and `ClosingFrontGate`: the client plays the platform's
/// light and hologram sequences (open / close) on those two states, and on `RaisingPlatform` / `LoweringPlatform` only when the
/// hangar lacks the loading gate / the front gate.
pub const GATE_STATES_SETTING: &str = "SC_DGS_PLATFORM_GATE_STATES";

fn gate_states() -> bool {
    !std::env::var(GATE_STATES_SETTING)
        .is_ok_and(|setting| ["0", "off", "false", "no"].contains(&setting.to_ascii_lowercase().as_str()))
}

/// `SC_DGS_PLATFORM_RETRIEVING=off|0|false|no` leaves `retrievingItems` alone (the close lights then run their whole sequence).
pub const RETRIEVING_SETTING: &str = "SC_DGS_PLATFORM_RETRIEVING";

fn retrieving_enabled() -> bool {
    !std::env::var(RETRIEVING_SETTING)
        .is_ok_and(|setting| ["0", "off", "false", "no"].contains(&setting.to_ascii_lowercase().as_str()))
}

/// The changes of a step, in the order they are sent: the door, then the manager.
pub fn changes(ids: &PlatformIds, indices: &Indices, step: Step) -> Vec<Change> {
    let (door_value, current, planned) = match step {
        // The gate opens alone: the pad's door waits for `Raise`.
        Step::OpenGate => (None, state::OPENING_LOADING_GATE, Some(state::OPEN_IDLE)),
        Step::Raise => (Some(door::OPENING), state::RAISING_PLATFORM, None),
        Step::Open => (
            Some(door::OPENING),
            if gate_states() { state::OPENING_LOADING_GATE } else { state::RAISING_PLATFORM },
            Some(state::OPEN_IDLE),
        ),
        Step::Opened => (Some(door::OPENED), state::OPEN_IDLE, None),
        Step::Close | Step::CloseToRest => (
            Some(door::CLOSING),
            if gate_states() { state::CLOSING_FRONT_GATE } else { state::LOWERING_PLATFORM },
            Some(state::CLOSED_IDLE),
        ),
        Step::Closed => (Some(door::CLOSED), state::CLOSED_IDLE, None),
        // Only the gate's door moves: the manager stays in the state it is in.
        Step::CloseGate => return Vec::new(),
    };
    // `autoSerializeState` first: the client ignores the door's state unless it is true.
    let mut changes = Vec::new();
    if let Some(door_value) = door_value {
        changes.extend([
        Change {
            entity_id: ids.platform,
            name: "autoSerializeState",
            key: AUTO_SERIALIZE_KEY,
            type_id: AUTO_SERIALIZE_TYPE,
            value: vec![AUTO_SERIALIZE_ON],
            index: indices.door.and(indices.door_auto),
        },
        // Before the state: the client fast-forwards the door's animation by the time since this stamp.
        Change {
            entity_id: ids.platform,
            name: "Door_OpenState_timestamp",
            key: STAMP_KEY,
            type_id: STAMP_TYPE,
            value: step_stamp(step),
            index: indices.door.and(indices.door_stamp),
        },
        Change {
            entity_id: ids.platform,
            name: "Door_OpenState",
            key: DOOR_STATE_KEY,
            type_id: DOOR_STATE_TYPE,
            value: vec![door_value],
            index: indices.door,
        },
        ]);
    }
    if ANNOUNCE_MANAGER {
        // Before the state: the client reads it as it handles the state change.
        let retrieving = match step {
            Step::Close => Some(RETRIEVING),
            Step::CloseToRest => Some(RETRIEVING_IDLE),
            Step::Open | Step::OpenGate => Some(RETRIEVING_IDLE),
            Step::Opened | Step::Closed | Step::Raise | Step::CloseGate => None,
        };
        if let Some(value) = retrieving.filter(|_| retrieving_enabled()) {
            changes.push(Change {
                entity_id: ids.manager,
                name: "retrievingItems",
                key: RETRIEVING_KEY,
                type_id: RETRIEVING_TYPE,
                value: vec![value],
                index: indices.retrieving,
            });
        }
        changes.push(Change {
            entity_id: ids.manager,
            name: "currentPlatformState",
            key: CURRENT_STATE_KEY,
            type_id: STATE_TYPE,
            value: vec![current],
            index: indices.current,
        });
        if let Some(planned) = planned {
            changes.push(Change {
                entity_id: ids.manager,
                name: "plannedEndPlatformState",
                key: PLANNED_STATE_KEY,
                type_id: STATE_TYPE,
                value: vec![planned],
                index: indices.planned,
            });
        }
    }
    changes
}

/// How long after a closing the client's door accepts an opening: it holds its local state for half a second after each
/// change, and drops the opposite request meanwhile.
pub const OPENING_DELAY: Duration = Duration::from_millis(700);

/// Sends a hangar's platform changes to its client through the outbox.
#[derive(Clone)]
pub struct PlatformLink {
    outbox: Outbox,
    session: SessionKey,
    ids: PlatformIds,
    indices: Indices,
    /// The loading gate's entity, when its door follows the platform's.
    gate: Option<u64>,
    /// How long the gate's door takes to open: the pad's door and the platform wait for it.
    gate_open: Duration,
    /// How long the pad's door takes to close: the gate's door waits for it.
    pad_close: Duration,
}

impl PlatformLink {
    pub fn new(outbox: Outbox, session: SessionKey, ids: PlatformIds) -> Self {
        Self {
            outbox,
            session,
            ids,
            indices: Indices::from_environment(),
            gate: None,
            gate_open: Duration::ZERO,
            pad_close: Duration::ZERO,
        }
    }

    /// The loading gate whose door is driven with the platform's. It opens first; the pad's door and the platform follow
    /// when `gate_open` has passed, so the platform only rises once the gate is fully open.
    pub fn with_gate(mut self, gate: u64, gate_open: Duration, pad_close: Duration) -> Self {
        self.gate = Some(gate);
        self.gate_open = gate_open;
        self.pad_close = pad_close;
        self
    }

    /// Whether the doors move one after the other: the gate first to open, the pad's door first to close.
    fn sequenced(&self) -> bool {
        self.gate.is_some() && !cfg!(test) && !self.gate_open.is_zero() && !self.pad_close.is_zero()
    }

    pub fn with_indices(mut self, indices: Indices) -> Self {
        self.indices = indices;
        self
    }

    pub fn ids(&self) -> PlatformIds {
        self.ids
    }

    /// Sends the step's changes and logs each. Returns how many were sent (those whose index is known). An opening is sent
    /// after `OPENING_DELAY` (the count is then 0): the client's door drops an opening that comes right after its own closing.
    pub fn announce(&self, step: Step) -> usize {
        if step == Step::Open && !cfg!(test) {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let link = self.clone();
                runtime.spawn(async move {
                    tokio::time::sleep(OPENING_DELAY).await;
                    if link.gate.is_some() && !link.gate_open.is_zero() {
                        link.send(Step::OpenGate);
                        tokio::time::sleep(link.gate_open).await;
                        link.send(Step::Raise);
                    } else {
                        link.send(step);
                    }
                });
                return 0;
            }
        }
        if matches!(step, Step::Close | Step::CloseToRest) && self.sequenced() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let link = self.clone();
                runtime.spawn(async move {
                    tokio::time::sleep(link.pad_close).await;
                    link.send(Step::CloseGate);
                });
            }
        }
        self.send(step)
    }

    fn send(&self, step: Step) -> usize {
        let mut sent = 0;
        // A close that goes in two steps leaves the gate's door to `CloseGate`.
        let gate = self.gate.filter(|_| !(matches!(step, Step::Close | Step::CloseToRest) && self.sequenced()));
        let all = changes(&self.ids, &self.indices, step)
            .into_iter()
            .chain(gate_changes(gate, &self.indices, step));
        for change in all {
            let value = crate::atc::hex(&change.value);
            match change.update() {
                Some(variables) => {
                    info!(
                        ?step,
                        entity = change.entity_id,
                        variable = change.name,
                        key = format_args!("{:#010x}", change.key),
                        index = change.index,
                        value,
                        "platform variable sent"
                    );
                    self.outbox.send(self.session, Update::Variables(variables));
                    sent += 1;
                }
                None => warn!(
                    ?step,
                    entity = change.entity_id,
                    variable = change.name,
                    key = format_args!("{:#010x}", change.key),
                    type_id = format_args!("{:#010x}", change.type_id),
                    value,
                    "platform variable not sent: its index is not known (SC_DGS_PLATFORM_INDICES)"
                ),
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::Sent;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn crc32(text: &[u8]) -> u32 {
        !text.iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        })
    }

    #[test]
    fn the_variables_are_keyed_by_their_lowercased_names() {
        assert_eq!(crc32(b"currentplatformstate"), CURRENT_STATE_KEY);
        assert_eq!(crc32(b"plannedendplatformstate"), PLANNED_STATE_KEY);
        assert_eq!(crc32(b"door_openstate"), DOOR_STATE_KEY);
    }

    #[test]
    fn the_layout_follows_the_class_and_the_ids_the_shells_first_static() {
        let layout = layout_of_class("Hangar_SmallFront_Orison").unwrap();
        let ids = PlatformIds::from_shell(layout, 1000 + 10);
        assert_eq!((ids.manager, ids.platform), (1000 + 80, 1000 + 84));
        assert_eq!(
            layout_of_class("Hangar_XLTop_Area18").unwrap().platform,
            100
        );
        assert!(layout_of_class("Hangar_XLFront_X").is_none());
        assert!(layout_of_class("Lobby").is_none());
    }

    /// `Stanton2_Orison`'s shell `hangar_smlfrnt_001` starts at the hangar's id + 10 (209 statics).
    #[test]
    fn orisons_hangar_finds_its_manager_and_platform_in_its_shell() {
        let hangar = HomeHangar::new(1_105_811_199, 0, false).expect("Orison's home hangar");
        let ids = PlatformIds::for_hangar(&hangar).expect("a small front hangar");
        assert_eq!(
            (
                ids.manager - hangar.entity_id,
                ids.platform - hangar.entity_id
            ),
            (80, 84)
        );
    }

    #[test]
    fn opening_sends_the_door_opening_then_the_manager_raising_toward_open_idle() {
        let ids = PlatformIds {
            manager: 80,
            platform: 84,
        };
        let indices = Indices {
            current: Some(11),
            planned: Some(12),
            door: Some(13),
            door_auto: Some(14),
            ..Indices::default()
        };
        let open = changes(&ids, &indices, Step::Open);
        // The timestamp goes just before the door's state, and is 8 bytes (a `CTimeValue`).
        let stamp = open.iter().position(|c| c.key == STAMP_KEY).expect("a timestamp");
        assert_eq!((open[stamp].value.len(), open[stamp + 1].key), (8, DOOR_STATE_KEY));
        let open: Vec<Change> = open.into_iter().filter(|c| c.key != STAMP_KEY).collect();
        let shown: Vec<(u64, u32, u32, Vec<u8>, Option<u32>)> = open
            .iter()
            .map(|c| (c.entity_id, c.key, c.type_id, c.value.clone(), c.index))
            .collect();
        assert_eq!(
            shown,
            [
                (84, AUTO_SERIALIZE_KEY, AUTO_SERIALIZE_TYPE, vec![1], Some(14)),
                (84, 0x7895_447b, 0x2cf9_9cab, vec![5], Some(13)),
                (80, RETRIEVING_KEY, RETRIEVING_TYPE, vec![RETRIEVING_IDLE], None),
                (80, 0xdf54_19be, 0xc8ab_346b, vec![1], Some(11)),
                (80, 0xe902_38ce, 0xc8ab_346b, vec![3], Some(12)),
            ]
        );
        let close = changes(&ids, &indices, Step::Close);
        assert_eq!(
            close.iter().filter(|c| c.key != STAMP_KEY).map(|c| c.value[0]).collect::<Vec<_>>(),
            [1, 2, 1, 4, 7]
        );
        // A close that nothing follows leaves `retrievingItems` idle, so the idle-closed lights are set at `ClosedIdle`.
        let rest = changes(&ids, &indices, Step::CloseToRest);
        assert_eq!(
            rest.iter().filter(|c| c.key != STAMP_KEY).map(|c| c.value[0]).collect::<Vec<_>>(),
            [1, 2, RETRIEVING_IDLE, 4, 7]
        );
        assert_eq!(
            changes(&ids, &indices, Step::Opened)
                .iter()
                .filter(|c| c.key != STAMP_KEY)
                .map(|c| c.value[0])
                .collect::<Vec<_>>(),
            [1, 3, 3]
        );
        assert_eq!(
            changes(&ids, &indices, Step::Closed)
                .iter()
                .filter(|c| c.key != STAMP_KEY)
                .map(|c| c.value[0])
                .collect::<Vec<_>>(),
            [1, 0, 7]
        );
    }

    #[test]
    fn the_gate_opens_alone_and_the_platform_rises_in_a_step_of_its_own() {
        let ids = PlatformIds { manager: 80, platform: 84 };
        let indices = Indices { current: Some(11), planned: Some(12), door: Some(13), door_auto: Some(14), ..Indices::default() };
        let gate = changes(&ids, &indices, Step::OpenGate);
        assert!(gate.iter().all(|change| change.entity_id == ids.manager), "the pad's door stays shut");
        assert_eq!(gate.iter().find(|c| c.key == CURRENT_STATE_KEY).unwrap().value, [state::OPENING_LOADING_GATE]);
        let raise = changes(&ids, &indices, Step::Raise);
        assert_eq!(raise.iter().find(|c| c.key == DOOR_STATE_KEY).unwrap().value, [door::OPENING]);
        assert_eq!(raise.iter().find(|c| c.key == CURRENT_STATE_KEY).unwrap().value, [state::RAISING_PLATFORM]);
        let gate_indices = Indices { gate: Some(20), ..indices };
        let opened = gate_changes(Some(90), &gate_indices, Step::Raise);
        assert_eq!(opened.iter().find(|c| c.key == DOOR_STATE_KEY).unwrap().value, [door::OPENED]);
        let opening = gate_changes(Some(90), &gate_indices, Step::OpenGate);
        assert_eq!(opening.iter().find(|c| c.key == DOOR_STATE_KEY).unwrap().value, [door::OPENING]);
    }

    #[test]
    fn the_pad_door_closes_first_and_the_gate_door_follows_in_a_step_of_its_own() {
        let ids = PlatformIds { manager: 80, platform: 84 };
        let indices = Indices { gate: Some(20), door: Some(13), current: Some(11), ..Indices::default() };
        assert!(changes(&ids, &indices, Step::CloseGate).is_empty(), "the manager stays where it is");
        let closing = gate_changes(Some(90), &indices, Step::CloseGate);
        assert_eq!(closing.iter().find(|c| c.key == DOOR_STATE_KEY).unwrap().value, [door::CLOSING]);
    }

    #[test]
    fn a_change_is_an_update_only_with_its_index() {
        let ids = PlatformIds {
            manager: 80,
            platform: 84,
        };
        let unknown = changes(&ids, &Indices::default(), Step::Open);
        assert!(unknown.iter().all(|change| change.update().is_none()));
        let known = changes(
            &ids,
            &Indices {
                door: Some(9),
                ..Indices::default()
            },
            Step::Open,
        );
        assert!(known[0].update().is_none(), "no autoSerializeState index, no update of it");
        let update = known[2].update().unwrap();
        assert_eq!(
            (
                update.entity_id,
                update.variables[0].index,
                update.variables[0].value.clone()
            ),
            (84, 9, vec![5])
        );
        assert_eq!(known[2].snapshot().value, [5]);
        assert_eq!(known[1].snapshot().key, STAMP_KEY);
    }

    #[test]
    fn the_indices_come_from_a_list_with_gaps() {
        assert_eq!(
            Indices::parse(Some("4,5,6")),
            Indices {
                current: Some(4),
                planned: Some(5),
                door: Some(6),
                gate: GATE_DOOR_INDEX,
                door_auto: PAD_AUTO_SERIALIZE_INDEX,
                gate_auto: GATE_AUTO_SERIALIZE_INDEX,
                door_stamp: PAD_STAMP_INDEX,
                gate_stamp: GATE_STAMP_INDEX,
                retrieving: MANAGER_RETRIEVING_INDEX,
            }
        );
        assert_eq!(
            Indices::parse(Some(",5")),
            Indices {
                current: MANAGER_CURRENT_INDEX,
                planned: Some(5),
                door: PAD_DOOR_INDEX,
                gate: GATE_DOOR_INDEX,
                door_auto: PAD_AUTO_SERIALIZE_INDEX,
                gate_auto: GATE_AUTO_SERIALIZE_INDEX,
                door_stamp: PAD_STAMP_INDEX,
                gate_stamp: GATE_STAMP_INDEX,
                retrieving: MANAGER_RETRIEVING_INDEX,
            }
        );
        // The manager's two slots are verified, so they are the defaults; the doors' are not.
        assert_eq!(
            Indices::parse(None),
            Indices {
                current: Some(1),
                planned: Some(2),
                door: Some(39),
                gate: Some(48),
                door_auto: Some(69),
                gate_auto: Some(80),
                door_stamp: Some(85),
                gate_stamp: Some(99),
                retrieving: Some(0),
            }
        );
    }

    #[test]
    fn the_link_sends_to_the_session_and_counts_what_it_sent() {
        let outbox = Outbox::default();
        let mut receiver: UnboundedReceiver<Sent> = outbox.subscribe((1, 2));
        let link = PlatformLink::new(
            outbox,
            (1, 2),
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
        assert_eq!(link.announce(Step::Open), 3);
        let Some(Update::Variables(first)) = receiver.try_recv().unwrap().unwrap().update else {
            panic!()
        };
        assert_eq!(
            (
                first.entity_id,
                first.variables[0].index,
                first.variables[0].value.clone()
            ),
            (84, 3, vec![5])
        );
        let unindexed = PlatformLink::new(
            Outbox::default(),
            (1, 2),
            PlatformIds {
                manager: 80,
                platform: 84,
            },
        )
        .with_indices(Indices::default());
        assert_eq!(unindexed.announce(Step::Open), 0);
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn the_gate_door_follows_the_platforms_steps_and_is_not_sent_without_its_own_index() {
        let indices = Indices {
            door: Some(1),
            gate: Some(2),
            ..Indices::default()
        };
        let values: Vec<u8> = [Step::Open, Step::Opened, Step::Close, Step::Closed]
            .into_iter()
            .map(|step| gate_changes(Some(72), &indices, step)[2].value[0])
            .collect();
        assert_eq!(values, [5, 3, 2, 0], "as the platform's own door");
        assert!(gate_changes(None, &indices, Step::Open).is_empty());
        let unknown = gate_changes(Some(72), &Indices::default(), Step::Open);
        assert!(unknown.iter().all(|change| change.update().is_none()), "no index, no update");
        assert_eq!(unknown[2].key, DOOR_STATE_KEY);
    }

    #[test]
    fn each_layout_names_a_gate_apart_from_its_manager_and_platform() {
        for layout in LAYOUTS {
            assert!(layout.gate != layout.manager && layout.gate != layout.platform && layout.gate < layout.statics);
        }
    }
}

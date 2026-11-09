//! The hangar's ship platform on the wire: `CSCLoadingPlatformManager` and the `SCItemDoor` that is the platform.
//!
//! Proven (the RE repository's `netcode/asop_ships.md`, section "4.10.2 re-derivation"): the manager has two replicated
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
//! - the absolute variable indices for an `EntitySnapshotUpdate` (`Indices`; none until read from a running client, so
//!   `SC_DGS_PLATFORM_INDICES=current,planned,door` sets them for a test, and without them the updates are logged and not
//!   sent);
//! - whether the manager runs authority on a replicated entity, or the client computes it from the door (`ANNOUNCE_MANAGER`
//!   sends the manager's variables as well as the door's);
//! - the platform door's own class and whether an update reaches a client-made static (it does not: `Snapshot Update For
//!   Missing Entity`, see `atc.rs`), which is what the game test shows with the log below.

use sc_api::internal::{session_update::Update, EntityVariable, EntityVariables};
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

/// `ELoadingPlatform_State`.
pub mod state {
    pub const RAISING_PLATFORM: u8 = 2;
    pub const OPEN_IDLE: u8 = 3;
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

/// Whether the manager's variables are sent as well as the door's. On a replicated manager with no authority on the
/// client only the door matters; where the client runs the manager as authority these are what it reads.
pub const ANNOUNCE_MANAGER: bool = true;

/// The shapes of the hangars: how many statics the shell has (to find it among a hangar's containers) and the indices,
/// among its statics, of the loading platform manager and of the platform. Proven from the `.soc` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellLayout {
    pub name: &'static str,
    pub statics: u64,
    pub manager: u64,
    pub platform: u64,
}

pub const LAYOUTS: [ShellLayout; 7] = [
    ShellLayout { name: "smlfrnt", statics: 209, manager: 70, platform: 74 },
    ShellLayout { name: "smltop", statics: 214, manager: 58, platform: 57 },
    ShellLayout { name: "medfrnt", statics: 239, manager: 61, platform: 64 },
    ShellLayout { name: "medtop", statics: 285, manager: 66, platform: 69 },
    ShellLayout { name: "lrgfront", statics: 303, manager: 63, platform: 68 },
    ShellLayout { name: "lrgtop", statics: 378, manager: 71, platform: 70 },
    ShellLayout { name: "xltop", statics: 615, manager: 106, platform: 100 },
];

/// The layout of a hangar class name (`Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`).
pub fn layout_of_class(class_name: &str) -> Option<&'static ShellLayout> {
    let kind = class_name.strip_prefix("Hangar_")?;
    let name = [
        ("SmallFront", "smlfrnt"), ("SmallTop", "smltop"), ("MediumFront", "medfrnt"), ("MediumTop", "medtop"),
        ("LargeFront", "lrgfront"), ("LargeTop", "lrgtop"), ("XLTop", "xltop"),
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
        Self { manager: first_static + layout.manager, platform: first_static + layout.platform }
    }

    /// The ids of a home hangar's platform: its shell is the container whose statics are as many as its layout says.
    /// `None` for a hangar of no known layout, or whose content has no such shell.
    pub fn for_hangar(hangar: &HomeHangar) -> Option<Self> {
        let layout = layout_of_class(&hangar.location.hangar_class)?;
        let ids = hangar.ids();
        let (_, shell) = ids.containers.iter().find(|(_, content)| content.statics.1 + 1 - content.statics.0 == layout.statics)?;
        Some(Self::from_shell(layout, shell.statics.0))
    }
}

/// The absolute indices of the variables, which `EntitySnapshotUpdate` needs: not read yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Indices {
    pub current: Option<u32>,
    pub planned: Option<u32>,
    pub door: Option<u32>,
}

impl Indices {
    /// `SC_DGS_PLATFORM_INDICES=current,planned,door`, each a number or empty, for a game test.
    pub fn from_environment() -> Self {
        Self::parse(std::env::var("SC_DGS_PLATFORM_INDICES").ok().as_deref())
    }

    fn parse(setting: Option<&str>) -> Self {
        let mut parts = setting.unwrap_or_default().split(',').map(|part| part.trim().parse().ok());
        Self { current: parts.next().flatten(), planned: parts.next().flatten(), door: parts.next().flatten() }
    }
}

/// What the platform does, in the order it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// `RequestOpen`: the platform starts to lower.
    Open,
    /// The lowering is done (the door is `Opened`).
    Opened,
    /// `RequestClose`: the platform starts to rise.
    Close,
    /// The rising is done (the door is `Closed`).
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
        Snapshot { key: self.key, type_id: self.type_id, value: self.value.clone() }
    }

    /// The update, `None` while the variable's index is not known.
    pub fn update(&self) -> Option<EntityVariables> {
        let index = self.index?;
        Some(EntityVariables { entity_id: self.entity_id, variables: vec![EntityVariable { index, value: self.value.clone() }] })
    }
}

/// The changes of a step, in the order they are sent: the door, then the manager.
pub fn changes(ids: &PlatformIds, indices: &Indices, step: Step) -> Vec<Change> {
    let (door_value, current, planned) = match step {
        Step::Open => (door::OPENING, state::RAISING_PLATFORM, Some(state::OPEN_IDLE)),
        Step::Opened => (door::OPENED, state::OPEN_IDLE, None),
        Step::Close => (door::CLOSING, state::LOWERING_PLATFORM, Some(state::CLOSED_IDLE)),
        Step::Closed => (door::CLOSED, state::CLOSED_IDLE, None),
    };
    let mut changes = vec![Change { entity_id: ids.platform, name: "Door_OpenState", key: DOOR_STATE_KEY, type_id: DOOR_STATE_TYPE, value: vec![door_value], index: indices.door }];
    if ANNOUNCE_MANAGER {
        changes.push(Change { entity_id: ids.manager, name: "currentPlatformState", key: CURRENT_STATE_KEY, type_id: STATE_TYPE, value: vec![current], index: indices.current });
        if let Some(planned) = planned {
            changes.push(Change { entity_id: ids.manager, name: "plannedEndPlatformState", key: PLANNED_STATE_KEY, type_id: STATE_TYPE, value: vec![planned], index: indices.planned });
        }
    }
    changes
}

/// Sends a hangar's platform changes to its client through the outbox.
#[derive(Clone)]
pub struct PlatformLink {
    outbox: Outbox,
    session: SessionKey,
    ids: PlatformIds,
    indices: Indices,
}

impl PlatformLink {
    pub fn new(outbox: Outbox, session: SessionKey, ids: PlatformIds) -> Self {
        Self { outbox, session, ids, indices: Indices::from_environment() }
    }

    pub fn with_indices(mut self, indices: Indices) -> Self {
        self.indices = indices;
        self
    }

    pub fn ids(&self) -> PlatformIds {
        self.ids
    }

    /// Sends the step's changes and logs each. Returns how many were sent (those whose index is known).
    pub fn announce(&self, step: Step) -> usize {
        let mut sent = 0;
        for change in changes(&self.ids, &self.indices, step) {
            let value = crate::atc::hex(&change.value);
            match change.update() {
                Some(variables) => {
                    info!(?step, entity = change.entity_id, variable = change.name, key = format_args!("{:#010x}", change.key), index = change.index, value, "platform variable sent");
                    self.outbox.send(self.session, Update::Variables(variables));
                    sent += 1;
                }
                None => warn!(?step, entity = change.entity_id, variable = change.name, key = format_args!("{:#010x}", change.key), type_id = format_args!("{:#010x}", change.type_id), value, "platform variable not sent: its index is not known (SC_DGS_PLATFORM_INDICES)"),
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
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
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
        assert_eq!(layout_of_class("Hangar_XLTop_Area18").unwrap().platform, 100);
        assert!(layout_of_class("Hangar_XLFront_X").is_none());
        assert!(layout_of_class("Lobby").is_none());
    }

    /// `Stanton2_Orison`'s shell `hangar_smlfrnt_001` starts at the hangar's id + 10 (209 statics).
    #[test]
    fn orisons_hangar_finds_its_manager_and_platform_in_its_shell() {
        let hangar = HomeHangar::new(1_105_811_199, 0, false).expect("Orison's home hangar");
        let ids = PlatformIds::for_hangar(&hangar).expect("a small front hangar");
        assert_eq!((ids.manager - hangar.entity_id, ids.platform - hangar.entity_id), (80, 84));
    }

    #[test]
    fn opening_sends_the_door_opening_then_the_manager_raising_toward_open_idle() {
        let ids = PlatformIds { manager: 80, platform: 84 };
        let indices = Indices { current: Some(11), planned: Some(12), door: Some(13) };
        let open = changes(&ids, &indices, Step::Open);
        let shown: Vec<(u64, u32, u32, Vec<u8>, Option<u32>)> = open.iter().map(|c| (c.entity_id, c.key, c.type_id, c.value.clone(), c.index)).collect();
        assert_eq!(shown, [
            (84, 0x7895_447b, 0x2cf9_9cab, vec![5], Some(13)),
            (80, 0xdf54_19be, 0xc8ab_346b, vec![2], Some(11)),
            (80, 0xe902_38ce, 0xc8ab_346b, vec![3], Some(12)),
        ]);
        let close = changes(&ids, &indices, Step::Close);
        assert_eq!(close.iter().map(|c| c.value[0]).collect::<Vec<_>>(), [2, 6, 7]);
        assert_eq!(changes(&ids, &indices, Step::Opened).iter().map(|c| c.value[0]).collect::<Vec<_>>(), [3, 3]);
        assert_eq!(changes(&ids, &indices, Step::Closed).iter().map(|c| c.value[0]).collect::<Vec<_>>(), [0, 7]);
    }

    #[test]
    fn a_change_is_an_update_only_with_its_index() {
        let ids = PlatformIds { manager: 80, platform: 84 };
        let unknown = changes(&ids, &Indices::default(), Step::Open);
        assert!(unknown.iter().all(|change| change.update().is_none()));
        let known = changes(&ids, &Indices { door: Some(9), ..Indices::default() }, Step::Open);
        let update = known[0].update().unwrap();
        assert_eq!((update.entity_id, update.variables[0].index, update.variables[0].value.clone()), (84, 9, vec![5]));
        assert_eq!(known[0].snapshot().value, [5]);
    }

    #[test]
    fn the_indices_come_from_a_list_with_gaps() {
        assert_eq!(Indices::parse(Some("4,5,6")), Indices { current: Some(4), planned: Some(5), door: Some(6) });
        assert_eq!(Indices::parse(Some(",5")), Indices { current: None, planned: Some(5), door: None });
        assert_eq!(Indices::parse(None), Indices::default());
    }

    #[test]
    fn the_link_sends_to_the_session_and_counts_what_it_sent() {
        let outbox = Outbox::default();
        let mut receiver: UnboundedReceiver<Sent> = outbox.subscribe((1, 2));
        let link = PlatformLink::new(outbox, (1, 2), PlatformIds { manager: 80, platform: 84 }).with_indices(Indices { current: Some(1), planned: Some(2), door: Some(3) });
        assert_eq!(link.announce(Step::Open), 3);
        let Some(Update::Variables(first)) = receiver.try_recv().unwrap().unwrap().update else { panic!() };
        assert_eq!((first.entity_id, first.variables[0].index, first.variables[0].value.clone()), (84, 3, vec![5]));
        let unindexed = PlatformLink::new(Outbox::default(), (1, 2), PlatformIds { manager: 80, platform: 84 }).with_indices(Indices::default());
        assert_eq!(unindexed.announce(Step::Open), 0);
    }
}

//! Reloading the weapon in the player's hand. The client's `OnActionReload` picks the magazine itself, the best one of the
//! class the weapon takes among those attached to the actor, and sends `SReload` (0x60) naming it (`m_suitMagId`); the DGS
//! only checks the choice and does the part of the game that is its authority: the magazine goes into the weapon's
//! magazine port and the spent one takes its place, so the player keeps both. The client runs the reload animation from
//! its own state machine. Like a draw (see `draw`), a reload changes the DGS's tree of what the player wears and the
//! binds the client holds, not the durable record, which has no rounds to keep.

use crate::{
    entity::{Bind, Transform},
    loadout::{port_attach_id, ITEM_FLAGS},
    worn::{Worn, WornItem},
};

/// The port of a weapon that holds the magazine in use.
pub const MAGAZINE_PORT: &str = "magazine_attach";
/// The prefix of the ports a magazine is kept in on the body, `magazine_attach_1` and on.
const SPARE_PORT_PREFIX: &str = "magazine_attach_";
/// The number of the first spare magazine port.
const FIRST_SPARE_PORT: u32 = 1;
/// The most spare magazine ports a worn item has (a backpack has eight).
const MAX_SPARE_PORTS: u32 = 16;
/// `EReloadTarget` of a reload of the weapon's main magazine; `reloadSecondary` is 1.
pub const RELOAD_PRIMARY: u32 = 0;

/// The rounds each magazine of a session holds, by the magazine's entity. A magazine is the same entity wherever it is
/// attached, so a reload that swaps the loaded and the spare one moves each with its own count. A magazine not listed
/// holds what it was made with (full).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ammo {
    rounds: std::collections::HashMap<u64, u32>,
}

impl Ammo {
    pub fn set(&mut self, magazine: u64, rounds: u32) {
        self.rounds.insert(magazine, rounds);
    }

    /// The rounds of `magazine`, `None` while it is full.
    pub fn get(&self, magazine: u64) -> Option<u32> {
        self.rounds.get(&magazine).copied()
    }

    /// A magazine that is gone.
    pub fn forget(&mut self, magazine: u64) {
        self.rounds.remove(&magazine);
    }
}

/// The index of the replicated variable `ac.ammoCount` (key 0x004e260a, an int32 of the plain count) in the variable table of
/// a magazine entity depends on the components the magazine's class registers before its ammo container, so it is known
/// per class only. The index 55 known for `behr_rifle_ballistic_03_mag` is WRONG for the
/// magazine as the DGS binds it: the client died with `CSerializedComponent::ReceiveDirtyValuesFromNetwork: Serialized
/// variable deserializing stream size mismatch varName="CAlignmentSlot::m_lockedByEntityId.0"` when a shot sent it.
/// No class has a verified index, so this list is empty and nothing is sent until one is.
pub const VERIFIED_AMMO_COUNT_INDICES: &[(u32, u32)] = &[];

/// The index of `ac.ammoCount` in the variables of a magazine of class `class_crc`, when it is verified.
pub fn ammo_count_index(class_crc: u32) -> Option<u32> {
    VERIFIED_AMMO_COUNT_INDICES
        .iter()
        .find(|(class, _)| *class == class_crc)
        .map(|(_, index)| *index)
}

/// `ac.ammoCount` of `magazine` (of class `class_crc`) set to `rounds`, to send as a session update; `None` while the index
/// of the variable is not verified for the class (a wrong index kills the client).
pub fn ammo_update(
    magazine: u64,
    class_crc: u32,
    rounds: u32,
) -> Option<sc_api::internal::EntityVariables> {
    let index = ammo_count_index(class_crc)?;
    Some(sc_api::internal::EntityVariables {
        entity_id: magazine,
        variables: vec![sc_api::internal::EntityVariable {
            index,
            value: rounds.to_le_bytes().to_vec(),
        }],
    })
}

/// Why a reload did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The client named no magazine: it found nothing better to load.
    NoMagazine,
    /// The reload is of a secondary ammo container, which the DGS does not handle.
    Secondary,
    /// The weapon or the magazine is not worn.
    NotWorn,
    /// The magazine is not in a magazine port, or is the weapon's own, or hangs from the weapon.
    NotSpare,
    /// The weapon's magazine is of another class than the one named.
    WrongClass,
}

/// What a reload changes.
pub struct Reloaded {
    /// The magazine now in the weapon, and the one it replaced, when there was one.
    pub loaded: u64,
    pub spent: Option<u64>,
    /// The rounds each holds when the session knows them (`Ammo`); filled by the caller.
    pub loaded_rounds: Option<u32>,
    pub spent_rounds: Option<u32>,
    pub worn: Worn,
    /// Children first.
    pub unbinds: Vec<u64>,
    /// Parents first.
    pub binds: Vec<Bind>,
}

fn bind_of(item: &WornItem) -> Bind {
    Bind {
        entity_id: item.geid,
        class_crc: item.class_crc,
        flags: ITEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: item.parent,
        parent_attach_id: item.port_crc,
        components: Vec::new(),
        snapshots: Vec::new(),
    }
}

/// Whether `port_crc` is the key of a port magazines are kept in.
fn is_spare_port(port_crc: u32) -> bool {
    (FIRST_SPARE_PORT..=MAX_SPARE_PORTS)
        .any(|number| port_attach_id(&format!("{SPARE_PORT_PREFIX}{number}")) == port_crc)
}

/// The reload of `weapon`, which the player holds, with `magazine`, the one the client named.
pub fn reload(worn: &Worn, weapon: u64, target: u32, magazine: u64) -> Result<Reloaded, Refusal> {
    if target != RELOAD_PRIMARY {
        return Err(Refusal::Secondary);
    }
    let Some(_) = worn.get(weapon) else {
        return Err(Refusal::NotWorn);
    };
    let port = port_attach_id(MAGAZINE_PORT);
    let spent = worn.child_at(weapon, port).cloned();
    let usable = |item: &WornItem| {
        !worn.subtree(weapon).contains(&item.geid)
            && is_spare_port(item.port_crc)
            && spent
                .as_ref()
                .map_or(true, |spent| spent.class_crc == item.class_crc)
    };
    let named = worn.get(magazine).cloned();
    // The client names the magazine it likes best, which may not fit the weapon (a rifle magazine for a pistol) or may be
    // none at all; the weapon takes the spare of its own class the player wears.
    let chosen = named.clone().filter(|item| usable(item)).or_else(|| {
        let class = spent.as_ref()?.class_crc;
        worn.items()
            .filter(|item| item.class_crc == class && usable(item))
            .min_by_key(|item| item.geid)
            .cloned()
    });
    let Some(spare) = chosen else {
        return Err(match named {
            _ if magazine == 0 => Refusal::NoMagazine,
            None => Refusal::NotWorn,
            Some(item)
                if worn.subtree(weapon).contains(&item.geid) || !is_spare_port(item.port_crc) =>
            {
                Refusal::NotSpare
            }
            Some(_) => Refusal::WrongClass,
        });
    };
    let mut after = worn.clone();
    let mut unbinds = Vec::new();
    let mut binds = Vec::new();
    let loaded = WornItem {
        parent: weapon,
        port_crc: port,
        ..spare.clone()
    };
    unbinds.push(spare.geid);
    after.attach(loaded.clone());
    binds.push(bind_of(&loaded));
    if let Some(spent) = &spent {
        // The spent magazine takes the place the loaded one came from.
        let put_away = WornItem {
            parent: spare.parent,
            port_crc: spare.port_crc,
            ..spent.clone()
        };
        unbinds.insert(0, spent.geid);
        after.attach(put_away.clone());
        binds.push(bind_of(&put_away));
    }
    Ok(Reloaded {
        loaded: spare.geid,
        spent: spent.map(|spent| spent.geid),
        loaded_rounds: None,
        spent_rounds: None,
        worn: after,
        unbinds,
        binds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER: u64 = 300_000_000_000;
    const BODY: u64 = 400_000_000_001;
    const SUIT: u64 = 400_000_000_002;
    const PISTOL: u64 = 400_000_000_003;
    const LOADED: u64 = 400_000_000_004;
    const SPARE: u64 = 400_000_000_005;
    const HELMET: u64 = 400_000_000_006;
    const PISTOL_MAGAZINE: u32 = 11;
    const OTHER_MAGAZINE: u32 = 12;

    fn bind(entity_id: u64, class_crc: u32, parent_id: u64, port: &str) -> Bind {
        Bind {
            entity_id,
            class_crc,
            flags: 0,
            transform: Transform::IDENTITY,
            parent_id,
            parent_attach_id: port_attach_id(port),
            components: Vec::new(),
            snapshots: Vec::new(),
        }
    }

    fn worn() -> Worn {
        Worn::from_binds(
            PLAYER,
            &[
                bind(BODY, 1, PLAYER, "Body_ItemPort"),
                bind(SUIT, 2, BODY, "Armor_Undersuit"),
                bind(PISTOL, 3, BODY, "weapon_attach_hand_right"),
                bind(LOADED, PISTOL_MAGAZINE, PISTOL, "magazine_attach"),
                bind(SPARE, PISTOL_MAGAZINE, SUIT, "magazine_attach_1"),
                bind(HELMET, 4, SUIT, "Armor_Helmet"),
            ],
        )
    }

    #[test]
    fn the_spare_goes_into_the_weapon_and_the_spent_one_takes_its_place() {
        let done = reload(&worn(), PISTOL, RELOAD_PRIMARY, SPARE).expect("a spare of the class");
        assert_eq!((done.loaded, done.spent), (SPARE, Some(LOADED)));
        assert_eq!(
            done.worn
                .child_at(PISTOL, port_attach_id(MAGAZINE_PORT))
                .map(|item| item.geid),
            Some(SPARE)
        );
        assert_eq!(
            done.worn
                .child_at(SUIT, port_attach_id("magazine_attach_1"))
                .map(|item| item.geid),
            Some(LOADED)
        );
        assert_eq!(done.worn.len(), worn().len());
        assert_eq!(done.unbinds, vec![LOADED, SPARE]);
        let binds: Vec<(u64, u64, u32)> = done
            .binds
            .iter()
            .map(|bind| (bind.entity_id, bind.parent_id, bind.parent_attach_id))
            .collect();
        assert_eq!(
            binds,
            vec![
                (SPARE, PISTOL, port_attach_id(MAGAZINE_PORT)),
                (LOADED, SUIT, port_attach_id("magazine_attach_1"))
            ]
        );
    }

    #[test]
    fn the_count_is_not_sent_for_a_class_without_a_proven_index() {
        assert_eq!(ammo_count_index(0x1234_5678), None);
        assert!(ammo_update(SPARE, 0x1234_5678, 7).is_none());
        assert!(
            VERIFIED_AMMO_COUNT_INDICES
                .iter()
                .all(|(_, index)| *index != 55),
            "55 crashed the client"
        );
    }

    #[test]
    fn each_magazine_keeps_its_rounds_through_a_swap() {
        let mut ammo = Ammo::default();
        ammo.set(LOADED, 4);
        let done = reload(&worn(), PISTOL, RELOAD_PRIMARY, SPARE).unwrap();
        assert_eq!(
            (ammo.get(done.loaded), ammo.get(done.spent.unwrap())),
            (None, Some(4)),
            "the spare is full, the spent one has 4"
        );
        let back = reload(&done.worn, PISTOL, RELOAD_PRIMARY, LOADED).unwrap();
        assert_eq!(ammo.get(back.loaded), Some(4));
        ammo.forget(LOADED);
        assert_eq!(ammo.get(LOADED), None);
    }

    #[test]
    fn a_second_reload_gives_the_magazines_back() {
        let first = reload(&worn(), PISTOL, RELOAD_PRIMARY, SPARE).unwrap();
        let second = reload(&first.worn, PISTOL, RELOAD_PRIMARY, LOADED)
            .expect("the other one is the spare now");
        assert_eq!(
            second
                .worn
                .child_at(PISTOL, port_attach_id(MAGAZINE_PORT))
                .map(|item| item.geid),
            Some(LOADED)
        );
    }

    #[test]
    fn an_empty_weapon_takes_the_magazine() {
        let mut empty = worn();
        empty.detach(LOADED);
        let done = reload(&empty, PISTOL, RELOAD_PRIMARY, SPARE).unwrap();
        assert_eq!(
            (done.spent, done.unbinds, done.binds.len()),
            (None, vec![SPARE], 1usize)
        );
        assert_eq!(
            done.worn
                .child_at(PISTOL, port_attach_id(MAGAZINE_PORT))
                .map(|item| item.geid),
            Some(SPARE)
        );
    }

    #[test]
    fn only_a_spare_magazine_of_the_class_is_taken() {
        let worn = worn();
        assert_eq!(
            reload(&worn, PISTOL, RELOAD_PRIMARY, 0)
                .map(|done| done.loaded)
                .ok(),
            Some(SPARE),
            "none named: the weapon takes its own spare"
        );
        let mut bare = worn.clone();
        bare.detach(SPARE);
        assert_eq!(
            reload(&bare, PISTOL, RELOAD_PRIMARY, 0).err(),
            Some(Refusal::NoMagazine)
        );
        assert_eq!(
            reload(&worn, PISTOL, 1, SPARE).err(),
            Some(Refusal::Secondary)
        );
        assert_eq!(
            reload(&bare, PISTOL, RELOAD_PRIMARY, 999).err(),
            Some(Refusal::NotWorn)
        );
        assert_eq!(
            reload(&worn, 999, RELOAD_PRIMARY, SPARE).err(),
            Some(Refusal::NotWorn)
        );
        assert_eq!(
            reload(&bare, PISTOL, RELOAD_PRIMARY, LOADED).err(),
            Some(Refusal::NotSpare),
            "already in the weapon"
        );
        assert_eq!(
            reload(&bare, PISTOL, RELOAD_PRIMARY, HELMET).err(),
            Some(Refusal::NotSpare),
            "not a magazine"
        );
        assert_eq!(
            reload(&worn, PISTOL, RELOAD_PRIMARY, HELMET)
                .map(|done| done.loaded)
                .ok(),
            Some(SPARE),
            "a wrong choice gets the weapon's own spare"
        );
        let mut other = worn.clone();
        other.attach(WornItem {
            geid: SPARE,
            class_crc: OTHER_MAGAZINE,
            parent: SUIT,
            port_crc: port_attach_id("magazine_attach_1"),
        });
        assert_eq!(
            reload(&other, PISTOL, RELOAD_PRIMARY, SPARE).err(),
            Some(Refusal::WrongClass)
        );
    }
}

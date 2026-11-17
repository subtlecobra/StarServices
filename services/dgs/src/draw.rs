//! Drawing and holstering a weapon. The client sends `SUnholster` naming the item port the weapon sits in (the owner entity
//! and the port's index in it) and the pose it wants, and `SHolster` naming the held weapon. The client's held entity
//! controller reads the hands from what is attached to the body's two hand ports (see `carry`), and a client that sees no
//! weapon in its hand after its draw puts the weapon away on its own. So a draw attaches the weapon to a hand port and a
//! holster puts it back where it came from, in the DGS's tree of what the player wears and in the binds the client holds.
//! The durable record is not touched: a draw lasts as long as the session, and the next join has every weapon holstered.

use crate::{
    carry::HAND_PORTS,
    entity::{Bind, Transform},
    loadout::{port_attach_id, ITEM_FLAGS},
    worn::{Worn, WornItem},
};

/// The ports a weapon is holstered in. The client names the port by its index in the owner's port list, so the weapon is
/// the owner's item in one of these that the index (see `PORT_INDEXES`) points at.
pub const HOLSTER_PORTS: [&str; 13] = [
    "wep_sidearm",
    "wep_stocked_1",
    "wep_stocked_2",
    "wep_stocked_3",
    "wep_stocked_4",
    "wep_unstocked_1",
    "wep_unstocked_2",
    "wep_unstocked_3",
    "wep_unstocked_4",
    "wep_primary",
    "wep_secondary",
    "wep_melee",
    "wep_holster",
];

/// The index `SUnholster` gives a holster port in its owner, for the ports of the known loadouts. The wire layout of the action
/// is: left hand port (`u16` index, `u64` owner), a flag byte, right hand port (index, owner), a flag byte, the pose;
/// the draw names the right hand's. For a rifle in the backpack's `wep_stocked_2` (index 0), a second weapon in
/// its `wep_stocked_3` (index 1) and a pistol in the legs' `wep_sidearm` (index 6).
pub const PORT_INDEXES: [(&str, u32); 3] = [
    ("wep_stocked_2", 0),
    ("wep_stocked_3", 1),
    ("wep_sidearm", 6),
];

/// `ESCActorResultPoseState` of a draw and of a holster.
pub const POSE_NO_WEAPON: u32 = 1;
pub const POSE_WEAPON: u32 = 2;

/// The hand a draw goes to first.
const PREFERRED_HAND: usize = 1;

/// What a draw or a holster changes.
pub struct Moved {
    pub weapon: u64,
    /// Where the weapon came from, to put it back: the item it hung from and the key of the port.
    pub from: (u64, u32),
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

/// Every weapon holstered in `owner`, in `HOLSTER_PORTS` order, with the name of its port.
fn holstered(worn: &Worn, owner: u64) -> Vec<(&'static str, u64)> {
    HOLSTER_PORTS
        .iter()
        .filter_map(|port| {
            worn.child_at(owner, port_attach_id(port))
                .map(|item| (*port, item.geid))
        })
        .collect()
}

/// The weapon holstered in `owner` that the client's port `index` names. An owner with one weapon gives that one. With
/// several, the port whose index is known (`PORT_INDEXES`) wins; for a port of unknown index, the one at that position of
/// the owner's weapons, else the first.
pub fn holstered_in(worn: &Worn, owner: u64, index: u32) -> Option<u64> {
    let weapons = holstered(worn, owner);
    if let [(_, only)] = weapons[..] {
        return Some(only);
    }
    let known = weapons.iter().find(|(port, _)| {
        PORT_INDEXES
            .iter()
            .any(|(name, at)| name == port && *at == index)
    });
    known
        .or_else(|| weapons.get(index as usize))
        .or_else(|| weapons.first())
        .map(|(_, weapon)| *weapon)
}

fn moved(worn: &Worn, weapon: u64, parent: u64, port_crc: u32) -> Option<Moved> {
    let item = worn.get(weapon)?.clone();
    let mut after = worn.clone();
    let mut unbinds = after.detach(weapon);
    unbinds.reverse();
    // What hangs from the weapon keeps its ports; the weapon alone changes parent.
    let mut binds = Vec::new();
    for geid in worn.subtree(weapon) {
        let mut part = worn.get(geid)?.clone();
        if geid == weapon {
            part.parent = parent;
            part.port_crc = port_crc;
        }
        after.attach(part.clone());
        binds.push(bind_of(&part));
    }
    Some(Moved {
        weapon,
        from: (item.parent, item.port_crc),
        worn: after,
        unbinds,
        binds,
    })
}

/// A draw from the port `index` of `owner`'s holster: the weapon goes to a free hand port of the body.
pub fn unholster(worn: &Worn, player: u64, owner: u64, index: u32) -> Option<Moved> {
    let weapon = holstered_in(worn, owner, index)?;
    let body = worn.child_at(player, port_attach_id("Body_ItemPort"))?.geid;
    let hands: Vec<u32> = HAND_PORTS.iter().map(|hand| port_attach_id(hand)).collect();
    let order = [PREFERRED_HAND, 1 - PREFERRED_HAND];
    let hand = order
        .iter()
        .map(|index| hands[*index])
        .find(|hand| worn.child_at(body, *hand).is_none())?;
    moved(worn, weapon, body, hand)
}

/// A holster of `weapon`, back to the item and port it came from. `None` when it is not in a hand.
pub fn holster(worn: &Worn, weapon: u64, back: (u64, u32)) -> Option<Moved> {
    let item = worn.get(weapon)?;
    if !HAND_PORTS
        .iter()
        .any(|hand| port_attach_id(hand) == item.port_crc)
        || !worn.contains(back.0)
    {
        return None;
    }
    moved(worn, weapon, back.0, back.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER: u64 = 300_000_000_000;
    const BODY: u64 = 400_000_000_001;
    const LEGS: u64 = 400_000_000_002;
    const PISTOL: u64 = 400_000_000_003;
    const OPTIC: u64 = 400_000_000_004;
    const KNIFE: u64 = 400_000_000_005;
    const SIDEARM_INDEX: u32 = 6;

    fn worn() -> Worn {
        let mut worn = Worn::default();
        let worn_bind = |entity_id, parent_id, port: &str| Bind {
            entity_id,
            class_crc: 7,
            flags: 0,
            transform: Transform::IDENTITY,
            parent_id,
            parent_attach_id: port_attach_id(port),
            components: Vec::new(),
            snapshots: Vec::new(),
        };
        worn = Worn::from_binds(
            PLAYER,
            &[
                worn_bind(BODY, PLAYER, "Body_ItemPort"),
                worn_bind(LEGS, BODY, "Armor_Legs"),
                worn_bind(PISTOL, LEGS, "wep_sidearm"),
                worn_bind(OPTIC, PISTOL, "optics_attach"),
                worn_bind(KNIFE, LEGS, "utility_attach_2"),
            ],
        );
        worn
    }

    #[test]
    fn a_draw_puts_the_weapon_and_what_hangs_from_it_in_the_right_hand() {
        let worn = worn();
        let draw = unholster(&worn, PLAYER, LEGS, SIDEARM_INDEX)
            .expect("a weapon is holstered in the legs");
        assert_eq!(draw.weapon, PISTOL);
        assert_eq!(draw.from, (LEGS, port_attach_id("wep_sidearm")));
        assert_eq!(draw.unbinds, vec![OPTIC, PISTOL]);
        let binds: Vec<(u64, u64, u32)> = draw
            .binds
            .iter()
            .map(|bind| (bind.entity_id, bind.parent_id, bind.parent_attach_id))
            .collect();
        assert_eq!(
            binds,
            vec![
                (PISTOL, BODY, port_attach_id("weapon_attach_hand_right")),
                (OPTIC, PISTOL, port_attach_id("optics_attach"))
            ]
        );
        assert_eq!(
            draw.worn
                .child_at(BODY, port_attach_id("weapon_attach_hand_right"))
                .map(|item| item.geid),
            Some(PISTOL)
        );
        assert!(draw
            .worn
            .child_at(LEGS, port_attach_id("wep_sidearm"))
            .is_none());
        assert!(draw.worn.contains(OPTIC));
    }

    #[test]
    fn a_second_weapon_goes_to_the_other_hand() {
        let worn = worn();
        let first = unholster(&worn, PLAYER, LEGS, SIDEARM_INDEX).unwrap();
        let mut worn = first.worn;
        worn.attach(WornItem {
            geid: 400_000_000_009,
            class_crc: 7,
            parent: BODY,
            port_crc: port_attach_id("wep_stocked_2"),
        });
        let second = unholster(&worn, PLAYER, BODY, 0).unwrap();
        assert_eq!(
            second.binds[0].parent_attach_id,
            port_attach_id("weapon_attach_hand_left")
        );
        let mut full = second.worn;
        full.attach(WornItem {
            geid: 400_000_000_010,
            class_crc: 7,
            parent: BODY,
            port_crc: port_attach_id("wep_stocked_1"),
        });
        assert!(
            unholster(&full, PLAYER, BODY, 0).is_none(),
            "both hands are full"
        );
    }

    #[test]
    fn a_holster_puts_it_back_where_it_came_from() {
        let worn = worn();
        let draw = unholster(&worn, PLAYER, LEGS, SIDEARM_INDEX).unwrap();
        let back = holster(&draw.worn, draw.weapon, draw.from).expect("it is in a hand");
        assert_eq!(
            back.worn
                .child_at(LEGS, port_attach_id("wep_sidearm"))
                .map(|item| item.geid),
            Some(PISTOL)
        );
        assert_eq!(back.unbinds, vec![OPTIC, PISTOL]);
        assert_eq!(back.binds[0].parent_id, LEGS);
        assert!(
            holster(&back.worn, PISTOL, draw.from).is_none(),
            "no longer in a hand"
        );
    }

    #[test]
    fn an_owner_with_no_weapon_in_a_holster_draws_nothing() {
        let worn = worn();
        assert_eq!(holstered_in(&worn, BODY, 0), None);
        assert!(unholster(&worn, PLAYER, KNIFE, 0).is_none());
    }

    // The `SUnholster` payloads: left port (u16 index, u64 owner), flag, right port, flag, pose.
    const PISTOL_DRAW: &str = "ff0300000000000000000006008ea0db215d0000000102";
    const RIFLE_DRAW: &str = "ff030000000000000000fe00008aa0db215d0000000102";
    const SECOND_DRAW: &str = "ff0300000000000000000001008aa0db215d0000000102";
    const PACK_ID: u64 = 400_000_000_138;
    const LEGS_ID: u64 = 400_000_000_142;
    const RIFLE_ID: u64 = 400_000_000_148;
    const PISTOL_ID: u64 = 400_000_000_162;
    const SECOND_ID: u64 = 400_000_000_223;

    /// (index, owner) of the right hand port of a payload.
    fn right_port(hex: &str) -> (u32, u64) {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect();
        (
            u32::from(u16::from_le_bytes([bytes[11], bytes[12]])),
            u64::from_le_bytes(bytes[13..21].try_into().unwrap()),
        )
    }

    fn gear() -> Worn {
        let bind = |entity_id, parent_id, port: &str| Bind {
            entity_id,
            class_crc: 7,
            flags: 0,
            transform: Transform::IDENTITY,
            parent_id,
            parent_attach_id: port_attach_id(port),
            components: Vec::new(),
            snapshots: Vec::new(),
        };
        Worn::from_binds(
            PLAYER,
            &[
                bind(BODY, PLAYER, "Body_ItemPort"),
                bind(LEGS_ID, BODY, "Armor_Legs"),
                bind(PACK_ID, BODY, "backpack"),
                bind(PISTOL_ID, LEGS_ID, "wep_sidearm"),
                bind(RIFLE_ID, PACK_ID, "wep_stocked_2"),
                bind(SECOND_ID, PACK_ID, "wep_stocked_3"),
            ],
        )
    }

    #[test]
    fn the_client_draws_pick_the_weapon_of_their_port() {
        let worn = gear();
        let pick = |hex: &str| {
            let (index, owner) = right_port(hex);
            holstered_in(&worn, owner, index)
        };
        assert_eq!(right_port(PISTOL_DRAW), (6, LEGS_ID));
        assert_eq!(pick(PISTOL_DRAW), Some(PISTOL_ID));
        assert_eq!(pick(RIFLE_DRAW), Some(RIFLE_ID));
        assert_eq!(
            pick(SECOND_DRAW),
            Some(SECOND_ID),
            "the second weapon of the backpack is not the first"
        );
    }

    #[test]
    fn an_owner_with_one_weapon_gives_it_whatever_the_index() {
        let worn = gear();
        assert_eq!(holstered_in(&worn, LEGS_ID, 0), Some(PISTOL_ID));
        assert_eq!(
            holstered_in(&worn, PACK_ID, 99),
            Some(RIFLE_ID),
            "an unknown index of several: the first"
        );
    }

    #[test]
    fn a_draw_of_the_second_weapon_puts_the_first_back() {
        let worn = gear();
        let first = unholster(&worn, PLAYER, PACK_ID, 0).unwrap();
        assert_eq!(first.weapon, RIFLE_ID);
        let back = holster(&first.worn, first.weapon, first.from).unwrap();
        let second = unholster(&back.worn, PLAYER, PACK_ID, 1).unwrap();
        assert_eq!(second.weapon, SECOND_ID);
        assert_eq!(second.from, (PACK_ID, port_attach_id("wep_stocked_3")));
    }
}

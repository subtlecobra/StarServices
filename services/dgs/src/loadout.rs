//! What a player wears: its body, head and the items on them, each an entity of its own that the DGS builds and the
//! replicant binds after the player, a batch per depth once the batch of each item's parent has ended. Without them the client has a player with no geometry, so nothing shows in first or third
//! person. See "Body and loadout" in the protocol notes (kept outside this repository).
//!
//! A player with no saved character (no `SaveCharacterCustomizations` reached the backend) gets the game's default, `DefaultPlayerLoadoutEntitlementRecord.PlayerDefaultLoadouts`, loadout `Default`,
//! `scripts/loadouts/player/defaultloadouts/pu_default.xml`, which includes `naked_default.xml`, with the default loadouts of
//! the items themselves (`head_default.xml`, `helmet_default.xml`, the pistol's and the lens's manual entries) filled in.
//! The client's own frontend builds the same 23 items for its local player, and logs them as `AttachmentReceived`. A saved
//! character changes the classes of the head's items, and may add some, see `appearance`.

use std::borrow::Cow;

use sc_api::internal::LoadoutItem;

use crate::entity::{Bind, Transform};

/// One item of a loadout: its class, the port of its parent it goes in, and that parent, an index into the same table, or
/// `None` for the player itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The class's name, or for a class a saved character chose, its GUID.
    pub class_name: Cow<'static, str>,
    /// CRC of the `EntityClassDefinition` GUID, as `sc-admin`'s `class_crc` and `class_crc` below.
    pub class_crc: u32,
    pub port: Cow<'static, str>,
    pub parent: Option<usize>,
}

const fn item(class_name: &'static str, class_crc: u32, port: &'static str, parent: Option<usize>) -> Item {
    Item { class_name: Cow::Borrowed(class_name), class_crc, port: Cow::Borrowed(port), parent }
}

/// The class CRC of an `EntityClassDefinition` GUID written as text (`dbaa8a7d-755f-4104-8b24-7b58fd1e76f6`): CRC-32C of
/// its 16 bytes, each half of 8 taken from its last byte to its first. `None` for text that is not a GUID.
pub fn class_crc(guid: &str) -> Option<u32> {
    let hex: Vec<u8> = guid.bytes().filter(|byte| *byte != b'-').collect();
    if hex.len() != 32 || guid.len() != 36 {
        return None;
    }
    let bytes = hex
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let crc = bytes.chunks(8).flat_map(|half| half.iter().rev()).fold(!0u32, |crc, byte| {
        (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
    });
    Some(!crc)
}

/// The default character, parents before children, as the client looks a parent up as it reads each bind. GUIDs and
/// CRCs from `Game2.dcb` of 4.10.2 (build 12913470).
pub const DEFAULT_LOADOUT: [Item; 23] = [
    // 0, dbaa8a7d-755f-4104-8b24-7b58fd1e76f6
    item("body_01_noMagicPocket", 1_844_164_324, "Body_ItemPort", None),
    // 1, 1d5cfab3-bf80-4550-b4ab-39e896a7086e
    item("PU_Protos_Head", 3_454_561_855, "Head_ItemPort", Some(0)),
    // 2, 8011c1ea-de55-4a23-9c1b-553ef3017aea
    item("Head_Teeth", 3_134_052_679, "Teeth_ItemPort", Some(1)),
    // 3, bec45eec-d975-45ab-a97f-a2c284f47814
    item("Head_Eyes_Brown_03", 2_007_655_374, "Eyes_ItemPort", Some(1)),
    // 4, 9873fa65-1abb-4907-a943-20c94c68de0f
    item("Default_LensDisplay_PU", 245_041_872, "Lens_ItemPort", Some(3)),
    // 5, a64be577-9f99-403d-bd9b-e4de74d55de6: the lens's own manual loadout
    item("FPS_DefaultRadar_Lens", 1_275_505_999, "radar", Some(4)),
    // 6, 968d0d95-2224-47dc-a05a-da423a4a1c81
    item("hair_02", 2_767_731_557, "Hair_ItemPort", Some(1)),
    // 7, 6217c113-a448-443b-82aa-1bb108ba8e11
    item("Head_Eyelashes", 3_602_428_806, "Eyelashes_ItemPort", Some(1)),
    // 8, 24967ba4-e384-431e-9e26-251351d44c5f
    item("Head_Eyedetail", 2_950_686_774, "Eyedetail_ItemPort", Some(1)),
    // 9, 89ec0bbc-7daf-4b09-a98d-f8dd8df32305: `naked_default.xml` overrides the head's `brows_002`
    item("brows_001", 2_804_479_089, "Eyebrow_ItemPort", Some(1)),
    // 10, 71268392-61f4-40f2-9280-92971478a719: from `head_default.xml`
    item("Shared_Scalp_Unified", 3_571_963_911, "Universal_Scalp_ItemPort", Some(1)),
    // 11, f0705a24-2c09-43b0-b90f-ce84d06ec1cd
    item("rsi_odyssey_undersuit_01_01_01", 1_103_158_163, "Armor_Undersuit", Some(0)),
    // 12, 0190a637-936f-4834-812b-f945a0413467
    item("rsi_odyssey_undersuit_helmet_01_01_01", 3_015_115_031, "Armor_Helmet", Some(11)),
    // 13, 77d6e8ea-e07c-4f53-9c59-6217d7451f11
    item("universal_necksock_01", 4_208_070_842, "universal_necksock", Some(12)),
    // 14, b48e460e-6809-4547-a7b1-3ebb3c5930ac: from `helmet_default.xml`
    item("FP_Visor", 3_412_431_319, "helmet_visor", Some(12)),
    // 15, 425bcac8-99e9-493e-aa41-0f8904856e45
    item("universal_necksock_undersuit_01", 766_493_181, "universal_necksock_undersuit", Some(11)),
    // 16, 7d50411f-088c-4c99-b85a-a6eaf95504c3
    item("crlf_consumable_healing_01", 2_747_339_340, "medPen_attach_1", Some(11)),
    // 17, 0ff9144c-970d-4861-ae27-b27ee3c969a9
    item("klwe_pistol_energy_01", 2_107_798_123, "wep_sidearm", Some(11)),
    // 18, e15e874d-a0c1-4f91-939b-b04bf7a9e839: the pistol's own manual loadout
    item("klwe_pistol_energy_01_mag", 3_830_248_767, "magazine_attach", Some(17)),
    // 19, the spare magazine on the undersuit
    item("klwe_pistol_energy_01_mag", 3_830_248_767, "magazine_attach_1", Some(11)),
    // 20, f047c8ed-7f5e-4bfa-81c2-c4c58e419314
    item("MobiGlas", 2_793_687_147, "mobiglas_attach", Some(0)),
    // 21, f92ef99d-1488-4001-89c7-82d713be0587
    item("PersonalMobiGlas_PU", 2_758_675_951, "mobiglas_screen_attach", Some(20)),
    // 22, f5a0d2f6-74fb-4247-aa3f-1fc61bcc2f49
    item("LegacyMobiGlas", 2_622_925_666, "legacy_mobiglas_screen_attach", Some(20)),
];

/// Item ids: a block per player, above the players' own ids (300,000,000,000 + index) and the hangars' (220,000,000,000 +
/// index x 0x10000), outside the client's local scope. A real server's items have GEIDs from the backend; ours have none
/// stored yet, so the block stands in, the same on every join.
const FIRST_ITEM_ID: u64 = 310_000_000_000;
/// Ids per player.
pub const ITEM_ID_STRIDE: u64 = 0x100;

/// The key of a port: CRC-32C (Castagnoli, the polynomial of class CRCs) of the lowercased port name. Read live from a
/// client: the player's item port container keys its one port, `Body_ItemPort`, as `0xab6341ac`, and `GetPortByKey`
/// (0x1428bb5f0, container slot `+0x718`) binary searches those keys for the bind's parent attachment. Plain CRC-32 of the
/// name as written (`0x1a507951`) found no port, and the client destroyed every item ("Failed to attach to itemport").
pub fn port_attach_id(port: &str) -> u32 {
    !port.bytes().map(|byte| byte.to_ascii_lowercase()).fold(!0u32, |crc, byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
    })
}

/// `kSpawnFlagReplicated`.
const SPAWN_FLAG_REPLICATED: u32 = 0x1000;
/// The bits of the root's spawn flags every item of its loadout takes over (`kInheritedSpawnFlags` of
/// `CLoadoutSpawnBatchHelper`).
const INHERITED_FLAGS: u32 = 0x4020_8000;

/// The flags of an item bind: a server's loadout spawn gives every item `Replicated` and the root's inherited bits, here
/// those of the player, so 0x201000.
pub const ITEM_FLAGS: u32 = SPAWN_FLAG_REPLICATED | (crate::player::PLAYER_FLAGS & INHERITED_FLAGS);

/// The id of item `slot` of player `index`.
pub fn item_id(player_index: u64, slot: usize) -> u64 {
    FIRST_ITEM_ID + player_index * ITEM_ID_STRIDE + slot as u64
}

/// The binds of `loadout` for the player `player_id`, in the table's order: each under its parent, in its port, with the
/// ids the DGS makes up for it when sc-entitygraph has none.
pub fn loadout_binds(player_id: u64, player_index: u64, loadout: &[Item]) -> Vec<Bind> {
    assert!(loadout.len() as u64 <= ITEM_ID_STRIDE, "a loadout takes more ids than a player has");
    let ids: Vec<u64> = (0..loadout.len()).map(|slot| item_id(player_index, slot)).collect();
    loadout_binds_with(player_id, loadout, &ids)
}

/// As `loadout_binds`, with `ids` for the items, one per item: the GEIDs sc-entitygraph made for them.
pub fn loadout_binds_with(player_id: u64, loadout: &[Item], ids: &[u64]) -> Vec<Bind> {
    assert_eq!(loadout.len(), ids.len(), "an id for each item");
    loadout
        .iter()
        .enumerate()
        .map(|(slot, item)| Bind {
            entity_id: ids[slot],
            class_crc: item.class_crc,
            flags: ITEM_FLAGS,
            transform: Transform::IDENTITY,
            parent_id: item.parent.map_or(player_id, |parent| ids[parent]),
            parent_attach_id: port_attach_id(&item.port),
            components: Vec::new(),
            snapshots: Vec::new(),
        })
        .collect()
}

/// The loadout as sc-entitygraph is asked to keep it.
pub fn request_items(loadout: &[Item]) -> Vec<LoadoutItem> {
    loadout
        .iter()
        .map(|item| LoadoutItem {
            class_crc: item.class_crc,
            port_crc: port_attach_id(&item.port),
            parent: item.parent.map_or(-1, |parent| parent as i32),
        })
        .collect()
}

/// The command id of a player's loadout: the player and what it wears, so the same loadout is made once and a changed
/// one (another head) is made anew.
pub fn command_id(player_id: u64, loadout: &[Item]) -> String {
    let hash = request_items(loadout).iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, item| {
        [item.class_crc, item.port_crc, item.parent as u32]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .fold(hash, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3))
    });
    format!("loadout:{player_id}:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_crcs_are_those_of_their_guids() {
        assert_eq!(class_crc("5287b146-fa24-49b8-8126-326164217c4f"), Some(2_961_494_058));
        assert_eq!(class_crc("dbaa8a7d-755f-4104-8b24-7b58fd1e76f6"), Some(DEFAULT_LOADOUT[0].class_crc));
        assert_eq!(class_crc("1D5CFAB3-BF80-4550-B4AB-39E896A7086E"), Some(DEFAULT_LOADOUT[1].class_crc));
        assert_eq!(class_crc("0ff9144c-970d-4861-ae27-b27ee3c969a9"), Some(DEFAULT_LOADOUT[17].class_crc));
        assert_eq!(class_crc("not a guid"), None);
        assert_eq!(class_crc("0ff9144c-970d-4861-ae27-b27ee3c969zz"), None);
    }

    #[test]
    fn parents_come_before_their_items() {
        for (slot, item) in DEFAULT_LOADOUT.iter().enumerate() {
            assert!(item.parent.is_none_or(|parent| parent < slot), "{} before its parent", item.class_name);
        }
        assert_eq!(DEFAULT_LOADOUT.iter().filter(|item| item.parent.is_none()).count(), 1, "only the body is on the player");
    }

    #[test]
    fn the_port_key_is_the_one_a_client_holds() {
        assert_eq!(port_attach_id("Body_ItemPort"), 0xab63_41ac, "read live from the player's port container");
        assert_eq!(port_attach_id("Body_ItemPort"), port_attach_id("body_itemport"));
    }

    #[test]
    fn the_body_hangs_from_the_player_and_the_rest_from_their_items() {
        let binds = loadout_binds(300_000_000_001, 1, &DEFAULT_LOADOUT);
        assert_eq!(binds.len(), 23);
        assert_eq!((binds[0].entity_id, binds[0].parent_id), (310_000_000_256, 300_000_000_001));
        assert_eq!(binds[1].parent_id, binds[0].entity_id);
        assert_eq!(binds[18].parent_id, binds[17].entity_id);
        assert_eq!(binds[0].parent_attach_id, port_attach_id("Body_ItemPort"));
    }

    #[test]
    fn the_command_id_follows_what_the_player_wears() {
        let plain = command_id(300_000_000_001, &DEFAULT_LOADOUT);
        assert_eq!(plain, command_id(300_000_000_001, &DEFAULT_LOADOUT));
        assert_ne!(plain, command_id(300_000_000_002, &DEFAULT_LOADOUT));
        let mut other = DEFAULT_LOADOUT.to_vec();
        other[3].class_crc += 1;
        assert_ne!(plain, command_id(300_000_000_001, &other));
        let items = request_items(&DEFAULT_LOADOUT);
        assert_eq!((items[0].parent, items[1].parent, items[0].port_crc), (-1, 0, 0xab63_41ac));
    }

    #[test]
    fn real_ids_replace_the_made_up_ones() {
        let ids: Vec<u64> = (0..23).map(|slot| 400_000_000_000 + slot).collect();
        let binds = loadout_binds_with(300_000_000_001, &DEFAULT_LOADOUT, &ids);
        assert_eq!((binds[0].entity_id, binds[0].parent_id, binds[1].parent_id), (400_000_000_000, 300_000_000_001, 400_000_000_000));
    }

    #[test]
    fn items_are_replicated_and_inherit_the_players_bits() {
        assert_eq!(ITEM_FLAGS, 0x0020_1000);
        assert!(loadout_binds(300_000_000_001, 1, &DEFAULT_LOADOUT).iter().all(|bind| bind.flags == ITEM_FLAGS));
    }

    #[test]
    fn item_ids_stay_out_of_the_local_scope_and_apart_per_player() {
        assert!(item_id(0, 0) > crate::player::LAST_LOCAL_ID);
        assert!(item_id(0, DEFAULT_LOADOUT.len() - 1) < item_id(1, 0));
        assert!(item_id(0, 0) > 300_000_000_000 + 1_000_000_000);
    }
}

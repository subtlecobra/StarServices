//! What every client gets first, under no system: the game rules and the game mode.
//!
//! No territory covers these, so no DGS has authority over them in any sense yet. A DGS builds them so that the replicant
//! need not know what they are.

use crate::entity::{Bind, Snapshot, Transform, UNIVERSE_ROOT_ID};
use crate::solar_system::ZONE_ATTACHMENT;

/// The CRC of `GameRulesSingleton`, GUID 2fc6e803-de79-4556-ae25-34275e6f5a51.
const GAME_RULES_CLASS_CRC: u32 = 0x42aa_0b07;
/// `SeedGameRulesAndMode` spawns with these: replicated and unstreamable.
const GAME_RULES_FLAGS: u32 = 0x3000;
/// The client registers one class per `GameMode` record, with the record's GUID, here `GameMode.SC_Default`:
/// 330ce5d3-fb01-4f82-8708-3154a3a4b78a.
const GAME_MODE_CLASS_CRC: u32 = 0x84e8_1cc1;
/// The seed lets the entity system pick this id. Outside the client's local range and the preallocated
/// range from 199998000000 to 200000000000, and below the GEIDs issued to players.
const GAME_MODE_ENTITY_ID: u64 = 210_000_000_001;
/// CRC-32 of `m_megamapname`.
const MEGA_MAP_KEY: u32 = 0x2ff5_d193;
/// `FUN_140618690`: the halves of FNV-1a 64 of `Stringchar`, xored.
pub(crate) const STRING_TYPE: u32 = 0x54a0_b124;
/// The `MegaMap` record of the PU. Only `SC_Frontend`'s value shows in real logs, so this one is unconfirmed.
const MEGA_MAP: &str = "MegaMap.PU_All";

/// A `String` variable's value: `[varint length][bytes]`, with no terminator.
pub(crate) fn string_value(value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut length = value.len();
    while length >= 0x80 {
        out.push(length as u8 | 0x80);
        length >>= 7;
    }
    out.push(length as u8);
    out.extend_from_slice(value.as_bytes());
    out
}

/// `SeedGameRulesAndMode` gives `GameRulesSingleton` the universe root's id, so one bind is both the root
/// the client's model needs and the `GameRules` component `WaitForGameRules` needs. It spawns the mode
/// beside that, and the mode's `GameMode` component sets `CGameRules+0x1f8` on spawn, which is what
/// `WaitForGameRulesSpawningModule` waits for. A record comes after its parent, as the client looks the
/// parent up when it reads the bind.
pub fn first_binds() -> [Bind; 2] {
    let megamap = Snapshot { key: MEGA_MAP_KEY, type_id: STRING_TYPE, value: string_value(MEGA_MAP) };
    [
        Bind {
            entity_id: UNIVERSE_ROOT_ID,
            class_crc: GAME_RULES_CLASS_CRC,
            flags: GAME_RULES_FLAGS,
            transform: Transform::IDENTITY,
            parent_id: 0,
            parent_attach_id: 0,
            components: Vec::new(),
            snapshots: Vec::new(),
        },
        Bind {
            entity_id: GAME_MODE_ENTITY_ID,
            class_crc: GAME_MODE_CLASS_CRC,
            flags: GAME_RULES_FLAGS,
            transform: Transform::IDENTITY,
            parent_id: UNIVERSE_ROOT_ID,
            parent_attach_id: ZONE_ATTACHMENT,
            components: Vec::new(),
            snapshots: vec![megamap],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-32C of a GUID with each 8-byte half reversed, as a class CRC is.
    fn class_crc(guid: &str) -> u32 {
        let bytes: Vec<u8> = (0..guid.len()).step_by(2).map(|i| u8::from_str_radix(&guid[i..i + 2], 16).unwrap()).collect();
        let reversed = bytes.chunks(8).flat_map(|half| half.iter().rev().copied());
        !reversed.fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
        })
    }

    #[test]
    fn the_game_mode_class_crc_comes_from_its_record_guid() {
        assert_eq!(class_crc("330ce5d3fb014f8287083154a3a4b78a"), GAME_MODE_CLASS_CRC);
        assert_eq!(GAME_MODE_CLASS_CRC, 2_229_804_225);
    }

    #[test]
    fn the_game_rules_class_crc_comes_from_its_guid() {
        assert_eq!(class_crc("2fc6e803de794556ae2534275e6f5a51"), GAME_RULES_CLASS_CRC);
    }

    #[test]
    fn the_mega_map_variable_is_keyed_and_typed_as_the_client_computes_it() {
        let crc32 = |text: &[u8]| {
            let step = |crc: u32, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            !text.iter().fold(!0u32, |crc, byte| (0..8).fold(crc ^ u32::from(*byte), step))
        };
        assert_eq!(crc32(b"m_megaMapName".to_ascii_lowercase().as_slice()), MEGA_MAP_KEY);
        let fnv = b"Stringchar".iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3));
        assert_eq!((fnv >> 32) as u32 ^ fnv as u32, STRING_TYPE);
    }

    #[test]
    fn the_game_mode_id_is_outside_the_ranges_the_client_reserves() {
        const { assert!(GAME_MODE_ENTITY_ID > crate::player::LAST_LOCAL_ID) };
        assert!(!(199_998_000_000..200_000_000_000).contains(&GAME_MODE_ENTITY_ID));
        assert!(GAME_MODE_ENTITY_ID < crate::player::player_geid(1_000_000).unwrap());
    }

    #[test]
    fn the_mode_hangs_from_the_root_and_names_its_map() {
        let [root, mode] = first_binds();
        assert_eq!((root.entity_id, root.parent_id), (UNIVERSE_ROOT_ID, 0));
        assert_eq!((mode.parent_id, mode.parent_attach_id), (UNIVERSE_ROOT_ID, ZONE_ATTACHMENT));
        assert_eq!(mode.snapshots[0].value, b"\x0eMegaMap.PU_All");
    }
}

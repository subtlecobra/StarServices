//! The ASOP kiosk's link to an air traffic controller (ATC), which the kiosk needs before its UI opens.
//!
//! Found on a running client (4.10.2, CL 12913470): at the first press the kiosk logs
//! `CEntityComponentShipInsuranceProvider::OnRequestOpen ... [NO ATC] ShipSelectorKiosk_Hangar_HighTech_1_a-032
//! [226553665713] - NO LINK TO ATC - Verify and export OC` and opens nothing (0x144c25cb0). The component keeps the
//! ATC's entity id in its entity variable `OwnerATCId` (component +0x9f8, registered in 0x144c2db90); 0 is "no link",
//! and a set id must name an entity with the `SCAirTrafficControllerDataManager` component that is streamed in on the
//! client (else `ATC [..] not streamed in`). The variable is only written by the kiosk's handler of the component event
//! `SEventATCDataManager_LinkWithATC` (0x144c18fc0), which an ATC entity sends from its `Register*` functions.
//! The hangar's own object containers hold no ATC, and the kiosks have no entity link to one, so on a client that makes
//! the hangar's statics itself (local entities, flag `0x10`) nothing ever sends the event. A real server spawns an
//! instanced hangar's content as replicated entities (see the RE repository's `transit/phase2_panel_authority.md`) and
//! the location's ATC links them.
//!
//! What the evidence says (4.10.2; see docs/SHIPS.md, "The kiosk's ATC link", and the RE repository's
//! `netcode/asop_ships.md`):
//!
//! - A real server's ATC (authority) runs `LoadFromXml` (0x14515b7e0, only with authority) at its `PostInitialize`
//!   (0x1451acd70), resolves the exported super GUIDs of its `ShipInsuranceProviders` and calls `RegisterInsuranceProvider`
//!   (0x1451db260), which sends `LinkWithATC` to the kiosk. The kiosk's handler writes `OwnerATCId`, a replicated
//!   variable (flags 5, serializer `Struct EntityId m_value uint64` = 0x728ae36a, read at 0x144c2db90 and 0x1406189a0).
//! - An `EntitySnapshotUpdate` does not reach a client-made static: 0x1415ad7c0 looks the id up in the replication view's
//!   bind table (0x141579740) and logs `Snapshot Update For Missing Entity` for a local entity. So the kiosk the client
//!   makes from `ocIdRange` cannot be given the variable, and no variable index is sent.
//! - A replicated kiosk is not an authority on the client, and a bind carries variables by key and type, so it can carry
//!   `OwnerATCId` with no index. This module binds, beside the client's own kiosks, a replicated copy of each at the
//!   same place, and the ATC entity they name. Which copy a press reaches shows in the client's log (the kiosk's
//!   entity id).
//!
//! Switch: the environment variable `SC_DGS_KIOSK_ATC`, on unless it is `0`, `false`, `off` or `no`.

use crate::entity::{Bind, Snapshot, Transform};
use crate::location::{hangar_content, HomeHangar};
use tracing::{info, warn};
use crate::solar_system::ZONE_ATTACHMENT;

/// CRC-32 of the lowercased variable name, as every entity variable is keyed.
pub const OWNER_ATC_ID_KEY: u32 = 0xf9df_8d76;
/// FNV-1a 64 halves xored of the entity id's serializer type (as `transit::wire`'s `ENTITY_ID_TYPE`).
pub const ENTITY_ID_TYPE: u32 = 0x728a_e36a;
/// `EntityClassDefinition.ATC_DataManager` (GUID 467cdeca-a5bc-4b29-ad21-44aed9bfce35), CRC-32C of its GUID: the class
/// of the generic ATC entity, with `SCAirTrafficControllerDataManagerParams`, comms and a restricted-area ATC component.
pub const ATC_CLASS_CRC: u32 = 1_225_794_389;
/// As the hangar's content: `AddChildToBatch`, replicated.
const ATC_FLAGS: u32 = 0x1000;

/// The ATC entity of a hangar: the first id after the hangar's content, inside its stride (`HangarIds::end`).
pub fn atc_id(hangar: &HomeHangar) -> u64 {
    hangar.ids().end
}

/// The ATC as a replicated entity under the hangar, at its origin.
pub fn atc_bind(hangar: &HomeHangar) -> Bind {
    Bind {
        entity_id: atc_id(hangar),
        class_crc: ATC_CLASS_CRC,
        flags: ATC_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: hangar.entity_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots: Vec::new(),
    }
}

/// The variable that links a kiosk (or a landing area) to the ATC `atc_id`.
pub fn owner_atc_snapshot(atc_id: u64) -> Snapshot {
    Snapshot { key: OWNER_ATC_ID_KEY, type_id: ENTITY_ID_TYPE, value: atc_id.to_le_bytes().to_vec() }
}

/// Raw bytes as hex, for logging the arguments of a call whose layout is not read yet.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `SC_DGS_KIOSK_ATC=local`: bind each kiosk at the id the client gives its own (same id, not a copy). Off by default:
/// the client makes its kiosk from the hangar's `ocIdRange` and a second entity with its id is not known to be accepted
/// (one batch refuses a duplicate id fatally, 0x1467b5750; the bind's own check, 0x1415995a0, only looks at the
/// replicated ids, and what the entity system does with a used id was not traced). It may close the client.
pub fn at_local_ids() -> bool {
    std::env::var("SC_DGS_KIOSK_ATC").is_ok_and(|setting| setting.eq_ignore_ascii_case("local"))
}

/// Whether the DGS binds the ATC and the kiosk copies (`SC_DGS_KIOSK_ATC`).
pub fn enabled() -> bool {
    enabled_by(std::env::var("SC_DGS_KIOSK_ATC").ok().as_deref())
}

fn enabled_by(setting: Option<&str>) -> bool {
    !matches!(setting.map(str::to_ascii_lowercase).as_deref(), Some("0" | "false" | "off" | "no"))
}

/// `hangar_smlfrnt_001`, the container with the ASOP kiosks of the small front hangars (209 statics).
pub const HANGAR_SMALL_FRONT_SOCPAK: u32 = 2_066_649_194;
/// `EntityClassDefinition.ShipSelectorKiosk_Hangar_HighTech_1_a` (GUID b63dfccb-b7ce-4a25-b3ac-cbcab5b62660), CRC-32C of
/// its GUID as the tables compute class CRCs.
pub const KIOSK_CLASS_CRC: u32 = 1_689_243_590;

/// A kiosk of `hangar_smlfrnt_001.soc`, from its entity XML: the place among the statics in document order (the client
/// gives it the container's first static id plus this, so `032` is the hangar's id + 177), its place in the container
/// and its rotation as the XML writes it, `w x y z`.
struct KioskSpot {
    name: &'static str,
    index: u64,
    position: [f64; 3],
    rotation_wxyz: [f64; 4],
}

#[allow(clippy::excessive_precision)] // the XML's own digits
const KIOSKS: [KioskSpot; 3] = [
    KioskSpot { name: "ShipSelectorKiosk_Hangar_HighTech_1_a-021", index: 68, position: [35.625000232947059, -35.749999810534064, -3.9018923416733742e-06], rotation_wxyz: [0.70710689, -5.4373993e-07, 2.063198e-07, 0.70710671] },
    KioskSpot { name: "ShipSelectorKiosk_Hangar_HighTech_1_a-020", index: 73, position: [-34.19594007183332, -73.432564226153772, 5.5119278840720654e-05], rotation_wxyz: [0.70710671, -2.063198e-07, -5.4373993e-07, -0.70710689] },
    KioskSpot { name: "ShipSelectorKiosk_Hangar_HighTech_1_a-032", index: 167, position: [3.8399128243327141e-05, -87.750042529543862, 0.24998114511254244], rotation_wxyz: [0.0, 0.0, 0.0, 1.0] },
];

/// The id of the replicated copy of kiosk `n` of `KIOSKS`: after the ATC, inside the hangar's stride.
pub fn kiosk_copy_id(hangar: &HomeHangar, n: usize) -> u64 {
    atc_id(hangar) + 1 + n as u64
}

/// The id the client gives its own kiosk `n` of `KIOSKS`: the first static of the container plus the kiosk's place.
fn local_kiosk_id(first_static: u64, n: usize) -> u64 {
    first_static + KIOSKS[n].index
}

/// A replicated copy of a kiosk, linked to the ATC through the variable the kiosk's handler of `LinkWithATC` would write.
fn kiosk_bind(hangar: &HomeHangar, n: usize, container: &Transform, entity_id: u64) -> Bind {
    let spot = &KIOSKS[n];
    let [w, x, y, z] = spot.rotation_wxyz;
    let pose = Transform { position: spot.position, rotation: [x, y, z, w] };
    Bind {
        entity_id,
        class_crc: KIOSK_CLASS_CRC,
        flags: ATC_FLAGS,
        transform: container.then(&pose),
        parent_id: hangar.entity_id,
        parent_attach_id: ZONE_ATTACHMENT,
        components: Vec::new(),
        snapshots: vec![owner_atc_snapshot(atc_id(hangar))],
    }
}

/// The ATC and the kiosk copies of a hangar that has `hangar_smlfrnt_001`; empty for any other, and when the switch is off.
/// The ATC goes first. Logs what it binds.
pub fn binds(hangar: &HomeHangar) -> Vec<Bind> {
    if !enabled() {
        return Vec::new();
    }
    binds_for(hangar)
}

fn binds_for(hangar: &HomeHangar) -> Vec<Bind> {
    let content = hangar_content(hangar.location.socpak_hash);
    let ids = hangar.ids();
    let Some((container, (_, container_ids))) = content.iter().zip(&ids.containers).find(|(content, _)| content.socpak_hash == HANGAR_SMALL_FRONT_SOCPAK) else {
        return Vec::new();
    };
    let mut binds = vec![atc_bind(hangar)];
    let local = at_local_ids();
    if local {
        warn!("SC_DGS_KIOSK_ATC=local: binding the kiosks at the client's own ids, which may be refused or close the client");
    }
    let id = |n: usize| if local { local_kiosk_id(container_ids.statics.0, n) } else { kiosk_copy_id(hangar, n) };
    binds.extend((0..KIOSKS.len()).map(|n| kiosk_bind(hangar, n, &container.transform, id(n))));
    info!(
        hangar = hangar.entity_id, atc = atc_id(hangar), atc_class = ATC_CLASS_CRC, owner_atc_key = format_args!("{OWNER_ATC_ID_KEY:#010x}"),
        "binding the hangar's ATC and kiosk copies (SC_DGS_KIOSK_ATC=0 turns it off)",
    );
    for (n, spot) in KIOSKS.iter().enumerate() {
        info!(
            kiosk = spot.name, copy = kiosk_copy_id(hangar, n), local = local_kiosk_id(container_ids.statics.0, n), atc = atc_id(hangar),
            "a replicated copy of the kiosk, with OwnerATCId set (the client's own kiosk is the `local` id)",
        );
    }
    binds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(text: &[u8]) -> u32 {
        !text.iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        })
    }

    #[test]
    fn the_link_variable_is_keyed_by_its_lowercased_name() {
        assert_eq!(OWNER_ATC_ID_KEY, crc32(b"owneratcid"));
    }

    #[test]
    fn the_link_carries_the_atc_id_as_an_entity_id() {
        let snapshot = owner_atc_snapshot(0x0102_0304_0506_0708);
        assert_eq!((snapshot.key, snapshot.type_id), (OWNER_ATC_ID_KEY, ENTITY_ID_TYPE));
        assert_eq!(snapshot.value, 0x0102_0304_0506_0708u64.to_le_bytes());
    }

    #[test]
    fn the_atc_is_bound_under_the_hangar_inside_its_stride() {
        let hangar = HomeHangar::new(crate::location::ORBITUARY, 0, false).expect("a home hangar");
        let bind = atc_bind(&hangar);
        assert_eq!((bind.class_crc, bind.parent_id), (ATC_CLASS_CRC, hangar.entity_id));
        assert!(bind.entity_id > hangar.ids().containers.last().map_or(0, |(id, _)| *id));
        assert!(bind.entity_id < crate::location::hangar_id(1), "the ATC is outside the hangar's block");
    }

    /// `Stanton2_Orison`, whose hangar socpak is 427479476 with `hangar_smlfrnt_001` as its first container.
    fn orison() -> HomeHangar {
        HomeHangar::new(1_105_811_199, 0, false).expect("Orison's home hangar")
    }

    #[test]
    fn bytes_log_as_hex() {
        assert_eq!(hex(&[0x01, 0xab, 0x00]), "01ab00");
    }

    #[test]
    fn the_switch_is_on_unless_turned_off() {
        assert!(enabled_by(None) && enabled_by(Some("1")) && enabled_by(Some("")));
        assert!(!enabled_by(Some("0")) && !enabled_by(Some("Off")) && !enabled_by(Some("false")) && !enabled_by(Some("NO")));
    }

    #[test]
    fn orisons_hangar_gets_the_atc_and_a_linked_copy_of_each_kiosk() {
        let hangar = orison();
        let binds = binds_for(&hangar);
        assert_eq!(binds.len(), 1 + KIOSKS.len());
        assert_eq!((binds[0].entity_id, binds[0].class_crc), (atc_id(&hangar), ATC_CLASS_CRC));
        for (n, bind) in binds[1..].iter().enumerate() {
            assert_eq!((bind.entity_id, bind.class_crc, bind.parent_id, bind.flags), (kiosk_copy_id(&hangar, n), KIOSK_CLASS_CRC, hangar.entity_id, 0x1000));
            assert_eq!(bind.snapshots.len(), 1);
            let link = &bind.snapshots[0];
            assert_eq!((link.key, link.type_id), (OWNER_ATC_ID_KEY, ENTITY_ID_TYPE));
            assert_eq!(link.value, atc_id(&hangar).to_le_bytes());
        }
        let ids: std::collections::HashSet<u64> = binds.iter().map(|bind| bind.entity_id).collect();
        assert_eq!(ids.len(), binds.len());
        assert!(binds.iter().all(|bind| bind.entity_id < crate::location::hangar_id(1)), "inside the hangar's stride");
    }

    #[test]
    fn the_client_makes_its_kiosks_at_the_ids_the_log_showed() {
        let hangar = orison();
        let content = hangar_content(hangar.location.socpak_hash);
        let ids = hangar.ids();
        let (_, (_, container_ids)) = content.iter().zip(&ids.containers).find(|(content, _)| content.socpak_hash == HANGAR_SMALL_FRONT_SOCPAK).expect("hangar_smlfrnt_001");
        // The first game test: kiosk 032 was entity 226553665713 of the hangar 226553665536.
        let hangar_of_the_log = 226_553_665_536u64;
        assert_eq!(local_kiosk_id(container_ids.statics.0, 2) - hangar.entity_id, 226_553_665_713 - hangar_of_the_log);
        assert_ne!(local_kiosk_id(container_ids.statics.0, 2), kiosk_copy_id(&hangar, 2));
    }

    #[test]
    fn a_hangar_without_the_container_binds_nothing() {
        let rest_stop = HomeHangar::new(crate::location::ORBITUARY, 0, false).expect("a home hangar");
        assert!(binds_for(&rest_stop).is_empty());
    }

    #[test]
    fn the_kiosk_copy_keeps_the_xml_rotation_as_x_y_z_w() {
        let hangar = orison();
        let bind = kiosk_bind(&hangar, 0, &Transform::IDENTITY, kiosk_copy_id(&hangar, 0));
        assert_eq!(bind.transform.rotation, [-5.4373993e-07, 2.063198e-07, 0.70710671, 0.70710689]);
        assert_eq!(bind.transform.position, KIOSKS[0].position);
    }
}

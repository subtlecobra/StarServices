//! The character a player made in the frontend's character creator, put on the entities the DGS binds. See "Body and
//! loadout" in the protocol notes (kept outside this repository).
//!
//! The client saves it with `SaveCharacterCustomizations`, which `sc-character-customizer` stores as it came, a
//! `CharacterCustomization` per character GEID. That service owns them: the DGS asks it, over
//! `CharacterCustomizerInternal.GetCustomization`, at each join. A real save does two more things, read from the client's own server code
//! (0x142aa8f00, which the frontend runs as its local server): it sets four properties on the player entity in the entity
//! graph, `GeomResourceModelTag`, `dnaData`, `HasCustomizedCharacter` and `customMaterialParams`, and it persists the
//! head's items (`CharacterCustomizer -> Persist Loadout`). The first three of those properties are serialized
//! variables of the `Player` class (`CSCActorDNAComponent`, 0x14355b210, and the geometry's model tag), so a bind carries
//! them as snapshots; `HasCustomizedCharacter` is not one of the player's 1076 variables and is not sent.
//!
//! There is one head class, `PU_Protos_Head`, and one body, for both genders: the model tag picks the male or female
//! geometry. A player with nothing saved keeps the default of `loadout`, and no snapshot is added.

use sc_api::internal::{character_customizer_internal_client::CharacterCustomizerInternalClient, GetCustomizationRequest};
use sc_proto::{
    common::types::v1::{entity_class_loadout::EntityClassLoadoutParams, CharacterCustomization},
    identity::Gender,
};
use tonic::transport::Channel;
use tracing::warn;

use crate::{
    entity::Snapshot,
    loadout::{class_crc, Item},
};

/// CRC-32 of the lowercased `GeomResourceModelTag`. Index 259 of the player's table in this build.
const MODEL_TAG_KEY: u32 = 0x3d92_ca35;
/// The model tag's serializer: 4 bytes, the `u32` of the `STag` after its index (0x142972e70 writes `+0xc` of the
/// variable, 0x142347670 the property). Type read from the running client (0x149e3ada0, set by 0x140476150).
const MODEL_TAG_TYPE: u32 = 0x6414_f741;
/// `ItemPorts.ActorModelTag.Male` (25f439d5-146b-4a61-a999-a486dfb68a49) and `.Female`
/// (d0794a94-efb0-4cad-ad38-2558b4d3c253), which 0x142ac5630 looks up for the two genders. A tag's `u32` is CRC-32C of
/// its GUID written in lowercase, read from the client's tag database, where both are.
const MALE_MODEL_TAG: u32 = 0x0bbd_d789;
const FEMALE_MODEL_TAG: u32 = 0x6630_9ee9;
/// CRC-32 of the lowercased `dnaData`. Index 1011 of the player's table in this build.
const DNA_KEY: u32 = 0xf88e_ab20;
/// `Sequence<uint8>`: the halves of FNV-1a 64 of `Sequenceuint8`, xored, as 0x1414770c0 computes it (read live as well).
/// The value is a varint count and the bytes (0x141a42b40, 0x141a10510).
const BYTES_TYPE: u32 = 0xd1bb_93a0;
/// CRC-32 of the lowercased `customMaterialParams`. Index 475 of the player's table in this build.
const MATERIAL_KEY: u32 = 0x765f_40f4;
/// `Sequence` of the material parameter entries, read from the running client (0x149ff1d78, set by 0x140f866c0).
const MATERIAL_TYPE: u32 = 0x060d_5d2c;
/// The slot of `PU_Protos_Head` in `DEFAULT_LOADOUT`, where an item of a port the default has not goes.
const HEAD_SLOT: usize = 1;

/// What a player looks like, as the character creator saved it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Appearance {
    gender: Option<Gender>,
    dna: Vec<u8>,
    /// As the client wrote it: a varint count of entries, each a GUID, a `u32` attachment name CRC and shader
    /// parameters, every value on a byte.
    material_params: Vec<u8>,
    /// The head's items the player chose, each a port and a class GUID, with the items in its own ports.
    items: Vec<EntityClassLoadoutParams>,
}

impl From<CharacterCustomization> for Appearance {
    fn from(saved: CharacterCustomization) -> Self {
        let gender = match Gender::try_from(saved.gender) {
            Ok(Gender::Unspecified) | Err(_) => None,
            Ok(gender) => Some(gender),
        };
        let items = saved.customizations.map(|loadout| loadout.params).unwrap_or_default();
        Self { gender, dna: saved.dna_matrix, material_params: saved.custom_material_params, items }
    }
}

impl Appearance {
    /// The variables of the player that carry it, for its bind. A part that was not saved is left to the class.
    pub fn player_snapshots(&self) -> Vec<Snapshot> {
        let mut snapshots = Vec::new();
        let tag = match self.gender {
            Some(Gender::Female) => Some(FEMALE_MODEL_TAG),
            Some(_) => Some(MALE_MODEL_TAG),
            None => None,
        };
        if let Some(tag) = tag {
            snapshots.push(Snapshot { key: MODEL_TAG_KEY, type_id: MODEL_TAG_TYPE, value: tag.to_le_bytes().to_vec() });
        }
        if !self.dna.is_empty() {
            let mut value = Vec::with_capacity(self.dna.len() + 3);
            varint(self.dna.len(), &mut value);
            value.extend_from_slice(&self.dna);
            snapshots.push(Snapshot { key: DNA_KEY, type_id: BYTES_TYPE, value });
        }
        if !self.material_params.is_empty() {
            snapshots.push(Snapshot { key: MATERIAL_KEY, type_id: MATERIAL_TYPE, value: self.material_params.clone() });
        }
        snapshots
    }

    /// `default` with the items the player chose: an item in the same port of the same parent changes class and keeps
    /// what is in its own ports, as on a real server, where a customized player's new eyes still hold the default lens
    /// and its radar. A port the default has not gets a new item, under the head for a port at the top of the tree.
    /// An entry whose GUID does not parse is skipped.
    pub fn items(&self, default: &[Item]) -> Vec<Item> {
        let mut items = default.to_vec();
        place(&mut items, None, &self.items);
        items
    }
}

fn place(items: &mut Vec<Item>, parent: Option<usize>, chosen: &[EntityClassLoadoutParams]) {
    for choice in chosen {
        let Some(crc) = class_crc(&choice.entity_class_guid) else {
            warn!(guid = %choice.entity_class_guid, port = %choice.port_name, "a saved item whose class is not a GUID");
            continue;
        };
        let same_port = |item: &Item| item.port.eq_ignore_ascii_case(&choice.port_name);
        // At the top of the tree the parent is not named: the port is looked for anywhere, the head's first.
        let found = match parent {
            Some(parent) => items.iter().position(|item| item.parent == Some(parent) && same_port(item)),
            None => items.iter().position(|item| item.parent == Some(HEAD_SLOT) && same_port(item)).or_else(|| items.iter().position(same_port)),
        };
        let slot = match found {
            Some(slot) => {
                if items[slot].class_crc != crc {
                    items[slot].class_crc = crc;
                    items[slot].class_name = choice.entity_class_guid.to_ascii_lowercase().into();
                }
                slot
            }
            None => {
                items.push(Item {
                    class_name: choice.entity_class_guid.to_ascii_lowercase().into(),
                    class_crc: crc,
                    port: choice.port_name.clone().into(),
                    parent: Some(parent.unwrap_or(HEAD_SLOT)),
                });
                items.len() - 1
            }
        };
        place(items, Some(slot), &choice.params);
    }
}

fn varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// The saved appearance of the character `geid`, `None` when it has none or the customizer cannot say. A join never fails
/// for this: the player then wears the default.
pub async fn load(customizer: &mut CharacterCustomizerInternalClient<Channel>, geid: u64) -> Option<Appearance> {
    match customizer.get_customization(GetCustomizationRequest { geid }).await {
        Ok(reply) => reply.into_inner().customization.map(Appearance::from),
        Err(status) => {
            warn!(code = ?status.code(), message = status.message(), geid, "saved character not had, using the default");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use sc_proto::common::types::v1::EntityClassLoadout;

    use super::*;
    use crate::loadout::DEFAULT_LOADOUT;

    const WHITE_EYES: &str = "6b4ca363-e160-4871-b709-e47467b40310";

    fn choice(port: &str, guid: &str, params: Vec<EntityClassLoadoutParams>) -> EntityClassLoadoutParams {
        EntityClassLoadoutParams { port_name: port.into(), entity_class_guid: guid.into(), params }
    }

    fn saved(gender: Gender, items: Vec<EntityClassLoadoutParams>) -> Appearance {
        Appearance::from(CharacterCustomization {
            gender: gender as i32,
            race: 1,
            generation: String::new(),
            dna_matrix: vec![7; 200],
            customizations: Some(EntityClassLoadout { params: items }),
            custom_material_params: vec![1, 2, 3],
        })
    }

    #[test]
    fn the_model_tags_are_the_crc32c_of_the_tag_guids() {
        let crc32c = |text: &str| {
            !text.bytes().fold(!0u32, |crc, byte| {
                (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
            })
        };
        assert_eq!(crc32c("25f439d5-146b-4a61-a999-a486dfb68a49"), MALE_MODEL_TAG);
        assert_eq!(crc32c("d0794a94-efb0-4cad-ad38-2558b4d3c253"), FEMALE_MODEL_TAG);
        // The tag database's first entry, `Global`, read live with its hash.
        assert_eq!(crc32c("1ca2768d-33ab-4583-97ac-17afcd0dafb0"), 0xff1a_8182);
    }

    #[test]
    fn the_keys_are_the_crc32_of_the_lowercased_names() {
        let crc32 = |text: &str| {
            !text.bytes().fold(!0u32, |crc, byte| {
                (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
            })
        };
        assert_eq!(crc32("geomresourcemodeltag"), MODEL_TAG_KEY);
        assert_eq!(crc32("dnadata"), DNA_KEY);
        assert_eq!(crc32("custommaterialparams"), MATERIAL_KEY);
    }

    #[test]
    fn the_type_of_the_dna_is_the_folded_fnv_of_its_name() {
        let hash = b"Sequenceuint8".iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3));
        assert_eq!((hash >> 32) as u32 ^ hash as u32, BYTES_TYPE);
    }

    #[test]
    fn a_saved_character_gives_the_player_its_tag_dna_and_materials() {
        let snapshots = saved(Gender::Female, Vec::new()).player_snapshots();
        let keys: Vec<(u32, u32)> = snapshots.iter().map(|snapshot| (snapshot.key, snapshot.type_id)).collect();
        assert_eq!(keys, [(MODEL_TAG_KEY, MODEL_TAG_TYPE), (DNA_KEY, BYTES_TYPE), (MATERIAL_KEY, MATERIAL_TYPE)]);
        assert_eq!(snapshots[0].value, FEMALE_MODEL_TAG.to_le_bytes());
        assert_eq!(&snapshots[1].value[..3], [0xc8, 0x01, 7], "a varint count of 200, then the bytes");
        assert_eq!(snapshots[1].value.len(), 202);
        assert_eq!(snapshots[2].value, [1, 2, 3], "as the client wrote them");
        assert_eq!(saved(Gender::Male, Vec::new()).player_snapshots()[0].value, MALE_MODEL_TAG.to_le_bytes());
    }

    #[test]
    fn nothing_saved_is_nothing_sent() {
        assert!(Appearance::default().player_snapshots().is_empty());
        let unspecified = Appearance::from(CharacterCustomization { gender: 0, ..Default::default() });
        assert!(unspecified.player_snapshots().is_empty());
        assert_eq!(unspecified.items(&DEFAULT_LOADOUT), DEFAULT_LOADOUT.to_vec());
    }

    /// A customizer holding one saved character, at an address of its own.
    async fn customizer(saved_geid: u64) -> CharacterCustomizerInternalClient<Channel> {
        use sc_api::internal::{
            character_customizer_internal_server::{CharacterCustomizerInternal, CharacterCustomizerInternalServer},
            GetCustomizationResponse,
        };
        use tonic::{transport::{server::TcpIncoming, Server}, Request, Response, Status};

        struct Saved(u64);
        #[tonic::async_trait]
        impl CharacterCustomizerInternal for Saved {
            async fn get_customization(&self, request: Request<GetCustomizationRequest>) -> Result<Response<GetCustomizationResponse>, Status> {
                let customization = (request.into_inner().geid == self.0)
                    .then(|| CharacterCustomization { gender: Gender::Female as i32, dna_matrix: vec![1], ..Default::default() });
                Ok(Response::new(GetCustomizationResponse { customization }))
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = Server::builder().add_service(CharacterCustomizerInternalServer::new(Saved(saved_geid)));
        tokio::spawn(server.serve_with_incoming(TcpIncoming::from(listener)));
        CharacterCustomizerInternalClient::new(tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap().connect_lazy())
    }

    #[tokio::test]
    async fn the_customizer_gives_a_saved_character_and_none_for_others() {
        let mut client = customizer(300_000_000_007).await;
        let appearance = load(&mut client, 300_000_000_007).await.unwrap();
        assert_eq!((appearance.gender, appearance.dna), (Some(Gender::Female), vec![1]));
        assert_eq!(load(&mut client, 300_000_000_008).await, None);
    }

    #[tokio::test]
    async fn a_customizer_that_is_not_there_gives_the_default() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap();
        let mut client = CharacterCustomizerInternalClient::new(endpoint.connect_lazy());
        assert_eq!(load(&mut client, 300_000_000_007).await, None);
    }

    #[test]
    fn chosen_eyes_replace_the_default_and_keep_the_lens() {
        let items = saved(Gender::Male, vec![choice("Eyes_ItemPort", WHITE_EYES, Vec::new())]).items(&DEFAULT_LOADOUT);
        assert_eq!(items.len(), DEFAULT_LOADOUT.len());
        assert_eq!(items[3].class_crc, class_crc(WHITE_EYES).unwrap());
        assert_eq!((items[3].port.as_ref(), items[3].parent), ("Eyes_ItemPort", Some(1)));
        assert_eq!(items[4], DEFAULT_LOADOUT[4], "the lens stays in the eyes");
    }

    #[test]
    fn a_port_the_default_has_not_gets_a_new_item_on_the_head() {
        let beard = "11111111-2222-3333-4444-555555555555";
        let items = saved(Gender::Male, vec![choice("Beard_ItemPort", beard, Vec::new())]).items(&DEFAULT_LOADOUT);
        assert_eq!(items.len(), DEFAULT_LOADOUT.len() + 1);
        let added = items.last().unwrap();
        assert_eq!((added.port.as_ref(), added.parent, added.class_crc), ("Beard_ItemPort", Some(1), class_crc(beard).unwrap()));
    }

    #[test]
    fn a_tree_from_the_head_port_down_matches_level_by_level() {
        let head = "1d5cfab3-bf80-4550-b4ab-39e896a7086e";
        let tree = vec![choice("Head_ItemPort", head, vec![choice("eyes_itemport", WHITE_EYES, Vec::new()), choice("Hair_ItemPort", "zz", Vec::new())])];
        let items = saved(Gender::Female, tree).items(&DEFAULT_LOADOUT);
        assert_eq!(items.len(), DEFAULT_LOADOUT.len());
        assert_eq!(items[1], DEFAULT_LOADOUT[1], "the same head class changes nothing");
        assert_eq!(items[3].class_crc, class_crc(WHITE_EYES).unwrap(), "ports match whatever their case");
        assert_eq!(items[6], DEFAULT_LOADOUT[6], "a class that is not a GUID is skipped");
    }

    #[test]
    fn a_join_wearing_a_saved_character_adds_to_the_default_binds() {
        use crate::control::{player_entities, player_entities_wearing};
        let plain = player_entities(1_000_000, None, true).unwrap();
        let beard = "11111111-2222-3333-4444-555555555555";
        let appearance = saved(Gender::Female, vec![choice("Beard_ItemPort", beard, Vec::new())]);
        let wearing = player_entities_wearing(1_000_000, None, true, Some(&appearance)).unwrap();
        assert_eq!(wearing.player.snapshots.len(), plain.player.snapshots.len() + 3);
        assert_eq!(wearing.player.snapshots.last().unwrap().key, MATERIAL_KEY);
        assert_eq!(wearing.items.len(), plain.items.len() + 1);
        let added = wearing.items.last().unwrap();
        assert_eq!((added.parent_id, added.class_crc), (plain.items[1].entity_id, class_crc(beard).unwrap()));
        assert_eq!(added.entity_id, plain.items[22].entity_id + 1);
    }
}

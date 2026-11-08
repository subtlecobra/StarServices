//! A client's interactions with entities the DGS holds, such as a press of a hangar elevator's call panel.
//!
//! An entity the client has no authority over sends an interaction to its authority: `CInteractableComponent`'s
//! `SvRequestInteract`. Its remote methods are registered in a row (`PostInitialize`, 0x1429284f0), so their indices
//! depend on what the entity registered before them; on `TransitInteractionUIPanel_Elevator_Ext` they are 10 for the
//! request and 11 for `ClOnInteract`, the answer. The authority runs the interaction (`OnInteractImpl`, 0x142916590)
//! and, only when it succeeds, sends `ClOnInteract` with the request's own arguments to the requesting client alone
//! (0x142859a10), which then runs the interaction's callbacks on its side. A refused interaction gets no answer, and the
//! client waits for none. See the RE repository's `docs/transit/phase2_interact_answer.md`.

/// `SvRequestInteract` on a transit call panel.
pub const PANEL_REQUEST_INTERACT: u32 = 10;
/// `ClOnInteract` on a transit call panel.
pub const PANEL_ON_INTERACT: u32 = 11;
/// CRC-32 of `callelevator`, the call panel's interaction, as the client hashes an interaction's lowercased name.
pub const CALL_ELEVATOR: u32 = 0xb275_937b;

/// The arguments of `SvRequestInteract` and `ClOnInteract` (writer 0x1429aefc0, reader 0x142939e40): a `CTimeValue`,
/// the request id, the interaction point's and the interaction's name hashes, the user, an `SInteractionMetaData`, then
/// the interactable's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interaction {
    pub request_id: u64,
    pub point: u32,
    pub interaction: u32,
    pub user: u64,
    pub interactable: u64,
}

/// `SInteractionMetaData` (0x1427d37b0): a varint `interactorDataType`, a `u32`, a varint `interactableDataType`, a
/// `u32`, then a byte of two bits. Its fields are not used; only its length is needed.
fn metadata_len(bytes: &[u8]) -> Option<usize> {
    let mut offset = 0;
    for field in 0..4 {
        if field % 2 == 0 {
            while *bytes.get(offset)? & 0x80 != 0 {
                offset += 1;
            }
            offset += 1;
        } else {
            offset += 4;
        }
    }
    (offset < bytes.len()).then_some(offset + 1)
}

impl Interaction {
    /// `None` for arguments that are not an interaction's.
    pub fn parse(args: &[u8]) -> Option<Self> {
        let u64_at = |offset: usize| Some(u64::from_le_bytes(args.get(offset..offset + 8)?.try_into().ok()?));
        let u32_at = |offset: usize| Some(u32::from_le_bytes(args.get(offset..offset + 4)?.try_into().ok()?));
        let metadata = 32;
        let interactable = metadata + metadata_len(args.get(metadata..)?)?;
        if args.len() != interactable + 8 {
            return None;
        }
        Some(Self { request_id: u64_at(8)?, point: u32_at(16)?, interaction: u32_at(20)?, user: u64_at(24)?, interactable: u64_at(interactable)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press of "Call elevator" on the panel H+246 of a running client's hangar, by its player.
    const PRESS: &str = "0da52c4dc1bd3f0001000000000000007e34bf427b9375b2a13e66d9450000000100000000000000000000f698a6bf34000000";

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
    }

    #[test]
    fn a_press_of_the_call_panel_is_read() {
        let press = Interaction::parse(&bytes(PRESS)).unwrap();
        assert_eq!(press, Interaction { request_id: 1, point: 0x42bf_347e, interaction: CALL_ELEVATOR, user: 0x45_d966_3ea1, interactable: 0x34_bfa6_98f6 });
    }

    #[test]
    fn what_is_not_an_interaction_is_not_read() {
        let press = bytes(PRESS);
        assert_eq!(Interaction::parse(&press[..press.len() - 1]), None);
        assert_eq!(Interaction::parse(&[press.clone(), vec![0]].concat()), None);
        assert_eq!(Interaction::parse(&press[..20]), None);
    }

    #[test]
    fn call_elevator_is_the_crc_of_its_lowercased_name() {
        let crc = !b"callelevator".iter().fold(!0u32, |crc, byte| {
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 })
        });
        assert_eq!(crc, CALL_ELEVATOR);
    }
}

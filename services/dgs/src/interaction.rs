//! A client's interactions with entities the DGS holds, such as a press of a hangar elevator's call panel.
//!
//! An entity the client has no authority over sends an interaction to its authority: `CInteractableComponent`'s
//! `SvRequestInteract`. Its remote methods are registered in a row (`PostInitialize`), so their indices
//! depend on what the entity registered before them; on `TransitInteractionUIPanel_Elevator_Ext` they are 10 for the
//! request and 11 for `ClOnInteract`, the answer. The authority runs the interaction (`OnInteractImpl`)
//! and, only when it succeeds, sends `ClOnInteract` with the request's own arguments to the requesting client alone
//!, which then runs the interaction's callbacks on its side. A refused interaction gets no answer, and the
//! client waits for none.

/// `SvRequestInteract` on a transit call panel.
pub const PANEL_REQUEST_INTERACT: u32 = 10;
/// `ClOnInteract` on a transit call panel.
pub const PANEL_ON_INTERACT: u32 = 11;
/// `SvRequestInteract` and `ClOnInteract` on an ASOP kiosk: `CInteractableComponent`'s methods 39 and 40, which the
/// component registers in a row (`PostInitialize`) after 38 others.
pub const KIOSK_REQUEST_INTERACT: u32 = 39;
pub const KIOSK_ON_INTERACT: u32 = 40;
/// CRC-32 of `callelevator`, the call panel's interaction, as the client hashes an interaction's lowercased name.
pub const CALL_ELEVATOR: u32 = 0xb275_937b;

/// The arguments of `SvRequestInteract` and `ClOnInteract`: a `CTimeValue`,
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

/// `SInteractionMetaData`: a varint `interactorDataType`, a `u32`, a varint `interactableDataType`, a
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

/// Where a cab panel's press has its user: it carries the interaction point and no interaction-name hash (the point is the
/// panel's `requestStop` / `requestDoorOpen` row), so the user follows the point at once and the metadata starts 4 bytes
/// earlier than in a call panel's `SvRequestInteract`.
const CAB_USER_OFFSET: usize = 20;
/// `SvRequestInteract` on a carriage's cab panel (`TransitInteractionUIPanel_Elevator_Int`), as the client sends it.
pub const CAB_PANEL_REQUEST_INTERACT: u32 = 14;

impl Interaction {
    /// A press on a cab panel (47 bytes: a `CTimeValue`, the request id, the point, the user, the 11 bytes of metadata and
    /// the interactable's id, which the client leaves 0 here). `interaction` is 0 and `interactable` is as sent.
    /// `None` for arguments that are not one.
    pub fn parse_cab(args: &[u8]) -> Option<Self> {
        let u64_at = |offset: usize| {
            Some(u64::from_le_bytes(
                args.get(offset..offset + 8)?.try_into().ok()?,
            ))
        };
        let metadata = CAB_USER_OFFSET + 8;
        let interactable = metadata + metadata_len(args.get(metadata..)?)?;
        if args.len() != interactable + 8 {
            return None;
        }
        Some(Self {
            request_id: u64_at(8)?,
            point: u32::from_le_bytes(args.get(16..20)?.try_into().ok()?),
            interaction: 0,
            user: u64_at(CAB_USER_OFFSET)?,
            interactable: u64_at(interactable)?,
        })
    }

    /// `None` for arguments that are not an interaction's.
    pub fn parse(args: &[u8]) -> Option<Self> {
        let u64_at = |offset: usize| {
            Some(u64::from_le_bytes(
                args.get(offset..offset + 8)?.try_into().ok()?,
            ))
        };
        let u32_at = |offset: usize| {
            Some(u32::from_le_bytes(
                args.get(offset..offset + 4)?.try_into().ok()?,
            ))
        };
        let metadata = 32;
        let interactable = metadata + metadata_len(args.get(metadata..)?)?;
        if args.len() != interactable + 8 {
            return None;
        }
        Some(Self {
            request_id: u64_at(8)?,
            point: u32_at(16)?,
            interaction: u32_at(20)?,
            user: u64_at(24)?,
            interactable: u64_at(interactable)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A press of "Call elevator" on the panel H+246 of a hangar, by its player.
    const PRESS: &str = "0da52c4dc1bd3f0001000000000000007e34bf427b9375b2a13e66d9450000000100000000000000000000f698a6bf34000000";

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_press_of_the_call_panel_is_read() {
        let press = Interaction::parse(&bytes(PRESS)).unwrap();
        assert_eq!(
            press,
            Interaction {
                request_id: 1,
                point: 0x42bf_347e,
                interaction: CALL_ELEVATOR,
                user: 0x45_d966_3ea1,
                interactable: 0x34_bfa6_98f6
            }
        );
    }

    /// A press on the cab panel 230000000063 (method 14), on the second row's point.
    const CAB_PRESS: &str = "13411bf361c23f0002000000000000005024adb8a13e66d94500000000000000000000000000000000000000000000";

    #[test]
    fn a_press_of_the_cab_panel_is_read() {
        let press = Interaction::parse_cab(&bytes(CAB_PRESS)).unwrap();
        assert_eq!(
            (press.request_id, press.point, press.user),
            (2, 0xb8ad_2450, 0x45_d966_3ea1)
        );
        assert_eq!(Interaction::parse(&bytes(CAB_PRESS)), None);
        assert_eq!(Interaction::parse_cab(&bytes(PRESS)), None);
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
            (0..8).fold(crc ^ u32::from(*byte), |crc, _| {
                if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                }
            })
        });
        assert_eq!(crc, CALL_ELEVATOR);
    }
}

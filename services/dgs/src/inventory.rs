//! A player's inventory requests: the moves, equips and stores its client asks for.
//!
//! The client queues each request (`CSCPlayerInventory::RequestInventoryManagementOperation`) and sends the next one only
//! after the first is answered. Only a query it runs itself; every other request goes to the authority of its player as the
//! remote method `pi.RmAuthorityProcessInventoryManagementOperation`, which answers the client with
//! `pi.RmTokenMoveRequestCompleted` (result, request id, item). Without that answer the request stays "pending" for ever
//! (`Inventory Mgmt Request Queued ... RequestInProgress[1]`), and so does every request behind it.
//!
//! The methods' indices are the order the player entity's components registered them in, which is not known for the
//! player (the registrar is 0x1444b4220 of 4.10.2). Within the inventory's methods the order is fixed: the process method is
//! the second and the completion the twelfth, so the answer is ten methods after the request, whatever the player's base.
//! The request is recognised by its shape instead: a request id, the type as a varint, then the player's own id.

/// Methods between `RmAuthorityProcessInventoryManagementOperation` and `RmTokenMoveRequestCompleted` in the registrar.
pub const COMPLETED_AFTER_PROCESS: u32 = 10;

/// The first fields of an `SInventoryManagementRequest`, as its bit stream writer (0x1442be760) puts them: the request id
/// (a `u32`), the type (a signed varint), then the player's entity id (a `u64`), each starting on a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub request_id: u32,
    pub kind: i32,
    pub player: u64,
}

/// `EInventoryManagementRequestType` values.
pub const KIND_QUERY_INVENTORY: i32 = 10;
pub const KIND_INVALID: i32 = 14;

impl Request {
    /// `None` for arguments that are not a request of `player`, or that are too short to be one.
    pub fn parse(args: &[u8], player: u64) -> Option<Self> {
        let request_id = u32::from_le_bytes(args.get(..4)?.try_into().ok()?);
        let (mut value, mut shift, mut at) = (0i64, 0u32, 4usize);
        loop {
            let byte = *args.get(at)?;
            value |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            at += 1;
            if byte & 0x80 == 0 {
                if shift < 63 && byte & 0x40 != 0 {
                    value |= -1i64 << shift;
                }
                break;
            }
            if shift > 28 {
                return None;
            }
        }
        let found = u64::from_le_bytes(args.get(at..at + 8)?.try_into().ok()?);
        let kind = i32::try_from(value).ok()?;
        // The request is written for its own player, and has more behind it: two inventory ids at least.
        (found == player && request_id != 0 && (0..=KIND_INVALID).contains(&kind) && args.len() >= at + 8 + 2 * 17).then_some(Self {
            request_id,
            kind,
            player,
        })
    }

    /// Whether the DGS completes the request with success. An invalid one fails, and so does a query, which a client runs
    /// itself and never sends.
    pub fn succeeds(&self) -> bool {
        self.kind != KIND_INVALID && self.kind != KIND_QUERY_INVENTORY
    }
}

/// The arguments of `RmTokenMoveRequestCompleted(bool result, u32 request_id, EntityId item)`: a bit, then the others each
/// on the next byte (0x1445445d0).
pub fn completed_args(result: bool, request_id: u32, item: u64) -> Vec<u8> {
    let mut args = vec![u8::from(result)];
    args.extend(request_id.to_le_bytes());
    args.extend(item.to_le_bytes());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER: u64 = 300_000_100_001;

    fn request(id: u32, kind: u8, player: u64) -> Vec<u8> {
        let mut args = id.to_le_bytes().to_vec();
        args.push(kind);
        args.extend(player.to_le_bytes());
        // Source and target inventory ids: holder, context, subject.
        for _ in 0..2 {
            args.extend(PLAYER.to_le_bytes());
            args.push(2);
            args.extend(1_105_811_199u64.to_le_bytes());
        }
        args.extend([0; 12]);
        args
    }

    #[test]
    fn a_request_of_the_player_is_read() {
        assert_eq!(Request::parse(&request(3, 7, PLAYER), PLAYER), Some(Request { request_id: 3, kind: 7, player: PLAYER }));
        assert!(Request::parse(&request(2, 1, PLAYER), PLAYER).unwrap().succeeds());
    }

    #[test]
    fn what_is_not_a_request_of_the_player_is_not_read() {
        assert_eq!(Request::parse(&request(3, 7, PLAYER + 1), PLAYER), None);
        assert_eq!(Request::parse(&request(0, 7, PLAYER), PLAYER), None);
        assert_eq!(Request::parse(&request(3, 15, PLAYER), PLAYER), None);
        assert_eq!(Request::parse(&request(3, 7, PLAYER)[..20], PLAYER), None);
    }

    #[test]
    fn an_invalid_request_fails() {
        assert!(!Request::parse(&request(4, 14, PLAYER), PLAYER).unwrap().succeeds());
    }

    #[test]
    fn the_completion_is_a_bit_then_the_request_and_the_item() {
        let args = completed_args(true, 0x0403_0201, 0x1122_3344_5566_7788);
        assert_eq!(args, [1, 1, 2, 3, 4, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(completed_args(false, 2, 0)[0], 0);
    }
}

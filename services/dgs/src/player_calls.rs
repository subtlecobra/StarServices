//! The calls a client makes on its own player entity that are not inventory requests nor action frames.
//!
//! Every one of these is **fire and forget**. The official server sends no `EntityRemoteMethodFrom` on the player entity in answer to any of them: the only server calls on the player are the ship list (156), the claim (158) and the
//! inventory completions (113, 114), which answer other requests. So the DGS accepts them, reads them and keeps what the
//! rest of the DGS may want to know (the inventory the client has open, the spectator flag), and sends nothing. Before this
//! module they went to the inventory parser and were logged as "not an inventory request"; the result for the client was
//! the same (no answer), but a malformed call could not be told from an unknown one.
//!
//! The calls (method index: arguments, as sent by the client):
//!
//! | method | name | arguments | example bytes |
//! |---|---|---|---|
//! | 26 | `journal.rmAuthEntriesViewed` | array: varint count, then the entries | `00` (an empty array, once) |
//! | 54 | `AuthoritySendAISignal` | string (varint length, bytes), entity id u64 | `0a 4f6e4c6f6f6b65644174 a514cee5d7000000` = `OnLookedAt` and the target; also `OnStaredAt` and `OnLookedAway` (24 calls) |
//! | 173 | `Insurance::RmAuthorityCleanUserClaimChains` (assumed) | none | (once, in the login burst) |
//! | 177 | `RmAuthorityRequestSpectatorMode` (assumed) | bool | `01` before an inventory is opened and `00` after it is closed (4 calls) |
//! | 192 | `pi.RmRequestOpenInventory` | entity id u64: the inventory | `1d85ed5a4d000000` = the player's own id |
//! | 193 | `pi.RmRequestCloseInventory` | entity id u64 | `1d85ed5a4d000000` |
//! | 201 | `AuthorityRestoreMarkerState` (assumed) | none | (once, in the login burst) |
//! | 255 | `pml.AuthClientInitializationFinished` (assumed) | none | 25 calls over the session |
//! | 263 | `rmAuthorityReportPersonalReputationData` (assumed) | none | 4 calls |
//!
//! What is not verified, and so not done: whether the stock server writes a replicated variable of the player or of the
//! inventory in answer to 177, 192 or 193 (a variable update has no method index, so it cannot be matched to these calls
//! without the player's slot map); the effect of 173 and 201 on the server.

use std::collections::HashMap;
use std::sync::Mutex;

use tracing::{debug, info, warn};

/// The method indices on the player entity, as the client sends them.
pub const METHOD_JOURNAL_ENTRIES_VIEWED: u32 = 26;
pub const METHOD_AI_SIGNAL: u32 = 54;
pub const METHOD_CLEAN_USER_CLAIM_CHAINS: u32 = 173;
pub const METHOD_SPECTATOR_MODE: u32 = 177;
pub const METHOD_OPEN_INVENTORY: u32 = 192;
pub const METHOD_CLOSE_INVENTORY: u32 = 193;
pub const METHOD_RESTORE_MARKER_STATE: u32 = 201;
pub const METHOD_CLIENT_INITIALIZATION_FINISHED: u32 = 255;
pub const METHOD_REPORT_REPUTATION: u32 = 263;

/// An entity id on the wire: eight bytes, little endian.
const ENTITY_ID_LEN: usize = 8;
/// The most bytes of an AI signal's name a call may carry (the known ones are 10 to 12).
const AI_SIGNAL_NAME_LIMIT: usize = 64;

/// A call on the player entity that needs no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerCall {
    /// `pi.RmRequestOpenInventory(EntityId)`: the container UI of this inventory is open on the client.
    OpenInventory(u64),
    /// `pi.RmRequestCloseInventory(EntityId)`.
    CloseInventory(u64),
    /// `RmAuthorityRequestSpectatorMode(bool)`.
    SpectatorMode(bool),
    /// An AI signal the player's actor raises about another entity (`OnLookedAt`, `OnStaredAt`, `OnLookedAway`).
    AiSignal { name: String, target: u64 },
    /// `journal.rmAuthEntriesViewed(array)`: the journal entries the client has read; the array's bytes after its count.
    JournalEntriesViewed { count: u64, entries: Vec<u8> },
    CleanUserClaimChains,
    RestoreMarkerState,
    ClientInitializationFinished,
    ReportPersonalReputationData,
}

/// What `PlayerCall::parse` makes of a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A call of this module that read as its layout.
    Call(PlayerCall),
    /// A method of this module whose arguments are not its layout: logged, never applied.
    Malformed(u32),
    /// Not a method of this module: the caller goes on to the inventory requests.
    Other,
}

struct Reader<'a> {
    args: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u64(&mut self) -> Option<u64> {
        let bytes = self.args.get(self.at..self.at + ENTITY_ID_LEN)?;
        self.at += ENTITY_ID_LEN;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// A varint of up to ten bytes (LEB128, as the protocol writes its counts and lengths).
    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = *self.args.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let bytes = self.args.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        Some(bytes)
    }

    fn done(&self) -> bool {
        self.at == self.args.len()
    }
}

impl PlayerCall {
    pub fn parse(method: u32, args: &[u8]) -> Parsed {
        let none = |call: PlayerCall| {
            if args.is_empty() {
                Parsed::Call(call)
            } else {
                Parsed::Malformed(method)
            }
        };
        match method {
            METHOD_OPEN_INVENTORY | METHOD_CLOSE_INVENTORY => {
                let mut reader = Reader { args, at: 0 };
                match reader.u64().filter(|_| reader.done()) {
                    Some(id) if method == METHOD_OPEN_INVENTORY => {
                        Parsed::Call(Self::OpenInventory(id))
                    }
                    Some(id) => Parsed::Call(Self::CloseInventory(id)),
                    None => Parsed::Malformed(method),
                }
            }
            METHOD_SPECTATOR_MODE => match args {
                [flag @ (0 | 1)] => Parsed::Call(Self::SpectatorMode(*flag == 1)),
                _ => Parsed::Malformed(method),
            },
            METHOD_AI_SIGNAL => {
                let mut reader = Reader { args, at: 0 };
                let parsed = (|| {
                    let len = usize::try_from(reader.varint()?).ok()?;
                    if len > AI_SIGNAL_NAME_LIMIT {
                        return None;
                    }
                    let name = String::from_utf8(reader.bytes(len)?.to_vec()).ok()?;
                    let target = reader.u64()?;
                    reader.done().then_some(Self::AiSignal { name, target })
                })();
                parsed.map_or(Parsed::Malformed(method), Parsed::Call)
            }
            METHOD_JOURNAL_ENTRIES_VIEWED => {
                let mut reader = Reader { args, at: 0 };
                match reader.varint() {
                    Some(count) => Parsed::Call(Self::JournalEntriesViewed {
                        count,
                        entries: args[reader.at..].to_vec(),
                    }),
                    None => Parsed::Malformed(method),
                }
            }
            METHOD_CLEAN_USER_CLAIM_CHAINS => none(Self::CleanUserClaimChains),
            METHOD_RESTORE_MARKER_STATE => none(Self::RestoreMarkerState),
            METHOD_CLIENT_INITIALIZATION_FINISHED => none(Self::ClientInitializationFinished),
            METHOD_REPORT_REPUTATION => none(Self::ReportPersonalReputationData),
            _ => Parsed::Other,
        }
    }
}

/// What the DGS keeps of these calls for each player.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayerUi {
    /// The inventories the client has asked to open and not to close, in the order they were opened.
    pub open_inventories: Vec<u64>,
    /// The last spectator mode the client asked for (false until it asks).
    pub spectating: bool,
    /// How many times the client finished initializing (`pml.AuthClientInitializationFinished`), once with each batch.
    pub initializations: u32,
}

/// The `PlayerUi` of every player in this DGS.
#[derive(Default)]
pub struct PlayerCalls {
    players: Mutex<HashMap<u64, PlayerUi>>,
}

impl PlayerCalls {
    /// Applies `call` made by `player`. Nothing is answered: see the module notes.
    pub fn apply(&self, player: u64, call: &PlayerCall) {
        let mut players = self.players.lock().unwrap();
        let ui = players.entry(player).or_default();
        match call {
            PlayerCall::OpenInventory(id) => {
                if !ui.open_inventories.contains(id) {
                    ui.open_inventories.push(*id);
                }
                info!(player, inventory = *id, own = *id == player, open = ui.open_inventories.len(), "the client opened an inventory");
            }
            PlayerCall::CloseInventory(id) => {
                ui.open_inventories.retain(|open| open != id);
                info!(player, inventory = *id, open = ui.open_inventories.len(), "the client closed an inventory");
            }
            PlayerCall::SpectatorMode(on) => {
                ui.spectating = *on;
                info!(player, spectating = *on, "the client asked for spectator mode");
            }
            PlayerCall::ClientInitializationFinished => {
                ui.initializations += 1;
                debug!(player, count = ui.initializations, "the client finished initializing");
            }
            PlayerCall::AiSignal { name, target } => {
                debug!(player, signal = %name, target = *target, "an AI signal of the player");
            }
            PlayerCall::JournalEntriesViewed { count, entries } => {
                debug!(player, count = *count, bytes = entries.len(), "journal entries viewed");
            }
            PlayerCall::CleanUserClaimChains
            | PlayerCall::RestoreMarkerState
            | PlayerCall::ReportPersonalReputationData => {
                debug!(player, call = ?call, "a player call that needs no answer");
            }
        }
    }

    /// Reads and applies a call; `false` when the method is not one of this module (the caller goes on).
    pub fn handle(&self, player: u64, method: u32, args: &[u8]) -> bool {
        match PlayerCall::parse(method, args) {
            Parsed::Call(call) => {
                self.apply(player, &call);
                true
            }
            Parsed::Malformed(method) => {
                warn!(player, method, args = args.len(), args_hex = %crate::atc::hex(args), "a player call whose arguments do not read as its layout");
                true
            }
            Parsed::Other => false,
        }
    }

    /// A copy of what is kept for `player`.
    pub fn ui(&self, player: u64) -> PlayerUi {
        self.players.lock().unwrap().get(&player).cloned().unwrap_or_default()
    }

    /// The player left this DGS.
    pub fn forget(&self, player: u64) {
        self.players.lock().unwrap().remove(&player);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A player id.
    const PLAYER: u64 = 332_237_997_341;

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_open_and_close_inventory_calls_carry_the_inventory_id() {
        // Method 192 and 193, the player's own id.
        let args = bytes("1d85ed5a4d000000");
        assert_eq!(PlayerCall::parse(METHOD_OPEN_INVENTORY, &args), Parsed::Call(PlayerCall::OpenInventory(PLAYER)));
        assert_eq!(PlayerCall::parse(METHOD_CLOSE_INVENTORY, &args), Parsed::Call(PlayerCall::CloseInventory(PLAYER)));
        assert_eq!(PlayerCall::parse(METHOD_OPEN_INVENTORY, &args[..7]), Parsed::Malformed(METHOD_OPEN_INVENTORY));
        assert_eq!(PlayerCall::parse(METHOD_OPEN_INVENTORY, &[args.clone(), vec![0]].concat()), Parsed::Malformed(METHOD_OPEN_INVENTORY));
    }

    #[test]
    fn the_spectator_request_is_a_bool() {
        // Frames 17199 (01) and 17908 (00).
        assert_eq!(PlayerCall::parse(METHOD_SPECTATOR_MODE, &[1]), Parsed::Call(PlayerCall::SpectatorMode(true)));
        assert_eq!(PlayerCall::parse(METHOD_SPECTATOR_MODE, &[0]), Parsed::Call(PlayerCall::SpectatorMode(false)));
        assert_eq!(PlayerCall::parse(METHOD_SPECTATOR_MODE, &[2]), Parsed::Malformed(METHOD_SPECTATOR_MODE));
        assert_eq!(PlayerCall::parse(METHOD_SPECTATOR_MODE, &[]), Parsed::Malformed(METHOD_SPECTATOR_MODE));
    }

    #[test]
    fn the_ai_signals_read_as_a_name_and_a_target() {
        let target = 0x0000_00d7_e5ce_14a5u64;
        for (hex, name) in [
            ("0a4f6e4c6f6f6b65644174a514cee5d7000000", "OnLookedAt"),
            ("0a4f6e5374617265644174a514cee5d7000000", "OnStaredAt"),
            ("0c4f6e4c6f6f6b656441776179a514cee5d7000000", "OnLookedAway"),
        ] {
            assert_eq!(
                PlayerCall::parse(METHOD_AI_SIGNAL, &bytes(hex)),
                Parsed::Call(PlayerCall::AiSignal { name: name.into(), target }),
                "{name}"
            );
        }
        // A name longer than its bytes, and trailing bytes, are not the layout.
        assert_eq!(PlayerCall::parse(METHOD_AI_SIGNAL, &bytes("0a4f6e")), Parsed::Malformed(METHOD_AI_SIGNAL));
        assert_eq!(PlayerCall::parse(METHOD_AI_SIGNAL, &bytes("0a4f6e4c6f6f6b65644174a514cee5d700000000")), Parsed::Malformed(METHOD_AI_SIGNAL));
    }

    #[test]
    fn the_calls_with_no_arguments_take_none() {
        for (method, call) in [
            (METHOD_CLEAN_USER_CLAIM_CHAINS, PlayerCall::CleanUserClaimChains),
            (METHOD_RESTORE_MARKER_STATE, PlayerCall::RestoreMarkerState),
            (METHOD_CLIENT_INITIALIZATION_FINISHED, PlayerCall::ClientInitializationFinished),
            (METHOD_REPORT_REPUTATION, PlayerCall::ReportPersonalReputationData),
        ] {
            assert_eq!(PlayerCall::parse(method, &[]), Parsed::Call(call));
            assert_eq!(PlayerCall::parse(method, &[0]), Parsed::Malformed(method));
        }
    }

    #[test]
    fn the_journal_call_is_an_array_with_its_count() {
        // The client sends `00`: no entries.
        assert_eq!(
            PlayerCall::parse(METHOD_JOURNAL_ENTRIES_VIEWED, &[0]),
            Parsed::Call(PlayerCall::JournalEntriesViewed { count: 0, entries: Vec::new() })
        );
        assert_eq!(PlayerCall::parse(METHOD_JOURNAL_ENTRIES_VIEWED, &[]), Parsed::Malformed(METHOD_JOURNAL_ENTRIES_VIEWED));
    }

    #[test]
    fn other_methods_are_left_to_the_inventory_requests() {
        assert_eq!(PlayerCall::parse(185, &bytes("00")), Parsed::Other);
        assert_eq!(PlayerCall::parse(7, &[]), Parsed::Other);
    }

    #[test]
    fn what_the_client_opened_is_kept_until_it_closes_it() {
        let calls = PlayerCalls::default();
        assert!(calls.handle(PLAYER, METHOD_SPECTATOR_MODE, &[1]));
        assert!(calls.handle(PLAYER, METHOD_OPEN_INVENTORY, &bytes("1d85ed5a4d000000")));
        assert!(calls.handle(PLAYER, METHOD_OPEN_INVENTORY, &bytes("1d85ed5a4d000000")));
        assert!(calls.handle(PLAYER, METHOD_CLIENT_INITIALIZATION_FINISHED, &[]));
        let ui = calls.ui(PLAYER);
        assert_eq!((ui.open_inventories.clone(), ui.spectating, ui.initializations), (vec![PLAYER], true, 1));
        assert!(calls.handle(PLAYER, METHOD_CLOSE_INVENTORY, &bytes("1d85ed5a4d000000")));
        assert!(calls.handle(PLAYER, METHOD_SPECTATOR_MODE, &[0]));
        let ui = calls.ui(PLAYER);
        assert_eq!((ui.open_inventories.is_empty(), ui.spectating), (true, false));
        // A malformed call is taken (nothing to do with the inventory parser) and changes nothing.
        assert!(calls.handle(PLAYER, METHOD_OPEN_INVENTORY, &[1, 2, 3]));
        assert!(calls.ui(PLAYER).open_inventories.is_empty());
        assert!(!calls.handle(PLAYER, 185, &[]));
        calls.forget(PLAYER);
        assert_eq!(calls.ui(PLAYER), PlayerUi::default());
    }
}

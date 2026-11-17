//! A dedicated game server. Entities are owned by DGSs and not by clients, and each DGS has authority over some areas of
//! one system. A DGS decides what an entity is, and holds those of the players it governs. The replicant asks it for them
//! when a player joins, and is the only thing that reaches it: clients never do. Its tick moves the players it holds from their
//! clients' input, see `movement`.
//!
//! A library as well as a binary so that the replicant's tests can run a real one. Nothing else may depend on it.

pub mod appearance;
pub mod area;
pub mod atc;
pub mod atc_wire;
pub mod brokered_instance;
pub mod carry;
pub mod combat;
pub mod control;
pub mod departure;
pub mod draw;
pub mod entity;
pub mod equip;
pub mod frames;
pub mod gamedata;
pub mod hangar_access;
pub mod hangar_instance;
pub mod hangar_relation;
pub mod hangar_slots;
pub mod holdings;
pub mod interaction;
pub mod inventory;
pub mod kiosk_calls;
pub mod loadout;
pub mod location;
pub mod movement;
pub mod outbox;
pub mod physics;
pub mod platform;
pub mod platform_times;
pub mod player;
pub mod player_calls;
pub mod reload;
pub mod retrieve;
pub mod retrieve_loadout;
pub mod ride;
pub mod ships;
pub mod solar_system;
#[cfg(any(test, feature = "fixtures"))]
pub mod testdata;
pub mod token;
pub mod transit;
pub mod universe;
pub mod wallet;
pub mod weapon_command;
pub mod worn;

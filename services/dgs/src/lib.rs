//! A dedicated game server. Entities are owned by DGSs and not by clients, and each DGS has authority over some areas of
//! one system. A DGS decides what an entity is, and holds those of the players it governs. The replicant asks it for them
//! when a player joins, and is the only thing that reaches it: clients never do. Its tick moves the players it holds from their
//! clients' input, see `movement`.
//!
//! A library as well as a binary so that the replicant's tests can run a real one. Nothing else may depend on it.

pub mod appearance;
pub mod area;
pub mod control;
pub mod entity;
pub mod frames;
pub mod gamedata;
pub mod holdings;
pub mod interaction;
pub mod loadout;
pub mod location;
pub mod movement;
pub mod outbox;
pub mod physics;
pub mod player;
pub mod solar_system;
#[cfg(any(test, feature = "fixtures"))]
pub mod testdata;
pub mod transit;
pub mod universe;

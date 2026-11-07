//! Character movement against the world: a player's capsule against the static collision of the zone it is in, with
//! gravity, ground, steps, slopes, jumping and crouching. See docs/DGS_PHYSICS.md.
//!
//! `world` holds the collision of a hangar, extracted offline from Data.p4k by `tools/gamedata-build` and fetched from the
//! region's `sc-gamedata` at startup. `controller::step` moves one player for one frame, in the frame of the hangar's zone, Z up, and `check` judges a
//! position a client reports for itself against the same world. Nothing here knows of connections or of the wire:
//! `movement` turns the client's input into a `controller::Input`.

pub mod check;
pub mod controller;
pub mod world;

pub use controller::{step, Input, Params, Stance, State};
pub use world::World;

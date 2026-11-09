//! sc-bot: a headless, protocol-level client of the shard, for testing the server stack without the game. It speaks the
//! game's UDP protocol (the CryNub channel), logs in at the Hub like the launcher, walks the join, and runs scenarios that
//! send what a client sends and assert on what comes back. See docs/BOT.md.

pub mod curve;
pub mod headless;
pub mod hub;
pub mod nub;
pub mod requests;
pub mod scenario;
pub mod scenarios;
pub mod session;
pub mod wire;
pub mod world;

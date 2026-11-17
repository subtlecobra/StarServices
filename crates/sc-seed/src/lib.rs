//! The game data seed, split in two so that only the build machine needs the game archive: `sc-admin seed-export`
//! writes a seed bundle from `Data.p4k` (`bundle`), and sc-seeder, or `sc-admin seed`, hands its rows to their owners
//! (`apply`). See docs/DATA.md, Game data.

pub mod apply;
pub mod bundle;
#[cfg(any(test, feature = "fakes"))]
pub mod fakes;

pub use apply::{apply, Owners};
pub use bundle::{Bundle, Content, Manifest};

#[cfg(test)]
mod tests;

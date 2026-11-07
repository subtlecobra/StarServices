//! Star systems offered to the client.
//!
//! The frontend keeps a destination only if a loaded `Level` has the same location id
//! (`universeLocationUniqueId`, AddAccessibleDestinations 0x143ee4630), so the ids must be the game's
//! own. `sc-admin init --p4k` writes them to destinations.json from the installed game.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use tracing::warn;

const FILE: &str = "destinations.json";
/// The client skips ids with these top 16 bits, but real servers still list one.
const SENTINEL_TOP_BITS: u64 = 0xffff;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub location_id: u64,
    pub name: String,
}

impl Destination {
    pub fn joinable(&self) -> bool {
        self.location_id >> 48 != SENTINEL_TOP_BITS
    }
}

/// Values from build 4.10.2 (CL 12913470), for running without the game files.
pub fn builtin() -> Vec<Destination> {
    [
        (0x0002_0001_0001_0001, "@ui_pregame_stanton_name"),
        (0x0003_0001_0001_0001, "@ui_pregame_pyro_name"),
        (0x0005_0001_0001_0001, "@ui_pregame_nyx_name"),
        (0xffff_0001_0001_0001, "@ui_pregame_stanton_name"),
    ]
    .into_iter()
    .map(|(location_id, name)| Destination { location_id, name: name.to_string() })
    .collect()
}

pub fn save(data_dir: &Path, destinations: &[Destination]) -> Result<(), String> {
    let path = data_dir.join(FILE);
    let text = serde_json::to_string_pretty(destinations).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Reads destinations.json, or falls back to the built-in list.
pub fn load(data_dir: &Path) -> Result<Vec<Destination>, String> {
    let path = data_dir.join(FILE);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display())),
        Err(_) => {
            warn!(path = %path.display(), "no destinations file: using the built-in 4.10.2 list");
            Ok(builtin())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_list_matches_a_real_session() {
        let list = builtin();
        assert_eq!(list.len(), 4);
        assert_eq!(list.iter().filter(|d| d.joinable()).count(), 3);
    }
}

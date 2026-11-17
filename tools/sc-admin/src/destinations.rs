//! Reads the star systems from the game archive.

use std::path::Path;

use p4k_formats::dcb::Dcb;
use p4k::P4k;
use sc_runtime::destinations::Destination;

const DATACORE_PATH: &str = "Data/Game2.dcb";
const LEVEL_STRUCT: &str = "Level";

pub fn from_p4k(path: &Path) -> Result<Vec<Destination>, String> {
    let archive = P4k::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let entry = archive.get(DATACORE_PATH).ok_or_else(|| format!("{DATACORE_PATH} not in archive"))?;
    let bytes = archive.read(entry).map_err(|e| format!("read {DATACORE_PATH}: {e}"))?;
    let datacore = Dcb::open(bytes).map_err(|e| format!("parse {DATACORE_PATH}: {e}"))?;
    let level = *datacore
        .struct_names()
        .get(LEVEL_STRUCT)
        .ok_or_else(|| format!("no {LEVEL_STRUCT} struct in {DATACORE_PATH}"))?;
    let (_, count) = datacore
        .instance_base(level)
        .ok_or_else(|| format!("no {LEVEL_STRUCT} instances in {DATACORE_PATH}"))?;

    let mut destinations: Vec<Destination> = Vec::new();
    for index in 0..count {
        let record = datacore
            .read_instance(level, index, 0, 1)
            .map_err(|e| format!("read {LEVEL_STRUCT} {index}: {e}"))?
            .to_json();
        let location_id = record["universeLocationUniqueId"].as_u64().unwrap_or(0);
        if location_id != 0 && !destinations.iter().any(|d| d.location_id == location_id) {
            let name = record["locDisplayName"].as_str().unwrap_or_default().to_string();
            destinations.push(Destination { location_id, name });
        }
    }
    if destinations.is_empty() {
        return Err(format!("no location ids in {DATACORE_PATH}"));
    }
    Ok(destinations)
}

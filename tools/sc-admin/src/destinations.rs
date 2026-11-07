//! Reads the star systems from the game archive.

use std::{collections::BTreeMap, path::Path};

use p4k_formats::dcb::Dcb;
use p4k::P4k;
use sc_runtime::destinations::Destination;
use serde_json::{json, Value};

const DATACORE_PATH: &str = "Data/Game2.dcb";
const LEVEL_STRUCT: &str = "Level";
/// Items sold in the game's shops, from the inventories in the archive. The dated anniversary sale
/// files are left out, and an item with a price wins over a listing without one.
pub fn shop_items_from_p4k(path: &Path) -> Result<Vec<Value>, String> {
    let archive = P4k::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut items: BTreeMap<String, Value> = BTreeMap::new();
    let entries: Vec<_> = archive.find("Scripts/ShopInventories/", &[".json"]).collect();
    for entry in entries {
        if entry.name.contains("Sale") {
            continue;
        }
        let bytes = archive.read(entry).map_err(|e| format!("read {}: {e}", entry.name))?;
        let Ok(inventory) = serde_json::from_slice::<Value>(&bytes) else { continue };
        for item in inventory["Inventory"].as_array().into_iter().flatten() {
            let Some(id) = item["ID"]["ID"][0].as_str() else { continue };
            let buy_price = item["BuyPrice"].as_f64().unwrap_or(0.0);
            let known_price = items.get(id).and_then(|known| known["buy_price"].as_f64()).unwrap_or(0.0);
            if !items.contains_key(id) || (buy_price > 0.0 && known_price == 0.0) {
                let row = json!({
                    "id": id,
                    "buy_price": buy_price,
                    "sell_price": item["SellPrice"],
                    "current_inventory": item["CurrentInventory"],
                    "max_inventory": item["MaxInventory"],
                });
                items.insert(id.to_string(), row);
            }
        }
    }
    Ok(items.into_values().collect())
}

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

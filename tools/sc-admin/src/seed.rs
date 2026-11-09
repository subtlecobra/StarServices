//! Imports game content: reads every seeded table from the archive into a `sc_seed::Content`, which `seed-export`
//! writes as a seed bundle and `seed` hands to the owners at once (sc_seed::apply). sc-admin never touches a database:
//! the entity classes and the container tree go to each region's sc-entitygraph, every other table to the service that
//! owns it, each through its seed RPC.

use std::{collections::BTreeMap, path::Path};

use sc_api::internal::{ ItemTypeFilter, BlueprintDefault, ContainerSeed, EntityClassInventory, EntityClassSeed, MissionContractSeed, RentalPrice, ReputationLadderStanding, ReputationScope, ReputationStanding, ScenarioSeed, ShopItem, StarterEntitlement, };
use sc_proto::{
    services::solar_system_shop::v1::RentalOffering,
};
use sc_seed::Content;
use serde_json::Value;

use crate::{
    containers::Node,
    content::{Blueprint, Contract, Game, Scope, Standing, Starter},
};

fn starter(row: &Starter) -> StarterEntitlement {
    StarterEntitlement {
        class_guid: row.class_guid.clone(),
        item_type: row.item_type.clone(),
        item_sub_type: row.item_sub_type.clone(),
        name: row.name.clone(),
        source: row.source.clone(),
        port_path: row.port_path.clone(),
    }
}

fn scope(row: &Scope) -> ReputationScope {
    ReputationScope {
        scope: row.name.clone(),
        standings_id: row.standings_id.clone(),
        display_name: row.display_name.clone(),
        description: row.description.clone(),
        initial_reputation: row.initial,
        ceiling: row.ceiling,
    }
}

fn standing(row: &Standing) -> ReputationStanding {
    ReputationStanding {
        id: row.id.clone(),
        standings_id: row.ladder.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        min: row.min,
        drift_seconds: row.drift_seconds,
        drift_amount: row.drift_amount,
        gated: row.gated,
    }
}

fn blueprint(row: &Blueprint) -> BlueprintDefault {
    BlueprintDefault {
        blueprint_id: row.id.clone(),
        category_id: row.category.clone(),
        item_class_id: row.item_class.clone(),
        process_type: row.process_type,
    }
}

fn contract(row: &Contract) -> MissionContractSeed {
    MissionContractSeed { id: row.id.clone(), body: row.body.clone() }
}

pub fn container(node: &Node) -> ContainerSeed {
    ContainerSeed {
        geid: node.geid,
        parent: node.parent,
        guid: node.guid.clone(),
        labels: node.labels.clone(),
        socpak_content_hash: node.contents_crc,
        socpak_file_path_hash: node.path_crc,
        start_id: node.start_id,
        has_planet: node.planet,
        streaming_immovable_count: node.immovable,
        streaming_movable_count: node.movable,
        name: node.name.clone(),
        system: node.system.clone(),
    }
}

/// Items sold in the game's shops, from the shop inventory files (`(path, bytes)`). The dated anniversary sale files
/// are left out, and an item with a price wins over a listing without one. In id order.
pub fn shop_items(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Vec<ShopItem> {
    let mut items: BTreeMap<String, ShopItem> = BTreeMap::new();
    for (name, bytes) in files {
        if name.contains("Sale") {
            continue;
        }
        let Ok(inventory) = serde_json::from_slice::<Value>(&bytes) else { continue };
        for item in inventory["Inventory"].as_array().into_iter().flatten() {
            let Some(id) = item["ID"]["ID"][0].as_str() else { continue };
            let buy_price = item["BuyPrice"].as_f64().unwrap_or(0.0);
            let known_price = items.get(id).map_or(0.0, |known| known.buy_price);
            if !items.contains_key(id) || (buy_price > 0.0 && known_price == 0.0) {
                let row = ShopItem {
                    id: id.to_string(),
                    buy_price,
                    sell_price: item["SellPrice"].as_f64().unwrap_or(0.0),
                    current_inventory: item["CurrentInventory"].as_f64().unwrap_or(0.0) as i32,
                    max_inventory: item["MaxInventory"].as_f64().unwrap_or(0.0) as i32,
                };
                items.insert(id.to_string(), row);
            }
        }
    }
    items.into_values().collect()
}

/// The build of the archive: the change number in `build_manifest.id` beside it, as gamedata-build reads it.
pub fn build_of(p4k: &Path) -> Result<String, String> {
    let path = p4k.parent().unwrap_or(Path::new(".")).join("build_manifest.id");
    let text = std::fs::read(&path).map_err(|e| format!("{}: {e}; give --build", path.display()))?;
    let manifest: Value = serde_json::from_slice(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    match &manifest["Data"]["RequestedP4ChangeNum"] {
        Value::String(build) if !build.is_empty() => Ok(build.clone()),
        Value::Number(build) => Ok(build.to_string()),
        _ => Err(format!("{}: no Data.RequestedP4ChangeNum; give --build", path.display())),
    }
}

/// Everything the archive seeds, with the rental prices of the price list (none without one).
pub fn read(game: &Game, rental_prices: Vec<RentalPrice>) -> Result<Content, String> {
    let classes = game.classes()?;
    let (starters, missing) = game.starter(&classes)?;
    let (scope_rows, standing_rows) = game.reputation()?;
    let ladder = scope_rows
        .iter()
        .flat_map(Scope::ladder_rows)
        .map(|(ladder, standing)| ReputationLadderStanding { standings_id: ladder.to_string(), standing_id: standing.to_string() })
        .collect();
    let default_blueprints = game.default_blueprints()?;
    let scenario_rows = game.scenarios()?;
    let (contract_rows, skipped) = game.contracts(&scope_rows, &standing_rows)?;
    let inventory_containers = game.containers()?;
    let class_rows: Vec<EntityClassSeed> = classes
        .iter()
        .map(|class| {
            let inventory = class.container.as_ref().and_then(|guid| inventory_containers.get(guid));
            let (name, description) = game.class_text(class);
            EntityClassSeed {
                guid: class.guid.clone(),
                guid_hash_crc: class.crc,
                class_name: class.name.clone(),
                item_type: class.item_type.clone(),
                item_sub_type: class.item_sub_type.clone(),
                item_type_enum: class.item_type_enum,
                item_sub_type_enum: class.item_sub_type_enum,
                name,
                description,
                occupancy_micro_scu: class.occupancy_micro_scu,
                inventory: inventory.map(|c| EntityClassInventory {
                    capacity_micro_scu: c.capacity_micro_scu,
                    closed: c.closed,
                    excluded: type_filters(&c.excluded),
                    only: type_filters(&c.only),
                }),
            }
        })
        .collect();
    let mut class_rows = class_rows;
    // The item banks' type filters, kept as a class of their own: not an entity, but where sc-entitygraph looks them up.
    if let Some(bank) = game.item_bank()? {
        class_rows.push(EntityClassSeed {
            guid: ITEM_BANK_CLASS.to_string(),
            class_name: "ItemBankContainer".to_string(),
            inventory: Some(EntityClassInventory { capacity_micro_scu: 0, closed: false, excluded: type_filters(&bank.excluded), only: type_filters(&bank.only) }),
            ..Default::default()
        });
    }
    let tree = game.universe()?;
    let shop_items = shop_items(game.shop_inventories()?);

    println!("entity classes: {}", class_rows.len());
    println!("shop items: {}", shop_items.len());
    println!("containers: {} nodes, {} edges", tree.nodes.len(), tree.nodes.iter().filter(|node| node.parent.is_some()).count());
    for (system, count) in &tree.per_system {
        println!("  {system}: {count} nodes");
    }
    if !tree.missing.is_empty() {
        println!("containers with no socpak: {}", tree.missing.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    if !missing.is_empty() {
        println!("loadout items with no class: {}", missing.join(", "));
    }
    println!(
        "mission contracts left out: {} not for release, {} gated on a scenario",
        skipped.not_for_release, skipped.needs_scenario
    );
    Ok(Content {
        classes: class_rows,
        containers: tree.nodes.iter().map(container).collect(),
        rental_prices,
        shop_items,
        starters: starters.iter().map(starter).collect(),
        scopes: scope_rows.iter().map(scope).collect(),
        standings: standing_rows.iter().map(standing).collect(),
        ladder,
        blueprints: default_blueprints.iter().map(blueprint).collect(),
        scenarios: scenario_rows.into_iter().map(|(template_id, status)| ScenarioSeed { template_id, status }).collect(),
        contracts: contract_rows.iter().map(contract).collect(),
    })
}

/// Checks a rental price list. The shop checks it again, as the owner of the prices.
pub fn rental_prices(text: &str) -> Result<Vec<RentalPrice>, String> {
    let list: Value = serde_json::from_str(text).map_err(|e| format!("rental prices: {e}"))?;
    let entries = list.as_array().ok_or("rental prices: expected a list")?;
    entries
        .iter()
        .map(|entry| {
            let id = entry["id"].as_str().filter(|id| !id.is_empty()).ok_or("rental prices: entry with no id")?;
            let offerings = entry["offerings"].as_array().filter(|o| !o.is_empty());
            let offerings = offerings.ok_or_else(|| format!("rental prices: {id} has no offerings"))?;
            let offerings = offerings
                .iter()
                .map(|offering| {
                    let days = offering["days"].as_u64().filter(|days| (1..=u64::from(u32::MAX)).contains(days));
                    let price = offering["price"].as_f64().filter(|price| price.is_finite() && *price >= 0.0);
                    match (days, price) {
                        (Some(days), Some(price)) => Ok(RentalOffering { days: days as u32, price }),
                        _ => Err(format!("rental prices: {id} needs positive days and a price")),
                    }
                })
                .collect::<Result<_, _>>()?;
            Ok(RentalPrice { id: id.to_string(), offerings })
        })
        .collect()
}

/// The key of the class that carries the item banks' type filters; sc-entitygraph reads it for `Location` inventories.
pub const ITEM_BANK_CLASS: &str = "itembankcontainer";

fn type_filters(filters: &[crate::content::TypeFilter]) -> Vec<ItemTypeFilter> {
    filters.iter().map(|filter| ItemTypeFilter { item_type: filter.item_type.clone(), item_sub_types: filter.item_sub_types.clone() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_price_list() {
        let prices = rental_prices(r#"[{"id": "abc", "offerings": [{"days": 1, "price": 100}]}]"#).unwrap();
        assert_eq!(prices, [RentalPrice { id: "abc".into(), offerings: vec![RentalOffering { days: 1, price: 100.0 }] }]);
    }

    #[test]
    fn refuses_entries_that_would_break_a_rental() {
        for text in [
            r#"{}"#,
            r#"[{"offerings": [{"days": 1, "price": 1}]}]"#,
            r#"[{"id": "a", "offerings": []}]"#,
            r#"[{"id": "a", "offerings": [{"days": 0, "price": 1}]}]"#,
            r#"[{"id": "a", "offerings": [{"days": 1, "price": -1}]}]"#,
        ] {
            assert!(rental_prices(text).is_err(), "{text}");
        }
    }

    #[test]
    fn shop_items_skip_sales_and_prefer_a_priced_listing() {
        let file = |entries: &str| format!(r#"{{"Inventory": [{entries}]}}"#).into_bytes();
        let entry = |id: &str, buy: f64| {
            format!(r#"{{"ID": {{"ID": ["{id}"]}}, "BuyPrice": {buy}, "SellPrice": 2, "CurrentInventory": 3.0, "MaxInventory": 4}}"#)
        };
        let items = shop_items([
            ("Scripts/ShopInventories/a.json".to_string(), file(&format!("{},{}", entry("b", 0.0), entry("a", 5.0)))),
            ("Scripts/ShopInventories/b.json".to_string(), file(&format!("{},{}", entry("b", 7.0), entry("a", 9.0)))),
            ("Scripts/ShopInventories/AnniversarySale.json".to_string(), file(&entry("c", 1.0))),
            ("Scripts/ShopInventories/broken.json".to_string(), b"{".to_vec()),
        ]);
        let item = |id: &str, buy_price: f64| ShopItem {
            id: id.into(),
            buy_price,
            sell_price: 2.0,
            current_inventory: 3,
            max_inventory: 4,
        };
        assert_eq!(items, [item("a", 5.0), item("b", 7.0)]);
    }
}


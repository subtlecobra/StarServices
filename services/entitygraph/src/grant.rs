//! `GrantItems` of `EntityGraphInternal`: new items for a player who is not live on a shard, stowed in one of their
//! inventories. The services that owe a player items (an entitlement claimed, a delivery, a rental) ask for them here
//! with a command id of their own, so a retry grants nothing twice. The checks are those of the admin RPCs, since the
//! plan is the same: ownership, capacity, and a refusal for what a DGS holds live.

use std::collections::HashMap;

use sc_api::internal::{GrantItemsRequest, GrantItemsResponse};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::EntityNodeProperties;
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    admin::{
        allocate_geids, apply_once, create_into, invalid, need_command, need_inventory, need_owner, need_target, recorded,
        refuse_taken, MAX_ITEMS, MAX_STACK,
    },
    commands::Plan,
    now_ms,
};

/// The granted items of a request, as the entities to create: `classes` by lowercase GUID.
fn properties(request: &GrantItemsRequest, first: u64, classes: &HashMap<String, Value>) -> Vec<EntityNodeProperties> {
    request
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let class = &classes[&item.entity_class_guid.trim().to_lowercase()];
            EntityNodeProperties {
                geid: first + index as u64,
                class_guid_crc: class["guidHashCrc"].as_u64().unwrap_or(0) as u32,
                stack_size: item.stack_size.max(1),
                owner_id: request.owner_id,
                scale: 1.0,
                item_type_enum: class["itemTypeEnum"].as_i64().unwrap_or(0) as i32,
                item_sub_type_enum: class["itemSubTypeEnum"].as_i64().unwrap_or(0) as i32,
                debug: HashMap::from([("createdBy".to_string(), "grant".to_string())]),
                ..Default::default()
            }
        })
        .collect()
}

pub async fn grant(db: &Arango, request: GrantItemsRequest) -> Result<GrantItemsResponse, Status> {
    need_command(&request.command_id)?;
    need_owner(request.owner_id)?;
    need_inventory(&request.inventory_id, "inventory_id")?;
    if request.items.is_empty() || request.items.len() > MAX_ITEMS as usize {
        return Err(invalid("1 to 500 items are needed"));
    }
    if request.items.iter().any(|item| item.stack_size > MAX_STACK) {
        return Err(Status::invalid_argument(format!("a stack of at most {MAX_STACK}")));
    }
    let geids_of = |found: &Value| -> Vec<u64> {
        found["geids"].as_array().map(|list| list.iter().filter_map(|geid| geid.as_str()?.parse().ok()).collect()).unwrap_or_default()
    };
    if let Some(found) = recorded(db, &request.command_id).await? {
        return Ok(GrantItemsResponse { geids: geids_of(&found), replayed: true });
    }
    let guids: Vec<String> = request.items.iter().map(|item| item.entity_class_guid.trim().to_lowercase()).collect();
    let rows = db
        .aql("FOR c IN entity_classes FILTER c._key IN @guids OR c.guid IN @guids RETURN c", json!({ "guids": guids }))
        .await
        .map_err(internal)?;
    let classes: HashMap<String, Value> = rows
        .into_iter()
        .flat_map(|row| {
            let names: Vec<String> = ["_key", "guid"].iter().filter_map(|field| row[*field].as_str().map(str::to_lowercase)).collect();
            names.into_iter().map(move |name| (name, row.clone()))
        })
        .collect();
    let mut missing: Vec<&String> = guids.iter().filter(|guid| !classes.contains_key(*guid)).collect();
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        let names: Vec<&str> = missing.iter().map(|guid| guid.as_str()).collect();
        return Err(Status::not_found(format!("no entity class {}: run sc-admin seed", names.join(", "))));
    }
    let first = allocate_geids(db, request.items.len() as u64).await?;
    let (owner, to) = (request.owner_id, request.inventory_id.as_str());
    let mut plan = Plan::new(None, now_ms());
    plan.refuse_live_inventory(to, owner);
    let made = properties(&request, first, &classes);
    for item in &made {
        refuse_taken(&mut plan, item.geid);
    }
    for item in made {
        plan.command(&create_into(item, to))?;
    }
    need_target(&mut plan, to);
    let geids: Vec<u64> = (first..first + request.items.len() as u64).collect();
    let result = json!({ "geids": geids.iter().map(u64::to_string).collect::<Vec<_>>() });
    let (result, replayed) = apply_once(db, &request.command_id, owner, plan, result).await?;
    info!(owner, reason = %request.reason, inventory = to, items = geids.len(), replayed, "items granted");
    Ok(GrantItemsResponse { geids: geids_of(&result), replayed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_api::internal::GrantedItem;

    #[test]
    fn items_take_their_class_owner_and_consecutive_geids() {
        let classes = HashMap::from([("abc".to_string(), json!({ "guidHashCrc": 7, "itemTypeEnum": 2 }))]);
        let request = GrantItemsRequest {
            owner_id: 300_000_000_001,
            items: vec![
                GrantedItem { entity_class_guid: " ABC ".into(), stack_size: 0 },
                GrantedItem { entity_class_guid: "abc".into(), stack_size: 5 },
            ],
            ..Default::default()
        };
        let made = properties(&request, 400_000_000_010, &classes);
        assert_eq!(made.iter().map(|item| (item.geid, item.stack_size, item.class_guid_crc)).collect::<Vec<_>>(), [(400_000_000_010, 1, 7), (400_000_000_011, 5, 7)]);
        assert!(made.iter().all(|item| item.owner_id == 300_000_000_001));
    }
}

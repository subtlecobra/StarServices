//! `entitlement.inventory.location`: the join's claim of a player's entitlements at the location they join. On the real
//! backend it moves the entitlement inventory to the location; here each entitlement that is an item becomes a new item
//! in the player's item bank there (`<player>:Location:<location id>`), made by sc-entitygraph without a game server,
//! since stowed items need none. Ships and vehicles take no room in an inventory, so sc-entitygraph skips them.
//!
//! Each entitlement has a command id of its own, so joining again, or joining the same location from another shard,
//! makes nothing twice. Whatever fails is logged and left for the next join: the client reads only the code of the reply,
//! and a failed claim must not keep the player out of the game.

use std::time::Duration;

use sc_api::internal::{
    entitlement_internal_client::EntitlementInternalClient, entity_graph_internal_client::EntityGraphInternalClient,
    GrantItemsRequest, GrantedItem, QueryEntitlementsRequest,
};
use sc_proto::services::entitlement::v2::{EntitlementSourceType, EntitlementStatus};
use serde_json::Value;
use tonic::transport::Channel;
use tracing::{info, warn};

/// How long a join waits for the whole claim.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Claims {
    pub entitlements: EntitlementInternalClient<Channel>,
    pub graph: EntityGraphInternalClient<Channel>,
}

/// The location a claim names: `{"locationId":N}`.
pub fn location_of(body: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("locationId").and_then(|id| id.as_u64().or_else(|| id.as_str()?.parse().ok())).filter(|id| *id != 0)
}

/// The inventory of a player's items at a location.
pub fn bank(player: u64, location: u64) -> String {
    format!("{player}:Location:{location}")
}

impl Claims {
    /// Makes the player's entitled items at `location`, within `CLAIM_TIMEOUT`.
    pub async fn claim(&self, account_id: u32, player: u64, location: u64) {
        if tokio::time::timeout(CLAIM_TIMEOUT, self.items(account_id, player, location)).await.is_err() {
            warn!(player, location, "the claim of entitled items timed out, the rest waits for the next join");
        }
    }

    async fn items(&self, account_id: u32, player: u64, location: u64) {
        let request = QueryEntitlementsRequest { account_id, player_id: player, filter: None, include_persistent_properties: false };
        let mut stream = match self.entitlements.clone().query_entitlements(request).await {
            Ok(stream) => stream.into_inner(),
            Err(status) => {
                warn!(player, %status, "no entitlements to claim");
                return;
            }
        };
        let mut entitled = Vec::new();
        loop {
            match stream.message().await {
                Ok(Some(page)) => entitled.extend(page.entitlements),
                Ok(None) => break,
                Err(status) => {
                    warn!(player, %status, "the entitlements stopped coming");
                    break;
                }
            }
        }
        let inventory = bank(player, location);
        let (mut made, mut skipped, mut failed) = (0, 0, 0);
        for entitlement in entitled {
            let live = [EntitlementStatus::Delivered as i32, EntitlementStatus::Undelivered as i32].contains(&entitlement.status);
            // A rental makes its own item when it starts.
            if !live || entitlement.source_type == EntitlementSourceType::Rental as i32 || entitlement.entity_class_guid.is_empty() {
                continue;
            }
            let request = GrantItemsRequest {
                command_id: format!("claim:{}:{location}", entitlement.urn),
                owner_id: player,
                inventory_id: inventory.clone(),
                items: vec![GrantedItem { entity_class_guid: entitlement.entity_class_guid.clone(), stack_size: 1 }],
                reason: format!("entitlement {}", entitlement.urn),
                items_only: true,
            };
            match self.graph.clone().grant_items(request).await {
                Ok(reply) if reply.get_ref().geids.first() == Some(&0) => skipped += 1,
                Ok(reply) => made += usize::from(!reply.get_ref().replayed),
                Err(status) => {
                    failed += 1;
                    warn!(player, urn = %entitlement.urn, %status, "an entitlement was not claimed");
                }
            }
        }
        info!(player, location, made, skipped, failed, "entitlements claimed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_location_is_read_from_the_body() {
        assert_eq!(location_of(r#"{"locationId":848868938}"#), Some(848_868_938));
        assert_eq!(location_of(r#"{"locationId":"7"}"#), Some(7));
        assert_eq!(location_of(r#"{"locationId":0}"#), None);
        assert_eq!(location_of("{}"), None);
        assert_eq!(location_of("not json"), None);
    }

    #[test]
    fn the_bank_is_the_players_location_inventory() {
        assert_eq!(bank(300_000_000_001, 42), "300000000001:Location:42");
    }
}

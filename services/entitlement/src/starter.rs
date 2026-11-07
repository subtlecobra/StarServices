//! What every player owns from the start. `sc-admin seed` reads the catalogue from the game data and hands
//! it to `EntitlementInternal.SeedStarterEntitlements`, which fills `starter_entitlements`. A player
//! receives those rows the first time this service, or sc-entitlement-v2 through it, sees them.

use std::{collections::HashSet, sync::Mutex};

use sc_db::Postgres;
use sc_proto::services::entitlement::v2::{EntitlementLogAction, EntitlementSourceType, EntitlementStatus};

const GRANT: &str = "
WITH added AS (
    INSERT INTO entitlements (urn, account_id, player_id, source_type, status, entity_class_guid,
                              item_type, item_sub_type, name)
    SELECT 'urn:sc:global:entitlement:uuid:' || md5($2::bigint::text || ':' || s.class_guid)::uuid,
           $1::bigint, $2::bigint, $3::int, $4::int, s.class_guid, s.item_type, s.item_sub_type, s.name
    FROM starter_entitlements s
    ON CONFLICT DO NOTHING RETURNING urn
)
INSERT INTO entitlement_history (urn, player_id, log_action, log_message)
SELECT urn, $2::bigint, $5::int, 'starter' FROM added";

#[derive(Default)]
pub struct Starter {
    granted: Mutex<HashSet<u64>>,
}

impl Starter {
    /// Safe to repeat: the URN comes from the player and the class. A player is remembered only once
    /// the catalogue has rows, so one seen before the seed still receives them.
    pub async fn ensure(&self, db: &Postgres, account_id: u32, geid: u64) -> Result<(), String> {
        if self.granted.lock().unwrap().contains(&geid) {
            return Ok(());
        }
        let source = EntitlementSourceType::IngamePersistent as i32;
        let status = EntitlementStatus::Packaged as i32;
        let action = EntitlementLogAction::Created as i32;
        db.execute(GRANT, &[&i64::from(account_id), &(geid as i64), &source, &status, &action]).await?;
        if db.query_opt("SELECT 1 FROM starter_entitlements LIMIT 1", &[]).await?.is_some() {
            self.granted.lock().unwrap().insert(geid);
        }
        Ok(())
    }
}

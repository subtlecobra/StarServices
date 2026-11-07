//! `scbackend.internal.EntitlementInternal`: how other services reach `entitlements`, which this service
//! owns. sc-eatransaction grants purchases, sc-rental moves and ends rental expiries, and
//! sc-entitlement-v2 reads and updates entitlements for its own API. The admin site revokes and reads history.

use sc_db::{internal, Postgres, Row, ToSql};
use sc_economy::{decoded, reply_stream, timestamp_of, ReplyStream};
use sc_proto::{
    internal::{
        entitlement_internal_server::EntitlementInternal, EntitlementHistoryEntry, EntitlementHistoryRequest,
        EntitlementHistoryResponse, GrantPurchaseRequest, GrantPurchaseResponse, QueryEntitlementsRequest,
        QueryEntitlementsResponse, RevokeEntitlementRequest, RevokeEntitlementResponse, RevokeExpiredRentalsRequest,
        RevokeExpiredRentalsResponse,
        SeedResponse, SeedStarterEntitlementsRequest, SetRentalExpiryRequest, SetRentalExpiryResponse,
        UpdateLoadoutRequest, UpdateLoadoutResponse,
    },
    services::entitlement::v2::{
        entitlement_filter::Exclude, Entitlement, EntitlementFilter, EntitlementLogAction, EntitlementMetadata,
        EntitlementSourceType, EntitlementStatus,
    },
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::{Entitlements, ENTITLEMENT_COLUMNS};

/// Rows per statement when seeding.
const SEED_ROWS: usize = 500;
/// The URN of a purchase, from the player and the caller's idempotency key.
const PURCHASE_URN: &str = "'urn:sc:global:entitlement:uuid:' || md5($1::bigint::text || ':purchase:' || $2::text)::uuid";

/// A v2 entitlement from a row of `ENTITLEMENT_COLUMNS`.
pub fn entitlement(row: &Row, include_properties: bool) -> Result<Entitlement, Status> {
    Ok(Entitlement {
        urn: row.get("urn"),
        source_urn: row.get("source_urn"),
        source_type: row.get("source_type"),
        status: row.get("status"),
        entity_class_guid: row.get("entity_class_guid"),
        player_id: row.get::<_, i64>("player_id") as u64,
        entity_configuration: decoded(row, "entity_configuration")?,
        entity_geid: row.get::<_, Option<i64>>("entity_geid").map(|geid| geid as u64),
        claim_timestamp: timestamp_of(row, "claimed_ms"),
        claim_version: row.get("claim_version"),
        rental_expiry: timestamp_of(row, "rental_expiry_ms"),
        arena_commander_loadout: decoded(row, "arena_commander_loadout")?,
        created_at: timestamp_of(row, "created_ms"),
        modified_at: timestamp_of(row, "modified_ms"),
        metadata: Some(EntitlementMetadata {
            name: row.get("name"),
            stack_size: row.get("stack_size"),
            source_sku: row.get("source_sku"),
            real_money: row.get("real_money"),
        }),
        persistent_properties: if include_properties { decoded(row, "persistent_properties")? } else { None },
    })
}

/// A source type of 0 means no source type, so it cannot be a condition.
pub fn source_filter(source_types: Vec<i32>) -> Vec<i32> {
    source_types.into_iter().filter(|source| *source != 0).collect()
}

/// A player's entitlements matching a v2 filter.
pub async fn query(
    db: &Postgres,
    geid: u64,
    filter: EntitlementFilter,
    include_properties: bool,
) -> Result<Vec<Entitlement>, Status> {
    let exclude = filter.exclude.clone().unwrap_or_else(Exclude::default);
    let geids: Vec<i64> = filter.entity_geid.iter().map(|geid| *geid as i64).collect();
    let sql = format!(
        "SELECT {ENTITLEMENT_COLUMNS} FROM entitlements
         WHERE player_id = $1
           AND ($2::text IS NULL OR item_type = $2)
           AND ($3::text IS NULL OR item_sub_type = $3)
           AND (cardinality($4::text[]) = 0 OR entity_class_guid = ANY($4))
           AND (cardinality($5::text[]) = 0 OR urn = ANY($5))
           AND (cardinality($6::int[]) = 0 OR source_type = ANY($6))
           AND (cardinality($7::bigint[]) = 0 OR entity_geid = ANY($7))
           AND NOT item_type = ANY($8::text[])
           AND NOT item_sub_type = ANY($9::text[])
           AND ($10 OR status <> $11)
         ORDER BY created_at, urn"
    );
    let rows = db
        .query(
            &sql,
            &[
                &(geid as i64),
                &filter.item_type,
                &filter.item_sub_type,
                &filter.entity_class_guid,
                &filter.entitlement_urn,
                &source_filter(filter.source_type.clone()),
                &geids,
                &exclude.item_type,
                &exclude.item_sub_type,
                &filter.include_revoked.unwrap_or(false),
                &(EntitlementStatus::Revoked as i32),
            ],
        )
        .await
        .map_err(internal)?;
    rows.iter().map(|row| entitlement(row, include_properties)).collect()
}

/// Stores a loadout, which v1 and v2 encode alike. One statement, so the entitlement and its history
/// change together.
pub async fn update_loadout(db: &Postgres, geid: u64, urn: &str, loadout: &[u8], command: &str) -> Result<(), Status> {
    let changed = db
        .execute(
            "WITH changed AS (
                 UPDATE entitlements SET arena_commander_loadout = $3, modified_at = now()
                 WHERE urn = $1 AND player_id = $2 RETURNING urn)
             INSERT INTO entitlement_history (urn, player_id, log_action, command_id)
             SELECT urn, $2, $5, $4 FROM changed",
            &[&urn, &(geid as i64), &loadout, &command, &(EntitlementLogAction::Updated as i32)],
        )
        .await
        .map_err(internal)?;
    if changed == 0 {
        return Err(Status::not_found(format!("entitlement {urn}")));
    }
    Ok(())
}

/// A rental is claimed as it is bought, so its entity is made at once. Anything else waits for a claim.
fn purchase_status(source_type: i32) -> i32 {
    match source_type == EntitlementSourceType::Rental as i32 {
        true => EntitlementStatus::Undelivered as i32,
        false => EntitlementStatus::Packaged as i32,
    }
}

/// Milliseconds since the epoch as a timestamp, or NULL for 0.
const EXPIRY: &str = "CASE WHEN $3::bigint = 0 THEN NULL ELSE to_timestamp($3::bigint / 1000.0) END";

fn check_purchase(request: &GrantPurchaseRequest) -> Result<(), Status> {
    if request.key.is_empty() || request.player_id == 0 || request.entity_class_guid.is_empty() {
        return Err(Status::invalid_argument("a purchase needs a key, a player and an entity class"));
    }
    if request.source_type == EntitlementSourceType::Unspecified as i32 {
        return Err(Status::invalid_argument("a purchase needs a source type"));
    }
    Ok(())
}

pub async fn grant_purchase(db: &Postgres, request: &GrantPurchaseRequest) -> Result<String, Status> {
    check_purchase(request)?;
    let geid = request.player_id as i64;
    let sql = format!(
        "WITH added AS (
             INSERT INTO entitlements (urn, account_id, player_id, source_type, status, entity_class_guid, rental_expiry)
             VALUES ({PURCHASE_URN}, $4, $1, $5, $6, $7, {EXPIRY})
             ON CONFLICT DO NOTHING RETURNING urn),
         logged AS (
             INSERT INTO entitlement_history (urn, player_id, log_action, log_message, command_id)
             SELECT urn, $1, $8, $9, $2 FROM added)
         SELECT {PURCHASE_URN} AS urn"
    );
    let row = db
        .query_opt(
            &sql,
            &[
                &geid,
                &request.key,
                &request.rental_expiry_ms,
                &i64::from(request.account_id),
                &request.source_type,
                &purchase_status(request.source_type),
                &request.entity_class_guid,
                &(EntitlementLogAction::Created as i32),
                &request.log_message,
            ],
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::internal("no urn"))?;
    Ok(row.get("urn"))
}

fn check_expiry(request: &SetRentalExpiryRequest) -> Result<(), Status> {
    if request.rental_expiry_ms <= 0 || request.urn.is_empty() {
        return Err(Status::invalid_argument("a rental expiry needs an entitlement and a time"));
    }
    Ok(())
}

pub async fn set_rental_expiry(db: &Postgres, request: &SetRentalExpiryRequest) -> Result<(), Status> {
    check_expiry(request)?;
    let geid = request.player_id as i64;
    let expiry = "to_timestamp($3::bigint / 1000.0)";
    let sql = format!(
        "WITH changed AS (
             UPDATE entitlements SET rental_expiry = {expiry}, modified_at = now(),
                 status = CASE WHEN $4 AND status = $5 THEN $6 ELSE status END
             WHERE urn = $1 AND player_id = $2
               AND (rental_expiry IS DISTINCT FROM {expiry} OR ($4 AND status = $5))
             RETURNING urn)
         INSERT INTO entitlement_history (urn, player_id, log_action, log_message, command_id)
         SELECT urn, $2, $7, $8, $9 FROM changed"
    );
    let parameters: [&(dyn ToSql + Sync); 9] = [
        &request.urn,
        &geid,
        &request.rental_expiry_ms,
        &request.reactivate,
        &(EntitlementStatus::Revoked as i32),
        &(EntitlementStatus::Undelivered as i32),
        &(EntitlementLogAction::Updated as i32),
        &request.log_message,
        &request.command_id,
    ];
    if db.execute(&sql, &parameters).await.map_err(internal)? > 0 {
        return Ok(());
    }
    // Nothing changed: already so, or not there.
    db.query_opt("SELECT 1 FROM entitlements WHERE urn = $1 AND player_id = $2", &[&request.urn, &geid])
        .await
        .map_err(internal)?
        .map(|_| ())
        .ok_or_else(|| Status::not_found(format!("entitlement {}", request.urn)))
}

/// Revokes the rental entitlements among `urns` whose expiry has passed. One that a renewal moved on stays.
pub async fn revoke_expired_rentals(db: &Postgres, urns: &[String]) -> Result<Vec<String>, Status> {
    let rows = db
        .query(
            "WITH revoked AS (
                 UPDATE entitlements SET status = $2, modified_at = now()
                 WHERE urn = ANY($1) AND source_type = $3 AND status <> $2
                   AND (rental_expiry IS NULL OR rental_expiry <= now())
                 RETURNING urn, player_id),
             logged AS (
                 INSERT INTO entitlement_history (urn, player_id, log_action, log_message)
                 SELECT urn, player_id, $4, 'rental expired' FROM revoked)
             SELECT urn FROM revoked",
            &[
                &urns,
                &(EntitlementStatus::Revoked as i32),
                &(EntitlementSourceType::Rental as i32),
                &(EntitlementLogAction::Revoked as i32),
            ],
        )
        .await
        .map_err(internal)?;
    Ok(rows.iter().map(|row| row.get("urn")).collect())
}

const DEFAULT_HISTORY: u32 = 50;
const MAX_HISTORY: u32 = 500;

/// Revokes one entitlement, whatever its source. Returns the status it had, and whether this call revoked it: a repeat
/// finds it revoked and changes nothing. Its history row is written in the same statement.
fn check_revoke(request: &RevokeEntitlementRequest) -> Result<(), Status> {
    if request.urn.is_empty() || request.player_id == 0 {
        return Err(Status::invalid_argument("a revocation needs an entitlement and a player"));
    }
    Ok(())
}

pub async fn revoke(db: &Postgres, request: &RevokeEntitlementRequest) -> Result<RevokeEntitlementResponse, Status> {
    check_revoke(request)?;
    let geid = request.player_id as i64;
    let revoked = EntitlementStatus::Revoked as i32;
    let row = db
        .query_opt(
            "WITH before AS (
                 SELECT urn, status FROM entitlements WHERE urn = $1 AND player_id = $2),
             changed AS (
                 UPDATE entitlements SET status = $3, modified_at = now()
                 WHERE urn = $1 AND player_id = $2 AND status <> $3 RETURNING urn),
             logged AS (
                 INSERT INTO entitlement_history (urn, player_id, log_action, log_message, command_id)
                 SELECT urn, $2, $4, $5, $6 FROM changed)
             SELECT before.status, (SELECT count(*) FROM changed) AS changed FROM before",
            &[
                &request.urn,
                &geid,
                &revoked,
                &(EntitlementLogAction::Revoked as i32),
                &request.log_message,
                &request.command_id,
            ],
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("entitlement {}", request.urn)))?;
    Ok(RevokeEntitlementResponse { previous_status: row.get("status"), revoked: row.get::<_, i64>("changed") > 0 })
}

fn history_limit(request: &EntitlementHistoryRequest) -> Result<u32, Status> {
    if request.player_id == 0 {
        return Err(Status::invalid_argument("missing player"));
    }
    Ok(match request.limit {
        0 => DEFAULT_HISTORY,
        limit => limit.min(MAX_HISTORY),
    })
}

pub async fn history(db: &Postgres, request: &EntitlementHistoryRequest) -> Result<Vec<EntitlementHistoryEntry>, Status> {
    let limit = history_limit(request)?;
    let rows = db
        .query(
            "SELECT urn, log_action, log_message, command_id, (extract(epoch FROM created_at) * 1000)::bigint AS created_ms
             FROM entitlement_history WHERE player_id = $1 AND ($2 = '' OR urn = $2)
             ORDER BY created_at DESC LIMIT $3",
            &[&(request.player_id as i64), &request.urn, &i64::from(limit)],
        )
        .await
        .map_err(internal)?;
    Ok(rows
        .iter()
        .map(|row| EntitlementHistoryEntry {
            urn: row.get("urn"),
            log_action: row.get("log_action"),
            log_message: row.get("log_message"),
            command_id: row.get("command_id"),
            created_at_ms: row.get("created_ms"),
        })
        .collect())
}

pub async fn seed_starters(db: &Postgres, request: SeedStarterEntitlementsRequest) -> Result<u64, Status> {
    let mut written = 0;
    for rows in request.entitlements.chunks(SEED_ROWS) {
        let column = |field: fn(&sc_proto::internal::StarterEntitlement) -> &String| -> Vec<&str> {
            rows.iter().map(|row| field(row).as_str()).collect()
        };
        let parameters: [&(dyn ToSql + Sync); 6] = [
            &column(|row| &row.class_guid),
            &column(|row| &row.item_type),
            &column(|row| &row.item_sub_type),
            &column(|row| &row.name),
            &column(|row| &row.source),
            &column(|row| &row.port_path),
        ];
        written += db
            .execute(
                "INSERT INTO starter_entitlements (class_guid, item_type, item_sub_type, name, source, port_path)
                 SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[])
                 ON CONFLICT (class_guid) DO UPDATE SET item_type = excluded.item_type,
                     item_sub_type = excluded.item_sub_type, name = excluded.name, source = excluded.source,
                     port_path = excluded.port_path",
                &parameters,
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl EntitlementInternal for Entitlements {
    type QueryEntitlementsStream = ReplyStream<QueryEntitlementsResponse>;

    async fn grant_purchase(
        &self,
        request: Request<GrantPurchaseRequest>,
    ) -> Result<Response<GrantPurchaseResponse>, Status> {
        let request = request.into_inner();
        let urn = grant_purchase(&self.db, &request).await?;
        info!(player = request.player_id, %urn, key = %request.key, "purchase granted");
        Ok(Response::new(GrantPurchaseResponse { urn }))
    }

    async fn set_rental_expiry(
        &self,
        request: Request<SetRentalExpiryRequest>,
    ) -> Result<Response<SetRentalExpiryResponse>, Status> {
        set_rental_expiry(&self.db, &request.into_inner()).await?;
        Ok(Response::new(SetRentalExpiryResponse {}))
    }

    async fn revoke_expired_rentals(
        &self,
        request: Request<RevokeExpiredRentalsRequest>,
    ) -> Result<Response<RevokeExpiredRentalsResponse>, Status> {
        let revoked = revoke_expired_rentals(&self.db, &request.into_inner().urns).await?;
        Ok(Response::new(RevokeExpiredRentalsResponse { revoked }))
    }

    async fn query_entitlements(
        &self,
        request: Request<QueryEntitlementsRequest>,
    ) -> Result<Response<Self::QueryEntitlementsStream>, Status> {
        let request = request.into_inner();
        if request.player_id == 0 {
            return Err(Status::invalid_argument("missing player"));
        }
        self.starter.ensure(&self.db, request.account_id, request.player_id).await.map_err(internal)?;
        let filter = request.filter.unwrap_or_default();
        let rows = query(&self.db, request.player_id, filter, request.include_persistent_properties).await?;
        Ok(Response::new(reply_stream(rows, |entitlements| QueryEntitlementsResponse { entitlements })))
    }

    async fn update_loadout(
        &self,
        request: Request<UpdateLoadoutRequest>,
    ) -> Result<Response<UpdateLoadoutResponse>, Status> {
        let request = request.into_inner();
        update_loadout(&self.db, request.player_id, &request.urn, &request.loadout, &request.command_id).await?;
        Ok(Response::new(UpdateLoadoutResponse {}))
    }

    async fn seed_starter_entitlements(
        &self,
        request: Request<SeedStarterEntitlementsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = seed_starters(&self.db, request.into_inner()).await?;
        info!(rows, "starter entitlements seeded");
        Ok(Response::new(SeedResponse { rows }))
    }

    async fn revoke_entitlement(
        &self,
        request: Request<RevokeEntitlementRequest>,
    ) -> Result<Response<RevokeEntitlementResponse>, Status> {
        let request = request.into_inner();
        let response = revoke(&self.db, &request).await?;
        if response.revoked {
            info!(player = request.player_id, urn = %request.urn, command = %request.command_id, "entitlement revoked");
        }
        Ok(Response::new(response))
    }

    async fn entitlement_history(
        &self,
        request: Request<EntitlementHistoryRequest>,
    ) -> Result<Response<EntitlementHistoryResponse>, Status> {
        let entries = history(&self.db, &request.into_inner()).await?;
        Ok(Response::new(EntitlementHistoryResponse { entries }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revocation_needs_an_entitlement_and_a_player() {
        assert!(check_revoke(&RevokeEntitlementRequest { urn: "u".into(), player_id: 1, ..Default::default() }).is_ok());
        for request in [
            RevokeEntitlementRequest { player_id: 1, ..Default::default() },
            RevokeEntitlementRequest { urn: "u".into(), ..Default::default() },
        ] {
            assert_eq!(check_revoke(&request).unwrap_err().code(), tonic::Code::InvalidArgument);
        }
        let none = EntitlementHistoryRequest::default();
        assert_eq!(history_limit(&none).unwrap_err().code(), tonic::Code::InvalidArgument);
        let many = EntitlementHistoryRequest { player_id: 1, limit: 10_000, ..Default::default() };
        assert_eq!(history_limit(&many).unwrap(), MAX_HISTORY);
    }

    #[test]
    fn an_unspecified_source_type_is_no_condition() {
        assert!(source_filter(vec![0]).is_empty());
        assert_eq!(source_filter(vec![0, 4]), [4]);
    }

    #[test]
    fn a_rental_is_claimed_as_it_is_bought() {
        assert_eq!(purchase_status(EntitlementSourceType::Rental as i32), EntitlementStatus::Undelivered as i32);
        assert_eq!(purchase_status(EntitlementSourceType::Platform as i32), EntitlementStatus::Packaged as i32);
    }

    #[test]
    fn a_purchase_needs_its_key_player_class_and_source() {
        let full = GrantPurchaseRequest {
            key: "k".into(),
            player_id: 1,
            entity_class_guid: "c".into(),
            source_type: EntitlementSourceType::Rental as i32,
            ..Default::default()
        };
        assert!(check_purchase(&full).is_ok());
        for request in [
            GrantPurchaseRequest { key: String::new(), ..full.clone() },
            GrantPurchaseRequest { player_id: 0, ..full.clone() },
            GrantPurchaseRequest { entity_class_guid: String::new(), ..full.clone() },
            GrantPurchaseRequest { source_type: 0, ..full.clone() },
        ] {
            assert_eq!(check_purchase(&request).unwrap_err().code(), tonic::Code::InvalidArgument);
        }
        let expiry = SetRentalExpiryRequest { urn: "u".into(), player_id: 1, rental_expiry_ms: 5, ..Default::default() };
        assert!(check_expiry(&expiry).is_ok());
        let none = SetRentalExpiryRequest { rental_expiry_ms: 0, ..expiry.clone() };
        assert_eq!(check_expiry(&none).unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-entitlement -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn purchases_and_rental_expiries_apply_once() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let geid = 930_000_000 + std::process::id() as u64;
        let history = |urn: String| {
            let db = db.clone();
            async move {
                let row = db.query_opt("SELECT count(*) FROM entitlement_history WHERE urn = $1", &[&urn]).await;
                row.unwrap().unwrap().get::<_, i64>(0)
            }
        };
        let past = 1_000;
        let grant = GrantPurchaseRequest {
            key: "k1".into(),
            account_id: 1,
            player_id: geid,
            source_type: EntitlementSourceType::Rental as i32,
            entity_class_guid: "ship".into(),
            rental_expiry_ms: past,
            log_message: "m".into(),
        };
        let urn = grant_purchase(&db, &grant).await.unwrap();
        assert_eq!(grant_purchase(&db, &grant).await.unwrap(), urn, "the key decides the urn");
        assert_eq!(history(urn.clone()).await, 1, "granted once");

        let owned = query(&db, geid, EntitlementFilter::default(), false).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].status, EntitlementStatus::Undelivered as i32);
        assert_eq!(owned[0].rental_expiry.unwrap().seconds, 1);

        // Expired, so revoked once. Moved on and reactivated, so kept.
        assert_eq!(revoke_expired_rentals(&db, &[urn.clone()]).await.unwrap(), [urn.clone()]);
        assert!(revoke_expired_rentals(&db, &[urn.clone()]).await.unwrap().is_empty());
        let later = SetRentalExpiryRequest {
            urn: urn.clone(),
            player_id: geid,
            rental_expiry_ms: 4_102_444_800_000,
            reactivate: true,
            ..Default::default()
        };
        set_rental_expiry(&db, &later).await.unwrap();
        set_rental_expiry(&db, &later).await.unwrap();
        assert_eq!(history(urn.clone()).await, 3, "created, revoked, renewed");
        assert!(revoke_expired_rentals(&db, &[urn.clone()]).await.unwrap().is_empty());
        let owned = query(&db, geid, EntitlementFilter::default(), false).await.unwrap();
        assert_eq!(owned[0].status, EntitlementStatus::Undelivered as i32);
        let elsewhere = SetRentalExpiryRequest { player_id: geid + 1, ..later.clone() };
        assert_eq!(set_rental_expiry(&db, &elsewhere).await.unwrap_err().code(), tonic::Code::NotFound);

        let revocation = RevokeEntitlementRequest { urn: urn.clone(), player_id: geid, log_message: "admin".into(), command_id: "a1".into() };
        let first = revoke(&db, &revocation).await.unwrap();
        assert_eq!((first.previous_status, first.revoked), (EntitlementStatus::Undelivered as i32, true));
        let again = revoke(&db, &revocation).await.unwrap();
        assert_eq!((again.previous_status, again.revoked), (EntitlementStatus::Revoked as i32, false));
        let elsewhere = RevokeEntitlementRequest { player_id: geid + 1, ..revocation.clone() };
        assert_eq!(revoke(&db, &elsewhere).await.unwrap_err().code(), tonic::Code::NotFound);
        let logged = super::history(&db, &EntitlementHistoryRequest { player_id: geid, urn: urn.clone(), limit: 0 }).await.unwrap();
        assert_eq!(logged.len(), 4, "created, revoked, renewed, revoked by the admin");
        assert_eq!((logged[0].log_action, logged[0].command_id.as_str()), (EntitlementLogAction::Revoked as i32, "a1"));

        update_loadout(&db, geid, &urn, &[1, 2], "c").await.unwrap();
        let missing = update_loadout(&db, geid, "none", &[1], "c").await.unwrap_err();
        assert_eq!(missing.code(), tonic::Code::NotFound);

        db.execute("DELETE FROM entitlement_history WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
        db.execute("DELETE FROM entitlements WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
    }

    /// Against a real database, as above.
    #[tokio::test]
    #[ignore]
    async fn seeded_starters_reach_a_player_on_first_query() {
        use sc_proto::internal::StarterEntitlement;
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let geid = 940_000_000 + std::process::id() as u64;
        let class = format!("test-starter-{geid}");
        let starter = StarterEntitlement { class_guid: class.clone(), name: "n".into(), source: "s".into(), ..Default::default() };
        let request = SeedStarterEntitlementsRequest { entitlements: vec![starter] };
        assert_eq!(seed_starters(&db, request).await.unwrap(), 1);
        crate::Starter::default().ensure(&db, 1, geid).await.unwrap();
        let filter = EntitlementFilter { entity_class_guid: vec![class.clone()], ..Default::default() };
        assert_eq!(query(&db, geid, filter, false).await.unwrap().len(), 1);
        db.execute("DELETE FROM starter_entitlements WHERE class_guid = $1", &[&class]).await.unwrap();
        db.execute("DELETE FROM entitlement_history WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
        db.execute("DELETE FROM entitlements WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
    }
}

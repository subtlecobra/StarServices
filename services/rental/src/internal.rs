//! `scbackend.internal.RentalInternal`: how sc-eatransaction creates, extends and looks up rentals,
//! which this service owns. Each call is safe to repeat: creating by the rental's id, extending by the
//! caller's key. The admin site lists and ends rentals: ending one makes it expire now, and the usual expiry, which
//! revokes the entitlement first, finishes it.

use sc_db::{internal, Postgres};
use sc_economy::{
    db,
    transaction::{retrying, Failure},
};
use sc_api::internal::{ rental_internal_server::RentalInternal, CreateRentalRequest, CreateRentalResponse, EndRentalRequest, EndRentalResponse, ExtendRentalRequest, ExtendRentalResponse, GetRentalRequest, GetRentalResponse, ListRentalsRequest, ListRentalsResponse, };
use sc_proto::{
    services::rental::v1::{Rental, RentalLogAction, RentalStatus},
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::{expire_rentals, rental, Rentals, COLUMNS};

const EXPIRE_MS: &str = "(extract(epoch FROM expire_at) * 1000)::bigint";

fn check_create(request: &CreateRentalRequest) -> Result<(), Status> {
    let missing = request.id.is_empty()
        || request.player_id == 0
        || request.entitlement_urn.is_empty()
        || request.entity_class_guid.is_empty();
    if missing || request.days == 0 || request.expire_at_ms <= 0 {
        return Err(Status::invalid_argument("a rental needs an id, a player, an entitlement, a class and a term"));
    }
    Ok(())
}

pub async fn create(db: &Postgres, request: &CreateRentalRequest) -> Result<(), Status> {
    check_create(request)?;
    db.execute(
        "WITH added AS (
             INSERT INTO rentals (id, status, began_at, expire_at, duration, account_id, player_id, entitlement_urn,
                                  entity_class_guid)
             VALUES ($1, $2, now(), to_timestamp($3::bigint / 1000.0), $4, $5, $6, $7, $8)
             ON CONFLICT DO NOTHING RETURNING id, player_id)
         INSERT INTO rental_history (rental_id, player_id, log_action, log_message)
         SELECT id, player_id, $9, $10 FROM added",
        &[
            &request.id,
            &(RentalStatus::Active as i32),
            &request.expire_at_ms,
            &(request.days as i32),
            &i64::from(request.account_id),
            &(request.player_id as i64),
            &request.entitlement_urn,
            &request.entity_class_guid,
            &(RentalLogAction::Created as i32),
            &request.log_message,
        ],
    )
    .await
    .map_err(internal)?;
    Ok(())
}

/// Lengthens a rental from its expiry, or from now for one that has ended, and makes it active again.
pub async fn extend(db: &Postgres, request: &ExtendRentalRequest) -> Result<ExtendRentalResponse, Failure> {
    if request.key.is_empty() || request.id.is_empty() || request.days == 0 {
        return Err(Status::invalid_argument("an extension needs a key, a rental and a term").into());
    }
    let geid = request.player_id as i64;
    let mut client = db.client().await.map_err(internal)?;
    let transaction = db!(client.transaction());
    let done = db!(transaction.query_opt(
        "SELECT c.rental_id, (extract(epoch FROM c.expire_at) * 1000)::bigint AS expire_ms, r.entitlement_urn
         FROM rental_commands c JOIN rentals r ON r.id = c.rental_id WHERE c.key = $1",
        &[&request.key],
    ));
    if let Some(done) = done {
        if done.get::<_, String>("rental_id") != request.id {
            return Err(Status::invalid_argument(format!("key {} extended another rental", request.key)).into());
        }
        return Ok(ExtendRentalResponse { expire_at_ms: done.get("expire_ms"), entitlement_urn: done.get("entitlement_urn") });
    }
    let extended = db!(transaction.query_opt(
        &format!(
            "UPDATE rentals SET status = $3, renewals = renewals + 1, duration = duration + $4,
                 began_at = COALESCE(began_at, now()),
                 expire_at = GREATEST(COALESCE(expire_at, now()), now()) + $4::int * interval '1 day'
             WHERE id = $1 AND player_id = $2
             RETURNING entitlement_urn, expire_at, {EXPIRE_MS} AS expire_ms"
        ),
        &[&request.id, &geid, &(RentalStatus::Active as i32), &(request.days as i32)],
    ))
    .ok_or_else(|| Status::not_found(format!("rental {}", request.id)))?;
    let expire_at: std::time::SystemTime = extended.get("expire_at");
    let recorded = db!(transaction.query_opt(
        "INSERT INTO rental_commands (key, rental_id, expire_at) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING
         RETURNING key",
        &[&request.key, &request.id, &expire_at],
    ));
    if recorded.is_none() {
        // A concurrent repeat committed first. Rolled back, the next attempt returns its result.
        return Err(Failure::Conflict(format!("extension {} raced", request.key)));
    }
    db!(transaction.execute(
        "INSERT INTO rental_history (rental_id, player_id, log_action, log_message) VALUES ($1, $2, $3, $4)",
        &[&request.id, &geid, &(RentalLogAction::Renewed as i32), &request.log_message],
    ));
    db!(transaction.commit());
    Ok(ExtendRentalResponse { expire_at_ms: extended.get("expire_ms"), entitlement_urn: extended.get("entitlement_urn") })
}

pub async fn get(db: &Postgres, request: &GetRentalRequest) -> Result<GetRentalResponse, Status> {
    let row = db
        .query_opt(
            "SELECT entity_class_guid, entitlement_urn FROM rentals WHERE id = $1 AND player_id = $2",
            &[&request.id, &(request.player_id as i64)],
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("rental {}", request.id)))?;
    Ok(GetRentalResponse { entity_class_guid: row.get("entity_class_guid"), entitlement_urn: row.get("entitlement_urn") })
}

pub async fn list(db: &Postgres, request: &ListRentalsRequest) -> Result<Vec<Rental>, Status> {
    if request.player_id == 0 {
        return Err(Status::invalid_argument("missing player"));
    }
    let rows = db
        .query(
            &format!(
                "SELECT {COLUMNS} FROM rentals WHERE player_id = $1 AND ($2 OR status <> $3) ORDER BY created_at DESC, id"
            ),
            &[&(request.player_id as i64), &request.include_finished, &(RentalStatus::Finished as i32)],
        )
        .await
        .map_err(internal)?;
    Ok(rows.iter().map(rental).collect())
}

/// Moves an active rental's expiry to now, once: a rental already due or finished is left as it is.
pub async fn end(db: &Postgres, request: &EndRentalRequest) -> Result<EndRentalResponse, Status> {
    if request.id.is_empty() || request.player_id == 0 {
        return Err(Status::invalid_argument("ending a rental needs the rental and its player"));
    }
    let geid = request.player_id as i64;
    let row = db
        .query_opt(
            &format!(
                "WITH before AS (SELECT id, {EXPIRE_MS} AS expire_ms FROM rentals WHERE id = $1 AND player_id = $2),
                 changed AS (
                     UPDATE rentals SET expire_at = now()
                     WHERE id = $1 AND player_id = $2 AND status = $3 AND expire_at > now() RETURNING id),
                 logged AS (
                     INSERT INTO rental_history (rental_id, player_id, log_action, log_message)
                     SELECT id, $2, $4, $5 FROM changed)
                 SELECT before.expire_ms, (SELECT count(*) FROM changed) AS changed FROM before"
            ),
            &[&request.id, &geid, &(RentalStatus::Active as i32), &(RentalLogAction::Cancelled as i32), &request.log_message],
        )
        .await
        .map_err(internal)?
        .ok_or_else(|| Status::not_found(format!("rental {}", request.id)))?;
    Ok(EndRentalResponse {
        ended: row.get::<_, i64>("changed") > 0,
        previous_expire_at_ms: row.get::<_, Option<i64>>("expire_ms").unwrap_or_default(),
    })
}

#[tonic::async_trait]
impl RentalInternal for Rentals {
    async fn create_rental(
        &self,
        request: Request<CreateRentalRequest>,
    ) -> Result<Response<CreateRentalResponse>, Status> {
        let request = request.into_inner();
        create(&self.db, &request).await?;
        info!(player = request.player_id, rental = %request.id, "rental created");
        Ok(Response::new(CreateRentalResponse {}))
    }

    async fn extend_rental(
        &self,
        request: Request<ExtendRentalRequest>,
    ) -> Result<Response<ExtendRentalResponse>, Status> {
        let request = request.into_inner();
        let response = retrying(|| extend(&self.db, &request)).await?;
        info!(player = request.player_id, rental = %request.id, "rental extended");
        Ok(Response::new(response))
    }

    async fn get_rental(&self, request: Request<GetRentalRequest>) -> Result<Response<GetRentalResponse>, Status> {
        Ok(Response::new(get(&self.db, &request.into_inner()).await?))
    }

    async fn list_rentals(&self, request: Request<ListRentalsRequest>) -> Result<Response<ListRentalsResponse>, Status> {
        Ok(Response::new(ListRentalsResponse { rentals: list(&self.db, &request.into_inner()).await? }))
    }

    /// Ends the rental now rather than at the next sweep. Should that fail, the sweep finishes it.
    async fn end_rental(&self, request: Request<EndRentalRequest>) -> Result<Response<EndRentalResponse>, Status> {
        let request = request.into_inner();
        let response = end(&self.db, &request).await?;
        if response.ended {
            info!(player = request.player_id, rental = %request.id, "rental ended early");
            if let Err(error) = expire_rentals(&self.db, &self.entitlements, self.graph.as_ref()).await {
                warn!(%error, "ended rental left for the sweep");
            }
        }
        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rental() -> CreateRentalRequest {
        CreateRentalRequest {
            id: "r".into(),
            account_id: 1,
            player_id: 2,
            entitlement_urn: "u".into(),
            entity_class_guid: "c".into(),
            days: 3,
            expire_at_ms: 1,
            log_message: String::new(),
        }
    }

    #[test]
    fn a_rental_needs_everything_that_identifies_it() {
        assert!(check_create(&rental()).is_ok());
        for refused in [
            CreateRentalRequest { id: String::new(), ..rental() },
            CreateRentalRequest { player_id: 0, ..rental() },
            CreateRentalRequest { entitlement_urn: String::new(), ..rental() },
            CreateRentalRequest { entity_class_guid: String::new(), ..rental() },
            CreateRentalRequest { days: 0, ..rental() },
            CreateRentalRequest { expire_at_ms: 0, ..rental() },
        ] {
            assert_eq!(check_create(&refused).unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-rental -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn creating_and_extending_repeat_safely() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let geid = 910_000_000 + std::process::id() as u64;
        let id = format!("test-rental-{geid}");
        let in_a_day = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64)
            + 86_400_000;
        let request = CreateRentalRequest { id: id.clone(), player_id: geid, expire_at_ms: in_a_day, ..rental() };
        create(&db, &request).await.unwrap();
        create(&db, &request).await.unwrap();
        let history = "SELECT count(*) FROM rental_history WHERE rental_id = $1";
        assert_eq!(db.query_opt(history, &[&id]).await.unwrap().unwrap().get::<_, i64>(0), 1);

        let found = get(&db, &GetRentalRequest { id: id.clone(), player_id: geid }).await.unwrap();
        assert_eq!((found.entity_class_guid.as_str(), found.entitlement_urn.as_str()), ("c", "u"));
        let elsewhere = get(&db, &GetRentalRequest { id: id.clone(), player_id: geid + 1 }).await;
        assert_eq!(elsewhere.unwrap_err().code(), tonic::Code::NotFound);

        let extension = ExtendRentalRequest { key: format!("{id}:x"), id: id.clone(), player_id: geid, days: 2, ..Default::default() };
        let first = retrying(|| extend(&db, &extension)).await.unwrap();
        let again = retrying(|| extend(&db, &extension)).await.unwrap();
        assert_eq!(first.expire_at_ms, again.expire_at_ms);
        assert!((first.expire_at_ms - (in_a_day + 2 * 86_400_000)).abs() < 1_000, "{}", first.expire_at_ms);
        assert_eq!(first.entitlement_urn, "u");
        let renewals = db.query_opt("SELECT renewals FROM rentals WHERE id = $1", &[&id]).await.unwrap().unwrap();
        assert_eq!(renewals.get::<_, i32>(0), 1);

        let listed = list(&db, &ListRentalsRequest { player_id: geid, include_finished: false }).await.unwrap();
        assert_eq!(listed.iter().map(|rental| rental.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
        let ending = EndRentalRequest { id: id.clone(), player_id: geid, log_message: "admin".into() };
        let ended = end(&db, &ending).await.unwrap();
        assert!(ended.ended);
        assert_eq!(ended.previous_expire_at_ms, first.expire_at_ms);
        assert!(!end(&db, &ending).await.unwrap().ended, "a repeat changes nothing");
        let elsewhere = EndRentalRequest { player_id: geid + 1, ..ending.clone() };
        assert_eq!(end(&db, &elsewhere).await.unwrap_err().code(), tonic::Code::NotFound);

        db.execute("DELETE FROM rental_commands WHERE rental_id = $1", &[&id]).await.unwrap();
        db.execute("DELETE FROM rental_history WHERE rental_id = $1", &[&id]).await.unwrap();
        db.execute("DELETE FROM rentals WHERE id = $1", &[&id]).await.unwrap();
    }
}

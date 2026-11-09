//! `scbackend.internal.entity_document.v1.ShipRecordsInternal`: the durable record of each ship a player owns, written
//! when the ship is acquired and replaced by every state change (see docs/SHIPS.md). It is the source of truth: the
//! entity in sc-entitygraph is rebuilt from it when the two disagree. A record is protobuf bytes, with the columns the
//! queries filter on kept beside it.

use prost::Message;
use sc_api::internal::{
    ship_records_internal_server::ShipRecordsInternal, GetShipsRequest, GetShipsResponse, RecordShipRequest, RecordShipResponse,
    ReplaceShipRequest, ReplaceShipResponse, ShipRecord,
};
use sc_db::{internal, Row};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Documents;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ship_records (
    ship_uid TEXT PRIMARY KEY,
    command_id TEXT NOT NULL UNIQUE,
    owner_id BIGINT NOT NULL,
    revision BIGINT NOT NULL,
    graph_synced BOOLEAN NOT NULL,
    body BYTEA NOT NULL
);
CREATE INDEX IF NOT EXISTS ship_records_owner ON ship_records (owner_id);";

const COLUMNS: &str = "ship_uid, revision, body";

fn record(row: &Row) -> Result<ShipRecord, Status> {
    let body: Vec<u8> = row.get("body");
    let mut record = ShipRecord::decode(body.as_slice()).map_err(internal)?;
    record.ship_uid = row.get("ship_uid");
    record.revision = row.get::<_, i64>("revision") as u64;
    Ok(record)
}

/// What a record must have to be kept.
fn check(record: &ShipRecord) -> Result<(), Status> {
    let need = |ok: bool, what: &str| if ok { Ok(()) } else { Err(Status::invalid_argument(format!("a ship needs {what}"))) };
    need(!record.ship_uid.is_empty() && record.ship_uid.len() <= 200, "a ship uid of 1 to 200 characters")?;
    need(!record.command_id.is_empty() && record.command_id.len() <= 200, "a command id of 1 to 200 characters")?;
    need(record.owner_id != 0, "an owner")?;
    need(!record.entity_class_guid.is_empty(), "an entity class")
}

fn encoded(record: &ShipRecord) -> Vec<u8> {
    // The revision is a column; the body keeps it zeroed so a write never disagrees with its row.
    ShipRecord { revision: 0, ..record.clone() }.encode_to_vec()
}

#[tonic::async_trait]
impl ShipRecordsInternal for Documents {
    async fn record_ship(&self, request: Request<RecordShipRequest>) -> Result<Response<RecordShipResponse>, Status> {
        let mut record = request.into_inner().record.ok_or_else(|| Status::invalid_argument("missing record"))?;
        check(&record)?;
        record.revision = 1;
        record.graph_synced = false;
        record.geid = 0;
        let body = encoded(&record);
        let added = self
            .db
            .query_opt(
                &format!(
                    "INSERT INTO ship_records (ship_uid, command_id, owner_id, revision, graph_synced, body)
                     VALUES ($1, $2, $3, 1, FALSE, $4) ON CONFLICT DO NOTHING RETURNING {COLUMNS}"
                ),
                &[&record.ship_uid, &record.command_id, &(record.owner_id as i64), &body],
            )
            .await
            .map_err(internal)?;
        if let Some(row) = added {
            info!(ship = %record.ship_uid, owner = record.owner_id, "ship recorded");
            return Ok(Response::new(RecordShipResponse { record: Some(self::record(&row)?), created: true }));
        }
        let found = self
            .db
            .query_opt(
                &format!("SELECT {COLUMNS} FROM ship_records WHERE command_id = $1 OR ship_uid = $2 ORDER BY (command_id = $1) DESC LIMIT 1"),
                &[&record.command_id, &record.ship_uid],
            )
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::aborted("the ship record changed meanwhile"))?;
        Ok(Response::new(RecordShipResponse { record: Some(self::record(&found)?), created: false }))
    }

    async fn get_ships(&self, request: Request<GetShipsRequest>) -> Result<Response<GetShipsResponse>, Status> {
        let request = request.into_inner();
        if request.owner_id == 0 {
            return Err(Status::invalid_argument("an owner is needed"));
        }
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM ship_records WHERE owner_id = $1
                     AND (cardinality($2::text[]) = 0 OR ship_uid = ANY($2)) AND (NOT $3 OR NOT graph_synced) ORDER BY ship_uid"
                ),
                &[&(request.owner_id as i64), &request.ship_uids, &request.unsynced_only],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(GetShipsResponse { ships: rows.iter().map(record).collect::<Result<_, _>>()? }))
    }

    async fn replace_ship(&self, request: Request<ReplaceShipRequest>) -> Result<Response<ReplaceShipResponse>, Status> {
        let request = request.into_inner();
        let new = request.record.ok_or_else(|| Status::invalid_argument("missing record"))?;
        check(&new)?;
        let body = encoded(&new);
        let changed = self
            .db
            .query_opt(
                &format!(
                    "UPDATE ship_records SET body = $3, graph_synced = $4, revision = revision + 1
                     WHERE ship_uid = $1 AND revision = $2 AND owner_id = $5 RETURNING {COLUMNS}"
                ),
                &[&new.ship_uid, &(request.expected_revision as i64), &body, &new.graph_synced, &(new.owner_id as i64)],
            )
            .await
            .map_err(internal)?;
        match changed {
            Some(row) => Ok(Response::new(ReplaceShipResponse { record: Some(record(&row)?) })),
            None => {
                let exists = self
                    .db
                    .query_opt("SELECT 1 FROM ship_records WHERE ship_uid = $1 AND owner_id = $2", &[&new.ship_uid, &(new.owner_id as i64)])
                    .await
                    .map_err(internal)?;
                Err(if exists.is_some() { Status::aborted(format!("ship {} changed meanwhile", new.ship_uid)) } else { Status::not_found(format!("no ship {}", new.ship_uid)) })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_api::internal::ShipState;

    fn ship(uid: &str, owner: u64) -> ShipRecord {
        ShipRecord {
            ship_uid: uid.into(),
            command_id: format!("acquire:{uid}"),
            owner_id: owner,
            entity_class_guid: "guid".into(),
            state: ShipState::Stored as i32,
            ..Default::default()
        }
    }

    #[test]
    fn a_record_needs_its_keys() {
        assert!(check(&ship("a", 1)).is_ok());
        assert!(check(&ship("", 1)).is_err());
        assert!(check(&ship("a", 0)).is_err());
        assert!(check(&ShipRecord { command_id: String::new(), ..ship("a", 1) }).is_err());
        assert!(check(&ShipRecord { entity_class_guid: String::new(), ..ship("a", 1) }).is_err());
    }

    #[test]
    fn the_body_leaves_the_revision_to_the_row() {
        let record = ShipRecord { revision: 9, ..ship("a", 1) };
        assert_eq!(ShipRecord::decode(encoded(&record).as_slice()).unwrap().revision, 0);
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-entity-document -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn records_are_written_once_and_replaced_at_their_revision() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let documents = Documents { db: sc_db::Postgres::connect(&url, SCHEMA).await.unwrap(), classes: crate::class_names().unwrap() };
        let owner = 960_000_000 + u64::from(std::process::id());
        let uid = format!("ship-{owner}");
        let first = documents.record_ship(Request::new(RecordShipRequest { record: Some(ship(&uid, owner)) })).await.unwrap().into_inner();
        assert!(first.created);
        let first = first.record.unwrap();
        assert_eq!((first.revision, first.graph_synced), (1, false));
        let again = documents.record_ship(Request::new(RecordShipRequest { record: Some(ship(&uid, owner)) })).await.unwrap().into_inner();
        assert!(!again.created, "the same command records nothing twice");
        assert_eq!(again.record.unwrap().revision, 1);
        let unsynced = GetShipsRequest { owner_id: owner, ship_uids: vec![], unsynced_only: true };
        assert_eq!(documents.get_ships(Request::new(unsynced.clone())).await.unwrap().into_inner().ships.len(), 1);

        let synced = ShipRecord { geid: 7, graph_synced: true, ..first.clone() };
        let replaced = documents.replace_ship(Request::new(ReplaceShipRequest { record: Some(synced.clone()), expected_revision: 1 })).await.unwrap().into_inner().record.unwrap();
        assert_eq!((replaced.revision, replaced.geid, replaced.graph_synced), (2, 7, true));
        let stale = documents.replace_ship(Request::new(ReplaceShipRequest { record: Some(synced), expected_revision: 1 })).await.unwrap_err();
        assert_eq!(stale.code(), tonic::Code::Aborted);
        let missing = documents.replace_ship(Request::new(ReplaceShipRequest { record: Some(ship("nobody", owner)), expected_revision: 1 })).await.unwrap_err();
        assert_eq!(missing.code(), tonic::Code::NotFound);
        assert!(documents.get_ships(Request::new(unsynced)).await.unwrap().into_inner().ships.is_empty());
        documents.db.execute("DELETE FROM ship_records WHERE owner_id = $1", &[&(owner as i64)]).await.unwrap();
    }
}

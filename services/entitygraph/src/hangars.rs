//! The hangars a player has (see docs/HANGARS.md): one `hangars` row per (player, location), keyed `<owner>:<location>`.
//! The DGS opens and owns the live instance; this is the record of which hangars exist and when each was last used.
//! The location's hangar list is game data this service does not have, so the caller names the default class.

use prost::Message as _;
use sc_api::internal::{
    hangars_internal_server::HangarsInternal, EnsureHangarRequest, EnsureHangarResponse, HangarRecord, HangarSize, ListHangarsRequest,
    ListHangarsResponse,
};
use sc_db::{internal, Arango};
use serde_json::{json, Value};
use tonic::{Request, Response, Status};

use crate::{
    admin::{invalid, need_owner},
    now_ms,
    ships::rules::hangar_size_of,
};

/// The collection of the rows.
pub const HANGARS: &str = "hangars";
/// Separates the owner from the location in a row's key.
const KEY_SEPARATOR: char = ':';

pub struct Hangars {
    pub db: Arango,
}

pub fn key(owner: u64, location: u32) -> String {
    format!("{owner}{KEY_SEPARATOR}{location}")
}

fn row(record: &HangarRecord) -> Value {
    json!({
        "_key": key(record.owner_id, record.location_id), "ownerId": record.owner_id.to_string(), "locationId": record.location_id,
        "hangarClass": record.hangar_class, "size": record.size, "lastUsed": record.last_used_ms, "createdAt": record.created_at_ms,
    })
}

fn record(row: &Value) -> Option<HangarRecord> {
    Some(HangarRecord {
        owner_id: row["ownerId"].as_str()?.parse().ok()?,
        location_id: row["locationId"].as_u64()? as u32,
        hangar_class: row["hangarClass"].as_str()?.to_string(),
        size: row["size"].as_i64()? as i32,
        last_used_ms: row["lastUsed"].as_i64().unwrap_or_default(),
        created_at_ms: row["createdAt"].as_i64().unwrap_or_default(),
    })
}

/// The record a new hangar gets, or the refusal.
pub fn new_record(request: &EnsureHangarRequest, now: i64) -> Result<HangarRecord, Status> {
    if request.location_id == 0 {
        return Err(invalid("a location is needed"));
    }
    let size = match request.size() {
        HangarSize::Unspecified => hangar_size_of(&request.hangar_class),
        given => given,
    };
    if request.hangar_class.is_empty() || size == HangarSize::Unspecified {
        return Err(invalid("a new hangar needs a hangar class that names a size"));
    }
    Ok(HangarRecord {
        owner_id: request.owner_id,
        location_id: request.location_id,
        hangar_class: request.hangar_class.clone(),
        size: size as i32,
        last_used_ms: now,
        created_at_ms: now,
    })
}

impl Hangars {
    pub async fn list(&self, request: ListHangarsRequest) -> Result<ListHangarsResponse, Status> {
        need_owner(request.owner_id)?;
        let query = "FOR h IN hangars FILTER h.ownerId == @o AND (@l == 0 OR h.locationId == @l) SORT h.locationId, h.hangarClass RETURN h";
        let rows = self.db.aql(query, json!({ "o": request.owner_id.to_string(), "l": request.location_id })).await.map_err(internal)?;
        Ok(ListHangarsResponse { hangars: rows.iter().filter_map(record).collect() })
    }

    pub async fn ensure(&self, request: EnsureHangarRequest) -> Result<EnsureHangarResponse, Status> {
        need_owner(request.owner_id)?;
        let now = if request.now_ms == 0 { now_ms() as i64 } else { request.now_ms };
        let found = self.db.aql("RETURN DOCUMENT(@c, @k)", json!({ "c": HANGARS, "k": key(request.owner_id, request.location_id) })).await.map_err(internal)?;
        if let Some(mut held) = found.iter().find(|found| !found.is_null()).and_then(record) {
            held.last_used_ms = now;
            self.db
                .aql("UPDATE @k WITH { lastUsed: @t } IN hangars", json!({ "k": key(held.owner_id, held.location_id), "t": now }))
                .await
                .map_err(internal)?;
            return Ok(EnsureHangarResponse { hangar: Some(held), created: false });
        }
        let new = new_record(&request, now)?;
        // Two callers racing for the same hangar: the loser's insert is ignored and it answers with the winner's row.
        let inserted = self
            .db
            .aql("INSERT @row INTO hangars OPTIONS { overwriteMode: 'ignore' } RETURN NEW", json!({ "row": row(&new) }))
            .await
            .map_err(internal)?;
        let created = !inserted.is_empty() && inserted[0]["createdAt"].as_i64() == Some(now);
        let hangar = inserted.iter().find_map(record).unwrap_or(new);
        Ok(EnsureHangarResponse { hangar: Some(hangar), created })
    }
}

#[tonic::async_trait]
impl HangarsInternal for Hangars {
    async fn list_hangars(&self, request: Request<ListHangarsRequest>) -> Result<Response<ListHangarsResponse>, Status> {
        Ok(Response::new(self.list(request.into_inner()).await?))
    }
    async fn ensure_hangar(&self, request: Request<EnsureHangarRequest>) -> Result<Response<EnsureHangarResponse>, Status> {
        Ok(Response::new(self.ensure(request.into_inner()).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(class: &str, size: HangarSize) -> EnsureHangarRequest {
        EnsureHangarRequest { owner_id: 7, location_id: 42, hangar_class: class.into(), size: size as i32, now_ms: 1000 }
    }

    #[test]
    fn a_new_hangar_takes_its_size_from_the_class_name() {
        let made = new_record(&request("Hangar_XLTop_Area18", HangarSize::Unspecified), 1000).unwrap();
        assert_eq!((made.size, made.last_used_ms, made.created_at_ms), (HangarSize::Xlarge as i32, 1000, 1000));
        let given = new_record(&request("Hangar_SmallFront_Orison", HangarSize::Medium), 5).unwrap();
        assert_eq!(given.size, HangarSize::Medium as i32, "an explicit size wins");
    }

    #[test]
    fn a_new_hangar_needs_a_location_and_a_class_with_a_size() {
        assert!(new_record(&request("", HangarSize::Unspecified), 1).is_err());
        assert!(new_record(&request("Garage_Thing", HangarSize::Unspecified), 1).is_err());
        let mut no_location = request("Hangar_SmallFront_Orison", HangarSize::Unspecified);
        no_location.location_id = 0;
        assert_eq!(new_record(&no_location, 1).unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_row_round_trips_and_is_keyed_by_owner_and_location() {
        let made = new_record(&request("Hangar_LargeFront_Lorville", HangarSize::Unspecified), 9).unwrap();
        let stored = row(&made);
        assert_eq!(stored["_key"], "7:42");
        assert_eq!(record(&stored).unwrap().encode_to_vec(), made.encode_to_vec());
    }

    /// Against a real ArangoDB: `SC_TEST_ARANGO=<url> cargo test -p sc-entitygraph -- --ignored hangars`.
    #[tokio::test]
    #[ignore]
    async fn hangars_are_recorded_once_per_location_and_listed() {
        let url = std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO");
        let hangars = Hangars { db: crate::open(&url).await.unwrap() };
        let owner = 600_000_000_000 + std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64 % 1_000_000_000;
        let ensure = |location: u32, class: &str, now: i64| EnsureHangarRequest { owner_id: owner, location_id: location, hangar_class: class.into(), now_ms: now, ..Default::default() };
        let first = hangars.ensure(ensure(2, "Hangar_SmallFront_Orison", 100)).await.unwrap();
        assert!(first.created);
        let again = hangars.ensure(ensure(2, "Hangar_XLTop_Other", 200)).await.unwrap();
        assert!(!again.created);
        let held = again.hangar.unwrap();
        assert_eq!((held.hangar_class.as_str(), held.last_used_ms, held.created_at_ms), ("Hangar_SmallFront_Orison", 200, 100), "the class stays; the use moves");
        assert!(hangars.ensure(ensure(1, "Hangar_LargeTop_Area18", 300)).await.unwrap().created);
        let all = hangars.list(ListHangarsRequest { owner_id: owner, location_id: 0 }).await.unwrap().hangars;
        assert_eq!(all.iter().map(|h| h.location_id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(hangars.list(ListHangarsRequest { owner_id: owner, location_id: 2 }).await.unwrap().hangars.len(), 1);
        assert!(hangars.list(ListHangarsRequest { owner_id: owner + 1, location_id: 0 }).await.unwrap().hangars.is_empty());
    }
}

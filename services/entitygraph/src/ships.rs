//! Ships (see docs/SHIPS.md). The durable record of a ship is sc-entity-document's and is written first; this module
//! keeps the ship's entity in the graph (stowed in the owner's hangar bank, `<owner>:Hangar:<location>`) and a
//! `ships` document beside it with the fields the kiosk's queries filter on. In-game queries read the graph; when it
//! lacks a ship or holds a stale one, it is made right from the record, which wins every disagreement.
//!
//! `rules` is the game's logic and has no I/O: pad classes and which hangars a ship fits, the states, the claim.

use std::{collections::HashMap, sync::Arc};

use prost::Message;
use sc_api::internal::{
    ship_records_internal_client::ShipRecordsInternalClient, ships_internal_server::ShipsInternal, AcquireShipRequest,
    AcquireShipResponse, ClaimShipRequest, ClaimShipResponse, GetShipsRequest, ListShipsRequest, ListShipsResponse, RecordShipRequest,
    ReplaceShipRequest, ReportShipRequest, ReportShipResponse, RetrieveShipRequest, RetrieveShipResponse, ShipRecord, ShipView,
};
use sc_db::{internal, Arango};
use sc_proto::services::entitygraph::v1::EntityNodeProperties;
use serde_json::{json, Value};
use tonic::{transport::Channel, Request, Response, Status};
use tracing::{info, warn};

use crate::{
    admin::{allocate_geids, create_into, invalid, need_command, need_owner, need_target, refuse_taken},
    commands::{Plan, WRITES},
    doc, now_ms,
};

pub mod rules {
    //! The game's rules for a ship, from the game data of 4.10.2 unless marked inferred.

    use sc_api::internal::{
        HangarSize, PadSize, ShipClaim, ShipClaimKind, ShipInsurance, ShipPolicy, ShipRecord, ShipState,
    };

    /// The `LandingPadSize` records: the box (x, y, z in metres) a ship must fit, by pad class.
    pub const PADS: [(PadSize, [f64; 3]); 6] = [
        (PadSize::Tiny, [12.0, 16.0, 6.0]),
        (PadSize::Xsmall, [24.0, 32.0, 12.0]),
        (PadSize::Small, [48.0, 48.0, 16.0]),
        (PadSize::Medium, [56.0, 88.0, 18.0]),
        (PadSize::Large, [72.0, 128.0, 36.0]),
        (PadSize::Xlarge, [160.0, 272.0, 64.0]),
    ];
    /// `ItemRecoveryEconomyParams.claimCostMultiplier`: a claim costs this fraction of the ship's price. Which price
    /// the game takes is not read (inferred: the purchase price).
    pub const CLAIM_COST_MULTIPLIER: f64 = 0.003;

    /// The smallest pad class whose box holds `bounding_box` (`VehicleComponentParams.maxBoundingBox`: x width, y
    /// length, z height). A ship may sit turned, so x and y are compared both ways (inferred).
    pub fn pad_for(bounding_box: [f64; 3]) -> PadSize {
        let [x, y, z] = bounding_box;
        PADS.iter()
            .find(|(_, [px, py, pz])| z <= *pz && ((x <= *px && y <= *py) || (y <= *px && x <= *py)))
            .map_or(PadSize::Unspecified, |(pad, _)| *pad)
    }

    /// The hangar size named by a hangar class such as `Hangar_SmallFront_Orison` (`Hangar_XLTop_Area18`).
    pub fn hangar_size_of(class_name: &str) -> HangarSize {
        let kind = class_name.strip_prefix("Hangar_").unwrap_or_default();
        for (prefix, size) in [("Small", HangarSize::Small), ("Medium", HangarSize::Medium), ("Large", HangarSize::Large), ("XL", HangarSize::Xlarge)] {
            if kind.starts_with(prefix) {
                return size;
            }
        }
        HangarSize::Unspecified
    }

    /// Whether a ship of pad class `pad` goes in a hangar of `hangar`: it fits when its pad class is not larger, and
    /// with `exact` only when it is the same (the hangar sizes are the pad classes Small to XLarge). A ship whose
    /// size is not known fits nowhere; no hangar filter lists everything.
    pub fn fits(pad: PadSize, hangar: HangarSize, exact: bool) -> bool {
        if hangar == HangarSize::Unspecified {
            return true;
        }
        if pad == PadSize::Unspecified {
            return false;
        }
        let (pad, hangar) = (pad as i32, hangar as i32);
        if exact { pad.max(HangarSize::Small as i32) == hangar } else { pad <= hangar }
    }

    /// The state at `now`: a claim whose time has come left the ship in the hangar bank, and a policy that ran out is
    /// no policy.
    pub fn at(record: &ShipRecord, now_ms: i64) -> ShipRecord {
        let mut record = record.clone();
        if record.state == ShipState::Claiming as i32 && record.claim.as_ref().is_some_and(|claim| claim.ready_at_ms <= now_ms) {
            record.state = ShipState::Stored as i32;
        }
        record
    }

    pub fn insured(insurance: Option<&ShipInsurance>, now_ms: i64) -> bool {
        insurance.is_some_and(|insurance| match insurance.policy() {
            ShipPolicy::Lifetime => true,
            ShipPolicy::Duration => insurance.expires_at_ms > now_ms,
            ShipPolicy::Unspecified => false,
        })
    }

    /// What a claim of `kind` would take if started at `now`: the class's base wait, a cost that is a fraction of the
    /// price, and the moment it can be paid to finish early. `chain` counts the claims in a row; the game's
    /// escalation of a chain (limit 1, every multiplier 1.0 in 4.10.2) changes nothing and is not applied. Both kinds
    /// take the same time and cost (inferred: the default loadout multipliers of the game data are not understood).
    pub fn quote(record: &ShipRecord, kind: ShipClaimKind, now_ms: i64) -> ShipClaim {
        let insurance = record.insurance.clone().unwrap_or_default();
        let minutes = |minutes: f64| (minutes * 60_000.0).round() as i64;
        ShipClaim {
            kind: kind as i32,
            started_at_ms: now_ms,
            ready_at_ms: now_ms + minutes(insurance.base_wait_minutes),
            expedite_after_ms: now_ms + minutes(insurance.mandatory_wait_minutes),
            cost_auec: (record.price_auec as f64 * CLAIM_COST_MULTIPLIER).round() as u64,
            chain: 1,
            command_id: String::new(),
        }
    }

    pub type Refused = String;

    /// DESTROYED to CLAIMING. An insured claim needs a live policy; a default claim does not.
    pub fn claim(record: &ShipRecord, kind: ShipClaimKind, now_ms: i64) -> Result<ShipRecord, Refused> {
        let record = at(record, now_ms);
        if record.state != ShipState::Destroyed as i32 {
            return Err(format!("the ship is {:?}, not destroyed", record.state()));
        }
        if kind == ShipClaimKind::Unspecified {
            return Err("a claim is default or insured".into());
        }
        if kind == ShipClaimKind::Insured && !insured(record.insurance.as_ref(), now_ms) {
            return Err("the ship has no insurance policy in force".into());
        }
        Ok(ShipRecord { state: ShipState::Claiming as i32, claim: Some(quote(&record, kind, now_ms)), ..record })
    }

    /// STORED to DEPLOYED, into a hangar the ship fits.
    pub fn retrieve(record: &ShipRecord, hangar: HangarSize, exact: bool, now_ms: i64) -> Result<ShipRecord, Refused> {
        let record = at(record, now_ms);
        match record.state() {
            ShipState::Stored => {}
            ShipState::Claiming => return Err("the claim has not finished".into()),
            state => return Err(format!("the ship is {state:?}, not stored")),
        }
        if hangar == HangarSize::Unspecified || !fits(record.pad_size(), hangar, exact) {
            return Err("the ship does not fit that hangar".into());
        }
        Ok(ShipRecord { state: ShipState::Deployed as i32, ..record })
    }

    /// DEPLOYED to STORED (put away at `location_id`) or DESTROYED.
    pub fn report(record: &ShipRecord, to: ShipState, location_id: u32) -> Result<ShipRecord, Refused> {
        if record.state() != ShipState::Deployed {
            return Err(format!("the ship is {:?}, not deployed", record.state()));
        }
        match to {
            ShipState::Stored if location_id != 0 => Ok(ShipRecord { state: to as i32, location_id, ..record.clone() }),
            ShipState::Stored => Err("a location to store it at is needed".into()),
            ShipState::Destroyed => Ok(ShipRecord { state: to as i32, ..record.clone() }),
            _ => Err("a deployed ship is stored or destroyed".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ship(state: ShipState) -> ShipRecord {
            ShipRecord {
                state: state as i32,
                pad_size: PadSize::Small as i32,
                price_auec: 1_000_000,
                insurance: Some(ShipInsurance { policy: ShipPolicy::Lifetime as i32, base_wait_minutes: 4.05, mandatory_wait_minutes: 1.35, ..Default::default() }),
                ..Default::default()
            }
        }

        #[test]
        fn a_ship_takes_the_smallest_pad_it_fits() {
            // The Avenger Titan's `maxBoundingBox` of 4.10.2.
            assert_eq!(pad_for([15.0, 20.0, 6.5]), PadSize::Xsmall);
            assert_eq!(pad_for([10.0, 14.0, 5.0]), PadSize::Tiny);
            assert_eq!(pad_for([16.0, 12.0, 5.0]), PadSize::Tiny, "turned");
            assert_eq!(pad_for([50.0, 80.0, 17.0]), PadSize::Medium);
            assert_eq!(pad_for([70.0, 120.0, 40.0]), PadSize::Xlarge, "too tall for Large");
            assert_eq!(pad_for([300.0, 300.0, 10.0]), PadSize::Unspecified);
        }

        #[test]
        fn hangar_sizes_come_from_the_class_names() {
            assert_eq!(hangar_size_of("Hangar_SmallFront_Orison"), HangarSize::Small);
            assert_eq!(hangar_size_of("Hangar_MediumTop_Area18"), HangarSize::Medium);
            assert_eq!(hangar_size_of("Hangar_LargeTop_RestStop_Pyro"), HangarSize::Large);
            assert_eq!(hangar_size_of("Hangar_XLTop_Lorville"), HangarSize::Xlarge);
            assert_eq!(hangar_size_of("Station_Lobby"), HangarSize::Unspecified);
        }

        #[test]
        fn a_ship_fits_the_hangars_of_its_size_and_up() {
            assert!(fits(PadSize::Xsmall, HangarSize::Small, false));
            assert!(fits(PadSize::Xsmall, HangarSize::Xlarge, false));
            assert!(!fits(PadSize::Medium, HangarSize::Small, false));
            assert!(fits(PadSize::Medium, HangarSize::Medium, true));
            assert!(!fits(PadSize::Medium, HangarSize::Large, true), "exact");
            assert!(fits(PadSize::Tiny, HangarSize::Small, true), "the smallest hangar is Small");
            assert!(!fits(PadSize::Unspecified, HangarSize::Small, false));
            assert!(fits(PadSize::Unspecified, HangarSize::Unspecified, false));
        }

        #[test]
        fn a_finished_claim_is_a_stored_ship() {
            let destroyed = ship(ShipState::Destroyed);
            let claiming = claim(&destroyed, ShipClaimKind::Insured, 1_000).unwrap();
            let claim = claiming.claim.clone().unwrap();
            assert_eq!(claiming.state(), ShipState::Claiming);
            assert_eq!((claim.ready_at_ms, claim.expedite_after_ms, claim.cost_auec), (1_000 + 243_000, 1_000 + 81_000, 3_000));
            assert_eq!(at(&claiming, 243_999).state(), ShipState::Claiming);
            assert_eq!(at(&claiming, 244_000).state(), ShipState::Stored);
        }

        #[test]
        fn claims_need_a_destroyed_ship_and_for_insured_a_policy() {
            assert!(claim(&ship(ShipState::Stored), ShipClaimKind::Default, 0).is_err());
            assert!(claim(&ship(ShipState::Deployed), ShipClaimKind::Default, 0).is_err());
            let mut lapsed = ship(ShipState::Destroyed);
            lapsed.insurance = Some(ShipInsurance { policy: ShipPolicy::Duration as i32, expires_at_ms: 500, ..Default::default() });
            assert!(claim(&lapsed, ShipClaimKind::Insured, 600).is_err());
            assert!(claim(&lapsed, ShipClaimKind::Insured, 400).is_ok());
            assert!(claim(&lapsed, ShipClaimKind::Default, 600).is_ok(), "the default claim needs no policy");
            lapsed.insurance = None;
            assert!(claim(&lapsed, ShipClaimKind::Insured, 0).is_err());
        }

        #[test]
        fn a_ship_is_retrieved_only_when_stored_and_fitting() {
            let stored = ship(ShipState::Stored);
            assert_eq!(retrieve(&stored, HangarSize::Small, false, 0).unwrap().state(), ShipState::Deployed);
            assert!(retrieve(&ShipRecord { pad_size: PadSize::Large as i32, ..stored.clone() }, HangarSize::Small, false, 0).is_err());
            assert!(retrieve(&stored, HangarSize::Unspecified, false, 0).is_err());
            assert!(retrieve(&ship(ShipState::Destroyed), HangarSize::Small, false, 0).is_err());
            assert!(retrieve(&ship(ShipState::Expired), HangarSize::Small, false, 0).is_err());
            let claiming = claim(&ship(ShipState::Destroyed), ShipClaimKind::Default, 0).unwrap();
            assert!(retrieve(&claiming, HangarSize::Small, false, 1).is_err(), "still claiming");
            assert!(retrieve(&claiming, HangarSize::Small, false, 300_000).is_ok());
        }

        #[test]
        fn a_deployed_ship_is_stored_or_lost() {
            let out = ship(ShipState::Deployed);
            assert_eq!(report(&out, ShipState::Stored, 9).unwrap().location_id, 9);
            assert!(report(&out, ShipState::Stored, 0).is_err());
            assert_eq!(report(&out, ShipState::Destroyed, 0).unwrap().state(), ShipState::Destroyed);
            assert!(report(&out, ShipState::Claiming, 9).is_err());
            assert!(report(&ship(ShipState::Stored), ShipState::Destroyed, 0).is_err());
        }
    }
}

/// The durable records, as sc-entity-document answers them. A trait so the tests can fail a write.
#[tonic::async_trait]
pub trait Records: Send + Sync {
    async fn record(&self, record: ShipRecord) -> Result<ShipRecord, Status>;
    async fn all(&self, owner: u64) -> Result<Vec<ShipRecord>, Status>;
    async fn replace(&self, record: ShipRecord, expected_revision: u64) -> Result<ShipRecord, Status>;
}

#[tonic::async_trait]
impl Records for ShipRecordsInternalClient<Channel> {
    async fn record(&self, record: ShipRecord) -> Result<ShipRecord, Status> {
        let reply = self.clone().record_ship(RecordShipRequest { record: Some(record) }).await?.into_inner();
        reply.record.ok_or_else(|| Status::internal("no record"))
    }

    async fn all(&self, owner: u64) -> Result<Vec<ShipRecord>, Status> {
        Ok(self.clone().get_ships(GetShipsRequest { owner_id: owner, ..Default::default() }).await?.into_inner().ships)
    }

    async fn replace(&self, record: ShipRecord, expected_revision: u64) -> Result<ShipRecord, Status> {
        let reply = self.clone().replace_ship(ReplaceShipRequest { record: Some(record), expected_revision }).await?.into_inner();
        reply.record.ok_or_else(|| Status::internal("no record"))
    }
}

pub struct Ships {
    pub db: Arango,
    pub records: Arc<dyn Records>,
}

fn bank(owner: u64, location: u32) -> String {
    format!("{owner}:Hangar:{location}")
}

fn stored(record: &ShipRecord) -> String {
    doc::hex(&record.encode_to_vec())
}

fn row_record(row: &Value) -> Option<ShipRecord> {
    ShipRecord::decode(doc::unhex(row["record"].as_str()?).as_slice()).ok()
}

/// What the graph keeps of a ship beside its entity: the record whole (as hex) and the fields queries filter on.
fn ship_row(record: &ShipRecord) -> Value {
    json!({
        "_key": record.geid.to_string(), "shipUid": record.ship_uid, "ownerId": record.owner_id.to_string(),
        "locationId": record.location_id, "padSize": record.pad_size, "state": record.state,
        "docRevision": record.revision, "record": stored(record), "updatedAt": now_ms(),
    })
}

/// How long a list waits for the durable records before it answers from the graph alone.
const RECORDS_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

const UPSERT_SHIP: &str = "UPSERT { _key: @row._key } INSERT @row UPDATE @row IN ships";

/// The view of a record at `now` for a hangar.
fn view(record: &ShipRecord, hangar: sc_api::internal::HangarSize, exact: bool, now: i64) -> ShipView {
    use sc_api::internal::{ShipClaimKind, ShipState};
    let ship = rules::at(record, now);
    let fits = rules::fits(ship.pad_size(), hangar, exact);
    let destroyed = ship.state() == ShipState::Destroyed;
    ShipView {
        fits,
        retrievable: fits && ship.state() == ShipState::Stored,
        default_claim_quote: destroyed.then(|| rules::quote(&ship, ShipClaimKind::Default, now)),
        insured_claim_quote: destroyed.then(|| rules::quote(&ship, ShipClaimKind::Insured, now)),
        insured_claim_available: destroyed && rules::insured(ship.insurance.as_ref(), now),
        ship: Some(ship),
    }
}

impl Ships {
    async fn class(&self, guid: &str) -> Result<Value, Status> {
        let guid = guid.trim().to_lowercase();
        // A class is keyed by its GUID: a lookup by key. The scan of every class by the `guid` field (no index) is only for
        // a class stored under another key.
        let by_key = self.db.aql("RETURN DOCUMENT('entity_classes', @g)", json!({ "g": guid })).await.map_err(internal)?;
        let rows = match by_key.into_iter().find(|row| !row.is_null()) {
            Some(row) => vec![row],
            None => self.db.aql("FOR c IN entity_classes FILTER c.guid == @g LIMIT 1 RETURN c", json!({ "g": guid })).await.map_err(internal)?,
        };
        rows.into_iter().next().ok_or_else(|| Status::not_found(format!("no entity class {guid}: run sc-admin seed")))
    }

    async fn row(&self, geid: u64) -> Result<Option<Value>, Status> {
        let rows = self.db.aql("FOR s IN ships FILTER s._key == @k RETURN s", json!({ "k": geid.to_string() })).await.map_err(internal)?;
        Ok(rows.into_iter().next())
    }

    /// Makes the graph hold `record`: its entity in the owner's hangar bank, and its `ships` row. Repeating it changes
    /// nothing. The entity and the row are written in one transaction.
    async fn write_graph(&self, record: &ShipRecord) -> Result<(), Status> {
        self.write_graph_with(record, None).await
    }

    /// Only the `ships` row of a record whose entity the graph holds: one transaction and no lookups.
    async fn write_row(&self, record: &ShipRecord) -> Result<(), Status> {
        self.db.transaction(&["ships"], &[(UPSERT_SHIP, json!({ "row": ship_row(record) }))]).await.map_err(crate::write_error)
    }

    /// `write_graph` with the class when the caller has looked it up already.
    async fn write_graph_with(&self, record: &ShipRecord, known: Option<&Value>) -> Result<(), Status> {
        let exists = self.db.aql("FOR e IN entities FILTER e._key == @k RETURN 1", json!({ "k": record.geid.to_string() })).await.map_err(internal)?;
        let mut statements: Vec<(String, Value)> = Vec::new();
        if exists.is_empty() {
            let looked_up;
            let class = match known {
                Some(class) => class,
                None => {
                    looked_up = self.class(&record.entity_class_guid).await?;
                    &looked_up
                }
            };
            let mut plan = Plan::new(None, now_ms());
            let to = bank(record.owner_id, record.location_id);
            plan.refuse_live_inventory(&to, record.owner_id);
            refuse_taken(&mut plan, record.geid);
            let properties = EntityNodeProperties {
                geid: record.geid,
                class_guid_crc: class["guidHashCrc"].as_u64().unwrap_or(0) as u32,
                stack_size: 1,
                owner_id: record.owner_id,
                scale: 1.0,
                item_type_enum: class["itemTypeEnum"].as_i64().unwrap_or(0) as i32,
                item_sub_type_enum: class["itemSubTypeEnum"].as_i64().unwrap_or(0) as i32,
                debug: HashMap::from([("createdBy".to_string(), "ship".to_string()), ("shipUid".to_string(), record.ship_uid.clone())]),
                ..Default::default()
            };
            plan.command(&create_into(properties, &to))?;
            need_target(&mut plan, &to);
            statements = plan.statements(record.owner_id);
        }
        statements.push((UPSERT_SHIP.to_string(), json!({ "row": ship_row(record) })));
        let mut writes: Vec<&str> = WRITES.to_vec();
        writes.push("ships");
        let borrowed: Vec<(&str, Value)> = statements.iter().map(|(query, bind)| (query.as_str(), bind.clone())).collect();
        self.db.transaction(&writes, &borrowed).await.map_err(crate::write_error)
    }

    /// Replaces the record, then the graph, then marks the record as held by the graph. The record is written first, so
    /// a failure after it leaves it ahead of the graph, which `list` makes right.
    async fn commit(&self, record: ShipRecord, expected_revision: u64) -> Result<ShipRecord, Status> {
        let record = self.records.replace(ShipRecord { graph_synced: false, ..record }, expected_revision).await?;
        self.write_graph(&record).await?;
        let synced = self.records.replace(ShipRecord { graph_synced: true, ..record.clone() }, record.revision).await?;
        self.write_row(&synced).await?;
        Ok(synced)
    }

    async fn find(&self, owner: u64, geid: u64) -> Result<ShipRecord, Status> {
        let row = self.row(geid).await?.ok_or_else(|| Status::not_found(format!("no ship {geid}")))?;
        let record = self.records.all(owner).await?.into_iter().find(|record| record.geid == geid);
        match (record, row_record(&row)) {
            (Some(record), _) if record.owner_id == owner => Ok(record),
            (None, Some(_)) => Err(Status::failed_precondition("the ship has no durable record")),
            _ => Err(Status::not_found(format!("no ship {geid}"))),
        }
    }

    pub async fn acquire(&self, request: AcquireShipRequest) -> Result<AcquireShipResponse, Status> {
        need_command(&request.command_id)?;
        need_owner(request.owner_id)?;
        if request.location_id == 0 {
            return Err(invalid("a location is needed"));
        }
        let class = self.class(&request.entity_class_guid).await?;
        let bounding_box = class["vehicle"]["maxBoundingBox"].as_array().map(|list| [0, 1, 2].map(|at| list.get(at).and_then(Value::as_f64).unwrap_or(f64::MAX)));
        let pad = match (request.pad_size(), bounding_box) {
            (sc_api::internal::PadSize::Unspecified, Some(bounding_box)) => rules::pad_for(bounding_box),
            (pad, _) => pad,
        };
        let seeded = |field: &str| class["vehicle"]["insurance"][field].as_f64();
        let mut insurance = request.insurance.clone().unwrap_or_default();
        if insurance.base_wait_minutes == 0.0 {
            insurance.base_wait_minutes = seeded("baseWaitTimeMinutes").unwrap_or(0.0);
            insurance.mandatory_wait_minutes = seeded("mandatoryWaitTimeMinutes").unwrap_or(0.0);
            insurance.expedite_fee_auec = seeded("baseExpeditingFee").unwrap_or(0.0) as u32;
        }
        if pad == sc_api::internal::PadSize::Unspecified {
            warn!(class = %request.entity_class_guid, "the ship's size is not known: it is listed for no hangar");
        }
        let draft = ShipRecord {
            ship_uid: format!("ship:{}", request.command_id),
            owner_id: request.owner_id,
            entity_class_guid: request.entity_class_guid.trim().to_lowercase(),
            entitlement_urn: request.entitlement_urn.clone(),
            name: request.name.clone(),
            state: sc_api::internal::ShipState::Stored as i32,
            insurance: Some(insurance),
            pad_size: pad as i32,
            location_id: request.location_id,
            created_at_ms: now_ms() as i64,
            updated_at_ms: now_ms() as i64,
            command_id: request.command_id.clone(),
            price_auec: request.price_auec,
            ..Default::default()
        };
        // 1. The durable record, the source of truth. If this fails nothing was acquired.
        let mut record = self.records.record(draft).await?;
        let replayed = record.graph_synced;
        if !replayed {
            // 2. The GEID is fixed in the record before the graph is written, so a retry makes the same entity.
            if record.geid == 0 {
                let geid = allocate_geids(&self.db, 1).await?;
                record = self.records.replace(ShipRecord { geid, ..record.clone() }, record.revision).await?;
            }
            // 3. The graph. A failure here leaves a record that is not graph_synced; `list` and `reconcile` finish it.
            match self.write_graph_with(&record, Some(&class)).await {
                Ok(()) => {
                    let synced = self.records.replace(ShipRecord { graph_synced: true, ..record.clone() }, record.revision).await?;
                    self.write_row(&synced).await?;
                    record = synced;
                }
                Err(error) => {
                    warn!(ship = %record.ship_uid, %error, "the ship is recorded; its entity will follow");
                    let view = view(&record, sc_api::internal::HangarSize::Unspecified, false, now_ms() as i64);
                    return Ok(AcquireShipResponse { ship: Some(view), replayed: false, graph_synced: false });
                }
            }
        }
        info!(owner = request.owner_id, ship = %record.ship_uid, geid = record.geid, replayed, "ship acquired");
        Ok(AcquireShipResponse { ship: Some(view(&record, sc_api::internal::HangarSize::Unspecified, false, now_ms() as i64)), replayed, graph_synced: true })
    }

    /// Writes into the graph every record it lacks or holds at another revision; counts the rows the graph has that no
    /// record explains.
    async fn reconcile(&self, owner: u64, records: &[ShipRecord], rows: &[Value]) -> Result<(u32, Vec<u64>), Status> {
        let mut repaired = 0;
        for record in records {
            let mut record = record.clone();
            if record.geid == 0 {
                let geid = allocate_geids(&self.db, 1).await?;
                record = self.records.replace(ShipRecord { geid, ..record }, record.revision).await?;
            }
            let held = rows.iter().find(|row| row["shipUid"] == json!(record.ship_uid));
            if held.is_some_and(|row| row["docRevision"].as_u64() == Some(record.revision)) && record.graph_synced {
                continue;
            }
            self.write_graph(&record).await?;
            if !record.graph_synced {
                let synced = self.records.replace(ShipRecord { graph_synced: true, ..record.clone() }, record.revision).await?;
                self.write_row(&synced).await?;
            }
            repaired += 1;
            info!(owner, ship = %record.ship_uid, "ship made right in the graph from its record");
        }
        let known: Vec<&str> = records.iter().map(|record| record.ship_uid.as_str()).collect();
        let orphans = rows.iter().filter(|row| !known.contains(&row["shipUid"].as_str().unwrap_or_default())).filter_map(|row| row["_key"].as_str()?.parse().ok()).collect();
        Ok((repaired, orphans))
    }

    /// The graph's `ships` rows of an owner, made to agree with the durable records: the record wins, and a row whose
    /// revision differs, or that is missing, is rewritten. Returns the rows as they are after, how many were repaired and
    /// the orphans (graph ships with no record). If the records cannot be asked the graph answers alone.
    pub async fn synced_rows(&self, owner: u64) -> Result<(Vec<Value>, u32, Vec<u64>), Status> {
        let owned = |db: &Arango| {
            let db = db.clone();
            async move { db.aql("FOR s IN ships FILTER s.ownerId == @o RETURN s", json!({ "o": owner.to_string() })).await.map_err(internal) }
        };
        // The graph's rows and the records are asked together; the records get `RECORDS_WAIT` at most.
        let (rows, records) = tokio::join!(owned(&self.db), async {
            match tokio::time::timeout(RECORDS_WAIT, self.records.all(owner)).await {
                Ok(records) => records,
                Err(_) => Err(Status::deadline_exceeded("sc-entity-document did not answer in time")),
            }
        });
        let mut rows = rows?;
        let (mut repaired, mut orphans) = (0, Vec::new());
        // The cross-check with the records. If they cannot be asked, the graph answers alone: it is what the game reads.
        match records {
            Ok(records) => {
                (repaired, orphans) = self.reconcile(owner, &records, &rows).await?;
                if repaired > 0 {
                    rows = owned(&self.db).await?;
                }
            }
            Err(error) => warn!(owner, %error, "the ship records could not be asked; the graph answers alone"),
        }
        Ok((rows, repaired, orphans))
    }

    pub async fn list(&self, request: ListShipsRequest) -> Result<ListShipsResponse, Status> {
        need_owner(request.owner_id)?;
        let now = if request.now_ms == 0 { now_ms() as i64 } else { request.now_ms };
        let (rows, repaired, orphans) = self.synced_rows(request.owner_id).await?;
        let ships = rows
            .iter()
            .filter(|row| !row["_key"].as_str().and_then(|key| key.parse::<u64>().ok()).is_some_and(|geid| orphans.contains(&geid)))
            .filter(|row| request.location_id == 0 || row["locationId"].as_u64() == Some(u64::from(request.location_id)))
            .filter_map(row_record)
            .map(|record| view(&record, request.hangar_size(), request.exact_size, now))
            .filter(|view| view.fits)
            .collect();
        Ok(ListShipsResponse { ships, repaired, orphans: orphans.len() as u32 })
    }

    /// Runs `change` on the record of a ship and commits it, once per command id.
    async fn transition(
        &self,
        owner: u64,
        geid: u64,
        command_id: &str,
        change: impl FnOnce(&ShipRecord) -> Result<ShipRecord, rules::Refused>,
    ) -> Result<(ShipRecord, bool), Status> {
        need_command(command_id)?;
        need_owner(owner)?;
        let record = self.find(owner, geid).await?;
        if record.last_command_id == command_id {
            return Ok((record, true));
        }
        let mut next = change(&record).map_err(Status::failed_precondition)?;
        next.last_command_id = command_id.to_string();
        next.updated_at_ms = now_ms() as i64;
        Ok((self.commit(next, record.revision).await?, false))
    }

    pub async fn claim(&self, request: ClaimShipRequest) -> Result<ClaimShipResponse, Status> {
        let now = if request.now_ms == 0 { now_ms() as i64 } else { request.now_ms };
        let (record, replayed) = self
            .transition(request.owner_id, request.geid, &request.command_id, |record| {
                let mut next = rules::claim(record, request.kind(), now)?;
                if let Some(claim) = next.claim.as_mut() {
                    claim.command_id = request.command_id.clone();
                }
                Ok(next)
            })
            .await?;
        Ok(ClaimShipResponse { ship: Some(view(&record, sc_api::internal::HangarSize::Unspecified, false, now)), replayed })
    }

    pub async fn retrieve(&self, request: RetrieveShipRequest) -> Result<RetrieveShipResponse, Status> {
        let now = if request.now_ms == 0 { now_ms() as i64 } else { request.now_ms };
        let (record, replayed) = self
            .transition(request.owner_id, request.geid, &request.command_id, |record| rules::retrieve(record, request.hangar_size(), request.exact_size, now))
            .await?;
        Ok(RetrieveShipResponse { ship: Some(view(&record, request.hangar_size(), request.exact_size, now)), replayed })
    }

    pub async fn report(&self, request: ReportShipRequest) -> Result<ReportShipResponse, Status> {
        let now = if request.now_ms == 0 { now_ms() as i64 } else { request.now_ms };
        let (record, replayed) = self
            .transition(request.owner_id, request.geid, &request.command_id, |record| rules::report(record, request.state(), request.location_id))
            .await?;
        Ok(ReportShipResponse { ship: Some(view(&record, sc_api::internal::HangarSize::Unspecified, false, now)), replayed })
    }
}

#[tonic::async_trait]
impl ShipsInternal for Ships {
    async fn acquire_ship(&self, request: Request<AcquireShipRequest>) -> Result<Response<AcquireShipResponse>, Status> {
        Ok(Response::new(self.acquire(request.into_inner()).await?))
    }
    async fn list_ships(&self, request: Request<ListShipsRequest>) -> Result<Response<ListShipsResponse>, Status> {
        Ok(Response::new(self.list(request.into_inner()).await?))
    }
    async fn claim_ship(&self, request: Request<ClaimShipRequest>) -> Result<Response<ClaimShipResponse>, Status> {
        Ok(Response::new(self.claim(request.into_inner()).await?))
    }
    async fn retrieve_ship(&self, request: Request<RetrieveShipRequest>) -> Result<Response<RetrieveShipResponse>, Status> {
        Ok(Response::new(self.retrieve(request.into_inner()).await?))
    }
    async fn report_ship(&self, request: Request<ReportShipRequest>) -> Result<Response<ReportShipResponse>, Status> {
        Ok(Response::new(self.report(request.into_inner()).await?))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sc_api::internal::{HangarSize, PadSize, ShipClaimKind, ShipInsurance, ShipPolicy, ShipState};

    use super::*;

    /// The records of sc-entity-document, in memory, with its revision rules.
    #[derive(Default)]
    struct Memory {
        by_uid: Mutex<HashMap<String, ShipRecord>>,
        /// The calls made, in order: the cost of an operation in round trips to sc-entity-document.
        calls: Mutex<Vec<&'static str>>,
    }

    #[tonic::async_trait]
    impl Records for Memory {
        async fn record(&self, mut record: ShipRecord) -> Result<ShipRecord, Status> {
            self.calls.lock().unwrap().push("record");
            let mut all = self.by_uid.lock().unwrap();
            if let Some(found) = all.values().find(|found| found.command_id == record.command_id || found.ship_uid == record.ship_uid) {
                return Ok(found.clone());
            }
            (record.revision, record.graph_synced, record.geid) = (1, false, 0);
            all.insert(record.ship_uid.clone(), record.clone());
            Ok(record)
        }
        async fn all(&self, owner: u64) -> Result<Vec<ShipRecord>, Status> {
            self.calls.lock().unwrap().push("all");
            Ok(self.by_uid.lock().unwrap().values().filter(|record| record.owner_id == owner).cloned().collect())
        }
        async fn replace(&self, mut record: ShipRecord, expected: u64) -> Result<ShipRecord, Status> {
            self.calls.lock().unwrap().push("replace");
            let mut all = self.by_uid.lock().unwrap();
            let held = all.get(&record.ship_uid).ok_or_else(|| Status::not_found("no ship"))?;
            if held.revision != expected {
                return Err(Status::aborted("changed meanwhile"));
            }
            record.revision = expected + 1;
            all.insert(record.ship_uid.clone(), record.clone());
            Ok(record)
        }
    }

    fn fresh() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64 % 1_000_000_000 * 100 + NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// Against a real ArangoDB: `SC_TEST_ARANGO=<url> cargo test -p sc-entitygraph -- --ignored ships`.
    #[tokio::test]
    #[ignore]
    async fn ships_are_acquired_listed_by_hangar_size_claimed_and_retrieved() {
        let url = std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO");
        let db = crate::open(&url).await.unwrap();
        let records = Arc::new(Memory::default());
        let ships = Ships { db: db.clone(), records: records.clone() };
        let base = fresh();
        let (owner, location) = (500_000_000_000 + base, 4242u32);
        let (small_guid, large_guid) = (format!("shipsmall{base}"), format!("shiplarge{base}"));
        let class = |guid: &str, crc: u64, bounding_box: [f64; 3]| {
            json!({ "_key": guid, "guid": guid, "guidHashCrc": (base + crc) % 0xffff_fff0 + 1, "className": guid,
                    "vehicle": { "maxBoundingBox": bounding_box, "insurance": { "baseWaitTimeMinutes": 4.05, "mandatoryWaitTimeMinutes": 1.35, "baseExpeditingFee": 2343 } } })
        };
        let classes = json!([class(&small_guid, 1, [15.0, 20.0, 6.5]), class(&large_guid, 2, [70.0, 120.0, 30.0])]);
        db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        let acquire = |command: &str, guid: &str, policy: ShipPolicy| AcquireShipRequest {
            command_id: format!("{command}-{base}"),
            owner_id: owner,
            entity_class_guid: guid.to_uppercase(),
            location_id: location,
            insurance: Some(ShipInsurance { policy: policy as i32, ..Default::default() }),
            price_auec: 1_000_000,
            ..Default::default()
        };

        // Acquisition writes the record, then the entity and its row, once per command.
        let small = ships.acquire(acquire("a", &small_guid, ShipPolicy::Lifetime)).await.unwrap();
        assert_eq!(std::mem::take(&mut *records.calls.lock().unwrap()), ["record", "replace", "replace"], "an acquisition is three round trips to the records, no more");
        let large = ships.acquire(acquire("b", &large_guid, ShipPolicy::Unspecified)).await.unwrap();
        let (small_ship, large_ship) = (small.ship.unwrap().ship.unwrap(), large.ship.unwrap().ship.unwrap());
        assert!(small.graph_synced && !small.replayed);
        assert!(small_ship.geid >= crate::admin::FIRST_ITEM_GEID && small_ship.geid != large_ship.geid);
        assert_eq!((small_ship.pad_size(), large_ship.pad_size()), (PadSize::Xsmall, PadSize::Large));
        assert_eq!(small_ship.insurance.unwrap().base_wait_minutes, 4.05, "the wait comes from the class");
        records.calls.lock().unwrap().clear();
        let again = ships.acquire(acquire("a", &small_guid, ShipPolicy::Lifetime)).await.unwrap();
        assert_eq!(std::mem::take(&mut *records.calls.lock().unwrap()), ["record"], "a repeated command is one");
        assert_eq!((again.replayed, again.ship.unwrap().ship.unwrap().geid), (true, small_ship.geid));
        let held = db.aql("FOR e IN entities FILTER e.ownerId == @o RETURN e._key", json!({ "o": owner.to_string() })).await.unwrap();
        assert_eq!(held.len(), 2, "one entity per ship");
        let in_bank = db.aql("FOR i IN inventories FILTER i.id == @id RETURN 1", json!({ "id": bank(owner, location) })).await.unwrap();
        assert_eq!(in_bank.len(), 1, "the hangar bank exists");
        assert_eq!(records.all(owner).await.unwrap().iter().filter(|record| record.graph_synced).count(), 2);

        // The kiosk of a small hangar lists the ship that fits; a large one both; an exact large one only the large.
        let list = |size: HangarSize, exact: bool, at: u32| ListShipsRequest { owner_id: owner, location_id: at, hangar_size: size as i32, exact_size: exact, now_ms: 0 };
        let geids = |reply: ListShipsResponse| reply.ships.iter().map(|view| view.ship.as_ref().unwrap().geid).collect::<Vec<_>>();
        records.calls.lock().unwrap().clear();
        assert_eq!(geids(ships.list(list(HangarSize::Small, false, location)).await.unwrap()), [small_ship.geid]);
        assert_eq!(std::mem::take(&mut *records.calls.lock().unwrap()), ["all"], "a list is one call to the records and repairs nothing");
        assert_eq!(geids(ships.list(list(HangarSize::Large, true, location)).await.unwrap()), [large_ship.geid]);
        let mut both = geids(ships.list(list(HangarSize::Xlarge, false, 0)).await.unwrap());
        both.sort();
        assert_eq!(both.len(), 2);
        assert!(geids(ships.list(list(HangarSize::Xlarge, false, location + 1)).await.unwrap()).is_empty(), "another location's bank");

        // Retrieve: only a stored ship that fits the hangar; once per command.
        let retrieve = |command: &str, geid: u64, size: HangarSize| RetrieveShipRequest { command_id: format!("{command}-{base}"), owner_id: owner, geid, hangar_size: size as i32, ..Default::default() };
        let refused = ships.retrieve(retrieve("r0", large_ship.geid, HangarSize::Small)).await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        let out = ships.retrieve(retrieve("r1", small_ship.geid, HangarSize::Small)).await.unwrap();
        assert_eq!((out.ship.unwrap().ship.unwrap().state(), out.replayed), (ShipState::Deployed, false));
        assert!(ships.retrieve(retrieve("r1", small_ship.geid, HangarSize::Small)).await.unwrap().replayed);
        assert_eq!(ships.retrieve(retrieve("r2", small_ship.geid, HangarSize::Small)).await.unwrap_err().code(), tonic::Code::FailedPrecondition, "already out");
        assert!(!records.all(owner).await.unwrap().iter().any(|record| record.state() == ShipState::Stored && record.geid == small_ship.geid));

        // Destroyed, then claimed. The insured claim needs a policy (the large ship has none) and takes the class's wait.
        let report = |command: &str, geid: u64, state: ShipState| ReportShipRequest { command_id: format!("{command}-{base}"), owner_id: owner, geid, state: state as i32, location_id: location, now_ms: 0 };
        ships.report(report("d1", small_ship.geid, ShipState::Destroyed)).await.unwrap();
        let claim = |command: &str, geid: u64, kind: ShipClaimKind, now: i64| ClaimShipRequest { command_id: format!("{command}-{base}"), owner_id: owner, geid, kind: kind as i32, now_ms: now };
        let destroyed = ships.list(list(HangarSize::Small, false, location)).await.unwrap().ships.remove(0);
        assert!(destroyed.insured_claim_available && destroyed.default_claim_quote.is_some() && !destroyed.retrievable);
        let started = 10_000_000_000i64;
        let claimed = ships.claim(claim("c1", small_ship.geid, ShipClaimKind::Insured, started)).await.unwrap().ship.unwrap();
        let timer = claimed.ship.unwrap().claim.unwrap();
        assert_eq!((timer.ready_at_ms - started, timer.expedite_after_ms - started, timer.cost_auec), (243_000, 81_000, 3_000));
        let early = RetrieveShipRequest { now_ms: started + 1, ..retrieve("r3", small_ship.geid, HangarSize::Small) };
        assert_eq!(ships.retrieve(early).await.unwrap_err().code(), tonic::Code::FailedPrecondition, "the claim has not finished");
        let at_ready = ListShipsRequest { now_ms: started + 243_000, ..list(HangarSize::Small, false, location) };
        assert!(ships.list(at_ready).await.unwrap().ships[0].retrievable, "stored once the time has come");
        assert_eq!(ships.claim(claim("c2", large_ship.geid, ShipClaimKind::Default, started)).await.unwrap_err().code(), tonic::Code::FailedPrecondition, "not destroyed");

        // The graph disagrees with the record: the record wins and the list says it repaired.
        let stale = ShipRecord { state: ShipState::Expired as i32, ..records.all(owner).await.unwrap().into_iter().find(|r| r.geid == large_ship.geid).unwrap() };
        let held = records.replace(stale.clone(), stale.revision).await.unwrap();
        let fixed = ships.list(list(HangarSize::Large, false, location)).await.unwrap();
        assert_eq!(fixed.repaired, 1);
        let large_now = fixed.ships.iter().find(|view| view.ship.as_ref().unwrap().geid == large_ship.geid).unwrap();
        assert_eq!(large_now.ship.as_ref().unwrap().state(), ShipState::Expired);
        assert!(!large_now.retrievable);
        let row = db.aql("FOR s IN ships FILTER s._key == @k RETURN s.docRevision", json!({ "k": large_ship.geid.to_string() })).await.unwrap();
        assert_eq!(row[0].as_u64(), Some(held.revision), "the graph holds the record's revision now");
        assert_eq!(ships.list(list(HangarSize::Large, false, location)).await.unwrap().repaired, 0, "and the next list repairs nothing");

        // A record whose graph write never happened (a crash after the first write) is finished by the next list.
        let lost = records.record(ShipRecord { ship_uid: format!("lost-{base}"), command_id: format!("lost-{base}"), owner_id: owner, entity_class_guid: large_guid.clone(), state: ShipState::Stored as i32, pad_size: PadSize::Large as i32, location_id: location, ..Default::default() }).await.unwrap();
        assert_eq!(lost.geid, 0);
        let listed = ships.list(list(HangarSize::Large, true, location)).await.unwrap();
        assert!(listed.repaired >= 1 && listed.ships.iter().any(|view| view.ship.as_ref().unwrap().ship_uid == lost.ship_uid));
        let finished = records.all(owner).await.unwrap().into_iter().find(|r| r.ship_uid == lost.ship_uid).unwrap();
        assert!(finished.graph_synced && finished.geid >= crate::admin::FIRST_ITEM_GEID);

        // A ship in the graph with no record is not listed.
        let orphan = ShipRecord { ship_uid: format!("orphan-{base}"), owner_id: owner, geid: 777_000_000_000 + base, entity_class_guid: large_guid.clone(), state: ShipState::Stored as i32, pad_size: PadSize::Large as i32, location_id: location, revision: 1, ..Default::default() };
        db.aql("INSERT @row INTO ships", json!({ "row": ship_row(&orphan) })).await.unwrap();
        let listed = ships.list(list(HangarSize::Large, false, location)).await.unwrap();
        assert_eq!(listed.orphans, 1);
        assert!(!listed.ships.iter().any(|view| view.ship.as_ref().unwrap().ship_uid == orphan.ship_uid));

        db.aql("FOR s IN ships FILTER s.ownerId == @o REMOVE s IN ships", json!({ "o": owner.to_string() })).await.unwrap();
        db.aql("FOR e IN entities FILTER e.ownerId == @o REMOVE e IN entities", json!({ "o": owner.to_string() })).await.unwrap();
        db.aql("FOR c IN entity_classes FILTER c._key IN @k REMOVE c IN entity_classes", json!({ "k": [small_guid, large_guid] })).await.unwrap();
    }
}

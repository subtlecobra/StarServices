//! sc.external.services.instance_manager.v1.InstanceManagerService. Reports, per requested identifier,
//! whether a player can use an instance on the shard. An identifier with no instance yet is eligible,
//! because entering it is what provisions one.

use std::{collections::HashMap, net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost_types::Timestamp;
use sc_db::{internal, Arango, DatabaseArgs};
use sc_proto::services::instance_manager::v1::{
    instance_manager_service_server::{InstanceManagerService, InstanceManagerServiceServer},
    GetAvailableInstancesRequest, GetAvailableInstancesResponse, Instance, InstanceActivityReport,
    InstanceAvailabilityInfo, InstancePrerequisites, InstanceStatus, InstanceTemplate, RoleSlotStats,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use serde_json::{json, Value};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const COLLECTIONS: &[&str] = &["instances"];
const QUERY: &str = "
FOR i IN instances
    FILTER i.instanceIdentifier IN @identifiers AND (@shard == '' OR i.shardId == @shard)
    FILTER i.completedAtMs == null
    SORT i.createdAtMs
    RETURN i";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Instances {
    db: Arango,
}

fn text(doc: &Value, field: &str) -> String {
    doc[field].as_str().unwrap_or_default().to_string()
}

fn optional_text(doc: &Value, field: &str) -> Option<String> {
    doc[field].as_str().map(str::to_string)
}

fn timestamp(doc: &Value, field: &str) -> Option<Timestamp> {
    let millis = doc[field].as_i64()?;
    Some(Timestamp { seconds: millis.div_euclid(1000), nanos: millis.rem_euclid(1000) as i32 * 1_000_000 })
}

fn players(doc: &Value) -> Vec<u64> {
    let list = doc["players"].as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter().filter_map(|geid| geid.as_str()?.parse().ok()).collect()
}

fn role_stats(doc: &Value) -> Vec<RoleSlotStats> {
    let list = doc["roles"].as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .map(|role| RoleSlotStats { name: text(role, "name"), current_count: role["count"].as_u64().unwrap_or(0) as u32 })
        .collect()
}

fn instance(doc: &Value) -> Instance {
    let geids = doc["entityGeids"].as_object();
    Instance {
        instance_id: text(doc, "_key"),
        instance_identifier: text(doc, "instanceIdentifier"),
        shard_id: text(doc, "shardId"),
        instance_entity_geids: geids
            .into_iter()
            .flatten()
            .filter_map(|(name, geid)| Some((name.clone(), geid.as_str()?.parse().ok()?)))
            .collect(),
        created_at: timestamp(doc, "createdAtMs"),
        template: Some(InstanceTemplate {
            instance_oc_path: text(doc, "ocPath"),
            r#type: doc["type"].as_i64().unwrap_or(0) as i32,
            ..Default::default()
        }),
        status: doc["status"].as_i64().unwrap_or(0) as i32,
        requester_urn: optional_text(doc, "requesterUrn"),
        assigned_to_urn: optional_text(doc, "assignedToUrn"),
        stow_inventory_id: optional_text(doc, "stowInventoryId"),
        last_activity_at: timestamp(doc, "lastActivityAtMs"),
        completed_at: timestamp(doc, "completedAtMs"),
    }
}

fn joinable(status: i32) -> bool {
    [InstanceStatus::Unassigned, InstanceStatus::Idle, InstanceStatus::Active].iter().any(|s| *s as i32 == status)
}

fn has_room(doc: &Value, prerequisites: &InstancePrerequisites) -> bool {
    if prerequisites.max_capacity != 0 && players(doc).len() as u32 >= prerequisites.max_capacity {
        return false;
    }
    if prerequisites.slot_rules.is_empty() {
        return true;
    }
    let stats = role_stats(doc);
    prerequisites.slot_rules.iter().any(|slot| {
        let taken = stats.iter().find(|role| role.name == slot.name).map_or(0, |role| role.current_count);
        taken < slot.max_count
    })
}

fn availability(
    identifier: &str,
    prerequisites: &InstancePrerequisites,
    candidates: &[&Value],
    report: bool,
) -> InstanceAvailabilityInfo {
    let open = |doc: &&&Value| joinable(doc["status"].as_i64().unwrap_or(0) as i32) && has_room(doc, prerequisites);
    let chosen = candidates.iter().find(open).or(candidates.first());
    InstanceAvailabilityInfo {
        instance_identifier: identifier.to_string(),
        instance: chosen.map(|doc| instance(doc)),
        activity_report: chosen.filter(|_| report).map(|doc| InstanceActivityReport {
            player_geids: players(doc),
            role_slot_stats: role_stats(doc),
        }),
        eligible: candidates.is_empty() || candidates.iter().any(|doc| open(&doc)),
    }
}

#[tonic::async_trait]
impl InstanceManagerService for Instances {
    async fn get_available_instances(
        &self,
        request: Request<GetAvailableInstancesRequest>,
    ) -> Result<Response<GetAvailableInstancesResponse>, Status> {
        let request = request.into_inner();
        let identifiers: Vec<&String> = request.instance_prerequisites.keys().collect();
        let bind = json!({ "identifiers": identifiers, "shard": request.shard_id });
        let rows = self.db.aql(QUERY, bind).await.map_err(internal)?;
        let mut by_identifier: HashMap<&str, Vec<&Value>> = HashMap::new();
        for row in &rows {
            by_identifier.entry(row["instanceIdentifier"].as_str().unwrap_or_default()).or_default().push(row);
        }
        let instance_results = request
            .instance_prerequisites
            .iter()
            .map(|(identifier, prerequisites)| {
                let candidates = by_identifier.get(identifier.as_str()).map(Vec::as_slice).unwrap_or_default();
                availability(identifier, prerequisites, candidates, request.request_activity_report)
            })
            .collect();
        Ok(Response::new(GetAvailableInstancesResponse { instance_results }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Instances { db: Arango::connect(&args.database.arango, COLLECTIONS).await? };
    let server = InstanceManagerServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "instance manager listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::instance_manager::v1::RoleSlot;

    fn doc(status: InstanceStatus, players: &[&str]) -> Value {
        json!({ "_key": "i1", "instanceIdentifier": "hangar", "shardId": "s", "status": status as i32, "players": players })
    }

    #[test]
    fn an_unknown_identifier_is_eligible_with_no_instance() {
        let info = availability("hangar", &InstancePrerequisites::default(), &[], true);
        assert!(info.eligible && info.instance.is_none() && info.activity_report.is_none());
    }

    #[test]
    fn a_full_instance_is_not_eligible_but_still_reported() {
        let full = doc(InstanceStatus::Active, &["1", "2"]);
        let prerequisites = InstancePrerequisites { max_capacity: 2, ..Default::default() };
        let info = availability("hangar", &prerequisites, &[&full], true);
        assert!(!info.eligible);
        assert_eq!(info.instance.unwrap().instance_id, "i1");
        assert_eq!(info.activity_report.unwrap().player_geids, vec![1, 2]);
    }

    #[test]
    fn stowing_instances_and_exhausted_roles_are_not_open() {
        let stowing = doc(InstanceStatus::Stowing, &[]);
        assert!(!availability("hangar", &InstancePrerequisites::default(), &[&stowing], false).eligible);
        let mut staffed = doc(InstanceStatus::Idle, &["1"]);
        staffed["roles"] = json!([{ "name": "pilot", "count": 1 }]);
        let slot_rules = vec![RoleSlot { name: "pilot".into(), max_count: 1 }];
        let prerequisites = InstancePrerequisites { slot_rules, ..Default::default() };
        assert!(!availability("hangar", &prerequisites, &[&staffed], false).eligible);
    }

    /// Against a real ArangoDB: `SC_TEST_ARANGO=<url> cargo test -p sc-instance-manager -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn availability_reads_the_stored_instances() {
        let url = std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO");
        let db = Arango::connect(&url, COLLECTIONS).await.unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros();
        let (known, unknown, key) = (format!("known-{stamp}"), format!("unknown-{stamp}"), stamp.to_string());
        let doc = json!({
            "_key": key,
            "instanceIdentifier": known,
            "shardId": "shard-a",
            "status": InstanceStatus::Active as i32,
            "players": ["7"],
            "createdAtMs": 1500,
            "entityGeids": { "root": "18446744073709551615" },
        });
        db.aql("INSERT @doc INTO instances", json!({ "doc": doc })).await.unwrap();

        let service = Instances { db: db.clone() };
        let ask = |shard: &str| {
            let prerequisites = InstancePrerequisites { max_capacity: 2, ..Default::default() };
            Request::new(GetAvailableInstancesRequest {
                instance_prerequisites: HashMap::from([(known.clone(), prerequisites), (unknown.clone(), Default::default())]),
                shard_id: shard.to_string(),
                request_activity_report: true,
            })
        };
        let reply = service.get_available_instances(ask("shard-a")).await.unwrap().into_inner();
        assert_eq!(reply.instance_results.len(), 2);
        let found = reply.instance_results.iter().find(|info| info.instance_identifier == known).unwrap();
        assert!(found.eligible);
        let instance = found.instance.as_ref().unwrap();
        assert_eq!(instance.instance_id, key);
        assert_eq!(instance.instance_entity_geids["root"], u64::MAX);
        assert_eq!(instance.created_at.as_ref().map(|time| (time.seconds, time.nanos)), Some((1, 500_000_000)));
        assert_eq!(found.activity_report.as_ref().unwrap().player_geids, [7]);
        let missing = reply.instance_results.iter().find(|info| info.instance_identifier == unknown).unwrap();
        assert!(missing.eligible && missing.instance.is_none());

        let elsewhere = service.get_available_instances(ask("shard-b")).await.unwrap().into_inner();
        assert!(elsewhere.instance_results.iter().all(|info| info.instance.is_none()));

        db.aql("REMOVE @key IN instances", json!({ "key": key })).await.unwrap();
    }
}

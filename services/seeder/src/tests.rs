use std::time::Duration;

use serde_json::{json, Value};

use super::*;

fn region(name: &str, status: Value) -> DynamicObject {
    let mut object: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "scbackend.io/v1alpha1", "kind": "ShardRegion", "metadata": { "name": name },
    }))
    .unwrap();
    object.data = json!({ "status": status });
    object
}

#[test]
fn a_region_is_seeded_once_its_entity_database_is_ready() {
    let ready = region("usa", json!({ "entityDb": { "state": "Ready", "service": "http://graph:50051" } }));
    assert_eq!(region_target(&ready).unwrap(), "http://graph:50051");
    let unnamed = region("eu", json!({ "entityDb": { "state": "Ready" } }));
    assert_eq!(region_target(&unnamed).unwrap(), "http://sc-entitygraph.scbackend-region-eu.svc:50051");
    let pending = region("asia", json!({ "entityDb": { "state": "Pending", "message": "arangodb not ready" } }));
    assert!(region_target(&pending).unwrap_err().contains("arangodb not ready"));
    assert!(region_target(&region("new", Value::Null)).is_err());
}

#[test]
fn named_regions_must_exist() {
    let listed = || vec![region("usa", Value::Null), region("eu", Value::Null)];
    assert_eq!(pick(listed(), &[]).unwrap().len(), 2);
    let picked = pick(listed(), &["eu".into()]).unwrap();
    assert_eq!(picked[0].metadata.name.as_deref(), Some("eu"));
    assert!(pick(listed(), &["mars".into()]).unwrap_err().contains("mars"));
    assert_eq!(named("usa=http://a:1"), ("usa".into(), "http://a:1".into()));
    assert_eq!(named("http://a:1"), ("http://a:1".into(), "http://a:1".into()));
}

fn args(owners: &str) -> Args {
    let mut args = Args::parse_from(["sc-seeder"; 1].into_iter().chain(["--bundle", "x"]));
    args.bundle = None;
    args.entitlement = owners.into();
    args.reputation = owners.into();
    args.blueprint_library = owners.into();
    args.mission_scenario = owners.into();
    args.contract_broker = owners.into();
    args.shop = owners.into();
    args.wait_secs = 0;
    args
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_directory_is_applied_to_every_region() {
    let out = std::env::temp_dir().join(format!("sc-seeder-{}", std::process::id()));
    let content = sc_seed::Content {
        classes: vec![sc_proto_rows::class("g1")],
        containers: vec![sc_proto_rows::node(1, None), sc_proto_rows::node(2, Some(1))],
        ..Default::default()
    };
    sc_seed::bundle::write(&out, "5", &content).unwrap();
    let (owned, address) = sc_seed::fakes::Owned::serve().await;
    let mut args = args(&address);
    args.bundle = Some(out.join("5"));
    args.entitygraph = vec![format!("usa={address}"), format!("eu={address}")];
    run(args).await.unwrap();
    let _ = std::fs::remove_dir_all(&out);
    assert_eq!(owned.containers.lock().unwrap().len(), 2);
    assert_eq!(owned.classes.lock().unwrap()[1], content.classes);
}

/// Pulls a pushed bundle and applies it to a real sc-entitygraph and fake global owners:
/// `SC_TEST_SEED_REGISTRY=localhost:5001 SC_TEST_SEED_BUILD=12913470 SC_TEST_ENTITYGRAPH=http://127.0.0.1:15099
/// cargo test -p sc-seeder -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_pull_and_apply() {
    let (owned, address) = sc_seed::fakes::Owned::serve().await;
    let mut args = args(&address);
    args.registry = Some(std::env::var("SC_TEST_SEED_REGISTRY").unwrap());
    args.build = Some(std::env::var("SC_TEST_SEED_BUILD").unwrap());
    args.plain_http = true;
    args.work_dir = std::env::current_dir().unwrap().join("../../target/seed-work");
    args.entitygraph = vec![format!("test={}", std::env::var("SC_TEST_ENTITYGRAPH").unwrap())];
    args.wait_secs = Duration::from_secs(5).as_secs();
    run(args).await.unwrap();
    println!(
        "owners got: {} starter entitlements, {} reputation calls, {} blueprints, {} scenarios, {} contracts, {} shop items, {} rental prices",
        owned.starters.lock().unwrap().len(),
        owned.reputation.lock().unwrap().len(),
        owned.blueprints.lock().unwrap().len(),
        owned.scenarios.lock().unwrap().len(),
        owned.contracts.lock().unwrap().iter().map(Vec::len).sum::<usize>(),
        owned.shop_items.lock().unwrap().iter().map(Vec::len).sum::<usize>(),
        owned.prices.lock().unwrap().len(),
    );
}

mod sc_proto_rows {
    pub use sc_proto::internal::{ContainerSeed, EntityClassSeed};

    pub fn class(guid: &str) -> EntityClassSeed {
        EntityClassSeed { guid: guid.into(), ..Default::default() }
    }

    pub fn node(geid: u64, parent: Option<u64>) -> ContainerSeed {
        ContainerSeed { geid, parent, ..Default::default() }
    }
}

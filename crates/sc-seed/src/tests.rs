//! A bundle applied to fake owners: each gets its own rows, in calls that fit a message, and every region the classes
//! and the whole tree.

use std::time::Duration;

use sc_api::internal::{ BlueprintDefault, ContainerSeed, EntityClassSeed, MissionContractSeed, RentalPrice, ReputationLadderStanding, ReputationScope, ReputationStanding, ScenarioSeed, ShopItem, StarterEntitlement, };
use sc_proto::{
    services::solar_system_shop::v1::RentalOffering,
};

use crate::{apply, apply::hand_over, bundle, fakes::Owned, Bundle, Content};

fn content() -> Content {
    Content {
        classes: (0..3).map(|n| EntityClassSeed { guid: format!("g{n}"), ..Default::default() }).collect(),
        containers: vec![
            ContainerSeed { geid: 1, labels: vec![3], ..Default::default() },
            ContainerSeed { geid: 2, parent: Some(1), ..Default::default() },
        ],
        rental_prices: vec![RentalPrice { id: "ship".into(), offerings: vec![RentalOffering { days: 3, price: 150.0 }] }],
        // Big enough to need two calls.
        shop_items: (0..80_000)
            .map(|n| ShopItem {
                id: format!("00000000-0000-0000-0000-{n:012}"),
                buy_price: n as f64,
                sell_price: 1.5,
                current_inventory: 10,
                max_inventory: 20,
            })
            .collect(),
        starters: vec![StarterEntitlement { class_guid: "g".into(), source: "s".into(), ..Default::default() }],
        scopes: vec![ReputationScope { scope: "scope".into(), ..Default::default() }],
        standings: vec![ReputationStanding { id: "standing".into(), ..Default::default() }],
        ladder: vec![ReputationLadderStanding { standings_id: "l".into(), standing_id: "standing".into() }],
        blueprints: vec![BlueprintDefault { blueprint_id: "b".into(), ..Default::default() }],
        scenarios: vec![ScenarioSeed { template_id: "t".into(), status: 1 }],
        // Big enough to need two calls.
        contracts: (0..6).map(|n| MissionContractSeed { id: format!("c{n}"), body: vec![7; 1 << 20] }).collect(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_owner_receives_its_own_rows_in_calls_that_fit() {
    let (owned, address) = Owned::serve().await;
    let regions = [("usa".to_string(), address.clone()), ("eu".to_string(), address.clone())];
    apply(content(), &regions, &Owned::owners(&address), Duration::ZERO).await.unwrap();

    let expected = content();
    assert_eq!(*owned.classes.lock().unwrap(), [expected.classes.clone(), expected.classes.clone()], "every region");
    assert_eq!(*owned.containers.lock().unwrap(), [expected.containers.clone(), expected.containers.clone()]);
    let shop_calls = owned.shop_items.lock().unwrap().clone();
    assert_eq!(shop_calls.len(), 2);
    assert_eq!(shop_calls.concat(), expected.shop_items);
    assert_eq!(owned.prices.lock().unwrap()[0].offerings, [RentalOffering { days: 3, price: 150.0 }]);
    assert_eq!(owned.starters.lock().unwrap()[0].class_guid, "g");
    let reputation = owned.reputation.lock().unwrap().clone();
    assert_eq!(reputation.len(), 3, "scopes, standings and ladder, each in its own call");
    assert_eq!(reputation[0].scopes[0].scope, "scope");
    assert_eq!(reputation[1].standings[0].id, "standing");
    assert_eq!(reputation[2].ladder[0].standing_id, "standing");
    assert_eq!(owned.blueprints.lock().unwrap()[0].blueprint_id, "b");
    assert_eq!(owned.scenarios.lock().unwrap()[0].template_id, "t");
    let calls = owned.contracts.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls.concat(), expected.contracts);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_written_bundle_applies_as_its_content_would() {
    let out = std::env::temp_dir().join(format!("sc-seed-apply-{}", std::process::id()));
    bundle::write(&out, "1", &content()).unwrap();
    let read = Bundle::open(&out.join("1"), Some("1")).unwrap().content().unwrap();
    let _ = std::fs::remove_dir_all(&out);
    assert_eq!(read, content());
    let (owned, address) = Owned::serve().await;
    apply(read, &[("usa".into(), address.clone())], &Owned::owners(&address), Duration::ZERO).await.unwrap();
    assert_eq!(owned.contracts.lock().unwrap().concat(), content().contracts);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_owner_is_named() {
    let mut owners = Owned::owners("");
    owners.shop = "http://127.0.0.1:1".into();
    let error = hand_over(&owners, Content::default(), Duration::ZERO).await.unwrap_err();
    assert!(error.starts_with("shop at http://127.0.0.1:1"), "{error}");
}

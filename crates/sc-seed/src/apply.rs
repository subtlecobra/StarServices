//! Hands a bundle's rows to their owners, through each one's seed RPC in `proto/scbackend/internal.proto`: the entity
//! classes and the container tree to the `sc-entitygraph` of every region, the other tables once to the global owner.
//! Nothing here touches a database. Each owner replaces rows by key, so a repeat changes nothing and a new build
//! updates what it finds; only the container tree loses rows, as its owner replaces it whole. Scenario statuses are
//! only added, as an operator may have started one.

use std::time::{Duration, Instant};

use prost::Message;
use sc_proto::internal::{
    blueprint_library_internal_client::BlueprintLibraryInternalClient,
    contract_broker_internal_client::ContractBrokerInternalClient,
    entitlement_internal_client::EntitlementInternalClient, entity_graph_internal_client::EntityGraphInternalClient,
    mission_scenario_internal_client::MissionScenarioInternalClient,
    reputation_internal_client::ReputationInternalClient,
    solar_system_shop_internal_client::SolarSystemShopInternalClient, ContainerSeed, EntityClassSeed,
    SeedBlueprintDefaultsRequest, SeedContainersRequest, SeedEntityClassesRequest, SeedMissionContractsRequest,
    SeedRentalPricesRequest, SeedReputationRequest, SeedScenariosRequest, SeedShopItemsRequest,
    SeedStarterEntitlementsRequest,
};
use tonic::transport::{Channel, Endpoint};

use crate::bundle::Content;

/// Well under the 16 MiB a service accepts in one message.
pub const BYTES_PER_CALL: usize = 4 << 20;
const RETRY: Duration = Duration::from_secs(5);

/// Where the owners of the global tables listen.
#[derive(Clone, Debug)]
pub struct Owners {
    pub entitlement: String,
    pub reputation: String,
    pub blueprint_library: String,
    pub mission_scenario: String,
    pub contract_broker: String,
    pub shop: String,
}

/// Connects to `address`, trying again every few seconds until `wait` has passed. With no wait, one try.
pub async fn channel(name: &str, address: &str, wait: Duration) -> Result<Channel, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("{name} address: {e}"))?;
    let deadline = Instant::now() + wait;
    loop {
        match endpoint.connect().await {
            Ok(channel) => return Ok(channel),
            Err(error) if Instant::now() + RETRY < deadline => {
                println!("{name} at {address} not reached ({error}), trying again");
                tokio::time::sleep(RETRY).await;
            }
            Err(error) => return Err(format!("{name} at {address}: {error}")),
        }
    }
}

/// Splits `rows` into runs whose encoding fits in one call, keeping their order.
pub fn batches<T: Message>(rows: Vec<T>, max_bytes: usize) -> Vec<Vec<T>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut size = 0;
    for row in rows {
        let length = row.encoded_len() + 8;
        if !batch.is_empty() && size + length > max_bytes {
            batches.push(std::mem::take(&mut batch));
            size = 0;
        }
        size += length;
        batch.push(row);
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn failed(owner: &'static str) -> impl Fn(tonic::Status) -> String {
    move |status| format!("{owner}: {}", status.message())
}

/// Seeds one region's `sc-entitygraph`: the classes, then the whole container tree in one stream.
pub async fn region(address: &str, classes: &[EntityClassSeed], containers: &[ContainerSeed], wait: Duration) -> Result<(), String> {
    let mut graph = EntityGraphInternalClient::new(channel("entitygraph", address, wait).await?);
    let requests: Vec<_> =
        batches(classes.to_vec(), BYTES_PER_CALL).into_iter().map(|classes| SeedEntityClassesRequest { classes }).collect();
    let written = graph.seed_entity_classes(tokio_stream::iter(requests)).await.map_err(failed("entitygraph"))?.into_inner().rows;
    println!("entity classes: {written}");
    tree(&mut graph, containers).await
}

/// Only the container tree of one region's `sc-entitygraph`.
pub async fn region_tree(address: &str, containers: &[ContainerSeed], wait: Duration) -> Result<(), String> {
    tree(&mut EntityGraphInternalClient::new(channel("entitygraph", address, wait).await?), containers).await
}

async fn tree(graph: &mut EntityGraphInternalClient<Channel>, containers: &[ContainerSeed]) -> Result<(), String> {
    let requests: Vec<_> = batches(containers.to_vec(), BYTES_PER_CALL)
        .into_iter()
        .map(|containers| SeedContainersRequest { containers })
        .collect();
    let written = graph.seed_containers(tokio_stream::iter(requests)).await.map_err(failed("entitygraph"))?.into_inner().rows;
    let edges = containers.iter().filter(|node| node.parent.is_some()).count();
    println!("containers: {written} nodes, {edges} edges");
    Ok(())
}

/// Hands each global owner its rows, in calls that fit a message.
pub async fn hand_over(owners: &Owners, content: Content, wait: Duration) -> Result<(), String> {
    let mut shop = SolarSystemShopInternalClient::new(channel("shop", &owners.shop, wait).await?);
    // The items first: a rental price adds a row for an item the shop lacks, which the items would then leave alone.
    let mut written = 0;
    for items in batches(content.shop_items, BYTES_PER_CALL) {
        let request = SeedShopItemsRequest { items };
        written += shop.seed_shop_items(request).await.map_err(failed("shop"))?.into_inner().rows;
    }
    println!("shop items: {written} new");
    let mut written = 0;
    for prices in batches(content.rental_prices, BYTES_PER_CALL) {
        let request = SeedRentalPricesRequest { prices };
        written += shop.seed_rental_prices(request).await.map_err(failed("shop"))?.into_inner().rows;
    }
    println!("rental prices: {written}");

    let mut entitlement = EntitlementInternalClient::new(channel("entitlement", &owners.entitlement, wait).await?);
    let mut written = 0;
    for entitlements in batches(content.starters, BYTES_PER_CALL) {
        let request = SeedStarterEntitlementsRequest { entitlements };
        written += entitlement.seed_starter_entitlements(request).await.map_err(failed("entitlement"))?.into_inner().rows;
    }
    println!("starter entitlements: {written}");

    let mut reputation = ReputationInternalClient::new(channel("reputation", &owners.reputation, wait).await?);
    let counts = (content.scopes.len(), content.standings.len(), content.ladder.len());
    let mut requests: Vec<SeedReputationRequest> = Vec::new();
    requests.extend(batches(content.scopes, BYTES_PER_CALL).into_iter().map(|scopes| SeedReputationRequest { scopes, ..Default::default() }));
    requests.extend(
        batches(content.standings, BYTES_PER_CALL).into_iter().map(|standings| SeedReputationRequest { standings, ..Default::default() }),
    );
    requests.extend(batches(content.ladder, BYTES_PER_CALL).into_iter().map(|ladder| SeedReputationRequest { ladder, ..Default::default() }));
    for request in requests {
        reputation.seed_reputation(request).await.map_err(failed("reputation"))?;
    }
    println!("reputation: {} scopes, {} standings, {} ladder rows", counts.0, counts.1, counts.2);

    let mut library = BlueprintLibraryInternalClient::new(channel("blueprint library", &owners.blueprint_library, wait).await?);
    let mut written = 0;
    for blueprints in batches(content.blueprints, BYTES_PER_CALL) {
        let request = SeedBlueprintDefaultsRequest { blueprints };
        written += library.seed_blueprint_defaults(request).await.map_err(failed("blueprint library"))?.into_inner().rows;
    }
    println!("default blueprints: {written}");

    let mut scenarios = MissionScenarioInternalClient::new(channel("mission scenario", &owners.mission_scenario, wait).await?);
    let count = content.scenarios.len();
    let mut added = 0;
    for rows in batches(content.scenarios, BYTES_PER_CALL) {
        let request = SeedScenariosRequest { scenarios: rows };
        added += scenarios.seed_scenarios(request).await.map_err(failed("mission scenario"))?.into_inner().rows;
    }
    println!("scenarios: {count} ({added} new)");

    let mut broker = ContractBrokerInternalClient::new(channel("contract broker", &owners.contract_broker, wait).await?);
    let mut written = 0;
    for contracts in batches(content.contracts, BYTES_PER_CALL) {
        let request = SeedMissionContractsRequest { contracts };
        written += broker.seed_mission_contracts(request).await.map_err(failed("contract broker"))?.into_inner().rows;
    }
    println!("mission contracts: {written}");
    Ok(())
}

/// The whole seed: every region's `sc-entitygraph` (as `(name, address)`), then the global owners once. `wait` is how
/// long to keep trying an owner that does not answer yet.
pub async fn apply(content: Content, regions: &[(String, String)], owners: &Owners, wait: Duration) -> Result<(), String> {
    for (name, address) in regions {
        println!("region {name} ({address}):");
        region(address, &content.classes, &content.containers, wait).await?;
    }
    let global = Content { classes: Vec::new(), containers: Vec::new(), ..content };
    hand_over(owners, global, wait).await
}

#[cfg(test)]
mod tests {
    use sc_proto::internal::MissionContractSeed;

    use super::*;

    #[test]
    fn batches_fit_a_call_and_keep_every_row_in_order() {
        let rows: Vec<MissionContractSeed> =
            (0..10).map(|n| MissionContractSeed { id: n.to_string(), body: vec![0; 100] }).collect();
        let split = batches(rows.clone(), 350);
        assert!(split.len() > 1);
        assert!(split.iter().all(|batch| batch.iter().map(|row| row.encoded_len() + 8).sum::<usize>() <= 350));
        assert_eq!(split.concat(), rows);
        // A row larger than a batch still goes, alone.
        assert_eq!(batches(rows[..1].to_vec(), 1).len(), 1);
        assert!(batches(Vec::<MissionContractSeed>::new(), 10).is_empty());
    }
}

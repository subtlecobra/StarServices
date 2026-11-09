//! sc.external.services.entity_subscription.v1.EntitySubscriptionService. Answers who follows an
//! entity: the explicit rows of `entity_subscriptions`, plus the owner of the entity, which sc-entitygraph
//! owns and answers over `EntityGraphInternal.GetOwners`.

use std::{collections::BTreeMap, net::SocketAddr, pin::Pin, process::ExitCode, time::Duration};

use clap::Parser;
use futures_util::{stream, Stream};
use sc_db::{internal, Arango, DatabaseArgs};
use sc_api::internal::{entity_graph_internal_client::EntityGraphInternalClient, GetOwnersRequest};
use sc_proto::{
    services::entity_subscription::v1::{
    entity_subscription_service_server::{EntitySubscriptionService, EntitySubscriptionServiceServer},
    QueryEntitiesRequest, QueryEntitiesResponse, SubscriberEntities,
    },
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use serde_json::{json, Value};
use tonic::{
    codec::CompressionEncoding,
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::{info, warn};

/// sc-entitygraph owns `entities`, so this service creates only its own collection.
const COLLECTIONS: &[&str] = &["entity_subscriptions"];
const SUBSCRIBERS_PER_MESSAGE: usize = 256;
const GRAPH_TIMEOUT: Duration = Duration::from_secs(10);
const QUERY: &str = "
FOR s IN entity_subscriptions
    FILTER LENGTH(@entities) == 0 OR s.entity IN @entities
    FILTER LENGTH(@tags) == 0 OR LENGTH(INTERSECTION(s.tags, @tags)) > 0
    RETURN { subscriber: s.subscriber, entity: s.entity }";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// `sc-entitygraph`, asked who owns the entities of a query.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH", default_value = "http://sc-entitygraph:50051")]
    entitygraph: String,
}

struct Subscriptions {
    db: Arango,
    graph: EntityGraphInternalClient<Channel>,
}

fn geid(value: &Value) -> Option<u64> {
    value.as_str()?.parse().ok()
}

/// A row of `entity_subscriptions` as `(subscriber, entity)`.
fn pair(row: &Value) -> Option<(u64, u64)> {
    Some((geid(&row["subscriber"])?, geid(&row["entity"])?))
}

/// Groups `(subscriber, entity)` pairs by subscriber, in subscriber order, each entity once.
fn messages(pairs: impl IntoIterator<Item = (u64, u64)>) -> Vec<QueryEntitiesResponse> {
    let mut grouped: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (subscriber, entity) in pairs {
        let entities = grouped.entry(subscriber).or_default();
        if !entities.contains(&entity) {
            entities.push(entity);
        }
    }
    let results: Vec<_> =
        grouped.into_iter().map(|(subscriber, entities)| SubscriberEntities { subscriber, entities }).collect();
    results.chunks(SUBSCRIBERS_PER_MESSAGE).map(|chunk| QueryEntitiesResponse { results: chunk.to_vec() }).collect()
}

impl Subscriptions {
    /// The owners of these entities, as `(owner, entity)`.
    async fn owners(&self, entities: Vec<u64>) -> Result<Vec<(u64, u64)>, Status> {
        let mut request = Request::new(GetOwnersRequest { entities });
        request.set_timeout(GRAPH_TIMEOUT);
        let reply = self.graph.clone().get_owners(request).await.map_err(|status| {
            warn!(%status, "entitygraph did not answer for owners");
            Status::unavailable("entity owners unavailable")
        })?;
        Ok(reply.into_inner().owners.into_iter().map(|found| (found.owner, found.entity)).collect())
    }
}

#[tonic::async_trait]
impl EntitySubscriptionService for Subscriptions {
    type QueryEntitiesStream = Pin<Box<dyn Stream<Item = Result<QueryEntitiesResponse, Status>> + Send>>;

    async fn query_entities(
        &self,
        request: Request<QueryEntitiesRequest>,
    ) -> Result<Response<Self::QueryEntitiesStream>, Status> {
        let request = request.into_inner();
        if request.entities.is_empty() && request.tags.is_empty() {
            return Ok(Response::new(Box::pin(stream::empty())));
        }
        let entities: Vec<String> = request.entities.iter().map(u64::to_string).collect();
        let bind = json!({ "entities": entities, "tags": request.tags });
        let rows = self.db.aql(QUERY, bind).await.map_err(internal)?;
        let mut pairs: Vec<(u64, u64)> = rows.iter().filter_map(pair).collect();
        // A tag names subscriptions only. Owners follow the entities they own.
        if request.tags.is_empty() {
            pairs.extend(self.owners(request.entities).await?);
        }
        Ok(Response::new(Box::pin(stream::iter(messages(pairs).into_iter().map(Ok)))))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let endpoint = Endpoint::from_shared(args.entitygraph.clone()).map_err(|e| format!("--entitygraph: {e}"))?;
    let service = Subscriptions {
        db: Arango::connect(&args.database.arango, COLLECTIONS).await?,
        graph: EntityGraphInternalClient::new(endpoint.connect_lazy()),
    };
    let server = EntitySubscriptionServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "entity subscription listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use sc_api::internal::{
        entity_graph_internal_server::{EntityGraphInternal, EntityGraphInternalServer},
        EntityOwner, GetOwnersResponse, LoadAreaRequest, LoadAreaResponse,
    };

    use super::*;

    #[test]
    fn large_ids_survive_and_messages_are_chunked() {
        let pairs = (0..SUBSCRIBERS_PER_MESSAGE as u64 + 1).map(|subscriber| (subscriber, u64::MAX));
        let chunks = messages(pairs);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].results[0].entities, vec![u64::MAX]);
        assert_eq!(chunks[1].results.len(), 1);
    }

    #[test]
    fn a_subscriber_gets_each_entity_once_in_subscriber_order() {
        let chunks = messages([(9, 1), (2, 1), (9, 1), (9, 3)]);
        let found: Vec<_> = chunks[0].results.iter().map(|r| (r.subscriber, r.entities.clone())).collect();
        assert_eq!(found, [(2, vec![1]), (9, vec![1, 3])]);
    }

    #[test]
    fn rows_with_bad_ids_are_skipped() {
        assert_eq!(pair(&json!({ "subscriber": "18446744073709551615", "entity": "4" })), Some((u64::MAX, 4)));
        assert_eq!(pair(&json!({ "subscriber": "x", "entity": "4" })), None);
    }

    /// Owns entity `n` by `n + 1000`, except entity 0, which has no owner.
    struct FakeGraph;

    #[tonic::async_trait]
    impl EntityGraphInternal for FakeGraph {
        type LoadAreaStream = Pin<Box<dyn Stream<Item = Result<LoadAreaResponse, Status>> + Send>>;

        async fn load_area(&self, _: Request<LoadAreaRequest>) -> Result<Response<Self::LoadAreaStream>, Status> {
            Err(Status::unimplemented("load_area"))
        }

        async fn get_owners(&self, request: Request<GetOwnersRequest>) -> Result<Response<GetOwnersResponse>, Status> {
            let entities = request.into_inner().entities.into_iter().filter(|&entity| entity != 0);
            let owners = entities.map(|entity| EntityOwner { entity, owner: entity + 1000 }).collect();
            Ok(Response::new(GetOwnersResponse { owners }))
        }

        async fn release_player(&self, _: Request<sc_api::internal::ReleasePlayerRequest>) -> Result<Response<sc_api::internal::ReleasePlayerResponse>, Status> {
            Err(Status::unimplemented("release_player"))
        }

        // ---- Admin inventories: not used by this test ----
        async fn admin_inventory_tree(
            &self,
            _: Request<sc_api::internal::AdminInventoryTreeRequest>,
        ) -> Result<Response<sc_api::internal::AdminInventoryTreeResponse>, Status> {
            Err(Status::unimplemented("admin_inventory_tree"))
        }

        async fn admin_move_item(
            &self,
            _: Request<sc_api::internal::AdminMoveItemRequest>,
        ) -> Result<Response<sc_api::internal::AdminMoveItemResponse>, Status> {
            Err(Status::unimplemented("admin_move_item"))
        }

        async fn admin_add_item(
            &self,
            _: Request<sc_api::internal::AdminAddItemRequest>,
        ) -> Result<Response<sc_api::internal::AdminAddItemResponse>, Status> {
            Err(Status::unimplemented("admin_add_item"))
        }

        async fn admin_remove_item(
            &self,
            _: Request<sc_api::internal::AdminRemoveItemRequest>,
        ) -> Result<Response<sc_api::internal::AdminRemoveItemResponse>, Status> {
            Err(Status::unimplemented("admin_remove_item"))
        }

        async fn admin_search_entity_classes(
            &self,
            _: Request<sc_api::internal::AdminSearchEntityClassesRequest>,
        ) -> Result<Response<sc_api::internal::AdminSearchEntityClassesResponse>, Status> {
            Err(Status::unimplemented("admin_search_entity_classes"))
        }
        async fn seed_entity_classes(
            &self,
            _: Request<tonic::Streaming<sc_api::internal::SeedEntityClassesRequest>>,
        ) -> Result<Response<sc_api::internal::SeedResponse>, Status> {
            Err(Status::unimplemented("seed_entity_classes"))
        }
        async fn seed_containers(
            &self,
            _: Request<tonic::Streaming<sc_api::internal::SeedContainersRequest>>,
        ) -> Result<Response<sc_api::internal::SeedResponse>, Status> {
            Err(Status::unimplemented("seed_containers"))
        }
    }

    async fn fake_graph() -> EntityGraphInternalClient<Channel> {
        let address: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tokio::spawn(Server::builder().add_service(EntityGraphInternalServer::new(FakeGraph)).serve(address));
        let endpoint = Endpoint::from_shared(format!("http://{address}")).unwrap();
        while tokio::net::TcpStream::connect(address).await.is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        EntityGraphInternalClient::new(endpoint.connect_lazy())
    }

    #[tokio::test]
    async fn owners_come_from_the_entity_graph() {
        let service = Subscriptions { db: Arango::lazy("http://127.0.0.1:1"), graph: fake_graph().await };
        assert_eq!(service.owners(vec![5, 0]).await.unwrap(), [(1005, 5)]);
    }

    #[tokio::test]
    async fn no_entity_graph_is_unavailable() {
        let endpoint = Endpoint::from_static("http://127.0.0.1:1");
        let service = Subscriptions { db: Arango::lazy("http://127.0.0.1:1"), graph: EntityGraphInternalClient::new(endpoint.connect_lazy()) };
        assert_eq!(service.owners(vec![5]).await.unwrap_err().code(), tonic::Code::Unavailable);
    }

    /// Against a real ArangoDB, for `entity_subscriptions`, with a stand-in for sc-entitygraph:
    /// `SC_TEST_ARANGO=<url> cargo test -p sc-entity-subscription -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn subscribers_come_from_rows_and_ownership() {
        let url = std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO");
        let db = Arango::connect(&url, COLLECTIONS).await.unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
        let (entity, owner, follower) = (stamp, stamp + 1000, stamp + 2);
        let tag = format!("party-{stamp}");
        let insert = "INSERT @doc INTO entity_subscriptions";
        let doc = json!({ "subscriber": follower.to_string(), "entity": entity.to_string(), "tags": [tag] });
        db.aql(insert, json!({ "doc": doc })).await.unwrap();

        let service = Subscriptions { db: db.clone(), graph: fake_graph().await };
        let ask = |entities: Vec<u64>, tags: Vec<String>| {
            Request::new(QueryEntitiesRequest { entities, tags })
        };
        let collect = |stream: <Subscriptions as EntitySubscriptionService>::QueryEntitiesStream| async {
            let replies: Vec<_> = stream.collect().await;
            let results = replies.into_iter().flat_map(|reply| reply.unwrap().results);
            let mut found: Vec<_> = results.map(|result| (result.subscriber, result.entities)).collect();
            found.sort();
            found
        };
        let by_entity = service.query_entities(ask(vec![entity], Vec::new())).await.unwrap().into_inner();
        let mut expected = [(owner, vec![entity]), (follower, vec![entity])];
        expected.sort();
        assert_eq!(collect(by_entity).await, expected);
        let by_tag = service.query_entities(ask(Vec::new(), vec![tag])).await.unwrap().into_inner();
        assert_eq!(collect(by_tag).await, [(follower, vec![entity])]);
        let nothing = service.query_entities(ask(Vec::new(), Vec::new())).await.unwrap().into_inner();
        assert!(collect(nothing).await.is_empty());

        let remove = "FOR s IN entity_subscriptions FILTER s.entity == @key REMOVE s IN entity_subscriptions";
        db.aql(remove, json!({ "key": entity.to_string() })).await.unwrap();
    }
}

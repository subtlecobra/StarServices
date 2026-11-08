//! The containers and entities a DGS has authority over, as sc-entitygraph, which owns the container tree, walks them
//! for it (`EntityGraphInternal.LoadArea`). A container belongs to the nearest root at or above it, across every DGS of
//! the system, so a root takes everything under it except what a deeper root claims. See docs/DGS_ZONES.md.

use std::ops::Range;

use sc_api::internal::{entity_graph_internal_client::EntityGraphInternalClient, LoadAreaRequest, LoadAreaResponse};
use tonic::transport::Channel;

/// One entity id range per container, empty for a container with no entities.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Area {
    entities: Vec<Range<u64>>,
}

impl Area {
    pub fn containers(&self) -> u64 {
        self.entities.len() as u64
    }

    pub fn entities(&self) -> u64 {
        self.entities.iter().map(|range| range.end - range.start).sum()
    }

    fn add(&mut self, message: LoadAreaResponse) {
        let ranges = message.containers.into_iter().map(|c| c.start_id..c.start_id.saturating_add(c.entities));
        self.entities.extend(ranges);
    }
}

/// The whole area, or the first error: a root the tree lacks is NOT_FOUND.
pub async fn load(client: &mut EntityGraphInternalClient<Channel>, request: LoadAreaRequest) -> Result<Area, String> {
    let mut stream = client.load_area(request).await.map_err(|status| status.message().to_string())?.into_inner();
    let mut area = Area::default();
    while let Some(message) = stream.message().await.map_err(|status| status.message().to_string())? {
        area.add(message);
    }
    Ok(area)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, pin::Pin, time::{Duration, Instant}};

    use futures_util::{stream, Stream};
    use sc_api::internal::{
        entity_graph_internal_server::{EntityGraphInternal, EntityGraphInternalServer},
        AreaContainer, GetOwnersRequest, GetOwnersResponse,
    };
    use tonic::{transport::Server, Request, Response, Status};

    use super::*;

    /// The size of `pyro-1`, the DGS with the most containers: see docs/DGS_ZONES.md.
    const CONTAINERS: u64 = 35_769;
    const ENTITIES: u64 = 553_438;
    /// As sc-entitygraph sends them.
    const PER_MESSAGE: usize = 4096;

    struct FakeGraph;

    #[tonic::async_trait]
    impl EntityGraphInternal for FakeGraph {
        type LoadAreaStream = Pin<Box<dyn Stream<Item = Result<LoadAreaResponse, Status>> + Send>>;

        async fn load_area(&self, request: Request<LoadAreaRequest>) -> Result<Response<Self::LoadAreaStream>, Status> {
            let request = request.into_inner();
            if request.roots != ["pyro1"] {
                return Err(Status::not_found(format!("no container in {} named {}", request.system, request.roots.join(", "))));
            }
            // Spread the entities over the containers, the remainder on the first, and leave every tenth one empty.
            let per = ENTITIES / (CONTAINERS - CONTAINERS / 10);
            let mut left = ENTITIES;
            let containers: Vec<AreaContainer> = (0..CONTAINERS)
                .map(|n| {
                    let entities = if n % 10 == 9 { 0 } else { per.min(left) };
                    left -= entities;
                    AreaContainer { start_id: 1_000_000 + n * 1000, entities }
                })
                .collect();
            let mut messages: Vec<LoadAreaResponse> =
                containers.chunks(PER_MESSAGE).map(|chunk| LoadAreaResponse { containers: chunk.to_vec() }).collect();
            messages[0].containers[0].entities += left;
            Ok(Response::new(Box::pin(stream::iter(messages.into_iter().map(Ok)))))
        }

        async fn get_owners(&self, _: Request<GetOwnersRequest>) -> Result<Response<GetOwnersResponse>, Status> {
            Err(Status::unimplemented("get_owners"))
        }

        async fn revoke_items(
            &self,
            _: Request<sc_api::internal::RevokeItemsRequest>,
        ) -> Result<Response<sc_api::internal::RevokeItemsResponse>, Status> {
            Err(Status::unimplemented("revoke_items"))
        }

        async fn grant_items(
            &self,
            _: Request<sc_api::internal::GrantItemsRequest>,
        ) -> Result<Response<sc_api::internal::GrantItemsResponse>, Status> {
            Err(Status::unimplemented("grant_items"))
        }

        async fn ensure_loadout(
            &self,
            _: Request<sc_api::internal::EnsureLoadoutRequest>,
        ) -> Result<Response<sc_api::internal::EnsureLoadoutResponse>, Status> {
            Err(Status::unimplemented("ensure_loadout"))
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

    async fn client() -> EntityGraphInternalClient<Channel> {
        let address: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        tokio::spawn(Server::builder().add_service(EntityGraphInternalServer::new(FakeGraph)).serve(address));
        loop {
            if let Ok(client) = EntityGraphInternalClient::connect(format!("http://{address}")).await {
                return client;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn request(roots: &[&str]) -> LoadAreaRequest {
        LoadAreaRequest { system: "pyro".into(), roots: roots.iter().map(|r| r.to_string()).collect(), ..Default::default() }
    }

    /// What the move to sc-entitygraph adds to a DGS's start: the transfer of the largest area over gRPC. The walk itself
    /// runs the same queries as before, in sc-entitygraph.
    #[tokio::test]
    async fn the_largest_area_arrives_whole_and_quickly() {
        let mut client = client().await;
        let started = Instant::now();
        let area = load(&mut client, request(&["pyro1"])).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!((area.containers(), area.entities()), (CONTAINERS, ENTITIES));
        println!("{CONTAINERS} containers, {ENTITIES} entities in {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn a_root_the_tree_lacks_is_an_error_naming_it() {
        let mut client = client().await;
        let error = load(&mut client, request(&["pyro9"])).await.unwrap_err();
        assert_eq!(error, "no container in pyro named pyro9");
    }

    #[test]
    fn a_container_with_no_entities_still_counts_as_a_container() {
        let mut area = Area::default();
        area.add(LoadAreaResponse { containers: vec![AreaContainer { start_id: 5, entities: 0 }] });
        assert_eq!((area.containers(), area.entities()), (1, 0));
    }
}

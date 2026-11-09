use super::*;

use sc_proto::{
    common::command::v1::NotifyDestination,
    services::{
        entitygraph::v1::NotificationBehavior,
        push::v1::PushDestination,
    },
};

fn arguments(only_on_failure: bool) -> MutationArguments {
    let push = PushDestination { addresses: vec!["address:player_id:1".into()], topics: vec!["commands/s/global/x/c1".into()] };
    let destination = NotifyDestination { destination: Some(notify_destination::Destination::Push(push)) };
    MutationArguments {
        id: "c1".into(),
        notifications: Some(NotificationBehavior { destinations: vec![destination], only_on_failure }),
        ..Default::default()
    }
}

#[test]
fn a_command_result_goes_to_the_topic_the_client_named_with_the_response_inside() {
    let response = MutateEntitiesResponse { body: Some(MutateEntitiesResponseBody {}) };
    let (topics, value) = command_result(&arguments(false), MUTATE_RESPONSE, &response);
    assert_eq!(topics, ["commands/s/global/x/c1"]);
    let body = CommandResponseBody::decode(value.as_slice()).unwrap();
    assert_eq!(body.id, "c1");
    assert!(body.status.is_none());
    let context = body.context.unwrap();
    assert_eq!(context.type_url, "type.googleapis.com/sc.external.services.entitygraph.v1.MutateEntitiesResponse");
    assert_eq!(MutateEntitiesResponse::decode(context.value.as_slice()).unwrap(), response);
}

#[test]
fn only_on_failure_means_silence_after_success() {
    let response = MutateEntitiesResponse::default();
    assert!(command_result(&arguments(true), MUTATE_RESPONSE, &response).0.is_empty());
}

#[test]
fn assertions_become_failed_preconditions() {
    let error = "aql: 400 Bad Request \"AQL: precondition: inventory is full\"".to_string();
    let status = write_error(error);
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(status.message(), "inventory is full");
}

/// Against a real ArangoDB: `SC_TEST_ARANGO=http://127.0.0.1:18600 cargo test -p sc-entitygraph -- --ignored`.
/// Each run uses fresh ids, so runs do not collide, but the test accounts get the first GEIDs of a new
/// state file and their entities are reset: do not point this at a database with real players.
mod live {
    use std::{sync::Arc, time::Duration};

    use futures_util::StreamExt;
    use sc_proto::services::entitygraph::v1::{
        change_snapshot_command, create_command, entity_edge_vertex, entity_filter,
        entity_graph_command::Params, entity_graph_service_client::EntityGraphServiceClient, erase_command,
        inventory_query, inventory_tree_filter, move_command, node_properties, scalar_value, set_snapshot_command,
        ChangeSnapshotCommand, ComparisonOperator, ContainerGraphQuery, ContainerGraphStreamQuery,
        ContainerProjection, ContainerQueryRequestBody, ContainerQueryStreamRequestBody, ContainerTreeProjection,
        CreateCommand, CreateInventoryArguments, EntityEdge, EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex,
        EntityFilter, EntityGraph, EntityGraphCommand, EntityGraphQuery, EntityNodeProperties, EntityProjection,
        EntityQueryRequestBody, EntitySnapshot, EntityTreeProjection, EntityVariable, EraseCommand,
        GetEntityStowContextsByGeidsRequestBody, GetEntityStowContextsByParentUrnsRequestBody,
        GetInventoriesRequestBody, InventoryConfiguration, InventoryNodeProperties, InventoryQuery,
        InventoryQueryRequestBody, InventoryTreeFilter, InventoryType, MoveCommand, MoveRequestBody,
        MutateEntitiesRequestBody, Node, NodeProperties, NodeType, PhysicalInventoryConfiguration,
        PhysicalInventoryEdgeConfiguration, PropertyFilter, ScalarValue, Scope, ScopeType, SetSnapshotCommand,
        StreamStowContextsByOwnerExternalRequestBody,
    };
    use sc_proto::services::entitygraph::v1::{entity_edge_properties, inventory_configuration};

    use super::*;

    fn fresh() -> u64 {
        now_ms() * 1000 + u64::from(std::process::id() % 1000)
    }

    struct Fixture {
        graph: Arc<Graph>,
        token: String,
        other_token: String,
        geid: u64,
    }

    async fn fixture() -> Fixture {
        let url = std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO");
        let dir = std::env::temp_dir().join(format!("sc-entitygraph-{}", fresh()));
        let state = State::open(&dir).unwrap();
        let mine = state.ensure_account(&format!("tester{}", fresh())).unwrap();
        let other = state.ensure_account(&format!("other{}", fresh())).unwrap();
        let (token, other_token) = (state.issue_token(&mine), state.issue_token(&other));
        let endpoint = Endpoint::from_static("http://127.0.0.1:1");
        let graph = Graph {
            state,
            db: open(&url).await.unwrap(),
            push: PushPublisherClient::new(endpoint.connect_lazy()),
            known_players: Mutex::default(),
        };
        // The state hands out the same first GEIDs in every run, and those would meet old documents.
        let stale = "FOR e IN entities FILTER e._key IN @keys REMOVE e IN entities";
        let keys = [mine.geid.to_string(), other.geid.to_string()];
        graph.db.aql(stale, json!({ "keys": keys })).await.unwrap();
        let stale = "FOR i IN inventories FILTER i.ownerId IN @keys REMOVE i IN inventories";
        graph.db.aql(stale, json!({ "keys": keys })).await.unwrap();
        Fixture { graph: Arc::new(graph), token, other_token, geid: mine.geid }
    }

    fn signed<T>(token: &str, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        request
    }

    fn geid_query(geid: u64) -> EntityGraphQuery {
        let value = ScalarValue { scalar_type: Some(scalar_value::ScalarType::UnsignedBigintValue(geid)) };
        EntityGraphQuery {
            filter: Some(EntityFilter {
                filter_type: Some(entity_filter::FilterType::PropertyFilter(PropertyFilter {
                    property: "geid".into(),
                    operator: ComparisonOperator::Equal as i32,
                    values: vec![value],
                })),
            }),
            ..Default::default()
        }
    }

    fn entity_body(query: EntityGraphQuery) -> EntityQueryRequest {
        let scope = Some(Scope { r#type: ScopeType::Global as i32, shard_id: String::new() });
        EntityQueryRequest { body: Some(EntityQueryRequestBody { scope, query: Some(query) }) }
    }

    fn geids(graph: &EntityGraph) -> Vec<u64> {
        let properties = graph.nodes.iter().filter_map(|node| node.properties.as_ref()?.properties.as_ref());
        properties
            .filter_map(|properties| match properties {
                node_properties::Properties::EntityProperties(entity) => Some(entity.geid),
                _ => None,
            })
            .collect()
    }

    fn entity_node(geid: u64, owner: u64) -> Node {
        let properties = EntityNodeProperties { geid, owner_id: owner, parent_urn: format!("urn:{geid}"), ..Default::default() };
        Node {
            properties: Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(properties)) }),
            r#type: NodeType::Entity as i32,
        }
    }

    fn inventory_node(id: &str, owner: u64, capacity: i32) -> Node {
        let properties = InventoryNodeProperties {
            id: id.into(),
            owner_id: owner,
            inventory_type: InventoryType::Physical as i32,
            configuration: Some(InventoryConfiguration {
                configuration: Some(inventory_configuration::Configuration::Physical(PhysicalInventoryConfiguration {
                    capacity,
                    occupancy: 0,
                })),
            }),
            ..Default::default()
        };
        Node {
            properties: Some(NodeProperties { properties: Some(node_properties::Properties::InventoryProperties(properties)) }),
            r#type: NodeType::Inventory as i32,
        }
    }

    fn stowed(geid: u64, inventory: &str, occupancy: u32) -> EntityEdge {
        EntityEdge {
            r#type: EntityEdgeType::StowedIn as i32,
            start: Some(EntityEdgeVertex { r#type: 1, id: Some(entity_edge_vertex::Id::EntityId(geid)) }),
            end: Some(EntityEdgeVertex { r#type: 2, id: Some(entity_edge_vertex::Id::InventoryId(inventory.into())) }),
            properties: Some(EntityEdgeProperties {
                inventory: Some(entity_edge_properties::Inventory::Physical(PhysicalInventoryEdgeConfiguration { occupancy })),
                ..Default::default()
            }),
        }
    }

    fn attached(geid: u64, parent: u64) -> EntityEdge {
        EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(EntityEdgeVertex { r#type: 1, id: Some(entity_edge_vertex::Id::EntityId(geid)) }),
            end: Some(EntityEdgeVertex { r#type: 1, id: Some(entity_edge_vertex::Id::EntityId(parent)) }),
            properties: Some(EntityEdgeProperties::default()),
        }
    }

    fn mutate(params: Vec<Params>) -> MutateEntitiesRequest {
        let commands = params.into_iter().map(|params| EntityGraphCommand { params: Some(params) }).collect();
        // A command id is applied once, so each request needs its own.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = format!("test-{}-{}", fresh(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let arguments = Some(MutationArguments { id, ..Default::default() });
        MutateEntitiesRequest { body: Some(MutateEntitiesRequestBody { arguments, commands }) }
    }

    fn create(graph: EntityGraph) -> Params {
        Params::Create(CreateCommand { creates: vec![create_command::Body { graphs: Some(graph) }], inventory_id: String::new() })
    }

    fn move_request(geid: u64, from: &str, to: &str) -> MoveRequest {
        let body = move_command::Body {
            entity_id: geid,
            from_inventory_id: from.into(),
            to_inventory_id: to.into(),
            ..Default::default()
        };
        let command = MoveCommand { moves: vec![body] };
        MoveRequest {
            body: Some(MoveRequestBody { arguments: Some(MutationArguments::default()), r#move: Some(command) }),
        }
    }

    async fn stow_of(fixture: &Fixture, geid: u64) -> sc_proto::services::entitygraph::v1::EntityStowContext {
        let body = GetEntityStowContextsByGeidsRequestBody { geids: vec![geid], ..Default::default() };
        let request = signed(&fixture.token, GetEntityStowContextsByGeidsRequest { body: Some(body) });
        let reply = fixture.graph.get_entity_stow_contexts_by_geids(request).await.unwrap().into_inner();
        reply.body.unwrap().results.remove(0)
    }

    async fn occupancy(fixture: &Fixture, id: &str) -> i64 {
        let query = "FOR i IN inventories FILTER i.id == @id RETURN i.configuration.physical.occupancy";
        fixture.graph.db.aql(query, json!({ "id": id })).await.unwrap()[0].as_i64().unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn the_player_exists_as_soon_as_it_is_queried() {
        let fixture = fixture().await;
        let request = signed(&fixture.token, entity_body(geid_query(fixture.geid)));
        let reply = fixture.graph.entity_query(request).await.unwrap().into_inner().body.unwrap();
        assert_eq!(geids(&reply.results.unwrap()), [fixture.geid]);
        let context = stow_of(&fixture, fixture.geid).await;
        assert_eq!(context.owner_id, fixture.geid);
        assert!(context.is_stowed);
        let request = signed(&fixture.token, GetInventoriesRequest { body: Some(GetInventoriesRequestBody::default()) });
        let reply = fixture.graph.get_inventories(request).await.unwrap().into_inner().body.unwrap();
        assert_eq!(reply.inventories.len(), 1);
    }

    #[tokio::test]
    #[ignore]
    async fn owners_come_from_the_entities() {
        let fixture = fixture().await;
        let request = signed(&fixture.token, entity_body(geid_query(fixture.geid)));
        fixture.graph.entity_query(request).await.unwrap();
        let ask = GetOwnersRequest { entities: vec![fixture.geid, fresh()] };
        let reply = EntityGraphInternal::get_owners(&*fixture.graph, Request::new(ask)).await.unwrap().into_inner();
        assert_eq!(reply.owners, [EntityOwner { entity: fixture.geid, owner: fixture.geid }]);
    }

    #[tokio::test]
    #[ignore]
    async fn items_move_atomically_between_inventories() {
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let (item, big_item) = (base, base + 1);
        let (locker, crate_box) = (format!("{me}:Container:{base}"), format!("{me}:Container:{}", base + 1));
        let seed = EntityGraph {
            nodes: vec![
                inventory_node(&locker, me, 10),
                inventory_node(&crate_box, me, 3),
                entity_node(item, me),
                entity_node(big_item, me),
            ],
            edges: vec![stowed(item, &locker, 2), stowed(big_item, &locker, 5)],
        };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create(seed)]))).await.unwrap();
        assert_eq!(occupancy(&fixture, &locker).await, 7);

        graph.r#move(signed(&fixture.token, move_request(item, &locker, &crate_box))).await.unwrap();
        assert_eq!(stow_of(&fixture, item).await.inventory_id.as_deref(), Some(crate_box.as_str()));
        assert_eq!((occupancy(&fixture, &locker).await, occupancy(&fixture, &crate_box).await), (5, 2));

        let wrong_source = graph.r#move(signed(&fixture.token, move_request(item, &locker, &crate_box))).await;
        assert_eq!(wrong_source.unwrap_err().code(), tonic::Code::FailedPrecondition);

        // It fails after the source was already credited: nothing of that may survive.
        let too_big = graph.r#move(signed(&fixture.token, move_request(big_item, &locker, &crate_box))).await;
        assert_eq!(too_big.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert_eq!(stow_of(&fixture, big_item).await.inventory_id.as_deref(), Some(locker.as_str()));
        assert_eq!((occupancy(&fixture, &locker).await, occupancy(&fixture, &crate_box).await), (5, 2));
        let edges = "FOR x IN entity_edges FILTER x._key == @key RETURN x._to";
        let key = format!("2_{big_item}");
        let to = graph.db.aql(edges, json!({ "key": key })).await.unwrap();
        assert_eq!(to, [json!(format!("inventories/{locker}"))]);

        let foreign = graph.r#move(signed(&fixture.other_token, move_request(big_item, &locker, &crate_box))).await;
        assert_eq!(foreign.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let anonymous = graph.r#move(Request::new(move_request(big_item, &locker, &crate_box))).await;
        assert_eq!(anonymous.unwrap_err().code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    #[ignore]
    async fn trees_snapshots_and_erasure_follow_the_edges() {
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let (ship, weapon, magazine) = (base, base + 1, base + 2);
        let locker = format!("{ship}:Container:0");
        let seed = EntityGraph {
            nodes: vec![entity_node(ship, me), entity_node(weapon, me), entity_node(magazine, me), inventory_node(&locker, me, 0)],
            edges: vec![stowed(weapon, &locker, 1), attached(magazine, weapon)],
        };
        let variable = |name_crc, byte: u8| EntityVariable { name_crc, r#type: 1, flags: 4, snapshot: vec![byte] };
        let set = Params::SetSnapshot(SetSnapshotCommand {
            snapshots: vec![set_snapshot_command::Body {
                snapshot: Some(EntitySnapshot { entity_id: weapon, variables: vec![variable(1, 1), variable(2, 2)], version: 1 }),
            }],
            inventory_id: String::new(),
        });
        let change = Params::ChangeSnapshot(ChangeSnapshotCommand {
            updates: vec![change_snapshot_command::Body {
                snapshot: Some(EntitySnapshot { entity_id: weapon, variables: vec![variable(2, 9)], version: 2 }),
            }],
            inventory_id: String::new(),
        });
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create(seed), set, change]))).await.unwrap();

        let tree = EntityTreeProjection { enabled: true, include_inventory_nodes: true, ..Default::default() };
        let projection = EntityProjection { tree: Some(tree), snapshots: true, ..Default::default() };
        let query = EntityGraphQuery { projection: Some(projection), ..geid_query(ship) };
        let reply = graph.entity_query(signed(&fixture.token, entity_body(query))).await.unwrap().into_inner().body.unwrap();
        let results = reply.results.unwrap();
        let mut found = geids(&results);
        found.sort();
        assert_eq!(found, [ship, weapon, magazine]);
        assert_eq!(results.nodes.len(), 4, "the locker is a node of the tree");
        assert_eq!(results.edges.len(), 3);
        let snapshot = reply.snapshots.iter().find(|snapshot| snapshot.entity_id == weapon).unwrap();
        assert_eq!(snapshot.version, 2);
        let mut bytes: Vec<_> = snapshot.variables.iter().map(|v| (v.name_crc, v.snapshot[0])).collect();
        bytes.sort();
        assert_eq!(bytes, [(1, 1), (2, 9)]);

        let plain = EntityTreeProjection { enabled: true, ..Default::default() };
        let query = EntityGraphQuery {
            projection: Some(EntityProjection { tree: Some(plain), ..Default::default() }),
            ..geid_query(weapon)
        };
        let reply = graph.entity_query(signed(&fixture.token, entity_body(query))).await.unwrap().into_inner().body.unwrap();
        let mut found = geids(&reply.results.unwrap());
        found.sort();
        assert_eq!(found, [weapon, magazine]);

        let tree = InventoryTreeFilter { root: Some(inventory_tree_filter::Root::EntityGeid(ship)), ..Default::default() };
        let query = InventoryQuery { find_by: Some(inventory_query::FindBy::Tree(tree)), ..Default::default() };
        let request = InventoryQueryRequest { body: Some(InventoryQueryRequestBody { query: Some(query) }) };
        let reply = graph.inventory_query(signed(&fixture.token, request)).await.unwrap().into_inner().body.unwrap();
        assert_eq!(reply.results.len(), 1);
        assert_eq!(reply.results[0].properties.as_ref().unwrap().id, locker);

        let body = GetEntityStowContextsByParentUrnsRequestBody { parent_urns: vec![format!("urn:{weapon}")], ..Default::default() };
        let request = GetEntityStowContextsByParentUrnsRequest { body: Some(body) };
        let reply = graph.get_entity_stow_contexts_by_parent_urns(Request::new(request)).await.unwrap();
        assert_eq!(reply.into_inner().body.unwrap().results[0].entity_id, weapon);

        let request = StreamStowContextsByOwnerRequest { body: Some(StreamStowContextsByOwnerExternalRequestBody {}) };
        let stream = graph.stream_stow_contexts_by_owner(signed(&fixture.token, request)).await.unwrap().into_inner();
        let chunks: Vec<_> = stream.collect().await;
        let streamed: Vec<u64> = chunks.into_iter().flat_map(|chunk| chunk.unwrap().body.unwrap().results).map(|c| c.entity_id).collect();
        assert!(streamed.contains(&fixture.geid) && streamed.contains(&magazine));

        let erase = Params::Erase(EraseCommand {
            erasures: vec![erase_command::Body { entity_id: ship, preconditions: Vec::new() }],
            inventory_id: String::new(),
        });
        graph.mutate_entities(signed(&fixture.token, mutate(vec![erase]))).await.unwrap();
        let left = "FOR e IN entities FILTER e._key IN @keys RETURN e._key";
        let keys = [ship.to_string(), weapon.to_string(), magazine.to_string()];
        assert!(graph.db.aql(left, json!({ "keys": keys })).await.unwrap().is_empty());
        let left = "FOR i IN inventories FILTER i.id == @id RETURN i";
        assert!(graph.db.aql(left, json!({ "id": locker })).await.unwrap().is_empty());
        let left = "FOR s IN entity_snapshots FILTER s._key == @key RETURN s";
        assert!(graph.db.aql(left, json!({ "key": weapon.to_string() })).await.unwrap().is_empty());
        let left = "FOR x IN entity_edges FILTER x.startId IN @keys RETURN x";
        assert!(graph.db.aql(left, json!({ "keys": keys })).await.unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn lazy_targets_and_containers() {
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let item = fresh();
        let locker = format!("{me}:Container:{item}");
        let target = format!("{me}:Container:{}", item + 1);
        let seed = EntityGraph { nodes: vec![inventory_node(&locker, me, 0), entity_node(item, me)], edges: vec![stowed(item, &locker, 0)] };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create(seed)]))).await.unwrap();
        let mut request = move_request(item, &locker, &target);
        let lazy = CreateInventoryArguments {
            properties: Some(InventoryNodeProperties { owner_id: me, ..Default::default() }),
            reference: 0,
        };
        request.body.as_mut().unwrap().r#move.as_mut().unwrap().moves[0].target_inventory_lazy_creation = Some(lazy);
        graph.r#move(signed(&fixture.token, request)).await.unwrap();
        assert_eq!(stow_of(&fixture, item).await.inventory_id.as_deref(), Some(target.as_str()));

        let (top, child) = (item.to_string(), (item + 1).to_string());
        let insert = "INSERT @doc INTO containers";
        for geid in [&top, &child] {
            let doc = json!({ "_key": geid, "geid": geid, "guid": "g", "labels": [1], "generation": 1 });
            graph.db.aql(insert, json!({ "doc": doc })).await.unwrap();
        }
        let edge = json!({ "_from": format!("containers/{child}"), "_to": format!("containers/{top}"), "type": 1, "startId": child, "endId": top });
        graph.db.aql("INSERT @doc INTO container_edges", json!({ "doc": edge })).await.unwrap();

        let tree = ContainerTreeProjection { enabled: true, ..Default::default() };
        let value = ScalarValue { scalar_type: Some(scalar_value::ScalarType::StringValue(top.clone())) };
        let wanted = sc_proto::services::entitygraph::v1::ContainerFilter {
            filter_type: Some(sc_proto::services::entitygraph::v1::container_filter::FilterType::PropertyFilter(PropertyFilter {
                property: "geid".into(),
                operator: ComparisonOperator::Equal as i32,
                values: vec![value],
            })),
        };
        let query = ContainerGraphQuery {
            filter: Some(wanted),
            projection: Some(ContainerProjection { tree: Some(tree), ..Default::default() }),
            ..Default::default()
        };
        let request = ContainerQueryRequest { body: Some(ContainerQueryRequestBody { scope: None, query: Some(query) }) };
        let reply = graph.container_query(Request::new(request)).await.unwrap().into_inner().body.unwrap();
        let results = reply.results.unwrap();
        assert_eq!((results.nodes.len(), results.edges.len()), (2, 1));

        let query = ContainerGraphStreamQuery { geid: item, ..Default::default() };
        let body = ContainerQueryStreamRequestBody { scope: None, query: Some(query), batch_size: 1 };
        let request = ContainerQueryStreamRequest { body: Some(body) };
        let stream = graph.container_query_stream(Request::new(request)).await.unwrap().into_inner();
        assert_eq!(stream.collect::<Vec<_>>().await.len(), 2);

        graph.db.aql("FOR c IN containers FILTER c._key IN @k REMOVE c IN containers", json!({ "k": [top, child] })).await.unwrap();
    }

    fn classed(geid: u64, owner: u64, class: u32, stack: u32) -> Node {
        let mut node = entity_node(geid, owner);
        if let Some(NodeProperties { properties: Some(node_properties::Properties::EntityProperties(entity)) }) = &mut node.properties {
            entity.class_guid_crc = class;
            entity.stack_size = stack;
        }
        node
    }

    fn create_into(graph: EntityGraph, inventory: &str) -> Params {
        Params::Create(CreateCommand { creates: vec![create_command::Body { graphs: Some(graph) }], inventory_id: inventory.into() })
    }

    async fn inventory_doc(fixture: &Fixture, id: &str) -> Option<serde_json::Value> {
        let query = "FOR i IN inventories FILTER i.id == @id RETURN i";
        fixture.graph.db.aql(query, json!({ "id": id })).await.unwrap().pop()
    }

    /// The type filters of an inventory: the item banks' (a `Location` inventory) and a container class's own.
    #[tokio::test]
    #[ignore]
    async fn inventories_refuse_the_item_types_they_exclude() {
        use sc_api::internal::AdminAddItemRequest;
        let fixture = fixture().await;
        let graph = &fixture.graph;
        let base = fresh();
        // A player of its own, so nothing is left for the tests that count the fixture's.
        let me = 301_000_000_000 + base % 1_000_000;
        let crc = |n: u64| ((base + n) % 0xffff_fff0) as u32 + 1;
        let class = |name: &str, crc: u32, kind: &str, sub: &str| json!({ "_key": format!("{name}{base}"), "guid": format!("{name}{base}"),
            "guidHashCrc": crc, "className": name, "itemType": kind, "itemSubType": sub, "occupancyMicroScu": 1000 });
        let mut classes = vec![
            class("cooler", crc(41), "Cooler", "UNDEFINED"),
            class("pistol", crc(42), "WeaponPersonal", "Medium"),
            class("suit", crc(43), "Char_Armor_Torso", "Heavy"),
        ];
        // The banks refuse coolers and only the heavy suit of armour; a box takes only pistols.
        classes.push(json!({ "_key": "itembankcontainer", "guid": "itembankcontainer", "guidHashCrc": 0, "className": "ItemBankContainer",
            "itemBank": { "capacityMicroScu": 0, "closed": false, "excluded": [{ "itemType": "Cooler", "itemSubTypes": [] }],
                           "only": [] } }));
        classes.push(json!({ "_key": format!("box{base}"), "guid": format!("box{base}"), "guidHashCrc": crc(44), "className": "Box",
            "itemType": "Container", "itemSubType": "Box", "occupancyMicroScu": 1000,
            "inventory": { "capacityMicroScu": 1_000_000, "closed": true, "excluded": [],
                           "only": [{ "itemType": "WeaponPersonal", "itemSubTypes": [] }] } }));
        graph.db.aql("FOR c IN @classes UPSERT { _key: c._key } INSERT c REPLACE c IN entity_classes", json!({ "classes": classes })).await.unwrap();
        let add = |n: u32, inventory: &str, name: &str| AdminAddItemRequest {
            command_id: format!("types:{base}:{n}"),
            owner_id: me,
            inventory_id: inventory.into(),
            entity_class_guid: format!("{name}{base}"),
            stack_size: 1,
            actor: "admin:1:root".into(),
            reason: "test".into(),
        };
        let bank = format!("{me}:Location:{base}");
        let admit = |n: u32, inventory: String, name: &'static str| {
            let graph = graph.clone();
            let request = add(n, &inventory, name);
            async move { EntityGraphInternal::admin_add_item(&*graph, Request::new(request)).await }
        };
        admit(1, bank.clone(), "pistol").await.unwrap();
        admit(2, bank.clone(), "suit").await.unwrap();
        let refused = admit(3, bank.clone(), "cooler").await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused:?}");
        // The box takes only pistols.
        let boxed = admit(4, bank.clone(), "box").await.unwrap().into_inner().geid;
        let inside = doc::container_inventory_id(boxed);
        admit(5, inside.clone(), "pistol").await.unwrap();
        let refused = admit(6, inside, "suit").await.unwrap_err();
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition, "{refused:?}");
    }

    /// `GrantItems`: several items stowed at once, once per command id, nothing for an unknown class or a full inventory.
    #[tokio::test]
    #[ignore]
    async fn grant_items_stows_them_once() {
        use sc_api::internal::{GrantItemsRequest, GrantedItem};
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        graph.get_inventories(signed(&fixture.token, GetInventoriesRequest { body: Some(GetInventoriesRequestBody::default()) })).await.unwrap();
        let base = fresh();
        let guid = format!("grant{base}");
        let class = json!([{ "_key": guid, "guid": guid, "guidHashCrc": ((base + 31) % 0xffff_fff0) as u32 + 1, "className": "Grant",
                             "occupancyMicroScu": 300_000 }]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": class })).await.unwrap();
        let location = format!("{me}:Location:{base}");
        let request = |command: &str, items: &[(&str, u32)]| GrantItemsRequest {
            command_id: command.into(),
            owner_id: me,
            inventory_id: location.clone(),
            items: items.iter().map(|(guid, stack)| GrantedItem { entity_class_guid: guid.to_uppercase(), stack_size: *stack }).collect(),
            reason: "test".into(),
            items_only: false,
        };
        let command = format!("grant:{base}");
        let first = EntityGraphInternal::grant_items(&**graph, Request::new(request(&command, &[(&guid, 0), (&guid, 3)]))).await.unwrap().into_inner();
        assert_eq!((first.geids.len(), first.replayed), (2, false));
        let again = EntityGraphInternal::grant_items(&**graph, Request::new(request(&command, &[(&guid, 0), (&guid, 3)]))).await.unwrap().into_inner();
        assert_eq!((again.geids.clone(), again.replayed), (first.geids.clone(), true), "a repeat grants nothing");
        let held = stow_of(&fixture, first.geids[1]).await;
        assert_eq!((held.owner_id, held.inventory_id.as_deref()), (me, Some(location.as_str())));
        let unknown = EntityGraphInternal::grant_items(&**graph, Request::new(request(&format!("{command}-x"), &[("nothing", 1)]))).await;
        assert_eq!(unknown.unwrap_err().code(), tonic::Code::NotFound);
        // A class that takes no room is skipped when only items are wanted.
        let ship = format!("ship{base}");
        let classes = json!([{ "_key": ship, "guid": ship, "guidHashCrc": ((base + 32) % 0xffff_fff0) as u32 + 1, "className": "Ship" }]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        let mut mixed = request(&format!("{command}-m"), &[(&ship, 1), (&guid, 1)]);
        mixed.items_only = true;
        let made = EntityGraphInternal::grant_items(&**graph, Request::new(mixed)).await.unwrap().into_inner();
        assert_eq!((made.geids[0], made.geids[1] > 0), (0, true), "the ship is skipped, the item stowed");
        // Revoked once, and a missing item is not an error.
        let revoke = |command: &str, geids: Vec<u64>| sc_api::internal::RevokeItemsRequest { command_id: command.into(), owner_id: me, geids, reason: "test".into() };
        let gone = EntityGraphInternal::revoke_items(&**graph, Request::new(revoke(&format!("{command}-r"), first.geids.clone()))).await.unwrap().into_inner();
        assert_eq!((gone.erased, gone.replayed), (2, false));
        let twice = EntityGraphInternal::revoke_items(&**graph, Request::new(revoke(&format!("{command}-r"), first.geids.clone()))).await.unwrap().into_inner();
        assert!(twice.replayed);
        let missing = EntityGraphInternal::revoke_items(&**graph, Request::new(revoke(&format!("{command}-m"), first.geids.clone()))).await.unwrap().into_inner();
        assert_eq!(missing.erased, 0, "already gone");
    }

    /// `EnsureLoadout`: the items of a loadout made once per command id, each attached to its parent's port.
    #[tokio::test]
    #[ignore]
    async fn ensure_loadout_makes_the_items_once() {
        use sc_api::internal::{EnsureLoadoutRequest, LoadoutItem};
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let crc = |n: u64| ((base + n) % 0xffff_fff0) as u32 + 1;
        let (body, head) = (crc(21), crc(22));
        let classes = json!([
            { "_key": format!("body{base}"), "guid": format!("body{base}"), "guidHashCrc": body, "className": "Body" },
            { "_key": format!("head{base}"), "guid": format!("head{base}"), "guidHashCrc": head, "className": "Head" },
        ]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        let items = vec![
            LoadoutItem { class_crc: body, port_crc: 0xab63_41ac, parent: -1 },
            LoadoutItem { class_crc: head, port_crc: 0x1234_5678, parent: 0 },
        ];
        let request = |command: &str, items: Vec<LoadoutItem>| EnsureLoadoutRequest { command_id: command.into(), player_id: me, items };
        let command = format!("loadout:{me}:{base}");
        let first = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request(&command, items.clone()))).await.unwrap().into_inner();
        assert_eq!((first.geids.len(), first.replayed), (2, false));
        assert!(first.geids[0] >= admin::FIRST_ITEM_GEID && first.geids[1] == first.geids[0] + 1);
        let again = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request(&command, items.clone()))).await.unwrap().into_inner();
        assert_eq!((again.geids.clone(), again.replayed), (first.geids.clone(), true), "a repeat makes nothing");
        let query = "FOR e IN entity_edges FILTER e.startId == @geid RETURN { to: e.endId, port: e.attachmentPortCrc }";
        let head_edge = graph.db.aql(query, json!({ "geid": first.geids[1].to_string() })).await.unwrap();
        assert_eq!(head_edge, [json!({ "to": first.geids[0].to_string(), "port": 0x1234_5678 })]);
        let body_edge = graph.db.aql(query, json!({ "geid": first.geids[0].to_string() })).await.unwrap();
        assert_eq!(body_edge[0]["to"], json!(me.to_string()), "a root is attached to the player");
        // Another loadout (another head) takes the place of this one: the old items go, the new ones stay.
        let other = vec![items[0], LoadoutItem { class_crc: body, port_crc: 0x2222_2222, parent: 0 }];
        let second = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request(&format!("{command}-b"), other))).await.unwrap().into_inner();
        let alive = |geids: Vec<u64>| {
            let graph = graph.clone();
            async move {
                graph.db.aql("FOR e IN entities FILTER e._key IN @keys RETURN e._key", json!({ "keys": geids.iter().map(u64::to_string).collect::<Vec<_>>() })).await.unwrap().len()
            }
        };
        assert_eq!(alive(first.geids.clone()).await, 0, "the old loadout is erased");
        assert_eq!(alive(second.geids.clone()).await, 2, "the new one stays");
        let keys = json!({ "keys": second.geids.iter().map(u64::to_string).collect::<Vec<_>>() });
        graph.db.aql("FOR e IN entities FILTER e._key IN @keys REMOVE e IN entities", keys.clone()).await.unwrap();
        graph.db.aql("FOR x IN entity_edges FILTER x.startId IN @keys REMOVE x IN entity_edges", keys).await.unwrap();
        let unknown = vec![LoadoutItem { class_crc: crc(99), port_crc: 1, parent: -1 }];
        let refused = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request(&format!("{command}-x"), unknown))).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::NotFound);
    }

    /// Equipping: an item of the player's goes from an inventory to a port of a worn item and back, once per command id,
    /// only where its type and size fit, and the loadout answers with what is worn.
    #[tokio::test]
    #[ignore]
    async fn equipping_moves_items_between_inventories_and_ports() {
        use sc_api::internal::{
            AdminAddItemRequest, AdminEquipItemRequest, AdminEquipmentRequest, AdminUnequipItemRequest, EnsureLoadoutRequest, LoadoutItem,
        };
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let crc = |n: u64| ((base + n) % 0xffff_fff0) as u32 + 1;
        let (body, suit, pistol, rifle) = (crc(31), crc(32), crc(33), crc(34));
        let guid = |name: &str| format!("{name}{base}");
        let port = |name: &str, min: i64, max: i64, kind: &str, subs: serde_json::Value| {
            json!({ "name": name, "minSize": min, "maxSize": max, "types": [{ "itemType": kind, "itemSubTypes": subs }],
                    "requiredPortTags": "", "portTags": "", "flags": "" })
        };
        let classes = json!([
            { "_key": guid("body"), "guid": guid("body"), "guidHashCrc": body, "className": "Body", "itemType": "Char_Body",
              "ports": [port("Armor_Undersuit", 1, 1, "Char_Armor_Undersuit", json!([])), port("Head_ItemPort", 1, 1, "Char_Head", json!([]))] },
            { "_key": guid("suit"), "guid": guid("suit"), "guidHashCrc": suit, "className": "Suit", "itemType": "Char_Armor_Undersuit",
              "ports": [port("wep_sidearm", 1, 1, "WeaponPersonal", json!(["Small"]))] },
            { "_key": guid("pistol"), "guid": guid("pistol"), "guidHashCrc": pistol, "className": "Pistol", "itemType": "WeaponPersonal",
              "itemSubType": "Small", "attachSize": 1 },
            { "_key": guid("rifle"), "guid": guid("rifle"), "guidHashCrc": rifle, "className": "Rifle", "itemType": "WeaponPersonal",
              "itemSubType": "Medium", "attachSize": 2 },
        ]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        // The player's items of earlier runs would be candidates too.
        let old = json!({ "me": me.to_string() });
        graph.db.aql("FOR x IN entity_edges FILTER x.startId IN (FOR e IN entities FILTER e.ownerId == @me AND e._key != @me RETURN e._key) REMOVE x IN entity_edges", old.clone()).await.unwrap();
        graph.db.aql("FOR e IN entities FILTER e.ownerId == @me AND e._key != @me REMOVE e IN entities", old.clone()).await.unwrap();
        graph.db.aql("UPSERT { _key: @me } INSERT { _key: @me, geid: @me, ownerId: @me } UPDATE {} IN entities", old).await.unwrap();
        let loadout = vec![
            LoadoutItem { class_crc: body, port_crc: 0xab63_41ac, parent: -1 },
            LoadoutItem { class_crc: suit, port_crc: crate::equip::port_crc("Armor_Undersuit"), parent: 0 },
        ];
        let command = format!("loadout:{me}:{base}");
        let request = EnsureLoadoutRequest { command_id: command.clone(), player_id: me, items: loadout };
        let made = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request.clone())).await.unwrap().into_inner();
        let (body_geid, suit_geid) = (made.geids[0], made.geids[1]);
        assert!(made.equipped.is_empty());
        let personal = format!("{me}:PlayerInventory:0");
        let add = |n: u32, class: &str| AdminAddItemRequest {
            command_id: format!("admin:equip-{base}-{n}"),
            owner_id: me,
            inventory_id: personal.clone(),
            entity_class_guid: class.into(),
            stack_size: 0,
            actor: "admin:1:root".into(),
            reason: "test".into(),
        };
        let gun = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(1, &guid("pistol")))).await.unwrap().into_inner().geid;
        let spare = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(2, &guid("pistol")))).await.unwrap().into_inner().geid;
        let long = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(3, &guid("rifle")))).await.unwrap().into_inner().geid;

        // The worn items and the ports that take something: the undersuit's sidearm port fits the pistols, not the rifle;
        // the head is not equipment.
        let seen = EntityGraphInternal::admin_equipment(&**graph, Request::new(AdminEquipmentRequest { owner_id: me })).await.unwrap().into_inner();
        assert_eq!(seen.worn.iter().map(|item| (item.geid, item.port.as_str())).collect::<Vec<_>>(), [(body_geid, "0xab6341ac"), (suit_geid, "Armor_Undersuit")]);
        let sidearm = seen.ports.iter().find(|port| port.port == "wep_sidearm").expect("the sidearm port");
        assert_eq!((sidearm.parent_geid, sidearm.occupant_geid), (suit_geid, 0));
        let mut fit = sidearm.candidates.clone();
        fit.sort_unstable();
        assert_eq!(fit, [gun, spare]);
        assert!(seen.ports.iter().all(|port| port.port != "Head_ItemPort"));

        let equip = |n: u32, geid: u64, parent: u64, port: &str| AdminEquipItemRequest {
            command_id: format!("admin:equip-{base}-e{n}"),
            owner_id: me,
            geid,
            parent_geid: parent,
            port: port.into(),
            actor: "admin:1:root".into(),
            reason: "test".into(),
        };
        fn refused<T>(result: Result<tonic::Response<T>, tonic::Status>) -> tonic::Code {
            result.map(|_| ()).unwrap_err().code()
        }
        assert_eq!(refused(EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(1, long, suit_geid, "wep_sidearm"))).await), tonic::Code::FailedPrecondition, "a rifle is not a sidearm");
        assert_eq!(refused(EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(2, gun, suit_geid, "nowhere"))).await), tonic::Code::FailedPrecondition);
        assert_eq!(refused(EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(3, gun, gun, "wep_sidearm"))).await), tonic::Code::FailedPrecondition, "the parent must be worn");
        let done = EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(4, gun, suit_geid, "WEP_SIDEARM"))).await.unwrap().into_inner();
        assert!(!done.replayed);
        let again = EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(4, gun, suit_geid, "WEP_SIDEARM"))).await.unwrap().into_inner();
        assert!(again.replayed, "a repeated command equips nothing twice");
        assert_eq!(refused(EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(5, spare, suit_geid, "wep_sidearm"))).await), tonic::Code::FailedPrecondition, "the port is taken");
        let row = graph.db.aql("FOR e IN entities FILTER e._key == @k RETURN e", json!({ "k": gun.to_string() })).await.unwrap().remove(0);
        assert_eq!((row["stowed"].as_bool(), row["inventoryId"].is_null()), (Some(false), true));
        let edges = graph.db.aql("FOR x IN entity_edges FILTER x.startId == @k RETURN { type: x.type, to: x.endId, port: x.attachmentPortCrc }", json!({ "k": gun.to_string() })).await.unwrap();
        assert_eq!(edges, [json!({ "type": 1, "to": suit_geid.to_string(), "port": crate::equip::port_crc("wep_sidearm") })], "attached, no longer stowed");
        assert_eq!(inventory_doc(&fixture, &personal).await.unwrap()["configuration"], serde_json::Value::Null);

        // The loadout answers with it, and with what was taken off.
        let on_join = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request.clone())).await.unwrap().into_inner();
        assert_eq!((on_join.replayed, on_join.geids.clone()), (true, made.geids.clone()));
        assert_eq!(on_join.equipped.iter().map(|item| (item.geid, item.class_crc, item.parent_geid, item.port_crc)).collect::<Vec<_>>(),
                   [(gun, pistol, suit_geid, crate::equip::port_crc("wep_sidearm"))]);
        let seen = EntityGraphInternal::admin_equipment(&**graph, Request::new(AdminEquipmentRequest { owner_id: me })).await.unwrap().into_inner();
        let sidearm = seen.ports.iter().find(|port| port.port == "wep_sidearm").unwrap();
        assert_eq!((sidearm.occupant_geid, sidearm.occupant_name.as_str()), (gun, "Pistol"));

        let off = |n: u32, geid: u64, to: &str| AdminUnequipItemRequest {
            command_id: format!("admin:equip-{base}-u{n}"),
            owner_id: me,
            geid,
            to_inventory_id: to.into(),
            actor: "admin:1:root".into(),
            reason: "test".into(),
        };
        let elsewhere = format!("{me}:Location:{base}");
        assert_eq!(refused(EntityGraphInternal::admin_unequip_item(&**graph, Request::new(off(1, body_geid, &personal))).await), tonic::Code::FailedPrecondition, "the body is the character");
        assert_eq!(refused(EntityGraphInternal::admin_unequip_item(&**graph, Request::new(off(2, spare, &personal))).await), tonic::Code::FailedPrecondition, "not worn");
        let taken = EntityGraphInternal::admin_unequip_item(&**graph, Request::new(off(3, gun, &elsewhere))).await.unwrap().into_inner();
        assert!(!taken.replayed);
        assert!(EntityGraphInternal::admin_unequip_item(&**graph, Request::new(off(3, gun, &elsewhere))).await.unwrap().into_inner().replayed);
        let row = graph.db.aql("FOR e IN entities FILTER e._key == @k RETURN e", json!({ "k": gun.to_string() })).await.unwrap().remove(0);
        assert_eq!((row["stowed"].as_bool(), row["inventoryId"].as_str()), (Some(true), Some(elsewhere.as_str())), "stowed in the bank, made on first need");
        let on_join = EntityGraphInternal::ensure_loadout(&**graph, Request::new(request.clone())).await.unwrap().into_inner();
        assert!(on_join.equipped.is_empty());

        // Equipped on the loadout's items, it survives a changed loadout: it is stowed in the personal inventory.
        EntityGraphInternal::admin_equip_item(&**graph, Request::new(equip(6, gun, suit_geid, "wep_sidearm"))).await.unwrap();
        let other = EnsureLoadoutRequest {
            command_id: format!("{command}-b"),
            player_id: me,
            items: vec![LoadoutItem { class_crc: body, port_crc: 0xab63_41ac, parent: -1 }],
        };
        let second = EntityGraphInternal::ensure_loadout(&**graph, Request::new(other)).await.unwrap().into_inner();
        assert_eq!(second.geids.len(), 1);
        let row = graph.db.aql("FOR e IN entities FILTER e._key IN @k RETURN { stowed: e.stowed, inv: e.inventoryId }", json!({ "k": [gun.to_string(), suit_geid.to_string()] })).await.unwrap();
        assert_eq!(row, [json!({ "stowed": true, "inv": personal })], "the old undersuit is gone, the pistol is in the personal inventory");
    }

    /// Location, ship and container inventories: created when first needed, sized by class, nested, guarded
    /// against loops, over-filling and other players.
    #[tokio::test]
    #[ignore]
    async fn inventories_of_every_kind() {
        use sc_proto::services::entitygraph::v1::{move_all_command, set_stack_command, MoveAllCommand, SetStackCommand};
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        // Classes: a ship with a 1 SCU hold, a crate of 0.5 SCU holding 0.9 SCU, a box of 0.1 SCU holding 0.2 SCU,
        // and an item of 0.3 SCU. CRCs from the run's ids, so runs do not meet.
        let crc = |n: u64| ((base + n) % 0xffff_fff0) as u32 + 1;
        let (ship_class, crate_class, box_class, item_class) = (crc(1), crc(2), crc(3), crc(4));
        let classes = json!([
            { "_key": format!("ship{base}"), "guidHashCrc": ship_class, "inventory": { "capacityMicroScu": 1_000_000, "closed": true } },
            { "_key": format!("crate{base}"), "guidHashCrc": crate_class, "occupancyMicroScu": 500_000,
              "inventory": { "capacityMicroScu": 900_000, "closed": true } },
            { "_key": format!("box{base}"), "guidHashCrc": box_class, "occupancyMicroScu": 100_000,
              "inventory": { "capacityMicroScu": 200_000, "closed": false } },
            { "_key": format!("item{base}"), "guidHashCrc": item_class, "occupancyMicroScu": 300_000 },
        ]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        let (ship, crate_box, small_box, item, other_item) = (base, base + 1, base + 2, base + 3, base + 4);
        let (hold, crate_inv, box_inv) =
            (doc::container_inventory_id(ship), doc::container_inventory_id(crate_box), doc::container_inventory_id(small_box));
        let location = format!("{me}:Location:{base}");

        // A ship: its hold comes with it, sized by its class. The pistol-like item of no container class gets none.
        let seed = EntityGraph { nodes: vec![classed(ship, me, ship_class, 1)], edges: Vec::new() };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create(seed)]))).await.unwrap();
        let hold_doc = inventory_doc(&fixture, &hold).await.expect("the ship's hold");
        assert_eq!(hold_doc["inventoryType"], json!(InventoryType::Physical as i32));
        assert_eq!(hold_doc["configuration"]["physical"]["capacity"], json!(1_000_000));
        assert_eq!(hold_doc["ownerId"], json!("0"), "a container is owned through its holder");

        // A crate created straight into the hold, with a box and an item stowed in the crate.
        let crate_graph = EntityGraph { nodes: vec![classed(crate_box, me, crate_class, 1)], edges: Vec::new() };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create_into(crate_graph, &hold)]))).await.unwrap();
        assert_eq!(stow_of(&fixture, crate_box).await.inventory_id.as_deref(), Some(hold.as_str()));
        assert_eq!(occupancy(&fixture, &hold).await, 500_000);
        let contents = EntityGraph {
            nodes: vec![classed(small_box, me, box_class, 1), classed(item, me, item_class, 1)],
            edges: Vec::new(),
        };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create_into(contents, &crate_inv)]))).await.unwrap();
        assert_eq!(occupancy(&fixture, &crate_inv).await, 400_000);
        assert_eq!(occupancy(&fixture, &hold).await, 500_000, "a crate takes its own room, whatever it holds");

        // Over capacity: a stack of three items (0.9 SCU) does not fit next to the box. Nothing of it stays.
        let stack = Params::Setstack(SetStackCommand {
            changes: vec![set_stack_command::Body { entity_id: item, amount: 3 }],
            inventory_id: String::new(),
        });
        let full = graph.mutate_entities(signed(&fixture.token, mutate(vec![stack]))).await;
        assert_eq!(full.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert_eq!(occupancy(&fixture, &crate_inv).await, 400_000);

        // Loops: the crate into its own inventory, or into the box inside it.
        for target in [&crate_inv, &box_inv] {
            let refused = graph.r#move(signed(&fixture.token, move_request(crate_box, &hold, target))).await;
            let status = refused.unwrap_err();
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert!(status.message().contains("inside itself"), "{}", status.message());
        }
        let all_into_box = Params::MoveAll(MoveAllCommand {
            moves: vec![move_all_command::Body { from_inventory_id: hold.clone(), to_inventory_id: box_inv.clone(), ..Default::default() }],
        });
        let refused = graph.mutate_entities(signed(&fixture.token, mutate(vec![all_into_box]))).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::FailedPrecondition);

        // Into the box, which nests inside the crate; the box inventory existed from the box's creation.
        graph.r#move(signed(&fixture.token, move_request(item, &crate_inv, &box_inv))).await.unwrap_err();
        assert_eq!(stow_of(&fixture, item).await.inventory_id.as_deref(), Some(crate_inv.as_str()), "0.3 SCU is more than the box holds");

        // A location inventory is made on first use, owned by the player and held by the player entity.
        graph.r#move(signed(&fixture.token, move_request(item, &crate_inv, &location))).await.unwrap();
        let bank = inventory_doc(&fixture, &location).await.expect("the location inventory");
        assert_eq!((bank["ownerId"].clone(), bank["context"].clone(), bank["subjectId"].clone()), (json!(me.to_string()), json!("Location"), json!(base.to_string())));
        assert_eq!(bank["inventoryType"], json!(InventoryType::Infinite as i32));
        assert_eq!(occupancy(&fixture, &crate_inv).await, 100_000);

        // Another player can neither use my location id, nor put into nor take from my containers.
        let theirs = EntityGraph { nodes: vec![classed(other_item, 0, item_class, 1)], edges: vec![stowed(other_item, &location, 0)] };
        let refused = graph.mutate_entities(signed(&fixture.other_token, mutate(vec![create(theirs)]))).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let not_mine = format!("{}:Location:{base}", me + 1_000_000);
        let refused = graph.r#move(signed(&fixture.token, move_request(item, &location, &not_mine))).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(inventory_doc(&fixture, &not_mine).await.is_none(), "a refused move creates nothing");
        let unowned = EntityGraph { nodes: vec![classed(other_item, 0, item_class, 1)], edges: Vec::new() };
        graph.mutate_entities(signed(&fixture.other_token, mutate(vec![create(unowned)]))).await.unwrap();
        let other = graph.db.aql("FOR e IN entities FILTER e._key == @k RETURN e", json!({ "k": other_item.to_string() })).await.unwrap();
        assert_eq!(other.len(), 1);
        let into_mine = graph.mutate_entities(signed(&fixture.other_token, mutate(vec![Params::Move(MoveCommand {
            moves: vec![move_command::Body { entity_id: other_item, to_inventory_id: box_inv.clone(), ..Default::default() }],
        })]))).await;
        assert_eq!(into_mine.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let take = graph.r#move(signed(&fixture.other_token, move_request(small_box, &crate_inv, &hold))).await;
        assert_eq!(take.unwrap_err().code(), tonic::Code::FailedPrecondition);

        // The hierarchy: from the ship down, every nested inventory; and all of them among the player's.
        let tree = InventoryTreeFilter { root: Some(inventory_tree_filter::Root::EntityGeid(ship)), ..Default::default() };
        let query = InventoryQuery { find_by: Some(inventory_query::FindBy::Tree(tree)), ..Default::default() };
        let request = InventoryQueryRequest { body: Some(InventoryQueryRequestBody { query: Some(query) }) };
        let reply = graph.inventory_query(signed(&fixture.token, request)).await.unwrap().into_inner().body.unwrap();
        let mut found: Vec<String> = reply.results.iter().map(|inventory| inventory.properties.as_ref().unwrap().id.clone()).collect();
        found.sort();
        let mut nested = vec![hold.clone(), crate_inv.clone(), box_inv.clone()];
        nested.sort();
        assert_eq!(found, nested);
        let request = signed(&fixture.token, GetInventoriesRequest { body: Some(GetInventoriesRequestBody::default()) });
        let reply = graph.get_inventories(request).await.unwrap().into_inner().body.unwrap();
        let ids: Vec<String> = reply.inventories.iter().filter_map(|node| match node.properties.as_ref()?.properties.as_ref()? {
            node_properties::Properties::InventoryProperties(inventory) => Some(inventory.id.clone()),
            _ => None,
        }).collect();
        for id in nested.iter().chain([&location]) {
            assert!(ids.contains(id), "{id} missing from {ids:?}");
        }
        let body = GetInventoriesRequestBody { owner_id: me.to_string() };
        let reply = graph.get_inventories(signed(&fixture.other_token, GetInventoriesRequest { body: Some(body) })).await.unwrap();
        assert!(reply.into_inner().body.unwrap().inventories.is_empty(), "nothing of mine is shared");

        // Erasing the ship takes its hold and everything nested in it.
        let erase = Params::Erase(EraseCommand {
            erasures: vec![erase_command::Body { entity_id: ship, preconditions: Vec::new() }],
            inventory_id: String::new(),
        });
        graph.mutate_entities(signed(&fixture.token, mutate(vec![erase]))).await.unwrap();
        for id in &nested {
            assert!(inventory_doc(&fixture, id).await.is_none(), "{id} survived");
        }
        let left = "FOR e IN entities FILTER e._key IN @keys RETURN e._key";
        let gone = [crate_box.to_string(), small_box.to_string()];
        assert!(graph.db.aql(left, json!({ "keys": gone })).await.unwrap().is_empty());
        graph.db.aql("FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs REMOVE c IN entity_classes",
            json!({ "crcs": [ship_class, crate_class, box_class, item_class] })).await.unwrap();
    }

    /// The admin RPCs, acting for a player: add by class, split and move, remove, the tree, class search, applied
    /// once per command id, and refused for what is live on a shard or not the player's.
    #[tokio::test]
    #[ignore]
    async fn admin_inventory_rpcs() {
        use sc_api::internal::{
            AdminAddItemRequest, AdminInventoryTreeRequest, AdminMoveItemRequest, AdminRemoveItemRequest,
            AdminSearchEntityClassesRequest, ItemTypeFilter,
        };
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        // The player and its personal inventory, as on its first call.
        graph.get_inventories(signed(&fixture.token, GetInventoriesRequest { body: Some(GetInventoriesRequestBody::default()) })).await.unwrap();
        let base = fresh();
        let crc = |n: u64| ((base + n) % 0xffff_fff0) as u32 + 1;
        let (ship_class, crate_class, item_class) = (crc(11), crc(12), crc(13));
        let (ship_guid, crate_guid, item_guid) = (format!("ship{base}"), format!("crate{base}"), format!("item{base}"));
        let classes = json!([
            { "_key": ship_guid, "guid": ship_guid, "guidHashCrc": ship_class, "className": format!("SHIP_{base}"),
              "inventory": { "capacityMicroScu": 1_000_000, "closed": true } },
            { "_key": crate_guid, "guid": crate_guid, "guidHashCrc": crate_class, "className": format!("Crate_{base}"),
              "name": { "en": "Test crate" }, "itemType": "Container", "itemSubType": "Cargo", "occupancyMicroScu": 500_000,
              "inventory": { "capacityMicroScu": 900_000, "closed": true } },
            { "_key": item_guid, "guid": item_guid, "guidHashCrc": item_class, "className": format!("Item_{base}"),
              "occupancyMicroScu": 300_000 },
        ]);
        graph.db.aql("FOR c IN @classes INSERT c INTO entity_classes", json!({ "classes": classes })).await.unwrap();
        let location = format!("{me}:Location:{base}");
        let command = |n: u32| format!("admin:test-{base}-{n}");
        let add = |n: u32, inventory: &str, guid: &str, stack: u32| AdminAddItemRequest {
            command_id: command(n),
            owner_id: me,
            inventory_id: inventory.into(),
            entity_class_guid: guid.to_uppercase(),
            stack_size: stack,
            actor: "admin:1:root".into(),
            reason: "test".into(),
        };

        // A crate into a location bank that does not exist yet: both made, the crate with its inventory.
        let made = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(1, &location, &crate_guid, 0))).await.unwrap().into_inner();
        assert!(made.geid >= admin::FIRST_ITEM_GEID && !made.replayed);
        let again = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(1, &location, &crate_guid, 0))).await.unwrap().into_inner();
        assert_eq!((again.geid, again.replayed), (made.geid, true), "a repeated command adds nothing");
        let crate_box = made.geid;
        let crate_inv = doc::container_inventory_id(crate_box);
        assert_eq!(inventory_doc(&fixture, &crate_inv).await.expect("the crate's inventory")["configuration"]["physical"]["capacity"], json!(900_000));
        let crate_doc = stow_of(&fixture, crate_box).await;
        assert_eq!((crate_doc.owner_id, crate_doc.inventory_id.as_deref()), (me, Some(location.as_str())));

        // Two items fill the crate to 0.6 SCU; two more would overfill it, and nothing of them stays.
        let items = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(2, &crate_inv, &item_guid, 2))).await.unwrap().into_inner();
        assert_eq!(occupancy(&fixture, &crate_inv).await, 600_000);
        let full = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(3, &crate_inv, &item_guid, 2))).await.unwrap_err();
        assert_eq!(full.code(), tonic::Code::FailedPrecondition, "{}", full.message());
        assert!(full.message().contains("inventory is full"), "{}", full.message());
        let unknown = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(4, &crate_inv, "nothing-seeded", 1))).await.unwrap_err();
        assert_eq!(unknown.code(), tonic::Code::NotFound);

        // A split: one of the two moves to the bank as a new item.
        let split = AdminMoveItemRequest {
            command_id: command(5),
            owner_id: me,
            geid: items.geid,
            from_inventory_id: crate_inv.clone(),
            to_inventory_id: location.clone(),
            quantity: 1,
            ..Default::default()
        };
        let moved = EntityGraphInternal::admin_move_item(&**graph, Request::new(split.clone())).await.unwrap().into_inner();
        assert_ne!(moved.moved_geid, items.geid);
        let replay = EntityGraphInternal::admin_move_item(&**graph, Request::new(split)).await.unwrap().into_inner();
        assert_eq!((replay.moved_geid, replay.replayed), (moved.moved_geid, true));
        assert_eq!(occupancy(&fixture, &crate_inv).await, 300_000);
        let stack = |geid: u64| graph.db.aql("FOR e IN entities FILTER e._key == @k RETURN e.stackSize", json!({ "k": geid.to_string() }));
        assert_eq!(stack(items.geid).await.unwrap(), [json!(1)]);
        assert_eq!(stack(moved.moved_geid).await.unwrap(), [json!(1)]);
        assert_eq!(stow_of(&fixture, moved.moved_geid).await.inventory_id.as_deref(), Some(location.as_str()));

        // Refused: the crate into itself, from the wrong inventory, into another player's bank.
        let whole = |n: u32, geid: u64, from: &str, to: &str| AdminMoveItemRequest {
            command_id: command(n),
            owner_id: me,
            geid,
            from_inventory_id: from.into(),
            to_inventory_id: to.into(),
            ..Default::default()
        };
        let looped = EntityGraphInternal::admin_move_item(&**graph, Request::new(whole(6, crate_box, &location, &crate_inv))).await.unwrap_err();
        assert!(looped.message().contains("inside itself"), "{}", looped.message());
        let wrong = EntityGraphInternal::admin_move_item(&**graph, Request::new(whole(7, items.geid, &location, &crate_inv))).await.unwrap_err();
        assert_eq!(wrong.code(), tonic::Code::FailedPrecondition);
        let theirs = format!("{}:Location:{base}", me + 1_000_000);
        let other = EntityGraphInternal::admin_move_item(&**graph, Request::new(whole(8, items.geid, &crate_inv, &theirs))).await.unwrap_err();
        assert!(other.message().contains("another owner"), "{}", other.message());
        assert!(inventory_doc(&fixture, &theirs).await.is_none());
        // The whole stack back into the crate.
        EntityGraphInternal::admin_move_item(&**graph, Request::new(whole(9, moved.moved_geid, &location, &crate_inv))).await.unwrap();
        assert_eq!(occupancy(&fixture, &crate_inv).await, 600_000);

        // A ship out on a shard: its hold is listed as live, and nothing goes into it or comes out of it.
        let ship = base;
        let hold = doc::container_inventory_id(ship);
        let arguments = MutationArguments {
            id: command(10),
            scope: Some(Scope { r#type: ScopeType::Shard as i32, shard_id: "shard-1".into() }),
            ..Default::default()
        };
        let spawned = EntityGraph { nodes: vec![classed(ship, me, ship_class, 1)], edges: Vec::new() };
        let commands = vec![EntityGraphCommand { params: Some(create(spawned)) }];
        let request = MutateEntitiesRequest { body: Some(MutateEntitiesRequestBody { arguments: Some(arguments), commands }) };
        graph.mutate_entities(signed(&fixture.token, request)).await.unwrap();
        let live = EntityGraphInternal::admin_add_item(&**graph, Request::new(add(11, &hold, &item_guid, 1))).await.unwrap_err();
        assert!(live.message().contains("live on a shard"), "{}", live.message());
        let live = EntityGraphInternal::admin_move_item(&**graph, Request::new(whole(12, items.geid, &crate_inv, &hold))).await.unwrap_err();
        assert!(live.message().contains("live on a shard"), "{}", live.message());
        let remove = |n: u32, geid: u64| AdminRemoveItemRequest { command_id: command(n), owner_id: me, geid, ..Default::default() };
        let unstowed = EntityGraphInternal::admin_remove_item(&**graph, Request::new(remove(13, ship))).await.unwrap_err();
        assert_eq!(unstowed.code(), tonic::Code::FailedPrecondition);
        let player = EntityGraphInternal::admin_remove_item(&**graph, Request::new(remove(14, me))).await.unwrap_err();
        assert_eq!(player.code(), tonic::Code::InvalidArgument);

        // The tree: personal first, the bank, the crate under it with its items and their classes, the live hold.
        let request = AdminInventoryTreeRequest { owner_id: me, ..Default::default() };
        let tree = EntityGraphInternal::admin_inventory_tree(&**graph, Request::new(request)).await.unwrap().into_inner().inventories;
        let find = |id: &str| tree.iter().find(|inventory| inventory.inventory.as_ref().unwrap().id == id).unwrap_or_else(|| panic!("{id} missing"));
        assert_eq!(tree[0].inventory.as_ref().unwrap().id, doc::player_inventory_id(me));
        let crate_node = find(&crate_inv);
        assert_eq!((crate_node.parent_inventory_id.as_str(), crate_node.depth, crate_node.item_count), (location.as_str(), 1, 2));
        assert_eq!(crate_node.holder_class_name, format!("Crate_{base}"));
        assert!(crate_node.items.iter().all(|item| item.class_name == format!("Item_{base}") && item.inventory_id == crate_inv));
        let bank = find(&location);
        assert_eq!(bank.items[0].held_inventory_ids, std::slice::from_ref(&crate_inv));
        assert_eq!(bank.items[0].display_name, "Test crate");
        assert!(find(&hold).live && !bank.live);
        let position = |id: &str| tree.iter().position(|inventory| inventory.inventory.as_ref().unwrap().id == id).unwrap();
        assert!(position(&location) < position(&crate_inv), "parents before children");
        let shallow = AdminInventoryTreeRequest { owner_id: me, max_depth: 0, max_items: 1 };
        let shallow = EntityGraphInternal::admin_inventory_tree(&**graph, Request::new(shallow)).await.unwrap().into_inner().inventories;
        let crate_node = shallow.iter().find(|inventory| inventory.inventory.as_ref().unwrap().id == crate_inv).unwrap();
        assert_eq!((crate_node.items.len(), crate_node.item_count), (1, 2));

        // Classes by part of a name, by English name, or by GUID.
        let search = |query: &str| AdminSearchEntityClassesRequest { query: query.into(), ..Default::default() };
        for query in [format!("rate_{base}"), "test CRATE".to_string(), crate_guid.to_uppercase()] {
            let found = EntityGraphInternal::admin_search_entity_classes(&**graph, Request::new(search(&query))).await.unwrap().into_inner().classes;
            assert!(found.iter().any(|class| class.guid == crate_guid && class.container && class.capacity_micro_scu == 900_000), "{query}");
        }
        let literal = EntityGraphInternal::admin_search_entity_classes(&**graph, Request::new(search(&format!("crate%{base}")))).await.unwrap();
        assert!(literal.into_inner().classes.is_empty(), "a % is taken literally");
        // By item type and sub type, and only classes with an English name.
        let typed = |any_of: Vec<ItemTypeFilter>, none_of: Vec<ItemTypeFilter>, subs: &[&str], named_only: bool| AdminSearchEntityClassesRequest {
            query: format!("rate_{base}"),
            any_of,
            none_of,
            item_sub_types: subs.iter().map(|name| name.to_string()).collect(),
            named_only,
            ..Default::default()
        };
        let filter = |item_type: &str, subs: &[&str]| ItemTypeFilter { item_type: item_type.into(), item_sub_types: subs.iter().map(|name| name.to_string()).collect() };
        let find = |request: AdminSearchEntityClassesRequest| async {
            EntityGraphInternal::admin_search_entity_classes(&**graph, Request::new(request)).await.unwrap().into_inner().classes
        };
        let found = find(typed(vec![], vec![], &[], true)).await;
        let found_class = found.iter().find(|class| class.guid == crate_guid).expect("it has an English name");
        let (item_type, sub_type) = (found_class.item_type.clone(), found_class.item_sub_type.clone());
        assert!(find(typed(vec![filter("NoSuchItemType", &[])], vec![], &[], false)).await.is_empty(), "another item type");
        assert!(find(typed(vec![], vec![], &["NoSuchSubType"], false)).await.is_empty(), "another sub type");
        assert!(find(typed(vec![filter(&item_type, &["NoSuchSubType"])], vec![], &[], false)).await.is_empty(), "its type, another sub type");
        // The fixture class may carry no sub type, which no sub type filter matches.
        let own_subs: Vec<&str> = if sub_type.is_empty() { vec![] } else { vec![sub_type.as_str()] };
        assert!(!find(typed(vec![filter(&item_type, &own_subs)], vec![], &[], false)).await.is_empty(), "its type and sub type");
        assert!(!find(typed(vec![filter(&item_type, &[])], vec![], &[], false)).await.is_empty(), "its type, any sub type");
        assert!(find(typed(vec![], vec![filter(&item_type, &[])], &[], false)).await.is_empty(), "not its type");
        assert!(!find(typed(vec![], vec![filter(&item_type, &["NoSuchSubType"])], &[], false)).await.is_empty(), "not another sub type of it");
        // Removing the crate erases it and its two items, once.
        let removed = EntityGraphInternal::admin_remove_item(&**graph, Request::new(remove(15, crate_box))).await.unwrap().into_inner();
        assert_eq!((removed.removed, removed.erased, removed.replayed), (true, 3, false));
        let again = EntityGraphInternal::admin_remove_item(&**graph, Request::new(remove(15, crate_box))).await.unwrap().into_inner();
        assert_eq!((again.erased, again.replayed), (3, true));
        assert!(inventory_doc(&fixture, &crate_inv).await.is_none());
        let left = "FOR e IN entities FILTER e._key IN @keys RETURN e._key";
        let gone = [crate_box.to_string(), items.geid.to_string(), moved.moved_geid.to_string()];
        assert!(graph.db.aql(left, json!({ "keys": gone })).await.unwrap().is_empty());
        let gone = EntityGraphInternal::admin_remove_item(&**graph, Request::new(remove(16, crate_box))).await.unwrap_err();
        assert_eq!(gone.code(), tonic::Code::NotFound);

        // The ship and its hold would show among the next tests' inventories, which share the player's GEID.
        let erase = Params::Erase(EraseCommand {
            erasures: vec![erase_command::Body { entity_id: ship, preconditions: Vec::new() }],
            inventory_id: String::new(),
        });
        graph.mutate_entities(signed(&fixture.token, mutate(vec![erase]))).await.unwrap();
        assert!(inventory_doc(&fixture, &hold).await.is_none());
        graph.db.aql("FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs REMOVE c IN entity_classes",
            json!({ "crcs": [ship_class, crate_class, item_class] })).await.unwrap();
    }

    /// Every command must at least be accepted by ArangoDB, whose AQL is only parsed at run time.
    #[tokio::test]
    #[ignore]
    async fn every_command_reaches_the_database() {
        use sc_proto::services::{
            entitygraph::v1::{
                activate_command, add_labels_command, add_tags_command, attach_command, change_flags_command,
                change_inventory_configuration_command, change_location_command, change_rank_command,
                create_inventory_command, create_lt_record_command, create_stow_contexts_command,
                deactivate_command, detach_command, erase_inventory_command, move_all_command,
                remove_labels_command, remove_snapshot_command, remove_tags_command, set_stack_command,
                set_tags_command, stack_command, sync_command, transfer_command, unstack_command,
                update_parent_urns_command, ActivateCommand, AddLabelsCommand, AddTagsCommand, AttachCommand,
                AttachmentOwnershipBehavior, ChangeFlagsCommand, ChangeInventoryConfigurationCommand,
                ChangeLocationCommand, ChangeRankCommand, CreateInventoryCommand, CreateLtRecordCommand,
                CreateStowContextsCommand, DeactivateCommand, DeleteLtRecordCommand, DetachCommand,
                EntityStowContext, EraseInventoryCommand, MoveAllCommand, RemoveLabelsCommand,
                RemoveSnapshotCommand, RemoveTagsCommand, SetStackCommand, SetTagsCommand, StackCommand,
                SyncCommand, SyncCommandEntityProperties, TransferCommand, UnstackCommand, UpdateParentUrnsCommand,
            },
            longterm::v1::OwnershipRecord,
        };
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let (parent, child, ghost) = (base, base + 1, base + 2);
        let (source, target, spare) =
            (format!("{me}:Container:{base}"), format!("{me}:Container:{}", base + 1), format!("{me}:Container:{}", base + 2));
        let seed = EntityGraph {
            nodes: vec![entity_node(parent, me), entity_node(child, me), inventory_node(&source, me, 5), inventory_node(&target, me, 5)],
            edges: vec![stowed(child, &source, 1)],
        };
        let inventory = |id: &str| InventoryNodeProperties { id: id.into(), owner_id: me, ..Default::default() };
        let urn = format!("urn:lt:{base}");
        let params = vec![
            create(seed),
            Params::Attach(AttachCommand {
                attachments: vec![attach_command::Body {
                    entity_id: parent,
                    parent_id: me,
                    ownership: AttachmentOwnershipBehavior::Inherit as i32,
                    ..Default::default()
                }],
                inventory_id: String::new(),
            }),
            Params::Detach(DetachCommand {
                detaches: vec![detach_command::Body { entity_id: parent, ownership: 0 }],
                inventory_id: String::new(),
            }),
            Params::AddLabels(AddLabelsCommand {
                updates: vec![add_labels_command::Body { entity_id: child, labels: vec![1, 2] }],
                inventory_id: String::new(),
            }),
            Params::RemoveLabels(RemoveLabelsCommand {
                updates: vec![remove_labels_command::Body { entity_id: child, labels: vec![1] }],
                inventory_id: String::new(),
            }),
            Params::ChangeFlags(ChangeFlagsCommand {
                changes: vec![change_flags_command::Body { entity_id: child, flags: 6, game_flags: u64::MAX }],
                inventory_id: String::new(),
            }),
            Params::ChangeLocation(ChangeLocationCommand {
                changes: vec![change_location_command::Body { entity_id: child, location_id: 3, scale: 2.0, ..Default::default() }],
                inventory_id: String::new(),
            }),
            Params::Stack(StackCommand { stacks: vec![stack_command::Body { entity_id: child, amount: 5 }], inventory_id: String::new() }),
            Params::Unstack(UnstackCommand { unstacks: vec![unstack_command::Body { entity_id: child, amount: 2 }], inventory_id: String::new() }),
            Params::Setstack(SetStackCommand { changes: vec![set_stack_command::Body { entity_id: parent, amount: 9 }], inventory_id: String::new() }),
            Params::ChangeRank(ChangeRankCommand {
                changes: vec![change_rank_command::Body { entity_id: child, inventory_id: source.clone(), rank: "b".into() }],
            }),
            Params::Activate(ActivateCommand { activations: vec![activate_command::Body { geids: vec![child], ..Default::default() }] }),
            Params::Deactivate(DeactivateCommand { deactivations: vec![deactivate_command::Body { geids: vec![parent], ..Default::default() }] }),
            Params::AddTags(AddTagsCommand { updates: vec![add_tags_command::Body { entity_id: child, tags: vec![1, 2] }], inventory_id: String::new() }),
            Params::RemoveTags(RemoveTagsCommand { updates: vec![remove_tags_command::Body { entity_id: child, tags: vec![1] }], inventory_id: String::new() }),
            Params::SetTags(SetTagsCommand { updates: vec![set_tags_command::Body { entity_id: parent, tags: vec![7] }], inventory_id: String::new() }),
            Params::CreateInventory(CreateInventoryCommand {
                creates: vec![create_inventory_command::Body { player_id: me, properties: Some(inventory(&spare)), reference: 0 }],
                ignore_already_exists_error: true,
            }),
            Params::ChangeInventoryConfig(ChangeInventoryConfigurationCommand {
                change_configs: vec![change_inventory_configuration_command::Body {
                    id: target.clone(),
                    config: Some(InventoryConfiguration {
                        configuration: Some(inventory_configuration::Configuration::Physical(PhysicalInventoryConfiguration { capacity: 8, occupancy: 0 })),
                    }),
                }],
            }),
            Params::Sync(SyncCommand {
                sync_tasks: vec![sync_command::Body {
                    version: 2,
                    entity_id: child,
                    snapshot: Some(EntitySnapshot { entity_id: child, variables: Vec::new(), version: 2 }),
                    attachment_edge: None,
                    dirty_flags: 0,
                    entity_properties: Some(SyncCommandEntityProperties { scale: 1.5, tags: vec![4], ..Default::default() }),
                }],
            }),
            Params::RemoveSnapshot(RemoveSnapshotCommand { removals: vec![remove_snapshot_command::Body { entity_id: child }], inventory_id: String::new() }),
            Params::CreateLtRecord(CreateLtRecordCommand {
                creates: vec![create_lt_record_command::Body {
                    record: Some(OwnershipRecord { urn: urn.clone(), owner_id: me, ..Default::default() }),
                }],
            }),
            Params::MoveAll(MoveAllCommand {
                moves: vec![move_all_command::Body { from_inventory_id: source.clone(), to_inventory_id: target.clone(), ..Default::default() }],
            }),
            Params::CreateStowContext(CreateStowContextsCommand {
                creates: vec![create_stow_contexts_command::Body {
                    context: Some(EntityStowContext { entity_id: ghost, owner_id: me, is_stowed: true, inventory_id: Some(spare.clone()), ..Default::default() }),
                }],
            }),
            Params::UpdateParentUrn(UpdateParentUrnsCommand {
                updates: vec![update_parent_urns_command::Body { entity_geid: ghost, new_parent_urn: "urn:new".into() }],
                inventory_id: String::new(),
            }),
            Params::Transfer(TransferCommand {
                transfers: vec![transfer_command::Body {
                    clear: false,
                    entity_id: parent,
                    include_children: true,
                    owner: Some(transfer_command::body::Owner::PlayerId(me)),
                }],
                inventory_id: String::new(),
            }),
        ];
        graph.mutate_entities(signed(&fixture.token, mutate(params))).await.unwrap();

        let context = stow_of(&fixture, child).await;
        assert_eq!(context.inventory_id.as_deref(), Some(target.as_str()));
        assert_eq!((occupancy(&fixture, &source).await, occupancy(&fixture, &target).await), (0, 1));
        let ghost_context = stow_of(&fixture, ghost).await;
        assert_eq!((ghost_context.parent_urn.as_str(), ghost_context.owner_id), ("urn:new", me));
        let doc = "FOR e IN entities FILTER e._key == @key RETURN e";
        let row = graph.db.aql(doc, json!({ "key": child.to_string() })).await.unwrap().remove(0);
        assert_eq!(row["stackSize"], json!(3));
        assert_eq!(row["labels"], json!([2]));
        assert_eq!(row["tags"], json!([4]));
        assert_eq!(row["gameFlags"], json!(u64::MAX.to_string()));
        assert_eq!(row["scale"], json!(1.5));

        let foreign = mutate(vec![Params::DeleteLtRecord(DeleteLtRecordCommand { urns: vec![urn.clone()] })]);
        let refused = graph.mutate_entities(signed(&fixture.other_token, foreign)).await;
        assert_eq!(refused.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let params = vec![
            Params::DeleteLtRecord(DeleteLtRecordCommand { urns: vec![urn] }),
            Params::EraseInventory(EraseInventoryCommand {
                erasures: vec![erase_inventory_command::Body { id: target.clone() }],
            }),
        ];
        graph.mutate_entities(signed(&fixture.token, mutate(params))).await.unwrap();
        let left = "FOR e IN entities FILTER e._key == @key RETURN e";
        assert!(graph.db.aql(left, json!({ "key": child.to_string() })).await.unwrap().is_empty());
    }

    /// Filters and prunes are compiled to text, so each kind is run once against the database.
    #[tokio::test]
    #[ignore]
    async fn filters_and_prunes_run_on_the_database() {
        use sc_proto::services::entitygraph::v1::{
            prune_edge_filter, BitFilter, BitOperator, EdgeFilter, EntityClassFilter, EntityClassPropertyFilter,
            EntityCompositeFilter, EntityPruneConstraint, LabelOperator, LogicalOperator, NodeLabelFilter,
            PruneEdgeFilter, PruneEdgePropertyFilter,
        };
        let fixture = fixture().await;
        let (graph, me) = (&fixture.graph, fixture.geid);
        let base = fresh();
        let (top, middle, bottom) = (base, base + 1, base + 2);
        let seed = EntityGraph {
            nodes: vec![entity_node(top, me), entity_node(middle, me), entity_node(bottom, me)],
            edges: vec![attached(middle, top), attached(bottom, middle)],
        };
        graph.mutate_entities(signed(&fixture.token, mutate(vec![create(seed)]))).await.unwrap();
        let text = |value: u64| ScalarValue { scalar_type: Some(scalar_value::ScalarType::StringValue(value.to_string())) };
        let wrap = |filter_type| EntityFilter { filter_type: Some(filter_type) };
        let is_geid = |geid: u64| geid_query(geid).filter.unwrap();
        let attached_to_top = wrap(entity_filter::FilterType::EdgeFilter(EdgeFilter { edge_type: "ATTACHED_TO".into(), values: vec![text(top)] }));
        let not_player = wrap(entity_filter::FilterType::LabelFilter(NodeLabelFilter { operator: LabelOperator::HasNone as i32, labels: vec![7] }));
        let no_bits = wrap(entity_filter::FilterType::BitFilter(BitFilter { property: "entityFlags".into(), operator: BitOperator::Not as i32, value: 1 }));
        let everything = wrap(entity_filter::FilterType::CompositeFilter(EntityCompositeFilter {
            operator: LogicalOperator::And as i32,
            filters: vec![attached_to_top, not_player, no_bits],
        }));
        let query = EntityGraphQuery { filter: Some(everything), ..Default::default() };
        let reply = graph.entity_query(signed(&fixture.token, entity_body(query))).await.unwrap().into_inner().body.unwrap();
        assert_eq!(geids(&reply.results.unwrap()), [middle]);

        let class = EntityClassFilter {
            filter_type: Some(sc_proto::services::entitygraph::v1::entity_class_filter::FilterType::PropertyFilter(EntityClassPropertyFilter {
                property: "className".into(),
                operator: ComparisonOperator::Equal as i32,
                values: vec![text(0)],
            })),
        };
        let query = EntityGraphQuery { entity_class_filter: Some(class), ..geid_query(top) };
        let reply = graph.entity_query(signed(&fixture.token, entity_body(query))).await.unwrap().into_inner().body.unwrap();
        assert!(geids(&reply.results.unwrap()).is_empty());

        let walk = |tree: EntityTreeProjection| EntityGraphQuery {
            projection: Some(EntityProjection { tree: Some(tree), ..Default::default() }),
            ..geid_query(top)
        };
        let run = |query: EntityGraphQuery| async {
            let reply = graph.entity_query(signed(&fixture.token, entity_body(query))).await.unwrap();
            let mut found = geids(&reply.into_inner().body.unwrap().results.unwrap());
            found.sort();
            found
        };
        let pruned = EntityTreeProjection {
            enabled: true,
            prune: Some(EntityPruneConstraint { from: Some(is_geid(middle)), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(run(walk(pruned)).await, [top, middle]);
        let at_root = EntityTreeProjection {
            enabled: true,
            prune: Some(EntityPruneConstraint { from: Some(is_geid(top)), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(run(walk(at_root)).await, [top]);
        let past_root = EntityTreeProjection {
            enabled: true,
            prune: Some(EntityPruneConstraint { from_root_node: true, from: Some(is_geid(top)), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(run(walk(past_root)).await, [top, middle, bottom]);
        let edge_cut = EntityTreeProjection {
            enabled: true,
            prune: Some(EntityPruneConstraint {
                edge_filter: Some(PruneEdgeFilter {
                    filter_type: Some(prune_edge_filter::FilterType::PropertyFilter(PruneEdgePropertyFilter {
                        property: "attachmentType".into(),
                        operator: ComparisonOperator::Equal as i32,
                        values: vec![ScalarValue { scalar_type: Some(scalar_value::ScalarType::IntegerValue(0)) }],
                        edge_type: "ATTACHED_TO".into(),
                    })),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(run(walk(edge_cut)).await, [top, middle]);
        let only_bottom = EntityTreeProjection { enabled: true, filter: Some(is_geid(bottom)), ..Default::default() };
        assert_eq!(run(walk(only_bottom.clone())).await, [bottom]);
        let path = EntityTreeProjection { path_mode: true, ..only_bottom };
        assert_eq!(run(walk(path)).await, [top, middle, bottom]);
    }

    /// The same query as the client makes, through a real gRPC server and gzip.
    #[tokio::test]
    #[ignore]
    async fn the_wire_format_works_end_to_end() {
        let fixture = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = EntityGraphServiceServer::from_arc(fixture.graph.clone())
            .accept_compressed(CompressionEncoding::Gzip)
            .max_decoding_message_size(MAX_MESSAGE_BYTES);
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(Server::builder().add_service(service).serve_with_incoming(incoming));
        tokio::time::sleep(Duration::from_millis(100)).await;

        let channel = Endpoint::from_shared(format!("http://{address}")).unwrap().connect().await.unwrap();
        let mut client = EntityGraphServiceClient::new(channel).send_compressed(CompressionEncoding::Gzip);
        let request = signed(&fixture.token, entity_body(geid_query(fixture.geid)));
        let reply = client.entity_query(request).await.unwrap().into_inner().body.unwrap();
        assert_eq!(geids(&reply.results.unwrap()), [fixture.geid]);
        let version = GetDatabaseVersionRequest { body: None };
        let reply = client.get_database_version(version).await.unwrap().into_inner();
        assert_eq!(reply.body.unwrap().version, DATABASE_VERSION);
    }
}

#[test]
fn owners_skip_unowned_and_unparsable_entities() {
    let rows = [
        json!({ "entity": "18446744073709551615", "owner": "7" }),
        json!({ "entity": "2", "owner": "0" }),
        json!({ "entity": "3", "owner": null }),
        json!({ "entity": "x", "owner": "8" }),
    ];
    assert_eq!(owners(&rows), [EntityOwner { entity: u64::MAX, owner: 7 }]);
}

/// The internal service on the port of the public one, through a real channel, with no database behind it.
#[tokio::test]
async fn the_internal_service_shares_the_port_and_refuses_a_bad_prefix() {
    use sc_api::internal::entity_graph_internal_client::EntityGraphInternalClient;
    use tokio_stream::wrappers::TcpListenerStream;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dir = std::env::temp_dir().join(format!("sc-entitygraph-internal-{}", std::process::id()));
    let graph = Arc::new(Graph {
        state: State::open(&dir).unwrap(),
        db: Arango::lazy("http://127.0.0.1:1"),
        push: PushPublisherClient::new(Endpoint::from_static("http://127.0.0.1:1").connect_lazy()),
        known_players: Mutex::default(),
    });
    let server = Server::builder()
        .add_service(EntityGraphServiceServer::from_arc(graph.clone()))
        .add_service(EntityGraphInternalServer::from_arc(graph));
    tokio::spawn(server.serve_with_incoming(TcpListenerStream::new(listener)));
    let mut client = EntityGraphInternalClient::connect(format!("http://{address}")).await.unwrap();
    let empty = client.get_owners(GetOwnersRequest::default()).await.unwrap().into_inner();
    assert!(empty.owners.is_empty());
    let request = LoadAreaRequest { system: "pyro".into(), roots: vec!["pyro1".into()], collection_prefix: "x/".into(), ..Default::default() };
    assert_eq!(client.load_area(request).await.unwrap_err().code(), tonic::Code::InvalidArgument);
    let _ = std::fs::remove_dir_all(dir);
}

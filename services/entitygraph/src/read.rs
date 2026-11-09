//! The read side of the graph. Everything is walked child to parent: an edge runs from what is held to
//! what holds it, so the contents of a node are its INBOUND neighbours.

use std::collections::{HashMap, HashSet};

use sc_db::{internal, Arango};
use sc_proto::{
    common::api::v1::{PaginationArguments, PaginationInfo, PaginationOrder, PaginationSort},
    services::entitygraph::v1::{
        entity_sorting_arguments, inventory_query, inventory_tree_filter, ContainerGraph, ContainerGraphQuery,
        ContainerGraphStreamQuery, ContainerPruneConstraint, ContainerQueryResponseBody, EntityEdgeType, EntityGraph,
        EntityGraphQuery, EntityProjection, EntityQueryResponseBody, EntitySortingArguments, EntityStowContext,
        EntityTreeProjection, Inventory, InventoryQuery, InventoryQueryResponseBody, InventoryTreeFilter, Node,
        Scope, ScopeType, SortComparator,
    },
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::info;

use crate::{
    doc::{self, entity_id, inventory_handle},
    filter::{self, Binds, Column, Kind, ALWAYS},
};

const MAX_DEPTH: u32 = 64;
/// The page size of the client's own graph implementation.
const DEFAULT_PAGE: u64 = 1024;
const MAX_PAGE: u64 = 10_000;
pub const DEFAULT_BATCH: usize = 1000;

struct Page {
    offset: u64,
    first: u64,
}

impl Page {
    fn new(arguments: Option<&PaginationArguments>) -> Self {
        let first = arguments.map_or(0, |page| u64::from(page.first));
        Self {
            offset: arguments.and_then(|page| page.after.parse().ok()).unwrap_or(0),
            first: if first == 0 { DEFAULT_PAGE } else { first.min(MAX_PAGE) },
        }
    }

    fn limit(&self) -> String {
        format!("LIMIT {}, {}", self.offset, self.first + 1)
    }

    /// Cuts the extra row `limit` asked for, which tells whether another page exists.
    fn finish<T>(&self, rows: &mut Vec<T>) -> PaginationInfo {
        let has_next_page = rows.len() as u64 > self.first;
        rows.truncate(self.first as usize);
        PaginationInfo {
            has_next_page,
            has_previous_page: self.offset > 0,
            start_cursor: self.offset.to_string(),
            end_cursor: (self.offset + rows.len() as u64).to_string(),
        }
    }
}

fn conditions(parts: &[String]) -> String {
    if parts.is_empty() {
        return ALWAYS.to_string();
    }
    parts.join(" AND ")
}

fn direction(order: PaginationOrder) -> &'static str {
    if order == PaginationOrder::Descending {
        "DESC"
    } else {
        "ASC"
    }
}

fn sort_expression(var: &str, column: Column, numeric: bool) -> String {
    if column.1 == Kind::Big || numeric {
        format!("TO_NUMBER({var}.{})", column.0)
    } else {
        format!("{var}.{}", column.0)
    }
}

fn entity_sort(sort: Option<&EntitySortingArguments>) -> String {
    use entity_sorting_arguments::Property;
    let Some(sort) = sort else {
        return "e._key".to_string();
    };
    let chosen = match &sort.property {
        Some(Property::EntityProperty(by)) => filter::entity_column(&by.property)
            .map(|column| sort_expression("e", column, by.sort_comparator() == SortComparator::Numerical)),
        _ => None,
    };
    format!("{} {}, e._key", chosen.unwrap_or_else(|| "e._key".to_string()), direction(sort.order()))
}

fn plain_sort(var: &str, sort: Option<&PaginationSort>, column: fn(&str) -> Option<Column>) -> String {
    let Some(sort) = sort else {
        return format!("{var}._key");
    };
    match column(&sort.by) {
        Some(column) => format!("{} {}, {var}._key", sort_expression(var, column, false), direction(sort.order())),
        None => format!("{var}._key {}", direction(sort.order())),
    }
}

/// One step of a traversal: the vertex, the edge that led to it (none for the start), whether the
/// vertex passes the filter, and the path from the start.
struct Step {
    vertex: Value,
    edge: Value,
    keep: bool,
    path: Vec<String>,
}

struct Walk<'a> {
    collections: &'a str,
    edges: &'a str,
    starts: Vec<String>,
    /// Only these edge types are followed. None follows all.
    types: Option<Vec<i32>>,
    prune: Vec<String>,
    keep: String,
    binds: Binds,
}

impl Walk<'_> {
    async fn run(mut self, db: &Arango) -> Result<Vec<Step>, Status> {
        let starts = self.binds.add(json!(self.starts));
        let mut prune = self.prune;
        let mut follow = ALWAYS.to_string();
        if let Some(types) = &self.types {
            let types = self.binds.add(json!(types));
            prune.push(format!("(e != null AND e.type NOT IN {types})"));
            follow = format!("e == null OR e.type IN {types}");
        }
        let prune = if prune.is_empty() { "false".to_string() } else { prune.join(" OR ") };
        let query = format!(
            "WITH {collections}
             FOR start IN {starts}
                 FOR v, e, p IN 0..{MAX_DEPTH} INBOUND start {edges}
                     PRUNE {prune}
                     OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                     FILTER {follow}
                     RETURN {{ v, e, keep: {keep}, path: p.vertices[*]._id }}",
            collections = self.collections,
            edges = self.edges,
            keep = self.keep,
        );
        let rows = db.aql(&query, self.binds.into_value()).await.map_err(internal)?;
        Ok(rows.into_iter().filter(|row| row["v"].is_object()).map(step).collect())
    }
}

fn step(row: Value) -> Step {
    let path = row["path"].as_array().into_iter().flatten();
    Step {
        path: path.filter_map(|id| id.as_str().map(str::to_string)).collect(),
        keep: row["keep"].as_bool().unwrap_or(false),
        vertex: row["v"].clone(),
        edge: row["e"].clone(),
    }
}

/// The ids to return. In path mode a kept node brings along every node on its way from the start.
fn kept(steps: &[Step], path_mode: bool) -> HashSet<String> {
    let mut ids = HashSet::new();
    for step in steps.iter().filter(|step| step.keep) {
        if path_mode {
            ids.extend(step.path.iter().cloned());
        } else {
            ids.extend(step.path.last().cloned());
        }
    }
    ids
}

fn tree_steps(steps: Vec<Step>, path_mode: bool) -> Vec<Step> {
    let ids = kept(&steps, path_mode);
    let mut seen = HashSet::new();
    let wanted = |step: &Step| {
        let id = step.vertex["_id"].as_str().unwrap_or_default();
        ids.contains(id) && seen.insert(id.to_string())
    };
    steps.into_iter().filter(wanted).collect()
}

fn is_inventory(doc: &Value) -> bool {
    doc["_id"].as_str().is_some_and(|id| id.starts_with(doc::INVENTORIES))
}

fn tree_walk<'a>(tree: &EntityTreeProjection, starts: Vec<String>) -> Result<Walk<'a>, Status> {
    let mut binds = Binds::default();
    let mut prune = Vec::new();
    if let Some(constraint) = &tree.prune {
        if let Some(from) = &constraint.from {
            let matches = filter::entity(from, "v", &mut binds)?;
            let guard = if constraint.from_root_node { "e != null AND " } else { "" };
            prune.push(format!("({guard}!IS_SAME_COLLECTION('inventories', v) AND ({matches}))"));
        }
        if let Some(edges) = &constraint.edge_filter {
            if let Some(matches) = filter::prune_edge(edges, "e", &mut binds)? {
                prune.push(format!("(e != null AND ({matches}))"));
            }
        }
        if let Some(inventories) = &constraint.inventory_filter {
            let matches = filter::inventory(inventories, "v", &mut binds)?;
            prune.push(format!("(IS_SAME_COLLECTION('inventories', v) AND ({matches}))"));
        }
    }
    let entity_keep = match &tree.filter {
        Some(wanted) => filter::entity(wanted, "v", &mut binds)?,
        None => ALWAYS.to_string(),
    };
    let inventory_keep = match &tree.inventory_filter {
        Some(wanted) => filter::inventory(wanted, "v", &mut binds)?,
        None => ALWAYS.to_string(),
    };
    let mut types = vec![EntityEdgeType::AttachedTo as i32];
    if tree.include_inventory_nodes {
        types.push(EntityEdgeType::StowedIn as i32);
    }
    Ok(Walk {
        collections: "entities, inventories",
        edges: "entity_edges",
        starts,
        types: Some(types),
        prune,
        keep: format!("IS_SAME_COLLECTION('inventories', v) ? ({inventory_keep}) : ({entity_keep})"),
        binds,
    })
}

fn handles(rows: &[Value]) -> Vec<String> {
    rows.iter().filter_map(|row| row["_id"].as_str().map(str::to_string)).collect()
}

async fn entity_tree(
    db: &Arango,
    roots: &[Value],
    tree: &EntityTreeProjection,
) -> Result<(Vec<Value>, Vec<Value>), Status> {
    let steps = tree_walk(tree, handles(roots))?.run(db).await?;
    let steps = tree_steps(steps, tree.path_mode);
    let shown = |step: &&Step| tree.include_inventory_nodes || !is_inventory(&step.vertex);
    let nodes = steps.iter().filter(shown).map(|step| step.vertex.clone()).collect();
    let edges = steps.iter().filter(shown).filter(|step| step.edge.is_object()).map(|step| step.edge.clone()).collect();
    Ok((nodes, edges))
}

async fn outgoing_edges(db: &Arango, nodes: &[Value]) -> Result<Vec<Value>, Status> {
    let bind = json!({ "ids": handles(nodes) });
    db.aql("FOR x IN entity_edges FILTER x._from IN @ids RETURN x", bind).await.map_err(internal)
}

async fn attachments(
    db: &Arango,
    nodes: &[Value],
    projection: &EntityProjection,
    response: &mut EntityQueryResponseBody,
) -> Result<(), Status> {
    let entities = nodes.iter().filter(|node| !is_inventory(node));
    if projection.snapshots {
        let keys: Vec<&Value> = entities.clone().map(|node| &node["_key"]).collect();
        let query = "FOR s IN entity_snapshots FILTER s._key IN @keys RETURN s";
        let rows = db.aql(query, json!({ "keys": keys })).await.map_err(internal)?;
        let (include, exclude) = (projection.snapshot_flags, projection.snapshot_exclude_flags);
        response.snapshots = rows.iter().map(|row| doc::snapshot_message(row, include, exclude)).collect();
    }
    if projection.entity_classes {
        let crcs: Vec<&Value> = entities.map(|node| &node["classGuidCrc"]).collect();
        let query = "FOR c IN entity_classes FILTER c.guidHashCrc IN @crcs RETURN c";
        let rows = db.aql(query, json!({ "crcs": crcs })).await.map_err(internal)?;
        response.entity_classes = rows.iter().map(doc::entity_class).collect();
    }
    Ok(())
}

/// An item whose class has an inventory container but which was made before the class was seeded (or before it had
/// one) has no `Container` inventory yet. The client asks for it by id when it opens the item, so it is made then, the
/// way creating the item would have: sized by the class, held by the item. Nothing is made for an entity that does not
/// exist or whose class is not known to have a container. Returns whether it was made.
pub async fn ensure_container(db: &Arango, inventory_id: &str, now: u64) -> Result<bool, Status> {
    let Some(parsed) = doc::parse_inventory_id(inventory_id) else {
        return Ok(false);
    };
    if parsed.context != doc::CONTAINER {
        return Ok(false);
    }
    let Some(mut inventory) = doc::implied_inventory_doc(inventory_id, now) else {
        return Ok(false);
    };
    doc::merge(&mut inventory, json!({ "dirty": true }));
    let edge = doc::holder_edge(inventory_id, parsed.holder);
    let query = "FOR e IN entities FILTER e._key == @holder
                     LET class = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN c)
                     FILTER class.inventory != null
                     LET missing = LENGTH(FOR i IN inventories FILTER i._key == @key LIMIT 1 RETURN 1) == 0
                     FILTER missing
                     INSERT MERGE(@doc, { inventoryType: @physical,
                                          configuration: { physical: { capacity: class.inventory.capacityMicroScu, occupancy: 0 } } })
                         INTO inventories OPTIONS { overwriteMode: 'ignore' }
                     RETURN 1";
    let bind = json!({
        "holder": parsed.holder.to_string(),
        "key": doc::key(inventory_id),
        "doc": inventory,
        "physical": sc_proto::services::entitygraph::v1::InventoryType::Physical as i32,
    });
    let made = db.aql(query, bind).await.map_err(internal)?;
    if made.is_empty() {
        return Ok(false);
    }
    let insert = "INSERT @edge INTO entity_edges OPTIONS { overwriteMode: 'ignore' }";
    db.aql(insert, json!({ "edge": edge })).await.map_err(internal)?;
    info!(inventory = inventory_id, "container inventory made on first query");
    Ok(true)
}

/// The client opens the ASOP's ship list by asking for the player's own `Location` (kind 3) and `Hangar` (kind 8)
/// inventories at the location id it computes from its local data, with `EntityQuery` and `inventory_id`. Neither exists
/// until something is stowed there, and a missing inventory makes the query fail instead of answering "no ships". So the
/// player's own inventory of these two kinds is made, infinite and held by the player, the first time it is asked for.
/// Another player's id, any other context and a malformed id make nothing. Returns whether it was made.
pub async fn ensure_player_inventory(db: &Arango, owner: u64, inventory_id: &str, now: u64) -> Result<bool, Status> {
    let Some(parsed) = doc::parse_inventory_id(inventory_id) else {
        return Ok(false);
    };
    if parsed.holder != owner || !matches!(parsed.context, doc::LOCATION | doc::HANGAR) {
        return Ok(false);
    }
    let Some(inventory) = doc::implied_inventory_doc(inventory_id, now) else {
        return Ok(false);
    };
    let query = "LET missing = LENGTH(FOR i IN inventories FILTER i._key == @key LIMIT 1 RETURN 1) == 0
                 FILTER missing
                 INSERT @doc INTO inventories OPTIONS { overwriteMode: 'ignore' }
                 RETURN 1";
    let made = db.aql(query, json!({ "key": doc::key(inventory_id), "doc": inventory })).await.map_err(internal)?;
    if made.is_empty() {
        return Ok(false);
    }
    let insert = "INSERT @edge INTO entity_edges OPTIONS { overwriteMode: 'ignore' }";
    db.aql(insert, json!({ "edge": doc::holder_edge(inventory_id, owner) })).await.map_err(internal)?;
    info!(inventory = inventory_id, context = parsed.context, "player inventory made on first query");
    Ok(true)
}

pub async fn entity_query(
    db: &Arango,
    scope: Option<Scope>,
    query: EntityGraphQuery,
) -> Result<EntityQueryResponseBody, Status> {
    let scope = scope.unwrap_or_default();
    // Nothing is buried yet: an erased entity is removed from the graph.
    if scope.r#type() == ScopeType::Graveyard {
        return Ok(EntityQueryResponseBody::default());
    }
    let mut binds = Binds::default();
    let mut parts = Vec::new();
    if scope.r#type() == ScopeType::Shard {
        parts.push(format!("e.shardId == {}", binds.add(scope.shard_id)));
    }
    if !query.inventory_id.is_empty() {
        parts.push(format!("e.inventoryId == {}", binds.add(query.inventory_id.as_str())));
    }
    if let Some(wanted) = &query.filter {
        parts.push(filter::entity(wanted, "e", &mut binds)?);
    }
    if let Some(classes) = &query.entity_class_filter {
        parts.push(filter::class_membership(classes, "e", &mut binds)?);
    }
    let page = Page::new(query.pagination.as_ref());
    let aql = format!(
        "FOR e IN entities FILTER {} SORT {} {} RETURN e",
        conditions(&parts),
        entity_sort(query.sort.as_ref()),
        page.limit(),
    );
    let mut roots = db.aql(&aql, binds.into_value()).await.map_err(internal)?;
    let page_info = page.finish(&mut roots);
    let projection = query.projection.unwrap_or_default();
    let tree = projection.tree.clone().unwrap_or_default();
    let (nodes, edges) = if tree.enabled {
        let (nodes, mut edges) = entity_tree(db, &roots, &tree).await?;
        if projection.outgoing_edges {
            // The tree walk follows edges from what is held to what holds it, so it never returns the edge a
            // node leaves by: the stowed edge of an item, which is what puts it in its inventory.
            let known: HashSet<String> = edges.iter().filter_map(|edge| edge["_key"].as_str().map(str::to_string)).collect();
            let mut leaving = outgoing_edges(db, &nodes).await?;
            leaving.retain(|edge| edge["_key"].as_str().is_some_and(|key| !known.contains(key)));
            edges.extend(leaving);
        }
        (nodes, edges)
    } else if projection.outgoing_edges {
        let edges = outgoing_edges(db, &roots).await?;
        (roots, edges)
    } else {
        (roots, Vec::new())
    };
    let mut response = EntityQueryResponseBody {
        page_info: Some(page_info),
        results: Some(EntityGraph {
            nodes: nodes.iter().map(doc::any_node).collect(),
            edges: edges.iter().map(doc::edge_message).collect(),
        }),
        ..Default::default()
    };
    attachments(db, &nodes, &projection, &mut response).await?;
    if !query.inventory_id.is_empty() {
        let results = response.results.as_ref();
        info!(
            inventory = %query.inventory_id,
            filtered = query.filter.is_some(),
            tree = tree.enabled,
            nodes = results.map_or(0, |graph| graph.nodes.len()),
            edges = results.map_or(0, |graph| graph.edges.len()),
            "inventory entity query"
        );
    }
    Ok(response)
}

fn inventory_message(doc: &Value) -> Inventory {
    Inventory { properties: Some(doc::inventory_properties(doc)) }
}

async fn inventory_tree(db: &Arango, tree: &InventoryTreeFilter) -> Result<Vec<Value>, Status> {
    let start = match &tree.root {
        Some(inventory_tree_filter::Root::EntityGeid(geid)) => entity_id(*geid),
        Some(inventory_tree_filter::Root::InventoryId(id)) => inventory_handle(id),
        None => return Err(Status::invalid_argument("an inventory tree needs a root")),
    };
    let mut binds = Binds::default();
    let mut prune = Vec::new();
    if let Some(constraint) = &tree.prune {
        if let Some(from) = &constraint.from {
            let matches = filter::inventory(from, "v", &mut binds)?;
            prune.push(format!("(IS_SAME_COLLECTION('inventories', v) AND ({matches}))"));
        }
        if let Some(edges) = &constraint.edge_filter {
            if let Some(matches) = filter::prune_edge(edges, "e", &mut binds)? {
                prune.push(format!("(e != null AND ({matches}))"));
            }
        }
        if let Some(entities) = &constraint.entity_filter {
            let matches = filter::entity(entities, "v", &mut binds)?;
            prune.push(format!("(!IS_SAME_COLLECTION('inventories', v) AND ({matches}))"));
        }
    }
    let wanted = match &tree.filter {
        Some(wanted) => filter::inventory(wanted, "v", &mut binds)?,
        None => ALWAYS.to_string(),
    };
    let walk = Walk {
        collections: "entities, inventories",
        edges: "entity_edges",
        starts: vec![start],
        types: None,
        prune,
        keep: format!("IS_SAME_COLLECTION('inventories', v) AND ({wanted})"),
        binds,
    };
    let steps = tree_steps(walk.run(db).await?, true);
    Ok(steps.into_iter().map(|step| step.vertex).filter(is_inventory).collect())
}

pub async fn inventory_query(db: &Arango, query: InventoryQuery) -> Result<InventoryQueryResponseBody, Status> {
    let page = Page::new(query.pagination.as_ref());
    let mut rows = match &query.find_by {
        Some(inventory_query::FindBy::Tree(tree)) => {
            let found = inventory_tree(db, tree).await?;
            found.into_iter().skip(page.offset as usize).take(page.first as usize + 1).collect()
        }
        by_filter => {
            let mut binds = Binds::default();
            let wanted = match by_filter {
                Some(inventory_query::FindBy::Filter(wanted)) => filter::inventory(wanted, "i", &mut binds)?,
                _ => ALWAYS.to_string(),
            };
            let sort = plain_sort("i", query.sort.as_ref(), filter::inventory_column);
            let aql = format!("FOR i IN inventories FILTER {wanted} SORT {sort} {} RETURN i", page.limit());
            db.aql(&aql, binds.into_value()).await.map_err(internal)?
        }
    };
    let page_info = page.finish(&mut rows);
    Ok(InventoryQueryResponseBody {
        page_info: Some(page_info),
        results: rows.iter().map(inventory_message).collect(),
    })
}

/// The inventories of a player: those it owns (personal, location, hangar, ...) and the containers of the entities
/// it owns (a ship's cargo grids, its crates and backpacks, at any depth), which have no owner of their own. With
/// `everything` false, only the shared ones.
pub async fn inventories_of(db: &Arango, owner: &str, everything: bool) -> Result<Vec<Node>, Status> {
    let query = "LET own = (FOR i IN inventories FILTER i.ownerId == @owner RETURN i)
                 LET held = (FOR e IN entities FILTER e.ownerId == @owner
                                 FOR i IN inventories FILTER i.holderId == e._key AND i.ownerId == '0' RETURN i)
                 FOR i IN UNION_DISTINCT(own, held)
                     FILTER @everything OR i.shared == true
                     SORT i._key
                     RETURN UNSET(i, 'dirty')";
    let rows = db.aql(query, json!({ "owner": owner, "everything": everything })).await.map_err(internal)?;
    Ok(rows.iter().map(doc::inventory_node).collect())
}

const STOW_FILTERS: &str = "
    FILTER @owner == '0' OR e.ownerId == @owner
    FILTER LENGTH(@crcs) == 0 OR e.classGuidCrc IN @crcs
    RETURN e";

fn stow_contexts(rows: &[Value]) -> Vec<EntityStowContext> {
    rows.iter().map(doc::stow_context).collect()
}

pub async fn stow_by_geids(
    db: &Arango,
    geids: &[u64],
    owner: u64,
    crcs: &[u32],
) -> Result<Vec<EntityStowContext>, Status> {
    let keys: Vec<String> = geids.iter().map(u64::to_string).collect();
    let bind = json!({ "keys": keys, "owner": owner.to_string(), "crcs": crcs });
    let query = format!("FOR e IN entities FILTER e._key IN @keys {STOW_FILTERS}");
    Ok(stow_contexts(&db.aql(&query, bind).await.map_err(internal)?))
}

pub async fn stow_by_parent_urns(
    db: &Arango,
    urns: &[String],
    owner: u64,
    crcs: &[u32],
) -> Result<Vec<EntityStowContext>, Status> {
    let bind = json!({ "urns": urns, "owner": owner.to_string(), "crcs": crcs });
    let query = format!("FOR e IN entities FILTER e.parentUrn IN @urns {STOW_FILTERS}");
    Ok(stow_contexts(&db.aql(&query, bind).await.map_err(internal)?))
}

pub async fn stow_by_owner(db: &Arango, owner: u64) -> Result<Vec<EntityStowContext>, Status> {
    let query = "FOR e IN entities FILTER e.ownerId == @owner SORT e._key RETURN e";
    let rows = db.aql(query, json!({ "owner": owner.to_string() })).await.map_err(internal)?;
    Ok(stow_contexts(&rows))
}

fn container_walk<'a>(
    starts: Vec<String>,
    constraint: Option<&ContainerPruneConstraint>,
    wanted: Option<&sc_proto::services::entitygraph::v1::ContainerFilter>,
) -> Result<Walk<'a>, Status> {
    let mut binds = Binds::default();
    let mut prune = Vec::new();
    if let Some(constraint) = constraint {
        if let Some(from) = &constraint.from {
            let matches = filter::container(from, "v", &mut binds)?;
            let guard = if constraint.from_root_node { "e != null AND " } else { "" };
            prune.push(format!("({guard}({matches}))"));
        }
    }
    let keep = match wanted {
        Some(wanted) => filter::container(wanted, "v", &mut binds)?,
        None => ALWAYS.to_string(),
    };
    Ok(Walk { collections: "containers", edges: "container_edges", starts, types: None, prune, keep, binds })
}

fn container_graph(steps: &[Step]) -> ContainerGraph {
    ContainerGraph {
        nodes: steps.iter().map(|step| doc::container_node(&step.vertex)).collect(),
        edges: steps.iter().filter(|step| step.edge.is_object()).map(|step| doc::container_edge(&step.edge)).collect(),
    }
}

/// Containers describe the universe every shard shares, so the request's scope selects nothing.
pub async fn container_query(
    db: &Arango,
    query: ContainerGraphQuery,
) -> Result<ContainerQueryResponseBody, Status> {
    let mut binds = Binds::default();
    let wanted = match &query.filter {
        Some(wanted) => filter::container(wanted, "c", &mut binds)?,
        None => ALWAYS.to_string(),
    };
    let page = Page::new(query.pagination.as_ref());
    let sort = plain_sort("c", query.sort.as_ref(), filter::container_column);
    let aql = format!("FOR c IN containers FILTER {wanted} SORT {sort} {} RETURN c", page.limit());
    let mut roots = db.aql(&aql, binds.into_value()).await.map_err(internal)?;
    let page_info = page.finish(&mut roots);
    let tree = query.projection.and_then(|projection| projection.tree).unwrap_or_default();
    let results = if tree.enabled {
        let walk = container_walk(handles(&roots), tree.prune.as_ref(), tree.filter.as_ref())?;
        container_graph(&tree_steps(walk.run(db).await?, false))
    } else {
        ContainerGraph { nodes: roots.iter().map(doc::container_node).collect(), edges: Vec::new() }
    };
    Ok(ContainerQueryResponseBody { page_info: Some(page_info), results: Some(results) })
}

pub async fn container_stream(
    db: &Arango,
    query: ContainerGraphStreamQuery,
    batch_size: usize,
) -> Result<Vec<ContainerGraph>, Status> {
    let starts = if query.geid != 0 {
        vec![format!("containers/{}", query.geid)]
    } else {
        // No geid means the whole hierarchy, walked from its top nodes.
        let aql = "FOR c IN containers FILTER LENGTH(FOR x IN container_edges FILTER x._from == c._id LIMIT 1 RETURN 1) == 0 RETURN c._id";
        let rows = db.aql(aql, json!({})).await.map_err(internal)?;
        rows.iter().filter_map(|id| id.as_str().map(str::to_string)).collect()
    };
    let steps = if query.prune.is_none() && query.filter.is_none() {
        container_levels(db, starts).await?
    } else {
        let walk = container_walk(starts, query.prune.as_ref(), query.filter.as_ref())?;
        tree_steps(walk.run(db).await?, false)
    };
    Ok(steps.chunks(batch_size.max(1)).map(container_graph).collect())
}

async fn containers_by_id(db: &Arango, ids: &[String]) -> Result<Vec<Value>, Status> {
    let keys: Vec<&str> = ids.iter().filter_map(|id| id.split_once('/').map(|(_, key)| key)).collect();
    let aql = "FOR c IN containers FILTER c._key IN @keys RETURN c";
    db.aql(aql, json!({ "keys": keys })).await.map_err(internal)
}

/// Pairs each edge with the vertex it came from, and leaves out an edge whose vertex is gone.
fn level_steps(edges: Vec<Value>, vertices: Vec<Value>) -> Vec<Step> {
    let id = |doc: &Value| doc["_id"].as_str().unwrap_or_default().to_string();
    let mut vertices: HashMap<String, Value> = vertices.into_iter().map(|vertex| (id(&vertex), vertex)).collect();
    let paired = edges.into_iter().filter_map(|edge| {
        let vertex = vertices.remove(edge["_from"].as_str()?)?;
        Some(Step { path: Vec::new(), keep: true, vertex, edge })
    });
    paired.collect()
}

/// The same breadth-first order as a traversal, one level per query. A traversal crosses shards for every
/// vertex: the client's whole-universe request took 346 s that way on the dev cluster and 7 s this way, and
/// the client gives this step a timeout.
async fn container_levels(db: &Arango, starts: Vec<String>) -> Result<Vec<Step>, Status> {
    let tops = containers_by_id(db, &starts).await?;
    let mut steps: Vec<Step> =
        tops.into_iter().map(|vertex| Step { path: Vec::new(), keep: true, vertex, edge: Value::Null }).collect();
    let mut seen: HashSet<String> = starts.iter().cloned().collect();
    let mut level = starts;
    while !level.is_empty() {
        let aql = "FOR e IN container_edges FILTER e._to IN @ids RETURN e";
        let edges = db.aql(aql, json!({ "ids": level })).await.map_err(internal)?;
        let source = |edge: &Value| edge["_from"].as_str().unwrap_or_default().to_string();
        let edges: Vec<Value> = edges.into_iter().filter(|edge| seen.insert(source(edge))).collect();
        level = edges.iter().map(source).collect();
        steps.extend(level_steps(edges, containers_by_id(db, &level).await?));
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walked(id: &str, keep: bool, path: &[&str]) -> Step {
        Step {
            vertex: json!({ "_id": id }),
            edge: Value::Null,
            keep,
            path: path.iter().map(|part| part.to_string()).collect(),
        }
    }

    #[test]
    fn path_mode_keeps_the_way_down_to_a_match() {
        let steps = || {
            vec![
                walked("entities/1", false, &["entities/1"]),
                walked("entities/2", false, &["entities/1", "entities/2"]),
                walked("entities/3", true, &["entities/1", "entities/2", "entities/3"]),
                walked("entities/4", false, &["entities/1", "entities/4"]),
            ]
        };
        let ids = |steps: Vec<Step>| steps.iter().map(|s| s.vertex["_id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(tree_steps(steps(), true)), ["entities/1", "entities/2", "entities/3"]);
        assert_eq!(ids(tree_steps(steps(), false)), ["entities/3"]);
    }

    #[test]
    fn a_level_pairs_edges_with_their_vertices() {
        let edge = |from: &str| json!({ "_from": format!("containers/{from}"), "_to": "containers/1", "startId": from, "endId": "1" });
        let vertex = |key: &str| json!({ "_id": format!("containers/{key}"), "geid": key });
        let steps = level_steps(vec![edge("2"), edge("3"), edge("4")], vec![vertex("3"), vertex("2")]);
        let pairs: Vec<(&str, &str)> =
            steps.iter().map(|step| (step.vertex["geid"].as_str().unwrap(), step.edge["startId"].as_str().unwrap())).collect();
        assert_eq!(pairs, [("2", "2"), ("3", "3")]);
    }

    #[test]
    fn pages_ask_for_one_row_more_than_they_return() {
        let page = Page::new(Some(&PaginationArguments { first: 2, after: "4".into() }));
        assert_eq!(page.limit(), "LIMIT 4, 3");
        let mut rows = vec![1, 2, 3];
        let info = page.finish(&mut rows);
        assert_eq!(rows, [1, 2]);
        assert!(info.has_next_page && info.has_previous_page);
        assert_eq!((info.start_cursor.as_str(), info.end_cursor.as_str()), ("4", "6"));
    }

    #[test]
    fn an_empty_page_size_means_the_clients_default() {
        assert_eq!(Page::new(None).first, DEFAULT_PAGE);
    }

    #[test]
    fn sorting_a_big_column_is_numeric_and_stable() {
        use sc_proto::services::entitygraph::v1::EntitySortingByProperty;
        let sort = EntitySortingArguments {
            order: PaginationOrder::Descending as i32,
            property: Some(entity_sorting_arguments::Property::EntityProperty(EntitySortingByProperty {
                property: "ownerId".into(),
                sort_comparator: 0,
            })),
        };
        assert_eq!(entity_sort(Some(&sort)), "TO_NUMBER(e.ownerId) DESC, e._key");
        assert_eq!(entity_sort(None), "e._key");
    }
}

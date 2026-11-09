//! Turns entity graph commands into AQL statements that run in one transaction, so a request either
//! happens completely or not at all. A statement that must stop the transaction uses ASSERT with a
//! message starting `precondition:`, which `main` reports as FAILED_PRECONDITION.
//!
//! Inventories (see docs/INVENTORY.md): a statement that changes what an inventory holds marks it `dirty`, and the
//! last statement of every plan counts the occupancy of each dirty inventory again from its stowed edges and refuses
//! the whole transaction when one is over its capacity. Counting at the end, instead of adding and subtracting on
//! the way, keeps stack changes, moves and erasures right in any order, and the write to the inventory document
//! makes two transactions that fill the same inventory conflict instead of both passing the check.

use prost::Message;
use sc_proto::services::entitygraph::v1::{
    entity_graph_command::Params, node_properties, transfer_command, AttachCommand,
    AttachmentOwnershipBehavior, AttachmentType, ChangeInventoryConfigurationCommand, CreateCommand, CreateInventoryArguments,
    CreateInventoryCommand, EntityEdge, EntityEdgeProperties, EntityEdgeType, EntityEdgeVertex, EntityGraphCommand,
    EntitySnapshot, EraseCommand, InventoryNodeProperties, InventoryType, MoveAllCommand, MoveCommand,
    MutationArguments, NodeType, ScopeType, SyncCommand,
};
use serde_json::{json, Map, Value};
use tonic::Status;

use crate::{
    doc::{self, edge_key, entity_id, inventory_handle, key, Stow},
    filter::{self, Binds},
};

pub const WRITES: &[&str] = &["applied_commands", "entities", "inventories", "entity_edges", "entity_snapshots", "lt_records"];
const MAX_DEPTH: u32 = 64;
/// The edges that place a node inside another: attachment and stowing. References do not.
const HOLDING_EDGES: &str = "[1, 2]";
const OVERWRITE: &str = "OPTIONS { overwriteMode: 'replace' }";
const KEEP_EXISTING: &str = "OPTIONS { overwriteMode: 'ignore' }";

#[derive(Default)]
pub struct Plan {
    queries: Vec<(String, Value)>,
    entities: Vec<String>,
    /// Inventory ids the caller writes into or takes from, checked against the caller before anything changes.
    inventories: Vec<String>,
    /// Entities taken out of the inventory they are in: that inventory is checked like the ones above.
    sources: Vec<String>,
    records: Vec<String>,
    stow: Stow,
    now: u64,
}

/// The owner of an inventory: its own owner, or else the first owner found walking up from it through its holder,
/// the entities that hold that one and the inventories they are stowed in. Binds `@me`; `{start}` is the handle the
/// walk starts from, `{direct}` the owner to use when it is not '0'.
fn effective_owner(start: &str, direct: &str) -> String {
    format!(
        "({direct} != '0' ? {direct} : NOT_NULL(FIRST(
             FOR v, x IN 0..{MAX_DEPTH} OUTBOUND {start} entity_edges
                 PRUNE x != null AND x.type NOT IN {HOLDING_EDGES}
                 OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                 FILTER (x == null OR x.type IN {HOLDING_EDGES}) AND v.ownerId != null AND v.ownerId != '0'
                 LIMIT 1
                 RETURN v.ownerId
         ), '0'))"
    )
}

/// What the access check needs of an inventory that may not exist yet: who its id says owns it, and its holder.
fn access_target(id: &str) -> Value {
    let parsed = doc::parse_inventory_id(id);
    let owner = parsed.as_ref().map_or(0, doc::implied_owner);
    let holder = parsed.as_ref().map_or(0, |parsed| parsed.holder);
    json!({ "id": id, "owner": owner.to_string(), "holder": holder.to_string() })
}
fn stowed_edge_key(geid: u64) -> String {
    edge_key(EntityEdgeType::StowedIn as i32, &geid.to_string(), "")
}

fn attached_edge_key(geid: u64) -> String {
    edge_key(EntityEdgeType::AttachedTo as i32, &geid.to_string(), "")
}

fn entity_vertex(geid: u64) -> EntityEdgeVertex {
    use sc_proto::services::entitygraph::v1::entity_edge_vertex::Id;
    EntityEdgeVertex { r#type: NodeType::Entity as i32, id: Some(Id::EntityId(geid)) }
}

fn inventory_vertex(id: &str) -> EntityEdgeVertex {
    use sc_proto::services::entitygraph::v1::entity_edge_vertex::Id;
    EntityEdgeVertex { r#type: NodeType::Inventory as i32, id: Some(Id::InventoryId(id.to_string())) }
}

/// Whether the edge leaves the entity `geid`, which then has a parent in its graph.
fn starts_at(edge: &EntityEdge, geid: u64) -> bool {
    use sc_proto::services::entitygraph::v1::entity_edge_vertex::Id;
    matches!(edge.start.as_ref().and_then(|vertex| vertex.id.as_ref()), Some(Id::EntityId(start)) if *start == geid)
}

fn stowed_edge(geid: u64, inventory_id: &str) -> Option<Value> {
    doc::edge_doc(&EntityEdge {
        r#type: EntityEdgeType::StowedIn as i32,
        start: Some(entity_vertex(geid)),
        end: Some(inventory_vertex(inventory_id)),
        properties: Some(EntityEdgeProperties::default()),
    })
}

fn set(bind: &mut Value, name: &str, value: impl Into<Value>) {
    if let Some(fields) = bind.as_object_mut() {
        fields.insert(name.to_string(), value.into());
    }
}

/// `include_children` of a transfer and the erasures walk everything below a node.
fn below(start: &str, field: &str) -> String {
    format!(
        "WITH entities, inventories
         LET ids = APPEND([{start}], (
             FOR v, x IN 1..{MAX_DEPTH} INBOUND {start} entity_edges
                 OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                 RETURN {field}
         ))"
    )
}

impl Plan {
    pub fn new(arguments: Option<&MutationArguments>, now: u64) -> Self {
        let scope = arguments.and_then(|arguments| arguments.scope.clone()).unwrap_or_default();
        let shard_id = (scope.r#type() == ScopeType::Shard).then_some(scope.shard_id);
        Self { stow: Stow { inventory_id: None, shard_id }, now, ..Default::default() }
    }

    fn push(&mut self, query: impl Into<String>, bind: Value) {
        self.queries.push((query.into(), bind));
    }

    fn touches(&mut self, geid: u64) {
        self.entities.push(geid.to_string());
    }

    fn touches_inventory(&mut self, id: &str) {
        self.inventories.push(id.to_string());
    }

    /// Marks the inventory `id` for the count at the end.
    fn mark(&mut self, id: &str) {
        self.push(
            "FOR i IN inventories FILTER i._key == @key UPDATE i WITH { dirty: true } IN inventories",
            json!({ "key": key(id) }),
        );
    }

    /// Marks the inventory the entity is stowed in now, if any.
    fn mark_holding(&mut self, geid: u64) {
        self.push(
            "FOR e IN entities FILTER e._key == @key AND e.stowed == true AND e.inventoryId != null
                 FOR i IN inventories FILTER i.id == e.inventoryId
                     UPDATE i WITH { dirty: true } IN inventories",
            json!({ "key": geid.to_string() }),
        );
    }

    /// Refuses to stow `moved` (an entity or inventory handle) into `target` when the target is inside it, which
    /// would make a loop: a crate in itself, or in a box it holds.
    fn refuse_cycle(&mut self, moved: &str, target: &str) {
        self.push(
            format!(
                "WITH entities, inventories
                 RETURN ASSERT(LENGTH(
                     FOR i IN inventories FILTER i.id == @target
                         FOR v, x IN 0..{MAX_DEPTH} OUTBOUND i entity_edges
                             PRUNE v._id == @moved OR (x != null AND x.type NOT IN {HOLDING_EDGES})
                             OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                             FILTER v._id == @moved
                             LIMIT 1
                             RETURN 1
                 ) == 0, 'precondition: an item cannot be stowed inside itself')"
            ),
            json!({ "moved": moved, "target": target }),
        );
    }

    /// Creates the inventory `id` if it is missing and its id says enough: a player's own inventories (location,
    /// personal, hangar, ...) as infinite ones owned by the player, and the container of an existing entity, with the
    /// capacity of its class when the class has an inventory container. A class known to have none gets no
    /// container; an entity of an unknown class gets an infinite one.
    fn ensure_inventory(&mut self, id: &str) {
        let Some(parsed) = doc::parse_inventory_id(id) else {
            return;
        };
        let Some(mut created) = doc::implied_inventory_doc(id, self.now) else {
            return;
        };
        doc::merge(&mut created, json!({ "dirty": true }));
        let edge = doc::holder_edge(id, parsed.holder);
        if parsed.context == doc::CONTAINER {
            self.containers(&[(parsed.holder, created, edge)], false);
        } else if doc::implied_owner(&parsed) != 0 {
            self.push(format!("INSERT @doc INTO inventories {KEEP_EXISTING}"), json!({ "doc": created }));
            self.push(format!("INSERT @doc INTO entity_edges {KEEP_EXISTING}"), json!({ "doc": edge }));
        }
    }

    /// Container inventories for entities, sized by their class (`entity_classes.inventory`, micro-SCU). With
    /// `class_required`, only for an entity whose class has an inventory container; otherwise also for one whose
    /// class is not known.
    fn containers(&mut self, wanted: &[(u64, Value, Value)], class_required: bool) {
        let rows: Vec<Value> = wanted
            .iter()
            .map(|(holder, doc, edge)| json!({ "holder": holder.to_string(), "doc": doc, "edge": edge }))
            .collect();
        let found = "FOR row IN @rows
                 FOR e IN entities FILTER e._key == row.holder
                     LET class = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN c)
                     FILTER class.inventory != null OR (!@required AND class == null)";
        let bind = json!({ "rows": rows, "required": class_required });
        let mut sized = bind.clone();
        set(&mut sized, "physical", InventoryType::Physical as i32);
        self.push(
            format!(
                "{found}
                     INSERT MERGE(row.doc, class.inventory == null ? {{}} : {{
                         inventoryType: @physical,
                         configuration: {{ physical: {{ capacity: class.inventory.capacityMicroScu, occupancy: 0 }} }}
                     }}) INTO inventories {KEEP_EXISTING}"
            ),
            sized,
        );
        self.push(format!("{found} INSERT row.edge INTO entity_edges {KEEP_EXISTING}"), bind);
    }

    /// The last statement: counts each marked inventory again and refuses one over its capacity. An item's room is
    /// the occupancy its stowed edge names, or else its class's `occupancyMicroScu` times its stack.
    fn recount(&self) -> (String, Value) {
        (
            "FOR i IN inventories FILTER i.dirty == true
                 LET used = SUM(
                     FOR x IN entity_edges FILTER x._to == i._id AND x.type == @stowed
                         LET e = DOCUMENT('entities', x.startId)
                         LET unit = e == null ? 0 : NOT_NULL(FIRST(
                             FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN c.occupancyMicroScu
                         ), 0)
                         RETURN x.physical.occupancy != null ? x.physical.occupancy : unit * MAX([1, NOT_NULL(e.stackSize, 0)])
                 )
                 LET capacity = NOT_NULL(i.configuration.physical.capacity, 0)
                 FILTER ASSERT(i.configuration == null OR capacity <= 0 OR used <= capacity,
                               CONCAT('precondition: inventory is full: ', i.id))
                 LET rules = i.context == 'Location'
                     ? FIRST(FOR c IN entity_classes FILTER c._key == @item_bank RETURN c.itemBank)
                     : FIRST(
                         FOR h IN entities FILTER h._key == TO_STRING(i.holderId)
                             FOR c IN entity_classes FILTER c.guidHashCrc == h.classGuidCrc RETURN c.inventory
                     )
                 LET excluded = NOT_NULL(rules.excluded, [])
                 LET only = NOT_NULL(rules.only, [])
                 FILTER ASSERT(
                     (LENGTH(excluded) == 0 AND LENGTH(only) == 0) OR LENGTH(
                         FOR x IN entity_edges FILTER x._to == i._id AND x.type == @stowed
                             LET e = DOCUMENT('entities', x.startId)
                             FILTER e != null
                             LET kind = FIRST(FOR c IN entity_classes FILTER c.guidHashCrc == e.classGuidCrc RETURN c)
                             FILTER kind != null
                             LET refused = LENGTH(FOR f IN excluded FILTER f.itemType == kind.itemType
                                               AND (LENGTH(f.itemSubTypes) == 0 OR kind.itemSubType IN f.itemSubTypes) RETURN 1) > 0
                             LET allowed = LENGTH(only) == 0 OR LENGTH(FOR f IN only FILTER f.itemType == kind.itemType
                                               AND (LENGTH(f.itemSubTypes) == 0 OR kind.itemSubType IN f.itemSubTypes) RETURN 1) > 0
                             FILTER refused OR NOT allowed
                             LIMIT 1
                             RETURN 1
                     ) == 0,
                     CONCAT('precondition: an item of that type is not allowed in inventory: ', i.id))
                 UPDATE i WITH {
                     dirty: null,
                     configuration: i.configuration == null ? null : { physical: { capacity: capacity, occupancy: used } },
                     updatedAt: @now
                 } IN inventories OPTIONS { keepNull: false, mergeObjects: false }"
                .to_string(),
            json!({ "stowed": EntityEdgeType::StowedIn as i32, "now": self.now, "item_bank": crate::seed::ITEM_BANK_CLASS }),
        )
    }

    /// A statement of the caller's own, such as an ASSERT, run in order with the commands' statements.
    pub fn check(&mut self, query: impl Into<String>, bind: Value) {
        self.push(query, bind);
    }

    /// The entity is taken out of the inventory it is in: it and that inventory are checked against the caller.
    pub fn takes_from(&mut self, geid: u64) {
        self.touches(geid);
        self.sources.push(geid.to_string());
    }

    /// Refuses when an entity up the holder chain of `start` (an entity's handle), the start included, is live on a
    /// shard: unstowed, with a shard id. The DGS has authority over it and everything it holds. The player `owner` is
    /// left out: whether the player is playing is the hub's to say, not the graph's. `inventory`, when given, is the
    /// inventory to start from instead if it exists already, `start` being its holder.
    fn refuse_live(&mut self, start: &str, inventory: Option<&str>, owner: u64, what: &str) {
        self.push(
            format!(
                "WITH entities, inventories
                 LET start = NOT_NULL(FIRST(FOR i IN inventories FILTER i._key == @inventory RETURN i._id), @start)
                 RETURN ASSERT(LENGTH(
                     FOR v, x IN 0..{MAX_DEPTH} OUTBOUND start entity_edges
                         PRUNE x != null AND x.type NOT IN {HOLDING_EDGES}
                         OPTIONS {{ order: 'bfs', uniqueVertices: 'global' }}
                         FILTER (x == null OR x.type IN {HOLDING_EDGES}) AND IS_SAME_COLLECTION('entities', v)
                             AND v._key != @owner AND v.stowed != true AND v.shardId != null AND v.shardId != ''
                         LIMIT 1
                         RETURN 1
                 ) == 0, CONCAT('precondition: ', @what))"
            ),
            json!({ "start": start, "inventory": inventory.map(key), "owner": owner.to_string(), "what": what }),
        );
    }

    /// See `refuse_live`: for an item and the inventories it is in.
    pub fn refuse_live_entity(&mut self, geid: u64, owner: u64) {
        self.refuse_live(&entity_id(geid), None, owner, "the item is live on a shard");
    }

    /// See `refuse_live`: for an inventory, which may not exist yet, and the entities that hold it.
    pub fn refuse_live_inventory(&mut self, id: &str, owner: u64) {
        let holder = entity_id(doc::inventory_holder(id));
        self.refuse_live(&holder, Some(id), owner, "the inventory is held by something live on a shard");
    }

    /// Updates one entity, if it exists. `object` is an AQL expression over `e` and the binds.
    fn patch(&mut self, geid: u64, object: &str, mut bind: Value) {
        self.touches(geid);
        set(&mut bind, "key", geid.to_string());
        set(&mut bind, "now", self.now);
        let query = format!(
            "FOR e IN entities FILTER e._key == @key
             UPDATE e WITH MERGE({object}, {{ updatedAt: @now, version: e.version + 1 }}) IN entities"
        );
        self.push(query, bind);
    }

    /// The statements to run, with the ownership checks first. An entity that belongs to someone else stops
    /// the transaction; one with no owner is open to everybody. An inventory is checked by its effective owner
    /// (`effective_owner`), or for one that does not exist yet, by the owner its id implies; a shared one is open.
    pub fn statements(self, caller: u64) -> Vec<(String, Value)> {
        let me = caller.to_string();
        let recount = self.recount();
        let mut targets: Vec<Value> = Vec::new();
        for id in &self.inventories {
            if !targets.iter().any(|target| target["id"] == json!(id)) {
                targets.push(access_target(id));
            }
        }
        let owner = effective_owner("start", "direct");
        let mut statements = vec![
            (
                "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key IN @keys AND e.ownerId NOT IN ['0', @me]
                 LIMIT 1 RETURN 1) == 0, 'precondition: entity of another owner')"
                    .to_string(),
                json!({ "keys": self.entities, "me": me }),
            ),
            (
                format!(
                    "WITH entities, inventories
                     LET current = (FOR e IN entities FILTER e._key IN @sources AND e.stowed == true
                                    AND e.inventoryId != null RETURN {{ id: e.inventoryId, owner: '0', holder: '0' }})
                     RETURN ASSERT(LENGTH(
                         FOR t IN APPEND(@targets, current)
                             LET i = FIRST(FOR i IN inventories FILTER i.id == t.id RETURN i)
                             FILTER i == null OR i.shared != true
                             LET start = i != null ? i._id : CONCAT('entities/', t.holder)
                             LET direct = i != null ? i.ownerId : t.owner
                             LET owner = {owner}
                             FILTER owner NOT IN ['0', @me]
                             LIMIT 1
                             RETURN 1
                     ) == 0, 'precondition: inventory of another owner')"
                ),
                json!({ "targets": targets, "sources": self.sources, "me": me }),
            ),
            (
                "RETURN ASSERT(LENGTH(FOR r IN lt_records FILTER r._key IN @keys AND r.ownerId NOT IN ['0', @me]
                 LIMIT 1 RETURN 1) == 0, 'precondition: record of another owner')"
                    .to_string(),
                json!({ "keys": self.records, "me": me }),
            ),
        ];
        statements.extend(self.queries);
        statements.push(recount);
        statements
    }

    pub fn command(&mut self, command: &EntityGraphCommand) -> Result<(), Status> {
        let Some(params) = &command.params else {
            return Err(Status::invalid_argument("command without parameters"));
        };
        match params {
            Params::Create(c) => self.create(c),
            Params::Erase(c) => self.erase(c)?,
            Params::Attach(c) => self.attach(c),
            Params::Detach(c) => {
                for body in &c.detaches {
                    self.touches(body.entity_id);
                    let bind = json!({ "key": attached_edge_key(body.entity_id) });
                    self.push("REMOVE @key IN entity_edges OPTIONS { ignoreErrors: true }", bind);
                }
            }
            Params::Transfer(c) => {
                for body in &c.transfers {
                    self.transfer(body);
                }
            }
            Params::Move(c) => self.moves(c),
            Params::AddLabels(c) => {
                for body in &c.updates {
                    self.patch(body.entity_id, "{ labels: UNION_DISTINCT(e.labels, @items) }", json!({ "items": body.labels }));
                }
            }
            Params::RemoveLabels(c) => {
                for body in &c.updates {
                    self.patch(body.entity_id, "{ labels: MINUS(e.labels, @items) }", json!({ "items": body.labels }));
                }
            }
            Params::ChangeFlags(c) => {
                for body in &c.changes {
                    let bind = json!({ "fields": {
                        "entityFlags": body.flags.to_string(),
                        "entityFlagsLow": body.flags & 0xffff_ffff,
                        "gameFlags": body.game_flags.to_string(),
                        "gameFlagsLow": body.game_flags & 0xffff_ffff,
                    }});
                    self.patch(body.entity_id, "@fields", bind);
                }
            }
            Params::ChangeLocation(c) => {
                for body in &c.changes {
                    let mut fields = Map::new();
                    if let Some(q) = body.orientation {
                        fields.insert("orientation".into(), json!({ "x": q.x, "y": q.y, "z": q.z, "w": q.w }));
                    }
                    if let Some(p) = body.position {
                        fields.insert("position".into(), json!({ "x": p.x, "y": p.y, "z": p.z }));
                    }
                    if body.location_id != 0 {
                        fields.insert("locationId".into(), json!(body.location_id));
                    }
                    if body.radius != 0.0 {
                        fields.insert("radius".into(), json!(body.radius));
                    }
                    if body.scale != 0.0 {
                        fields.insert("scale".into(), json!(body.scale));
                    }
                    self.patch(body.entity_id, "@fields", json!({ "fields": fields }));
                }
            }
            Params::SetSnapshot(c) => {
                for snapshot in c.snapshots.iter().filter_map(|body| body.snapshot.as_ref()) {
                    self.set_snapshot(snapshot);
                }
            }
            Params::ChangeSnapshot(c) => {
                for snapshot in c.updates.iter().filter_map(|body| body.snapshot.as_ref()) {
                    self.change_snapshot(snapshot);
                }
            }
            Params::RemoveSnapshot(c) => {
                for body in &c.removals {
                    self.touches(body.entity_id);
                    let bind = json!({ "key": body.entity_id.to_string() });
                    self.push("REMOVE @key IN entity_snapshots OPTIONS { ignoreErrors: true }", bind);
                }
            }
            Params::Stack(c) => {
                for body in &c.stacks {
                    self.patch(body.entity_id, "{ stackSize: e.stackSize + @amount }", json!({ "amount": body.amount }));
                    self.mark_holding(body.entity_id);
                }
            }
            Params::Unstack(c) => {
                for body in &c.unstacks {
                    self.patch(body.entity_id, "{ stackSize: MAX([0, e.stackSize - @amount]) }", json!({ "amount": body.amount }));
                    self.mark_holding(body.entity_id);
                }
            }
            Params::Setstack(c) => {
                for body in &c.changes {
                    self.patch(body.entity_id, "{ stackSize: @amount }", json!({ "amount": body.amount }));
                    self.mark_holding(body.entity_id);
                }
            }
            Params::ChangeRank(c) => {
                for body in &c.changes {
                    self.touches(body.entity_id);
                    let bind = json!({ "key": stowed_edge_key(body.entity_id), "rank": body.rank });
                    self.push("FOR x IN entity_edges FILTER x._key == @key UPDATE x WITH { rank: @rank } IN entity_edges", bind);
                }
            }
            Params::Activate(c) => {
                for geid in c.activations.iter().flat_map(|body| &body.geids) {
                    self.patch(*geid, "{ active: true }", json!({}));
                }
            }
            Params::Deactivate(c) => {
                for geid in c.deactivations.iter().flat_map(|body| &body.geids) {
                    self.patch(*geid, "{ active: false }", json!({}));
                }
            }
            Params::AddTags(c) => {
                for body in &c.updates {
                    self.patch(body.entity_id, "{ tags: UNION_DISTINCT(e.tags, @items) }", json!({ "items": body.tags }));
                }
            }
            Params::RemoveTags(c) => {
                for body in &c.updates {
                    self.patch(body.entity_id, "{ tags: MINUS(e.tags, @items) }", json!({ "items": body.tags }));
                }
            }
            Params::SetTags(c) => {
                for body in &c.updates {
                    self.patch(body.entity_id, "{ tags: @items }", json!({ "items": body.tags }));
                }
            }
            Params::CreateInventory(c) => self.create_inventories(c),
            Params::EraseInventory(c) => {
                for body in &c.erasures {
                    self.touches_inventory(&body.id);
                    self.erase_tree(&inventory_handle(&body.id));
                }
            }
            Params::ChangeInventoryConfig(c) => self.change_inventory_config(c),
            Params::Sync(c) => self.sync(c),
            Params::CreateLtRecord(c) => {
                for record in c.creates.iter().filter_map(|body| body.record.as_ref()) {
                    self.records.push(key(&record.urn));
                    let persistent = record.persistent_properties.as_ref().map(|p| doc::hex(&p.encode_to_vec()));
                    let bind = json!({ "doc": {
                        "_key": key(&record.urn),
                        "urn": record.urn,
                        "classGuidCrc": record.class_guid_crc,
                        "stackSize": record.stack_size,
                        "inventoryId": record.inventory_id,
                        "status": record.status,
                        "ownerId": record.owner_id.to_string(),
                        "persistentProperties": persistent,
                    }});
                    self.push(format!("INSERT @doc INTO lt_records {OVERWRITE}"), bind);
                }
            }
            Params::DeleteLtRecord(c) => {
                let keys: Vec<String> = c.urns.iter().map(|urn| key(urn)).collect();
                self.records.extend(keys.iter().cloned());
                self.push("FOR k IN @keys REMOVE k IN lt_records OPTIONS { ignoreErrors: true }", json!({ "keys": keys }));
            }
            Params::MoveAll(c) => self.move_all(c),
            Params::CreateStowContext(c) => {
                for context in c.creates.iter().filter_map(|body| body.context.as_ref()) {
                    self.create_stow_context(context);
                }
            }
            Params::UpdateParentUrn(c) => {
                for body in &c.updates {
                    self.patch(body.entity_geid, "{ parentUrn: @urn }", json!({ "urn": body.new_parent_urn }));
                }
            }
        }
        Ok(())
    }

    /// Creates the graphs. Every created entity whose class has an inventory container (a crate, a backpack, a
    /// ship's cargo grid) gets its container inventory. With an inventory id, the entities of a graph that nothing
    /// in it holds are stowed there.
    fn create(&mut self, command: &CreateCommand) {
        let inventory_id = (!command.inventory_id.is_empty()).then(|| command.inventory_id.clone());
        let stow = Stow { inventory_id, shard_id: self.stow.shard_id.clone() };
        let stow = if stow.inventory_id.is_some() { Stow { shard_id: None, ..stow } } else { stow };
        let mut created = Vec::new();
        for graph in command.creates.iter().filter_map(|body| body.graphs.as_ref()) {
            let mut roots = Vec::new();
            for properties in graph.nodes.iter().filter_map(|node| node.properties.as_ref()?.properties.as_ref()) {
                match properties {
                    node_properties::Properties::EntityProperties(entity) => {
                        self.touches(entity.geid);
                        let doc = doc::entity_doc(entity, &stow, self.now);
                        self.push(format!("INSERT @doc INTO entities {OVERWRITE}"), json!({ "doc": doc }));
                        created.push(entity.geid);
                        if !graph.edges.iter().any(|edge| starts_at(edge, entity.geid)) {
                            roots.push(entity.geid);
                        }
                    }
                    node_properties::Properties::InventoryProperties(inventory) => {
                        self.touches_inventory(&inventory.id);
                        self.insert_inventory(inventory, doc::inventory_holder(&inventory.id), OVERWRITE);
                    }
                }
            }
            for edge in &graph.edges {
                self.edge(edge);
            }
            if let Some(id) = &stow.inventory_id {
                for geid in roots {
                    self.edge(&EntityEdge {
                        r#type: EntityEdgeType::StowedIn as i32,
                        start: Some(entity_vertex(geid)),
                        end: Some(inventory_vertex(id)),
                        properties: Some(EntityEdgeProperties::default()),
                    });
                }
            }
        }
        if !created.is_empty() {
            let wanted: Vec<(u64, Value, Value)> = created
                .iter()
                .filter_map(|geid| {
                    let id = doc::container_inventory_id(*geid);
                    let mut inventory = doc::implied_inventory_doc(&id, self.now)?;
                    doc::merge(&mut inventory, json!({ "dirty": true }));
                    Some((*geid, inventory, doc::holder_edge(&id, *geid)))
                })
                .collect();
            self.containers(&wanted, true);
        }
    }

    fn edge(&mut self, edge: &EntityEdge) {
        use sc_proto::services::entitygraph::v1::entity_edge_vertex::Id;
        let start = edge.start.as_ref().and_then(|vertex| vertex.id.as_ref());
        let end = edge.end.as_ref().and_then(|vertex| vertex.id.as_ref());
        match (start, end) {
            (Some(Id::EntityId(geid)), Some(Id::ShardId(shard))) => {
                self.patch(*geid, "{ stowed: false, inventoryId: null, shardId: @shard }", json!({ "shard": shard }));
            }
            (Some(Id::EntityId(geid)), Some(Id::InventoryId(id))) if edge.r#type == EntityEdgeType::StowedIn as i32 => {
                if let Some(doc) = doc::edge_doc(edge) {
                    self.sources.push(geid.to_string());
                    self.ensure_inventory(id);
                    self.refuse_cycle(&entity_id(*geid), id);
                    self.release(*geid);
                    self.push(format!("INSERT @doc INTO entity_edges {OVERWRITE}"), json!({ "doc": doc }));
                    self.claim(*geid, id);
                    self.patch(*geid, "{ stowed: true, inventoryId: @id, shardId: null }", json!({ "id": id }));
                }
            }
            _ => {
                if let Some(doc) = doc::edge_doc(edge) {
                    self.push(format!("INSERT @doc INTO entity_edges {OVERWRITE}"), json!({ "doc": doc }));
                }
            }
        }
    }

    fn insert_inventory(&mut self, properties: &InventoryNodeProperties, holder: u64, options: &str) {
        let mut doc = doc::inventory_doc(properties, holder, self.now);
        doc::merge(&mut doc, json!({ "dirty": true }));
        self.push(format!("INSERT @doc INTO inventories {options}"), json!({ "doc": doc }));
        if holder != 0 {
            let edge = doc::holder_edge(&properties.id, holder);
            self.push(format!("INSERT @doc INTO entity_edges {OVERWRITE}"), json!({ "doc": edge }));
        }
    }

    fn create_inventories(&mut self, command: &CreateInventoryCommand) {
        let options = if command.ignore_already_exists_error { KEEP_EXISTING } else { "" };
        for body in &command.creates {
            let mut properties = body.properties.clone().unwrap_or_default();
            if properties.owner_id == 0 {
                properties.owner_id = body.player_id;
            }
            if properties.id.is_empty() {
                properties.id = format!("{}:{}:{}", body.reference, properties.context, properties.subject_id);
            }
            let holder = if body.reference != 0 { body.reference } else { doc::inventory_holder(&properties.id) };
            self.touches_inventory(&properties.id);
            self.insert_inventory(&properties, holder, options);
        }
    }

    fn lazy_inventory(&mut self, id: &str, arguments: Option<&CreateInventoryArguments>) {
        let Some(arguments) = arguments else {
            return;
        };
        let mut properties = arguments.properties.clone().unwrap_or_default();
        properties.id = id.to_string();
        let holder = if arguments.reference != 0 { arguments.reference } else { doc::inventory_holder(id) };
        self.insert_inventory(&properties, holder, KEEP_EXISTING);
    }

    fn change_inventory_config(&mut self, command: &ChangeInventoryConfigurationCommand) {
        for body in &command.change_configs {
            self.touches_inventory(&body.id);
            let physical = body.config.as_ref().and_then(doc::physical_configuration);
            let bind = json!({ "key": key(&body.id), "config": doc::physical_doc(physical.as_ref()), "now": self.now });
            // The occupancy is counted again at the end; a caller's figure for it is not kept.
            self.push(
                "FOR i IN inventories FILTER i._key == @key
                 UPDATE i WITH { configuration: @config, updatedAt: @now, dirty: true } IN inventories OPTIONS { mergeObjects: false }",
                bind,
            );
        }
    }

    /// Marks the inventory the entity's stowed edge points to, which it is about to leave.
    fn release(&mut self, geid: u64) {
        self.push(
            "FOR x IN entity_edges FILTER x._key == @edge
                 FOR i IN inventories FILTER i._id == x._to
                     UPDATE i WITH { dirty: true } IN inventories",
            json!({ "edge": stowed_edge_key(geid) }),
        );
    }

    /// Marks `inventory_id`, which the entity now occupies; the count at the end refuses to overfill it.
    fn claim(&mut self, _geid: u64, inventory_id: &str) {
        // Stowing into an inventory needs the same right as taking from one.
        self.touches_inventory(inventory_id);
        self.mark(inventory_id);
    }

    /// A stowed item is taken out of its inventory and attached to `parent`'s port `port_crc`. What hangs from the item
    /// goes with it. The item's inventory is checked against the caller, as for a move.
    pub fn equip(&mut self, geid: u64, parent: u64, port_crc: u32) {
        self.touches(geid);
        self.touches(parent);
        self.sources.push(geid.to_string());
        self.release(geid);
        self.push("REMOVE @edge IN entity_edges OPTIONS { ignoreErrors: true }", json!({ "edge": stowed_edge_key(geid) }));
        self.edge(&EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(entity_vertex(geid)),
            end: Some(entity_vertex(parent)),
            properties: Some(EntityEdgeProperties {
                attachment_type: AttachmentType::ItemPort as i32,
                attachment_port_crc: port_crc,
                ..Default::default()
            }),
        });
        self.patch(
            geid,
            "{ stowed: false, inventoryId: null, shardId: null, unstowedFromInventoryId: NOT_NULL(e.inventoryId, '') }",
            json!({}),
        );
    }

    /// A worn item is detached from its parent and stowed in `inventory_id`, created on first need as for a move. Refused
    /// when it is not attached to anything, or the inventory is inside the item.
    pub fn unequip(&mut self, geid: u64, inventory_id: &str) {
        self.touches(geid);
        self.ensure_inventory(inventory_id);
        self.refuse_cycle(&entity_id(geid), inventory_id);
        self.push(
            "RETURN ASSERT(LENGTH(FOR x IN entity_edges FILTER x._key == @edge LIMIT 1 RETURN 1) == 1
                           AND LENGTH(FOR i IN inventories FILTER i._key == @to LIMIT 1 RETURN 1) == 1,
                           'precondition: the item is not worn, or the target inventory does not exist')",
            json!({ "edge": attached_edge_key(geid), "to": key(inventory_id) }),
        );
        self.push("REMOVE @edge IN entity_edges OPTIONS { ignoreErrors: true }", json!({ "edge": attached_edge_key(geid) }));
        if let Some(doc) = stowed_edge(geid, inventory_id) {
            self.push(format!("INSERT @doc INTO entity_edges {OVERWRITE}"), json!({ "doc": doc }));
        }
        self.claim(geid, inventory_id);
        self.patch(geid, "{ stowed: true, inventoryId: @to, shardId: null, moving: false }", json!({ "to": inventory_id }));
    }

    /// A worn item moves to another port: its attached edge (one per child) is replaced. What hangs from it goes with
    /// it. Refused when the item is not worn.
    pub fn reattach(&mut self, geid: u64, parent: u64, port_crc: u32) {
        self.touches(geid);
        self.touches(parent);
        self.push(
            "RETURN ASSERT(LENGTH(FOR x IN entity_edges FILTER x._key == @edge LIMIT 1 RETURN 1) == 1,
                           'precondition: the item is not worn')",
            json!({ "edge": attached_edge_key(geid) }),
        );
        self.edge(&EntityEdge {
            r#type: EntityEdgeType::AttachedTo as i32,
            start: Some(entity_vertex(geid)),
            end: Some(entity_vertex(parent)),
            properties: Some(EntityEdgeProperties {
                attachment_type: AttachmentType::ItemPort as i32,
                attachment_port_crc: port_crc,
                ..Default::default()
            }),
        });
    }

    /// A stowed item leaves its inventory and is live on `shard_id` (the DGS has it), as a dropped item is.
    pub fn unstow(&mut self, geid: u64, shard_id: &str) {
        self.touches(geid);
        self.sources.push(geid.to_string());
        self.release(geid);
        self.push("REMOVE @edge IN entity_edges OPTIONS { ignoreErrors: true }", json!({ "edge": stowed_edge_key(geid) }));
        self.patch(
            geid,
            "{ stowed: false, unstowedFromInventoryId: NOT_NULL(e.inventoryId, ''), inventoryId: null, shardId: @shard }",
            json!({ "shard": shard_id }),
        );
    }

    /// An item lies in the world: nobody owns it, in `zone` at `position` and `orientation` (`x y z w`), dropped by
    /// `dropped_by`.
    pub fn place_in_world(&mut self, geid: u64, zone: u64, position: [f64; 3], orientation: [f64; 4], dropped_by: u64) {
        let [x, y, z] = position;
        let [qx, qy, qz, qw] = orientation;
        self.patch(
            geid,
            "{ ownerId: '0', droppedBy: @by, zoneId: @zone, position: { x: @x, y: @y, z: @z }, orientation: { x: @qx, y: @qy, z: @qz, w: @qw } }",
            json!({ "by": dropped_by.to_string(), "zone": zone.to_string(), "x": x, "y": y, "z": z, "qx": qx, "qy": qy, "qz": qz, "qw": qw }),
        );
    }

    /// An item lying in the world is stowed in `inventory_id` (created on first need as for a move) and owned by `owner`.
    pub fn pick_up(&mut self, geid: u64, inventory_id: &str, owner: u64) {
        self.touches(geid);
        self.ensure_inventory(inventory_id);
        self.refuse_cycle(&entity_id(geid), inventory_id);
        self.push(
            "RETURN ASSERT(LENGTH(FOR i IN inventories FILTER i._key == @to LIMIT 1 RETURN 1) == 1,
                           'precondition: the target inventory does not exist')",
            json!({ "to": key(inventory_id) }),
        );
        if let Some(doc) = stowed_edge(geid, inventory_id) {
            self.push(format!("INSERT @doc INTO entity_edges {OVERWRITE}"), json!({ "doc": doc }));
        }
        self.claim(geid, inventory_id);
        self.patch(
            geid,
            "{ stowed: true, inventoryId: @to, shardId: null, ownerId: @owner, droppedBy: null, zoneId: null, moving: false }",
            json!({ "to": inventory_id, "owner": owner.to_string() }),
        );
    }

    /// The rounds in a magazine.
    pub fn set_ammo(&mut self, geid: u64, rounds: u32) {
        self.patch(geid, "{ ammoCount: @rounds }", json!({ "rounds": rounds }));
    }

    pub fn moves(&mut self, command: &MoveCommand) {
        for body in &command.moves {
            self.touches(body.entity_id);
            self.sources.push(body.entity_id.to_string());
            if body.target_inventory_lazy_creation.is_some() {
                self.lazy_inventory(&body.to_inventory_id, body.target_inventory_lazy_creation.as_ref());
            } else {
                self.ensure_inventory(&body.to_inventory_id);
            }
            self.refuse_cycle(&entity_id(body.entity_id), &body.to_inventory_id);
            self.push(
                "RETURN ASSERT(
                     LENGTH(FOR e IN entities FILTER e._key == @key AND e.stowed == true
                            AND (@from == '' OR e.inventoryId == @from) LIMIT 1 RETURN 1) == 1
                     AND LENGTH(FOR i IN inventories FILTER i._key == @to LIMIT 1 RETURN 1) == 1,
                     'precondition: entity is not in the source inventory, or the target does not exist')",
                json!({
                    "key": body.entity_id.to_string(),
                    "from": body.from_inventory_id,
                    "to": key(&body.to_inventory_id),
                }),
            );
            self.release(body.entity_id);
            self.push(
                format!(
                    "LET old = FIRST(FOR x IN entity_edges FILTER x._key == @edge RETURN x)
                     INSERT MERGE(@doc, {{
                         rank: @rank != '' ? @rank : NOT_NULL(old.rank, ''),
                         physical: old.physical
                     }}) INTO entity_edges {OVERWRITE}"
                ),
                json!({
                    "edge": stowed_edge_key(body.entity_id),
                    "doc": stowed_edge(body.entity_id, &body.to_inventory_id),
                    "rank": body.rank,
                }),
            );
            self.claim(body.entity_id, &body.to_inventory_id);
            self.patch(
                body.entity_id,
                "{ stowed: true, inventoryId: @to, shardId: null, moving: false }",
                json!({ "to": body.to_inventory_id }),
            );
        }
    }

    fn move_all(&mut self, command: &MoveAllCommand) {
        for body in &command.moves {
            self.touches_inventory(&body.from_inventory_id);
            self.touches_inventory(&body.to_inventory_id);
            if body.target_inventory_lazy_creation.is_some() {
                self.lazy_inventory(&body.to_inventory_id, body.target_inventory_lazy_creation.as_ref());
            } else {
                self.ensure_inventory(&body.to_inventory_id);
            }
            self.push(
                "RETURN ASSERT(@from != @to, 'precondition: an inventory cannot be moved into itself')",
                json!({ "from": body.from_inventory_id, "to": body.to_inventory_id }),
            );
            self.refuse_cycle(&inventory_handle(&body.from_inventory_id), &body.to_inventory_id);
            self.mark(&body.from_inventory_id);
            self.mark(&body.to_inventory_id);
            self.push(
                "FOR x IN entity_edges FILTER x._to == @from AND x.type == @kind
                     UPDATE x WITH { _to: @to, endId: @target } IN entity_edges",
                json!({
                    "from": inventory_handle(&body.from_inventory_id),
                    "to": inventory_handle(&body.to_inventory_id),
                    "target": body.to_inventory_id,
                    "kind": EntityEdgeType::StowedIn as i32,
                }),
            );
            self.push(
                "FOR e IN entities FILTER e.stowed == true AND e.inventoryId == @from
                     UPDATE e WITH { inventoryId: @to, updatedAt: @now, version: e.version + 1 } IN entities",
                json!({ "from": body.from_inventory_id, "to": body.to_inventory_id, "now": self.now }),
            );
        }
    }

    fn erase(&mut self, command: &EraseCommand) -> Result<(), Status> {
        for body in &command.erasures {
            self.touches(body.entity_id);
            for condition in &body.preconditions {
                let mut binds = Binds::default();
                let check = filter::precondition(condition, "e", &mut binds)?;
                let entity = binds.add(body.entity_id.to_string());
                let query = format!(
                    "RETURN ASSERT(LENGTH(FOR e IN entities FILTER e._key == {entity} AND ({check}) LIMIT 1 RETURN 1) == 1,
                     'precondition: erase condition not met')"
                );
                self.push(query, binds.into_value());
            }
            self.release(body.entity_id);
            self.erase_tree(&entity_id(body.entity_id));
        }
        Ok(())
    }

    /// Removes a node and everything it holds. The edges go last, because they are what finds the rest.
    fn erase_tree(&mut self, handle: &str) {
        let ids = below("@start", "x._from");
        let bind = json!({ "start": handle });
        self.push(
            format!(
                "{ids}
                 FOR id IN ids FILTER IS_SAME_COLLECTION('entities', id)
                     REMOVE PARSE_IDENTIFIER(id).key IN entity_snapshots OPTIONS {{ ignoreErrors: true }}"
            ),
            bind.clone(),
        );
        self.push(
            format!(
                "{ids}
                 FOR id IN ids FILTER IS_SAME_COLLECTION('entities', id)
                     REMOVE PARSE_IDENTIFIER(id).key IN entities OPTIONS {{ ignoreErrors: true }}"
            ),
            bind.clone(),
        );
        self.push(
            format!(
                "{ids}
                 FOR id IN ids FILTER IS_SAME_COLLECTION('inventories', id)
                     REMOVE PARSE_IDENTIFIER(id).key IN inventories OPTIONS {{ ignoreErrors: true }}"
            ),
            bind.clone(),
        );
        self.push(
            format!(
                "{ids}
                 FOR x IN entity_edges FILTER x._from IN ids OR x._to IN ids
                     REMOVE x IN entity_edges"
            ),
            bind,
        );
    }

    fn attach(&mut self, command: &AttachCommand) {
        use sc_proto::services::entitygraph::v1::EntityEdgeProperties as Properties;
        for body in &command.attachments {
            self.touches(body.entity_id);
            let edge = EntityEdge {
                r#type: EntityEdgeType::AttachedTo as i32,
                start: Some(entity_vertex(body.entity_id)),
                end: Some(entity_vertex(body.parent_id)),
                properties: Some(Properties {
                    attachment_type: body.attachment_type,
                    attachment_port_crc: body.attachment_port as u32,
                    ..Default::default()
                }),
            };
            self.edge(&edge);
            if matches!(body.ownership(), AttachmentOwnershipBehavior::Inherit | AttachmentOwnershipBehavior::Adopt) {
                let bind = json!({ "key": body.entity_id.to_string(), "parent": body.parent_id.to_string(), "now": self.now });
                self.push(
                    "FOR p IN entities FILTER p._key == @parent
                         FOR e IN entities FILTER e._key == @key
                             UPDATE e WITH { ownerId: p.ownerId, updatedAt: @now, version: e.version + 1 } IN entities",
                    bind,
                );
            }
        }
    }

    fn transfer(&mut self, body: &sc_proto::services::entitygraph::v1::transfer_command::Body) {
        let owner = match body.owner {
            _ if body.clear => 0,
            Some(transfer_command::body::Owner::PlayerId(id)) => id,
            Some(transfer_command::body::Owner::OrganizationId(id)) => id,
            None => 0,
        };
        self.touches(body.entity_id);
        if !body.include_children {
            self.patch(body.entity_id, "{ ownerId: @owner }", json!({ "owner": owner.to_string() }));
            return;
        }
        let ids = below("@start", "v._id");
        self.push(
            format!(
                "{ids}
                 FOR e IN entities FILTER e._id IN ids
                     UPDATE e WITH {{ ownerId: @owner, updatedAt: @now, version: e.version + 1 }} IN entities"
            ),
            json!({ "start": entity_id(body.entity_id), "owner": owner.to_string(), "now": self.now }),
        );
    }

    fn set_snapshot(&mut self, snapshot: &EntitySnapshot) {
        self.touches(snapshot.entity_id);
        let doc = json!({
            "_key": snapshot.entity_id.to_string(),
            "variables": doc::snapshot_variables(snapshot),
            "version": snapshot.version,
        });
        self.push(format!("INSERT @doc INTO entity_snapshots {OVERWRITE}"), json!({ "doc": doc }));
    }

    /// Replaces the variables the change names and keeps the others.
    fn change_snapshot(&mut self, snapshot: &EntitySnapshot) {
        self.touches(snapshot.entity_id);
        let names: Vec<u32> = snapshot.variables.iter().map(|variable| variable.name_crc).collect();
        self.push(
            format!(
                "LET old = FIRST(FOR s IN entity_snapshots FILTER s._key == @key RETURN s)
                 LET kept = (FOR v IN NOT_NULL(old.variables, []) FILTER v.nameCrc NOT IN @names RETURN v)
                 INSERT {{ _key: @key, variables: APPEND(kept, @variables), version: @version }}
                 INTO entity_snapshots {OVERWRITE}"
            ),
            json!({
                "key": snapshot.entity_id.to_string(),
                "names": names,
                "variables": doc::snapshot_variables(snapshot),
                "version": snapshot.version,
            }),
        );
    }

    fn sync(&mut self, command: &SyncCommand) {
        for body in &command.sync_tasks {
            let mut fields = Map::new();
            if let Some(properties) = &body.entity_properties {
                fields.insert("entityFlags".into(), json!(properties.entity_flags.to_string()));
                fields.insert("entityFlagsLow".into(), json!(properties.entity_flags & 0xffff_ffff));
                if let Some(q) = properties.orientation {
                    fields.insert("orientation".into(), json!({ "x": q.x, "y": q.y, "z": q.z, "w": q.w }));
                }
                if let Some(p) = properties.position {
                    fields.insert("position".into(), json!({ "x": p.x, "y": p.y, "z": p.z }));
                }
                fields.insert("scale".into(), json!(properties.scale));
                fields.insert("radius".into(), json!(properties.radius));
                fields.insert("tags".into(), json!(properties.tags));
                if properties.owner_id != 0 {
                    fields.insert("ownerId".into(), json!(properties.owner_id.to_string()));
                }
                if properties.ownership_status != 0 {
                    fields.insert("ownershipStatus".into(), json!(properties.ownership_status));
                }
                if let Some(time) = &properties.ownership_timestamp {
                    fields.insert("ownershipTimestamp".into(), doc::timestamp_doc(time));
                }
            }
            self.patch(body.entity_id, "@fields", json!({ "fields": fields }));
            if let Some(snapshot) = &body.snapshot {
                self.change_snapshot(snapshot);
            }
            if let Some(edge) = &body.attachment_edge {
                self.edge(edge);
            }
        }
    }

    fn create_stow_context(&mut self, context: &sc_proto::services::entitygraph::v1::EntityStowContext) {
        use sc_proto::services::entitygraph::v1::EntityNodeProperties;
        self.touches(context.entity_id);
        let fields = json!({
            "stowed": context.is_stowed,
            "inventoryId": context.inventory_id,
            "shardId": context.shard_id,
            "parentUrn": context.parent_urn,
            "ownerId": context.owner_id.to_string(),
            "classGuidCrc": context.class_guid_crc,
            "unstowedFromInventoryId": context.unstowed_from_inventory_id,
            "moving": context.moving.unwrap_or(false),
        });
        let properties = EntityNodeProperties { geid: context.entity_id, scale: 1.0, ..Default::default() };
        let mut created = doc::entity_doc(&properties, &Stow::default(), self.now);
        doc::merge(&mut created, fields.clone());
        self.push(
            "UPSERT { _key: @key } INSERT @created UPDATE @fields IN entities",
            json!({ "key": context.entity_id.to_string(), "created": created, "fields": fields }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_proto::services::entitygraph::v1::{move_command, stack_command, StackCommand};

    fn plan() -> Plan {
        Plan::new(None, 5)
    }

    #[test]
    fn every_statement_list_starts_with_the_ownership_checks() {
        let mut plan = plan();
        let stack = StackCommand { stacks: vec![stack_command::Body { entity_id: 9, amount: 2 }], ..Default::default() };
        plan.command(&EntityGraphCommand { params: Some(Params::Stack(stack)) }).unwrap();
        let statements = plan.statements(7);
        assert_eq!(statements.len(), 6);
        assert!(statements[0].0.contains("entity of another owner"));
        assert_eq!(statements[0].1["keys"], json!(["9"]));
        assert_eq!(statements[0].1["me"], json!("7"));
        assert_eq!(statements[3].1["amount"], json!(2));
        assert!(statements[4].0.contains("dirty: true"), "a stack change marks its inventory");
        assert!(statements[5].0.contains("inventory is full"), "the count comes last");
    }

    #[test]
    fn stowing_checks_who_owns_the_target_inventory() {
        let mut plan = plan();
        plan.claim(1, "someone:else");
        assert_eq!(plan.inventories, ["someone:else"]);
    }

    #[test]
    fn a_move_checks_the_source_before_it_changes_anything() {
        let body = move_command::Body {
            entity_id: 9,
            from_inventory_id: "1:Container:0".into(),
            to_inventory_id: "2:Container:0".into(),
            ..Default::default()
        };
        let mut plan = plan();
        plan.moves(&MoveCommand { moves: vec![body] });
        let statements = plan.statements(7);
        let source = statements.iter().position(|(query, _)| query.contains("not in the source inventory")).unwrap();
        let cycle = statements.iter().position(|(query, _)| query.contains("inside itself")).unwrap();
        let insert = statements.iter().position(|(query, _)| query.contains("INTO entity_edges {OVERWRITE}".replace("{OVERWRITE}", OVERWRITE).as_str())).unwrap();
        assert!(source > 2 && cycle > 2 && insert > source && insert > cycle);
        assert_eq!(statements[1].1["sources"], json!(["9"]));
        assert_eq!(statements[insert].1["doc"]["_to"], json!("inventories/2:Container:0"));
    }

    #[test]
    fn an_empty_command_is_refused() {
        let error = plan().command(&EntityGraphCommand::default()).unwrap_err();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn creating_into_an_inventory_stows_instead_of_placing_on_the_shard() {
        use sc_proto::services::entitygraph::v1::{
            create_command, EntityGraph, EntityNodeProperties, Node, NodeProperties, Scope,
        };
        let arguments = MutationArguments {
            scope: Some(Scope { r#type: ScopeType::Shard as i32, shard_id: "s1".into() }),
            ..Default::default()
        };
        let node = Node {
            properties: Some(NodeProperties {
                properties: Some(node_properties::Properties::EntityProperties(EntityNodeProperties {
                    geid: 9,
                    ..Default::default()
                })),
            }),
            r#type: NodeType::Entity as i32,
        };
        let graph = EntityGraph { nodes: vec![node], edges: Vec::new() };
        let bodies = vec![create_command::Body { graphs: Some(graph) }];
        let created = |inventory_id: &str| {
            let mut plan = Plan::new(Some(&arguments), 5);
            let command = CreateCommand { creates: bodies.clone(), inventory_id: inventory_id.into() };
            plan.command(&EntityGraphCommand { params: Some(Params::Create(command)) }).unwrap();
            let statements = plan.statements(7);
            statements.into_iter().find(|(query, _)| query.starts_with("INSERT @doc INTO entities")).unwrap().1
        };
        assert_eq!(created("")["doc"]["shardId"], json!("s1"));
        let stowed = created("1:Container:0");
        assert_eq!(stowed["doc"]["inventoryId"], json!("1:Container:0"));
        assert_eq!(stowed["doc"]["shardId"], Value::Null);
    }
}

//! The custom resources: ShardRegion, ShardCluster, ShardTemplate and Shard.

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_warm_shards() -> u32 {
    1
}

fn default_scale_down_delay() -> u32 {
    300
}

fn default_shard_template() -> String {
    "default".to_string()
}

/// A pool of shards serving one region. The operator opens and closes shards as demand changes.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "scbackend.io",
    version = "v1alpha1",
    kind = "ShardCluster",
    namespaced,
    status = "ShardClusterStatus",
    shortname = "shc",
    printcolumn = r#"{"name":"Region","type":"string","jsonPath":".spec.region"}"#,
    printcolumn = r#"{"name":"Shards","type":"integer","jsonPath":".status.readyShards"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredShards"}"#,
    printcolumn = r#"{"name":"Players","type":"integer","jsonPath":".status.players"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ShardClusterSpec {
    /// Region this cluster serves. The Hub allocates by region, and it names the ShardRegion whose game data the
    /// shards use.
    pub region: String,
    /// PU frontend image.
    pub image: String,
    /// Game build these shards serve. It is part of each shard id.
    pub build: String,
    /// Address clients use to reach the shards.
    pub public_address: String,
    /// First public UDP port. Shard N uses this port plus N.
    pub public_port_base: u16,
    /// First Kubernetes node port. Shard N uses this port plus N. Clusters need separate ranges.
    pub node_port_base: u16,
    /// Empty shards kept ready beyond what current players need.
    #[serde(default = "default_warm_shards")]
    pub warm_shards: u32,
    /// Upper limit on shards.
    pub max_shards: u32,
    /// Players a shard takes before another is needed.
    pub players_per_shard: u32,
    /// How long a surplus shard must stay empty before it closes.
    #[serde(default = "default_scale_down_delay")]
    pub scale_down_delay_seconds: u32,
    /// ShardTemplate new shards use. A running shard keeps the one it opened with.
    #[serde(default = "default_shard_template")]
    pub shard_template: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShardClusterStatus {
    pub ready_shards: u32,
    pub desired_shards: u32,
    pub players: u32,
    pub shards: Vec<ShardSummary>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShardSummary {
    /// Random name of the shard's pod and service.
    pub name: String,
    /// `pub_<shard cluster>_<build>_<shard number>`, the id the game sees.
    pub id: String,
    pub public_port: u16,
    pub ready: bool,
    pub players: u32,
}

/// How a shard divides the universe between its dedicated game servers.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(group = "scbackend.io", version = "v1alpha1", kind = "ShardTemplate", namespaced, shortname = "sht")]
#[serde(rename_all = "camelCase")]
pub struct ShardTemplateSpec {
    pub systems: Vec<SystemLayout>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemLayout {
    /// `stanton`, `pyro` or `nyx`.
    pub name: String,
    pub dgs: Vec<DgsLayout>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DgsLayout {
    /// Unique in the template, and part of the pod name.
    pub name: String,
    /// Containers by name, such as `stanton1`: all of that name in the system. Each takes everything under
    /// it, except what a deeper root claims.
    pub roots: Vec<String>,
}

/// One running shard: its replicant pod, that pod's Service and its DGS pods, all in a namespace of its own. The
/// Shard stays here, holding a finalizer until that namespace is gone.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "scbackend.io",
    version = "v1alpha1",
    kind = "Shard",
    namespaced,
    status = "ShardStatus",
    shortname = "shd",
    printcolumn = r#"{"name":"Id","type":"string","jsonPath":".spec.id"}"#,
    printcolumn = r#"{"name":"Template","type":"string","jsonPath":".spec.template"}"#,
    printcolumn = r#"{"name":"Namespace","type":"string","jsonPath":".status.namespace"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Players","type":"integer","jsonPath":".status.players"}"#,
    printcolumn = r#"{"name":"DGS","type":"integer","jsonPath":".status.dgsReady"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ShardSpec {
    /// The ShardCluster this shard belongs to.
    pub cluster: String,
    /// `pub_<shard cluster>_<build>_<shard number>`, the id the game sees.
    pub id: String,
    /// The name the game and the status show.
    pub display_name: String,
    pub slot: u32,
    /// The ShardTemplate this shard's DGSs come from.
    pub template: String,
    /// Kept here because a change of region must not move a running shard. Empty on a shard from before shards
    /// had namespaces, which runs beside the operator until it empties.
    #[serde(default)]
    pub namespace: String,
    /// The ShardRegion its DGSs get their game data from. Empty on a shard from before regions, which takes its
    /// cluster's region.
    #[serde(default)]
    pub region: String,
    /// The game build it serves, the change number in its id. Empty on a shard from before regions, which reads it
    /// from its id.
    #[serde(default)]
    pub build: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum ShardPhase {
    /// The replicant is not answering yet.
    #[default]
    Pending,
    /// The replicant answers and every DGS is ready.
    Running,
    /// The replicant answers, so players can join, but a DGS is not ready or the template is unusable.
    Degraded,
    /// Deleted, and waiting for its namespace to go. Its slot and NodePort stay taken until then.
    Closing,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShardStatus {
    pub phase: ShardPhase,
    pub players: u32,
    pub dgs_ready: u32,
    pub dgs_desired: u32,
    /// Where its pods and Service are.
    #[serde(default)]
    pub namespace: String,
    /// What the shard is waiting for, or why it cannot go on.
    #[serde(default)]
    pub message: String,
    /// Its build was ready in its region and its pods were made. Its region keeps the build until it closes.
    #[serde(default)]
    pub opened: bool,
}

fn default_cache_replicas() -> u32 {
    2
}

fn default_cache_size() -> String {
    "10Gi".to_string()
}

fn default_primaries() -> u32 {
    3
}

fn default_replicas_per_primary() -> u32 {
    1
}

fn default_valkey_memory() -> String {
    "512Mi".to_string()
}

fn default_valkey_size() -> String {
    "2Gi".to_string()
}

fn default_work_size() -> String {
    "4Gi".to_string()
}

/// A region: its own namespace `scbackend-region-<name>` with the region's game data, a Valkey cluster that holds every
/// listed game build and the stateless `sc-gamedata` in front of it, which loads the builds from the registry and serves
/// them to the DGSs, and the region's entity database with the `sc-entitygraph` that owns it. A shard opens only once
/// its build and the entity database are ready in its region.
#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "scbackend.io",
    version = "v1alpha1",
    kind = "ShardRegion",
    status = "ShardRegionStatus",
    shortname = "shr",
    printcolumn = r#"{"name":"Namespace","type":"string","jsonPath":".status.namespace"}"#,
    printcolumn = r#"{"name":"Valkey","type":"integer","jsonPath":".status.cache.readyNodes"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".status.replicas"}"#,
    printcolumn = r#"{"name":"EntityDB","type":"string","jsonPath":".status.entityDb.state"}"#,
    printcolumn = r#"{"name":"Message","type":"string","jsonPath":".status.message"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ShardRegionSpec {
    /// Game builds, by change number such as `12913470`, that the region's game data holds. A build that shards still
    /// run on is kept beside these until they close, so a patch lists only the new one.
    pub game_builds: Vec<String>,
    /// Where the builds are pulled from (empty fields take the operator's settings), the Valkey cluster that holds them
    /// and the `sc-gamedata` replicas.
    #[serde(default)]
    pub gamedata: GamedataSource,
    /// Deprecated: the per-replica volumes of the former `sc-gamedata` StatefulSet. Only `replicas` is read, as
    /// `gamedata.serviceReplicas` when that is not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheSpec>,
    /// The region's entity database: an ArangoDeployment and the `sc-entitygraph` that owns it, in the region's
    /// namespace. Deleting the region deletes them, and the entities with them.
    #[serde(default)]
    pub entity_db: EntityDbSpec,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum EntityDbMode {
    /// One server, for a small region.
    Single,
    /// Agents, DB-Servers and Coordinators: the layout that survives losing a member.
    #[default]
    Cluster,
}

fn default_agents() -> u32 {
    3
}

fn default_db_servers() -> u32 {
    3
}

fn default_coordinators() -> u32 {
    2
}

fn default_db_size() -> String {
    "32Gi".to_string()
}

fn default_agent_size() -> String {
    "8Gi".to_string()
}

fn default_service_replicas() -> u32 {
    1
}

/// The ArangoDeployment `arangodb` of the region, which kube-arangodb runs, and `sc-entitygraph` in front of it. The
/// defaults are the layout the dev cluster ran before regions had their own: 3 Agents, 3 DB-Servers, 2 Coordinators.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EntityDbSpec {
    /// Fixed once the deployment exists: kube-arangodb does not convert one mode into the other.
    #[serde(default)]
    pub mode: EntityDbMode,
    /// Cluster mode: Agents, the Raft quorum. Odd.
    #[serde(default = "default_agents")]
    pub agents: u32,
    /// Cluster mode: DB-Servers, which hold the shards.
    #[serde(default = "default_db_servers")]
    pub db_servers: u32,
    /// Cluster mode: Coordinators, which the `arangodb` Service sends clients to.
    #[serde(default = "default_coordinators")]
    pub coordinators: u32,
    /// StorageClass of the volumes. Empty: the cluster's default. Fixed once the deployment exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// Volume of each DB-Server, or of the single server.
    #[serde(default = "default_db_size")]
    pub size: String,
    /// Volume of each Agent.
    #[serde(default = "default_agent_size")]
    pub agent_size: String,
    /// ArangoDB image. Empty: the operator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `sc-entitygraph` replicas.
    #[serde(default = "default_service_replicas")]
    pub service_replicas: u32,
    /// Send the game's `EntityGraphService` calls, which reach the backend through the one gateway, to this region.
    /// One region at a time: until each region has an entry point of its own, the gateway has only one route for it.
    #[serde(default)]
    pub client_route: bool,
}

impl Default for EntityDbSpec {
    fn default() -> Self {
        Self {
            mode: EntityDbMode::default(),
            agents: default_agents(),
            db_servers: default_db_servers(),
            coordinators: default_coordinators(),
            storage_class: None,
            size: default_db_size(),
            agent_size: default_agent_size(),
            image: None,
            service_replicas: default_service_replicas(),
            client_route: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum EntityDbState {
    /// The database or `sc-entitygraph` is not ready yet.
    #[default]
    Pending,
    /// The ArangoDeployment is Ready and an `sc-entitygraph` replica answers.
    Ready,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EntityDbStatus {
    #[serde(default)]
    pub state: EntityDbState,
    /// The address `sc-entitygraph` answers at, which DGSs and the other callers use.
    #[serde(default)]
    pub service: String,
    /// The ArangoDB address, for `sc-entitygraph` and the seed.
    #[serde(default)]
    pub database: String,
    /// What it is waiting for.
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GamedataSource {
    /// `host:port` of the OCI registry, as pods reach it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Builds are pulled as `<registry>/<repository>:<build>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Plain HTTP instead of HTTPS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plain_http: Option<bool>,
    /// Address the registry's host name resolves to in the `sc-gamedata` pods, for a name only the nodes know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_ip: Option<String>,
    /// The Valkey cluster that holds the region's game data.
    #[serde(default)]
    pub cache: ValkeySpec,
    /// `sc-gamedata` replicas, stateless. Unset: the deprecated `cache.replicas`, else 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_replicas: Option<u32>,
    /// Scratch space of each `sc-gamedata` replica (an emptyDir), which holds a build's image and files while it is
    /// loaded into Valkey: twice the largest build, about.
    #[serde(default = "default_work_size")]
    pub work_size: String,
}

impl Default for GamedataSource {
    fn default() -> Self {
        Self {
            registry: None,
            repository: None,
            plain_http: None,
            registry_ip: None,
            cache: ValkeySpec::default(),
            service_replicas: None,
            work_size: default_work_size(),
        }
    }
}

/// The region's Valkey cluster: `primaries` nodes, each with `replicasPerPrimary` replicas, every node with a
/// ReadWriteOnce volume of its own that it persists to, so a restarted node loads its share again. Keys spread over the
/// primaries by hash slot. Nothing is ever evicted: a full cluster fails the load of a build instead.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ValkeySpec {
    /// At least 3. Raising it adds nodes and moves slots to them; lowering it is refused.
    #[serde(default = "default_primaries")]
    pub primaries: u32,
    /// Fixed once the cluster is formed.
    #[serde(default = "default_replicas_per_primary")]
    pub replicas_per_primary: u32,
    /// Memory of each node, request and limit. Valkey may use three quarters of it for data; the rest is for the
    /// rewrite of its append-only file. A primary holds about (all builds) / primaries.
    #[serde(default = "default_valkey_memory")]
    pub memory: String,
    /// StorageClass of the volumes. Empty: the cluster's default. Fixed once the StatefulSet exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// Size of each node's volume, at least twice its memory for the append-only file and its rewrite. Fixed once
    /// the StatefulSet exists.
    #[serde(default = "default_valkey_size")]
    pub size: String,
    /// Valkey image. Empty: the operator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

impl Default for ValkeySpec {
    fn default() -> Self {
        Self {
            primaries: default_primaries(),
            replicas_per_primary: default_replicas_per_primary(),
            memory: default_valkey_memory(),
            storage_class: None,
            size: default_valkey_size(),
            image: None,
        }
    }
}

/// Deprecated: the former per-replica volumes of `sc-gamedata`. Kept so a ShardRegion written before the Valkey cache
/// still validates.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CacheSpec {
    /// Read as `gamedata.serviceReplicas` when that is not set.
    #[serde(default = "default_cache_replicas")]
    pub replicas: u32,
    /// Ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class: Option<String>,
    /// Ignored.
    #[serde(default = "default_cache_size")]
    pub size: String,
}

impl Default for CacheSpec {
    fn default() -> Self {
        Self { replicas: default_cache_replicas(), storage_class: None, size: default_cache_size() }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub enum BuildState {
    /// Not complete in the region's Valkey yet, or no `sc-gamedata` replica answers for it.
    #[default]
    Pending,
    /// Complete in the region's Valkey, as a ready `sc-gamedata` replica reports.
    Ready,
}

/// The region's Valkey cluster as last seen.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    /// Nodes it is meant to have, primaries and replicas.
    #[serde(default)]
    pub nodes: u32,
    /// Nodes that see the cluster as ok.
    #[serde(default)]
    pub ready_nodes: u32,
    /// The init Job formed it with this many nodes.
    #[serde(default)]
    pub formed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShardRegionStatus {
    /// Every build the region holds or is loading, `gameBuilds` and those kept for running shards.
    #[serde(default)]
    pub builds: BTreeMap<String, BuildState>,
    /// Builds kept only because shards still run on them.
    #[serde(default)]
    pub retained_builds: Vec<String>,
    #[serde(default)]
    pub namespace: String,
    /// The address DGSs reach `sc-gamedata` at.
    #[serde(default)]
    pub cache_service: String,
    /// `sc-gamedata` replicas.
    #[serde(default)]
    pub replicas: u32,
    #[serde(default)]
    pub ready_replicas: u32,
    /// The Valkey cluster.
    #[serde(default)]
    pub cache: CacheStatus,
    /// What the region is waiting for, or why it cannot go on.
    #[serde(default)]
    pub message: String,
    /// The region's entity database and its `sc-entitygraph`.
    #[serde(default)]
    pub entity_db: EntityDbStatus,
}

//! Opens and closes shards for every ShardCluster as player demand changes, and tells the Hub which
//! shard a player should join.
//!
//! Two levels: each ShardCluster becomes Shard objects as demand changes, and each Shard becomes a namespace with
//! a replicant pod (the PU frontend), a NodePort Service, one pod and one Service per DGS of its ShardTemplate, and a
//! NetworkPolicy that lets only the replicant reach the DGSs. A Shard holds a finalizer, so deleting one deletes its
//! namespace, and everything in it, and the Shard goes when that has finished. Player counts come
//! from the replicants. A shard with players or a reservation on it is never closed.
//!
//! Each ShardRegion becomes a namespace with its game data (a Valkey cluster holding its game builds and the stateless
//! `sc-gamedata` in front of it, see region.rs) and the region's entity database (an ArangoDeployment with kube-arangodb's operator for that namespace, and `sc-entitygraph` in front of
//! it), and a Shard makes its pods only once its build and that database are ready in its region.

mod crd;
mod entitydb;
mod region;
mod scaling;
mod shard;
mod template;

use std::{
    collections::{hash_map::RandomState, BTreeMap, HashMap, HashSet},
    hash::BuildHasher,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use clap::{Parser, Subcommand};
use crd::{
    Shard, ShardCluster, ShardClusterStatus, ShardPhase, ShardRegion, ShardRegionStatus, ShardSpec, ShardStatus, ShardSummary,
    ShardTemplate,
};
use entitydb::EntityDbSettings;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, StatefulSet},
        batch::v1::Job,
        core::v1::{ConfigMap, Namespace, PersistentVolumeClaim, Pod, ResourceQuota, Secret, Service, ServiceAccount},
        networking::v1::NetworkPolicy,
        rbac::v1::{ClusterRoleBinding, RoleBinding},
    },
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
    apimachinery::pkg::apis::meta::v1::OwnerReference,
};
use kube::{
    api::{ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, ObjectMeta, Patch, PatchParams, PostParams},
    Api, Client, CustomResourceExt, Resource, ResourceExt,
};
use sc_proto::internal::{
    game_data_client::GameDataClient,
    shard_directory_server::{ShardDirectory, ShardDirectoryServer},
    AllocateShardRequest, AllocateShardResponse, ListGameBuildsRequest,
};
use region::{ChartRegion, GamedataSettings};
use sc_runtime::CommonArgs;
use scaling::{
    arrived, desired_shards, display_name, free_slots, pick_shard, shard_id, shard_name, shard_namespace, Candidate,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use shard::{
    drained, draining, namespace_lost, outdated, phase, plan, plan_closing, pod_hash, quota_outdated, quota_pods, replaceable, resolved,
    shard_build, to_adopt, Action, Closing,
    ClusterPod, Desired, DgsSettings, Existing, Observed, ObservedNamespace, AREA_ANNOTATION, CLUSTER_LABEL, DGS_SHARD_LABEL,
    POD_HASH_ANNOTATION,
    DRAIN_ANNOTATION, QUOTA_NAME, SHARD_FINALIZER, SHARD_ID_LABEL, SHARD_LABEL, SLOT_LABEL, STATUS_PORT,
};
use template::{assignments, dgs_areas, ChartTemplate, DgsAssignment};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};
use tonic::{transport::Server, Request, Response, Status};
use tracing::{info, warn};

const FIELD_MANAGER: &str = "sc-shard-operator";
const RECONCILE_PERIOD: Duration = Duration::from_secs(3);
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);
/// A reservation stops counting once the player has had time to connect.
const RESERVATION_LIFETIME: Duration = Duration::from_secs(60);
const INSTALL_ATTEMPTS: u32 = 30;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Namespace holding the ShardClusters, ShardTemplates and Shards.
    #[arg(long, env = "SCBACKEND_NAMESPACE", default_value = "scbackend")]
    namespace: String,
    /// The sc-dgs image of the DGS pods.
    #[arg(long, env = "SCBACKEND_DGS_IMAGE")]
    dgs_image: String,
    /// The `sc-character-customizer` address of the DGS pods, which they ask for saved characters. A full service name.
    #[arg(long, env = "SCBACKEND_DGS_CUSTOMIZER", default_value = "http://sc-character-customizer.scbackend.svc:50051")]
    dgs_customizer: String,
    /// The sc-gamedata image of each ShardRegion's game data.
    #[arg(long, env = "SCBACKEND_GAMEDATA_IMAGE")]
    gamedata_image: String,
    /// The registry, `host:port` as pods reach it, game builds are pulled from where a ShardRegion does not say.
    #[arg(long, env = "SCBACKEND_GAMEDATA_REGISTRY", default_value = "localhost:5001")]
    gamedata_registry: String,
    /// Game builds are pulled as `<registry>/<repository>:<build>`.
    #[arg(long, env = "SCBACKEND_GAMEDATA_REPOSITORY", default_value = "scbackend/gamedata")]
    gamedata_repository: String,
    /// Plain HTTP to that registry.
    #[arg(long, env = "SCBACKEND_GAMEDATA_PLAIN_HTTP", default_value_t = true, action = clap::ArgAction::Set)]
    gamedata_plain_http: bool,
    /// Address the registry's host resolves to in the sc-gamedata pods, for a name only the nodes know. Empty: their DNS.
    #[arg(long, env = "SCBACKEND_GAMEDATA_REGISTRY_IP", default_value = "")]
    gamedata_registry_ip: String,
    /// Valkey, where a region's `gamedata.cache.image` is empty: each region's game data cluster.
    // valkey 9.0.7
    #[arg(
        long,
        env = "SCBACKEND_VALKEY_IMAGE",
        default_value = "docker.io/valkey/valkey:9.0.7@sha256:7e1deda01b64ece3167d370ab5be3452d29734feec22cbcbc69b3a64f6859d6d"
    )]
    valkey_image: String,
    /// The sc-entitygraph image each ShardRegion runs in front of its entity database.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH_IMAGE")]
    entitygraph_image: String,
    /// ArangoDB, where a region's `entityDb.image` is empty.
    // arangodb 3.12.13
    #[arg(
        long,
        env = "SCBACKEND_ARANGO_IMAGE",
        default_value = "docker.io/library/arangodb:3.12.13@sha256:e48adfc58986a10e0316aa23ccae95ca580d880a41c76a8f0df68f9c29b470f2"
    )]
    arango_image: String,
    /// kube-arangodb, which each region runs for its own namespace. The version of the CRDs installed in the cluster.
    // kube-arangodb 1.4.7
    #[arg(
        long,
        env = "SCBACKEND_ARANGO_OPERATOR_IMAGE",
        default_value = "docker.io/arangodb/kube-arangodb:1.4.7@sha256:5dbc63080044d0eb86c0447af79d437658b7b73d634801e00fcaafede4f3468a"
    )]
    arango_operator_image: String,
    /// sc-push's internal publisher, as each region's sc-entitygraph reaches it.
    #[arg(long, env = "SCBACKEND_ENTITYGRAPH_PUSH", default_value = "http://sc-push.scbackend.svc:50051")]
    entitygraph_push: String,
    /// RUST_LOG of each region's sc-entitygraph.
    #[arg(long, env = "SCBACKEND_REGION_LOG_LEVEL", default_value = "info")]
    region_log_level: String,
    /// The Secret with state.json in this namespace, which each region gets a copy of for sc-entitygraph.
    #[arg(long, env = "SCBACKEND_REGION_DATA_SECRET", default_value = "scbackend-data")]
    region_data_secret: String,
    /// The chart's ClusterRole with what kube-arangodb does in a region's namespace.
    #[arg(long, env = "SCBACKEND_ARANGO_OPERATOR_ROLE", default_value = "scbackend-sc-shard-operator-region-arango")]
    arango_operator_role: String,
    /// The chart's ClusterRole with kube-arangodb's reads outside the namespace.
    #[arg(long, env = "SCBACKEND_ARANGO_OPERATOR_CLUSTER_ROLE", default_value = "scbackend-sc-shard-operator-region-arango-cluster")]
    arango_operator_cluster_role: String,
    /// The chart's ClusterRole for the ArangoDB members.
    #[arg(long, env = "SCBACKEND_ARANGO_MEMBER_ROLE", default_value = "scbackend-sc-shard-operator-region-arango-member")]
    arango_member_role: String,
    /// The Gateway, in this namespace, the game's gRPC calls come through, and its listener.
    #[arg(long, env = "SCBACKEND_GATEWAY", default_value = "sc-edge")]
    gateway: String,
    #[arg(long, env = "SCBACKEND_GATEWAY_LISTENER", default_value = "grpc")]
    gateway_listener: String,
}

#[derive(Subcommand)]
enum Command {
    /// Print the CustomResourceDefinitions.
    Crd,
    /// Apply the CustomResourceDefinitions and the chart's ShardTemplates. Helm only creates the files in a
    /// chart's `crds` directory on first install, so this is what brings a running cluster up to date.
    Install {
        /// YAML list of ShardTemplates, as `shardTemplates` in the chart values.
        #[arg(long)]
        templates: PathBuf,
        /// YAML list of ShardRegions, as `shardRegions` in the chart values.
        #[arg(long)]
        regions: Option<PathBuf>,
    },
}

#[derive(Clone)]
struct LiveShard {
    slot: u32,
    name: String,
    id: String,
    /// Takes players. A draining shard does not, even when it answers.
    ready: bool,
    players: u32,
    draining: bool,
    /// Its replicant has an address.
    running: bool,
    answering: bool,
}

#[derive(Clone)]
struct ClusterView {
    public_address: String,
    public_port_base: u16,
    players_per_shard: u32,
    shards: Vec<LiveShard>,
}

#[derive(Default)]
struct Shared {
    /// By region.
    clusters: HashMap<String, ClusterView>,
    /// By shard name.
    reservations: HashMap<String, Vec<Instant>>,
    /// When a shard was first seen empty, by shard name.
    empty_since: HashMap<String, Instant>,
    /// Player count at the last pass, by shard name.
    last_players: HashMap<String, u32>,
}

impl Shared {
    fn reserved(&mut self, shard: &str) -> u32 {
        let Some(times) = self.reservations.get_mut(shard) else { return 0 };
        times.retain(|time| time.elapsed() < RESERVATION_LIFETIME);
        times.len() as u32
    }

    fn forget(&mut self, shard: &str) {
        self.empty_since.remove(shard);
        self.last_players.remove(shard);
        self.reservations.remove(shard);
    }
}

#[derive(Clone)]
struct Directory {
    shared: Arc<Mutex<Shared>>,
}

#[tonic::async_trait]
impl ShardDirectory for Directory {
    async fn allocate(
        &self,
        request: Request<AllocateShardRequest>,
    ) -> Result<Response<AllocateShardResponse>, Status> {
        let region = request.into_inner().region;
        let mut shared = self.shared.lock().unwrap();
        let Some(view) = shared.clusters.get(&region).cloned() else {
            return Err(Status::not_found(format!("no ShardCluster for region '{region}'")));
        };
        let candidates: Vec<Candidate> = view
            .shards
            .iter()
            .map(|s| Candidate {
                slot: s.slot,
                ready: s.ready,
                players: s.players,
                reserved: shared.reserved(&s.name),
            })
            .collect();
        let Some(slot) = pick_shard(&candidates, view.players_per_shard) else {
            return Err(Status::resource_exhausted(format!("no shard available in '{region}'")));
        };
        let shard = view.shards.iter().find(|s| s.slot == slot).expect("slot came from this list");
        shared.reservations.entry(shard.name.clone()).or_default().push(Instant::now());
        info!(%region, shard = %shard.id, name = %shard.name, "shard allocated");
        Ok(Response::new(AllocateShardResponse {
            name: shard.id.clone(),
            public_address: view.public_address.clone(),
            public_port: u32::from(view.public_port_base) + slot,
        }))
    }
}

/// Asks a shard for its player count. None means it is not answering yet.
async fn query_players(ip: &str) -> Option<u32> {
    let read = async {
        let mut stream = TcpStream::connect((ip, STATUS_PORT)).await.ok()?;
        stream.write_all(b"GET /status HTTP/1.0\r\n\r\n").await.ok()?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.ok()?;
        let body = reply.split("\r\n\r\n").nth(1)?;
        let status: serde_json::Value = serde_json::from_str(body).ok()?;
        status["players"].as_u64().map(|players| players as u32)
    };
    timeout(STATUS_TIMEOUT, read).await.ok().flatten()
}

fn owner<K: Resource<DynamicType = ()>>(object: &K) -> Result<OwnerReference, String> {
    object.controller_owner_ref(&()).ok_or_else(|| format!("{} has no uid", object.name_any()))
}

fn is_owned_by<K: ResourceExt>(object: &K, owner: Option<&str>) -> bool {
    owner.is_some_and(|uid| object.owner_references().iter().any(|reference| reference.uid == uid))
}

fn terminating<K: ResourceExt>(object: &K) -> bool {
    object.meta().deletion_timestamp.is_some()
}

/// From before shards had namespaces. It stays in the operator's namespace, where owner references remove its
/// objects, until it empties.
fn in_operator_namespace(shard: &Shard) -> bool {
    shard.spec.namespace.is_empty()
}

fn existing<K: ResourceExt>(object: &K, shard: &Shard) -> Existing {
    Existing {
        name: object.name_any(),
        owned: !in_operator_namespace(shard) || is_owned_by(object, shard.uid().as_deref()),
        terminating: terminating(object),
        area: object.annotations().get(AREA_ANNOTATION).cloned().unwrap_or_default(),
        pod_hash: object.annotations().get(POD_HASH_ANNOTATION).cloned().unwrap_or_default(),
    }
}

fn is_ready(pod: &Pod) -> bool {
    let conditions = pod.status.as_ref().and_then(|status| status.conditions.as_ref());
    conditions.is_some_and(|all| all.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

fn label<'a, K: ResourceExt>(object: &'a K, key: &str) -> Option<&'a str> {
    object.labels().get(key).map(String::as_str)
}

/// Creates from a manifest. An object already there, such as one a previous pass made, is not an error.
async fn create<K>(api: &Api<K>, manifest: Value, what: &str) -> Result<(), String>
where
    K: Clone + DeserializeOwned + Serialize + std::fmt::Debug,
{
    let object: K = serde_json::from_value(manifest).map_err(|e| format!("{what} spec: {e}"))?;
    match api.create(&PostParams::default(), &object).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(status)) if status.code == 409 => Ok(()),
        Err(error) => Err(format!("create {what}: {error}")),
    }
}

fn observed_namespace(namespace: &Namespace) -> ObservedNamespace {
    ObservedNamespace { shard: label(namespace, SHARD_LABEL).map(str::to_string), terminating: terminating(namespace) }
}

fn new_shard(cluster: &ShardCluster, name: &str, spec: ShardSpec) -> Result<Shard, String> {
    let labels = [
        (CLUSTER_LABEL.to_string(), spec.cluster.clone()),
        (SHARD_ID_LABEL.to_string(), spec.id.clone()),
        (SLOT_LABEL.to_string(), spec.slot.to_string()),
        ("app.kubernetes.io/part-of".to_string(), "scbackend".to_string()),
    ];
    let metadata = ObjectMeta {
        name: Some(name.to_string()),
        labels: Some(labels.into()),
        owner_references: Some(vec![owner(cluster)?]),
        finalizers: (!spec.namespace.is_empty()).then(|| vec![SHARD_FINALIZER.to_string()]),
        ..ObjectMeta::default()
    };
    Ok(Shard { metadata, spec, status: None })
}

/// Opens a shard. Its pods follow when the Shard is reconciled.
async fn open_shard(shards: &Api<Shard>, cluster: &ShardCluster, slot: u32) -> Result<Shard, String> {
    let cluster_name = cluster.name_any();
    let name = shard_name(&cluster_name, RandomState::new().hash_one(slot));
    let spec = ShardSpec {
        cluster: cluster_name.clone(),
        id: shard_id(&cluster_name, &cluster.spec.build, slot),
        display_name: display_name(&name),
        slot,
        template: cluster.spec.shard_template.clone(),
        namespace: shard_namespace(&cluster.spec.region, slot),
        region: cluster.spec.region.clone(),
        build: cluster.spec.build.clone(),
    };
    let shard = new_shard(cluster, &name, spec)?;
    let shard = shards.create(&PostParams::default(), &shard).await.map_err(|e| format!("create shard {name}: {e}"))?;
    info!(id = %shard.spec.id, %name, "shard opened");
    Ok(shard)
}

/// Its finalizer takes its namespace with it. See `finish_closing`.
async fn close_shard(shards: &Api<Shard>, name: &str) -> Result<(), String> {
    shards.delete(name, &DeleteParams::background()).await.map_err(|e| format!("delete shard {name}: {e}"))?;
    info!(shard = %name, "shard closed");
    Ok(())
}

/// Gives every replicant from before Shards existed a Shard of the same name, id and slot. It keeps running,
/// with its players. The Shard takes over its owner references when reconciled.
async fn adopt_pods(shards: &Api<Shard>, cluster: &ShardCluster, pods: &[Pod], known: &[Shard]) -> Result<Vec<Shard>, String> {
    let cluster_uid = cluster.uid();
    let listed: Vec<ClusterPod> = pods
        .iter()
        .map(|pod| ClusterPod {
            name: pod.name_any(),
            slot: label(pod, SLOT_LABEL).and_then(|slot| slot.parse().ok()),
            id: label(pod, SHARD_ID_LABEL).map(str::to_string),
            owned_by_cluster: is_owned_by(pod, cluster_uid.as_deref()),
            terminating: terminating(pod),
        })
        .collect();
    let names: Vec<String> = known.iter().map(ResourceExt::name_any).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut adopted = Vec::new();
    for pod in to_adopt(&listed, &names) {
        let spec = ShardSpec {
            cluster: cluster.name_any(),
            id: pod.id.clone().unwrap_or_default(),
            display_name: display_name(&pod.name),
            slot: pod.slot.unwrap_or_default(),
            template: cluster.spec.shard_template.clone(),
            namespace: String::new(),
            region: cluster.spec.region.clone(),
            build: String::new(),
        };
        let shard = new_shard(cluster, &pod.name, spec)?;
        let shard = shards.create(&PostParams::default(), &shard).await.map_err(|e| format!("adopt {}: {e}", pod.name))?;
        info!(shard = %pod.name, id = %shard.spec.id, "shard adopted");
        adopted.push(shard);
    }
    Ok(adopted)
}

async fn take_ownership<K>(api: &Api<K>, name: &str, owner: &OwnerReference) -> Result<(), String>
where
    K: Clone + DeserializeOwned + std::fmt::Debug,
{
    let patch = Patch::Merge(json!({ "metadata": { "ownerReferences": [owner] } }));
    api.patch(name, &PatchParams::default(), &patch).await.map_err(|e| format!("adopt {name}: {e}"))?;
    Ok(())
}

/// Pods, Services, policies and quotas are listed across all namespaces, by label.
struct Apis {
    namespaces: Api<Namespace>,
    pods: Api<Pod>,
    services: Api<Service>,
    quotas: Api<ResourceQuota>,
    policies: Api<NetworkPolicy>,
    shards: Api<Shard>,
}

/// What every Shard of a cluster is reconciled against in one pass.
struct Pass<'a> {
    client: &'a Client,
    /// The operator's own.
    namespace: &'a str,
    apis: &'a Apis,
    cluster: &'a ShardCluster,
    namespaces: &'a [Namespace],
    pods: &'a [Pod],
    services: &'a [Service],
    quotas: &'a [ResourceQuota],
    policies: &'a [NetworkPolicy],
    dgs: &'a DgsSettings,
}

fn replicant_of<'a>(pods: &'a [Pod], shard: &str) -> Option<&'a Pod> {
    pods.iter().find(|pod| label(*pod, SHARD_LABEL) == Some(shard))
}

fn namespace_of<'a>(namespaces: &'a [Namespace], shard: &Shard) -> Option<&'a Namespace> {
    namespaces.iter().find(|namespace| namespace.name_any() == shard.spec.namespace)
}

/// Makes the namespace and its quota, or brings the quota up to the template. True when the shard's objects can go
/// in. A new namespace waits for the next pass: until its default ServiceAccount exists and its quota has been
/// counted, the API server rejects pods.
async fn prepare_namespace(pass: &Pass<'_>, shard: &Shard, dgs: Option<&[DgsAssignment]>) -> Result<bool, String> {
    let name = shard.name_any();
    let target = &shard.spec.namespace;
    let created = namespace_of(pass.namespaces, shard).is_none();
    if created {
        let manifest = shard::namespace(&name, &shard.spec, &pass.cluster.spec.region);
        let object: Namespace = serde_json::from_value(manifest).map_err(|e| format!("namespace {target} spec: {e}"))?;
        match pass.apis.namespaces.create(&PostParams::default(), &object).await {
            Ok(_) => info!(shard = %name, namespace = %target, "namespace created"),
            // The list of this pass would have shown one of ours.
            Err(kube::Error::Api(status)) if status.code == 409 => {
                return Err(format!("namespace {target} exists and is not this shard's"));
            }
            Err(error) => return Err(format!("create namespace {target}: {error}")),
        }
    }
    let current = pass.quotas.iter().find(|quota| quota.metadata.namespace.as_ref() == Some(target));
    let count = dgs.map(<[DgsAssignment]>::len).filter(|count| quota_outdated(current.and_then(|quota| quota.spec.as_ref()), *count));
    if let Some(count) = count {
        let manifest = shard::quota(&name, &shard.spec, count);
        let quotas: Api<ResourceQuota> = Api::namespaced(pass.client.clone(), target);
        let params = PatchParams::apply(FIELD_MANAGER).force();
        quotas.patch(QUOTA_NAME, &params, &Patch::Apply(&manifest)).await.map_err(|e| format!("apply quota: {e}"))?;
        info!(shard = %name, pods = quota_pods(count), "quota applied");
    }
    Ok(!created)
}

/// Brings one Shard's objects to what its template says. `replaceable` is `shard::replaceable` of the shard: its pods
/// can be brought to the operator's current settings.
async fn apply_shard(
    pass: &Pass<'_>,
    shard: &Shard,
    dgs: Option<&[DgsAssignment]>,
    target: &str,
    replaceable: bool,
) -> Result<(), String> {
    let Pass { client, cluster, pods, services, policies, dgs: settings, .. } = pass;
    if !in_operator_namespace(shard) && !prepare_namespace(pass, shard, dgs).await? {
        return Ok(());
    }
    let name = shard.name_any();
    let dgs_pods = pods.iter().filter(|pod| label(*pod, DGS_SHARD_LABEL) == Some(&name));
    let observed = Observed {
        replicant: replicant_of(pods, &name).map(|pod| existing(pod, shard)),
        service: services.iter().find(|service| label(*service, SHARD_LABEL) == Some(&name)).map(|s| existing(s, shard)),
        dgs: dgs_pods.map(|pod| existing(pod, shard)).collect(),
        own_namespace: !in_operator_namespace(shard),
        dgs_services: services.iter().filter(|service| label(*service, DGS_SHARD_LABEL) == Some(&name)).map(|s| existing(s, shard)).collect(),
        dgs_policy: policies.iter().find(|policy| label(*policy, SHARD_LABEL) == Some(&name)).map(|p| existing(p, shard)),
        replaceable,
    };
    let pod_api: Api<Pod> = Api::namespaced((*client).clone(), target);
    let service_api: Api<Service> = Api::namespaced((*client).clone(), target);
    let policy_api: Api<NetworkPolicy> = Api::namespaced((*client).clone(), target);
    let reference = owner(shard)?;
    let manifest_owner = in_operator_namespace(shard).then_some(&reference);
    let replicant_pod = || {
        let areas = dgs.map(dgs_areas).unwrap_or_default();
        shard::replicant_pod(&name, &shard.spec, &cluster.spec.image, &areas, manifest_owner)
    };
    let actions = {
        let dgs_hash = |assignment: &DgsAssignment| pod_hash(&shard::dgs_pod(&name, &shard.spec, assignment, settings, manifest_owner));
        let desired = Desired { replicant: dgs.map(|_| pod_hash(&replicant_pod())), dgs: &dgs_hash };
        plan(&observed, dgs, &desired)
    };
    for action in actions {
        match action {
            Action::CreateService => {
                let port = shard::node_port(&cluster.spec, shard.spec.slot);
                create(&service_api, shard::service(&name, &shard.spec, port, manifest_owner), &format!("service {name}")).await?;
            }
            Action::CreateReplicant => create(&pod_api, replicant_pod(), &format!("pod {name}")).await?,
            Action::CreateDgs(assignment) => {
                let manifest = shard::dgs_pod(&name, &shard.spec, &assignment, settings, manifest_owner);
                create(&pod_api, manifest, &format!("DGS {} of {name}", assignment.name)).await?;
            }
            Action::CreateDgsService(assignment) => {
                let manifest = shard::dgs_service(&name, &shard.spec, &assignment);
                create(&service_api, manifest, &format!("service of DGS {} of {name}", assignment.name)).await?;
            }
            Action::CreateDgsPolicy => create(&policy_api, shard::dgs_policy(&name, &shard.spec), &format!("DGS policy of {name}")).await?,
            Action::DeleteService(service) => {
                service_api.delete(&service, &DeleteParams::default()).await.map_err(|e| format!("delete service {service}: {e}"))?;
            }
            Action::DeletePod(pod) => {
                pod_api.delete(&pod, &DeleteParams::default()).await.map_err(|e| format!("delete pod {pod}: {e}"))?;
                info!(shard = %name, %pod, "pod deleted, to be made again from the current template and settings");
            }
            Action::AdoptPod(pod) => take_ownership(&pod_api, &pod, &reference).await?,
            Action::AdoptService(service) => take_ownership(&service_api, &service, &reference).await?,
        }
    }
    Ok(())
}

async fn patch_status(shards: &Api<Shard>, shard: &Shard, status: ShardStatus) -> Result<(), String> {
    if shard.status.as_ref() == Some(&status) {
        return Ok(());
    }
    let patch = Patch::Merge(json!({ "status": status }));
    shards
        .patch_status(&shard.name_any(), &PatchParams::apply(FIELD_MANAGER), &patch)
        .await
        .map_err(|e| format!("patch shard status: {e}"))?;
    Ok(())
}

/// Brings one Shard to what its template says and updates its status, which also shows a failure. `players` is None
/// when the replicant does not answer. `gate` is `region::gate`: until the shard's build is ready in its region, nothing
/// of it is made, and what already runs is left alone. `replaceable` as for `apply_shard`.
async fn reconcile_shard(
    pass: &Pass<'_>,
    shard: &Shard,
    dgs: Option<&[DgsAssignment]>,
    players: Option<u32>,
    replaceable: bool,
    gate: Result<(), String>,
) -> Result<(), String> {
    let target = if in_operator_namespace(shard) { pass.namespace } else { shard.spec.namespace.as_str() };
    let opened = gate.is_ok();
    let outcome = match gate {
        Ok(()) => apply_shard(pass, shard, dgs, target, replaceable).await,
        Err(waiting) => {
            if shard.status.as_ref().is_none_or(|status| status.message != waiting) {
                info!(shard = %shard.name_any(), %waiting, "shard waits for its region");
            }
            Err(waiting)
        }
    };

    let name = shard.name_any();
    let dgs_pods = pass.pods.iter().filter(|pod| label(*pod, DGS_SHARD_LABEL) == Some(&name));
    let dgs_ready = dgs_pods.filter(|pod| !terminating(*pod) && is_ready(pod)).count() as u32;
    let dgs_desired = dgs.map_or(0, |all| all.len() as u32);
    let status = ShardStatus {
        phase: phase(players.is_some(), dgs_ready, dgs_desired),
        players: players.unwrap_or(0),
        dgs_ready,
        dgs_desired,
        namespace: target.to_string(),
        message: outcome.as_ref().err().cloned().unwrap_or_default(),
        opened: opened || shard.status.as_ref().is_some_and(|status| status.opened),
    };
    patch_status(&pass.apis.shards, shard, status).await?;
    if opened {
        outcome
    } else {
        Ok(())
    }
}

/// Takes deleted Shards through their finalizer. Runs apart from the ShardClusters, as their Shards outlive them.
async fn finish_closing(client: &Client, namespace: &str) -> Result<(), String> {
    let shards: Api<Shard> = Api::namespaced(client.clone(), namespace);
    let namespaces: Api<Namespace> = Api::all(client.clone());
    let listed = shards.list(&ListParams::default()).await.map_err(|e| format!("list shards: {e}"))?.items;
    let holds_finalizer = |shard: &&Shard| shard.finalizers().iter().any(|finalizer| finalizer == SHARD_FINALIZER);
    for shard in listed.iter().filter(|shard| terminating(*shard)).filter(holds_finalizer) {
        let name = shard.name_any();
        let target = &shard.spec.namespace;
        let found = namespaces.get_opt(target).await.map_err(|e| format!("read namespace {target}: {e}"))?;
        let step = plan_closing(&name, found.as_ref().map(observed_namespace).as_ref());
        match step {
            Closing::DeleteNamespace => {
                namespaces.delete(target, &DeleteParams::background()).await.map_err(|e| format!("delete namespace {target}: {e}"))?;
                info!(shard = %name, namespace = %target, "namespace deleted");
            }
            Closing::Wait => {}
            Closing::RemoveFinalizer => {
                let rest: Vec<&String> = shard.finalizers().iter().filter(|finalizer| *finalizer != SHARD_FINALIZER).collect();
                let patch = Patch::Merge(json!({ "metadata": { "finalizers": rest } }));
                shards.patch(&name, &PatchParams::default(), &patch).await.map_err(|e| format!("finish {name}: {e}"))?;
                info!(shard = %name, "shard gone");
                continue;
            }
        }
        let status = ShardStatus {
            phase: ShardPhase::Closing,
            namespace: target.clone(),
            message: format!("waiting for namespace {target} to finish Terminating"),
            ..shard.status.clone().unwrap_or_default()
        };
        if shard.status.as_ref() != Some(&status) {
            warn!(shard = %name, namespace = %target, "shard holds its slot until its namespace is gone");
        }
        patch_status(&shards, shard, status).await?;
    }
    Ok(())
}

/// The DGSs of each usable template, by name. One that does not validate runs no DGS changes, and says why.
fn usable_templates(templates: &[ShardTemplate]) -> HashMap<String, Vec<DgsAssignment>> {
    let mut usable = HashMap::new();
    for template in templates {
        match assignments(&template.spec) {
            Ok(all) => {
                usable.insert(template.name_any(), all);
            }
            Err(error) => warn!(template = %template.name_any(), %error, "invalid ShardTemplate"),
        }
    }
    usable
}

async fn reconcile(
    client: &Client,
    namespace: &str,
    cluster: &ShardCluster,
    shared: &Mutex<Shared>,
    dgs: &DgsSettings,
    regions: &BTreeMap<String, ShardRegionStatus>,
) -> Result<(), String> {
    let apis = Apis {
        namespaces: Api::all(client.clone()),
        pods: Api::all(client.clone()),
        services: Api::all(client.clone()),
        quotas: Api::all(client.clone()),
        policies: Api::all(client.clone()),
        shards: Api::namespaced(client.clone(), namespace),
    };
    let clusters: Api<ShardCluster> = Api::namespaced(client.clone(), namespace);
    let templates: Api<ShardTemplate> = Api::namespaced(client.clone(), namespace);
    let name = cluster.name_any();
    let spec = &cluster.spec;

    let selector = ListParams::default().labels(&format!("{CLUSTER_LABEL}={name}"));
    let namespaces = apis.namespaces.list(&selector).await.map_err(|e| format!("list namespaces: {e}"))?.items;
    let quotas = apis.quotas.list(&selector).await.map_err(|e| format!("list quotas: {e}"))?.items;
    let pods = apis.pods.list(&selector).await.map_err(|e| format!("list pods: {e}"))?.items;
    let services = apis.services.list(&selector).await.map_err(|e| format!("list services: {e}"))?.items;
    let policies = apis.policies.list(&selector).await.map_err(|e| format!("list network policies: {e}"))?.items;
    let (closing, mut shards): (Vec<Shard>, Vec<Shard>) =
        apis.shards.list(&selector).await.map_err(|e| format!("list shards: {e}"))?.items.into_iter().partition(terminating);
    let templates = templates.list(&ListParams::default()).await.map_err(|e| format!("list shard templates: {e}"))?.items;
    let templates = usable_templates(&templates);
    let adopted = adopt_pods(&apis.shards, cluster, &pods, &shards).await?;
    shards.extend(adopted);
    shards.sort_by_key(|shard| shard.spec.slot);

    let mut current = Vec::new();
    for shard in shards {
        let shard_name = shard.name_any();
        let replicant = replicant_of(&pods, &shard_name).filter(|pod| !terminating(*pod));
        let id = shard_id(&name, &spec.build, shard.spec.slot);
        let observed = namespace_of(&namespaces, &shard).map(observed_namespace);
        let lost = !in_operator_namespace(&shard) && namespace_lost(&shard_name, observed.as_ref());
        if outdated(&shard.spec, &id, replicant.is_some()) || lost {
            close_shard(&apis.shards, &shard_name).await?;
        } else {
            current.push(shard);
        }
    }

    let mut live = Vec::new();
    let mut answers = HashMap::new();
    for shard in &current {
        let shard_name = shard.name_any();
        let replicant = replicant_of(&pods, &shard_name).filter(|pod| !terminating(*pod));
        let ip = replicant.and_then(|pod| pod.status.as_ref()).and_then(|status| status.pod_ip.clone());
        let players = match &ip {
            Some(ip) => query_players(ip).await,
            None => None,
        };
        answers.insert(shard_name.clone(), players);
        let marked = shard.annotations().contains_key(DRAIN_ANNOTATION);
        let draining = draining(&shard.spec, &shard_id(&name, &spec.build, shard.spec.slot), marked);
        live.push(LiveShard {
            slot: shard.spec.slot,
            name: shard_name,
            id: shard.spec.id.clone(),
            ready: players.is_some() && !draining,
            players: players.unwrap_or(0),
            draining,
            running: ip.is_some(),
            answering: players.is_some(),
        });
    }

    let (players, desired, active, to_close, replaceable_shards) = {
        let mut shared = shared.lock().unwrap();
        let mut players = 0;
        let mut demand = 0;
        let mut idle = Vec::new();
        let mut to_close = Vec::new();
        // Shards whose pods can be brought to the current settings, see `shard::replaceable`.
        let mut replaceable_shards = HashSet::new();
        for shard in &live {
            let previous = shared.last_players.insert(shard.name.clone(), shard.players).unwrap_or(0);
            if let Some(times) = shared.reservations.get_mut(&shard.name) {
                let count = arrived(previous, shard.players).min(times.len());
                times.drain(..count);
            }
            let reserved = shared.reserved(&shard.name);
            let load = shard.players + reserved;
            players += load;
            if !shard.draining {
                demand += load;
            }
            if load > 0 {
                shared.empty_since.remove(&shard.name);
                continue;
            }
            let since = *shared.empty_since.entry(shard.name.clone()).or_insert_with(Instant::now);
            if replaceable(load, shard.running, shard.answering, since.elapsed()) {
                replaceable_shards.insert(shard.name.clone());
            }
            if !shard.draining {
                idle.push((shard.clone(), since));
            } else if drained(shard.running, shard.answering, since.elapsed()) {
                to_close.push(shard.name.clone());
            }
        }
        // Draining shards take nobody new, so only the others count towards what demand needs.
        let desired = desired_shards(spec, demand);
        let active = live.iter().filter(|shard| !shard.draining).count() as u32;
        // Close from the top, one per pass, and only shards that have stayed empty.
        let delay = Duration::from_secs(u64::from(spec.scale_down_delay_seconds));
        if active > desired {
            to_close.extend(idle.iter().rev().find(|(_, since)| since.elapsed() >= delay).map(|(shard, _)| shard.name.clone()));
        }
        shared.clusters.insert(
            spec.region.clone(),
            ClusterView {
                public_address: spec.public_address.clone(),
                public_port_base: spec.public_port_base,
                players_per_shard: spec.players_per_shard,
                shards: live.clone(),
            },
        );
        (players, desired, active, to_close, replaceable_shards)
    };

    // Anything left of an earlier shard keeps its slot taken, so its namespace name and NodePort are not reused.
    let labelled = namespaces.iter().map(|namespace| label(namespace, SLOT_LABEL));
    let labelled = labelled.chain(services.iter().map(|service| label(service, SLOT_LABEL)));
    let used: Vec<u32> = live
        .iter()
        .map(|shard| shard.slot)
        .chain(closing.iter().map(|shard| shard.spec.slot))
        .chain(labelled.flatten().filter_map(|slot| slot.parse().ok()))
        .collect();
    let missing = desired.saturating_sub(active);
    for slot in free_slots(&used, missing, spec.max_shards) {
        current.push(open_shard(&apis.shards, cluster, slot).await?);
    }
    for shard in &to_close {
        close_shard(&apis.shards, shard).await?;
        current.retain(|open| open.name_any() != *shard);
        shared.lock().unwrap().forget(shard);
    }

    let pass = Pass {
        client,
        namespace,
        apis: &apis,
        cluster,
        namespaces: &namespaces,
        pods: &pods,
        services: &services,
        quotas: &quotas,
        policies: &policies,
        dgs,
    };
    for shard in &current {
        let dgs = templates.get(&shard.spec.template).map(Vec::as_slice);
        if dgs.is_none() {
            warn!(shard = %shard.name_any(), template = %shard.spec.template, "no usable ShardTemplate, so no DGS changes");
        }
        let players = answers.get(&shard.name_any()).copied().flatten();
        let replaceable = replaceable_shards.contains(&shard.name_any());
        let mut shard = shard.clone();
        shard.spec = resolved(&shard.spec, &spec.region);
        let gate = region::gate(&shard.spec.region, &shard.spec.build, regions);
        if let Err(error) = reconcile_shard(&pass, &shard, dgs, players, replaceable, gate).await {
            warn!(shard = %shard.name_any(), %error, "shard reconcile failed");
        }
    }

    let status = ShardClusterStatus {
        ready_shards: live.iter().filter(|shard| shard.ready).count() as u32,
        desired_shards: desired,
        players,
        shards: live
            .iter()
            .map(|shard| ShardSummary {
                name: display_name(&shard.name),
                id: shard.id.clone(),
                public_port: spec.public_port_base + shard.slot as u16,
                ready: shard.ready,
                players: shard.players,
            })
            .collect(),
    };
    let patch = Patch::Merge(json!({ "status": status }));
    clusters
        .patch_status(&name, &PatchParams::apply(FIELD_MANAGER), &patch)
        .await
        .map_err(|e| format!("patch status: {e}"))?;
    Ok(())
}

/// A Shard's build, while it runs: it opened, or it ran before regions existed. Its region keeps the build until then.
fn running_build(shard: &Shard, clusters: &[ShardCluster]) -> Option<(String, String)> {
    let status = shard.status.as_ref()?;
    let running = status.opened || matches!(status.phase, ShardPhase::Running | ShardPhase::Degraded);
    if terminating(shard) || !running {
        return None;
    }
    let cluster = clusters.iter().find(|cluster| cluster.name_any() == shard.spec.cluster);
    let region = match (&shard.spec.region, cluster) {
        (region, _) if !region.is_empty() => region.clone(),
        (_, Some(cluster)) => cluster.spec.region.clone(),
        (_, None) => return None,
    };
    Some((region, shard_build(&shard.spec)))
}

async fn apply<K>(api: &Api<K>, name: &str, manifest: &Value) -> Result<(), String>
where
    K: Clone + DeserializeOwned + std::fmt::Debug,
{
    let params = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(name, &params, &Patch::Apply(manifest)).await.map_err(|e| format!("apply {name}: {e}"))?;
    Ok(())
}

/// The region's namespace, owned by its ShardRegion, which first takes the finalizer that deletes its database before
/// the namespace goes. Returns the owner reference the region's objects carry.
async fn apply_namespace(client: &Client, object: &ShardRegion) -> Result<OwnerReference, String> {
    let name = object.name_any();
    region::validate_name(&name)?;
    let reference = owner(object)?;
    let target = region::region_namespace(&name);
    let namespaces: Api<Namespace> = Api::all(client.clone());
    if let Some(found) = namespaces.get_opt(&target).await.map_err(|e| format!("read namespace {target}: {e}"))? {
        if terminating(&found) {
            return Err(format!("namespace {target} is Terminating"));
        }
        if !is_owned_by(&found, object.uid().as_deref()) {
            return Err(format!("namespace {target} exists and is not this region's"));
        }
    }
    hold_region(client, object).await?;
    apply(&namespaces, &target, &region::namespace(&name, &reference)).await?;
    Ok(reference)
}

/// The former `sc-gamedata` StatefulSet, whose replicas each pulled the builds into a volume of their own: deleted, and
/// its volumes with it. The game data is in Valkey now.
async fn remove_former_cache(client: &Client, region: &str) -> Result<(), String> {
    let target = region::region_namespace(region);
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &target);
    if sets.get_opt(region::CACHE_NAME).await.map_err(|e| format!("read statefulset {}: {e}", region::CACHE_NAME))?.is_some() {
        delete_if_present(&sets, region::CACHE_NAME).await?;
        info!(%region, "former sc-gamedata StatefulSet deleted");
    }
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &target);
    let selector = ListParams::default().labels(&format!("{}={region},{}={}", region::REGION_LABEL, region::NAME_LABEL, region::CACHE_NAME));
    for claim in claims.list(&selector).await.map_err(|e| format!("list volumes: {e}"))?.items {
        if !terminating(&claim) {
            delete_if_present(&claims, &claim.name_any()).await?;
            info!(%region, claim = %claim.name_any(), "volume of the former sc-gamedata deleted");
        }
    }
    Ok(())
}

/// What `apply_cache` did: the builds, those kept for shards, the Valkey nodes it runs, and why they differ from the
/// spec's.
struct Applied {
    builds: Vec<String>,
    retained: Vec<String>,
    nodes: u32,
    warning: Option<String>,
}

/// Applies the region's game data: the Valkey cluster (ConfigMap, headless Service, NetworkPolicy, StatefulSet and the
/// Job that forms it) and `sc-gamedata` (Service and Deployment).
async fn apply_cache(
    client: &Client,
    object: &ShardRegion,
    settings: &GamedataSettings,
    in_use: &[String],
    reference: &OwnerReference,
) -> Result<Applied, String> {
    let name = object.name_any();
    let (builds, retained) = region::wanted_builds(&object.spec.game_builds, in_use)?;
    let cache = &object.spec.gamedata.cache;
    region::validate_cache(cache)?;
    let target = region::region_namespace(&name);
    let source = region::source(&object.spec.gamedata, settings);
    remove_former_cache(client, &name).await?;

    let config = region::valkey_config(&name, cache, reference);
    let maps: Api<ConfigMap> = Api::namespaced(client.clone(), &target);
    apply(&maps, region::VALKEY_NAME, &config).await?;
    let services: Api<Service> = Api::namespaced(client.clone(), &target);
    apply(&services, region::VALKEY_NAME, &region::valkey_service(&name, reference)).await?;
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), &target);
    apply(&policies, region::VALKEY_NAME, &region::valkey_policy(&name, reference)).await?;
    let sets: Api<StatefulSet> = Api::namespaced(client.clone(), &target);
    let running = sets.get_opt(region::VALKEY_NAME).await.map_err(|e| format!("read statefulset {}: {e}", region::VALKEY_NAME))?;
    let running = running.and_then(|set| set.spec).and_then(|spec| spec.replicas).and_then(|replicas| u32::try_from(replicas).ok());
    let (nodes, warning) = region::valkey_scale(cache, running);
    let set = region::valkey_statefulset(&name, cache, nodes, &source.valkey_image, &region::checksum(&config), reference);
    apply(&sets, region::VALKEY_NAME, &set).await?;

    let jobs: Api<Job> = Api::namespaced(client.clone(), &target);
    let job = region::valkey_init_name(cache, nodes);
    if jobs.get_opt(&job).await.map_err(|e| format!("read job {job}: {e}"))?.is_none() {
        create(&jobs, region::valkey_init_job(&name, cache, nodes, &source.valkey_image, reference), &format!("job {job}")).await?;
        info!(region = %name, %job, nodes, "valkey init job created");
    }
    let selector = ListParams::default().labels(&format!("{}={name},{}={}", region::REGION_LABEL, region::NAME_LABEL, region::VALKEY_INIT_NAME));
    for old in jobs.list(&selector).await.map_err(|e| format!("list jobs: {e}"))?.items {
        if old.name_any() != job && !terminating(&old) {
            delete_if_present(&jobs, &old.name_any()).await?;
        }
    }

    apply(&services, region::CACHE_NAME, &region::service(&name, reference)).await?;
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &target);
    apply(&deployments, region::CACHE_NAME, &region::deployment(&name, &object.spec, &source, &builds, reference)).await?;
    Ok(Applied { builds, retained, nodes, warning })
}

/// `sc-gamedata`'s `ListBuilds`: each build and whether it is complete in Valkey. None when no replica answers.
async fn list_builds(region: &str) -> Option<Vec<(String, bool)>> {
    let endpoint = tonic::transport::Endpoint::from_shared(region::cache_address(region)).ok()?;
    let endpoint = endpoint.connect_timeout(Duration::from_secs(1)).timeout(Duration::from_secs(2));
    let listed = async { GameDataClient::new(endpoint.connect().await.map_err(|e| e.to_string())?).list_builds(ListGameBuildsRequest {}).await.map_err(|e| e.to_string()) };
    match listed.await {
        Ok(reply) => Some(reply.into_inner().builds.into_iter().map(|build| (build.build, build.ready)).collect()),
        Err(error) => {
            warn!(%region, %error, "sc-gamedata did not list its builds");
            None
        }
    }
}

/// What runs of the region's game data: `sc-gamedata`'s pods and builds, and the Valkey nodes. `nodes` is what the
/// StatefulSet was given.
async fn observe_cache(client: &Client, region: &str, cache: &crd::ValkeySpec, nodes: u32) -> region::Observed {
    let target = region::region_namespace(region);
    let api: Api<Pod> = Api::namespaced(client.clone(), &target);
    let selector = ListParams::default().labels(&format!("{}={region}", region::REGION_LABEL));
    let pods = match api.list(&selector).await {
        Ok(list) => list.items,
        Err(error) => {
            warn!(%region, %error, "listing the region's pods failed");
            Vec::new()
        }
    };
    let named = |name: &'static str| pods.iter().filter(move |pod| label(*pod, region::NAME_LABEL) == Some(name));
    let gamedata: Vec<region::CachePod> = named(region::CACHE_NAME).map(|pod| region::CachePod { ready: is_ready(pod), terminating: terminating(pod) }).collect();
    let ready_nodes = named(region::VALKEY_NAME).filter(|pod| is_ready(pod) && !terminating(*pod)).count() as u32;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &target);
    let job = jobs.get_opt(&region::valkey_init_name(cache, nodes)).await.ok().flatten();
    let formed = job.and_then(|job| job.status).and_then(|status| status.succeeded).unwrap_or(0) > 0;
    let listed = match region::ready_replicas(&gamedata) {
        0 => None,
        _ => list_builds(region).await,
    };
    region::Observed { pods: gamedata, listed, cache: crd::CacheStatus { nodes, ready_nodes, formed } }
}

fn dynamic_api(client: &Client, namespace: &str, (group, version, kind, plural): (&str, &str, &str, &str)) -> Api<DynamicObject> {
    let resource = ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural);
    Api::namespaced_with(client.clone(), namespace, &resource)
}

/// A delete that finds nothing has nothing left to do.
async fn delete_if_present<K>(api: &Api<K>, name: &str) -> Result<(), String>
where
    K: Clone + DeserializeOwned + std::fmt::Debug,
{
    match api.delete(name, &DeleteParams::background()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
        Err(error) => Err(format!("delete {name}: {error}")),
    }
}

/// Applies the region's entity database: kube-arangodb's operator for the namespace with its bindings, the
/// ArangoDeployment, the copy of the data Secret, sc-entitygraph, and the gateway's way to it.
async fn apply_entity_db(client: &Client, object: &ShardRegion, settings: &EntityDbSettings, reference: &OwnerReference) -> Result<(), String> {
    let name = object.name_any();
    let spec = &object.spec.entity_db;
    let target = region::region_namespace(&name);

    let own_secrets: Api<Secret> = Api::namespaced(client.clone(), &settings.namespace);
    let source = own_secrets.get(&settings.data_secret).await.map_err(|e| format!("read secret {}: {e}", settings.data_secret))?;
    // As the API has it, base64 per key.
    let data = serde_json::to_value(source.data.unwrap_or_default()).map_err(|e| format!("secret {}: {e}", settings.data_secret))?;
    let data: BTreeMap<String, String> = serde_json::from_value(data).map_err(|e| format!("secret {}: {e}", settings.data_secret))?;
    let secrets: Api<Secret> = Api::namespaced(client.clone(), &target);
    apply(&secrets, &settings.data_secret, &entitydb::data_secret(&name, settings, &data, reference)).await?;

    let accounts: Api<ServiceAccount> = Api::namespaced(client.clone(), &target);
    apply(&accounts, entitydb::ARANGO_OPERATOR_NAME, &entitydb::operator_service_account(&name, reference)).await?;
    let bindings: Api<RoleBinding> = Api::namespaced(client.clone(), &target);
    apply(&bindings, entitydb::ARANGO_OPERATOR_NAME, &entitydb::operator_role_binding(&name, settings, reference)).await?;
    apply(&bindings, entitydb::MEMBER_BINDING_NAME, &entitydb::member_role_binding(&name, settings, reference)).await?;
    let cluster_bindings: Api<ClusterRoleBinding> = Api::all(client.clone());
    let binding = entitydb::cluster_binding_name(&name);
    apply(&cluster_bindings, &binding, &entitydb::operator_cluster_role_binding(&name, settings, reference)).await?;
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &target);
    apply(&deployments, entitydb::ARANGO_OPERATOR_NAME, &entitydb::operator_deployment(&name, settings, reference)).await?;

    let databases = dynamic_api(client, &target, entitydb::ARANGO_API);
    let database = entitydb::arango_deployment(&name, spec, settings, reference);
    apply(&databases, entitydb::DATABASE_NAME, &database)
        .await
        .map_err(|error| format!("{error} (are kube-arangodb's CRDs installed?)"))?;

    let checksum = entitydb::checksum(&data);
    let graph = entitydb::entitygraph_deployment(&name, spec, settings, &checksum, reference);
    apply(&deployments, entitydb::ENTITYGRAPH_NAME, &graph).await?;
    let services: Api<Service> = Api::namespaced(client.clone(), &target);
    apply(&services, entitydb::ENTITYGRAPH_NAME, &entitydb::entitygraph_service(&name, reference)).await?;

    let grants = dynamic_api(client, &target, entitydb::REFERENCE_GRANT_API);
    apply(&grants, entitydb::REFERENCE_GRANT_NAME, &entitydb::reference_grant(&name, settings, reference)).await?;
    let routes = dynamic_api(client, &settings.namespace, entitydb::GRPC_ROUTE_API);
    let route = entitydb::route_name(&name);
    match spec.client_route {
        true => apply(&routes, &route, &entitydb::grpc_route(&name, settings, reference)).await,
        false => delete_if_present(&routes, &route).await,
    }
}

/// What runs of the entity database. Errors read as not there yet.
async fn observe_entity_db(client: &Client, region: &str) -> entitydb::Observed {
    let target = region::region_namespace(region);
    let databases = dynamic_api(client, &target, entitydb::ARANGO_API);
    let database = match databases.get_opt(entitydb::DATABASE_NAME).await {
        Ok(found) => found.map(|found| serde_json::to_value(found).is_ok_and(|value| entitydb::arango_ready(&value))),
        Err(error) => {
            warn!(%region, %error, "reading the ArangoDeployment failed");
            None
        }
    };
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), &target);
    let graph = deployments.get_opt(entitydb::ENTITYGRAPH_NAME).await.ok().flatten();
    let graph_ready = graph.and_then(|graph| graph.status).and_then(|status| status.ready_replicas).unwrap_or(0);
    entitydb::Observed { database, graph_ready: u32::try_from(graph_ready).unwrap_or(0) }
}

fn holds_region_finalizer(object: &ShardRegion) -> bool {
    object.finalizers().iter().any(|finalizer| finalizer == entitydb::REGION_FINALIZER)
}

/// Sets the region's finalizers to `finalizers`, if it has not changed since it was read.
async fn set_region_finalizers(client: &Client, object: &ShardRegion, finalizers: Vec<String>) -> Result<(), String> {
    let name = object.name_any();
    let regions: Api<ShardRegion> = Api::all(client.clone());
    let patch = Patch::Merge(json!({ "metadata": { "finalizers": finalizers, "resourceVersion": object.resource_version() } }));
    regions.patch(&name, &PatchParams::default(), &patch).await.map_err(|e| format!("finalizers of region {name}: {e}"))?;
    Ok(())
}

async fn hold_region(client: &Client, object: &ShardRegion) -> Result<(), String> {
    if holds_region_finalizer(object) {
        return Ok(());
    }
    let mut finalizers = object.finalizers().to_vec();
    finalizers.push(entitydb::REGION_FINALIZER.to_string());
    set_region_finalizers(client, object, finalizers).await
}

/// A deleted region: its ArangoDeployment goes first, while the region's kube-arangodb still runs to take its members'
/// finalizers off, then the region's finalizer, after which the garbage collector takes the namespace and the rest.
/// Returns what it waits for.
async fn release_region(client: &Client, object: &ShardRegion) -> Result<String, String> {
    let name = object.name_any();
    if !holds_region_finalizer(object) {
        return Ok("deleting".to_string());
    }
    let databases = dynamic_api(client, &region::region_namespace(&name), entitydb::ARANGO_API);
    let found = match databases.get_opt(entitydb::DATABASE_NAME).await {
        Ok(found) => found,
        // No such kind: kube-arangodb is not installed, so there is nothing of it to wait for.
        Err(kube::Error::Api(status)) if status.code == 404 => None,
        Err(error) => return Err(format!("read ArangoDeployment {}: {error}", entitydb::DATABASE_NAME)),
    };
    if let Some(found) = found {
        if found.metadata.deletion_timestamp.is_none() {
            delete_if_present(&databases, entitydb::DATABASE_NAME).await?;
            info!(region = %name, "entity database deleted");
        }
        return Ok(format!("deleting: waiting for ArangoDeployment {} to go", entitydb::DATABASE_NAME));
    }
    let rest = object.finalizers().iter().filter(|finalizer| *finalizer != entitydb::REGION_FINALIZER).cloned().collect();
    set_region_finalizers(client, object, rest).await?;
    info!(region = %name, "region released");
    Ok("deleting".to_string())
}

async fn reconcile_region(client: &Client, object: &ShardRegion, settings: &Settings, in_use: &[String]) -> ShardRegionStatus {
    let name = object.name_any();
    let namespace = match terminating(object) {
        true => Err("deleting".to_string()),
        false => apply_namespace(client, object).await,
    };
    let (outcome, entity_error) = match &namespace {
        Ok(reference) => (
            apply_cache(client, object, &settings.gamedata, in_use, reference).await,
            apply_entity_db(client, object, &settings.entity_db, reference).await.err(),
        ),
        Err(error) if terminating(object) => (Err(error.clone()), Some(release_region(client, object).await.unwrap_or_else(|error| error))),
        Err(error) => (Err(error.clone()), Some(error.clone())),
    };
    if let Some(error) = entity_error.as_ref().filter(|_| !terminating(object)) {
        warn!(region = %name, %error, "entity database reconcile failed");
    }
    let cache = &object.spec.gamedata.cache;
    let (builds, retained, nodes, error) = match outcome {
        Ok(applied) => {
            if let Some(warning) = &applied.warning {
                warn!(region = %name, %warning, "game data");
            }
            (applied.builds, applied.retained, applied.nodes, applied.warning)
        }
        Err(error) => {
            warn!(region = %name, %error, "region reconcile failed");
            // sc-gamedata keeps running what it was last given, which the status goes on reporting.
            let last = object.status.as_ref();
            let builds = last.map(|status| status.builds.keys().cloned().collect()).unwrap_or_default();
            let retained = last.map(|status| status.retained_builds.clone()).unwrap_or_default();
            let nodes = last.map(|status| status.cache.nodes).filter(|nodes| *nodes > 0).unwrap_or_else(|| region::valkey_nodes(cache));
            (builds, retained, nodes, Some(error))
        }
    };
    let observed_cache = match region::validate_name(&name) {
        Ok(()) => observe_cache(client, &name, cache, nodes).await,
        Err(_) => region::Observed::default(),
    };
    let observed = match region::validate_name(&name) {
        Ok(()) => observe_entity_db(client, &name).await,
        Err(_) => entitydb::Observed::default(),
    };
    let entity_db = entitydb::status(&name, &observed, entity_error.as_deref());
    if object.status.as_ref().map(|last| last.entity_db.state) != Some(entity_db.state) {
        info!(region = %name, state = ?entity_db.state, message = %entity_db.message, "entity database");
    }
    let replicas = region::service_replicas(&object.spec);
    let status = region::status(&name, &builds, &retained, replicas, &observed_cache, error.as_deref(), entity_db);
    if object.status.as_ref() != Some(&status) {
        let regions: Api<ShardRegion> = Api::all(client.clone());
        let patch = json!({ "apiVersion": "scbackend.io/v1alpha1", "kind": "ShardRegion", "status": status });
        let params = PatchParams::apply(FIELD_MANAGER).force();
        if let Err(error) = regions.patch_status(&name, &params, &Patch::Apply(&patch)).await {
            warn!(region = %name, %error, "patch region status failed");
        }
        for (build, state) in &status.builds {
            if object.status.as_ref().and_then(|last| last.builds.get(build)) != Some(state) {
                info!(region = %name, %build, ?state, "game build");
            }
        }
    }
    status
}

/// Every ShardRegion's status of this pass, by name, which the shards are opened against. A region that could not be
/// listed is missing from it, so no shard opens.
/// What the operator gives every region.
struct Settings {
    gamedata: GamedataSettings,
    entity_db: EntityDbSettings,
}

async fn reconcile_regions(client: &Client, namespace: &str, settings: &Settings) -> Result<BTreeMap<String, ShardRegionStatus>, String> {
    let regions: Api<ShardRegion> = Api::all(client.clone());
    let regions = regions.list(&ListParams::default()).await.map_err(|e| format!("list shard regions: {e}"))?.items;
    let shards: Api<Shard> = Api::namespaced(client.clone(), namespace);
    let shards = shards.list(&ListParams::default()).await.map_err(|e| format!("list shards: {e}"))?.items;
    let clusters: Api<ShardCluster> = Api::namespaced(client.clone(), namespace);
    let clusters = clusters.list(&ListParams::default()).await.map_err(|e| format!("list shard clusters: {e}"))?.items;
    let running: Vec<(String, String)> = shards.iter().filter_map(|shard| running_build(shard, &clusters)).collect();
    let mut statuses = BTreeMap::new();
    for object in &regions {
        let name = object.name_any();
        let in_use: Vec<String> = running.iter().filter(|(region, _)| *region == name).map(|(_, build)| build.clone()).collect();
        statuses.insert(name, reconcile_region(client, object, settings, &in_use).await);
    }
    Ok(statuses)
}

async fn control_loop(client: Client, namespace: String, shared: Arc<Mutex<Shared>>, dgs: DgsSettings, settings: Settings) {
    let clusters: Api<ShardCluster> = Api::namespaced(client.clone(), &namespace);
    loop {
        if let Err(error) = finish_closing(&client, &namespace).await {
            warn!(%error, "closing shards failed");
        }
        let regions = reconcile_regions(&client, &namespace, &settings).await.unwrap_or_else(|error| {
            warn!(%error, "reconciling regions failed");
            BTreeMap::new()
        });
        match clusters.list(&ListParams::default()).await {
            Ok(list) => {
                for cluster in &list.items {
                    if let Err(error) = reconcile(&client, &namespace, cluster, &shared, &dgs, &regions).await {
                        warn!(cluster = %cluster.name_any(), %error, "reconcile failed");
                    }
                }
            }
            Err(error) => warn!(%error, "listing ShardClusters failed"),
        }
        sleep(RECONCILE_PERIOD).await;
    }
}

async fn kube_client() -> Result<Client, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::try_default().await.map_err(|e| format!("kubernetes client: {e}"))
}

fn definitions() -> [CustomResourceDefinition; 4] {
    [ShardCluster::crd(), ShardTemplate::crd(), Shard::crd(), ShardRegion::crd()]
}

async fn install_crds(client: Client) -> Result<(), String> {
    let api: Api<CustomResourceDefinition> = Api::all(client);
    let params = PatchParams::apply(FIELD_MANAGER).force();
    for definition in definitions() {
        let name = definition.name_any();
        api.patch(&name, &params, &Patch::Apply(&definition)).await.map_err(|e| format!("apply {name}: {e}"))?;
        let established = |crd: &CustomResourceDefinition| {
            let conditions = crd.status.as_ref().and_then(|status| status.conditions.as_ref());
            conditions.is_some_and(|all| all.iter().any(|c| c.type_ == "Established" && c.status == "True"))
        };
        let mut attempts = 0;
        while !established(&api.get(&name).await.map_err(|e| format!("read {name}: {e}"))?) {
            attempts += 1;
            if attempts == INSTALL_ATTEMPTS {
                return Err(format!("{name} was not established"));
            }
            sleep(Duration::from_secs(1)).await;
        }
        info!(%name, "CRD applied");
    }
    Ok(())
}

/// An invalid template is applied too: the operator reports it and leaves the DGSs alone, where failing here
/// would stop the operator and with it allocation.
async fn install_templates(client: Client, namespace: &str, file: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let templates: Vec<ChartTemplate> = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let api: Api<ShardTemplate> = Api::namespaced(client, namespace);
    let params = PatchParams::apply(FIELD_MANAGER).force();
    for template in templates {
        let object = ShardTemplate::new(&template.name, template.spec);
        // A CRD can report established a moment before its API answers.
        let mut attempts = 0;
        while let Err(error) = api.patch(&template.name, &params, &Patch::Apply(&object)).await {
            attempts += 1;
            if attempts == INSTALL_ATTEMPTS {
                return Err(format!("apply ShardTemplate {}: {error}", template.name));
            }
            sleep(Duration::from_secs(1)).await;
        }
        info!(template = %template.name, "ShardTemplate applied");
    }
    Ok(())
}

/// Like the templates: the chart's regions, applied once their CRD is there.
async fn install_regions(client: Client, file: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let regions: Vec<ChartRegion> = serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let api: Api<ShardRegion> = Api::all(client);
    let params = PatchParams::apply(FIELD_MANAGER).force();
    for region in regions {
        let object = ShardRegion::new(&region.name, region.spec);
        let mut attempts = 0;
        while let Err(error) = api.patch(&region.name, &params, &Patch::Apply(&object)).await {
            attempts += 1;
            if attempts == INSTALL_ATTEMPTS {
                return Err(format!("apply ShardRegion {}: {error}", region.name));
            }
            sleep(Duration::from_secs(1)).await;
        }
        info!(region = %region.name, builds = ?object.spec.game_builds, "ShardRegion applied");
    }
    Ok(())
}

async fn install(namespace: String, templates: PathBuf, regions: Option<PathBuf>) -> Result<(), String> {
    let client = kube_client().await?;
    install_crds(client.clone()).await?;
    install_templates(client.clone(), &namespace, &templates).await?;
    match regions {
        Some(regions) => install_regions(client, &regions).await,
        None => Ok(()),
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let client = kube_client().await?;
    let shared = Arc::new(Mutex::new(Shared::default()));
    let dgs = DgsSettings { image: args.dgs_image.clone(), customizer: args.dgs_customizer.clone() };
    let gamedata = GamedataSettings {
        image: args.gamedata_image.clone(),
        registry: args.gamedata_registry.clone(),
        repository: args.gamedata_repository.clone(),
        plain_http: args.gamedata_plain_http,
        registry_ip: Some(args.gamedata_registry_ip.clone()).filter(|ip| !ip.is_empty()),
        valkey_image: args.valkey_image.clone(),
    };
    let entity_db = EntityDbSettings {
        arango_image: args.arango_image.clone(),
        operator_image: args.arango_operator_image.clone(),
        entitygraph_image: args.entitygraph_image.clone(),
        push: args.entitygraph_push.clone(),
        log_level: args.region_log_level.clone(),
        data_secret: args.region_data_secret.clone(),
        operator_role: args.arango_operator_role.clone(),
        operator_cluster_role: args.arango_operator_cluster_role.clone(),
        member_role: args.arango_member_role.clone(),
        namespace: args.namespace.clone(),
        gateway: args.gateway.clone(),
        listener: args.gateway_listener.clone(),
    };
    let settings = Settings { gamedata, entity_db };
    tokio::spawn(control_loop(client, args.namespace.clone(), shared.clone(), dgs, settings));
    info!(listen = %args.listen, namespace = %args.namespace, "shard operator running");
    Server::builder()
        .add_service(ShardDirectoryServer::new(Directory { shared }))
        .serve(args.listen)
        .await
        .map_err(|e| e.to_string())
}

fn print_crds() -> ExitCode {
    for definition in definitions() {
        match serde_yaml::to_string(&definition) {
            Ok(text) => print!("---\n{text}"),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args = Args::parse();
    match args.command {
        Some(Command::Crd) => print_crds(),
        Some(Command::Install { templates, regions }) => sc_runtime::run(None, install(args.namespace, templates, regions)),
        None => sc_runtime::run(args.common.health, serve(args)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chart_ships_the_definitions_of_this_code() {
        let chart = [
            include_str!("../../../deploy/charts/sc-shard-operator/crds/shardclusters.yaml"),
            include_str!("../../../deploy/charts/sc-shard-operator/crds/shardtemplates.yaml"),
            include_str!("../../../deploy/charts/sc-shard-operator/crds/shards.yaml"),
            include_str!("../../../deploy/charts/sc-shard-operator/crds/shardregions.yaml"),
        ];
        for (file, definition) in chart.iter().zip(definitions()) {
            let file: serde_yaml::Value = serde_yaml::from_str(file).unwrap();
            assert_eq!(file, serde_yaml::to_value(&definition).unwrap(), "{}", definition.name_any());
        }
    }

    /// Every kind the operator lists and makes in a shard's namespace, which fails at run time if the chart forgets it.
    #[test]
    fn the_chart_lets_the_operator_make_what_a_shard_needs() {
        let chart = include_str!("../../../deploy/charts/sc-shard-operator/templates/operator.yaml");
        let rule = "- apiGroups: [\"networking.k8s.io\"]\n    resources: [\"networkpolicies\"]\n    verbs: [\"list\", \"create\"]";
        assert!(chart.contains(rule));
    }

    /// Everything a region's game data is made of, and the former StatefulSet and volumes it deletes.
    #[test]
    fn the_chart_lets_the_operator_make_a_regions_game_data() {
        let chart = include_str!("../../../deploy/charts/sc-shard-operator/templates/operator.yaml");
        for rule in [
            "- apiGroups: [\"apps\"]\n    resources: [\"statefulsets\"]\n    verbs: [\"get\", \"list\", \"create\", \"patch\", \"delete\"]",
            "- apiGroups: [\"\"]\n    resources: [\"configmaps\"]\n    verbs: [\"get\", \"create\", \"patch\"]",
            "- apiGroups: [\"batch\"]\n    resources: [\"jobs\"]\n    verbs: [\"get\", \"list\", \"create\", \"delete\"]",
            "- apiGroups: [\"\"]\n    resources: [\"persistentvolumeclaims\"]\n    verbs: [\"list\", \"delete\"]",
            "- apiGroups: [\"networking.k8s.io\"]\n    resources: [\"networkpolicies\"]\n    verbs: [\"get\", \"create\", \"patch\"]",
            "- name: SCBACKEND_VALKEY_IMAGE",
        ] {
            assert!(chart.contains(rule), "{rule}");
        }
        assert!(chart.contains("\"shardregions.scbackend.io\""));
        assert!(chart.contains("\"--regions\", \"/templates/regions.yaml\""));
    }

    /// Everything a region's entity database is made of, which fails at run time if the chart forgets it.
    #[test]
    fn the_chart_lets_the_operator_make_a_regions_entity_db() {
        let chart = include_str!("../../../deploy/charts/sc-shard-operator/templates/operator.yaml");
        for rule in [
            "- apiGroups: [\"database.arangodb.com\"]\n    resources: [\"arangodeployments\"]\n    verbs: [\"get\", \"create\", \"patch\", \"delete\"]",
            "- apiGroups: [\"apps\"]\n    resources: [\"deployments\"]\n    verbs: [\"get\", \"create\", \"patch\"]",
            "- apiGroups: [\"gateway.networking.k8s.io\"]\n    resources: [\"referencegrants\"]\n    verbs: [\"get\", \"create\", \"patch\"]",
            "- apiGroups: [\"gateway.networking.k8s.io\"]\n    resources: [\"grpcroutes\"]\n    verbs: [\"get\", \"create\", \"patch\", \"delete\"]",
            "- apiGroups: [\"rbac.authorization.k8s.io\"]\n    resources: [\"clusterrolebindings\"]\n    verbs: [\"get\", \"create\", \"patch\"]",
            "resourceNames: [\"arango-operator\", \"arango-member\"]",
            "verbs: [\"bind\"]",
            "- name: SCBACKEND_ARANGO_OPERATOR_CLUSTER_ROLE",
        ] {
            assert!(chart.contains(rule), "{rule}");
        }
        assert!(!chart.contains("DGS_ENTITYGRAPH"), "a DGS reaches its region's");
        let roles = include_str!("../../../deploy/charts/sc-shard-operator/templates/arango.yaml");
        assert!(roles.contains("arangodeployments/status") && roles.contains("persistentvolumes"));
    }

    /// What `sc-shard-operator.regions` renders from the chart's values, with `build` filled in.
    #[test]
    fn the_install_command_reads_the_charts_regions() {
        let rendered = "- name: usa\n  spec:\n    entityDb:\n      clientRoute: true\n    gameBuilds:\n    - \"12913470\"\n    gamedata:\n      cache:\n        memory: 512Mi\n        primaries: 3\n        replicasPerPrimary: 1\n        size: 2Gi\n      serviceReplicas: 2\n";
        let regions: Vec<ChartRegion> = serde_yaml::from_str(rendered).unwrap();
        assert_eq!(regions[0].name, "usa");
        assert_eq!(regions[0].spec.game_builds, ["12913470"]);
        let gamedata = &regions[0].spec.gamedata;
        assert_eq!(region::service_replicas(&regions[0].spec), 2);
        assert_eq!((gamedata.cache.primaries, gamedata.cache.replicas_per_primary, gamedata.cache.memory.as_str()), (3, 1, "512Mi"));
        assert_eq!((gamedata.registry.as_ref(), gamedata.repository.as_ref()), (None, None), "the operator's settings");
        assert!(regions[0].spec.cache.is_none());
        let entity_db = &regions[0].spec.entity_db;
        assert!(entity_db.client_route);
        assert_eq!((entity_db.mode, entity_db.db_servers), (crd::EntityDbMode::Cluster, 3), "the rest from the defaults");
    }

    #[test]
    fn a_region_spec_takes_its_defaults() {
        let spec: crd::ShardRegionSpec = serde_json::from_value(json!({ "gameBuilds": ["1"] })).unwrap();
        assert_eq!(spec.gamedata, crd::GamedataSource::default());
        let cache = &spec.gamedata.cache;
        assert_eq!((cache.primaries, cache.replicas_per_primary, cache.memory.as_str(), cache.size.as_str()), (3, 1, "512Mi", "2Gi"));
        assert_eq!((cache.storage_class.as_ref(), cache.image.as_ref(), spec.gamedata.work_size.as_str()), (None, None, "4Gi"));
        assert_eq!(region::service_replicas(&spec), 2);
        assert_eq!(spec.entity_db, crd::EntityDbSpec::default());
        let db = &spec.entity_db;
        assert_eq!((db.agents, db.db_servers, db.coordinators), (3, 3, 2));
        assert_eq!((db.size.as_str(), db.agent_size.as_str(), db.service_replicas, db.client_route), ("32Gi", "8Gi", 1, false));
        let single: crd::ShardRegionSpec = serde_json::from_value(json!({ "gameBuilds": ["1"], "entityDb": { "mode": "Single" } })).unwrap();
        assert_eq!(single.entity_db.mode, crd::EntityDbMode::Single);
    }

    /// The `usa` region as the chart applied it before the Valkey cache: it still reads, and keeps its replica count.
    #[test]
    fn a_region_from_before_the_valkey_cache_still_reads() {
        let spec = json!({ "gameBuilds": ["1"], "cache": { "replicas": 3, "size": "10Gi" }, "entityDb": { "clientRoute": true } });
        let spec: crd::ShardRegionSpec = serde_json::from_value(spec).unwrap();
        assert_eq!(region::service_replicas(&spec), 3);
        assert_eq!(spec.gamedata.cache, crd::ValkeySpec::default());
        let status = json!({ "builds": { "1": "Ready" }, "replicas": 2, "readyReplicas": 2, "entityDb": { "state": "Ready" } });
        let status: ShardRegionStatus = serde_json::from_value(status).unwrap();
        assert_eq!(status.cache, crd::CacheStatus::default());
    }

    #[test]
    fn a_region_status_from_before_entity_databases_still_reads() {
        let status = json!({ "builds": { "1": "Ready" }, "namespace": "scbackend-region-usa", "replicas": 2, "readyReplicas": 2 });
        let status: ShardRegionStatus = serde_json::from_value(status).unwrap();
        assert_eq!(status.entity_db.state, crd::EntityDbState::Pending, "no shard opens until it is reconciled");
    }

    fn with_status(region: &str, build: &str, status: ShardStatus) -> Shard {
        let spec = ShardSpec { region: region.into(), build: build.into(), ..spec("scbackend-shard-usa-010") };
        Shard { metadata: ObjectMeta { name: Some("s".into()), ..ObjectMeta::default() }, spec, status: Some(status) }
    }

    #[test]
    fn a_region_keeps_the_build_of_a_shard_that_runs_or_opened() {
        let clusters = [cluster()];
        let running = ShardStatus { phase: ShardPhase::Running, ..ShardStatus::default() };
        assert_eq!(running_build(&with_status("usa", "1", running.clone()), &clusters), Some(("usa".into(), "1".into())));
        let opened = ShardStatus { opened: true, ..ShardStatus::default() };
        assert!(running_build(&with_status("usa", "1", opened), &clusters).is_some());
        let waiting = ShardStatus { message: "waiting".into(), ..ShardStatus::default() };
        assert_eq!(running_build(&with_status("usa", "1", waiting), &clusters), None, "a shard waiting for its build holds none");
        let legacy = with_status("", "", running.clone());
        let legacy = Shard { spec: ShardSpec { id: "pub_usa_12913470_020".into(), ..legacy.spec }, ..legacy };
        assert_eq!(running_build(&legacy, &clusters), Some(("usa".into(), "12913470".into())), "from its cluster and id");
        let mut closing = with_status("usa", "1", running);
        closing.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Default::default()));
        assert_eq!(running_build(&closing, &clusters), None);
    }

    #[test]
    fn a_new_shard_keeps_its_clusters_region_and_build() {
        let shard = new_shard(&cluster(), "s", spec("scbackend-shard-usa-010")).unwrap();
        assert_eq!((shard.spec.region.as_str(), shard.spec.build.as_str()), ("usa", "1"));
        let old = json!({ "cluster": "usa", "id": "pub_usa_12913470_020", "displayName": "x", "slot": 1, "template": "default" });
        let old: ShardSpec = serde_json::from_value(old).unwrap();
        assert_eq!((old.region.as_str(), old.build.as_str()), ("", ""), "a Shard from before regions still reads");
    }

    #[test]
    fn an_existing_shard_cluster_gets_the_default_template() {
        let spec = json!({
            "region": "usa", "image": "x", "build": "1", "publicAddress": "127.0.0.2", "publicPortBase": 64090,
            "nodePortBase": 30090, "maxShards": 10, "playersPerShard": 50,
        });
        let spec: crd::ShardClusterSpec = serde_json::from_value(spec).unwrap();
        assert_eq!(spec.shard_template, "default");
    }

    #[test]
    fn a_shard_from_before_namespaces_still_reads() {
        let shard = json!({
            "apiVersion": "scbackend.io/v1alpha1", "kind": "Shard", "metadata": { "name": "usa-silver-raven" },
            "spec": { "cluster": "usa", "id": "pub_usa_12913470_010", "displayName": "usa_silver_raven", "slot": 0, "template": "default" },
            "status": { "dgsDesired": 10, "dgsReady": 10, "phase": "Running", "players": 0 },
        });
        let shard: Shard = serde_json::from_value(shard).unwrap();
        assert!(in_operator_namespace(&shard));
        assert_eq!(shard.status.unwrap().namespace, "");
    }

    fn cluster() -> ShardCluster {
        let spec = json!({
            "region": "usa", "image": "x", "build": "1", "publicAddress": "127.0.0.2", "publicPortBase": 64090,
            "nodePortBase": 30090, "maxShards": 10, "playersPerShard": 50,
        });
        let mut cluster = ShardCluster::new("usa", serde_json::from_value(spec).unwrap());
        cluster.metadata.uid = Some("uid".into());
        cluster
    }

    fn spec(namespace: &str) -> ShardSpec {
        ShardSpec {
            cluster: "usa".into(),
            id: "id".into(),
            display_name: "s".into(),
            slot: 0,
            template: "default".into(),
            namespace: namespace.into(),
            region: "usa".into(),
            build: "1".into(),
        }
    }

    #[test]
    fn only_a_shard_with_a_namespace_gets_the_finalizer() {
        let shard = new_shard(&cluster(), "s", spec("scbackend-shard-usa-010")).unwrap();
        assert_eq!(shard.finalizers(), [SHARD_FINALIZER]);
        assert!(new_shard(&cluster(), "s", spec("")).unwrap().finalizers().is_empty());
    }
}

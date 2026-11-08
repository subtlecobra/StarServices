//! What one Shard is made of and how to get there, kept free of the Kubernetes client like scaling.rs. The
//! operator lists what exists and carries out the actions of `plan`.

use std::{collections::BTreeMap, time::Duration};

use k8s_openapi::{api::core::v1::ResourceQuotaSpec, apimachinery::pkg::apis::meta::v1::OwnerReference};
use serde_json::{json, Value};

use crate::{
    crd::{ShardClusterSpec, ShardPhase, ShardSpec},
    entitydb::entitygraph_address,
    region::cache_address,
    template::DgsAssignment,
};

pub const CLUSTER_LABEL: &str = "scbackend.io/shardcluster";
pub const SHARD_LABEL: &str = "scbackend.io/shard";
pub const SHARD_ID_LABEL: &str = "scbackend.io/shard-id";
pub const SLOT_LABEL: &str = "scbackend.io/slot";
/// DGS pods must not carry SHARD_LABEL, which selects the pods behind the shard's Service, or SLOT_LABEL,
/// which marks a pod as a replicant.
pub const DGS_SHARD_LABEL: &str = "scbackend.io/dgs-shard";
const DGS_LABEL: &str = "scbackend.io/dgs";
pub const AREA_ANNOTATION: &str = "scbackend.io/area";
/// On a replicant or DGS pod: `pod_hash` of the manifest it was made from, which holds everything the operator's settings,
/// the shard's region and its spec put in it. A pod without one is from an operator older than this annotation.
pub const POD_HASH_ANNOTATION: &str = "scbackend.io/pod-hash";
/// On a Shard: take no more players and close once empty. scripts/cluster-up.sh sets it to replace running shards.
pub const DRAIN_ANNOTATION: &str = "scbackend.io/drain";
const REGION_LABEL: &str = "scbackend.io/region";
const PART_OF_LABEL: &str = "app.kubernetes.io/part-of";
/// Holds a Shard until its namespace is gone. A namespaced object cannot own a Namespace.
pub const SHARD_FINALIZER: &str = "scbackend.io/shard-namespace";
pub const QUOTA_NAME: &str = "shard";
/// Lets the replicant alone reach the DGSs of its namespace.
const DGS_POLICY_NAME: &str = "dgs";
/// A deploy restarts the operator, which loses its reservations, so a player sent to a draining shard may still be on
/// the way. Matches RESERVATION_LIFETIME.
const DRAIN_GRACE: Duration = Duration::from_secs(60);

const SHARD_PORT: u16 = 64090;
pub const STATUS_PORT: u16 = 8081;
const HEALTH_PORT: u16 = 8080;
const DGS_PORT: u16 = 50051;

const REPLICANT_CPU_MILLIS: u32 = 10;
const DGS_CPU_MILLIS: u32 = 5;
const POD_MEMORY_MIB: u32 = 16;
const POD_MEMORY_LIMIT_MIB: u32 = 128;
/// A DGS holds the collision of its hangars (Orbituary's alone decodes to 767k triangles) besides its area.
const DGS_MEMORY_LIMIT_MIB: u32 = 512;

/// The replicant's pod and Service. The namespace says which shard they belong to.
pub const REPLICANT_NAME: &str = "replicant-server";

pub fn dgs_pod_name(dgs: &str) -> String {
    format!("dgs-{dgs}")
}

/// A DGS's Service has its pod's name, so that the two cannot drift apart.
pub fn dgs_service_name(dgs: &str) -> String {
    dgs_pod_name(dgs)
}

/// How the replicant reaches a DGS, with `{name}` for its name. Empty for a shard in the operator's namespace: its DGSs
/// have no Services, as those names would clash with another shard's there, and it only waits to drain.
pub fn dgs_address(shard: &ShardSpec) -> String {
    match shard.namespace.as_str() {
        "" => String::new(),
        namespace => format!("http://{}.{namespace}.svc:{DGS_PORT}", dgs_service_name("{name}")),
    }
}

/// The game build of a shard: its own, or the change number in the id `pub_<cluster>_<build>_<number>` of a shard from
/// before shards kept it. Cluster names have no underscores, being Kubernetes names.
pub fn shard_build(shard: &ShardSpec) -> String {
    if !shard.build.is_empty() {
        return shard.build.clone();
    }
    let mut parts = shard.id.rsplitn(3, '_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(build), Some(_)) => build.to_string(),
        _ => String::new(),
    }
}

/// A shard's region and build, filled in from its cluster and id when it is from before shards kept them.
pub fn resolved(shard: &ShardSpec, cluster_region: &str) -> ShardSpec {
    let region = if shard.region.is_empty() { cluster_region.to_string() } else { shard.region.clone() };
    ShardSpec { region, build: shard_build(shard), ..shard.clone() }
}

pub fn node_port(cluster: &ShardClusterSpec, slot: u32) -> u32 {
    u32::from(cluster.node_port_base) + slot
}

/// A pod or Service as listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Existing {
    pub name: String,
    /// Has this Shard as an owner. Objects from before Shards existed have the ShardCluster instead.
    pub owned: bool,
    pub terminating: bool,
    /// AREA_ANNOTATION, on a DGS pod.
    pub area: String,
    /// POD_HASH_ANNOTATION, on a pod. Empty when it has none.
    pub pod_hash: String,
}

#[derive(Clone, Debug, Default)]
pub struct Observed {
    pub replicant: Option<Existing>,
    pub service: Option<Existing>,
    pub dgs: Vec<Existing>,
    /// The shard has a namespace of its own, the only place DGSs get Services and a policy.
    pub own_namespace: bool,
    pub dgs_services: Vec<Existing>,
    pub dgs_policy: Option<Existing>,
    /// Nobody is on the shard or on the way to it, see `replaceable`: its pods can be replaced to bring them to what the
    /// operator would make now. A shard with players keeps its pods, however old, until it drains.
    pub replaceable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    CreateService,
    CreateReplicant,
    CreateDgs(DgsAssignment),
    CreateDgsService(DgsAssignment),
    CreateDgsPolicy,
    DeletePod(String),
    DeleteService(String),
    AdoptPod(String),
    AdoptService(String),
}

/// What the pods of a shard should be, as `pod_hash` of the manifests the operator would make for them now.
pub struct Desired<'a> {
    /// None when the template is missing or invalid: the replicant's DGS areas are then unknown, so it is not compared.
    pub replicant: Option<String>,
    pub dgs: &'a dyn Fn(&DgsAssignment) -> String,
}

/// `dgs` is None when the template is missing or invalid: new work waits, and running DGSs stay. A name still
/// held by a terminating object waits for the next pass. A DGS pod whose area changed is replaced at once; a pod whose
/// manifest differs from `desired` otherwise, such as one an older operator made with other settings, only while the
/// shard is `replaceable`. It comes back under the same name in a later pass.
pub fn plan(observed: &Observed, dgs: Option<&[DgsAssignment]>, desired: &Desired) -> Vec<Action> {
    let mut actions = Vec::new();
    match &observed.service {
        None => actions.push(Action::CreateService),
        Some(service) if !service.owned && !service.terminating => actions.push(Action::AdoptService(service.name.clone())),
        Some(_) => {}
    }
    let drifted = |pod: &Existing, wanted: &str| observed.replaceable && !pod.terminating && pod.pod_hash != wanted;
    match &observed.replicant {
        None => actions.push(Action::CreateReplicant),
        Some(pod) if !pod.owned && !pod.terminating => actions.push(Action::AdoptPod(pod.name.clone())),
        Some(pod) if desired.replicant.as_deref().is_some_and(|wanted| drifted(pod, wanted)) => {
            actions.push(Action::DeletePod(pod.name.clone()));
        }
        Some(_) => {}
    }
    if observed.own_namespace && observed.dgs_policy.is_none() {
        actions.push(Action::CreateDgsPolicy);
    }
    let Some(dgs) = dgs else { return actions };
    for assignment in dgs {
        let name = dgs_pod_name(&assignment.name);
        match observed.dgs.iter().find(|pod| pod.name == name) {
            None => actions.push(Action::CreateDgs(assignment.clone())),
            Some(pod) if !pod.terminating && pod.area != assignment.area() => actions.push(Action::DeletePod(name)),
            Some(pod) if drifted(pod, &(desired.dgs)(assignment)) => actions.push(Action::DeletePod(name)),
            Some(_) => {}
        }
    }
    let wanted = |pod: &Existing| dgs.iter().any(|assignment| dgs_pod_name(&assignment.name) == pod.name);
    for pod in observed.dgs.iter().filter(|pod| !pod.terminating && !wanted(pod)) {
        actions.push(Action::DeletePod(pod.name.clone()));
    }
    // After the pods, and existing shards get these too: a quota that has not caught up fails only this part of a pass.
    if observed.own_namespace {
        for assignment in dgs {
            let name = dgs_service_name(&assignment.name);
            if !observed.dgs_services.iter().any(|service| service.name == name) {
                actions.push(Action::CreateDgsService(assignment.clone()));
            }
        }
        let wanted = |service: &Existing| dgs.iter().any(|assignment| dgs_service_name(&assignment.name) == service.name);
        for service in observed.dgs_services.iter().filter(|service| !service.terminating && !wanted(service)) {
            actions.push(Action::DeleteService(service.name.clone()));
        }
    }
    actions
}

/// Whether a shard's pods can be replaced: nobody is on it or reserved for it, and, as for a draining shard's close
/// (see `drained`), it has been seen empty long enough that a player sent before an operator restart has arrived.
pub fn replaceable(load: u32, running: bool, answering: bool, empty_for: Duration) -> bool {
    load == 0 && drained(running, answering, empty_for)
}

/// A stable hash (64-bit FNV-1a, hex) of a pod manifest as the operator makes it, without its owner references, which
/// name the Shard and not a setting. serde_json keeps object keys sorted, so equal manifests give equal text.
pub fn pod_hash(manifest: &Value) -> String {
    let mut manifest = manifest.clone();
    if let Some(metadata) = manifest.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("ownerReferences");
        let annotations = metadata.get_mut("annotations").and_then(Value::as_object_mut);
        // A replicant has no other annotation: the stamped manifest must hash as it did before the stamp.
        if annotations.is_some_and(|annotations| annotations.remove(POD_HASH_ANNOTATION).is_some() && annotations.is_empty()) {
            metadata.remove("annotations");
        }
    }
    let hash = manifest.to_string().bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// Puts `pod_hash` of the manifest in its POD_HASH_ANNOTATION.
fn stamped(mut manifest: Value) -> Value {
    let hash = pod_hash(&manifest);
    manifest["metadata"]["annotations"][POD_HASH_ANNOTATION] = Value::String(hash);
    manifest
}

/// Allocation depends only on the replicant. A client sent to a shard whose DGSs are not all up joins as far as they let
/// it, and waits there, see `link` in the replicant.
pub fn phase(answering: bool, dgs_ready: u32, dgs_desired: u32) -> ShardPhase {
    match answering {
        false => ShardPhase::Pending,
        true if dgs_desired > 0 && dgs_ready >= dgs_desired => ShardPhase::Running,
        true => ShardPhase::Degraded,
    }
}

/// A shard from an older game build, once a deploy has removed its pod. Its id cannot be reused, so the Shard
/// closes and a new one opens, as a shard did when each was only a pod.
pub fn outdated(shard: &ShardSpec, current_id: &str, has_replicant: bool) -> bool {
    shard.id != current_id && !has_replicant
}

/// A shard that takes no more players and closes once empty: one from before shards had namespaces, one of an older
/// game build, or one a deploy marked with DRAIN_ANNOTATION.
pub fn draining(shard: &ShardSpec, current_id: &str, marked: bool) -> bool {
    shard.namespace.is_empty() || shard.id != current_id || marked
}

/// Whether an empty draining shard can go. `running` is false when its replicant has no pod or never got an address,
/// so it cannot hold anyone. One that runs and does not answer may hold players, so it stays.
pub fn drained(running: bool, answering: bool, empty_for: Duration) -> bool {
    !running || (answering && empty_for >= DRAIN_GRACE)
}

/// A Namespace as listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedNamespace {
    /// SHARD_LABEL: the Shard it was made for.
    pub shard: Option<String>,
    pub terminating: bool,
}

/// A live Shard whose namespace is going or belongs to another Shard cannot run there, and waiting would keep the
/// slot from being replaced. It closes, and a new shard opens in a free slot. A missing namespace is simply created.
pub fn namespace_lost(shard: &str, namespace: Option<&ObservedNamespace>) -> bool {
    namespace.is_some_and(|ns| ns.terminating || ns.shard.as_deref() != Some(shard))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closing {
    DeleteNamespace,
    /// The namespace is Terminating. The Shard keeps its slot until it is gone, however long that takes.
    Wait,
    RemoveFinalizer,
}

/// The next step for a deleted Shard. A namespace not labelled as this Shard's is never deleted.
pub fn plan_closing(shard: &str, namespace: Option<&ObservedNamespace>) -> Closing {
    match namespace {
        Some(ns) if ns.shard.as_deref() != Some(shard) => Closing::RemoveFinalizer,
        Some(ns) if ns.terminating => Closing::Wait,
        Some(_) => Closing::DeleteNamespace,
        None => Closing::RemoveFinalizer,
    }
}

/// A pod of the cluster, for finding those from before Shards existed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterPod {
    pub name: String,
    pub slot: Option<u32>,
    pub id: Option<String>,
    pub owned_by_cluster: bool,
    pub terminating: bool,
}

/// Replicant pods that need a Shard made for them. A pod of a closed Shard, waiting for garbage collection, is
/// owned by that Shard and not the cluster, so it is not brought back.
pub fn to_adopt<'a>(pods: &'a [ClusterPod], shards: &[&str]) -> Vec<&'a ClusterPod> {
    let adoptable = |pod: &&ClusterPod| {
        pod.owned_by_cluster
            && !pod.terminating
            && pod.slot.is_some()
            && pod.id.is_some()
            && !shards.contains(&pod.name.as_str())
    };
    pods.iter().filter(adoptable).collect()
}

fn shard_labels(cluster: &str, name: &str, shard: &ShardSpec) -> BTreeMap<String, String> {
    BTreeMap::from([
        (CLUSTER_LABEL.to_string(), cluster.to_string()),
        (SHARD_LABEL.to_string(), name.to_string()),
        (SHARD_ID_LABEL.to_string(), shard.id.clone()),
        (SLOT_LABEL.to_string(), shard.slot.to_string()),
        (PART_OF_LABEL.to_string(), "scbackend".to_string()),
    ])
}

/// A label value allows more than a DNS label, but also has a limit and must start and end alphanumeric.
fn region_label(region: &str) -> String {
    let allowed = |c: &char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    let kept: String = region.chars().filter(allowed).take(63).collect();
    kept.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string()
}

pub fn namespace(shard_name: &str, shard: &ShardSpec, region: &str) -> Value {
    let mut labels = shard_labels(&shard.cluster, shard_name, shard);
    labels.insert(REGION_LABEL.to_string(), region_label(region));
    json!({ "metadata": { "name": shard.namespace, "labels": labels } })
}

/// One pod more than the shard runs, as a renamed DGS can start while the old pod is still stopping.
pub fn quota_pods(dgs: usize) -> u32 {
    dgs as u32 + 2
}

/// The replicant's, one per DGS and one more, as a renamed DGS's is made before the old one goes.
pub fn quota_services(dgs: usize) -> u32 {
    dgs as u32 + 2
}

/// Whether a namespace's quota is not the one of `dgs` DGSs. A shard opened before DGSs had Services has room for one
/// Service, whatever its pods say, so both counts matter.
pub fn quota_outdated(current: Option<&ResourceQuotaSpec>, dgs: usize) -> bool {
    let hard = current.and_then(|spec| spec.hard.as_ref());
    let count = |name: &str| hard.and_then(|hard| hard.get(name)).map(|count| count.0.as_str());
    count("pods") != Some(quota_pods(dgs).to_string().as_str())
        || count("services") != Some(quota_services(dgs).to_string().as_str())
        || count("limits.memory") != Some(quota_memory_limit(quota_pods(dgs)).as_str())
}

/// Comes from the pods of a shard, so it fits them and only stops what should not be there. No `limits.cpu`: these
/// pods set no CPU limit, and a quota on it would reject them.
pub fn quota(shard_name: &str, shard: &ShardSpec, dgs: usize) -> Value {
    let pods = quota_pods(dgs);
    json!({
        "apiVersion": "v1",
        "kind": "ResourceQuota",
        "metadata": { "name": QUOTA_NAME, "labels": shard_labels(&shard.cluster, shard_name, shard) },
        "spec": {
            "hard": {
                "pods": pods.to_string(),
                "requests.cpu": format!("{}m", REPLICANT_CPU_MILLIS + (pods - 1) * DGS_CPU_MILLIS),
                "requests.memory": format!("{}Mi", pods * POD_MEMORY_MIB),
                "limits.memory": quota_memory_limit(pods),
                "services": quota_services(dgs).to_string(),
                "services.nodeports": "1",
            },
        },
    })
}

/// The replicant's limit and a DGS's for every other pod, the replacement included.
fn quota_memory_limit(pods: u32) -> String {
    format!("{}Mi", POD_MEMORY_LIMIT_MIB + (pods - 1) * DGS_MEMORY_LIMIT_MIB)
}

fn resources(cpu_millis: u32, memory_limit_mib: u32) -> Value {
    json!({
        "requests": { "cpu": format!("{cpu_millis}m"), "memory": format!("{POD_MEMORY_MIB}Mi") },
        "limits": { "memory": format!("{memory_limit_mib}Mi") },
    })
}

/// `owner` is only for a shard in the operator's namespace. Inside its own, the namespace delete removes everything.
/// `dgs_areas` is `template::dgs_areas` of the shard's DGSs, which tells the replicant which DGS governs a player.
pub fn replicant_pod(name: &str, shard: &ShardSpec, image: &str, dgs_areas: &str, owner: Option<&OwnerReference>) -> Value {
    stamped(json!({
        "metadata": { "name": REPLICANT_NAME, "labels": shard_labels(&shard.cluster, name, shard), "ownerReferences": owner.map(|o| [o]) },
        "spec": {
            "automountServiceAccountToken": false,
            "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "seccompProfile": { "type": "RuntimeDefault" } },
            "containers": [{
                "name": "shard",
                "image": image,
                // Tags such as `dev` are reused, and a cached image would run old shard code.
                "imagePullPolicy": "Always",
                "env": [
                    { "name": "NO_COLOR", "value": "1" },
                    { "name": "SCBACKEND_SHARD_ID", "value": shard.id },
                    { "name": "SCBACKEND_SHARD_NAME", "value": shard.display_name },
                    { "name": "SCBACKEND_DGS_AREAS", "value": dgs_areas },
                    { "name": "SCBACKEND_DGS_ADDRESS", "value": dgs_address(shard) },
                    { "name": "SCBACKEND_LISTEN", "value": format!("0.0.0.0:{SHARD_PORT}") },
                    { "name": "SCBACKEND_STATUS", "value": format!("0.0.0.0:{STATUS_PORT}") },
                    { "name": "SCBACKEND_HEALTH", "value": format!("0.0.0.0:{HEALTH_PORT}") },
                ],
                "ports": [
                    { "name": "shard", "containerPort": SHARD_PORT, "protocol": "UDP" },
                    { "name": "health", "containerPort": HEALTH_PORT, "protocol": "TCP" },
                ],
                "readinessProbe": { "httpGet": { "path": "/", "port": "health" }, "periodSeconds": 2 },
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": { "drop": ["ALL"] },
                },
                "resources": resources(REPLICANT_CPU_MILLIS, POD_MEMORY_LIMIT_MIB),
            }],
        },
    }))
}

pub fn service(name: &str, shard: &ShardSpec, node_port: u32, owner: Option<&OwnerReference>) -> Value {
    json!({
        "metadata": { "name": REPLICANT_NAME, "labels": shard_labels(&shard.cluster, name, shard), "ownerReferences": owner.map(|o| [o]) },
        "spec": {
            "type": "NodePort",
            "selector": { SHARD_LABEL: name },
            "ports": [{ "name": "shard", "port": SHARD_PORT, "targetPort": "shard", "protocol": "UDP", "nodePort": node_port }],
        },
    })
}

fn dgs_labels<'a>(shard_name: &'a str, shard: &'a ShardSpec, dgs: &'a str) -> BTreeMap<&'a str, &'a str> {
    BTreeMap::from([
        (CLUSTER_LABEL, shard.cluster.as_str()),
        (DGS_SHARD_LABEL, shard_name),
        (DGS_LABEL, dgs),
        (SHARD_ID_LABEL, shard.id.as_str()),
        (PART_OF_LABEL, "scbackend"),
    ])
}

/// One DGS, at a name that outlives its pod. It selects by labels DGS pods have always had, so it serves the pods of a
/// shard opened before these Services. Only the Service of `service` carries SHARD_LABEL and SLOT_LABEL: those say which
/// is the replicant's, and which slot a shard holds.
pub fn dgs_service(shard_name: &str, shard: &ShardSpec, dgs: &DgsAssignment) -> Value {
    json!({
        "metadata": { "name": dgs_service_name(&dgs.name), "labels": dgs_labels(shard_name, shard, &dgs.name) },
        "spec": {
            "type": "ClusterIP",
            "selector": { DGS_SHARD_LABEL: shard_name, DGS_LABEL: dgs.name },
            "ports": [{ "name": "grpc", "port": DGS_PORT, "targetPort": "grpc", "protocol": "TCP" }],
        },
    })
}

/// Selecting the DGS pods makes them refuse all that is not listed, and the one thing listed is the replicant on the
/// gRPC port. The node's own traffic, which is the kubelet's probes, is not subject to a policy. Ingress only: the DGSs
/// still reach DNS, their region's sc-entitygraph, sc-character-customizer and sc-identity, and the replicant's pod has no policy, which keeps its
/// clients and the operator's reads. A DGS has no database access: it is given no database address.
pub fn dgs_policy(shard_name: &str, shard: &ShardSpec) -> Value {
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": DGS_POLICY_NAME, "labels": shard_labels(&shard.cluster, shard_name, shard) },
        "spec": {
            "podSelector": { "matchLabels": { DGS_SHARD_LABEL: shard_name } },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [{ "podSelector": { "matchLabels": { SHARD_LABEL: shard_name } } }],
                "ports": [{ "protocol": "TCP", "port": DGS_PORT }],
            }],
        },
    })
}

/// What every DGS pod runs and reaches, from the chart.
pub struct DgsSettings {
    pub image: String,
    /// `sc-character-customizer`, which DGSs ask for saved characters. A full service name: DGS pods are in namespaces of
    /// their own.
    pub customizer: String,
    /// `sc-identity`, which DGSs ask for an account's in-game badges, for the player's bind. A full service name.
    pub identity: String,
}

pub fn dgs_pod(shard_name: &str, shard: &ShardSpec, dgs: &DgsAssignment, settings: &DgsSettings, owner: Option<&OwnerReference>) -> Value {
    let labels = dgs_labels(shard_name, shard, &dgs.name);
    let mut env = vec![
        json!({ "name": "NO_COLOR", "value": "1" }),
        json!({ "name": "SCBACKEND_SHARD_ID", "value": shard.id }),
        json!({ "name": "SCBACKEND_DGS_NAME", "value": dgs.name }),
        json!({ "name": "SCBACKEND_DGS_SYSTEM", "value": dgs.system }),
        json!({ "name": "SCBACKEND_DGS_ROOTS", "value": dgs.roots.join(",") }),
        json!({ "name": "SCBACKEND_LISTEN", "value": format!("0.0.0.0:{DGS_PORT}") }),
        json!({ "name": "SCBACKEND_HEALTH", "value": format!("0.0.0.0:{HEALTH_PORT}") }),
        json!({ "name": "SCBACKEND_CUSTOMIZER", "value": settings.customizer }),
        json!({ "name": "SCBACKEND_IDENTITY", "value": settings.identity }),
    ];
    // The region's sc-entitygraph, which walks each DGS's area from the container tree it owns, in front of the region's
    // entity database. A full service name: DGS pods are in namespaces of their own. See `resolved`.
    if !shard.region.is_empty() {
        env.push(json!({ "name": "SCBACKEND_ENTITYGRAPH", "value": entitygraph_address(&shard.region) }));
    }
    // The region's cache, which serves the game data of the shard's build.
    if !shard.region.is_empty() && !shard.build.is_empty() {
        env.push(json!({ "name": "SC_GAMEDATA_ADDR", "value": cache_address(&shard.region) }));
        env.push(json!({ "name": "SC_GAME_BUILD", "value": shard.build }));
    }
    // An empty variable would reach the DGS as one empty root.
    if !dgs.excluded.is_empty() {
        env.push(json!({ "name": "SCBACKEND_DGS_EXCLUDED_ROOTS", "value": dgs.excluded.join(",") }));
    }
    stamped(json!({
        "metadata": {
            "name": dgs_pod_name(&dgs.name),
            "labels": labels,
            "annotations": { AREA_ANNOTATION: dgs.area() },
            "ownerReferences": owner.map(|o| [o]),
        },
        "spec": {
            "automountServiceAccountToken": false,
            "securityContext": { "runAsNonRoot": true, "runAsUser": 65532, "seccompProfile": { "type": "RuntimeDefault" } },
            "containers": [{
                "name": "dgs",
                "image": settings.image,
                // As for the replicant.
                "imagePullPolicy": "Always",
                "env": env,
                "ports": [
                    { "name": "grpc", "containerPort": DGS_PORT, "protocol": "TCP" },
                    { "name": "health", "containerPort": HEALTH_PORT, "protocol": "TCP" },
                ],
                // The DGS opens this port once its area is loaded. The health port answers from the start.
                "readinessProbe": { "tcpSocket": { "port": "grpc" }, "periodSeconds": 2 },
                "livenessProbe": { "httpGet": { "path": "/", "port": "health" }, "periodSeconds": 30 },
                "securityContext": {
                    "allowPrivilegeEscalation": false,
                    "readOnlyRootFilesystem": true,
                    "capabilities": { "drop": ["ALL"] },
                },
                "resources": resources(DGS_CPU_MILLIS, DGS_MEMORY_LIMIT_MIB),
            }],
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ShardSpec {
        ShardSpec {
            cluster: "usa".into(),
            id: "pub_usa_12913470_010".into(),
            display_name: "usa_crimson_falcon".into(),
            slot: 0,
            template: "default".into(),
            namespace: "scbackend-shard-usa-010".into(),
            region: "usa".into(),
            build: "12913470".into(),
        }
    }

    fn settings() -> DgsSettings {
        DgsSettings {
            image: "image".into(),
            customizer: "http://sc-character-customizer.scbackend.svc:50051".into(),
            identity: "http://sc-identity.scbackend.svc:50051".into(),
        }
    }

    fn dgs(name: &str) -> DgsAssignment {
        DgsAssignment { name: name.into(), system: "stanton".into(), roots: vec!["stanton1".into()], excluded: vec![] }
    }

    fn existing(name: &str) -> Option<Existing> {
        Some(Existing { name: name.into(), owned: true, pod_hash: "replicant".into(), ..Existing::default() })
    }

    fn hash_of(assignment: &DgsAssignment) -> String {
        format!("hash-{}", assignment.name)
    }

    /// What the operator would make now: the pods of `existing` and `pod_of` match it.
    fn plan_now(observed: &Observed, dgs: Option<&[DgsAssignment]>) -> Vec<Action> {
        plan(observed, dgs, &Desired { replicant: Some("replicant".into()), dgs: &hash_of })
    }

    fn pod_of(name: &str, assignment: &DgsAssignment) -> Existing {
        Existing { name: dgs_pod_name(name), owned: true, area: assignment.area(), pod_hash: hash_of(assignment), ..Existing::default() }
    }

    fn complete(assignment: &DgsAssignment) -> Observed {
        Observed { replicant: existing("s"), service: existing("s"), dgs: vec![pod_of(&assignment.name, assignment)], ..Observed::default() }
    }

    #[test]
    fn a_new_shard_gets_everything() {
        let wanted = [dgs("stanton-1"), dgs("nyx-1")];
        let actions = plan_now(&Observed::default(), Some(&wanted));
        assert_eq!(
            actions,
            [
                Action::CreateService,
                Action::CreateReplicant,
                Action::CreateDgs(wanted[0].clone()),
                Action::CreateDgs(wanted[1].clone()),
            ]
        );
    }

    #[test]
    fn a_complete_shard_needs_nothing() {
        let wanted = dgs("nyx-1");
        assert_eq!(plan_now(&complete(&wanted), Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn a_pod_removed_by_a_deploy_comes_back_under_the_same_name() {
        let wanted = dgs("nyx-1");
        let observed = Observed { replicant: None, service: None, ..complete(&wanted) };
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::CreateService, Action::CreateReplicant]);
    }

    #[test]
    fn a_name_still_terminating_waits() {
        let wanted = dgs("nyx-1");
        let mut observed = complete(&wanted);
        for object in [&mut observed.replicant, &mut observed.service] {
            *object = Some(Existing { name: "s".into(), terminating: true, ..Existing::default() });
        }
        observed.dgs[0].terminating = true;
        observed.dgs[0].area = "old".into();
        assert_eq!(plan_now(&observed, Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn objects_from_before_shards_are_adopted_and_kept_running() {
        let wanted = dgs("nyx-1");
        let observed = Observed {
            replicant: Some(Existing { name: "s".into(), ..Existing::default() }),
            service: Some(Existing { name: "s".into(), ..Existing::default() }),
            ..Observed::default()
        };
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(
            actions,
            [Action::AdoptService("s".into()), Action::AdoptPod("s".into()), Action::CreateDgs(wanted.clone())]
        );
    }

    #[test]
    fn a_changed_area_replaces_the_pod() {
        let wanted = dgs("nyx-1");
        let mut observed = complete(&wanted);
        observed.dgs[0].area = "old".into();
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("dgs-nyx-1".into())]);
    }

    #[test]
    fn a_changed_area_replaces_the_pod_even_with_players() {
        let wanted = dgs("nyx-1");
        let mut observed = complete(&wanted);
        observed.dgs[0].area = "old".into();
        observed.dgs[0].pod_hash = "old".into();
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("dgs-nyx-1".into())]);
    }

    fn empty(assignment: &DgsAssignment) -> Observed {
        Observed { replaceable: true, ..complete(assignment) }
    }

    #[test]
    fn a_pod_from_other_settings_is_replaced_on_an_empty_shard() {
        let wanted = dgs("nyx-1");
        let mut observed = empty(&wanted);
        observed.dgs[0].pod_hash = "old".into();
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("dgs-nyx-1".into())]);
    }

    #[test]
    fn a_pod_from_other_settings_runs_on_while_the_shard_has_players() {
        let wanted = dgs("nyx-1");
        let mut observed = complete(&wanted);
        observed.dgs[0].pod_hash = "old".into();
        observed.replicant.as_mut().unwrap().pod_hash = "old".into();
        assert_eq!(plan_now(&observed, Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn a_pod_from_before_the_hash_is_replaced_on_an_empty_shard() {
        let wanted = dgs("nyx-1");
        let mut observed = empty(&wanted);
        observed.dgs[0].pod_hash = String::new();
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("dgs-nyx-1".into())]);
    }

    #[test]
    fn an_unchanged_pod_of_an_empty_shard_is_left_alone() {
        let wanted = dgs("nyx-1");
        assert_eq!(plan_now(&empty(&wanted), Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn a_replicant_from_other_settings_is_replaced_only_on_an_empty_shard() {
        let wanted = dgs("nyx-1");
        let mut observed = empty(&wanted);
        observed.replicant.as_mut().unwrap().pod_hash = String::new();
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("s".into())]);
        let unknown = Desired { replicant: None, dgs: &hash_of };
        assert_eq!(plan(&observed, Some(std::slice::from_ref(&wanted)), &unknown), [], "no template, so its areas are unknown");
    }

    #[test]
    fn a_terminating_pod_of_an_empty_shard_waits() {
        let wanted = dgs("nyx-1");
        let mut observed = empty(&wanted);
        observed.dgs[0].pod_hash = "old".into();
        observed.dgs[0].terminating = true;
        assert_eq!(plan_now(&observed, Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn only_a_shard_nobody_is_on_or_on_the_way_to_is_replaceable() {
        assert!(replaceable(0, true, true, DRAIN_GRACE));
        assert!(!replaceable(1, true, true, DRAIN_GRACE), "a player or a reservation");
        assert!(!replaceable(0, true, true, DRAIN_GRACE - Duration::from_secs(1)), "a player may be on the way");
        assert!(!replaceable(0, true, false, DRAIN_GRACE), "it may hold players it cannot report");
        assert!(replaceable(0, false, false, Duration::ZERO), "nothing is running");
    }

    #[test]
    fn a_pod_carries_the_hash_of_its_manifest() {
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let stamp = pod["metadata"]["annotations"][POD_HASH_ANNOTATION].as_str().unwrap();
        assert_eq!(stamp, pod_hash(&pod));
        assert_eq!(pod["metadata"]["annotations"][AREA_ANNOTATION], dgs("nyx-1").area());
        let replicant = replicant_pod("s", &spec(), "image", "", None);
        assert_eq!(replicant["metadata"]["annotations"][POD_HASH_ANNOTATION].as_str().unwrap(), pod_hash(&replicant));
        serde_json::from_value::<k8s_openapi::api::core::v1::Pod>(replicant).unwrap();
    }

    #[test]
    fn the_hash_follows_settings_region_and_shard_but_not_the_owner() {
        let hash = |shard: &ShardSpec, settings: &DgsSettings, owner: Option<&OwnerReference>| {
            pod_hash(&dgs_pod("s", shard, &dgs("nyx-1"), settings, owner))
        };
        let base = hash(&spec(), &settings(), None);
        let owner = OwnerReference { name: "s".into(), uid: "uid".into(), ..OwnerReference::default() };
        assert_eq!(base, hash(&spec(), &settings(), Some(&owner)));
        assert_ne!(base, hash(&spec(), &DgsSettings { image: "other".into(), ..settings() }, None));
        assert_ne!(base, hash(&spec(), &DgsSettings { customizer: "other".into(), ..settings() }, None));
        assert_ne!(base, hash(&ShardSpec { region: "eu".into(), ..spec() }, &settings(), None));
        assert_ne!(base, hash(&ShardSpec { build: "1".into(), ..spec() }, &settings(), None));
        assert_eq!(base, hash(&spec(), &settings(), None), "the same manifest, the same hash");
    }

    #[test]
    fn a_dgs_gone_from_the_template_is_removed() {
        let wanted = dgs("nyx-1");
        let mut observed = complete(&wanted);
        observed.dgs.push(pod_of("pyro-1", &dgs("pyro-1")));
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::DeletePod("dgs-pyro-1".into())]);
    }

    #[test]
    fn no_usable_template_leaves_the_dgs_pods_alone_and_still_serves_clients() {
        let mut observed = complete(&dgs("nyx-1"));
        observed.replicant = None;
        assert_eq!(plan_now(&observed, None), [Action::CreateReplicant]);
    }

    fn in_a_namespace(assignment: &DgsAssignment) -> Observed {
        Observed {
            own_namespace: true,
            dgs_services: vec![Existing { name: dgs_service_name(&assignment.name), ..Existing::default() }],
            dgs_policy: existing(DGS_POLICY_NAME),
            ..complete(assignment)
        }
    }

    #[test]
    fn a_new_shard_gets_a_service_per_dgs_and_the_policy() {
        let wanted = [dgs("stanton-1"), dgs("nyx-1")];
        let observed = Observed { own_namespace: true, ..Observed::default() };
        let actions = plan_now(&observed, Some(&wanted));
        assert_eq!(
            actions,
            [
                Action::CreateService,
                Action::CreateReplicant,
                Action::CreateDgsPolicy,
                Action::CreateDgs(wanted[0].clone()),
                Action::CreateDgs(wanted[1].clone()),
                Action::CreateDgsService(wanted[0].clone()),
                Action::CreateDgsService(wanted[1].clone()),
            ]
        );
    }

    #[test]
    fn a_complete_namespace_needs_nothing() {
        let wanted = dgs("nyx-1");
        assert_eq!(plan_now(&in_a_namespace(&wanted), Some(std::slice::from_ref(&wanted))), []);
    }

    #[test]
    fn a_running_shard_from_before_these_objects_gets_them_and_keeps_its_pods() {
        let wanted = dgs("nyx-1");
        let observed = Observed { own_namespace: true, ..complete(&wanted) };
        let actions = plan_now(&observed, Some(std::slice::from_ref(&wanted)));
        assert_eq!(actions, [Action::CreateDgsPolicy, Action::CreateDgsService(wanted.clone())]);
    }

    #[test]
    fn a_shard_in_the_operator_namespace_gets_neither() {
        let wanted = dgs("nyx-1");
        assert_eq!(plan_now(&complete(&wanted), Some(std::slice::from_ref(&wanted))), []);
        assert_eq!(dgs_address(&ShardSpec { namespace: String::new(), ..spec() }), "");
    }

    #[test]
    fn the_service_of_a_dgs_gone_from_the_template_goes_with_it() {
        let wanted = dgs("nyx-1");
        let mut observed = in_a_namespace(&wanted);
        observed.dgs_services.push(Existing { name: "dgs-pyro-1".into(), ..Existing::default() });
        observed.dgs_services.push(Existing { name: "dgs-pyro-2".into(), terminating: true, ..Existing::default() });
        assert_eq!(plan_now(&observed, Some(std::slice::from_ref(&wanted))), [Action::DeleteService("dgs-pyro-1".into())]);
    }

    #[test]
    fn no_usable_template_leaves_dgs_services_alone_and_keeps_the_policy() {
        let mut observed = in_a_namespace(&dgs("nyx-1"));
        observed.dgs_policy = None;
        assert_eq!(plan_now(&observed, None), [Action::CreateDgsPolicy]);
    }

    #[test]
    fn a_dgs_service_selects_its_pod_and_only_that() {
        let assignment = dgs("nyx-1");
        let pod = dgs_pod("s", &spec(), &assignment, &settings(), None);
        let other = dgs_pod("s", &spec(), &dgs("pyro-1"), &settings(), None);
        let replicant = replicant_pod("s", &spec(), "image", "", None);
        let service = dgs_service("s", &spec(), &assignment);
        let selector = service["spec"]["selector"].as_object().unwrap();
        let selected = |pod: &Value| selector.iter().all(|(key, value)| pod["metadata"]["labels"].get(key) == Some(value));
        assert!(selected(&pod));
        assert!(!selected(&other));
        assert!(!selected(&replicant));
        assert_eq!(service["metadata"]["name"], pod["metadata"]["name"]);
        assert_eq!(service["spec"]["ports"][0]["port"], pod["spec"]["containers"][0]["ports"][0]["containerPort"]);
        assert_eq!(service["spec"]["ports"][0]["targetPort"], pod["spec"]["containers"][0]["ports"][0]["name"]);
        serde_json::from_value::<k8s_openapi::api::core::v1::Service>(service).unwrap();
    }

    #[test]
    fn a_dgs_service_is_not_taken_for_the_replicants_or_for_a_slot() {
        let service = dgs_service("s", &spec(), &dgs("nyx-1"));
        let labels = &service["metadata"]["labels"];
        assert!(labels.get(SHARD_LABEL).is_none(), "the operator finds the replicant's Service by this");
        assert!(labels.get(SLOT_LABEL).is_none());
        assert!(labels.get(CLUSTER_LABEL).is_some(), "the operator lists by this");
    }

    #[test]
    fn the_replicant_alone_reaches_the_dgs_port() {
        let policy = dgs_policy("s", &spec());
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let replicant = replicant_pod("s", &spec(), "image", "", None);
        let spec_of = &policy["spec"];
        let matches = |selector: &Value, pod: &Value| {
            selector["matchLabels"].as_object().unwrap().iter().all(|(key, value)| pod["metadata"]["labels"].get(key) == Some(value))
        };
        assert!(matches(&spec_of["podSelector"], &pod));
        assert!(!matches(&spec_of["podSelector"], &replicant), "the replicant takes clients and the operator's reads");
        assert_eq!(spec_of["policyTypes"], json!(["Ingress"]));
        assert_eq!(spec_of["ingress"].as_array().unwrap().len(), 1);
        let rule = &spec_of["ingress"][0];
        assert_eq!(rule["from"].as_array().unwrap().len(), 1);
        assert!(matches(&rule["from"][0]["podSelector"], &replicant));
        assert!(!matches(&rule["from"][0]["podSelector"], &pod), "a DGS cannot call another");
        assert!(rule["from"][0].get("namespaceSelector").is_none(), "only this namespace");
        assert_eq!(rule["ports"], json!([{ "protocol": "TCP", "port": DGS_PORT }]));
        serde_json::from_value::<k8s_openapi::api::networking::v1::NetworkPolicy>(policy).unwrap();
    }

    #[test]
    fn the_replicant_is_told_how_to_reach_a_dgs_of_its_own_namespace() {
        let pod = replicant_pod("s", &spec(), "image", "", None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        let address = "http://dgs-{name}.scbackend-shard-usa-010.svc:50051";
        assert!(env.contains(&json!({ "name": "SCBACKEND_DGS_ADDRESS", "value": address })));
        assert_eq!(address.replace("{name}", "nyx-1"), format!("http://{}.scbackend-shard-usa-010.svc:{DGS_PORT}", dgs_service_name("nyx-1")));
    }

    #[test]
    fn phases() {
        assert_eq!(phase(false, 10, 10), ShardPhase::Pending);
        assert_eq!(phase(true, 10, 10), ShardPhase::Running);
        assert_eq!(phase(true, 9, 10), ShardPhase::Degraded);
        assert_eq!(phase(true, 0, 0), ShardPhase::Degraded);
    }

    #[test]
    fn only_a_shard_of_an_old_build_with_no_pod_closes() {
        let current = "pub_usa_12913470_010";
        assert!(!outdated(&spec(), current, false));
        let old = ShardSpec { id: "pub_usa_12000000_010".into(), ..spec() };
        assert!(!outdated(&old, current, true));
        assert!(outdated(&old, current, false));
    }

    fn legacy(name: &str) -> ClusterPod {
        ClusterPod { name: name.into(), slot: Some(0), id: Some("id".into()), owned_by_cluster: true, terminating: false }
    }

    #[test]
    fn adopts_only_replicants_the_cluster_still_owns_with_no_shard() {
        let closed = ClusterPod { owned_by_cluster: false, ..legacy("closed") };
        let leaving = ClusterPod { terminating: true, ..legacy("leaving") };
        let dgs = ClusterPod { slot: None, id: None, owned_by_cluster: false, ..legacy("dgs-nyx-1") };
        let pods = [legacy("old"), legacy("done"), closed, leaving, dgs];
        let adopted = to_adopt(&pods, &["done"]);
        assert_eq!(adopted, [&pods[0]]);
    }

    #[test]
    fn dgs_pods_stay_out_of_the_shard_service() {
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let selector = service("s", &spec(), 30090, None)["spec"]["selector"].clone();
        let (key, _) = selector.as_object().unwrap().iter().next().unwrap();
        assert!(pod["metadata"]["labels"].get(key).is_none());
        assert!(pod["metadata"]["labels"].get(SLOT_LABEL).is_none());
        assert_eq!(pod["metadata"]["name"], "dgs-nyx-1");
        assert!(pod["metadata"]["labels"].get(CLUSTER_LABEL).is_some(), "the operator lists by this");
    }

    #[test]
    fn the_replicant_keeps_the_labels_and_names_it_always_had() {
        let pod = replicant_pod("usa-crimson-falcon", &spec(), "image", "pyro:pyro-1=pyro3", None);
        let labels = &pod["metadata"]["labels"];
        assert_eq!(labels[SHARD_LABEL], "usa-crimson-falcon");
        assert_eq!(labels[SLOT_LABEL], "0");
        assert_eq!(labels[SHARD_ID_LABEL], "pub_usa_12913470_010");
        let env = &pod["spec"]["containers"][0]["env"];
        assert!(env.as_array().unwrap().contains(&json!({ "name": "SCBACKEND_SHARD_NAME", "value": "usa_crimson_falcon" })));
        assert!(env.as_array().unwrap().contains(&json!({ "name": "SCBACKEND_DGS_AREAS", "value": "pyro:pyro-1=pyro3" })));
    }

    #[test]
    fn a_dgs_with_nothing_excluded_gets_no_variable() {
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        assert!(!env.iter().any(|variable| variable["name"] == "SCBACKEND_DGS_EXCLUDED_ROOTS"));
    }

    #[test]
    fn a_dgs_gets_full_service_names_and_no_database() {
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        let entitygraph = "http://sc-entitygraph.scbackend-region-usa.svc:50051";
        assert!(env.contains(&json!({ "name": "SCBACKEND_ENTITYGRAPH", "value": entitygraph })), "its region's");
        assert!(!env.iter().any(|var| var["name"] == "SCBACKEND_ARANGO"), "a DGS reads no database");
        let customizer = "http://sc-character-customizer.scbackend.svc:50051";
        assert!(env.contains(&json!({ "name": "SCBACKEND_CUSTOMIZER", "value": customizer })));
        assert!(env.contains(&json!({ "name": "SCBACKEND_IDENTITY", "value": "http://sc-identity.scbackend.svc:50051" })));
        assert!(!env.iter().any(|var| var["name"] == "SCBACKEND_POSTGRES"), "a DGS gets no database of saved characters");
    }

    #[test]
    fn only_a_shard_in_the_operator_namespace_has_owner_references() {
        let owner = OwnerReference::default();
        let manifests = |owner| {
            [
                replicant_pod("s", &spec(), "image", "", owner),
                service("s", &spec(), 30090, owner),
                dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), owner),
            ]
        };
        for manifest in manifests(None) {
            let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(manifest).unwrap();
            assert!(pod.metadata.owner_references.is_none());
        }
        for manifest in manifests(Some(&owner)) {
            let pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(manifest).unwrap();
            assert_eq!(pod.metadata.owner_references.unwrap().len(), 1);
        }
    }

    #[test]
    fn shard_pods_need_no_service_account_token() {
        assert_eq!(replicant_pod("s", &spec(), "image", "", None)["spec"]["automountServiceAccountToken"], false);
        assert_eq!(dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None)["spec"]["automountServiceAccountToken"], false);
    }

    #[test]
    fn a_namespace_carries_the_labels_it_is_found_by() {
        let namespace = namespace("usa-crimson-falcon", &spec(), "usa");
        assert_eq!(namespace["metadata"]["name"], "scbackend-shard-usa-010");
        let labels = &namespace["metadata"]["labels"];
        assert_eq!(labels[PART_OF_LABEL], "scbackend");
        assert_eq!(labels[CLUSTER_LABEL], "usa");
        assert_eq!(labels[SHARD_LABEL], "usa-crimson-falcon");
        assert_eq!(labels[REGION_LABEL], "usa");
        assert_eq!(labels[SLOT_LABEL], "0");
        serde_json::from_value::<k8s_openapi::api::core::v1::Namespace>(namespace).unwrap();
    }

    #[test]
    fn a_region_label_is_always_a_valid_label_value() {
        assert_eq!(region_label("EU_West.1"), "EU_West.1");
        assert_eq!(region_label("-usa west_"), "usawest");
        assert_eq!(region_label(&"a".repeat(80)).len(), 63);
    }

    fn millis(value: &Value) -> u32 {
        value.as_str().unwrap().trim_end_matches('m').parse().unwrap()
    }

    fn mebibytes(value: &Value) -> u32 {
        value.as_str().unwrap().trim_end_matches("Mi").parse().unwrap()
    }

    #[test]
    fn the_quota_fits_a_whole_shard_and_one_replacement_pod() {
        let count = 10;
        let replicant = replicant_pod("s", &spec(), "image", "", None);
        let dgs_pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let replicant = &replicant["spec"]["containers"][0]["resources"];
        let dgs_pod = &dgs_pod["spec"]["containers"][0]["resources"];
        let total = |of: fn(&Value) -> u32, path: [&str; 2]| {
            of(&replicant[path[0]][path[1]]) + (count + 1) * of(&dgs_pod[path[0]][path[1]])
        };
        let quota = quota("s", &spec(), count as usize);
        let hard = &quota["spec"]["hard"];
        assert_eq!(hard["pods"], "12");
        assert_eq!(hard["services"], "12", "the replicant's, one per DGS and one replacement");
        assert_eq!(hard["services.nodeports"], "1");
        assert_eq!(millis(&hard["requests.cpu"]), total(millis, ["requests", "cpu"]));
        assert_eq!(mebibytes(&hard["requests.memory"]), total(mebibytes, ["requests", "memory"]));
        assert_eq!(mebibytes(&hard["limits.memory"]), total(mebibytes, ["limits", "memory"]));
        assert!(hard.get("limits.cpu").is_none());
        serde_json::from_value::<k8s_openapi::api::core::v1::ResourceQuota>(quota).unwrap();
    }

    fn quota_spec(count: usize) -> ResourceQuotaSpec {
        let quota: k8s_openapi::api::core::v1::ResourceQuota = serde_json::from_value(quota("s", &spec(), count)).unwrap();
        quota.spec.unwrap()
    }

    #[test]
    fn a_quota_from_before_dgs_services_is_brought_up_to_date() {
        let mut old = quota_spec(10);
        assert!(!quota_outdated(Some(&old), 10));
        let hard = old.hard.as_mut().unwrap();
        hard.insert("services".to_string(), k8s_openapi::apimachinery::pkg::api::resource::Quantity("1".to_string()));
        assert!(quota_outdated(Some(&old), 10), "its pods are right, and no DGS Service would fit");
    }

    #[test]
    fn a_quota_follows_the_template_and_a_missing_one_is_made() {
        assert!(quota_outdated(Some(&quota_spec(10)), 9));
        assert!(quota_outdated(None, 10));
    }

    fn ns(shard: &str, terminating: bool) -> ObservedNamespace {
        ObservedNamespace { shard: Some(shard.into()), terminating }
    }

    #[test]
    fn closing_deletes_the_namespace_then_waits_then_lets_the_shard_go() {
        assert_eq!(plan_closing("s", Some(&ns("s", false))), Closing::DeleteNamespace);
        assert_eq!(plan_closing("s", Some(&ns("s", true))), Closing::Wait);
        assert_eq!(plan_closing("s", None), Closing::RemoveFinalizer);
    }

    #[test]
    fn closing_never_deletes_a_namespace_that_is_not_the_shards() {
        assert_eq!(plan_closing("s", Some(&ns("other", false))), Closing::RemoveFinalizer);
        assert_eq!(plan_closing("s", Some(&ns("other", true))), Closing::RemoveFinalizer);
        assert_eq!(plan_closing("s", Some(&ObservedNamespace::default())), Closing::RemoveFinalizer);
    }

    #[test]
    fn a_shard_gives_up_a_namespace_it_cannot_use() {
        assert!(!namespace_lost("s", None));
        assert!(!namespace_lost("s", Some(&ns("s", false))));
        assert!(namespace_lost("s", Some(&ns("s", true))));
        assert!(namespace_lost("s", Some(&ns("old", true))));
        assert!(namespace_lost("s", Some(&ns("old", false))));
        assert!(namespace_lost("s", Some(&ObservedNamespace::default())));
    }

    #[test]
    fn old_layouts_old_builds_and_marked_shards_drain() {
        let current = "pub_usa_12913470_010";
        assert!(!draining(&spec(), current, false));
        assert!(draining(&spec(), current, true));
        assert!(draining(&ShardSpec { namespace: String::new(), ..spec() }, current, false));
        assert!(draining(&ShardSpec { id: "pub_usa_12000000_010".into(), ..spec() }, current, false));
    }

    #[test]
    fn a_draining_shard_goes_only_when_nobody_can_be_on_it() {
        let settled = DRAIN_GRACE;
        assert!(drained(true, true, settled));
        assert!(!drained(true, true, settled - Duration::from_secs(1)), "a player may be on the way");
        assert!(!drained(true, false, settled), "it may hold players it cannot report");
        assert!(drained(false, false, Duration::ZERO), "nothing is running");
    }

    #[test]
    fn a_dgs_reaches_its_regions_cache_for_its_build() {
        let pod = dgs_pod("s", &spec(), &dgs("nyx-1"), &settings(), None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        let address = "http://sc-gamedata.scbackend-region-usa.svc:50051";
        assert!(env.contains(&json!({ "name": "SC_GAMEDATA_ADDR", "value": address })));
        assert!(env.contains(&json!({ "name": "SC_GAME_BUILD", "value": "12913470" })));
    }

    #[test]
    fn a_dgs_of_an_old_shard_gets_the_build_of_its_shard_not_the_clusters() {
        let old = ShardSpec { id: "pub_usa_12000000_020".into(), region: String::new(), build: String::new(), ..spec() };
        let old = resolved(&old, "usa");
        assert_eq!((old.region.as_str(), old.build.as_str()), ("usa", "12000000"));
        let pod = dgs_pod("s", &old, &dgs("nyx-1"), &settings(), None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        assert!(env.contains(&json!({ "name": "SC_GAME_BUILD", "value": "12000000" })));
        let moved = resolved(&ShardSpec { region: "eu".into(), ..spec() }, "usa");
        assert_eq!(moved.region, "eu", "a shard keeps the region it opened in");
    }

    #[test]
    fn a_build_is_read_from_an_id_only_when_the_shard_lacks_it() {
        assert_eq!(shard_build(&spec()), "12913470");
        assert_eq!(shard_build(&ShardSpec { build: String::new(), ..spec() }), "12913470");
        assert_eq!(shard_build(&ShardSpec { build: String::new(), id: "odd".into(), ..spec() }), "");
    }

    #[test]
    fn a_dgs_without_a_region_gets_no_cache() {
        let pod = dgs_pod("s", &ShardSpec { region: String::new(), ..spec() }, &dgs("nyx-1"), &settings(), None);
        let env = pod["spec"]["containers"][0]["env"].as_array().unwrap();
        assert!(!env.iter().any(|var| var["name"] == "SC_GAMEDATA_ADDR"));
    }

    #[test]
    fn dgs_names_become_pod_names() {
        assert_eq!(dgs_pod_name("stanton-1"), "dgs-stanton-1");
    }
}

//! What one ShardRegion is made of, and whether a shard's game build is ready in it. Kept free of the Kubernetes client
//! like shard.rs: the operator lists what exists and applies these manifests.
//!
//! A region is a namespace `scbackend-region-<name>` holding its game data:
//!
//! - a Valkey cluster, `gamedata-valkey`: a StatefulSet of `primaries * (1 + replicasPerPrimary)` nodes, each with a
//!   ReadWriteOnce volume it persists to (append-only file), a headless Service whose pod names the nodes announce, a
//!   ConfigMap with `valkey.conf`, `start.sh` and `init.sh` (in `valkey/` beside this crate's sources), a Job that forms
//!   the cluster or adds the nodes a raise brought, and a NetworkPolicy that lets only `sc-gamedata`, the nodes and the
//!   Job reach them;
//! - `sc-gamedata`, a stateless Deployment and its Service, the only reader and writer of the game data keys, which
//!   loads every build it is given from the registry into Valkey and serves the DGSs.
//!
//! A build is ready in the region when a ready `sc-gamedata` replica lists it as complete (`ListBuilds`): the replicas
//! share Valkey, so one answer is the region's.
//!
//! The namespace also holds the region's entity database and `sc-entitygraph`, see entitydb.rs. A shard opens once
//! its build and the entity database are both ready.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::{json, Value};

use crate::crd::{
    BuildState, CacheStatus, EntityDbState, EntityDbStatus, GamedataSource, ShardRegionSpec, ShardRegionStatus, ValkeySpec,
};

/// On every object of a region, and what the operator finds its pods by.
pub const REGION_LABEL: &str = "scbackend.io/shardregion";
pub const NAME_LABEL: &str = "app.kubernetes.io/name";
const PART_OF_LABEL: &str = "app.kubernetes.io/part-of";
/// The Deployment, its Service and its pods' app name. Also the former StatefulSet's, which the operator deletes.
pub const CACHE_NAME: &str = "sc-gamedata";
/// The Valkey StatefulSet, its headless Service, ConfigMap and NetworkPolicy, and its pods' app name.
pub const VALKEY_NAME: &str = "gamedata-valkey";
/// The Job that forms the cluster, named after the layout it forms, and its pods' app name.
pub const VALKEY_INIT_NAME: &str = "gamedata-valkey-init";
const NAMESPACE_PREFIX: &str = "scbackend-region-";
const MAX_DNS_LABEL: usize = 63;
const GAMEDATA_PORT: u16 = 50051;
const VALKEY_PORT: u16 = 6379;
const VALKEY_BUS_PORT: u16 = 16379;
const WORK_DIR: &str = "/work";
const CONFIG_DIR: &str = "/etc/valkey";
/// The builds a `sc-gamedata` pod is given.
pub const BUILDS_ENV: &str = "SC_GAMEDATA_BUILDS";
/// Reads Valkey in 1 MiB ranges and writes it in 1 MiB pieces, so a replica needs little whatever the files.
const CPU_MILLIS: u32 = 20;
const MEMORY_MIB: u32 = 64;
const MEMORY_LIMIT_MIB: u32 = 256;
const RUN_AS: u32 = 65532;
/// The `valkey` user of the official image.
const VALKEY_RUN_AS: u32 = 999;
/// Valkey's share of a node's memory; the rest is for the fork that rewrites the append-only file.
const MAXMEMORY_PERCENT: u64 = 75;
/// Valkey cluster mode needs at least three primaries to form.
const MIN_PRIMARIES: u32 = 3;
const VALKEY_CONF: &str = include_str!("../valkey/valkey.conf");
const VALKEY_START: &str = include_str!("../valkey/start.sh");
const VALKEY_INIT: &str = include_str!("../valkey/init.sh");

pub fn region_namespace(region: &str) -> String {
    format!("{NAMESPACE_PREFIX}{region}")
}

/// How a DGS reaches its region's `sc-gamedata`. A full service name, as DGSs run in their shard's namespace.
pub fn cache_address(region: &str) -> String {
    format!("http://{CACHE_NAME}.{}.svc:{GAMEDATA_PORT}", region_namespace(region))
}

/// A ShardRegion's name goes into its namespace's, so it must make a DNS label there.
pub fn validate_name(region: &str) -> Result<(), String> {
    let inner = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    let room = MAX_DNS_LABEL - NAMESPACE_PREFIX.len();
    let valid = !region.is_empty()
        && region.len() <= room
        && region.chars().all(inner)
        && !region.starts_with('-')
        && !region.ends_with('-');
    match valid {
        true => Ok(()),
        false => Err(format!("'{region}' is not a valid region name: lowercase letters, digits and '-', at most {room}")),
    }
}

/// A build is the tag of its image, and travels in a comma separated list.
fn valid_build(build: &str) -> bool {
    let first = build.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    first && build.len() <= 128 && build.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The builds the region holds: those listed, in their order, then those shards in the region still run on, which a
/// patch removed from the list. Returns them all and the kept ones.
pub fn wanted_builds(listed: &[String], in_use: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut all: Vec<String> = Vec::new();
    for build in listed {
        if !valid_build(build) {
            return Err(format!("'{build}' is not a valid game build"));
        }
        if !all.contains(build) {
            all.push(build.clone());
        }
    }
    let mut retained: Vec<String> = in_use.iter().filter(|build| valid_build(build) && !all.contains(build)).cloned().collect();
    retained.sort();
    retained.dedup();
    all.extend(retained.iter().cloned());
    if all.is_empty() {
        return Err("no game builds".to_string());
    }
    Ok((all, retained))
}

/// The operator's settings for what a region leaves empty, and the images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GamedataSettings {
    pub image: String,
    pub registry: String,
    pub repository: String,
    pub plain_http: bool,
    pub registry_ip: Option<String>,
    pub valkey_image: String,
}

/// The region's source with the operator's settings filling the gaps.
pub fn source(spec: &GamedataSource, defaults: &GamedataSettings) -> GamedataSettings {
    let set = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    GamedataSettings {
        image: defaults.image.clone(),
        registry: set(&spec.registry).unwrap_or_else(|| defaults.registry.clone()),
        repository: set(&spec.repository).unwrap_or_else(|| defaults.repository.clone()),
        plain_http: spec.plain_http.unwrap_or(defaults.plain_http),
        registry_ip: set(&spec.registry_ip).or_else(|| defaults.registry_ip.clone()),
        valkey_image: set(&spec.cache.image).unwrap_or_else(|| defaults.valkey_image.clone()),
    }
}

/// `sc-gamedata` replicas: `gamedata.serviceReplicas`, else the deprecated `cache.replicas`, else 2.
pub fn service_replicas(spec: &ShardRegionSpec) -> u32 {
    spec.gamedata.service_replicas.or(spec.cache.as_ref().map(|cache| cache.replicas)).unwrap_or(2)
}

/// Valkey nodes, primaries and replicas.
pub fn valkey_nodes(spec: &ValkeySpec) -> u32 {
    spec.primaries * (1 + spec.replicas_per_primary)
}

/// What a Kubernetes quantity such as `512Mi` or `1G` is in bytes.
pub fn quantity_bytes(quantity: &str) -> Option<u64> {
    let quantity = quantity.trim();
    let split = quantity.find(|c: char| !c.is_ascii_digit()).unwrap_or(quantity.len());
    let (number, unit) = quantity.split_at(split);
    let number: u64 = number.parse().ok()?;
    let factor: u64 = match unit {
        "" => 1,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        _ => return None,
    };
    number.checked_mul(factor)
}

/// The Valkey cluster a region asks for, or why it cannot have it.
pub fn validate_cache(spec: &ValkeySpec) -> Result<(), String> {
    if spec.primaries < MIN_PRIMARIES {
        return Err(format!("gamedata.cache.primaries is {}, a Valkey cluster needs at least {MIN_PRIMARIES}", spec.primaries));
    }
    if quantity_bytes(&spec.memory).is_none_or(|bytes| bytes < 64 << 20) {
        return Err(format!("gamedata.cache.memory '{}' is not a quantity of at least 64Mi", spec.memory));
    }
    Ok(())
}

fn labels(region: &str, name: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([(REGION_LABEL, region.to_string()), (NAME_LABEL, name.to_string()), (PART_OF_LABEL, "scbackend".to_string())])
}

/// Selects the pods of `name`. Fixed for the life of what selects them.
fn selector(region: &str, name: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([(REGION_LABEL, region.to_string()), (NAME_LABEL, name.to_string())])
}

fn metadata(region: &str, name: &str, owner: &OwnerReference) -> Value {
    json!({ "name": name, "namespace": region_namespace(region), "labels": labels(region, name), "ownerReferences": [owner] })
}

/// Owned by the ShardRegion, so deleting the region deletes the namespace and all in it.
pub fn namespace(region: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": region_namespace(region),
            "labels": { REGION_LABEL: region, PART_OF_LABEL: "scbackend" },
            "ownerReferences": [owner],
        },
    })
}

pub fn service(region: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(region, CACHE_NAME, owner),
        "spec": {
            "type": "ClusterIP",
            "selector": selector(region, CACHE_NAME),
            "ports": [{ "name": "grpc", "port": GAMEDATA_PORT, "targetPort": "grpc", "protocol": "TCP" }],
        },
    })
}

/// A Valkey node's host name: its pod's name in the headless Service.
fn valkey_host(region: &str, index: u32) -> String {
    format!("{VALKEY_NAME}-{index}.{VALKEY_NAME}.{}.svc", region_namespace(region))
}

/// Every node, for `sc-gamedata`, which needs any one of them to find the cluster.
pub fn valkey_addresses(region: &str, spec: &ValkeySpec) -> String {
    (0..valkey_nodes(spec)).map(|index| format!("redis://{}:{VALKEY_PORT}", valkey_host(region, index))).collect::<Vec<_>>().join(",")
}

/// `builds` is `wanted_builds`. A change of it rolls the replicas, which keep nothing: the data is in Valkey.
pub fn deployment(region: &str, spec: &ShardRegionSpec, source: &GamedataSettings, builds: &[String], owner: &OwnerReference) -> Value {
    let env = json!([
        { "name": "NO_COLOR", "value": "1" },
        { "name": BUILDS_ENV, "value": builds.join(",") },
        { "name": "SC_GAMEDATA_WORK_DIR", "value": WORK_DIR },
        { "name": "SC_GAMEDATA_VALKEY", "value": valkey_addresses(region, &spec.gamedata.cache) },
        { "name": "SC_GAMEDATA_OWNER", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } },
        { "name": "SC_GAMEDATA_REGISTRY", "value": source.registry },
        { "name": "SC_GAMEDATA_REPOSITORY", "value": source.repository },
        { "name": "SC_GAMEDATA_PLAIN_HTTP", "value": source.plain_http.to_string() },
    ]);
    let registry_host = source.registry.rsplit_once(':').map_or(source.registry.as_str(), |(host, _)| host);
    let host_aliases = source.registry_ip.as_ref().map(|ip| json!([{ "ip": ip, "hostnames": [registry_host] }]));
    let mut pod = json!({
        "automountServiceAccountToken": false,
        "securityContext": {
            "runAsNonRoot": true,
            "runAsUser": RUN_AS,
            "runAsGroup": RUN_AS,
            "fsGroup": RUN_AS,
            "seccompProfile": { "type": "RuntimeDefault" },
        },
        "containers": [{
            "name": "gamedata",
            "image": source.image,
            // Tags such as `dev` are reused, as for the shard pods.
            "imagePullPolicy": "Always",
            "env": env,
            "ports": [{ "name": "grpc", "containerPort": GAMEDATA_PORT, "protocol": "TCP" }],
            // SERVING only once Valkey answers and every build is complete in it.
            "readinessProbe": { "grpc": { "port": GAMEDATA_PORT }, "periodSeconds": 5 },
            // Not the gRPC health check: a load takes as long as it takes, and a replica that restarts for it starts over.
            "livenessProbe": { "tcpSocket": { "port": "grpc" }, "initialDelaySeconds": 10, "periodSeconds": 30 },
            "volumeMounts": [{ "name": "work", "mountPath": WORK_DIR }],
            "securityContext": {
                "allowPrivilegeEscalation": false,
                "readOnlyRootFilesystem": true,
                "capabilities": { "drop": ["ALL"] },
            },
            "resources": {
                "requests": { "cpu": format!("{CPU_MILLIS}m"), "memory": format!("{MEMORY_MIB}Mi") },
                "limits": { "memory": format!("{MEMORY_LIMIT_MIB}Mi") },
            },
        }],
        // On the node's disk, not in memory: a build's image and files while they are loaded.
        "volumes": [{ "name": "work", "emptyDir": { "sizeLimit": spec.gamedata.work_size } }],
    });
    if let Some(aliases) = host_aliases {
        pod["hostAliases"] = aliases;
    }
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": metadata(region, CACHE_NAME, owner),
        "spec": {
            "replicas": service_replicas(spec),
            "selector": { "matchLabels": selector(region, CACHE_NAME) },
            "template": { "metadata": { "labels": labels(region, CACHE_NAME) }, "spec": pod },
        },
    })
}

/// `valkey.conf` with the region's memory, and the scripts.
pub fn valkey_config(region: &str, spec: &ValkeySpec, owner: &OwnerReference) -> Value {
    let memory = quantity_bytes(&spec.memory).unwrap_or(512 << 20);
    let conf = format!("{VALKEY_CONF}maxmemory {}\n", memory / 100 * MAXMEMORY_PERCENT);
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": metadata(region, VALKEY_NAME, owner),
        "data": { "valkey.conf": conf, "start.sh": VALKEY_START, "init.sh": VALKEY_INIT },
    })
}

/// Headless, and with the addresses of pods not ready yet: a node finds the others by name before the cluster is up.
pub fn valkey_service(region: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(region, VALKEY_NAME, owner),
        "spec": {
            "clusterIP": "None",
            "publishNotReadyAddresses": true,
            "selector": selector(region, VALKEY_NAME),
            "ports": [
                { "name": "valkey", "port": VALKEY_PORT, "targetPort": VALKEY_PORT, "protocol": "TCP" },
                { "name": "bus", "port": VALKEY_BUS_PORT, "targetPort": VALKEY_BUS_PORT, "protocol": "TCP" },
            ],
        },
    })
}

/// Only `sc-gamedata`, which owns the data, the nodes themselves (replication and the cluster bus) and the Job that
/// forms the cluster reach Valkey. The kubelet's probes are the node's own traffic, which no policy applies to.
pub fn valkey_policy(region: &str, owner: &OwnerReference) -> Value {
    let from = |name: &str| json!({ "podSelector": { "matchLabels": selector(region, name) } });
    json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": metadata(region, VALKEY_NAME, owner),
        "spec": {
            "podSelector": { "matchLabels": selector(region, VALKEY_NAME) },
            "policyTypes": ["Ingress"],
            "ingress": [{
                "from": [from(CACHE_NAME), from(VALKEY_NAME), from(VALKEY_INIT_NAME)],
                "ports": [{ "protocol": "TCP", "port": VALKEY_PORT }, { "protocol": "TCP", "port": VALKEY_BUS_PORT }],
            }],
        },
    })
}

/// FNV-1a, stable across builds, of the ConfigMap, which rolls the nodes when it changes.
pub fn checksum(value: &Value) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn valkey_security() -> Value {
    json!({ "allowPrivilegeEscalation": false, "readOnlyRootFilesystem": true, "capabilities": { "drop": ["ALL"] } })
}

fn valkey_pod_security() -> Value {
    json!({
        "runAsNonRoot": true,
        "runAsUser": VALKEY_RUN_AS,
        "runAsGroup": VALKEY_RUN_AS,
        "fsGroup": VALKEY_RUN_AS,
        "seccompProfile": { "type": "RuntimeDefault" },
    })
}

/// The nodes. `nodes` is what to run, `valkey_nodes` unless a lowering is refused. Each persists to its own volume and
/// announces its pod's name, which `start.sh` also uses to find the others again after their IPs changed.
pub fn valkey_statefulset(region: &str, spec: &ValkeySpec, nodes: u32, image: &str, config: &str, owner: &OwnerReference) -> Value {
    let mut claim = json!({
        "metadata": { "name": "data", "labels": labels(region, VALKEY_NAME) },
        "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": spec.size } } },
    });
    if let Some(class) = spec.storage_class.as_ref().filter(|class| !class.is_empty()) {
        claim["spec"]["storageClassName"] = json!(class);
    }
    let field = |path: &str| json!({ "fieldRef": { "fieldPath": path } });
    let ok = format!("valkey-cli -p {VALKEY_PORT} cluster info | grep -q cluster_state:ok");
    let pod = json!({
        "automountServiceAccountToken": false,
        "securityContext": valkey_pod_security(),
        // Primaries and their replicas on different machines where the scheduler can.
        "affinity": { "podAntiAffinity": { "preferredDuringSchedulingIgnoredDuringExecution": [{
            "weight": 100,
            "podAffinityTerm": { "topologyKey": "kubernetes.io/hostname", "labelSelector": { "matchLabels": selector(region, VALKEY_NAME) } },
        }] } },
        "containers": [{
            "name": "valkey",
            "image": image,
            "imagePullPolicy": "IfNotPresent",
            "command": ["sh", format!("{CONFIG_DIR}/start.sh")],
            "env": [
                { "name": "POD_IP", "valueFrom": field("status.podIP") },
                { "name": "POD_NAME", "valueFrom": field("metadata.name") },
                { "name": "POD_HOSTNAME", "value": format!("$(POD_NAME).{VALKEY_NAME}.{}.svc", region_namespace(region)) },
            ],
            "ports": [
                { "name": "valkey", "containerPort": VALKEY_PORT, "protocol": "TCP" },
                { "name": "bus", "containerPort": VALKEY_BUS_PORT, "protocol": "TCP" },
            ],
            // Ready once it sees the cluster whole.
            "readinessProbe": { "exec": { "command": ["sh", "-c", ok] }, "periodSeconds": 5 },
            // The port answers while the node loads its file, which can take a while.
            "livenessProbe": { "tcpSocket": { "port": "valkey" }, "initialDelaySeconds": 10, "periodSeconds": 15 },
            "volumeMounts": [
                { "name": "data", "mountPath": "/data" },
                { "name": "config", "mountPath": CONFIG_DIR, "readOnly": true },
            ],
            "securityContext": valkey_security(),
            "resources": {
                "requests": { "cpu": "50m", "memory": spec.memory },
                "limits": { "memory": spec.memory },
            },
        }],
        "volumes": [{ "name": "config", "configMap": { "name": VALKEY_NAME } }],
    });
    json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": metadata(region, VALKEY_NAME, owner),
        "spec": {
            "serviceName": VALKEY_NAME,
            "replicas": nodes,
            // The nodes start together; the Job forms the cluster once they all answer.
            "podManagementPolicy": "Parallel",
            "updateStrategy": { "type": "RollingUpdate" },
            "selector": { "matchLabels": selector(region, VALKEY_NAME) },
            // Deleting the region deletes the data; scaling down is refused, so a volume never goes alone.
            "persistentVolumeClaimRetentionPolicy": { "whenDeleted": "Delete", "whenScaled": "Retain" },
            "template": {
                "metadata": { "labels": labels(region, VALKEY_NAME), "annotations": { "checksum/config": config } },
                "spec": pod,
            },
            "volumeClaimTemplates": [claim],
        },
    })
}

/// One Job per layout, named after it: a new layout (more primaries) is a new Job, which adds the new nodes.
pub fn valkey_init_name(spec: &ValkeySpec, nodes: u32) -> String {
    format!("{VALKEY_INIT_NAME}-{nodes}-{}", spec.replicas_per_primary)
}

/// Forms the cluster from `nodes` nodes, or brings a formed one to them; `init.sh` changes nothing on a cluster that is
/// already so. It retries until every node answers.
pub fn valkey_init_job(region: &str, spec: &ValkeySpec, nodes: u32, image: &str, owner: &OwnerReference) -> Value {
    let hosts: Vec<String> = (0..nodes).map(|index| valkey_host(region, index)).collect();
    let mut metadata = metadata(region, &valkey_init_name(spec, nodes), owner);
    metadata["labels"][NAME_LABEL] = json!(VALKEY_INIT_NAME);
    json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": metadata,
        "spec": {
            "backoffLimit": 20,
            "template": {
                "metadata": { "labels": labels(region, VALKEY_INIT_NAME) },
                "spec": {
                    "restartPolicy": "OnFailure",
                    "automountServiceAccountToken": false,
                    "securityContext": valkey_pod_security(),
                    "containers": [{
                        "name": "init",
                        "image": image,
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["sh", format!("{CONFIG_DIR}/init.sh")],
                        "env": [
                            { "name": "NODES", "value": hosts.join(" ") },
                            { "name": "REPLICAS", "value": spec.replicas_per_primary.to_string() },
                        ],
                        "volumeMounts": [{ "name": "config", "mountPath": CONFIG_DIR, "readOnly": true }],
                        "securityContext": valkey_security(),
                        "resources": { "requests": { "cpu": "10m", "memory": "16Mi" }, "limits": { "memory": "64Mi" } },
                    }],
                    "volumes": [{ "name": "config", "configMap": { "name": VALKEY_NAME } }],
                },
            },
        },
    })
}

/// The nodes to run: the spec's, unless that is fewer than run now, which would drop the slots of the nodes removed.
/// Returns the count and why it differs.
pub fn valkey_scale(spec: &ValkeySpec, running: Option<u32>) -> (u32, Option<String>) {
    let wanted = valkey_nodes(spec);
    match running {
        Some(running) if running > wanted => (
            running,
            Some(format!("gamedata.cache asks for {wanted} Valkey nodes, {running} run: lowering it is not supported, they stay")),
        ),
        _ => (wanted, None),
    }
}

/// An `sc-gamedata` pod as listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CachePod {
    pub ready: bool,
    pub terminating: bool,
}

pub fn ready_replicas(pods: &[CachePod]) -> u32 {
    pods.iter().filter(|pod| pod.ready && !pod.terminating).count() as u32
}

/// A build is Ready once a ready `sc-gamedata` replica lists it as complete in Valkey. `listed` is its `ListBuilds`
/// answer, None when none answered.
pub fn build_states(builds: &[String], listed: Option<&[(String, bool)]>) -> BTreeMap<String, BuildState> {
    let complete = |build: &String| listed.is_some_and(|listed| listed.iter().any(|(name, ready)| name == build && *ready));
    builds.iter().map(|build| (build.clone(), if complete(build) { BuildState::Ready } else { BuildState::Pending })).collect()
}

/// Whether a shard of `build` in `region` may make its pods: its build is ready in the region's game data, and the
/// region's entity database answers. `regions` has every ShardRegion there is, by name.
pub fn gate(region: &str, build: &str, regions: &BTreeMap<String, ShardRegionStatus>) -> Result<(), String> {
    let Some(status) = regions.get(region) else {
        return Err(format!("waiting for ShardRegion '{region}', which does not exist"));
    };
    if build.is_empty() {
        return Err("the shard has no game build".to_string());
    }
    match status.builds.get(build) {
        Some(BuildState::Ready) if status.entity_db.state == EntityDbState::Ready => Ok(()),
        Some(BuildState::Ready) => {
            let mut message = format!("waiting for the entity database of region '{region}'");
            if !status.entity_db.message.is_empty() {
                message = format!("{message}: {}", status.entity_db.message);
            }
            Err(message)
        }
        Some(BuildState::Pending) => {
            let mut message = format!(
                "waiting for {CACHE_NAME} of region '{region}' to hold game build {build} ({}/{} replicas ready)",
                status.ready_replicas, status.replicas
            );
            if !status.message.is_empty() {
                message = format!("{message}: {}", status.message);
            }
            Err(message)
        }
        None if status.message.is_empty() && status.namespace.is_empty() => {
            Err(format!("waiting for ShardRegion '{region}' to be reconciled"))
        }
        None => Err(format!("game build {build} is not in the gameBuilds of ShardRegion '{region}'")),
    }
}

/// What was seen of the region's game data in this pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// `sc-gamedata` pods.
    pub pods: Vec<CachePod>,
    /// `sc-gamedata`'s `ListBuilds`: each build and whether it is complete. None when no replica answered.
    pub listed: Option<Vec<(String, bool)>>,
    pub cache: CacheStatus,
}

/// The region's status for this pass. `error` is what stopped it, if anything. `entity_db` is entitydb::status.
pub fn status(
    region: &str,
    builds: &[String],
    retained: &[String],
    replicas: u32,
    observed: &Observed,
    error: Option<&str>,
    entity_db: EntityDbStatus,
) -> ShardRegionStatus {
    let states = build_states(builds, observed.listed.as_deref());
    let ready = ready_replicas(&observed.pods);
    let cache = &observed.cache;
    let message = match error {
        Some(error) => error.to_string(),
        None if states.values().all(|state| *state == BuildState::Ready) => String::new(),
        None if !cache.formed || cache.ready_nodes < cache.nodes => {
            format!("waiting for the Valkey cluster ({}/{} nodes ready)", cache.ready_nodes, cache.nodes)
        }
        None if observed.listed.is_none() => format!("waiting for {CACHE_NAME} to answer ({ready}/{replicas} replicas ready)"),
        None => {
            let pending: Vec<&str> = states.iter().filter(|(_, state)| **state == BuildState::Pending).map(|(build, _)| build.as_str()).collect();
            format!("loading {} into Valkey ({ready}/{replicas} replicas ready)", pending.join(", "))
        }
    };
    ShardRegionStatus {
        builds: states,
        retained_builds: retained.to_vec(),
        namespace: region_namespace(region),
        cache_service: cache_address(region),
        replicas,
        ready_replicas: ready,
        cache: cache.clone(),
        message,
        entity_db,
    }
}

/// A ChartRegion is an entry of `shardRegions` in the chart values, as the install command reads it.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ChartRegion {
    pub name: String,
    pub spec: ShardRegionSpec,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::CacheSpec;

    fn builds(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn settings() -> GamedataSettings {
        GamedataSettings {
            image: "localhost:5001/scbackend/sc-gamedata:dev".into(),
            registry: "localhost:5001".into(),
            repository: "scbackend/gamedata".into(),
            plain_http: true,
            registry_ip: None,
            valkey_image: "docker.io/valkey/valkey:9.0".into(),
        }
    }

    fn spec(list: &[&str]) -> ShardRegionSpec {
        ShardRegionSpec { game_builds: builds(list), gamedata: GamedataSource::default(), cache: None, entity_db: Default::default() }
    }

    fn owner() -> OwnerReference {
        OwnerReference { api_version: "scbackend.io/v1alpha1".into(), kind: "ShardRegion".into(), name: "usa".into(), uid: "uid".into(), ..OwnerReference::default() }
    }

    fn env(manifest: &Value) -> BTreeMap<String, Value> {
        let env = manifest["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap();
        env.iter().map(|var| (var["name"].as_str().unwrap().to_string(), var.get("value").cloned().unwrap_or(var["valueFrom"].clone()))).collect()
    }

    fn matches(selector: &Value, labels: &Value) {
        for (key, value) in selector.as_object().unwrap() {
            assert_eq!(&labels[key], value, "{key}");
        }
    }

    #[test]
    fn a_region_has_its_namespace_and_the_dgs_address_of_the_contract() {
        assert_eq!(region_namespace("usa"), "scbackend-region-usa");
        assert_eq!(cache_address("usa"), "http://sc-gamedata.scbackend-region-usa.svc:50051");
    }

    #[test]
    fn region_names_make_a_namespace_name() {
        assert_eq!(validate_name("usa"), Ok(()));
        assert_eq!(validate_name(&"a".repeat(46)), Ok(()));
        for name in ["", "USA", "eu.west", "-usa", "usa-", "eu_west", &"a".repeat(47)] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn the_namespace_is_the_regions() {
        let namespace = namespace("usa", &owner());
        assert_eq!(namespace["metadata"]["name"], "scbackend-region-usa");
        assert_eq!(namespace["metadata"]["labels"][REGION_LABEL], "usa");
        assert_eq!(namespace["metadata"]["ownerReferences"][0]["uid"], "uid");
        assert!(namespace["metadata"]["labels"].get("scbackend.io/shardcluster").is_none(), "a shard's are found by this");
        serde_json::from_value::<k8s_openapi::api::core::v1::Namespace>(namespace).unwrap();
    }

    #[test]
    fn sc_gamedata_is_stateless_in_front_of_the_regions_valkey() {
        let deployment = deployment("usa", &spec(&["12913470"]), &settings(), &builds(&["12913470", "12000000"]), &owner());
        let env = env(&deployment);
        assert_eq!(env[BUILDS_ENV], "12913470,12000000");
        assert_eq!(env["SC_GAMEDATA_WORK_DIR"], "/work");
        assert_eq!(env["SC_GAMEDATA_REGISTRY"], "localhost:5001");
        assert_eq!(env["SC_GAMEDATA_REPOSITORY"], "scbackend/gamedata");
        assert_eq!(env["SC_GAMEDATA_PLAIN_HTTP"], "true");
        assert_eq!(env["SC_GAMEDATA_OWNER"], json!({ "fieldRef": { "fieldPath": "metadata.name" } }), "the lock holder");
        let nodes: Vec<&str> = env["SC_GAMEDATA_VALKEY"].as_str().unwrap().split(',').collect();
        assert_eq!(nodes.len(), 6);
        assert_eq!(nodes[0], "redis://gamedata-valkey-0.gamedata-valkey.scbackend-region-usa.svc:6379");
        let pod = &deployment["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        assert_eq!(container["image"], "localhost:5001/scbackend/sc-gamedata:dev");
        assert_eq!(container["volumeMounts"][0]["mountPath"], "/work");
        assert_eq!(pod["volumes"][0], json!({ "name": "work", "emptyDir": { "sizeLimit": "4Gi" } }), "disk, not memory");
        assert_eq!(container["readinessProbe"]["grpc"]["port"], 50051);
        assert!(container["readinessProbe"]["grpc"].get("service").is_none(), "the overall status");
        assert_eq!(container["resources"]["limits"]["memory"], "256Mi");
        assert_eq!(deployment["spec"]["replicas"], 2);
        assert_eq!(deployment["metadata"]["namespace"], "scbackend-region-usa");
        assert!(deployment["spec"].get("volumeClaimTemplates").is_none());
        let parsed: k8s_openapi::api::apps::v1::Deployment = serde_json::from_value(deployment).unwrap();
        assert_eq!(parsed.metadata.owner_references.unwrap()[0].uid, "uid");
    }

    #[test]
    fn service_replicas_come_from_the_spec_or_the_deprecated_cache() {
        let mut region = spec(&["1"]);
        assert_eq!(service_replicas(&region), 2);
        region.cache = Some(CacheSpec { replicas: 3, storage_class: None, size: "10Gi".into() });
        assert_eq!(service_replicas(&region), 3, "a region from before the Valkey cache keeps its count");
        region.gamedata.service_replicas = Some(4);
        assert_eq!(service_replicas(&region), 4);
        assert_eq!(deployment("usa", &region, &settings(), &builds(&["1"]), &owner())["spec"]["replicas"], 4);
    }

    #[test]
    fn gamedata_pods_run_like_the_others() {
        let deployment = deployment("usa", &spec(&["1"]), &settings(), &builds(&["1"]), &owner());
        let pod = &deployment["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
        let container = &pod["containers"][0]["securityContext"];
        assert_eq!(container["readOnlyRootFilesystem"], true);
        assert_eq!(container["allowPrivilegeEscalation"], false);
        assert!(pod.get("hostAliases").is_none());
    }

    #[test]
    fn a_registry_only_the_nodes_resolve_gets_a_host_alias() {
        let source = GamedataSettings { registry: "registry.alexandria.local:5000".into(), registry_ip: Some("10.0.0.5".into()), ..settings() };
        let deployment = deployment("usa", &spec(&["1"]), &source, &builds(&["1"]), &owner());
        assert_eq!(deployment["spec"]["template"]["spec"]["hostAliases"], json!([{ "ip": "10.0.0.5", "hostnames": ["registry.alexandria.local"] }]));
    }

    #[test]
    fn the_service_selects_the_gamedata_pods() {
        let deployment = deployment("usa", &spec(&["1"]), &settings(), &builds(&["1"]), &owner());
        let service = service("usa", &owner());
        assert_eq!(service["metadata"]["name"], "sc-gamedata");
        assert_eq!(service["spec"]["selector"], deployment["spec"]["selector"]["matchLabels"]);
        matches(&service["spec"]["selector"], &deployment["spec"]["template"]["metadata"]["labels"]);
        assert_eq!(service["spec"]["ports"][0]["port"], 50051);
        assert_eq!(service["spec"]["ports"][0]["targetPort"], deployment["spec"]["template"]["spec"]["containers"][0]["ports"][0]["name"]);
        serde_json::from_value::<k8s_openapi::api::core::v1::Service>(service).unwrap();
    }

    #[test]
    fn region_settings_win_over_the_operators() {
        let spec = GamedataSource {
            registry: Some("r:1".into()),
            plain_http: Some(false),
            registry_ip: Some(String::new()),
            cache: ValkeySpec { image: Some("valkey:8.1".into()), ..ValkeySpec::default() },
            ..GamedataSource::default()
        };
        let source = source(&spec, &settings());
        assert_eq!((source.registry.as_str(), source.repository.as_str(), source.plain_http), ("r:1", "scbackend/gamedata", false));
        assert_eq!(source.registry_ip, None);
        assert_eq!(source.valkey_image, "valkey:8.1");
    }

    #[test]
    fn quantities_read_as_bytes() {
        assert_eq!(quantity_bytes("512Mi"), Some(512 << 20));
        assert_eq!(quantity_bytes("2Gi"), Some(2 << 30));
        assert_eq!(quantity_bytes("1G"), Some(1_000_000_000));
        assert_eq!(quantity_bytes("1000"), Some(1000));
        assert_eq!(quantity_bytes("1.5Gi"), None);
        assert_eq!(quantity_bytes("Mi"), None);
    }

    #[test]
    fn the_valkey_layout_is_checked() {
        assert_eq!(validate_cache(&ValkeySpec::default()), Ok(()));
        assert!(validate_cache(&ValkeySpec { primaries: 2, ..ValkeySpec::default() }).is_err());
        assert!(validate_cache(&ValkeySpec { memory: "lots".into(), ..ValkeySpec::default() }).is_err());
        assert!(validate_cache(&ValkeySpec { memory: "32Mi".into(), ..ValkeySpec::default() }).is_err());
        assert_eq!(valkey_nodes(&ValkeySpec::default()), 6);
        assert_eq!(valkey_nodes(&ValkeySpec { primaries: 5, replicas_per_primary: 2, ..ValkeySpec::default() }), 15);
    }

    #[test]
    fn valkey_never_evicts_and_keeps_room_for_its_rewrite() {
        let config = valkey_config("usa", &ValkeySpec::default(), &owner());
        let conf = config["data"]["valkey.conf"].as_str().unwrap();
        for line in ["cluster-enabled yes", "maxmemory-policy noeviction", "appendonly yes", "cluster-preferred-endpoint-type hostname", "dir /data"] {
            assert!(conf.lines().any(|l| l == line), "{line}");
        }
        assert!(conf.ends_with(&format!("maxmemory {}\n", (512u64 << 20) / 100 * 75)), "{conf}");
        assert!(config["data"]["start.sh"].as_str().unwrap().contains("--cluster-announce-hostname"));
        assert!(config["data"]["init.sh"].as_str().unwrap().contains("--cluster create"));
        serde_json::from_value::<k8s_openapi::api::core::v1::ConfigMap>(config).unwrap();
    }

    #[test]
    fn each_valkey_node_persists_to_its_own_volume() {
        let cache = ValkeySpec { storage_class: Some("fast".into()), size: "8Gi".into(), memory: "1Gi".into(), ..ValkeySpec::default() };
        let set = valkey_statefulset("usa", &cache, 6, "docker.io/valkey/valkey:9.0", "abc", &owner());
        assert_eq!(set["spec"]["replicas"], 6);
        assert_eq!(set["spec"]["serviceName"], "gamedata-valkey");
        let claim = &set["spec"]["volumeClaimTemplates"][0]["spec"];
        assert_eq!(claim["accessModes"], json!(["ReadWriteOnce"]));
        assert_eq!(claim["storageClassName"], "fast");
        assert_eq!(claim["resources"]["requests"]["storage"], "8Gi");
        assert_eq!(set["spec"]["persistentVolumeClaimRetentionPolicy"]["whenDeleted"], "Delete");
        let pod = &set["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        assert_eq!(container["image"], "docker.io/valkey/valkey:9.0");
        assert_eq!(container["command"], json!(["sh", "/etc/valkey/start.sh"]));
        assert_eq!(container["resources"]["limits"]["memory"], "1Gi");
        assert_eq!(container["resources"]["requests"]["memory"], "1Gi");
        let env = env(&set);
        assert_eq!(env["POD_HOSTNAME"], "$(POD_NAME).gamedata-valkey.scbackend-region-usa.svc", "announced, and what clients follow");
        assert_eq!(env["POD_IP"], json!({ "fieldRef": { "fieldPath": "status.podIP" } }));
        assert_eq!(pod["securityContext"]["runAsUser"], 999);
        assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(set["spec"]["template"]["metadata"]["annotations"]["checksum/config"], "abc");
        matches(&set["spec"]["selector"]["matchLabels"], &set["spec"]["template"]["metadata"]["labels"]);
        let service = valkey_service("usa", &owner());
        assert_eq!(service["spec"]["clusterIP"], "None");
        assert_eq!(service["spec"]["publishNotReadyAddresses"], true);
        matches(&service["spec"]["selector"], &set["spec"]["template"]["metadata"]["labels"]);
        serde_json::from_value::<k8s_openapi::api::apps::v1::StatefulSet>(set).unwrap();
        serde_json::from_value::<k8s_openapi::api::core::v1::Service>(service).unwrap();
    }

    #[test]
    fn the_init_job_forms_the_layout_it_is_named_after() {
        let job = valkey_init_job("usa", &ValkeySpec::default(), 6, "docker.io/valkey/valkey:9.0", &owner());
        assert_eq!(job["metadata"]["name"], "gamedata-valkey-init-6-1");
        assert_eq!(job["metadata"]["labels"][NAME_LABEL], "gamedata-valkey-init");
        let env = env(&json!({ "spec": { "template": job["spec"]["template"] } }));
        let nodes: Vec<&str> = env["NODES"].as_str().unwrap().split(' ').collect();
        assert_eq!(nodes.len(), 6);
        assert_eq!(nodes[5], "gamedata-valkey-5.gamedata-valkey.scbackend-region-usa.svc");
        assert_eq!(env["REPLICAS"], "1");
        assert_eq!(job["spec"]["template"]["spec"]["containers"][0]["command"], json!(["sh", "/etc/valkey/init.sh"]));
        serde_json::from_value::<k8s_openapi::api::batch::v1::Job>(job).unwrap();
        let more = ValkeySpec { primaries: 4, ..ValkeySpec::default() };
        assert_eq!(valkey_init_name(&more, valkey_nodes(&more)), "gamedata-valkey-init-8-1");
    }

    #[test]
    fn only_gamedata_valkey_and_its_init_reach_valkey() {
        let policy = valkey_policy("usa", &owner());
        assert_eq!(policy["spec"]["podSelector"]["matchLabels"], json!({ REGION_LABEL: "usa", NAME_LABEL: "gamedata-valkey" }));
        assert_eq!(policy["spec"]["policyTypes"], json!(["Ingress"]));
        let from: Vec<&Value> = policy["spec"]["ingress"][0]["from"].as_array().unwrap().iter().map(|peer| &peer["podSelector"]["matchLabels"][NAME_LABEL]).collect();
        assert_eq!(from, ["sc-gamedata", "gamedata-valkey", "gamedata-valkey-init"]);
        let deployment = deployment("usa", &spec(&["1"]), &settings(), &builds(&["1"]), &owner());
        matches(&policy["spec"]["ingress"][0]["from"][0]["podSelector"]["matchLabels"], &deployment["spec"]["template"]["metadata"]["labels"]);
        let job = valkey_init_job("usa", &ValkeySpec::default(), 6, "v", &owner());
        matches(&policy["spec"]["ingress"][0]["from"][2]["podSelector"]["matchLabels"], &job["spec"]["template"]["metadata"]["labels"]);
        serde_json::from_value::<k8s_openapi::api::networking::v1::NetworkPolicy>(policy).unwrap();
    }

    #[test]
    fn lowering_the_valkey_nodes_is_refused() {
        assert_eq!(valkey_scale(&ValkeySpec::default(), None), (6, None));
        assert_eq!(valkey_scale(&ValkeySpec::default(), Some(6)), (6, None));
        let more = ValkeySpec { primaries: 4, ..ValkeySpec::default() };
        assert_eq!(valkey_scale(&more, Some(6)), (8, None));
        let (nodes, message) = valkey_scale(&ValkeySpec::default(), Some(8));
        assert_eq!(nodes, 8);
        assert!(message.unwrap().contains("not supported"));
    }

    #[test]
    fn the_config_checksum_follows_the_config() {
        let small = valkey_config("usa", &ValkeySpec::default(), &owner());
        let large = valkey_config("usa", &ValkeySpec { memory: "1Gi".into(), ..ValkeySpec::default() }, &owner());
        assert_eq!(checksum(&small), checksum(&small.clone()));
        assert_ne!(checksum(&small), checksum(&large));
    }

    #[test]
    fn builds_still_in_use_are_kept_after_the_list() {
        let (all, retained) = wanted_builds(&builds(&["2", "2"]), &builds(&["1", "2", "0", "1"])).unwrap();
        assert_eq!(all, ["2", "0", "1"]);
        assert_eq!(retained, ["0", "1"]);
        let (all, retained) = wanted_builds(&builds(&["2"]), &[]).unwrap();
        assert_eq!((all, retained), (builds(&["2"]), vec![]));
    }

    #[test]
    fn builds_are_image_tags_in_a_comma_list() {
        assert!(wanted_builds(&builds(&["1,2"]), &[]).is_err());
        assert!(wanted_builds(&builds(&[""]), &[]).is_err());
        assert!(wanted_builds(&builds(&["-1"]), &[]).is_err());
        assert!(wanted_builds(&[], &[]).is_err());
        assert!(wanted_builds(&builds(&["4.3.1-live.12913470"]), &[]).is_ok());
    }

    fn db_ready() -> EntityDbStatus {
        EntityDbStatus { state: EntityDbState::Ready, ..EntityDbStatus::default() }
    }

    fn formed() -> CacheStatus {
        CacheStatus { nodes: 6, ready_nodes: 6, formed: true }
    }

    fn observed(ready_pods: usize, listed: Option<&[(&str, bool)]>) -> Observed {
        Observed {
            pods: (0..ready_pods).map(|_| CachePod { ready: true, terminating: false }).collect(),
            listed: listed.map(|listed| listed.iter().map(|(build, ready)| (build.to_string(), *ready)).collect()),
            cache: formed(),
        }
    }

    #[test]
    fn a_build_is_ready_once_sc_gamedata_lists_it_complete() {
        let all = builds(&["2", "1"]);
        let listed = [("2".to_string(), false), ("1".to_string(), true)];
        let states = build_states(&all, Some(&listed));
        assert_eq!((states["1"], states["2"]), (BuildState::Ready, BuildState::Pending));
        let states = build_states(&all, None);
        assert_eq!((states["1"], states["2"]), (BuildState::Pending, BuildState::Pending), "nobody answered");
        // During a roll an older replica may not list a new build at all.
        let states = build_states(&all, Some(&listed[1..]));
        assert_eq!(states["2"], BuildState::Pending);
    }

    #[test]
    fn the_status_says_what_the_region_waits_for() {
        let all = builds(&["2", "1"]);
        let status = status("usa", &all, &builds(&["1"]), 2, &observed(2, Some(&[("2", false), ("1", true)])), None, db_ready());
        assert_eq!(status.message, "loading 2 into Valkey (2/2 replicas ready)");
        assert_eq!(status.retained_builds, ["1"]);
        assert_eq!(status.cache_service, "http://sc-gamedata.scbackend-region-usa.svc:50051");
        assert_eq!(status.cache, formed());
        let done = super::status("usa", &all, &[], 2, &observed(2, Some(&[("2", true), ("1", true)])), None, db_ready());
        assert_eq!(done.message, "");
        assert_eq!(done.ready_replicas, 2);
        let forming = Observed { cache: CacheStatus { nodes: 6, ready_nodes: 2, formed: false }, ..observed(0, None) };
        let waiting = super::status("usa", &all, &[], 2, &forming, None, db_ready());
        assert_eq!(waiting.message, "waiting for the Valkey cluster (2/6 nodes ready)");
        let silent = super::status("usa", &all, &[], 2, &observed(0, None), None, db_ready());
        assert_eq!(silent.message, "waiting for sc-gamedata to answer (0/2 replicas ready)");
        let failed = super::status("usa", &all, &[], 2, &observed(0, None), Some("apply statefulset: denied"), db_ready());
        assert_eq!(failed.message, "apply statefulset: denied");
    }

    #[test]
    fn a_shard_opens_only_once_its_build_is_ready_in_its_region() {
        let mut regions = BTreeMap::new();
        let error = gate("usa", "1", &regions).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
        regions.insert("usa".to_string(), ShardRegionStatus::default());
        assert!(gate("usa", "1", &regions).unwrap_err().contains("to be reconciled"));
        let ready = status("usa", &builds(&["1", "2"]), &[], 2, &observed(2, Some(&[("1", true), ("2", false)])), None, db_ready());
        regions.insert("usa".to_string(), ready);
        assert_eq!(gate("usa", "1", &regions), Ok(()));
        let error = gate("usa", "2", &regions).unwrap_err();
        assert!(error.contains("to hold game build 2 (2/2 replicas ready): loading 2 into Valkey"), "{error}");
        assert!(gate("usa", "3", &regions).unwrap_err().contains("not in the gameBuilds"));
        assert!(gate("eu", "1", &regions).is_err());
        assert!(gate("usa", "", &regions).is_err());
    }

    #[test]
    fn a_shard_also_waits_for_the_entity_database_of_its_region() {
        let listed: &[(&str, bool)] = &[("1", true)];
        let waiting = EntityDbStatus { message: "waiting for ArangoDeployment arangodb to be Ready".into(), ..EntityDbStatus::default() };
        let regions = BTreeMap::from([("usa".to_string(), status("usa", &builds(&["1"]), &[], 2, &observed(2, Some(listed)), None, waiting))]);
        let error = gate("usa", "1", &regions).unwrap_err();
        assert_eq!(error, "waiting for the entity database of region 'usa': waiting for ArangoDeployment arangodb to be Ready");
        let regions = BTreeMap::from([("usa".to_string(), status("usa", &builds(&["1"]), &[], 2, &observed(2, Some(listed)), None, db_ready()))]);
        assert_eq!(gate("usa", "1", &regions), Ok(()));
        // A build still loading says so first, whatever the database.
        let loading = BTreeMap::from([("usa".to_string(), status("usa", &builds(&["1"]), &[], 2, &observed(2, Some(&[("1", false)])), None, db_ready()))]);
        assert!(gate("usa", "1", &loading).unwrap_err().contains("to hold game build 1"));
    }
}

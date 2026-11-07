//! The entity database of a ShardRegion: an ArangoDeployment named `arangodb` and `sc-entitygraph`, its only reader
//! and writer, in the region's namespace. Kept free of the Kubernetes client like region.rs.
//!
//! kube-arangodb's deployment operator only watches the namespace it runs in, so each region runs one of its own,
//! `arango-operator`, with the rights the chart's ClusterRoles give it there. The cluster-wide kube-arangodb release
//! of the deployment only installs and upgrades the CRDs.
//!
//! The game reaches `EntityGraphService` through the one gateway, in the operator's namespace, so the region marked
//! `clientRoute` gets a GRPCRoute there whose backend is its `sc-entitygraph`, which a ReferenceGrant in the region's
//! namespace allows.

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use serde_json::{json, Value};

use crate::{
    crd::{EntityDbMode, EntityDbSpec, EntityDbState, EntityDbStatus},
    region::{region_namespace, REGION_LABEL},
};

/// The ArangoDeployment, and so its Service. The same name in every region, so a replication set up later pairs
/// `arangodb` of one region with `arangodb` of another.
pub const DATABASE_NAME: &str = "arangodb";
pub const ENTITYGRAPH_NAME: &str = "sc-entitygraph";
pub const ARANGO_OPERATOR_NAME: &str = "arango-operator";
/// Binds the `default` ServiceAccount, which the ArangoDB members run as.
pub const MEMBER_BINDING_NAME: &str = "arango-member";
pub const REFERENCE_GRANT_NAME: &str = "sc-entitygraph-gateway";
/// Held by a ShardRegion until its ArangoDeployment is gone. The members carry kube-arangodb's finalizers, which only
/// the region's own `arango-operator` removes, so deleting the namespace first would leave it Terminating for good.
pub const REGION_FINALIZER: &str = "scbackend.io/entity-db";
pub const ENTITYGRAPH_SERVICE: &str = "sc.external.services.entitygraph.v1.EntityGraphService";
const NAME_LABEL: &str = "app.kubernetes.io/name";
const PART_OF_LABEL: &str = "app.kubernetes.io/part-of";
const DATABASE_PORT: u16 = 8529;
const GRAPH_PORT: u16 = 50051;
const HEALTH_PORT: u16 = 8080;
const OPERATOR_PORT: u16 = 8528;
const RUN_AS: u32 = 65532;
/// What kube-arangodb's chart runs its operator as.
const OPERATOR_RUN_AS: u32 = 1000;
/// Each `kind` and its API, for the dynamic client.
pub const ARANGO_API: (&str, &str, &str, &str) = ("database.arangodb.com", "v1", "ArangoDeployment", "arangodeployments");
pub const GRPC_ROUTE_API: (&str, &str, &str, &str) = ("gateway.networking.k8s.io", "v1", "GRPCRoute", "grpcroutes");
pub const REFERENCE_GRANT_API: (&str, &str, &str, &str) = ("gateway.networking.k8s.io", "v1beta1", "ReferenceGrant", "referencegrants");

/// The operator's settings for every region's entity database, from the chart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityDbSettings {
    /// ArangoDB, where a region names none.
    pub arango_image: String,
    /// kube-arangodb, the version of the CRDs the cluster-wide release installs.
    pub operator_image: String,
    pub entitygraph_image: String,
    /// sc-push's internal publisher, a full service name.
    pub push: String,
    pub log_level: String,
    /// The Secret with state.json in the operator's namespace, copied into each region for the token secret.
    pub data_secret: String,
    /// ClusterRoles the chart defines for kube-arangodb: bound in the region's namespace for `arango-operator`,
    /// cluster-wide for its reads of CRDs, nodes and volumes, and in the namespace for the members.
    pub operator_role: String,
    pub operator_cluster_role: String,
    pub member_role: String,
    /// The operator's namespace, where the gateway and its routes are.
    pub namespace: String,
    pub gateway: String,
    pub listener: String,
}

/// Where `sc-entitygraph` of a region answers. A full service name: DGSs and the global services are elsewhere.
pub fn entitygraph_address(region: &str) -> String {
    format!("http://{ENTITYGRAPH_NAME}.{}.svc:{GRAPH_PORT}", region_namespace(region))
}

/// The region's ArangoDB, the Service kube-arangodb makes for the Coordinators or the single server.
pub fn database_address(region: &str) -> String {
    format!("http://{DATABASE_NAME}.{}.svc:{DATABASE_PORT}", region_namespace(region))
}

/// The gateway's route to a region, in the operator's namespace. Named after the region, so two cannot share it.
pub fn route_name(region: &str) -> String {
    format!("{ENTITYGRAPH_NAME}-{region}")
}

/// A ClusterRoleBinding is cluster-scoped, so it carries the region's namespace in its name.
pub fn cluster_binding_name(region: &str) -> String {
    format!("{}-{ARANGO_OPERATOR_NAME}", region_namespace(region))
}

fn labels(region: &str, name: &str) -> BTreeMap<&'static str, String> {
    BTreeMap::from([(REGION_LABEL, region.to_string()), (NAME_LABEL, name.to_string()), (PART_OF_LABEL, "scbackend".to_string())])
}

fn metadata(region: &str, name: &str, owner: &OwnerReference) -> Value {
    json!({ "name": name, "namespace": region_namespace(region), "labels": labels(region, name), "ownerReferences": [owner] })
}

fn claim(size: &str, storage_class: Option<&str>) -> Value {
    let mut claim = json!({ "spec": { "accessModes": ["ReadWriteOnce"], "resources": { "requests": { "storage": size } } } });
    if let Some(class) = storage_class.filter(|class| !class.is_empty()) {
        claim["spec"]["storageClassName"] = json!(class);
    }
    claim
}

/// Requests equal to limits, as the dev deployment had.
fn resources(cpu: &str, memory: &str) -> Value {
    json!({ "requests": { "cpu": cpu, "memory": memory }, "limits": { "cpu": cpu, "memory": memory } })
}

/// The spec of the dev cluster's former `environments/dev/arangodb.yaml`, as kube-arangodb's chart rendered it: no TLS,
/// as only the cluster reaches it, and the operator's JWT secret `arangodb-jwt` with an empty root password, which is
/// what `sc-db` logs in with. Production keeps members of a group on separate nodes.
pub fn arango_deployment(region: &str, spec: &EntityDbSpec, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    let class = spec.storage_class.as_deref();
    let image = spec.image.clone().filter(|image| !image.is_empty()).unwrap_or_else(|| settings.arango_image.clone());
    let mut body = json!({
        "environment": "Production",
        "image": image,
        "imagePullPolicy": "IfNotPresent",
        "tls": { "caSecretName": "None" },
        "networkAttachedVolumes": true,
    });
    match spec.mode {
        EntityDbMode::Cluster => {
            body["mode"] = json!("Cluster");
            body["agents"] = json!({ "count": spec.agents, "resources": resources("100m", "512Mi"), "volumeClaimTemplate": claim(&spec.agent_size, class) });
            body["dbservers"] = json!({
                "count": spec.db_servers,
                "resources": resources("500m", "2Gi"),
                "memoryReservation": 15,
                "volumeClaimTemplate": claim(&spec.size, class),
            });
            body["coordinators"] = json!({ "count": spec.coordinators, "resources": resources("250m", "1Gi") });
        }
        EntityDbMode::Single => {
            body["mode"] = json!("Single");
            body["single"] = json!({ "resources": resources("500m", "2Gi"), "volumeClaimTemplate": claim(&spec.size, class) });
        }
    }
    json!({
        "apiVersion": format!("{}/{}", ARANGO_API.0, ARANGO_API.1),
        "kind": ARANGO_API.2,
        "metadata": metadata(region, DATABASE_NAME, owner),
        "spec": body,
    })
}

/// Ready once kube-arangodb says so, which it does when every member answers.
pub fn arango_ready(deployment: &Value) -> bool {
    let conditions = deployment["status"]["conditions"].as_array();
    conditions.is_some_and(|all| all.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

pub fn operator_service_account(region: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": metadata(region, ARANGO_OPERATOR_NAME, owner),
        // Only the operator's pod mounts its token.
        "automountServiceAccountToken": false,
    })
}

fn role_binding(region: &str, name: &str, role: &str, account: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "RoleBinding",
        "metadata": metadata(region, name, owner),
        "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": role },
        "subjects": [{ "kind": "ServiceAccount", "name": account, "namespace": region_namespace(region) }],
    })
}

/// What kube-arangodb's operator does in the region's namespace: its ArangoDeployments, their members, pods,
/// volumes, Services and Secrets.
pub fn operator_role_binding(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    role_binding(region, ARANGO_OPERATOR_NAME, &settings.operator_role, ARANGO_OPERATOR_NAME, owner)
}

/// The members read their own pod.
pub fn member_role_binding(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    role_binding(region, MEMBER_BINDING_NAME, &settings.member_role, "default", owner)
}

/// Its reads outside the namespace: CRDs, nodes, persistent volumes. Owned by the ShardRegion, so it goes with it.
pub fn operator_cluster_role_binding(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "rbac.authorization.k8s.io/v1",
        "kind": "ClusterRoleBinding",
        "metadata": { "name": cluster_binding_name(region), "labels": labels(region, ARANGO_OPERATOR_NAME), "ownerReferences": [owner] },
        "roleRef": { "apiGroup": "rbac.authorization.k8s.io", "kind": "ClusterRole", "name": settings.operator_cluster_role },
        "subjects": [{ "kind": "ServiceAccount", "name": ARANGO_OPERATOR_NAME, "namespace": region_namespace(region) }],
    })
}

/// kube-arangodb's deployment operator, as its chart runs it, for this namespace alone. It does not install CRDs:
/// the cluster-wide release does, at the version this image expects.
pub fn operator_deployment(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    let selector = json!({ REGION_LABEL: region, NAME_LABEL: ARANGO_OPERATOR_NAME });
    let probe = |path: &str| json!({ "httpGet": { "path": path, "port": OPERATOR_PORT, "scheme": "HTTPS" }, "initialDelaySeconds": 5, "periodSeconds": 10 });
    let field = |path: &str| json!({ "fieldRef": { "fieldPath": path } });
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": metadata(region, ARANGO_OPERATOR_NAME, owner),
        "spec": {
            // --mode.single: one replica, no leader election.
            "replicas": 1,
            "strategy": { "type": "Recreate" },
            "selector": { "matchLabels": selector },
            "template": {
                "metadata": { "labels": labels(region, ARANGO_OPERATOR_NAME) },
                "spec": {
                    "serviceAccountName": ARANGO_OPERATOR_NAME,
                    "automountServiceAccountToken": true,
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": OPERATOR_RUN_AS,
                        "runAsGroup": OPERATOR_RUN_AS,
                        "fsGroup": OPERATOR_RUN_AS,
                        "seccompProfile": { "type": "RuntimeDefault" },
                    },
                    "containers": [{
                        "name": "operator",
                        "image": settings.operator_image,
                        "imagePullPolicy": "IfNotPresent",
                        "args": ["--operator.deployment", "--mode.single", "--crd.install=false", "--chaos.allowed=false"],
                        "env": [
                            { "name": "MY_POD_NAMESPACE", "valueFrom": field("metadata.namespace") },
                            { "name": "MY_POD_NAME", "valueFrom": field("metadata.name") },
                            { "name": "MY_CONTAINER_NAME", "value": "operator" },
                            { "name": "MY_POD_IP", "valueFrom": field("status.podIP") },
                        ],
                        "ports": [{ "name": "metrics", "containerPort": OPERATOR_PORT }],
                        "securityContext": {
                            "privileged": false,
                            "allowPrivilegeEscalation": false,
                            "readOnlyRootFilesystem": true,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        "resources": { "requests": { "cpu": "250m", "memory": "256Mi" }, "limits": { "cpu": "1", "memory": "512Mi" } },
                        "livenessProbe": probe("/health"),
                        "readinessProbe": probe("/ready"),
                    }],
                },
            },
        },
    })
}

/// A copy of the operator namespace's Secret, for the token secret in state.json. `data` is as the API returns it,
/// base64 per key.
pub fn data_secret(region: &str, settings: &EntityDbSettings, data: &BTreeMap<String, String>, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": metadata(region, &settings.data_secret, owner),
        "type": "Opaque",
        "data": data,
    })
}

/// FNV-1a over the Secret, which restarts `sc-entitygraph` when it changes. Stable across builds, unlike std's hasher.
pub fn checksum(data: &BTreeMap<String, String>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (key, value) in data {
        for byte in key.bytes().chain([0]).chain(value.bytes()).chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// As the sc-service chart ran it in the operator's namespace, with the region's database and full service names.
pub fn entitygraph_deployment(region: &str, spec: &EntityDbSpec, settings: &EntityDbSettings, checksum: &str, owner: &OwnerReference) -> Value {
    let selector = json!({ REGION_LABEL: region, NAME_LABEL: ENTITYGRAPH_NAME });
    let health = |period: u32| json!({ "httpGet": { "path": "/", "port": "health" }, "periodSeconds": period });
    let mut startup = health(2);
    startup["failureThreshold"] = json!(90);
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": metadata(region, ENTITYGRAPH_NAME, owner),
        "spec": {
            "replicas": spec.service_replicas,
            "selector": { "matchLabels": selector },
            "template": {
                "metadata": { "labels": labels(region, ENTITYGRAPH_NAME), "annotations": { "checksum/data": checksum } },
                "spec": {
                    "automountServiceAccountToken": false,
                    "securityContext": {
                        "runAsNonRoot": true,
                        "runAsUser": RUN_AS,
                        "runAsGroup": RUN_AS,
                        "fsGroup": RUN_AS,
                        "seccompProfile": { "type": "RuntimeDefault" },
                    },
                    "containers": [{
                        "name": "service",
                        "image": settings.entitygraph_image,
                        // Tags such as `dev` are reused, as for the shard pods.
                        "imagePullPolicy": "Always",
                        "env": [
                            { "name": "RUST_LOG", "value": settings.log_level },
                            { "name": "NO_COLOR", "value": "1" },
                            { "name": "SCBACKEND_DATA", "value": "/data" },
                            { "name": "SCBACKEND_ARANGO", "value": database_address(region) },
                            { "name": "SCBACKEND_HEALTH", "value": format!("0.0.0.0:{HEALTH_PORT}") },
                            { "name": "SCBACKEND_LISTEN", "value": format!("0.0.0.0:{GRAPH_PORT}") },
                            { "name": "SCBACKEND_PUSH", "value": settings.push },
                        ],
                        "ports": [
                            { "name": "grpc", "containerPort": GRAPH_PORT, "protocol": "TCP" },
                            { "name": "health", "containerPort": HEALTH_PORT, "protocol": "TCP" },
                        ],
                        "startupProbe": startup,
                        "readinessProbe": health(5),
                        "livenessProbe": health(10),
                        "securityContext": {
                            "allowPrivilegeEscalation": false,
                            "readOnlyRootFilesystem": true,
                            "capabilities": { "drop": ["ALL"] },
                        },
                        // ContainerQueryStream holds the whole object container tree, about 200,000 nodes, while it answers.
                        "resources": { "requests": { "cpu": "100m", "memory": "256Mi" }, "limits": { "memory": "2Gi" } },
                        "volumeMounts": [{ "name": "data", "mountPath": "/data", "readOnly": true }],
                    }],
                    "volumes": [{ "name": "data", "secret": { "secretName": settings.data_secret } }],
                },
            },
        },
    })
}

pub fn entitygraph_service(region: &str, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": metadata(region, ENTITYGRAPH_NAME, owner),
        "spec": {
            "type": "ClusterIP",
            "selector": { REGION_LABEL: region, NAME_LABEL: ENTITYGRAPH_NAME },
            "ports": [{ "name": "grpc", "port": GRAPH_PORT, "targetPort": "grpc", "protocol": "TCP" }],
        },
    })
}

/// Lets GRPCRoutes in the operator's namespace, where the gateway takes them from, send to this `sc-entitygraph`.
pub fn reference_grant(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": format!("{}/{}", REFERENCE_GRANT_API.0, REFERENCE_GRANT_API.1),
        "kind": REFERENCE_GRANT_API.2,
        "metadata": metadata(region, REFERENCE_GRANT_NAME, owner),
        "spec": {
            "from": [{ "group": GRPC_ROUTE_API.0, "kind": GRPC_ROUTE_API.2, "namespace": settings.namespace }],
            "to": [{ "group": "", "kind": "Service", "name": ENTITYGRAPH_NAME }],
        },
    })
}

/// The game's `EntityGraphService` calls, to this region's `sc-entitygraph`.
pub fn grpc_route(region: &str, settings: &EntityDbSettings, owner: &OwnerReference) -> Value {
    json!({
        "apiVersion": format!("{}/{}", GRPC_ROUTE_API.0, GRPC_ROUTE_API.1),
        "kind": GRPC_ROUTE_API.2,
        "metadata": {
            "name": route_name(region),
            "namespace": settings.namespace,
            "labels": labels(region, ENTITYGRAPH_NAME),
            "ownerReferences": [owner],
        },
        "spec": {
            "parentRefs": [{ "name": settings.gateway, "sectionName": settings.listener }],
            "rules": [{
                "matches": [{ "method": { "type": "Exact", "service": ENTITYGRAPH_SERVICE } }],
                "backendRefs": [{ "name": ENTITYGRAPH_NAME, "namespace": region_namespace(region), "port": GRAPH_PORT }],
            }],
        },
    })
}

/// What was seen of the entity database in this pass. `database` is None while the ArangoDeployment does not exist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    pub database: Option<bool>,
    pub graph_ready: u32,
}

/// The entity database's status for this pass. `error` is what stopped it being applied, if anything, which the
/// message shows.
pub fn status(region: &str, observed: &Observed, error: Option<&str>) -> EntityDbStatus {
    let ready = observed.database == Some(true) && observed.graph_ready > 0;
    let message = match (error, observed.database) {
        (Some(error), _) => error.to_string(),
        (None, None) => format!("waiting for ArangoDeployment {DATABASE_NAME} to be created"),
        (None, Some(false)) => format!("waiting for ArangoDeployment {DATABASE_NAME} to be Ready"),
        (None, Some(true)) if observed.graph_ready == 0 => format!("waiting for {ENTITYGRAPH_NAME} to be ready"),
        (None, Some(true)) => String::new(),
    };
    EntityDbStatus {
        // What runs decides, so a failed apply of a later change does not hold shards back.
        state: if ready { EntityDbState::Ready } else { EntityDbState::Pending },
        service: entitygraph_address(region),
        database: database_address(region),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> EntityDbSettings {
        EntityDbSettings {
            arango_image: "docker.io/library/arangodb:3.12.13".into(),
            operator_image: "docker.io/arangodb/kube-arangodb:1.4.7".into(),
            entitygraph_image: "localhost:5001/scbackend/sc-entitygraph:dev".into(),
            push: "http://sc-push.scbackend.svc:50051".into(),
            log_level: "info".into(),
            data_secret: "scbackend-data".into(),
            operator_role: "scbackend-sc-shard-operator-region-arango".into(),
            operator_cluster_role: "scbackend-sc-shard-operator-region-arango-cluster".into(),
            member_role: "scbackend-sc-shard-operator-region-arango-member".into(),
            namespace: "scbackend".into(),
            gateway: "sc-edge".into(),
            listener: "grpc".into(),
        }
    }

    fn owner() -> OwnerReference {
        OwnerReference { api_version: "scbackend.io/v1alpha1".into(), kind: "ShardRegion".into(), name: "usa".into(), uid: "uid".into(), ..OwnerReference::default() }
    }

    fn env(manifest: &Value) -> BTreeMap<String, Value> {
        let env = manifest["spec"]["template"]["spec"]["containers"][0]["env"].as_array().unwrap();
        env.iter().map(|var| (var["name"].as_str().unwrap().to_string(), var.get("value").cloned().unwrap_or(var["valueFrom"].clone()))).collect()
    }

    #[test]
    fn addresses_are_full_service_names_in_the_region() {
        assert_eq!(entitygraph_address("usa"), "http://sc-entitygraph.scbackend-region-usa.svc:50051");
        assert_eq!(database_address("usa"), "http://arangodb.scbackend-region-usa.svc:8529");
        assert_eq!(route_name("usa"), "sc-entitygraph-usa");
        assert_eq!(cluster_binding_name("usa"), "scbackend-region-usa-arango-operator");
    }

    #[test]
    fn the_default_database_is_the_dev_cluster_layout() {
        let deployment = arango_deployment("usa", &EntityDbSpec::default(), &settings(), &owner());
        assert_eq!(deployment["apiVersion"], "database.arangodb.com/v1");
        assert_eq!(deployment["kind"], "ArangoDeployment");
        assert_eq!(deployment["metadata"]["name"], "arangodb");
        assert_eq!(deployment["metadata"]["namespace"], "scbackend-region-usa");
        assert_eq!(deployment["metadata"]["ownerReferences"][0]["uid"], "uid");
        let spec = &deployment["spec"];
        assert_eq!(spec["mode"], "Cluster");
        assert_eq!(spec["environment"], "Production");
        assert_eq!(spec["image"], "docker.io/library/arangodb:3.12.13");
        assert_eq!(spec["tls"]["caSecretName"], "None");
        assert!(spec.get("auth").is_none(), "the operator's JWT secret, as before");
        assert!(spec.get("bootstrap").is_none(), "root keeps the empty password sc-db logs in with");
        assert_eq!((spec["agents"]["count"].as_u64(), spec["dbservers"]["count"].as_u64(), spec["coordinators"]["count"].as_u64()), (Some(3), Some(3), Some(2)));
        assert_eq!(spec["agents"]["resources"]["limits"], json!({ "cpu": "100m", "memory": "512Mi" }));
        assert_eq!(spec["dbservers"]["resources"]["requests"], json!({ "cpu": "500m", "memory": "2Gi" }));
        assert_eq!(spec["coordinators"]["resources"]["limits"]["memory"], "1Gi");
        assert_eq!(spec["agents"]["volumeClaimTemplate"]["spec"]["resources"]["requests"]["storage"], "8Gi");
        assert_eq!(spec["dbservers"]["volumeClaimTemplate"]["spec"]["resources"]["requests"]["storage"], "32Gi");
        assert!(spec["dbservers"]["volumeClaimTemplate"]["spec"].get("storageClassName").is_none(), "the cluster's default");
        assert!(spec.get("single").is_none());
    }

    #[test]
    fn a_region_can_ask_for_a_single_server_and_its_own_storage() {
        let spec = EntityDbSpec {
            mode: EntityDbMode::Single,
            storage_class: Some("fast".into()),
            size: "100Gi".into(),
            image: Some("arangodb:3.12.14".into()),
            ..EntityDbSpec::default()
        };
        let deployment = arango_deployment("eu", &spec, &settings(), &owner());
        let body = &deployment["spec"];
        assert_eq!(body["mode"], "Single");
        assert_eq!(body["image"], "arangodb:3.12.14");
        assert_eq!(body["single"]["volumeClaimTemplate"]["spec"]["storageClassName"], "fast");
        assert_eq!(body["single"]["volumeClaimTemplate"]["spec"]["resources"]["requests"]["storage"], "100Gi");
        assert!(body.get("agents").is_none() && body.get("dbservers").is_none() && body.get("coordinators").is_none());
        let cluster = EntityDbSpec { storage_class: Some("fast".into()), agents: 5, ..EntityDbSpec::default() };
        let body = arango_deployment("eu", &cluster, &settings(), &owner())["spec"].clone();
        assert_eq!(body["agents"]["count"], 5);
        assert_eq!(body["agents"]["volumeClaimTemplate"]["spec"]["storageClassName"], "fast");
    }

    #[test]
    fn readiness_is_kube_arangodbs_ready_condition() {
        assert!(arango_ready(&json!({ "status": { "conditions": [{ "type": "UpToDate", "status": "True" }, { "type": "Ready", "status": "True" }] } })));
        assert!(!arango_ready(&json!({ "status": { "conditions": [{ "type": "Ready", "status": "False" }] } })));
        assert!(!arango_ready(&json!({ "spec": {} })));
    }

    #[test]
    fn each_region_runs_kube_arangodb_for_its_namespace_only() {
        let deployment = operator_deployment("usa", &settings(), &owner());
        assert_eq!(deployment["metadata"]["namespace"], "scbackend-region-usa");
        let pod = &deployment["spec"]["template"]["spec"];
        assert_eq!(pod["serviceAccountName"], "arango-operator");
        let container = &pod["containers"][0];
        assert_eq!(container["image"], "docker.io/arangodb/kube-arangodb:1.4.7");
        let args: Vec<&str> = container["args"].as_array().unwrap().iter().map(|arg| arg.as_str().unwrap()).collect();
        assert!(args.contains(&"--operator.deployment"));
        assert!(args.contains(&"--crd.install=false"), "the cluster-wide release owns the CRDs");
        // It watches the namespace this names, its own.
        assert_eq!(env(&deployment)["MY_POD_NAMESPACE"], json!({ "fieldRef": { "fieldPath": "metadata.namespace" } }));
        assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
        let labels = &deployment["spec"]["template"]["metadata"]["labels"];
        for (key, value) in deployment["spec"]["selector"]["matchLabels"].as_object().unwrap() {
            assert_eq!(&labels[key], value);
        }
        serde_json::from_value::<k8s_openapi::api::apps::v1::Deployment>(deployment).unwrap();
    }

    #[test]
    fn the_region_operator_gets_the_charts_roles() {
        let binding = operator_role_binding("usa", &settings(), &owner());
        assert_eq!(binding["roleRef"]["kind"], "ClusterRole");
        assert_eq!(binding["roleRef"]["name"], "scbackend-sc-shard-operator-region-arango");
        assert_eq!(binding["subjects"][0], json!({ "kind": "ServiceAccount", "name": "arango-operator", "namespace": "scbackend-region-usa" }));
        serde_json::from_value::<k8s_openapi::api::rbac::v1::RoleBinding>(binding).unwrap();
        let members = member_role_binding("usa", &settings(), &owner());
        assert_eq!(members["subjects"][0]["name"], "default");
        let cluster = operator_cluster_role_binding("usa", &settings(), &owner());
        assert!(cluster["metadata"].get("namespace").is_none());
        assert_eq!(cluster["metadata"]["ownerReferences"][0]["uid"], "uid", "goes with the region");
        assert_eq!(cluster["roleRef"]["name"], "scbackend-sc-shard-operator-region-arango-cluster");
        serde_json::from_value::<k8s_openapi::api::rbac::v1::ClusterRoleBinding>(cluster).unwrap();
        let account = operator_service_account("usa", &owner());
        serde_json::from_value::<k8s_openapi::api::core::v1::ServiceAccount>(account).unwrap();
    }

    #[test]
    fn entitygraph_runs_against_the_regions_database() {
        let deployment = entitygraph_deployment("usa", &EntityDbSpec::default(), &settings(), "abc", &owner());
        let env = env(&deployment);
        assert_eq!(env["SCBACKEND_ARANGO"], "http://arangodb.scbackend-region-usa.svc:8529");
        assert_eq!(env["SCBACKEND_PUSH"], "http://sc-push.scbackend.svc:50051");
        assert_eq!(env["SCBACKEND_DATA"], "/data");
        assert_eq!(env["SCBACKEND_HEALTH"], "0.0.0.0:8080");
        assert_eq!(env["RUST_LOG"], "info");
        let pod = &deployment["spec"]["template"]["spec"];
        let container = &pod["containers"][0];
        assert_eq!(container["image"], "localhost:5001/scbackend/sc-entitygraph:dev");
        assert_eq!(container["resources"]["limits"]["memory"], "2Gi");
        assert_eq!(container["resources"]["requests"], json!({ "cpu": "100m", "memory": "256Mi" }));
        assert_eq!(container["startupProbe"]["failureThreshold"], 90);
        assert_eq!(container["readinessProbe"]["httpGet"]["port"], "health");
        assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(pod["securityContext"]["runAsUser"], 65532);
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(pod["volumes"][0]["secret"]["secretName"], "scbackend-data");
        assert_eq!(deployment["spec"]["template"]["metadata"]["annotations"]["checksum/data"], "abc");
        assert_eq!(deployment["spec"]["replicas"], 1);
        let service = entitygraph_service("usa", &owner());
        let labels = &deployment["spec"]["template"]["metadata"]["labels"];
        for (key, value) in service["spec"]["selector"].as_object().unwrap() {
            assert_eq!(&labels[key], value);
        }
        assert_eq!(service["spec"]["ports"][0]["targetPort"], container["ports"][0]["name"]);
        serde_json::from_value::<k8s_openapi::api::apps::v1::Deployment>(deployment).unwrap();
        serde_json::from_value::<k8s_openapi::api::core::v1::Service>(service).unwrap();
    }

    #[test]
    fn the_data_secret_is_copied_and_its_checksum_follows_it() {
        let data = BTreeMap::from([("state.json".to_string(), "e30=".to_string())]);
        let secret = data_secret("usa", &settings(), &data, &owner());
        assert_eq!(secret["metadata"]["name"], "scbackend-data");
        assert_eq!(secret["metadata"]["namespace"], "scbackend-region-usa");
        assert_eq!(secret["data"]["state.json"], "e30=");
        serde_json::from_value::<k8s_openapi::api::core::v1::Secret>(secret).unwrap();
        let other = BTreeMap::from([("state.json".to_string(), "e31=".to_string())]);
        assert_eq!(checksum(&data), checksum(&data.clone()));
        assert_ne!(checksum(&data), checksum(&other));
        assert_eq!(checksum(&BTreeMap::new()), "cbf29ce484222325");
    }

    #[test]
    fn the_gateway_reaches_the_region_across_namespaces() {
        let route = grpc_route("usa", &settings(), &owner());
        assert_eq!(route["apiVersion"], "gateway.networking.k8s.io/v1");
        assert_eq!(route["metadata"]["namespace"], "scbackend", "where the gateway takes routes from");
        assert_eq!(route["spec"]["parentRefs"][0], json!({ "name": "sc-edge", "sectionName": "grpc" }));
        let rule = &route["spec"]["rules"][0];
        assert_eq!(rule["matches"][0]["method"], json!({ "type": "Exact", "service": "sc.external.services.entitygraph.v1.EntityGraphService" }));
        assert_eq!(rule["backendRefs"][0], json!({ "name": "sc-entitygraph", "namespace": "scbackend-region-usa", "port": 50051 }));
        let grant = reference_grant("usa", &settings(), &owner());
        assert_eq!(grant["apiVersion"], "gateway.networking.k8s.io/v1beta1");
        assert_eq!(grant["metadata"]["namespace"], "scbackend-region-usa", "in the namespace of what it allows reaching");
        assert_eq!(grant["spec"]["from"][0], json!({ "group": "gateway.networking.k8s.io", "kind": "GRPCRoute", "namespace": "scbackend" }));
        assert_eq!(grant["spec"]["to"][0], json!({ "group": "", "kind": "Service", "name": "sc-entitygraph" }));
    }

    #[test]
    fn the_status_says_what_the_entity_database_waits_for() {
        let pending = status("usa", &Observed::default(), None);
        assert_eq!(pending.state, EntityDbState::Pending);
        assert!(pending.message.contains("to be created"), "{}", pending.message);
        assert_eq!(pending.service, "http://sc-entitygraph.scbackend-region-usa.svc:50051");
        assert_eq!(pending.database, "http://arangodb.scbackend-region-usa.svc:8529");
        let starting = status("usa", &Observed { database: Some(false), graph_ready: 1 }, None);
        assert!(starting.message.contains("to be Ready"));
        let graph = status("usa", &Observed { database: Some(true), graph_ready: 0 }, None);
        assert!(graph.message.contains("sc-entitygraph"));
        let ready = status("usa", &Observed { database: Some(true), graph_ready: 1 }, None);
        assert_eq!((ready.state, ready.message.as_str()), (EntityDbState::Ready, ""));
        let failed = status("usa", &Observed { database: Some(true), graph_ready: 1 }, Some("apply arangodb: denied"));
        assert_eq!((failed.state, failed.message.as_str()), (EntityDbState::Ready, "apply arangodb: denied"), "what runs decides");
        let never = status("usa", &Observed::default(), Some("no kube-arangodb CRDs"));
        assert_eq!((never.state, never.message.as_str()), (EntityDbState::Pending, "no kube-arangodb CRDs"));
    }
}

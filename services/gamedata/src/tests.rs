//! Pulls from a fake registry served in the test (an index, a manifest and a gzip tar layer built here) into the store
//! in memory, which stands for Valkey. The tests marked `ignore` run against a real Valkey cluster, see
//! `scripts/gamedata-valkey-test.sh`.

use std::{
    collections::HashMap,
    convert::Infallible,
    future::Future,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use bytes::Bytes;
use futures_util::StreamExt;
use http::{header, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use sc_api::internal::{game_data_client::GameDataClient, GetGameFileRequest, ListGameBuildsRequest};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic_health::pb::{health_check_response::ServingStatus as Health, health_client::HealthClient, HealthCheckRequest};

use super::*;
use sc_oci::{hex, sha256_digest};

use crate::{
    cache::{key, Backoff, StoredManifest},
    kv::{Kv, Memory},
};

const BUILD: &str = "12913470";
const REPOSITORY: &str = "scbackend/gamedata";
const COLLISION: &str = "collision/1200814982.col";
const EMPTY: &str = "empty.txt";
const OLD: &str = "12000000";

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-gamedata").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file of the image, a bit over two chunks so the stream has three.
fn collision() -> Vec<u8> {
    (0..(2 * cache::PIECE_BYTES + 12_345)).map(|i| (i * 31 % 251) as u8).collect()
}

fn file_manifest(build: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let files: Vec<_> = files
        .iter()
        .map(|(path, data)| json!({"path": path, "size": data.len(), "sha256": hex(&Sha256::digest(data))}))
        .collect();
    serde_json::to_vec(&json!({"build": build, "files": files})).unwrap()
}

/// A gzip tar with these entries, written raw so a test can put `..` in a name.
fn layer(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut directory = tar::Header::new_gnu();
    directory.set_entry_type(tar::EntryType::Directory);
    directory.set_size(0);
    directory.set_mode(0o755);
    directory.set_path("gamedata/").unwrap();
    directory.set_cksum();
    builder.append(&directory, &[][..]).unwrap();
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&tar).unwrap();
    gzip.finish().unwrap()
}

fn good_layer() -> Vec<u8> {
    layer_of(BUILD)
}

fn layer_of(build: &str) -> Vec<u8> {
    let collision = collision();
    let manifest = file_manifest(build, &[(COLLISION, &collision), (EMPTY, b"")]);
    layer(&[
        (&format!("gamedata/{build}/manifest.json"), &manifest),
        (&format!("gamedata/{build}/{COLLISION}"), &collision),
        (&format!("gamedata/{build}/{EMPTY}"), b""),
    ])
}

#[derive(Default)]
struct Fake {
    manifests: HashMap<String, (&'static str, Vec<u8>)>,
    blobs: HashMap<String, Vec<u8>>,
    /// Pulls must present it, after a token challenge.
    token: Option<String>,
    /// Blobs answer with a redirect to another path, as registries on object storage do.
    redirect: bool,
    requests: AtomicUsize,
    /// Requests for a tag, which each pull starts with. A HEAD, which only asks for the digest, is not one.
    pulls: AtomicUsize,
    /// HEADs of a tag: what a replica asks to see whether the tag names another image.
    checks: AtomicUsize,
    address: OnceLock<SocketAddr>,
}

impl Fake {
    /// An image of one layer, tagged `BUILD`, behind an index with an attestation entry first.
    fn image(layer: Vec<u8>, blob: Option<Vec<u8>>) -> (Self, String) {
        Self::image_of(BUILD, layer, blob)
    }

    /// Another image's tags, manifests and blobs too.
    fn with(mut self, other: Self) -> Self {
        self.manifests.extend(other.manifests);
        self.blobs.extend(other.blobs);
        self
    }

    fn image_of(build: &str, layer: Vec<u8>, blob: Option<Vec<u8>>) -> (Self, String) {
        let layer_digest = sha256_digest(&layer);
        let config = b"{}".to_vec();
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": sha256_digest(&config), "size": config.len()},
            "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": layer_digest, "size": layer.len()}],
        }))
        .unwrap();
        let manifest_digest = sha256_digest(&manifest);
        let index = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": sha256_digest(b"attestation"), "size": 11,
                 "platform": {"os": "unknown", "architecture": "unknown"}},
                {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": manifest_digest, "size": manifest.len(),
                 "platform": {"os": "linux", "architecture": "amd64"}},
            ],
        }))
        .unwrap();
        let index_digest = sha256_digest(&index);
        let mut fake = Fake::default();
        fake.manifests.insert(build.into(), ("application/vnd.oci.image.index.v1+json", index));
        fake.manifests.insert(manifest_digest, ("application/vnd.oci.image.manifest.v1+json", manifest));
        fake.blobs.insert(layer_digest, blob.unwrap_or(layer));
        (fake, index_digest)
    }

    /// The same image as a single Docker manifest, without an index.
    fn docker_image(layer: Vec<u8>) -> (Self, String) {
        let manifest = serde_json::to_vec(&json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
            "config": {"mediaType": "application/vnd.docker.container.image.v1+json", "digest": sha256_digest(b"{}"), "size": 2},
            "layers": [{"mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip", "digest": sha256_digest(&layer), "size": layer.len()}],
        }))
        .unwrap();
        let digest = sha256_digest(&manifest);
        let mut fake = Fake::default();
        fake.manifests.insert(BUILD.into(), ("application/vnd.docker.distribution.manifest.v2+json", manifest));
        fake.blobs.insert(sha256_digest(&layer), layer);
        (fake, digest)
    }

    fn respond(&self, request: &Request<Incoming>) -> Response<Full<Bytes>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let reply = |status: StatusCode, body: Vec<u8>| {
            let mut response = Response::new(Full::new(Bytes::from(body)));
            *response.status_mut() = status;
            response
        };
        let path = request.uri().path();
        if path == "/token" {
            let query = request.uri().query().unwrap_or_default();
            assert!(query.contains("scope=repository:scbackend/gamedata:pull"), "{query}");
            assert!(query.contains("service=fake"), "{query}");
            return reply(StatusCode::OK, serde_json::to_vec(&json!({"token": self.token})).unwrap());
        }
        if let Some(digest) = path.strip_prefix("/storage/") {
            return match self.blobs.get(digest) {
                Some(blob) => reply(StatusCode::OK, blob.clone()),
                None => reply(StatusCode::NOT_FOUND, Vec::new()),
            };
        }
        if let Some(token) = &self.token {
            let presented = request.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
            if presented != Some(&format!("Bearer {token}")) {
                let mut response = reply(StatusCode::UNAUTHORIZED, Vec::new());
                let challenge = format!(
                    r#"Bearer realm="http://{}/token",service="fake",scope="repository:{REPOSITORY}:pull""#,
                    self.address.get().unwrap()
                );
                response.headers_mut().insert(header::WWW_AUTHENTICATE, challenge.parse().unwrap());
                return response;
            }
        }
        let Some(rest) = path.strip_prefix(&format!("/v2/{REPOSITORY}/")) else { return reply(StatusCode::NOT_FOUND, Vec::new()) };
        if let Some(reference) = rest.strip_prefix("manifests/") {
            let head = request.method() == http::Method::HEAD;
            if !reference.starts_with("sha256:") {
                let counter = if head { &self.checks } else { &self.pulls };
                counter.fetch_add(1, Ordering::SeqCst);
            }
            return match self.manifests.get(reference) {
                Some((media_type, body)) => {
                    let mut response = reply(StatusCode::OK, if head { Vec::new() } else { body.clone() });
                    response.headers_mut().insert(header::CONTENT_TYPE, media_type.parse().unwrap());
                    response.headers_mut().insert("docker-content-digest", sha256_digest(body).parse().unwrap());
                    response
                }
                None => reply(StatusCode::NOT_FOUND, Vec::new()),
            };
        }
        if let Some(digest) = rest.strip_prefix("blobs/") {
            if self.redirect {
                let mut response = reply(StatusCode::TEMPORARY_REDIRECT, Vec::new());
                response.headers_mut().insert(header::LOCATION, format!("/storage/{digest}").parse().unwrap());
                return response;
            }
            return match self.blobs.get(digest) {
                Some(blob) => reply(StatusCode::OK, blob.clone()),
                None => reply(StatusCode::NOT_FOUND, Vec::new()),
            };
        }
        reply(StatusCode::NOT_FOUND, Vec::new())
    }

    async fn serve(self) -> (Arc<Self>, Registry) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        self.address.set(address).unwrap();
        let fake = Arc::new(self);
        let server = fake.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let fake = server.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let response = fake.respond(&request);
                        async move { Ok::<_, Infallible>(response) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        let registry = Registry::new(&address.to_string(), REPOSITORY, true).unwrap();
        (fake, registry)
    }
}

fn settings(owner: &str) -> Settings {
    Settings {
        owner: owner.into(),
        chunk_bytes: 64 << 20,
        lock: Duration::from_millis(900),
        check: Duration::from_millis(50),
        poll: Duration::from_millis(20),
        recheck: Duration::from_millis(100),
        backoff: Backoff { first: Duration::from_millis(50), max: Duration::from_millis(200) },
        registration: Duration::from_millis(300),
        prune_every: Duration::from_millis(100),
    }
}

async fn cache(kv: &Arc<Memory>, work: &str, builds: &[&str], settings: Settings) -> Arc<Cache> {
    let (health, _) = tonic_health::server::health_reporter();
    let cache = Cache::new(scratch(work), builds.iter().map(ToString::to_string).collect(), settings, health).await.unwrap();
    cache.attach(kv.clone());
    cache
}

fn start(cache: &Arc<Cache>, registry: &Registry) -> tokio::task::JoinHandle<()> {
    let (cache, registry) = (cache.clone(), registry.clone());
    tokio::spawn(async move { cache.run(registry).await })
}

async fn until<F: Future<Output = bool>>(what: &str, check: impl Fn() -> F) {
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A gRPC server for the cache, and a client and a health probe on it.
type Client = GameDataClient<tonic::transport::Channel>;
type Probe = HealthClient<tonic::transport::Channel>;

/// A cache whose gRPC server, with its health service, runs on a port of its own.
async fn served(kv: &Arc<Memory>, work: &str, builds: &[&str], settings: Settings) -> (Arc<Cache>, Client, Probe) {
    let (health, health_service) = tonic_health::server::health_reporter();
    let cache = Cache::new(scratch(work), builds.iter().map(ToString::to_string).collect(), settings, health).await.unwrap();
    cache.attach(kv.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(
        Server::builder()
            .add_service(health_service)
            .add_service(GameDataServer::from_arc(cache.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let channel = tonic::transport::Channel::from_shared(address).unwrap().connect().await.unwrap();
    (cache, GameDataClient::new(channel.clone()), HealthClient::new(channel))
}

async fn serving(probe: &Probe) -> bool {
    let check = HealthCheckRequest { service: String::new() };
    probe.clone().check(check).await.unwrap().get_ref().status == Health::Serving as i32
}

async fn fetch(client: &mut Client, build: &str, path: &str) -> (Vec<u8>, Vec<u64>, Option<tonic::Status>) {
    let request = GetGameFileRequest { build: build.into(), path: path.into() };
    let mut stream = match client.get_file(request).await {
        Ok(response) => response.into_inner(),
        Err(status) => return (Vec::new(), Vec::new(), Some(status)),
    };
    let (mut data, mut sizes) = (Vec::new(), Vec::new());
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                sizes.push(chunk.size);
                data.extend_from_slice(&chunk.data);
            }
            Err(status) => return (data, sizes, Some(status)),
        }
    }
    (data, sizes, None)
}

#[tokio::test]
async fn an_image_behind_an_index_and_a_token_is_pulled_and_checked() {
    let (mut fake, index_digest) = Fake::image(good_layer(), None);
    fake.token = Some("anonymous".into());
    fake.redirect = true;
    let (_fake, registry) = fake.serve().await;
    let work = scratch("pull");
    let pulled = store::pull(&registry, &work, BUILD).await.unwrap();
    assert_eq!(pulled.digest, index_digest);
    assert_eq!(std::fs::read(pulled.root.join(COLLISION)).unwrap(), collision());
    let sizes: Vec<(&str, u64)> = pulled.files.iter().map(|file| (file.path.as_str(), file.size)).collect();
    assert_eq!(sizes, [(COLLISION, collision().len() as u64), (EMPTY, 0)]);
    drop(pulled);
    assert!(!work.join(format!(".pull-{BUILD}")).exists(), "the work directory goes with the pull");
}

#[tokio::test]
async fn a_single_docker_manifest_is_pulled() {
    let (fake, digest) = Fake::docker_image(good_layer());
    let (_fake, registry) = fake.serve().await;
    assert_eq!(store::pull(&registry, &scratch("docker"), BUILD).await.unwrap().digest, digest);
}

#[tokio::test]
async fn a_corrupt_blob_is_refused() {
    let layer = good_layer();
    let mut corrupt = layer.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0xff;
    let (fake, _) = Fake::image(layer, Some(corrupt));
    let (_fake, registry) = fake.serve().await;
    let work = scratch("corrupt");
    let error = store::pull(&registry, &work, BUILD).await.unwrap_err();
    assert!(error.contains("blob sha256:"), "{error}");
    assert!(!work.join(format!(".pull-{BUILD}")).exists());
}

#[tokio::test]
async fn a_file_that_does_not_match_the_manifest_is_refused() {
    let collision = collision();
    let manifest = file_manifest(BUILD, &[(COLLISION, b"something else")]);
    let layer = layer(&[
        (&format!("gamedata/{BUILD}/manifest.json"), &manifest),
        (&format!("gamedata/{BUILD}/{COLLISION}"), &collision),
    ]);
    let (fake, _) = Fake::image(layer, None);
    let (_fake, registry) = fake.serve().await;
    let work = scratch("mismatch");
    let error = store::pull(&registry, &work, BUILD).await.unwrap_err();
    assert!(error.contains(COLLISION), "{error}");
    assert!(!work.join(format!(".pull-{BUILD}")).exists());
}

#[tokio::test]
async fn path_traversal_is_refused() {
    let work = scratch("traversal");
    let evil = layer(&[(&format!("gamedata/{BUILD}/../../escaped"), b"x")]);
    let (fake, _) = Fake::image(evil, None);
    let (_fake, registry) = fake.serve().await;
    let error = store::pull(&registry, &work, BUILD).await.unwrap_err();
    assert!(error.contains("unsafe path"), "{error}");
    assert!(!work.join("escaped").exists() && !work.join(".pull-escaped").exists());

    // Nor may the manifest name a file outside the build, or one whose key would be a build's own or a part's.
    for path in ["../outside", "manifest", "complete", "lock", "a#0"] {
        let manifest = file_manifest(BUILD, &[(path, b"x")]);
        let (fake, _) = Fake::image(layer(&[(&format!("gamedata/{BUILD}/manifest.json"), &manifest)]), None);
        let (_fake, registry) = fake.serve().await;
        let error = store::pull(&registry, &work, BUILD).await.unwrap_err();
        assert!(error.contains("unsafe path"), "{path}: {error}");
    }

    for path in ["../x", "/etc/passwd", "", "a/../b", "./a", "manifest", "a#1"] {
        assert!(store::safe_path(path).is_none(), "{path}");
    }
    assert!(store::safe_path(COLLISION).is_some());
    for build in ["..", ".x", "a/b", ""] {
        assert!(!store::valid_build(build), "{build}");
    }
}

#[test]
fn the_chunk_size_stays_within_a_valkey_value() {
    assert!(Settings::new("p".into(), 64 << 20, Duration::from_secs(1)).is_ok());
    assert!(Settings::new("p".into(), 512 << 20, Duration::from_secs(1)).is_ok());
    assert!(Settings::new("p".into(), (512 << 20) + 1, Duration::from_secs(1)).is_err());
    assert!(Settings::new("p".into(), 1000, Duration::from_secs(1)).is_err());
    assert_eq!(cache::chunks(10, 10), 0);
    assert_eq!(cache::chunks(11, 10), 2);
    assert_eq!(cache::chunks(30, 10), 3);
}

#[tokio::test]
async fn the_service_turns_serving_and_streams_files_from_valkey() {
    let (fake, digest) = Fake::image(good_layer(), None);
    let (_fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let (cache, mut client, probe) = served(&kv, "service", &[BUILD], settings("a")).await;
    assert!(!serving(&probe).await);
    assert_eq!(fetch(&mut client, BUILD, COLLISION).await.2.unwrap().code(), Code::Unavailable);
    let builds = client.list_builds(ListGameBuildsRequest {}).await.unwrap().into_inner().builds;
    assert_eq!((builds.len(), builds[0].ready), (1, false));

    let run = start(&cache, &registry);
    until("serving", || serving(&probe)).await;
    let builds = client.list_builds(ListGameBuildsRequest {}).await.unwrap().into_inner().builds;
    assert_eq!((builds[0].build.as_str(), builds[0].ready, builds[0].file_count), (BUILD, true, 2));
    let mut keys = kv.keys();
    keys.retain(|key| !key.starts_with("gd:replica:"));
    let expected: Vec<String> = [COLLISION, "complete", EMPTY, "manifest"].iter().map(|name| key(BUILD, name)).collect();
    assert_eq!(keys, expected, "the files, the manifest and the marker, no lock left");
    assert_eq!(kv.get(&key(BUILD, "complete")).await.unwrap().unwrap(), digest.as_bytes());
    assert_eq!(kv.smembers("gd:builds").await.unwrap(), [BUILD]);
    let manifest: StoredManifest = serde_json::from_slice(&kv.get(&key(BUILD, "manifest")).await.unwrap().unwrap()).unwrap();
    assert_eq!((manifest.digest.as_str(), manifest.files[0].chunks), (digest.as_str(), 0));
    assert!(std::fs::read_dir(scratch_path("service")).unwrap().next().is_none(), "the work directory is emptied");

    let (data, sizes, error) = fetch(&mut client, BUILD, COLLISION).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(data, collision());
    assert_eq!(sizes, vec![collision().len() as u64, 0, 0]);
    let (data, sizes, error) = fetch(&mut client, BUILD, EMPTY).await;
    assert_eq!((data.len(), sizes, error.map(|e| e.code())), (0, vec![0], None));

    for (build, path, expected) in [
        ("1", COLLISION, Code::NotFound),
        (BUILD, "collision/1.col", Code::NotFound),
        (BUILD, "../12913470/collision/1200814982.col", Code::InvalidArgument),
        (BUILD, "/etc/passwd", Code::InvalidArgument),
        (BUILD, "manifest", Code::InvalidArgument),
        (BUILD, "collision/1200814982.col#0", Code::InvalidArgument),
    ] {
        assert_eq!(fetch(&mut client, build, path).await.2.unwrap().code(), expected, "{build} {path}");
    }

    // Valkey away: not serving, files unavailable; back: serving again.
    kv.down.store(true, Ordering::SeqCst);
    until("not serving", || async { !serving(&probe).await }).await;
    assert_eq!(fetch(&mut client, BUILD, COLLISION).await.2.unwrap().code(), Code::Unavailable);
    kv.down.store(false, Ordering::SeqCst);
    until("serving again", || serving(&probe)).await;
    run.abort();
}

fn scratch_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-gamedata").join(name)
}

#[tokio::test]
async fn one_replica_loads_a_build_while_the_others_wait() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let replicas = [
        cache(&kv, "replica-a", &[BUILD], settings("a")).await,
        cache(&kv, "replica-b", &[BUILD], settings("b")).await,
        cache(&kv, "replica-c", &[BUILD], settings("c")).await,
    ];
    let runs: Vec<_> = replicas.iter().map(|replica| start(replica, &registry)).collect();
    until("every replica ready", || async { replicas.iter().all(|replica| replica.is_ready(BUILD)) }).await;
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 1, "pulled once for the region");
    runs.iter().for_each(|run| run.abort());
}

#[tokio::test]
async fn a_complete_build_is_reused_and_loaded_again_once_lost() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let first = cache(&kv, "reuse-a", &[BUILD], settings("a")).await;
    let run = start(&first, &registry);
    until("loaded", || async { first.is_ready(BUILD) }).await;
    run.abort();
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 1);

    let second = cache(&kv, "reuse-b", &[BUILD], settings("b")).await;
    let run = start(&second, &registry);
    until("reused", || async { second.is_ready(BUILD) }).await;
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 1, "no pull for a complete build");

    // Valkey lost it, say every node restarted without its volume: loaded again.
    kv.del(&key(BUILD, "complete")).await.unwrap();
    until("lost", || async { !second.is_ready(BUILD) }).await;
    until("loaded again", || async { second.is_ready(BUILD) }).await;
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 2);
    run.abort();
}

/// `scripts/gamedata-publish.sh` run again for the same build with other files gives the tag another image. A replica
/// that starts sees it, the build is loaded again once for the region, and the old files are not served.
#[tokio::test]
async fn a_build_pushed_again_under_its_tag_is_loaded_again() {
    let kv = Arc::new(Memory::default());
    let (first, first_digest) = Fake::image(good_layer(), None);
    let (first, first_registry) = first.serve().await;
    let old = cache(&kv, "again-old", &[BUILD], settings("old")).await;
    let run = start(&old, &first_registry);
    until("loaded", || async { old.is_ready(BUILD) }).await;
    // The tag still names the same image: asked about, never pulled again.
    until("the tag checked", || async { first.checks.load(Ordering::SeqCst) >= 2 }).await;
    assert_eq!(first.pulls.load(Ordering::SeqCst), 1);
    run.abort();

    let table = b"8\trows\n".to_vec();
    let manifest = file_manifest(BUILD, &[("tables/home_locations.tsv", &table)]);
    let newer = layer(&[(&format!("gamedata/{BUILD}/manifest.json"), &manifest), (&format!("gamedata/{BUILD}/tables/home_locations.tsv"), &table)]);
    let (second, second_digest) = Fake::image(newer, None);
    assert_ne!(first_digest, second_digest);
    let (second, second_registry) = second.serve().await;
    let (replicas, mut client, _) = served(&kv, "again-new", &[BUILD], settings("new")).await;
    // Another replica sees the new tag at the same time; one of them loads it, the other follows.
    let follower = cache(&kv, "again-follower", &[BUILD], settings("follower")).await;
    let runs = [start(&replicas, &second_registry), start(&follower, &second_registry)];
    let complete = || async { kv.get(&key(BUILD, "complete")).await.unwrap() == Some(second_digest.as_bytes().to_vec()) };
    until("loaded again", complete).await;
    until("served again", || {
        let mut client = client.clone();
        let table = table.clone();
        async move { fetch(&mut client, BUILD, "tables/home_locations.tsv").await.0 == table }
    })
    .await;
    assert_eq!(fetch(&mut client, BUILD, COLLISION).await.2.unwrap().code(), Code::NotFound, "the old files are gone");
    until("both serve the new image", || async {
        [&replicas, &follower].iter().all(|replica| replica.loaded_digest(BUILD).as_deref() == Some(second_digest.as_str()))
    })
    .await;
    assert_eq!(second.pulls.load(Ordering::SeqCst), 1, "pulled once for the region");
    runs.iter().for_each(|run| run.abort());
}

#[tokio::test]
async fn a_build_whose_lock_another_holds_waits_for_it() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    assert!(kv.acquire(&key(BUILD, "lock"), "elsewhere", 60_000).await.unwrap());
    let replica = cache(&kv, "locked", &[BUILD], settings("a")).await;
    let run = start(&replica, &registry);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 0, "another replica is loading it");
    kv.release(&key(BUILD, "lock"), "elsewhere").await.unwrap();
    until("loaded after the lock went", || async { replica.is_ready(BUILD) }).await;
    run.abort();
}

#[tokio::test]
async fn large_files_are_stored_in_parts() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (_fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let parts = Settings { chunk_bytes: 1 << 20, ..settings("a") };
    let (cache, mut client, _probe) = served(&kv, "parts", &[BUILD], parts).await;
    let run = start(&cache, &registry);
    until("loaded", || async { cache.is_ready(BUILD) }).await;
    let whole = key(BUILD, COLLISION);
    let lengths = [kv.strlen(&format!("{whole}#0")).await.unwrap(), kv.strlen(&format!("{whole}#1")).await.unwrap(), kv.strlen(&format!("{whole}#2")).await.unwrap()];
    assert_eq!(lengths, [1 << 20, 1 << 20, 12_345]);
    assert_eq!(kv.get(&whole).await.unwrap(), None);
    let manifest: StoredManifest = serde_json::from_slice(&kv.get(&key(BUILD, "manifest")).await.unwrap().unwrap()).unwrap();
    assert_eq!((manifest.chunk_bytes, manifest.files[0].chunks), (1 << 20, 3));
    let (data, _, error) = fetch(&mut client, BUILD, COLLISION).await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(data, collision());
    run.abort();
}

#[tokio::test]
async fn a_file_changed_in_valkey_is_never_served_whole() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (_fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let (cache, mut client, _probe) = served(&kv, "changed", &[BUILD], settings("a")).await;
    let run = start(&cache, &registry);
    until("loaded", || async { cache.is_ready(BUILD) }).await;
    let mut corrupt = collision();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    kv.put(&key(BUILD, COLLISION), &corrupt);
    let (data, _, error) = fetch(&mut client, BUILD, COLLISION).await;
    assert_eq!(error.unwrap().code(), Code::DataLoss);
    assert!(data.len() < corrupt.len(), "the last message is held back");
    kv.put(&key(BUILD, COLLISION), b"short");
    assert_eq!(fetch(&mut client, BUILD, COLLISION).await.2.unwrap().code(), Code::DataLoss);
    run.abort();
}

#[tokio::test]
async fn builds_no_replica_wants_are_pruned() {
    let (old, _) = Fake::image_of(OLD, layer_of(OLD), None);
    let (fake, _) = Fake::image(good_layer(), None);
    let (_fake, registry) = fake.with(old).serve().await;
    let kv = Arc::new(Memory::default());
    let before = cache(&kv, "prune-old", &[OLD], settings("old")).await;
    let after = cache(&kv, "prune-new", &[BUILD], settings("new")).await;
    let old_run = start(&before, &registry);
    until("old build loaded", || async { before.is_ready(OLD) }).await;
    let new_run = start(&after, &registry);
    until("new build loaded", || async { after.is_ready(BUILD) }).await;
    // The replica with the old list still runs, so its build stays.
    assert_eq!(after.prune(kv.as_ref()).await.unwrap(), []);
    assert!(kv.get(&key(OLD, "complete")).await.unwrap().is_some());

    // Once it is gone and its registration expired, the build goes, and only it.
    old_run.abort();
    until("old build pruned", || async { kv.keys().iter().all(|key| !key.starts_with(&format!("gd:{OLD}:"))) }).await;
    assert_eq!(kv.smembers("gd:builds").await.unwrap(), [BUILD]);
    assert_eq!(kv.smembers("gd:replicas").await.unwrap(), ["new"]);
    assert!(after.is_ready(BUILD) && kv.get(&key(BUILD, COLLISION)).await.unwrap().is_some());
    new_run.abort();
}

#[tokio::test]
async fn pruning_reports_what_it_freed_and_skips_a_build_being_loaded() {
    let (old, _) = Fake::image_of(OLD, layer_of(OLD), None);
    let (_fake, registry) = old.serve().await;
    let kv = Arc::new(Memory::default());
    let before = cache(&kv, "freed-old", &[OLD], settings("old")).await;
    let run = start(&before, &registry);
    until("old build loaded", || async { before.is_ready(OLD) }).await;
    run.abort();
    let pruner = cache(&kv, "freed-new", &[BUILD], Settings { registration: Duration::from_secs(60), ..settings("new") }).await;
    kv.del("gd:replica:old").await.unwrap();
    kv.acquire(&key(OLD, "lock"), "loader", 60_000).await.unwrap();
    assert_eq!(pruner.prune(kv.as_ref()).await.unwrap(), [], "locked: a replica is loading it");
    kv.release(&key(OLD, "lock"), "loader").await.unwrap();
    let freed = pruner.prune(kv.as_ref()).await.unwrap();
    assert_eq!(freed, [cache::Freed { build: OLD.into(), files: 2, bytes: collision().len() as u64 }]);
    assert!(kv.keys().is_empty(), "{:?}", kv.keys());
}

#[tokio::test]
async fn a_build_missing_from_the_registry_is_retried_and_others_served() {
    let (fake, _) = Fake::image(good_layer(), None);
    let (_fake, registry) = fake.serve().await;
    let kv = Arc::new(Memory::default());
    let (cache, mut client, probe) = served(&kv, "missing", &[BUILD, "99999999"], settings("a")).await;
    let run = start(&cache, &registry);
    until("one build loaded", || async { cache.is_ready(BUILD) }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!cache.is_ready("99999999"));
    assert!(!serving(&probe).await, "not every build is there");
    assert!(fetch(&mut client, BUILD, COLLISION).await.2.is_none());
    assert!(kv.get(&key("99999999", "lock")).await.unwrap().is_none(), "the lock goes with a failed load");
    run.abort();
}

/// The nodes of a real Valkey, from `SC_GAMEDATA_TEST_VALKEY`, in cluster mode unless `SC_GAMEDATA_TEST_VALKEY_CLUSTER`
/// is `false`.
async fn real_valkey() -> Arc<dyn Kv> {
    let nodes: Vec<String> = std::env::var("SC_GAMEDATA_TEST_VALKEY")
        .expect("SC_GAMEDATA_TEST_VALKEY: redis://host:port,...")
        .split(',')
        .map(str::to_string)
        .collect();
    match std::env::var("SC_GAMEDATA_TEST_VALKEY_CLUSTER").as_deref() {
        Ok("false") => Arc::new(crate::kv::single(&nodes[0]).await.unwrap()),
        _ => Arc::new(crate::kv::cluster(&nodes).await.unwrap()),
    }
}

/// Two replicas against a real Valkey cluster: one loads, both serve, files in parts spread over the nodes come back
/// whole, and the build nobody wants any more is pruned. Run by `scripts/gamedata-valkey-test.sh` on a new cluster.
#[tokio::test]
#[ignore = "needs a Valkey cluster, see scripts/gamedata-valkey-test.sh"]
async fn real_valkey_cluster_loads_serves_and_prunes() {
    let kv = real_valkey().await;
    kv.ping().await.unwrap();
    let (old, _) = Fake::image_of(OLD, layer_of(OLD), None);
    let (fake, _) = Fake::image(good_layer(), None);
    let (fake, registry) = fake.with(old).serve().await;
    let parts = |owner: &str| Settings { chunk_bytes: 1 << 20, ..settings(owner) };
    let make = |work: &str, builds: &[&str], settings: Settings| {
        let (kv, work, builds) = (kv.clone(), work.to_string(), builds.iter().map(ToString::to_string).collect::<Vec<_>>());
        async move {
            let (health, _) = tonic_health::server::health_reporter();
            let cache = Cache::new(scratch(&work), builds, settings, health).await.unwrap();
            cache.attach(kv);
            cache
        }
    };
    let before = make("real-old", &[OLD], parts("old")).await;
    let old_run = start(&before, &registry);
    until("old build loaded", || async { before.is_ready(OLD) }).await;

    let a = make("real-a", &[BUILD], parts("a")).await;
    let b = make("real-b", &[BUILD], parts("b")).await;
    let runs = [start(&a, &registry), start(&b, &registry)];
    until("both replicas ready", || async { a.is_ready(BUILD) && b.is_ready(BUILD) }).await;
    assert_eq!(fake.pulls.load(Ordering::SeqCst), 2, "each build pulled once");

    for replica in [&a, &b] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(Server::builder().add_service(GameDataServer::from_arc(replica.clone())).serve_with_incoming(TcpListenerStream::new(listener)));
        let mut client = GameDataClient::connect(address).await.unwrap();
        let (data, sizes, error) = fetch(&mut client, BUILD, COLLISION).await;
        assert!(error.is_none(), "{error:?}");
        assert_eq!(data, collision());
        assert_eq!(sizes.len(), 3);
    }

    old_run.abort();
    until("old build pruned", || async { kv.get(&key(OLD, "manifest")).await.unwrap().is_none() }).await;
    for part in 0..3 {
        assert_eq!(kv.strlen(&format!("{}#{part}", key(OLD, COLLISION))).await.unwrap(), 0);
    }
    assert!(kv.get(&key(BUILD, "complete")).await.unwrap().is_some());
    runs.iter().for_each(|run| run.abort());
}

/// A running sc-gamedata (`SC_GAMEDATA_TEST_ADDR`) serves `SC_GAMEDATA_TEST_PATH` of `SC_GAMEDATA_TEST_BUILD` byte for
/// byte as the file `SC_GAMEDATA_TEST_EXPECT`.
#[tokio::test]
#[ignore = "needs a running sc-gamedata, see scripts/gamedata-valkey-test.sh"]
async fn running_service_serves_the_file_byte_identical() {
    let variable = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} not set"));
    let channel = tonic::transport::Channel::from_shared(variable("SC_GAMEDATA_TEST_ADDR")).unwrap().connect().await.unwrap();
    let (mut client, probe) = (GameDataClient::new(channel.clone()), HealthClient::new(channel));
    until("the service serving", || serving(&probe)).await;
    let (data, _, error) = fetch(&mut client, &variable("SC_GAMEDATA_TEST_BUILD"), &variable("SC_GAMEDATA_TEST_PATH")).await;
    assert!(error.is_none(), "{error:?}");
    let expected = std::fs::read(variable("SC_GAMEDATA_TEST_EXPECT")).unwrap();
    assert_eq!(data.len(), expected.len());
    assert!(data == expected, "the bytes differ");
    println!("{} bytes, sha256 {}", data.len(), hex(&Sha256::digest(&data)));
}

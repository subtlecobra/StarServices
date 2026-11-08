//! The game data of the builds a replica is given, kept in the region's Valkey cluster, which `sc-gamedata` alone reads
//! and writes. The replicas share it: whichever takes a build's lock first pulls the build's image and copies it in,
//! the others wait for it, and all of them serve every complete build.
//!
//! Keys, spread over the cluster's nodes by hash slot, so a build's files are spread by path:
//!
//! | Key | Value |
//! |---|---|
//! | `gd:<build>:<path>` | A file of the build, as the image has it |
//! | `gd:<build>:<path>#<n>` | Part `n` (from 0) of a file larger than the chunk size, which the manifest says |
//! | `gd:<build>:manifest` | JSON: the build, the image digest, the chunk size, and each file's path, size, sha256 and parts |
//! | `gd:<build>:complete` | The image digest, written last: the build is whole |
//! | `gd:<build>:lock` | The replica loading (or pruning) the build, with an expiry it renews while it works |
//! | `gd:builds` | Set of every build that was ever started, so pruning finds them without scanning every node |
//! | `gd:replica:<pod>` | JSON list of the builds a replica is configured with, expiring unless renewed |
//! | `gd:replicas` | Set of the replicas that registered |
//! | `gd:prune:lock` | The replica pruning |
//!
//! Serving reads a file in 1 MiB ranges (`GETRANGE`), so a replica holds at most a range per request whatever the file
//! size, and hashes it as it goes: the last message is held back until the sha256 matches the manifest, and a mismatch
//! ends the stream with DATA_LOSS instead, so a client never gets a whole stream of wrong bytes.
//!
//! A build pushed again under the same tag (a fixed `gamedata-build`, new tables) is a new image digest. Each replica asks
//! the registry what the tag names when it starts and every `recheck` after, a HEAD of the manifest, and when that is not
//! the digest `complete` holds, the replica that takes the build's lock loads the build again: it pulls the new image
//! first, then replaces the files in Valkey and writes the new digest to `complete`, which the other replicas follow. A
//! file read while it is replaced fails its size or sha256 check rather than being served wrong. A registry that does not
//! answer leaves the build as it is.
//!
//! Pruning: a build in `gd:builds` that no live replica is configured with (every replica registers its list, and the
//! operator keeps the builds of running shards in that list) is deleted by the replica that holds the prune lock, under
//! the build's own lock so it never races a load. A replica still running an older list during a rollout keeps that
//! list's builds until it is gone and its registration expires.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock, RwLock,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sc_api::internal::{
    game_data_server::GameData, GameBuildStatus, GameFileChunk, GetGameFileRequest, ListGameBuildsRequest, ListGameBuildsResponse,
};
use tokio::{io::AsyncReadExt, sync::mpsc};
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Request, Response, Status};
use tonic_health::{server::HealthReporter, ServingStatus};
use tracing::{info, warn};

use sc_oci::{hex, Registry};

use crate::{
    kv::{Kv, MAX_VALUE_BYTES},
    store,
};

/// What a gRPC message carries, and what a read or write of Valkey moves at once.
pub const PIECE_BYTES: usize = 1 << 20;
/// Names of a build's own keys, which no file may have.
pub const RESERVED: [&str; 3] = ["manifest", "complete", "lock"];
const BUILDS_SET: &str = "gd:builds";
const REPLICAS_SET: &str = "gd:replicas";
const PRUNE_LOCK: &str = "gd:prune:lock";

pub fn key(build: &str, name: &str) -> String {
    format!("gd:{build}:{name}")
}

fn replica_key(owner: &str) -> String {
    format!("gd:replica:{owner}")
}

/// A file of a stored build. `chunks` 0: one value; otherwise that many parts of `chunk_bytes`, the last one shorter.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StoredFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub chunks: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredManifest {
    pub build: String,
    pub digest: String,
    pub chunk_bytes: u64,
    pub files: Vec<StoredFile>,
}

impl StoredManifest {
    /// The keys of a file and how long each value is.
    pub fn parts(&self, file: &StoredFile) -> Vec<(String, u64)> {
        let whole = key(&self.build, &file.path);
        if file.chunks == 0 {
            return vec![(whole, file.size)];
        }
        (0..u64::from(file.chunks))
            .map(|n| (format!("{whole}#{n}"), self.chunk_bytes.min(file.size - n * self.chunk_bytes)))
            .collect()
    }
}

/// How many parts a file of `size` takes: none above one value.
pub fn chunks(size: u64, chunk_bytes: u64) -> u32 {
    match size > chunk_bytes {
        true => size.div_ceil(chunk_bytes) as u32,
        false => 0,
    }
}

/// How long a failed load waits before it is tried again: doubling from `first` up to `max`.
#[derive(Clone, Copy)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

/// Timing and identity of a replica.
#[derive(Clone)]
pub struct Settings {
    /// Who holds a lock: the pod's name.
    pub owner: String,
    /// Files larger than this are stored in parts of this size. At most Valkey's 512 MiB.
    pub chunk_bytes: u64,
    /// A lock's expiry, renewed at a third of it.
    pub lock: Duration,
    /// How often a complete build is checked to still be there, and a missing one looked at again.
    pub check: Duration,
    /// How often a replica waiting for another one's load looks.
    pub poll: Duration,
    /// How often the registry is asked whether a complete build's tag still names the image loaded.
    pub recheck: Duration,
    pub backoff: Backoff,
    /// A replica's registration expires after this, renewed at a third of it.
    pub registration: Duration,
    pub prune_every: Duration,
}

impl Settings {
    pub fn new(owner: String, chunk_bytes: u64, recheck: Duration) -> Result<Self, String> {
        if chunk_bytes < PIECE_BYTES as u64 || chunk_bytes > MAX_VALUE_BYTES {
            return Err(format!("chunk size {chunk_bytes} is not between {PIECE_BYTES} and {MAX_VALUE_BYTES} bytes"));
        }
        Ok(Self {
            owner,
            chunk_bytes,
            lock: Duration::from_secs(30),
            check: Duration::from_secs(10),
            poll: Duration::from_secs(2),
            recheck,
            backoff: Backoff { first: Duration::from_secs(5), max: Duration::from_secs(300) },
            registration: Duration::from_secs(60),
            prune_every: Duration::from_secs(60),
        })
    }
}

/// A complete build, as this replica serves it.
pub struct Ready {
    manifest: StoredManifest,
    files: HashMap<String, StoredFile>,
}

impl Ready {
    fn new(manifest: StoredManifest) -> Arc<Self> {
        let files = manifest.files.iter().map(|file| (file.path.clone(), file.clone())).collect();
        Arc::new(Self { manifest, files })
    }
}

/// How a reload of a build ended.
enum Reload {
    /// The digest now loaded.
    Loaded(String),
    /// Another replica holds the lock, or has loaded it already.
    Busy,
    Failed,
}

/// What pruning freed of one build.
#[derive(Debug, PartialEq, Eq)]
pub struct Freed {
    pub build: String,
    pub files: usize,
    pub bytes: u64,
}

pub struct Cache {
    pub builds: Vec<String>,
    work: PathBuf,
    settings: Settings,
    kv: OnceLock<Arc<dyn Kv>>,
    ready: RwLock<HashMap<String, Arc<Ready>>>,
    valkey_up: AtomicBool,
    serving: std::sync::Mutex<Option<bool>>,
    health: HealthReporter,
}

impl Cache {
    pub async fn new(work: PathBuf, builds: Vec<String>, settings: Settings, health: HealthReporter) -> Result<Arc<Self>, String> {
        let mut unique = Vec::new();
        for build in builds.into_iter().map(|b| b.trim().to_string()).filter(|b| !b.is_empty()) {
            if !store::valid_build(&build) {
                return Err(format!("build {build:?} is not a valid name"));
            }
            if !unique.contains(&build) {
                unique.push(build);
            }
        }
        if unique.is_empty() {
            return Err("no builds".into());
        }
        std::fs::create_dir_all(&work).map_err(|error| format!("{}: {error}", work.display()))?;
        health.set_service_status("", ServingStatus::NotServing).await;
        Ok(Arc::new(Self {
            builds: unique,
            work,
            settings,
            kv: OnceLock::new(),
            ready: RwLock::default(),
            valkey_up: AtomicBool::new(false),
            serving: std::sync::Mutex::new(None),
            health,
        }))
    }

    /// The store, once connected. Until then nothing is ready.
    pub fn attach(&self, kv: Arc<dyn Kv>) {
        if self.kv.set(kv).is_ok() {
            self.valkey_up.store(true, Ordering::SeqCst);
        }
    }

    fn kv(&self) -> Option<Arc<dyn Kv>> {
        self.kv.get().cloned()
    }

    #[cfg(test)]
    pub fn is_ready(&self, build: &str) -> bool {
        self.ready.read().unwrap().contains_key(build)
    }

    #[cfg(test)]
    pub fn loaded_digest(&self, build: &str) -> Option<String> {
        self.ready.read().unwrap().get(build).map(|ready| ready.manifest.digest.clone())
    }

    /// SERVING when Valkey answers and every build is complete in it.
    async fn refresh_health(&self) {
        let all = {
            let ready = self.ready.read().unwrap();
            self.builds.iter().all(|build| ready.contains_key(build))
        };
        let serving = all && self.valkey_up.load(Ordering::SeqCst);
        let changed = self.serving.lock().unwrap().replace(serving) != Some(serving);
        if changed {
            match serving {
                true => info!(builds = self.builds.len(), "every build ready"),
                false => warn!(valkey = self.valkey_up.load(Ordering::SeqCst), "not serving"),
            }
        }
        let status = if serving { ServingStatus::Serving } else { ServingStatus::NotServing };
        self.health.set_service_status("", status).await;
    }

    async fn mark_ready(&self, build: &str, ready: Arc<Ready>) {
        self.ready.write().unwrap().insert(build.to_string(), ready);
        self.refresh_health().await;
    }

    async fn mark_missing(&self, build: &str) {
        if self.ready.write().unwrap().remove(build).is_some() {
            warn!(build, "build no longer complete in valkey");
            self.refresh_health().await;
        }
    }

    /// Keeps every build complete in Valkey, registers the replica, prunes and watches Valkey's health. Runs for good.
    pub async fn run(self: &Arc<Self>, registry: Registry) {
        let mut tasks = tokio::task::JoinSet::new();
        for build in self.builds.clone() {
            let (cache, registry) = (self.clone(), registry.clone());
            tasks.spawn(async move { cache.keep(build, registry).await });
        }
        let cache = self.clone();
        tasks.spawn(async move { cache.housekeeping().await });
        while tasks.join_next().await.is_some() {}
    }

    async fn keep(self: Arc<Self>, build: String, registry: Registry) {
        let mut wait = self.settings.backoff.first;
        // What the build's tag named when last asked, and when that was.
        let (mut tagged, mut asked): (Option<String>, Option<Instant>) = (None, None);
        loop {
            let Some(kv) = self.kv() else {
                tokio::time::sleep(self.settings.poll).await;
                continue;
            };
            match kv.get(&key(&build, "complete")).await {
                Ok(Some(digest)) => {
                    let digest = String::from_utf8_lossy(&digest).into_owned();
                    let current = self.ready.read().unwrap().get(&build).map(|ready| ready.manifest.digest.clone());
                    if current.as_deref() != Some(digest.as_str()) {
                        match read_manifest(kv.as_ref(), &build).await {
                            Ok(Some(manifest)) => {
                                info!(build, files = manifest.files.len(), digest = manifest.digest, "build complete in valkey");
                                self.mark_ready(&build, Ready::new(manifest)).await;
                            }
                            Ok(None) => warn!(build, "complete build without a manifest"),
                            Err(error) => warn!(build, %error, "manifest unreadable"),
                        }
                    }
                    if asked.is_none_or(|at| at.elapsed() >= self.settings.recheck) {
                        asked = Some(Instant::now());
                        match registry.digest(&build).await {
                            Ok(digest) => tagged = Some(digest),
                            Err(error) => warn!(build, %error, "the build's tag not checked, keeping what is loaded"),
                        }
                    }
                    match tagged.as_deref() {
                        Some(newer) if newer != digest => match self.reload(&kv, &registry, &build, &digest, newer).await {
                            Reload::Loaded(loaded) => {
                                tagged = Some(loaded);
                                wait = self.settings.backoff.first;
                            }
                            Reload::Busy => tokio::time::sleep(self.settings.poll).await,
                            Reload::Failed => {
                                tokio::time::sleep(wait).await;
                                wait = (wait * 2).min(self.settings.backoff.max);
                            }
                        },
                        _ => tokio::time::sleep(self.settings.check).await,
                    }
                }
                Ok(None) => {
                    self.mark_missing(&build).await;
                    match kv.acquire(&key(&build, "lock"), &self.settings.owner, self.settings.lock.as_millis() as u64).await {
                        Ok(true) => {
                            let started = Instant::now();
                            match self.load(&kv, &registry, &build).await {
                                Ok(ready) => {
                                    info!(build, files = ready.files.len(), elapsed = ?started.elapsed(), "build loaded into valkey");
                                    self.mark_ready(&build, ready).await;
                                    wait = self.settings.backoff.first;
                                }
                                Err(error) => {
                                    warn!(build, %error, retry = ?wait, "build not loaded");
                                    tokio::time::sleep(wait).await;
                                    wait = (wait * 2).min(self.settings.backoff.max);
                                }
                            }
                        }
                        // Another replica loads it.
                        Ok(false) => tokio::time::sleep(self.settings.poll).await,
                        Err(error) => {
                            warn!(build, %error, "lock not taken");
                            tokio::time::sleep(self.settings.poll).await;
                        }
                    }
                }
                Err(error) => {
                    warn!(build, %error, "build not checked");
                    tokio::time::sleep(self.settings.poll).await;
                }
            }
        }
    }

    /// Loads a complete build again because its tag names `newer` now, not `loaded`, under the build's lock. Returns the
    /// digest loaded on success.
    async fn reload(&self, kv: &Arc<dyn Kv>, registry: &Registry, build: &str, loaded: &str, newer: &str) -> Reload {
        let lock = key(build, "lock");
        match kv.acquire(&lock, &self.settings.owner, self.settings.lock.as_millis() as u64).await {
            Ok(true) => {}
            Ok(false) => return Reload::Busy,
            Err(error) => {
                warn!(build, %error, "lock not taken");
                return Reload::Busy;
            }
        }
        // Another replica may have loaded it again between the look and the lock.
        match kv.get(&key(build, "complete")).await {
            Ok(Some(now)) if now.as_slice() == loaded.as_bytes() => {}
            _ => {
                if let Err(error) = kv.release(&lock, &self.settings.owner).await {
                    warn!(build, %error, "lock not released, it expires");
                }
                return Reload::Busy;
            }
        }
        info!(build, loaded, tagged = newer, "the build's tag names another image, loading it again");
        let started = Instant::now();
        match self.load(kv, registry, build).await {
            Ok(ready) => {
                let digest = ready.manifest.digest.clone();
                info!(build, files = ready.files.len(), digest, elapsed = ?started.elapsed(), "build loaded into valkey again");
                self.mark_ready(build, ready).await;
                Reload::Loaded(digest)
            }
            Err(error) => {
                warn!(build, %error, "build not loaded again, serving what is loaded");
                Reload::Failed
            }
        }
    }

    /// Loads a build under its lock, which a task renews meanwhile; a lost lock stops the load before it is complete.
    async fn load(&self, kv: &Arc<dyn Kv>, registry: &Registry, build: &str) -> Result<Arc<Ready>, String> {
        let lock = key(build, "lock");
        let lost = Arc::new(AtomicBool::new(false));
        let renewal = tokio::spawn({
            let (kv, lock, owner, lost) = (kv.clone(), lock.clone(), self.settings.owner.clone(), lost.clone());
            let expiry = self.settings.lock;
            async move {
                loop {
                    tokio::time::sleep(expiry / 3).await;
                    if !matches!(kv.renew(&lock, &owner, expiry.as_millis() as u64).await, Ok(true)) {
                        lost.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
        let result = self.load_locked(kv, registry, build, &lost).await;
        renewal.abort();
        if let Err(error) = kv.release(&lock, &self.settings.owner).await {
            warn!(build, %error, "lock not released, it expires");
        }
        result
    }

    async fn load_locked(&self, kv: &Arc<dyn Kv>, registry: &Registry, build: &str, lost: &AtomicBool) -> Result<Arc<Ready>, String> {
        kv.sadd(BUILDS_SET, build).await?;
        // Pulled first, so a build loaded again stays served until its new files are on hand.
        let pulled = store::pull(registry, &self.work, build).await?;
        // What an attempt that did not finish left, or the build this one replaces.
        if let Some(old) = read_manifest(kv.as_ref(), build).await? {
            delete_build(kv.as_ref(), &old).await?;
        }
        let chunk_bytes = self.settings.chunk_bytes;
        let files = pulled
            .files
            .iter()
            .map(|file| StoredFile { path: file.path.clone(), size: file.size, sha256: file.sha256.clone(), chunks: chunks(file.size, chunk_bytes) })
            .collect();
        let manifest = StoredManifest { build: build.to_string(), digest: pulled.digest.clone(), chunk_bytes, files };
        let json = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
        kv.set(&key(build, "manifest"), &json).await?;
        for file in &manifest.files {
            if lost.load(Ordering::SeqCst) {
                return Err("lost the build's lock".into());
            }
            upload(kv.as_ref(), &pulled.root, &manifest, file).await?;
        }
        if lost.load(Ordering::SeqCst) || !kv.renew(&key(build, "lock"), &self.settings.owner, self.settings.lock.as_millis() as u64).await? {
            return Err("lost the build's lock".into());
        }
        kv.set(&key(build, "complete"), manifest.digest.as_bytes()).await?;
        Ok(Ready::new(manifest))
    }

    /// Checks that Valkey answers (every `poll`), registers the builds this replica wants (at a third of the
    /// registration's life) and prunes (every `prune_every`).
    async fn housekeeping(&self) {
        let (mut registered, mut pruned): (Option<Instant>, Option<Instant>) = (None, None);
        let list = serde_json::to_vec(&self.builds).unwrap_or_default();
        loop {
            match self.kv() {
                Some(kv) => {
                    let up = kv.ping().await.inspect_err(|error| warn!(%error, "valkey does not answer")).is_ok();
                    self.valkey_up.store(up, Ordering::SeqCst);
                    if up && registered.is_none_or(|last| last.elapsed() >= self.settings.registration / 3) {
                        let registration = async {
                            kv.set_px(&replica_key(&self.settings.owner), &list, self.settings.registration.as_millis() as u64).await?;
                            kv.sadd(REPLICAS_SET, &self.settings.owner).await
                        };
                        match registration.await {
                            Ok(()) => registered = Some(Instant::now()),
                            Err(error) => warn!(%error, "replica not registered"),
                        }
                    }
                    if registered.is_some() && pruned.is_none_or(|last| last.elapsed() >= self.settings.prune_every) {
                        pruned = Some(Instant::now());
                        if let Err(error) = self.prune(kv.as_ref()).await {
                            warn!(%error, "pruning failed");
                        }
                    }
                }
                None => self.valkey_up.store(false, Ordering::SeqCst),
            }
            self.refresh_health().await;
            tokio::time::sleep(self.settings.poll).await;
        }
    }

    /// Deletes every build no live replica wants, under the prune lock. Returns what it freed.
    pub async fn prune(&self, kv: &dyn Kv) -> Result<Vec<Freed>, String> {
        let owner = &self.settings.owner;
        if !kv.acquire(PRUNE_LOCK, owner, self.settings.lock.as_millis() as u64 * 10).await? {
            return Ok(Vec::new());
        }
        let result = self.prune_locked(kv).await;
        kv.release(PRUNE_LOCK, owner).await?;
        result
    }

    async fn prune_locked(&self, kv: &dyn Kv) -> Result<Vec<Freed>, String> {
        let mut wanted: HashSet<String> = self.builds.iter().cloned().collect();
        for replica in kv.smembers(REPLICAS_SET).await? {
            match kv.get(&replica_key(&replica)).await? {
                Some(list) => wanted.extend(serde_json::from_slice::<Vec<String>>(&list).unwrap_or_default()),
                None => kv.srem(REPLICAS_SET, &replica).await?,
            }
        }
        let mut freed = Vec::new();
        for build in kv.smembers(BUILDS_SET).await? {
            if wanted.contains(&build) {
                continue;
            }
            if !store::valid_build(&build) {
                kv.srem(BUILDS_SET, &build).await?;
                continue;
            }
            let lock = key(&build, "lock");
            // A replica is loading it, so some replica wants it after all.
            if !kv.acquire(&lock, &self.settings.owner, self.settings.lock.as_millis() as u64).await? {
                continue;
            }
            let result = async {
                let manifest = read_manifest(kv, &build).await?;
                let (files, bytes) = match &manifest {
                    Some(manifest) => delete_build(kv, manifest).await?,
                    None => {
                        kv.del(&key(&build, "complete")).await?;
                        (0, 0)
                    }
                };
                kv.srem(BUILDS_SET, &build).await?;
                Ok::<_, String>(Freed { build: build.clone(), files, bytes })
            }
            .await;
            kv.release(&lock, &self.settings.owner).await?;
            let result = result?;
            info!(build, files = result.files, bytes = result.bytes, "build pruned from valkey, no replica wants it");
            freed.push(result);
        }
        Ok(freed)
    }
}

async fn read_manifest(kv: &dyn Kv, build: &str) -> Result<Option<StoredManifest>, String> {
    let Some(json) = kv.get(&key(build, "manifest")).await? else { return Ok(None) };
    let manifest: StoredManifest = serde_json::from_slice(&json).map_err(|error| format!("manifest of {build}: {error}"))?;
    if manifest.build != build || manifest.chunk_bytes == 0 {
        return Err(format!("manifest of {build} is for build {}", manifest.build));
    }
    for file in &manifest.files {
        if store::safe_path(&file.path).is_none() || (file.chunks > 0 && u64::from(file.chunks) != file.size.div_ceil(manifest.chunk_bytes)) {
            return Err(format!("manifest of {build} lists {:?} wrongly", file.path));
        }
    }
    Ok(Some(manifest))
}

/// Deletes a build: the complete marker first, so nobody takes it as whole, then its files and the manifest. Returns
/// how many files and bytes went.
async fn delete_build(kv: &dyn Kv, manifest: &StoredManifest) -> Result<(usize, u64), String> {
    kv.del(&key(&manifest.build, "complete")).await?;
    let mut bytes = 0;
    for file in &manifest.files {
        for (part, _) in manifest.parts(file) {
            kv.del(&part).await?;
        }
        bytes += file.size;
    }
    kv.del(&key(&manifest.build, "manifest")).await?;
    Ok((manifest.files.len(), bytes))
}

/// Copies a file into its keys a piece at a time: SET the first piece of each part, APPEND the rest.
async fn upload(kv: &dyn Kv, root: &Path, manifest: &StoredManifest, file: &StoredFile) -> Result<(), String> {
    let path = root.join(&file.path);
    let mut reader = tokio::fs::File::open(&path).await.map_err(|error| format!("{}: {error}", file.path))?;
    let mut buffer = vec![0u8; PIECE_BYTES];
    for (part, length) in manifest.parts(file) {
        if length > MAX_VALUE_BYTES {
            return Err(format!("{part}: {length} bytes is more than a value holds"));
        }
        let mut written = 0u64;
        loop {
            let want = (length - written).min(PIECE_BYTES as u64) as usize;
            reader.read_exact(&mut buffer[..want]).await.map_err(|error| format!("{}: {error}", file.path))?;
            match written {
                0 => kv.set(&part, &buffer[..want]).await?,
                _ => kv.append(&part, &buffer[..want]).await?,
            }
            written += want as u64;
            if written == length {
                break;
            }
        }
    }
    Ok(())
}

type ChunkStream = Pin<Box<dyn Stream<Item = Result<GameFileChunk, Status>> + Send>>;

/// Streams a file's parts in pieces, hashing them; the last message waits for the hash to match.
async fn stream_file(kv: Arc<dyn Kv>, file: StoredFile, parts: Vec<(String, u64)>, sender: mpsc::Sender<Result<GameFileChunk, Status>>) {
    let mut hasher = Sha256::new();
    let mut held: Option<GameFileChunk> = None;
    let mut first = true;
    for (part, length) in parts {
        let mut offset = 0u64;
        while offset < length {
            let end = (offset + PIECE_BYTES as u64).min(length);
            let data = match kv.get_range(&part, offset, end - 1).await {
                Ok(data) if data.len() as u64 == end - offset => data,
                Ok(data) => {
                    warn!(part, offset, got = data.len(), "file changed while read");
                    let _ = sender.send(Err(Status::data_loss(format!("{}: changed while read", file.path)))).await;
                    return;
                }
                Err(error) => {
                    let _ = sender.send(Err(Status::unavailable(format!("{}: {error}", file.path)))).await;
                    return;
                }
            };
            hasher.update(&data);
            offset = end;
            let chunk = GameFileChunk { data, size: if first { file.size } else { 0 } };
            first = false;
            if let Some(previous) = held.replace(chunk) {
                if sender.send(Ok(previous)).await.is_err() {
                    return;
                }
            }
        }
    }
    let last = held.unwrap_or(GameFileChunk { data: Vec::new(), size: 0 });
    let sha256 = hex(&hasher.finalize());
    let message = match sha256.eq_ignore_ascii_case(&file.sha256) {
        true => Ok(last),
        false => {
            warn!(path = file.path, sha256, expected = file.sha256, "file in valkey does not match its manifest");
            Err(Status::data_loss(format!("{}: sha256 {sha256}, manifest says {}", file.path, file.sha256)))
        }
    };
    let _ = sender.send(message).await;
}

#[tonic::async_trait]
impl GameData for Cache {
    type GetFileStream = ChunkStream;

    async fn get_file(&self, request: Request<GetGameFileRequest>) -> Result<Response<ChunkStream>, Status> {
        let GetGameFileRequest { build, path } = request.into_inner();
        store::safe_path(&path).ok_or_else(|| Status::invalid_argument("path must be relative, without `..` or `#`"))?;
        if !self.builds.contains(&build) {
            return Err(Status::not_found(format!("unknown build {build:?}")));
        }
        let ready = self.ready.read().unwrap().get(&build).cloned();
        let ready = ready.ok_or_else(|| Status::unavailable(format!("build {build} is not loaded yet")))?;
        let kv = self.kv().ok_or_else(|| Status::unavailable("valkey not connected"))?;
        let file = ready.files.get(&path).cloned().ok_or_else(|| Status::not_found(format!("build {build} has no {path}")))?;
        let parts = ready.manifest.parts(&file);
        for (part, length) in &parts {
            let stored = kv.strlen(part).await.map_err(Status::unavailable)?;
            if stored != *length {
                warn!(build, path, part, stored, expected = length, "file in valkey has the wrong size");
                return Err(Status::data_loss(format!("{path}: {stored} bytes stored of {length}")));
            }
        }
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(stream_file(kv, file, parts, sender));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn list_builds(&self, _: Request<ListGameBuildsRequest>) -> Result<Response<ListGameBuildsResponse>, Status> {
        let ready = self.ready.read().unwrap();
        let builds = self
            .builds
            .iter()
            .map(|build| {
                let files = ready.get(build).map(|ready| ready.files.len() as u64);
                GameBuildStatus { build: build.clone(), ready: files.is_some(), file_count: files.unwrap_or(0) }
            })
            .collect();
        Ok(Response::new(ListGameBuildsResponse { builds }))
    }
}

//! Game data of the shard's build: the home location tables (`crate::location`), the hangar elevator tables
//! (`crate::transit`) and the collision of the hangars this DGS
//! loads (`collision/<socpak hash>.col`), by their paths in the build. Nothing of it is in the image. In a cluster it comes
//! from the region's `sc-gamedata`, which owns the files of each game build; for a local run, from a build directory that
//! `tools/gamedata-build` wrote (`<out>/<build>/`).
//!
//! `main` loads everything before it opens its port, retrying with a backoff while the files cannot be had: a shard only
//! opens once its build is ready in the region, so that is normally at once, and a DGS that has not loaded its hangars'
//! collision is not ready rather than letting players walk through walls.

use std::{path::PathBuf, sync::Arc, time::Duration};

use sc_api::internal::{game_data_client::GameDataClient, GetGameFileRequest};
use tokio::time::{sleep, timeout};
use tonic::{
    transport::{Channel, Endpoint},
    Code, Status,
};
use tracing::warn;

use crate::{
    location::{Tables, ENTITY_COUNTS_PATH, HANGAR_CONTENT_PATH, HOME_LOCATIONS_PATH},
    physics::World,
    transit,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A whole file, all its chunks. A hangar's collision is a few megabytes.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// More than the largest file the service may send; it says the size first.
const MAX_FILE_BYTES: u64 = 1 << 30;

/// How long a failed load waits before it is tried again: doubling from `first` up to `max`.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { first: Duration::from_secs(1), max: Duration::from_secs(30) }
    }
}

/// Where the files of the shard's game build are.
#[derive(Clone)]
pub enum GameData {
    /// `sc-gamedata`, for one game build.
    Service { client: GameDataClient<Channel>, build: String },
    /// A build directory of `tools/gamedata-build`, for a local run.
    Directory(PathBuf),
}

impl GameData {
    /// Connected on first use, so a DGS starts while the service is away.
    pub fn service(address: &str, build: &str) -> Result<Self, String> {
        let endpoint = Endpoint::from_shared(address.to_string()).map_err(|error| format!("gamedata address: {error}"))?;
        let channel = endpoint.connect_timeout(CONNECT_TIMEOUT).connect_lazy();
        Ok(Self::Service { client: GameDataClient::new(channel), build: build.to_string() })
    }

    pub fn directory(directory: impl Into<PathBuf>) -> Self {
        Self::Directory(directory.into())
    }

    /// A file of the build, whole.
    pub async fn file(&self, path: &str) -> Result<Vec<u8>, Status> {
        let (client, build) = match self {
            Self::Service { client, build } => (client, build),
            Self::Directory(directory) => {
                return tokio::fs::read(directory.join(path)).await.map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => Status::not_found(format!("{}: no {path}", directory.display())),
                    _ => Status::unavailable(format!("{}: {error}", directory.join(path).display())),
                });
            }
        };
        let fetch = async {
            let request = GetGameFileRequest { build: build.clone(), path: path.to_string() };
            let mut stream = client.clone().get_file(request).await?.into_inner();
            let mut data = Vec::new();
            let mut size = None;
            while let Some(chunk) = stream.message().await? {
                if size.is_none() {
                    if chunk.size > MAX_FILE_BYTES {
                        return Err(Status::out_of_range(format!("{path} is {} bytes", chunk.size)));
                    }
                    data.reserve_exact(chunk.size as usize);
                    size = Some(chunk.size);
                }
                data.extend_from_slice(&chunk.data);
            }
            match size {
                Some(size) if size == data.len() as u64 => Ok(data),
                size => Err(Status::data_loss(format!("{path}: {} bytes of {size:?}", data.len()))),
            }
        };
        timeout(FETCH_TIMEOUT, fetch).await.unwrap_or_else(|_| Err(Status::deadline_exceeded(format!("{path} not fetched in time"))))
    }

    /// The build and where it is, for the log.
    pub fn describe(&self) -> String {
        match self {
            Self::Service { build, .. } => format!("build {build} from sc-gamedata"),
            Self::Directory(directory) => format!("build directory {}", directory.display()),
        }
    }

    /// The three tables of the build. A table that is missing or does not parse is an error.
    pub async fn tables(&self) -> Result<Tables, String> {
        let mut texts = Vec::new();
        for path in [HOME_LOCATIONS_PATH, HANGAR_CONTENT_PATH, ENTITY_COUNTS_PATH] {
            let bytes = self.file(path).await.map_err(|status| describe_status(path, &status))?;
            texts.push(String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))?);
        }
        Tables::parse(&texts[0], &texts[1], &texts[2])
    }

    /// The three hangar elevator tables of the build. A table that is missing or does not parse is an error.
    pub async fn transit_tables(&self) -> Result<transit::Tables, String> {
        let mut texts = Vec::new();
        for path in [transit::MANAGERS_PATH, transit::GATEWAYS_PATH, transit::HANGARS_PATH] {
            let bytes = self.file(path).await.map_err(|status| describe_status(path, &status))?;
            texts.push(String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8"))?);
        }
        transit::Tables::parse(&texts[0], &texts[1], &texts[2])
    }

    /// A hangar's collision, built off the async runtime (a third of a second) and kept for `World::loaded`.
    pub async fn hangar_collision(&self, hash: u32) -> Result<Arc<World>, String> {
        let path = collision_path(hash);
        let bytes = self.file(&path).await.map_err(|status| describe_status(&path, &status))?;
        tokio::task::spawn_blocking(move || World::from_bytes(hash, &bytes))
            .await
            .map_err(|error| format!("{path}: {error}"))?
            .map_err(|error| format!("{path}: {error}"))
    }
}

fn describe_status(path: &str, status: &Status) -> String {
    match status.code() {
        Code::NotFound => format!("{path}: not in the build ({})", status.message()),
        code => format!("{path}: {code:?}, {}", status.message()),
    }
}

/// Runs `load` until it succeeds, waiting `backoff` between tries. Whatever fails is logged as `what`.
pub async fn retrying<T, F, Fut>(what: &str, backoff: Backoff, mut load: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut wait = backoff.first;
    loop {
        match load().await {
            Ok(value) => return value,
            Err(error) => warn!(what, %error, retry = ?wait, "game data not loaded"),
        }
        sleep(wait).await;
        wait = (wait * 2).min(backoff.max);
    }
}

/// The path of a hangar's collision in a build.
pub fn collision_path(hash: u32) -> String {
    format!("collision/{hash}.col")
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, pin::Pin};

    use sc_api::internal::{
        game_data_server::{GameData as GameDataService, GameDataServer},
        GameFileChunk, ListGameBuildsRequest, ListGameBuildsResponse,
    };
    use tokio::net::TcpListener;
    use tokio_stream::{wrappers::TcpListenerStream, Stream};
    use tonic::{transport::Server, Request, Response};

    use super::*;
    use crate::physics::world::{rest_stop_collision, REST_STOP};
    use crate::testdata;

    const BUILD: &str = "12913470";

    /// sc-gamedata as the DGS sees it: files by build and path, in chunks.
    struct Fake(HashMap<(String, String), Vec<u8>>);

    #[tonic::async_trait]
    impl GameDataService for Fake {
        type GetFileStream = Pin<Box<dyn Stream<Item = Result<GameFileChunk, Status>> + Send>>;

        async fn get_file(&self, request: Request<GetGameFileRequest>) -> Result<Response<Self::GetFileStream>, Status> {
            let request = request.into_inner();
            let data = self.0.get(&(request.build, request.path)).ok_or_else(|| Status::not_found("no such file"))?;
            let size = data.len() as u64;
            let chunks: Vec<_> = data
                .chunks(1 << 20)
                .enumerate()
                .map(|(index, chunk)| Ok(GameFileChunk { data: chunk.to_vec(), size: if index == 0 { size } else { 0 } }))
                .collect();
            Ok(Response::new(Box::pin(tokio_stream::iter(chunks))))
        }

        async fn list_builds(&self, _: Request<ListGameBuildsRequest>) -> Result<Response<ListGameBuildsResponse>, Status> {
            Ok(Response::new(ListGameBuildsResponse::default()))
        }
    }

    async fn serve(files: &[(&str, &[u8])]) -> String {
        let files = files.iter().map(|(path, data)| ((BUILD.to_string(), path.to_string()), data.to_vec())).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(Server::builder().add_service(GameDataServer::new(Fake(files))).serve_with_incoming(TcpListenerStream::new(listener)));
        address
    }

    const TRANSIT: [&str; 3] = [transit::MANAGERS_PATH, transit::GATEWAYS_PATH, transit::HANGARS_PATH];

    fn whole_build() -> Vec<(&'static str, &'static [u8])> {
        let tables = [HOME_LOCATIONS_PATH, HANGAR_CONTENT_PATH, ENTITY_COUNTS_PATH].into_iter().chain(TRANSIT);
        tables.map(|path| (path, testdata::bytes(path))).collect()
    }

    #[tokio::test]
    async fn the_tables_and_the_collision_come_from_gamedata() {
        let mut files = whole_build();
        let path = collision_path(REST_STOP);
        files.push((&path, rest_stop_collision()));
        let gamedata = GameData::service(&serve(&files).await, BUILD).unwrap();
        assert_eq!(gamedata.file(&path).await.unwrap(), rest_stop_collision());
        let tables = gamedata.tables().await.unwrap();
        assert_eq!(tables.locations(), 8);
        assert_eq!(gamedata.transit_tables().await.unwrap().hangars().len(), 32);
        let world = gamedata.hangar_collision(REST_STOP).await.unwrap();
        assert!(world.triangles() > 500_000);
        assert!(World::loaded(REST_STOP).is_some());
    }

    #[tokio::test]
    async fn what_is_missing_or_broken_is_an_error() {
        // A service that is not there.
        let away = GameData::service("http://127.0.0.1:1", BUILD).unwrap();
        assert_eq!(away.file(&collision_path(REST_STOP)).await.unwrap_err().code(), Code::Unavailable);
        assert!(away.tables().await.is_err());
        // A build without the collision, with a collision that does not decode, and with a broken table.
        let mut files = whole_build();
        files.push(("collision/7.col", b"not a collision file"));
        let lacking = GameData::service(&serve(&files).await, BUILD).unwrap();
        assert!(lacking.hangar_collision(REST_STOP).await.err().unwrap().contains("not in the build"));
        assert!(lacking.hangar_collision(7).await.is_err());
        let (content, counts) = (testdata::bytes(HANGAR_CONTENT_PATH), testdata::bytes(ENTITY_COUNTS_PATH));
        let broken = GameData::service(&serve(&[(HOME_LOCATIONS_PATH, b"1\tbroken"), (HANGAR_CONTENT_PATH, content), (ENTITY_COUNTS_PATH, counts)]).await, BUILD).unwrap();
        assert!(broken.tables().await.unwrap_err().contains("line 1"));
        let without = GameData::service(&serve(&[(HOME_LOCATIONS_PATH, testdata::bytes(HOME_LOCATIONS_PATH))]).await, BUILD).unwrap();
        assert!(without.tables().await.unwrap_err().contains(HANGAR_CONTENT_PATH));
        // A build of before the transit tables, and one whose hangars name a manager it lacks.
        assert!(without.transit_tables().await.unwrap_err().contains(transit::MANAGERS_PATH));
        let [_, gateways, hangars] = TRANSIT.map(testdata::bytes);
        let files = [(transit::MANAGERS_PATH, &b"\n"[..]), (transit::GATEWAYS_PATH, gateways), (transit::HANGARS_PATH, hangars)];
        let unmanaged = GameData::service(&serve(&files).await, BUILD).unwrap();
        assert!(unmanaged.transit_tables().await.unwrap_err().contains("names a manager"));
    }

    /// `testdata/` is laid out as a build directory of `tools/gamedata-build`, with one hangar's collision.
    #[tokio::test]
    async fn a_build_directory_serves_a_local_run() {
        // Read first, so that a missing `testdata/` stops with the pointer to the script that makes it.
        testdata::bytes(HOME_LOCATIONS_PATH);
        testdata::bytes(&collision_path(REST_STOP));
        let gamedata = GameData::directory(testdata::dir());
        assert_eq!(gamedata.tables().await.unwrap().locations(), 8);
        assert_eq!(gamedata.transit_tables().await.unwrap().managers().len(), 7);
        assert!(gamedata.hangar_collision(REST_STOP).await.unwrap().triangles() > 500_000);
        assert!(gamedata.hangar_collision(7).await.err().unwrap().contains("not in the build"));
    }

    #[tokio::test]
    async fn loading_waits_until_the_files_are_there() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let backoff = Backoff { first: Duration::from_millis(5), max: Duration::from_millis(10) };
        let value = retrying("test", backoff, || async {
            match attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0..=2 => Err("not yet".to_string()),
                n => Ok(n),
            }
        })
        .await;
        assert_eq!(value, 3);
    }

    /// Against a running sc-gamedata: `SC_GAMEDATA_ADDR=http://127.0.0.1:50051 SC_GAME_BUILD=12913470 cargo test -p sc-dgs
    /// gamedata -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a running sc-gamedata"]
    async fn a_running_service_has_the_tables_and_the_rest_stop_hangar() {
        let address = std::env::var("SC_GAMEDATA_ADDR").unwrap();
        let gamedata = GameData::service(&address, &std::env::var("SC_GAME_BUILD").unwrap()).unwrap();
        assert_eq!(gamedata.file(&collision_path(REST_STOP)).await.unwrap(), rest_stop_collision());
        assert_eq!(gamedata.file(HOME_LOCATIONS_PATH).await.unwrap(), testdata::bytes(HOME_LOCATIONS_PATH));
        assert!(gamedata.tables().await.unwrap().locations() >= 8);
    }
}

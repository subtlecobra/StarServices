//! The home locations a player can live at: the DGS's home locations table of the environment's game build, which
//! `tools/gamedata-build` writes into the build's game data and the region's `sc-gamedata` serves. This service is
//! global, so it asks the `sc-gamedata` of the region that takes the entity calls (`entityRegion` in StarServices-CN),
//! for the build the environment runs.
//!
//! Fetched at start, retried with a backoff until it is there, and fetched again every `REFRESH` so a build pushed again
//! is seen. Until the first fetch the list is empty: the site says so and refuses to set a home location.

use std::{
    sync::{Arc, LazyLock, RwLock},
    time::Duration,
};

use sc_proto::internal::{game_data_client::GameDataClient, GetGameFileRequest};
use tokio::time::{sleep, timeout};
use tonic::transport::Endpoint;
use tracing::{info, warn};

/// Where the table is in a game build.
pub const HOME_LOCATIONS_PATH: &str = "tables/home_locations.tsv";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// The table is a few kilobytes.
const MAX_BYTES: u64 = 16 << 20;
const REFRESH: Duration = Duration::from_secs(600);
const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);

pub struct Place {
    pub id: u32,
    pub name: String,
    pub system: String,
}

/// The rows of `home_locations.tsv`: id, name and system lead each line.
pub fn parse(table: &str) -> Vec<Place> {
    table
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let id = columns.next()?.trim().parse().ok()?;
            let name = columns.next()?.to_string();
            let system = columns.next()?.to_string();
            Some(Place { id, name, system })
        })
        .collect()
}

#[cfg(not(test))]
fn initial() -> Arc<Vec<Place>> {
    Arc::default()
}

/// Tests read the table that sc-dgs's tests use, which comes from the game: `scripts/game-files.sh` generates it.
#[cfg(test)]
fn initial() -> Arc<Vec<Place>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dgs/testdata").join(HOME_LOCATIONS_PATH);
    let table = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{}: {error}. The test data comes from the game and is not in the repository: generate it from your copy of \
             the game with scripts/game-files.sh (see README, \"Game files\")",
            path.display()
        )
    });
    Arc::new(parse(&table))
}

static PLACES: LazyLock<RwLock<Arc<Vec<Place>>>> = LazyLock::new(|| RwLock::new(initial()));

/// The home locations as last fetched, empty before the first fetch.
pub fn places() -> Arc<Vec<Place>> {
    PLACES.read().unwrap().clone()
}

fn set(places: Vec<Place>) {
    *PLACES.write().unwrap() = Arc::new(places);
}

/// The table of `build` from the `sc-gamedata` at `address`, as text.
async fn fetch(address: &str, build: &str) -> Result<String, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|error| format!("gamedata address: {error}"))?;
    let fetch = async {
        let channel = endpoint.connect_timeout(CONNECT_TIMEOUT).connect().await.map_err(|error| format!("{address}: {error}"))?;
        let request = GetGameFileRequest { build: build.to_string(), path: HOME_LOCATIONS_PATH.to_string() };
        let mut stream = GameDataClient::new(channel).get_file(request).await.map_err(|status| status.message().to_string())?.into_inner();
        let (mut data, mut size) = (Vec::new(), None);
        while let Some(chunk) = stream.message().await.map_err(|status| status.message().to_string())? {
            if size.is_none() {
                if chunk.size > MAX_BYTES {
                    return Err(format!("{HOME_LOCATIONS_PATH} is {} bytes", chunk.size));
                }
                size = Some(chunk.size);
            }
            data.extend_from_slice(&chunk.data);
        }
        if size != Some(data.len() as u64) {
            return Err(format!("{HOME_LOCATIONS_PATH}: {} bytes of {size:?}", data.len()));
        }
        String::from_utf8(data).map_err(|_| format!("{HOME_LOCATIONS_PATH} is not UTF-8"))
    };
    timeout(FETCH_TIMEOUT, fetch).await.unwrap_or_else(|_| Err(format!("{HOME_LOCATIONS_PATH} not fetched in time")))
}

/// Keeps the list up to date for good: fetched until it is there, then every `REFRESH`. A failed refresh keeps the list.
pub async fn keep(address: String, build: String) {
    let mut wait = FIRST_RETRY;
    loop {
        match fetch(&address, &build).await.map(|table| parse(&table)) {
            Ok(places) if !places.is_empty() => {
                if places.len() != self::places().len() {
                    info!(build, places = places.len(), "home locations loaded from gamedata");
                }
                set(places);
                wait = FIRST_RETRY;
                sleep(REFRESH).await;
            }
            result => {
                let error = result.err().unwrap_or_else(|| "the table has no rows".into());
                warn!(%address, build, %error, retry = ?wait, "home locations not loaded from gamedata");
                sleep(wait).await;
                wait = (wait * 2).min(MAX_RETRY);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rows_lead_with_id_name_and_system() {
        let places = parse("848868938\tRR_P3_LEO\tPyro\tHangar\n\nbad\n1\tA\n2\tB\tStanton\n");
        let rows: Vec<(u32, &str, &str)> = places.iter().map(|place| (place.id, place.name.as_str(), place.system.as_str())).collect();
        assert_eq!(rows, [(848_868_938, "RR_P3_LEO", "Pyro"), (2, "B", "Stanton")]);
        assert!(super::places().len() >= 8, "tests start with the fixture");
    }

    #[tokio::test]
    async fn a_missing_service_is_an_error() {
        assert!(fetch("http://127.0.0.1:1", "12913470").await.is_err());
    }
}

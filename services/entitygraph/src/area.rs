//! The area of a DGS, served over `EntityGraphInternal.LoadArea`: its roots and everything under them. A container
//! belongs to the nearest root at or above it, across every DGS of the system, so a root takes everything under it
//! except what a deeper root claims. See docs/DGS_ZONES.md.

use std::collections::{HashSet, VecDeque};

use futures_util::{stream, Stream};
use sc_db::{internal, Arango};
use sc_api::internal::{AreaContainer, LoadAreaRequest, LoadAreaResponse};
use serde_json::{json, Value};
use tonic::Status;

/// About 60 KB a message. A Pyro DGS has some 36,000 containers.
pub const CONTAINERS_PER_MESSAGE: usize = 4096;

const FIELDS: &str = "{ key: c._key, name: c.name, startId: c.identifiers.startId,
    immovable: c.identifiers.streamingImmovableCount, movable: c.identifiers.streamingMovableCount }";

/// A prefix only ever names a test copy of the collections.
fn valid_prefix(prefix: &str) -> bool {
    prefix.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn name(container: &Value) -> &str {
    container["name"].as_str().unwrap_or_default()
}

/// The roots no container answered to, which means a stale template or a tree seeded without names.
fn missing_roots<'a>(roots: &'a [String], found: &[Value]) -> Vec<&'a str> {
    let found: HashSet<&str> = found.iter().map(name).collect();
    roots.iter().map(String::as_str).filter(|root| !found.contains(root)).collect()
}

fn area_container(container: &Value) -> Result<AreaContainer, String> {
    let count = |field: &str| container[field].as_u64().unwrap_or(0);
    let start = container["startId"].as_str().and_then(|id| id.parse().ok());
    let start_id = start.ok_or_else(|| format!("container with no start id: {container}"))?;
    Ok(AreaContainer { start_id, entities: count("immovable") + count("movable") })
}

/// Goes down from the roots one level at a time, not entering any container named in `stop`, and hands out the
/// containers in messages of [`CONTAINERS_PER_MESSAGE`].
struct Walk {
    level: Vec<Value>,
    stop: HashSet<String>,
    pending: VecDeque<LoadAreaResponse>,
}

impl Walk {
    fn new(starts: Vec<Value>, stop: HashSet<String>) -> Self {
        Self { level: starts, stop, pending: VecDeque::new() }
    }

    /// None once everything is sent. An error ends the walk.
    async fn next(
        &mut self,
        children: impl AsyncFnOnce(Vec<String>) -> Result<Vec<Value>, String>,
    ) -> Option<Result<LoadAreaResponse, String>> {
        if let Some(message) = self.pending.pop_front() {
            return Some(Ok(message));
        }
        if self.level.is_empty() {
            return None;
        }
        let level = std::mem::take(&mut self.level);
        let containers: Result<Vec<AreaContainer>, String> = level.iter().map(area_container).collect();
        let containers = match containers {
            Ok(containers) => containers,
            Err(error) => return Some(Err(error)),
        };
        let keys = level.iter().map(|container| container["key"].as_str().unwrap_or_default().to_string()).collect();
        match children(keys).await {
            Ok(mut next) => {
                next.retain(|container| !self.stop.contains(name(container)));
                self.level = next;
            }
            Err(error) => return Some(Err(error)),
        }
        let messages = containers.chunks(CONTAINERS_PER_MESSAGE);
        self.pending.extend(messages.map(|chunk| LoadAreaResponse { containers: chunk.to_vec() }));
        // A level is never empty here, so there is a message.
        self.pending.pop_front().map(Ok)
    }
}

/// The containers under these, by two plain queries, as a graph traversal crosses shards at every step: see
/// docs/DATA.md.
async fn children(db: &Arango, containers: &str, edges: &str, keys: Vec<String>) -> Result<Vec<Value>, String> {
    let ids: Vec<String> = keys.iter().map(|key| format!("{containers}/{key}")).collect();
    let found = db.aql("FOR e IN @@edges FILTER e._to IN @ids RETURN e.startId", json!({ "@edges": edges, "ids": ids })).await?;
    let level = format!("FOR c IN @@containers FILTER c._key IN @keys RETURN {FIELDS}");
    db.aql(&level, json!({ "@containers": containers, "keys": found })).await
}

pub type AreaStream = std::pin::Pin<Box<dyn Stream<Item = Result<LoadAreaResponse, Status>> + Send>>;

/// Finds the roots before answering, so a root the tree lacks is NOT_FOUND and not a broken stream.
pub async fn load(db: &Arango, request: LoadAreaRequest) -> Result<AreaStream, Status> {
    if !valid_prefix(&request.collection_prefix) {
        return Err(Status::invalid_argument("bad collection prefix"));
    }
    if request.roots.iter().all(String::is_empty) {
        return Err(Status::invalid_argument("no roots"));
    }
    let prefix = &request.collection_prefix;
    let (containers, edges) = (format!("{prefix}containers"), format!("{prefix}container_edges"));
    let query = format!("FOR c IN @@containers FILTER c.system == @system AND c.name IN @roots RETURN {FIELDS}");
    let bind = json!({ "@containers": containers, "system": request.system, "roots": request.roots });
    let starts = db.aql(&query, bind).await.map_err(internal)?;
    let missing = missing_roots(&request.roots, &starts);
    if !missing.is_empty() {
        return Err(Status::not_found(format!("no container in {} named {}", request.system, missing.join(", "))));
    }
    let stop = request.roots.iter().chain(&request.excluded_roots).filter(|root| !root.is_empty()).cloned().collect();
    let state = (Walk::new(starts, stop), db.clone(), containers, edges);
    let stream = stream::unfold(Some(state), |state| async move {
        let (mut walk, db, containers, edges) = state?;
        match walk.next(async |keys| children(&db, &containers, &edges, keys).await).await? {
            Ok(message) => Some((Ok(message), Some((walk, db, containers, edges)))),
            Err(error) => Some((Err(internal(error)), None)),
        }
    });
    Ok(Box::pin(stream))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn container(key: &str, name: &str, start: u64, immovable: u32, movable: u32) -> Value {
        json!({ "key": key, "name": name, "startId": start.to_string(), "immovable": immovable, "movable": movable })
    }

    /// stanton1 holds a moon and an unnamed group, and the moon is another DGS's root.
    fn tree() -> HashMap<String, Vec<Value>> {
        HashMap::from([
            ("1".to_string(), vec![container("2", "stanton1a", 100, 50, 0), container("3", "", 0, 0, 0)]),
            ("3".to_string(), vec![container("4", "outpost", 200, 3, 2)]),
            ("2".to_string(), vec![container("5", "mine", 300, 7, 0)]),
        ])
    }

    async fn walked(starts: Vec<Value>, stop: &[&str]) -> Vec<AreaContainer> {
        let tree = tree();
        let mut walk = Walk::new(starts, stop.iter().map(|s| s.to_string()).collect());
        let mut found = Vec::new();
        let children = |keys: Vec<String>| keys.iter().filter_map(|key| tree.get(key)).flatten().cloned().collect();
        while let Some(message) = walk.next(async |keys| Ok(children(keys))).await {
            found.extend(message.unwrap().containers);
        }
        found
    }

    fn totals(containers: &[AreaContainer]) -> (usize, u64) {
        (containers.len(), containers.iter().map(|c| c.entities).sum())
    }

    #[tokio::test]
    async fn a_root_takes_everything_under_it() {
        let found = walked(vec![container("1", "stanton1", 10, 4, 1)], &["stanton1"]).await;
        assert_eq!(totals(&found), (5, 5 + 50 + 5 + 7));
        let starts: Vec<u64> = found.iter().map(|c| c.start_id).collect();
        assert_eq!(starts, [10, 100, 0, 300, 200], "parents come before their children");
    }

    #[tokio::test]
    async fn a_deeper_root_of_another_dgs_is_left_out() {
        let found = walked(vec![container("1", "stanton1", 10, 4, 1)], &["stanton1", "stanton1a"]).await;
        assert_eq!(totals(&found), (3, 5 + 5));
    }

    #[tokio::test]
    async fn a_root_inside_a_root_of_the_same_dgs_counts_once() {
        let starts = vec![container("1", "stanton1", 10, 4, 1), container("2", "stanton1a", 100, 50, 0)];
        let found = walked(starts, &["stanton1", "stanton1a"]).await;
        assert_eq!(totals(&found), (5, 5 + 50 + 5 + 7));
    }

    #[tokio::test]
    async fn a_large_level_is_split_into_messages() {
        let starts: Vec<Value> =
            (0..CONTAINERS_PER_MESSAGE as u64 * 2 + 1).map(|n| container(&n.to_string(), "x", n, 1, 0)).collect();
        let mut walk = Walk::new(starts, HashSet::new());
        let mut sizes = Vec::new();
        while let Some(message) = walk.next(async |_| Ok(Vec::new())).await {
            sizes.push(message.unwrap().containers.len());
        }
        assert_eq!(sizes, [CONTAINERS_PER_MESSAGE, CONTAINERS_PER_MESSAGE, 1]);
    }

    #[tokio::test]
    async fn a_failed_level_ends_the_walk() {
        let mut walk = Walk::new(vec![container("1", "stanton1", 10, 4, 1)], HashSet::new());
        assert!(walk.next(async |_| Err("down".to_string())).await.unwrap().is_err());
        assert!(walk.next(async |_| Ok(Vec::new())).await.is_none());
    }

    #[tokio::test]
    async fn a_container_with_no_start_id_is_an_error() {
        let mut walk = Walk::new(vec![json!({ "key": "1", "name": "stanton1" })], HashSet::new());
        assert!(walk.next(async |_| Ok(Vec::new())).await.unwrap().is_err());
    }

    #[test]
    fn a_container_with_no_entities_still_counts_as_a_container() {
        assert_eq!(area_container(&container("1", "", 0, 0, 0)).unwrap(), AreaContainer { start_id: 0, entities: 0 });
    }

    #[test]
    fn unknown_roots_are_named() {
        let roots = ["stanton1".to_string(), "stanton9".to_string()];
        assert_eq!(missing_roots(&roots, &[container("1", "stanton1", 0, 0, 0)]), ["stanton9"]);
    }

    #[test]
    fn a_prefix_cannot_name_another_collection() {
        assert!(valid_prefix("") && valid_prefix("test_1_"));
        assert!(!valid_prefix("x/") && !valid_prefix("a b") && !valid_prefix("`"));
    }
}

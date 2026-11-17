//! The key-value store under the cache: the region's Valkey cluster in service, a map in memory in the tests. Only what
//! the cache needs, every command on one key, so each goes to the node that holds its hash slot.

#[cfg(test)]
use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Instant,
};
use std::time::Duration;

use redis::{aio::ConnectionLike, cluster::ClusterClient, AsyncCommands, Client, SetExpiry, SetOptions};

/// Valkey refuses a string larger than this.
pub const MAX_VALUE_BYTES: u64 = 512 * 1024 * 1024;

/// Extends a lock's expiry if it is still the owner's.
const RENEW: &str = "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('pexpire', KEYS[1], ARGV[2]) else return 0 end";
/// Deletes a lock if it is still the owner's.
const RELEASE: &str = "if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end";

#[tonic::async_trait]
pub trait Kv: Send + Sync + 'static {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    /// The length of a string, 0 when there is none.
    async fn strlen(&self, key: &str) -> Result<u64, String>;
    /// Bytes `start..=end` of a string, fewer at its end.
    async fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, String>;
    async fn set(&self, key: &str, value: &[u8]) -> Result<(), String>;
    async fn append(&self, key: &str, value: &[u8]) -> Result<(), String>;
    /// SET with an expiry, in milliseconds.
    async fn set_px(&self, key: &str, value: &[u8], ms: u64) -> Result<(), String>;
    /// `SET key owner NX PX ms`: true when the lock is now the owner's.
    async fn acquire(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String>;
    /// Extends the lock if the owner still holds it. False when it lost it.
    async fn renew(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String>;
    async fn release(&self, key: &str, owner: &str) -> Result<(), String>;
    async fn del(&self, key: &str) -> Result<(), String>;
    async fn sadd(&self, set: &str, member: &str) -> Result<(), String>;
    async fn srem(&self, set: &str, member: &str) -> Result<(), String>;
    async fn smembers(&self, set: &str) -> Result<Vec<String>, String>;
    async fn ping(&self) -> Result<(), String>;
}

/// Valkey through a redis-rs connection: a cluster connection, which sends each command to the node of its key's slot
/// and follows the cluster as it changes, or a single server's connection manager.
pub struct Valkey<C> {
    connection: C,
}

/// Connects to the cluster that `nodes` (`redis://host:port`) belong to. Any one reachable node is enough.
pub async fn cluster(nodes: &[String]) -> Result<Valkey<redis::cluster_async::ClusterConnection>, String> {
    let client = ClusterClient::builder(nodes.to_vec())
        .connection_timeout(Duration::from_secs(5))
        .response_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("valkey: {error}"))?;
    let connection = client.get_async_connection().await.map_err(|error| format!("valkey: {error}"))?;
    Ok(Valkey { connection })
}

/// One server without cluster mode, for a small setup.
pub async fn single(node: &str) -> Result<Valkey<redis::aio::ConnectionManager>, String> {
    let client = Client::open(node).map_err(|error| format!("valkey: {error}"))?;
    let connection = client.get_connection_manager().await.map_err(|error| format!("valkey: {error}"))?;
    Ok(Valkey { connection })
}

fn failed(error: redis::RedisError) -> String {
    format!("valkey: {error}")
}

#[tonic::async_trait]
impl<C: ConnectionLike + Clone + Send + Sync + 'static> Kv for Valkey<C> {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.connection.clone().get(key).await.map_err(failed)
    }

    async fn strlen(&self, key: &str) -> Result<u64, String> {
        self.connection.clone().strlen(key).await.map_err(failed)
    }

    async fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
        let (start, end) = (start as isize, end as isize);
        self.connection.clone().getrange(key, start, end).await.map_err(failed)
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.connection.clone().set(key, value).await.map_err(failed)
    }

    async fn append(&self, key: &str, value: &[u8]) -> Result<(), String> {
        let _: u64 = self.connection.clone().append(key, value).await.map_err(failed)?;
        Ok(())
    }

    async fn set_px(&self, key: &str, value: &[u8], ms: u64) -> Result<(), String> {
        let options = SetOptions::default().with_expiration(SetExpiry::PX(ms));
        self.connection.clone().set_options(key, value, options).await.map_err(failed)
    }

    async fn acquire(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String> {
        let options = SetOptions::default().conditional_set(redis::ExistenceCheck::NX).with_expiration(SetExpiry::PX(ms));
        let reply: Option<String> = self.connection.clone().set_options(key, owner, options).await.map_err(failed)?;
        Ok(reply.is_some())
    }

    async fn renew(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String> {
        let reply: i64 = redis::cmd("EVAL")
            .arg(RENEW)
            .arg(1)
            .arg(key)
            .arg(owner)
            .arg(ms)
            .query_async(&mut self.connection.clone())
            .await
            .map_err(failed)?;
        Ok(reply == 1)
    }

    async fn release(&self, key: &str, owner: &str) -> Result<(), String> {
        let _: i64 = redis::cmd("EVAL")
            .arg(RELEASE)
            .arg(1)
            .arg(key)
            .arg(owner)
            .query_async(&mut self.connection.clone())
            .await
            .map_err(failed)?;
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), String> {
        let _: u64 = self.connection.clone().del(key).await.map_err(failed)?;
        Ok(())
    }

    async fn sadd(&self, set: &str, member: &str) -> Result<(), String> {
        let _: u64 = self.connection.clone().sadd(set, member).await.map_err(failed)?;
        Ok(())
    }

    async fn srem(&self, set: &str, member: &str) -> Result<(), String> {
        let _: u64 = self.connection.clone().srem(set, member).await.map_err(failed)?;
        Ok(())
    }

    async fn smembers(&self, set: &str) -> Result<Vec<String>, String> {
        self.connection.clone().smembers(set).await.map_err(failed)
    }

    async fn ping(&self) -> Result<(), String> {
        let _: String = redis::cmd("PING").query_async(&mut self.connection.clone()).await.map_err(failed)?;
        Ok(())
    }
}

/// The store in memory, for the tests: strings with their expiry, and sets. `down` makes every call fail, as an
/// unreachable Valkey does.
/// A value and when it expires.
#[cfg(test)]
type Stored = (Vec<u8>, Option<Instant>);

#[cfg(test)]
#[derive(Default)]
pub struct Memory {
    strings: Mutex<HashMap<String, Stored>>,
    sets: Mutex<HashMap<String, BTreeSet<String>>>,
    pub down: AtomicBool,
}

#[cfg(test)]
impl Memory {
    fn check(&self) -> Result<(), String> {
        match self.down.load(Ordering::SeqCst) {
            true => Err("valkey: connection refused".into()),
            false => Ok(()),
        }
    }

    fn live(&self, key: &str) -> Option<Vec<u8>> {
        let mut strings = self.strings.lock().unwrap();
        match strings.get(key) {
            Some((_, Some(expiry))) if *expiry <= Instant::now() => {
                strings.remove(key);
                None
            }
            Some((value, _)) => Some(value.clone()),
            None => None,
        }
    }

    /// Every live key, for the tests to look at.
    pub fn keys(&self) -> Vec<String> {
        let names: Vec<String> = self.strings.lock().unwrap().keys().cloned().collect();
        let mut live: Vec<String> = names.into_iter().filter(|key| self.live(key).is_some()).collect();
        live.sort();
        live
    }

    /// Writes a value as it is, for a test to corrupt one.
    pub fn put(&self, key: &str, value: &[u8]) {
        self.strings.lock().unwrap().insert(key.to_string(), (value.to_vec(), None));
    }
}

#[cfg(test)]
#[tonic::async_trait]
impl Kv for Memory {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.check()?;
        Ok(self.live(key))
    }

    async fn strlen(&self, key: &str) -> Result<u64, String> {
        self.check()?;
        Ok(self.live(key).map_or(0, |value| value.len() as u64))
    }

    async fn get_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
        self.check()?;
        let value = self.live(key).unwrap_or_default();
        let start = (start as usize).min(value.len());
        let end = (end as usize).saturating_add(1).min(value.len()).max(start);
        Ok(value[start..end].to_vec())
    }

    async fn set(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.check()?;
        self.put(key, value);
        Ok(())
    }

    async fn append(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.check()?;
        let mut current = self.live(key).unwrap_or_default();
        current.extend_from_slice(value);
        self.put(key, &current);
        Ok(())
    }

    async fn set_px(&self, key: &str, value: &[u8], ms: u64) -> Result<(), String> {
        self.check()?;
        let expiry = Instant::now() + Duration::from_millis(ms);
        self.strings.lock().unwrap().insert(key.to_string(), (value.to_vec(), Some(expiry)));
        Ok(())
    }

    async fn acquire(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String> {
        self.check()?;
        if self.live(key).is_some() {
            return Ok(false);
        }
        self.set_px(key, owner.as_bytes(), ms).await?;
        Ok(true)
    }

    async fn renew(&self, key: &str, owner: &str, ms: u64) -> Result<bool, String> {
        self.check()?;
        if self.live(key).as_deref() != Some(owner.as_bytes()) {
            return Ok(false);
        }
        self.set_px(key, owner.as_bytes(), ms).await?;
        Ok(true)
    }

    async fn release(&self, key: &str, owner: &str) -> Result<(), String> {
        self.check()?;
        if self.live(key).as_deref() == Some(owner.as_bytes()) {
            self.strings.lock().unwrap().remove(key);
        }
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), String> {
        self.check()?;
        self.strings.lock().unwrap().remove(key);
        Ok(())
    }

    async fn sadd(&self, set: &str, member: &str) -> Result<(), String> {
        self.check()?;
        self.sets.lock().unwrap().entry(set.to_string()).or_default().insert(member.to_string());
        Ok(())
    }

    async fn srem(&self, set: &str, member: &str) -> Result<(), String> {
        self.check()?;
        if let Some(members) = self.sets.lock().unwrap().get_mut(set) {
            members.remove(member);
        }
        Ok(())
    }

    async fn smembers(&self, set: &str) -> Result<Vec<String>, String> {
        self.check()?;
        Ok(self.sets.lock().unwrap().get(set).map(|members| members.iter().cloned().collect()).unwrap_or_default())
    }

    async fn ping(&self) -> Result<(), String> {
        self.check()
    }
}

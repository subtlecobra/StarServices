//! YugabyteDB, through its PostgreSQL protocol.

use std::{str::FromStr, time::Duration};

use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod};
use tokio_postgres::{types::ToSql, Config, NoTls};
use tracing::{info, warn};

pub use tokio_postgres::Row;

const POOL_SIZE: usize = 8;
const CONNECT_ATTEMPTS: u32 = 60;
const CONNECT_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Postgres {
    pool: Pool,
}

impl Postgres {
    /// Connects, waiting for the database while the cluster starts, then applies `schema`. Each
    /// service owns its tables, and `schema` must be safe to run again (`IF NOT EXISTS`).
    pub async fn connect(url: &str, schema: &str) -> Result<Self, String> {
        let config = Config::from_str(url).map_err(|e| format!("postgres url: {e}"))?;
        let manager = Manager::from_config(config, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(manager).max_size(POOL_SIZE).build().map_err(|e| format!("postgres pool: {e}"))?;
        let db = Self { pool };
        for attempt in 1..=CONNECT_ATTEMPTS {
            match db.apply(schema).await {
                Ok(()) => {
                    info!("postgres ready");
                    return Ok(db);
                }
                Err(error) if attempt < CONNECT_ATTEMPTS => {
                    warn!(%error, attempt, "postgres not ready");
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
                Err(error) => return Err(format!("postgres: {error}")),
            }
        }
        unreachable!("the last attempt returns")
    }

    async fn apply(&self, schema: &str) -> Result<(), String> {
        let client = self.client().await?;
        client.batch_execute(schema).await.map_err(|e| e.to_string())
    }

    /// A pooled connection, for transactions.
    pub async fn client(&self) -> Result<Object, String> {
        self.pool.get().await.map_err(|e| e.to_string())
    }

    pub async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, String> {
        self.client().await?.execute(sql, params).await.map_err(|e| e.to_string())
    }

    pub async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, String> {
        self.client().await?.query(sql, params).await.map_err(|e| e.to_string())
    }

    pub async fn query_opt(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Option<Row>, String> {
        self.client().await?.query_opt(sql, params).await.map_err(|e| e.to_string())
    }
}

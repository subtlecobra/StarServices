//! `admin_audit`, the only table this service owns: who did what to whom, when, and what it was before and after.
//!
//! An action is written before its owner is asked, as `pending`, and finished with the outcome. An action whose row
//! cannot be written is not done, so nothing changes unrecorded. A form submitted twice carries the same request id,
//! so it is one row, and the owners, keyed by the same id, apply it once.

use std::sync::Mutex;

use sc_db::Postgres;
use serde_json::Value;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS admin_audit (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    request_id TEXT NOT NULL UNIQUE,
    actor_account_id BIGINT NOT NULL,
    actor_handle TEXT NOT NULL,
    action TEXT NOT NULL,
    owner TEXT NOT NULL,
    target_account_id BIGINT,
    target_geid BIGINT,
    detail TEXT NOT NULL,
    before TEXT,
    after TEXT,
    outcome TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS admin_audit_by_target ON admin_audit (target_account_id, created_at);
CREATE INDEX IF NOT EXISTS admin_audit_by_time ON admin_audit (created_at);";

pub const PENDING: &str = "pending";
pub const OK: &str = "ok";

/// An action as it starts.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub request_id: String,
    pub actor_account_id: u32,
    pub actor_handle: String,
    /// Such as `money.adjust` or `location.home`.
    pub action: String,
    /// The service asked, such as `sc-ledger`.
    pub owner: String,
    pub target_account_id: Option<u32>,
    pub target_geid: Option<u64>,
    /// What was asked, as JSON.
    pub detail: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    pub entry: Entry,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub outcome: String,
    pub created_at_ms: i64,
}

#[tonic::async_trait]
pub trait AuditLog: Send + Sync {
    /// Records the start of an action. A repeated request id keeps the first row.
    async fn begin(&self, entry: &Entry) -> Result<(), String>;
    async fn finish(&self, request_id: &str, outcome: &str, before: Option<&Value>, after: Option<&Value>) -> Result<(), String>;
    /// Newest first, for one account or for all.
    async fn list(&self, target_account_id: Option<u32>, limit: u32) -> Result<Vec<Record>, String>;
}

pub struct PostgresAudit {
    db: Postgres,
}

impl PostgresAudit {
    pub async fn connect(url: &str) -> Result<Self, String> {
        Ok(Self { db: Postgres::connect(url, SCHEMA).await? })
    }
}

fn json_text(value: Option<&Value>) -> Option<String> {
    value.map(Value::to_string)
}

fn parse(text: Option<String>) -> Option<Value> {
    text.and_then(|text| serde_json::from_str(&text).ok())
}

#[tonic::async_trait]
impl AuditLog for PostgresAudit {
    async fn begin(&self, entry: &Entry) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO admin_audit (request_id, actor_account_id, actor_handle, action, owner, target_account_id,
                                          target_geid, detail, outcome)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (request_id) DO NOTHING",
                &[
                    &entry.request_id,
                    &i64::from(entry.actor_account_id),
                    &entry.actor_handle,
                    &entry.action,
                    &entry.owner,
                    &entry.target_account_id.map(i64::from),
                    &entry.target_geid.map(|geid| geid as i64),
                    &entry.detail.to_string(),
                    &PENDING,
                ],
            )
            .await
            .map(|_| ())
    }

    async fn finish(&self, request_id: &str, outcome: &str, before: Option<&Value>, after: Option<&Value>) -> Result<(), String> {
        self.db
            .execute(
                "UPDATE admin_audit SET outcome = $2, before = COALESCE($3, before), after = COALESCE($4, after),
                     finished_at = now()
                 WHERE request_id = $1",
                &[&request_id, &outcome, &json_text(before), &json_text(after)],
            )
            .await
            .map(|_| ())
    }

    async fn list(&self, target_account_id: Option<u32>, limit: u32) -> Result<Vec<Record>, String> {
        let rows = self
            .db
            .query(
                "SELECT request_id, actor_account_id, actor_handle, action, owner, target_account_id, target_geid,
                        detail, before, after, outcome, (extract(epoch FROM created_at) * 1000)::bigint AS created_ms
                 FROM admin_audit WHERE ($1::bigint IS NULL OR target_account_id = $1)
                 ORDER BY created_at DESC LIMIT $2",
                &[&target_account_id.map(i64::from), &i64::from(limit)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| Record {
                entry: Entry {
                    request_id: row.get("request_id"),
                    actor_account_id: row.get::<_, i64>("actor_account_id") as u32,
                    actor_handle: row.get("actor_handle"),
                    action: row.get("action"),
                    owner: row.get("owner"),
                    target_account_id: row.get::<_, Option<i64>>("target_account_id").map(|id| id as u32),
                    target_geid: row.get::<_, Option<i64>>("target_geid").map(|geid| geid as u64),
                    detail: parse(Some(row.get("detail"))).unwrap_or(Value::Null),
                },
                before: parse(row.get("before")),
                after: parse(row.get("after")),
                outcome: row.get("outcome"),
                created_at_ms: row.get("created_ms"),
            })
            .collect())
    }
}

/// The audit in memory, for tests.
#[derive(Default)]
pub struct MemoryAudit {
    pub records: Mutex<Vec<Record>>,
    /// Refuses every write, as a database that is away.
    pub broken: bool,
}

#[tonic::async_trait]
impl AuditLog for MemoryAudit {
    async fn begin(&self, entry: &Entry) -> Result<(), String> {
        if self.broken {
            return Err("audit unavailable".into());
        }
        let mut records = self.records.lock().unwrap();
        if !records.iter().any(|record| record.entry.request_id == entry.request_id) {
            records.push(Record { entry: entry.clone(), outcome: PENDING.into(), ..Default::default() });
        }
        Ok(())
    }

    async fn finish(&self, request_id: &str, outcome: &str, before: Option<&Value>, after: Option<&Value>) -> Result<(), String> {
        let mut records = self.records.lock().unwrap();
        let record = records.iter_mut().find(|record| record.entry.request_id == request_id).ok_or("no such action")?;
        record.outcome = outcome.to_string();
        if before.is_some() {
            record.before = before.cloned();
        }
        if after.is_some() {
            record.after = after.cloned();
        }
        Ok(())
    }

    async fn list(&self, target_account_id: Option<u32>, limit: u32) -> Result<Vec<Record>, String> {
        let records = self.records.lock().unwrap();
        Ok(records
            .iter()
            .rev()
            .filter(|record| target_account_id.is_none() || record.entry.target_account_id == target_account_id)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-admin-web -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn an_action_is_recorded_once_with_its_outcome() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let audit = PostgresAudit::connect(&url).await.unwrap();
        let target = 2_000_000_000 + std::process::id();
        let entry = Entry {
            request_id: format!("test-{target}"),
            actor_account_id: 1,
            actor_handle: "root".into(),
            action: "money.adjust".into(),
            owner: "sc-ledger".into(),
            target_account_id: Some(target),
            target_geid: Some(u64::from(target)),
            detail: serde_json::json!({ "delta": 5 }),
        };
        audit.begin(&entry).await.unwrap();
        audit.begin(&entry).await.unwrap();
        let before = serde_json::json!({ "balance": 1 });
        let after = serde_json::json!({ "balance": 6 });
        audit.finish(&entry.request_id, OK, Some(&before), Some(&after)).await.unwrap();
        let found = audit.list(Some(target), 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].outcome.as_str(), found[0].after.clone()), (OK, Some(after)));
        assert_eq!(found[0].entry.detail["delta"], 5);
        audit.db.execute("DELETE FROM admin_audit WHERE target_account_id = $1", &[&i64::from(target)]).await.unwrap();
    }
}

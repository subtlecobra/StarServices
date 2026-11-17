//! `admin_api_tokens`: the access tokens of the developer API (`/api/v1`), which stand in for a login there.
//!
//! A token belongs to the admin who made it and acts as that admin: the API asks sc-identity for the account's roles on
//! every request, so taking the admin role away, or disabling the account, stops its tokens at once. Only the SHA-256 of
//! a token is kept; the token itself is shown once, when it is made.

use std::sync::Mutex;

use ring::digest::{digest, SHA256};
use sc_db::Postgres;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS admin_api_tokens (
    token_id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    account_id BIGINT NOT NULL,
    name TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    last_used_at_ms BIGINT,
    revoked_at_ms BIGINT
);
CREATE INDEX IF NOT EXISTS admin_api_tokens_by_account ON admin_api_tokens (account_id);";

/// What every token starts with, so a leaked one can be recognized.
pub const PREFIX: &str = "scat_";
/// The most live tokens an account may hold.
pub const MAX_LIVE: usize = 20;
pub const MAX_NAME: usize = 60;

/// A token as listed: never the secret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenInfo {
    pub token_id: String,
    pub account_id: u32,
    pub name: String,
    pub created_at_ms: i64,
    pub last_used_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
}

#[tonic::async_trait]
pub trait Tokens: Send + Sync {
    /// Stores a token made by the caller (`token_id`, the secret's hash).
    async fn create(&self, token_id: &str, token_hash: &str, account_id: u32, name: &str, now_ms: i64) -> Result<(), String>;
    /// Newest first, revoked ones included.
    async fn list(&self, account_id: u32) -> Result<Vec<TokenInfo>, String>;
    /// Revokes one of the account's tokens: whether there was a live one.
    async fn revoke(&self, account_id: u32, token_id: &str, now_ms: i64) -> Result<bool, String>;
    /// The account of a live token, noting its use.
    async fn verify(&self, token_hash: &str, now_ms: i64) -> Result<Option<u32>, String>;
}

pub fn hash(token: &str) -> String {
    digest(&SHA256, token.as_bytes()).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `text` could be a token, before anything is looked up.
pub fn well_formed(text: &str) -> bool {
    text.strip_prefix(PREFIX).is_some_and(|rest| rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub struct PostgresTokens {
    db: Postgres,
}

impl PostgresTokens {
    pub async fn connect(url: &str) -> Result<Self, String> {
        Ok(Self { db: Postgres::connect(url, SCHEMA).await? })
    }
}

#[tonic::async_trait]
impl Tokens for PostgresTokens {
    async fn create(&self, token_id: &str, token_hash: &str, account_id: u32, name: &str, now_ms: i64) -> Result<(), String> {
        self.db
            .execute(
                "INSERT INTO admin_api_tokens (token_id, token_hash, account_id, name, created_at_ms) VALUES ($1, $2, $3, $4, $5)",
                &[&token_id, &token_hash, &i64::from(account_id), &name, &now_ms],
            )
            .await
            .map(|_| ())
    }

    async fn list(&self, account_id: u32) -> Result<Vec<TokenInfo>, String> {
        let rows = self
            .db
            .query(
                "SELECT token_id, account_id, name, created_at_ms, last_used_at_ms, revoked_at_ms
                 FROM admin_api_tokens WHERE account_id = $1 ORDER BY created_at_ms DESC",
                &[&i64::from(account_id)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| TokenInfo {
                token_id: row.get("token_id"),
                account_id: row.get::<_, i64>("account_id") as u32,
                name: row.get("name"),
                created_at_ms: row.get("created_at_ms"),
                last_used_at_ms: row.get("last_used_at_ms"),
                revoked_at_ms: row.get("revoked_at_ms"),
            })
            .collect())
    }

    async fn revoke(&self, account_id: u32, token_id: &str, now_ms: i64) -> Result<bool, String> {
        let changed = self
            .db
            .execute(
                "UPDATE admin_api_tokens SET revoked_at_ms = $3 WHERE account_id = $1 AND token_id = $2 AND revoked_at_ms IS NULL",
                &[&i64::from(account_id), &token_id, &now_ms],
            )
            .await?;
        Ok(changed > 0)
    }

    async fn verify(&self, token_hash: &str, now_ms: i64) -> Result<Option<u32>, String> {
        let row = self
            .db
            .query_opt(
                "UPDATE admin_api_tokens SET last_used_at_ms = $2 WHERE token_hash = $1 AND revoked_at_ms IS NULL RETURNING account_id",
                &[&token_hash, &now_ms],
            )
            .await?;
        Ok(row.map(|row| row.get::<_, i64>("account_id") as u32))
    }
}

/// The tokens in memory, for tests.
#[derive(Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct MemoryTokens {
    rows: Mutex<Vec<(TokenInfo, String)>>,
}

#[tonic::async_trait]
impl Tokens for MemoryTokens {
    async fn create(&self, token_id: &str, token_hash: &str, account_id: u32, name: &str, now_ms: i64) -> Result<(), String> {
        let info = TokenInfo { token_id: token_id.into(), account_id, name: name.into(), created_at_ms: now_ms, ..Default::default() };
        self.rows.lock().unwrap().push((info, token_hash.into()));
        Ok(())
    }

    async fn list(&self, account_id: u32) -> Result<Vec<TokenInfo>, String> {
        Ok(self.rows.lock().unwrap().iter().rev().filter(|(info, _)| info.account_id == account_id).map(|(info, _)| info.clone()).collect())
    }

    async fn revoke(&self, account_id: u32, token_id: &str, now_ms: i64) -> Result<bool, String> {
        let mut rows = self.rows.lock().unwrap();
        let found = rows.iter_mut().find(|(info, _)| info.account_id == account_id && info.token_id == token_id && info.revoked_at_ms.is_none());
        Ok(found.map(|(info, _)| info.revoked_at_ms = Some(now_ms)).is_some())
    }

    async fn verify(&self, token_hash: &str, now_ms: i64) -> Result<Option<u32>, String> {
        let mut rows = self.rows.lock().unwrap();
        let found = rows.iter_mut().find(|(info, hash)| hash == token_hash && info.revoked_at_ms.is_none());
        Ok(found.map(|(info, _)| {
            info.last_used_at_ms = Some(now_ms);
            info.account_id
        }))
    }
}

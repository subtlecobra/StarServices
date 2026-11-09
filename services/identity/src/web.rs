//! Web accounts, for sc-portal, sc-admin-web and sc-admin: passwords, emails, roles, web sessions and the login audit.
//! Everything here is in identity's own tables, next to `accounts`.
//!
//! A registered account takes its index from `registered_account_index`, from 100,001 on, and its account id and GEID
//! follow from the index as for the accounts sc-admin issues (1,000,000 and 300,000,000,000 plus the index), because the
//! DGS derives a player's hangar and items from that index. sc-admin's own accounts count up from 1, so the two never
//! meet.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use sc_db::{internal, Postgres, Row};
use sc_api::internal::{
    AccountSummary, ChangePasswordRequest, GameBadge, ChangePasswordResponse, GetAccountRequest, GetAccountResponse,
    IssueGameTokenRequest, IssueGameTokenResponse, ListWebSessionsRequest, ListWebSessionsResponse,
    PasswordLoginRequest, PasswordLoginResponse, RegisterAccountRequest, RegisterAccountResponse,
    RevokeWebSessionRequest, RevokeWebSessionResponse, SearchAccountsRequest, SearchAccountsResponse,
    SetAccountStateRequest, SetAccountStateResponse, VerifyWebSessionRequest, VerifyWebSessionResponse, WebClient,
    WebSession,
};
use sc_runtime::state::{Account, State, TOKEN_LIFETIME_SECS};
use tonic::Status;
use tracing::{info, warn};

use crate::password::{check_policy, Hasher};

/// Additive only: tables, columns and indexes are added, never changed or dropped.
pub const SCHEMA: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS accounts_handle_folded ON accounts (lower(handle));
CREATE SEQUENCE IF NOT EXISTS registered_account_index START 100001;
CREATE TABLE IF NOT EXISTS account_credentials (
    account_id BIGINT PRIMARY KEY,
    email TEXT,
    email_verified_at_ms BIGINT,
    password_hash TEXT,
    password_changed_at_ms BIGINT,
    disabled BOOLEAN NOT NULL DEFAULT false,
    updated_at_ms BIGINT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS account_credentials_email ON account_credentials (email);
CREATE TABLE IF NOT EXISTS account_roles (
    account_id BIGINT NOT NULL,
    role TEXT NOT NULL,
    granted_at_ms BIGINT NOT NULL,
    PRIMARY KEY (account_id, role)
);
CREATE TABLE IF NOT EXISTS account_game_badges (
    account_id BIGINT NOT NULL,
    badge INTEGER NOT NULL,
    granted_at_ms BIGINT NOT NULL,
    PRIMARY KEY (account_id, badge)
);
CREATE TABLE IF NOT EXISTS web_sessions (
    session_id TEXT PRIMARY KEY,
    token_hash BYTEA NOT NULL UNIQUE,
    account_id BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    last_seen_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    revoked_at_ms BIGINT,
    ip TEXT NOT NULL,
    user_agent TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS web_sessions_account ON web_sessions (account_id);
CREATE TABLE IF NOT EXISTS account_audit (
    event_id TEXT PRIMARY KEY,
    at_ms BIGINT NOT NULL,
    event TEXT NOT NULL,
    account_id BIGINT,
    login TEXT NOT NULL,
    ip TEXT NOT NULL,
    user_agent TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS account_audit_login ON account_audit (login, at_ms);
CREATE INDEX IF NOT EXISTS account_audit_ip ON account_audit (ip, at_ms);
CREATE INDEX IF NOT EXISTS account_audit_account ON account_audit (account_id, at_ms);";

const FIRST_ACCOUNT_ID: i64 = 1_000_000;
const FIRST_GEID: i64 = 300_000_000_000;
/// The DGS puts a player's hangar ids at 220,000,000,000 + index x 0x10000, which must stay below the players' GEIDs.
const LAST_REGISTERED_INDEX: i64 = 1_199_999;

pub const ROLES: &[&str] = &["admin"];

/// The badges the game has, once each, by their `PlatformBadge` id: the seven of its `EAccountBadgeMask` (reflected at
/// 0x142d4d3e0), named for the bit each sets in the mask's second word: QA 0x01 (id 14), STAFF 0x02 (1), GM 0x04 (126),
/// SUBSCRIBER 0x08 (5), CONCIERGE 0x10 (9), MMHC 0x20 (123), BACKER 0x40 (4). The console takes an account as a
/// developer's when that word has bit 0, 1 or 2, which ids 14, 1 and 126 set (`CXConsole::IsCommandExecutionAllowed`):
/// that unlocks its restricted commands and cvars. The client reads only the ids.
pub const GAME_BADGES: &[(u32, &str)] = &[
    (1, "RSI_STAFF"),
    (14, "RSI_QA"),
    (126, "RSI_GM"),
    (5, "RSI_SUBSCRIBER"),
    (9, "RSI_CONCIERGE"),
    (123, "RSI_MMHC"),
    (4, "RSI_BACKER"),
];

pub fn game_badge(id: u32) -> Option<GameBadge> {
    GAME_BADGES.iter().find(|(known, _)| *known == id).map(|(id, name)| GameBadge { id: *id, name: (*name).to_string() })
}

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * MINUTE_MS;
const SESSION_LIFETIME_MS: i64 = 30 * DAY_MS;
/// A session unused this long ends.
const SESSION_IDLE_MS: i64 = 7 * DAY_MS;
/// `last_seen_at_ms` is written at most this often, so a page view is not a write.
const LAST_SEEN_STEP_MS: i64 = 5 * MINUTE_MS;
const TOKEN_BYTES: usize = 32;

const FAILURE_WINDOW_MS: i64 = 15 * MINUTE_MS;
/// Failed logins in the window, for one login name from one address, for one login name from anywhere, and from one
/// address for any name. A name is counted whether an account has it or not, so the limit says nothing about accounts.
const MAX_FAILURES_LOGIN_IP: i64 = 10;
const MAX_FAILURES_LOGIN: i64 = 100;
const MAX_FAILURES_IP: i64 = 50;
const REGISTER_WINDOW_MS: i64 = 60 * MINUTE_MS;
const MAX_REGISTRATIONS_IP: i64 = 5;
/// Wrong current passwords per account in the window.
const MAX_PASSWORD_CHANGE_FAILURES: i64 = 10;

/// Sessions and audit rows are kept this long after they end.
const SESSION_RETENTION_MS: i64 = 30 * DAY_MS;
const AUDIT_RETENTION_MS: i64 = 180 * DAY_MS;

const HANDLE_MIN: usize = 3;
const HANDLE_MAX: usize = 32;
const EMAIL_MAX: usize = 254;

const FAILED_LOGIN: &str = "Wrong login or password.";
const TAKEN: &str = "That handle or email cannot be used. Choose another, or log in.";
const THROTTLED: &str = "Too many attempts. Wait a few minutes and try again.";

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn random_bytes<const N: usize>() -> Result<[u8; N], Status> {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| Status::internal("no system randomness"))?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn token_hash(token: &str) -> Vec<u8> {
    digest::digest(&digest::SHA256, token.as_bytes()).as_ref().to_vec()
}

/// A token as issued: 32 random bytes, base64url.
fn well_formed_token(token: &str) -> bool {
    token.len() == 43 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn clip(text: &str, max: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// What logins are counted and found by: trimmed and lower case.
pub fn normalize_login(login: &str) -> String {
    clip(login.trim(), EMAIL_MAX).to_lowercase()
}

pub fn check_handle(handle: &str) -> Result<(), Status> {
    let fine = (HANDLE_MIN..=HANDLE_MAX).contains(&handle.len())
        && handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if fine {
        Ok(())
    } else {
        Err(Status::invalid_argument("A handle has 3 to 32 of the letters A-Z, digits, '_' and '-'."))
    }
}

/// Lower case. A plain shape check: one '@', something on each side, a dot in the domain, nothing odd.
pub fn normalize_email(email: &str) -> Result<String, Status> {
    let email = email.trim().to_lowercase();
    let bad = || Status::invalid_argument("That email address does not look right.");
    if email.len() > EMAIL_MAX || email.chars().any(|c| c.is_whitespace() || c.is_control() || "<>()[]\\,;:\"".contains(c)) {
        return Err(bad());
    }
    let (local, domain) = email.split_once('@').ok_or_else(bad)?;
    let domain_fine = domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && !domain.contains('@');
    if local.is_empty() || !domain_fine {
        return Err(bad());
    }
    Ok(email)
}

/// A client's address and agent, as stored.
struct Client {
    ip: String,
    user_agent: String,
}

impl Client {
    fn from(client: Option<WebClient>) -> Self {
        let client = client.unwrap_or_default();
        Self { ip: clip(&client.ip, 64), user_agent: clip(&client.user_agent, 256) }
    }
}

pub struct Web {
    db: Postgres,
    state: Arc<State>,
    hasher: Hasher,
}

/// An account with what the web part keeps for it.
struct Stored {
    account: Account,
    email: Option<String>,
    password_hash: Option<String>,
    disabled: bool,
}

const STORED: &str = "SELECT a.account_id, a.handle, a.geid, a.created_at_ms, c.email, c.password_hash,
    COALESCE(c.disabled, false) AS disabled
    FROM accounts a LEFT JOIN account_credentials c ON c.account_id = a.account_id";

fn stored(row: &Row) -> Stored {
    Stored {
        account: Account {
            account_id: row.get::<_, i64>("account_id") as u32,
            handle: row.get("handle"),
            geid: row.get::<_, i64>("geid") as u64,
            created_at_ms: row.get::<_, i64>("created_at_ms") as u64,
        },
        email: row.get("email"),
        password_hash: row.get("password_hash"),
        disabled: row.get("disabled"),
    }
}

impl Web {
    pub fn new(db: Postgres, state: Arc<State>, hasher: Hasher) -> Self {
        Self { db, state, hasher }
    }

    async fn audit(&self, event: &str, account_id: Option<u32>, login: &str, client: &Client, detail: &str) {
        let Ok(id) = random_bytes::<16>() else { return };
        let result = self
            .db
            .execute(
                "INSERT INTO account_audit (event_id, at_ms, event, account_id, login, ip, user_agent, detail)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[&hex(&id), &now_ms(), &event, &account_id.map(i64::from), &login, &client.ip, &client.user_agent, &detail],
            )
            .await;
        if let Err(error) = result {
            warn!(%error, event, "audit not written");
        }
        info!(event, account_id, ip = %client.ip, "account audit");
    }

    async fn count(&self, sql: &str, params: &[&(dyn sc_db::ToSql + Sync)]) -> Result<i64, Status> {
        let row = self.db.query_opt(sql, params).await.map_err(internal)?;
        Ok(row.map_or(0, |row| row.get::<_, i64>(0)))
    }

    async fn account(&self, account_id: u32) -> Result<Option<Stored>, Status> {
        let row = self
            .db
            .query_opt(&format!("{STORED} WHERE a.account_id = $1"), &[&i64::from(account_id)])
            .await
            .map_err(internal)?;
        Ok(row.as_ref().map(stored))
    }

    async fn roles(&self, account_id: u32) -> Result<Vec<String>, Status> {
        let rows = self
            .db
            .query("SELECT role FROM account_roles WHERE account_id = $1 ORDER BY role", &[&i64::from(account_id)])
            .await
            .map_err(internal)?;
        Ok(rows.iter().map(|row| row.get("role")).collect())
    }

    async fn game_badges(&self, account_id: u32) -> Result<Vec<GameBadge>, Status> {
        let rows = self
            .db
            .query("SELECT badge FROM account_game_badges WHERE account_id = $1 ORDER BY badge", &[&i64::from(account_id)])
            .await
            .map_err(internal)?;
        Ok(rows.iter().filter_map(|row| game_badge(row.get::<_, i32>("badge") as u32)).collect())
    }

    async fn summary(&self, stored: Stored) -> Result<AccountSummary, Status> {
        Ok(AccountSummary {
            game_badges: self.game_badges(stored.account.account_id).await?,
            account_id: stored.account.account_id,
            roles: self.roles(stored.account.account_id).await?,
            handle: stored.account.handle,
            email: stored.email.unwrap_or_default(),
            created_at: stored.account.created_at_ms as i64,
            disabled: stored.disabled,
            geid: stored.account.geid,
        })
    }

    async fn revoke_all(&self, account_id: u32, except: Option<&[u8]>) -> Result<u64, Status> {
        let keep = except.map(<[u8]>::to_vec).unwrap_or_default();
        self.db
            .execute(
                "UPDATE web_sessions SET revoked_at_ms = $2
                 WHERE account_id = $1 AND revoked_at_ms IS NULL AND token_hash <> $3",
                &[&i64::from(account_id), &now_ms(), &keep],
            )
            .await
            .map_err(internal)
    }

    pub async fn register(&self, request: RegisterAccountRequest) -> Result<RegisterAccountResponse, Status> {
        let client = Client::from(request.client);
        let handle = request.handle.trim().to_string();
        check_handle(&handle)?;
        let email = normalize_email(&request.email)?;
        check_policy(&request.password, &handle, &email).map_err(Status::invalid_argument)?;
        let since = now_ms() - REGISTER_WINDOW_MS;
        let recent = self
            .count(
                "SELECT count(*) FROM account_audit WHERE ip = $1 AND event = 'register' AND at_ms > $2",
                &[&client.ip, &since],
            )
            .await?;
        if recent >= MAX_REGISTRATIONS_IP {
            self.audit("register_throttled", None, &handle.to_lowercase(), &client, "").await;
            return Err(Status::resource_exhausted(THROTTLED));
        }
        let password_hash = self.hasher.hash(&request.password).await?;

        let mut db = self.db.client().await.map_err(internal)?;
        let transaction = db.transaction().await.map_err(internal)?;
        let index: i64 = transaction
            .query_one("SELECT nextval('registered_account_index')", &[])
            .await
            .map_err(internal)?
            .get(0);
        if index > LAST_REGISTERED_INDEX {
            warn!(index, "registered account indexes are used up");
            return Err(Status::resource_exhausted("Registration is closed."));
        }
        let (account_id, geid, now) = (FIRST_ACCOUNT_ID + index, FIRST_GEID + index, now_ms());
        let inserted = async {
            transaction
                .execute(
                    "INSERT INTO accounts (geid, account_id, handle, created_at_ms) VALUES ($1, $2, $3, $4)",
                    &[&geid, &account_id, &handle, &now],
                )
                .await?;
            transaction
                .execute(
                    "INSERT INTO account_credentials
                     (account_id, email, password_hash, password_changed_at_ms, disabled, updated_at_ms)
                     VALUES ($1, $2, $3, $4, false, $4)",
                    &[&account_id, &email, &password_hash, &now],
                )
                .await
        }
        .await;
        match inserted {
            Ok(_) => transaction.commit().await.map_err(internal)?,
            Err(error) if error.code().is_some_and(|code| code.code() == "23505") => {
                drop(transaction);
                self.audit("register_refused", None, &handle.to_lowercase(), &client, "taken").await;
                return Err(Status::already_exists(TAKEN));
            }
            Err(error) => return Err(internal(error)),
        }
        let account_id = account_id as u32;
        self.audit("register", Some(account_id), &handle.to_lowercase(), &client, "").await;
        Ok(RegisterAccountResponse { account_id, handle })
    }

    pub async fn login(&self, request: PasswordLoginRequest) -> Result<PasswordLoginResponse, Status> {
        let client = Client::from(request.client);
        let login = normalize_login(&request.login);
        let since = now_ms() - FAILURE_WINDOW_MS;
        let failures = "SELECT count(*) FROM account_audit WHERE event = 'login_failed' AND at_ms > $1";
        let throttled = self
            .count(&format!("{failures} AND login = $2 AND ip = $3"), &[&since, &login, &client.ip])
            .await?
            >= MAX_FAILURES_LOGIN_IP
            || self.count(&format!("{failures} AND login = $2"), &[&since, &login]).await? >= MAX_FAILURES_LOGIN
            || self.count(&format!("{failures} AND ip = $2"), &[&since, &client.ip]).await? >= MAX_FAILURES_IP;
        if throttled {
            self.audit("login_throttled", None, &login, &client, "").await;
            return Err(Status::resource_exhausted(THROTTLED));
        }

        let found = if login.contains('@') {
            self.db.query_opt(&format!("{STORED} WHERE c.email = $1"), &[&login]).await
        } else {
            self.db.query_opt(&format!("{STORED} WHERE lower(a.handle) = $1"), &[&login]).await
        };
        let found = found.map_err(internal)?.as_ref().map(stored);
        let hash = found.as_ref().and_then(|found| found.password_hash.as_deref());
        // With no account or no password, a dummy hash is verified, so the answer takes as long and says the same.
        let verified = self.hasher.verify(&request.password, hash).await?;
        let Some(found) = found.filter(|_| verified.ok) else {
            self.audit("login_failed", None, &login, &client, "").await;
            return Err(Status::unauthenticated(FAILED_LOGIN));
        };
        let account_id = found.account.account_id;
        if found.disabled {
            self.audit("login_disabled", Some(account_id), &login, &client, "").await;
            return Err(Status::permission_denied("This account is disabled."));
        }
        if verified.rehash {
            let rehashed = self.hasher.hash(&request.password).await?;
            self.db
                .execute(
                    "UPDATE account_credentials SET password_hash = $2, updated_at_ms = $3 WHERE account_id = $1",
                    &[&i64::from(account_id), &rehashed, &now_ms()],
                )
                .await
                .map_err(internal)?;
            info!(account_id, "password hash upgraded");
        }

        let token = URL_SAFE_NO_PAD.encode(random_bytes::<TOKEN_BYTES>()?);
        let session_id = hex(&random_bytes::<16>()?);
        let now = now_ms();
        let expires_at = now + SESSION_LIFETIME_MS;
        self.db
            .execute(
                "INSERT INTO web_sessions
                 (session_id, token_hash, account_id, created_at_ms, last_seen_at_ms, expires_at_ms, ip, user_agent)
                 VALUES ($1, $2, $3, $4, $4, $5, $6, $7)",
                &[&session_id, &token_hash(&token), &i64::from(account_id), &now, &expires_at, &client.ip, &client.user_agent],
            )
            .await
            .map_err(internal)?;
        self.audit("login", Some(account_id), &login, &client, &session_id).await;
        Ok(PasswordLoginResponse {
            session_token: token,
            account_id,
            roles: self.roles(account_id).await?,
            handle: found.account.handle,
            expires_at,
        })
    }

    pub async fn verify_session(&self, request: VerifyWebSessionRequest) -> Result<VerifyWebSessionResponse, Status> {
        if !well_formed_token(&request.token) {
            return Ok(VerifyWebSessionResponse::default());
        }
        let row = self
            .db
            .query_opt(
                "SELECT s.session_id, s.account_id, s.last_seen_at_ms, s.expires_at_ms, a.handle,
                 COALESCE(c.disabled, false) AS disabled
                 FROM web_sessions s JOIN accounts a ON a.account_id = s.account_id
                 LEFT JOIN account_credentials c ON c.account_id = s.account_id
                 WHERE s.token_hash = $1 AND s.revoked_at_ms IS NULL",
                &[&token_hash(&request.token)],
            )
            .await
            .map_err(internal)?;
        let Some(row) = row else { return Ok(VerifyWebSessionResponse::default()) };
        let now = now_ms();
        let last_seen: i64 = row.get("last_seen_at_ms");
        let live = now < row.get::<_, i64>("expires_at_ms") && now < last_seen + SESSION_IDLE_MS;
        if !live || row.get::<_, bool>("disabled") {
            return Ok(VerifyWebSessionResponse::default());
        }
        if now - last_seen > LAST_SEEN_STEP_MS {
            let session_id: String = row.get("session_id");
            self.db
                .execute("UPDATE web_sessions SET last_seen_at_ms = $2 WHERE session_id = $1", &[&session_id, &now])
                .await
                .map_err(internal)?;
        }
        let account_id = row.get::<_, i64>("account_id") as u32;
        Ok(VerifyWebSessionResponse {
            valid: true,
            account_id,
            handle: row.get("handle"),
            roles: self.roles(account_id).await?,
        })
    }

    pub async fn revoke_session(&self, request: RevokeWebSessionRequest) -> Result<RevokeWebSessionResponse, Status> {
        let now = now_ms();
        let revoked = if !request.token.is_empty() {
            self.db
                .execute(
                    "UPDATE web_sessions SET revoked_at_ms = $2 WHERE token_hash = $1 AND revoked_at_ms IS NULL",
                    &[&token_hash(&request.token), &now],
                )
                .await
        } else if !request.session_id.is_empty() {
            self.db
                .execute(
                    "UPDATE web_sessions SET revoked_at_ms = $3
                     WHERE session_id = $1 AND account_id = $2 AND revoked_at_ms IS NULL",
                    &[&request.session_id, &i64::from(request.account_id), &now],
                )
                .await
        } else {
            return Err(Status::invalid_argument("a token, or an account and a session id"));
        };
        Ok(RevokeWebSessionResponse { revoked: revoked.map_err(internal)? > 0 })
    }

    pub async fn list_sessions(&self, request: ListWebSessionsRequest) -> Result<ListWebSessionsResponse, Status> {
        let now = now_ms();
        let rows = self
            .db
            .query(
                "SELECT session_id, token_hash, created_at_ms, last_seen_at_ms, expires_at_ms, ip, user_agent
                 FROM web_sessions
                 WHERE account_id = $1 AND revoked_at_ms IS NULL AND expires_at_ms > $2 AND last_seen_at_ms > $3
                 ORDER BY created_at_ms DESC",
                &[&i64::from(request.account_id), &now, &(now - SESSION_IDLE_MS)],
            )
            .await
            .map_err(internal)?;
        let current = token_hash(&request.current_token);
        let sessions = rows
            .iter()
            .map(|row| WebSession {
                session_id: row.get("session_id"),
                created_at: row.get("created_at_ms"),
                last_seen_at: row.get("last_seen_at_ms"),
                expires_at: row.get("expires_at_ms"),
                ip: row.get("ip"),
                user_agent: row.get("user_agent"),
                current: !request.current_token.is_empty() && row.get::<_, Vec<u8>>("token_hash") == current,
            })
            .collect();
        Ok(ListWebSessionsResponse { sessions })
    }

    pub async fn change_password(&self, request: ChangePasswordRequest) -> Result<ChangePasswordResponse, Status> {
        let client = Client::from(request.client);
        let account_id = request.account_id;
        let found = self.account(account_id).await?.ok_or_else(|| Status::not_found("no such account"))?;
        let since = now_ms() - FAILURE_WINDOW_MS;
        let failures = self
            .count(
                "SELECT count(*) FROM account_audit WHERE account_id = $1 AND event = 'password_change_failed'
                 AND at_ms > $2",
                &[&i64::from(account_id), &since],
            )
            .await?;
        let login = found.account.handle.to_lowercase();
        if failures >= MAX_PASSWORD_CHANGE_FAILURES {
            return Err(Status::resource_exhausted(THROTTLED));
        }
        let verified = self.hasher.verify(&request.current_password, found.password_hash.as_deref()).await?;
        if !verified.ok {
            self.audit("password_change_failed", Some(account_id), &login, &client, "").await;
            return Err(Status::unauthenticated("The current password is wrong."));
        }
        let email = found.email.clone().unwrap_or_default();
        check_policy(&request.new_password, &found.account.handle, &email).map_err(Status::invalid_argument)?;
        let hash = self.hasher.hash(&request.new_password).await?;
        let now = now_ms();
        self.db
            .execute(
                "UPDATE account_credentials SET password_hash = $2, password_changed_at_ms = $3, updated_at_ms = $3
                 WHERE account_id = $1",
                &[&i64::from(account_id), &hash, &now],
            )
            .await
            .map_err(internal)?;
        let keep = token_hash(&request.keep_session_token);
        let revoked = self.revoke_all(account_id, Some(&keep)).await?;
        self.audit("password_changed", Some(account_id), &login, &client, &format!("revoked {revoked}")).await;
        Ok(ChangePasswordResponse {})
    }

    pub async fn issue_game_token(&self, request: IssueGameTokenRequest) -> Result<IssueGameTokenResponse, Status> {
        let found = self.account(request.account_id).await?.ok_or_else(|| Status::not_found("no such account"))?;
        if found.disabled {
            return Err(Status::permission_denied("This account is disabled."));
        }
        let token = self.state.issue_token(&found.account);
        let client = Client { ip: String::new(), user_agent: String::new() };
        self.audit("game_token", Some(found.account.account_id), &found.account.handle.to_lowercase(), &client, "")
            .await;
        Ok(IssueGameTokenResponse {
            token,
            handle: found.account.handle,
            expires_at: now_ms() + TOKEN_LIFETIME_SECS as i64 * 1000,
        })
    }

    pub async fn get_account(&self, request: GetAccountRequest) -> Result<GetAccountResponse, Status> {
        let found = self.account(request.account_id).await?.ok_or_else(|| Status::not_found("no such account"))?;
        Ok(GetAccountResponse { account: Some(self.summary(found).await?) })
    }

    pub async fn search(&self, request: SearchAccountsRequest) -> Result<SearchAccountsResponse, Status> {
        let limit = match request.limit {
            0 => 50,
            limit => limit.min(200),
        } as i64;
        let after: i64 = match request.page_token.as_str() {
            "" => -1,
            token => token.parse().map_err(|_| Status::invalid_argument("bad page token"))?,
        };
        let query = request.query.trim().to_lowercase();
        let escaped: String = query
            .chars()
            .flat_map(|c| if "%_\\".contains(c) { vec!['\\', c] } else { vec![c] })
            .collect();
        let pattern = format!("%{escaped}%");
        let exact_id: i64 = query.parse::<u32>().map_or(-1, i64::from);
        let rows = self
            .db
            .query(
                &format!(
                    "{STORED} WHERE a.account_id > $1
                     AND ($2 = '' OR lower(a.handle) LIKE $3 OR c.email LIKE $3 OR a.account_id = $4)
                     ORDER BY a.account_id LIMIT $5"
                ),
                &[&after, &query, &pattern, &exact_id, &(limit + 1)],
            )
            .await
            .map_err(internal)?;
        let more = rows.len() as i64 > limit;
        let mut accounts = Vec::new();
        for row in rows.iter().take(limit as usize) {
            accounts.push(self.summary(stored(row)).await?);
        }
        let next_page_token = match (more, accounts.last()) {
            (true, Some(last)) => last.account_id.to_string(),
            _ => String::new(),
        };
        Ok(SearchAccountsResponse { accounts, next_page_token })
    }

    pub async fn set_state(&self, request: SetAccountStateRequest) -> Result<SetAccountStateResponse, Status> {
        let account_id = request.account_id;
        let found = self.account(account_id).await?.ok_or_else(|| Status::not_found("no such account"))?;
        if request.set_roles {
            if let Some(unknown) = request.roles.iter().find(|role| !ROLES.contains(&role.as_str())) {
                return Err(Status::invalid_argument(format!("unknown role '{unknown}'")));
            }
        }
        if request.set_game_badges {
            if let Some(unknown) = request.game_badges.iter().find(|badge| game_badge(**badge).is_none()) {
                return Err(Status::invalid_argument(format!("unknown game badge {unknown:#x}")));
            }
        }
        let hash = match &request.new_password {
            Some(password) => {
                let email = found.email.clone().unwrap_or_default();
                check_policy(password, &found.account.handle, &email).map_err(Status::invalid_argument)?;
                Some(self.hasher.hash(password).await?)
            }
            None => None,
        };
        let id = i64::from(account_id);
        let now = now_ms();
        let mut db = self.db.client().await.map_err(internal)?;
        let transaction = db.transaction().await.map_err(internal)?;
        transaction
            .execute(
                "INSERT INTO account_credentials (account_id, disabled, updated_at_ms) VALUES ($1, false, $2)
                 ON CONFLICT (account_id) DO NOTHING",
                &[&id, &now],
            )
            .await
            .map_err(internal)?;
        if let Some(disabled) = request.disabled {
            transaction
                .execute(
                    "UPDATE account_credentials SET disabled = $2, updated_at_ms = $3 WHERE account_id = $1",
                    &[&id, &disabled, &now],
                )
                .await
                .map_err(internal)?;
        }
        if let Some(hash) = &hash {
            transaction
                .execute(
                    "UPDATE account_credentials SET password_hash = $2, password_changed_at_ms = $3, updated_at_ms = $3
                     WHERE account_id = $1",
                    &[&id, hash, &now],
                )
                .await
                .map_err(internal)?;
        }
        if request.set_roles {
            transaction.execute("DELETE FROM account_roles WHERE account_id = $1", &[&id]).await.map_err(internal)?;
            for role in &request.roles {
                transaction
                    .execute(
                        "INSERT INTO account_roles (account_id, role, granted_at_ms) VALUES ($1, $2, $3)
                         ON CONFLICT DO NOTHING",
                        &[&id, role, &now],
                    )
                    .await
                    .map_err(internal)?;
            }
        }
        if request.set_game_badges {
            transaction.execute("DELETE FROM account_game_badges WHERE account_id = $1", &[&id]).await.map_err(internal)?;
            for badge in &request.game_badges {
                transaction
                    .execute(
                        "INSERT INTO account_game_badges (account_id, badge, granted_at_ms) VALUES ($1, $2, $3)
                         ON CONFLICT DO NOTHING",
                        &[&id, &(*badge as i32), &now],
                    )
                    .await
                    .map_err(internal)?;
            }
        }
        transaction.commit().await.map_err(internal)?;
        if request.disabled == Some(true) || hash.is_some() {
            self.revoke_all(account_id, None).await?;
        }
        let mut changes = Vec::new();
        if let Some(disabled) = request.disabled {
            changes.push(format!("disabled={disabled}"));
        }
        if request.set_roles {
            changes.push(format!("roles={}", request.roles.join(",")));
        }
        if request.set_game_badges {
            let names: Vec<String> = request.game_badges.iter().filter_map(|badge| game_badge(*badge)).map(|badge| badge.name).collect();
            changes.push(format!("game_badges={}", names.join(",")));
        }
        if hash.is_some() {
            changes.push("password reset".to_string());
        }
        let client = Client { ip: String::new(), user_agent: String::new() };
        self.audit("admin_update", Some(account_id), &found.account.handle.to_lowercase(), &client, &changes.join(" "))
            .await;
        let found = self.account(account_id).await?.ok_or_else(|| Status::not_found("no such account"))?;
        Ok(SetAccountStateResponse { account: Some(self.summary(found).await?) })
    }

    /// Drops sessions and audit rows past their retention.
    pub async fn clean_up(&self) -> Result<(), String> {
        let now = now_ms();
        let sessions = self
            .db
            .execute(
                "DELETE FROM web_sessions WHERE expires_at_ms < $1 OR revoked_at_ms < $1 OR last_seen_at_ms < $2",
                &[&(now - SESSION_RETENTION_MS), &(now - SESSION_RETENTION_MS - SESSION_IDLE_MS)],
            )
            .await?;
        let audit = self.db.execute("DELETE FROM account_audit WHERE at_ms < $1", &[&(now - AUDIT_RETENTION_MS)]).await?;
        if sessions + audit > 0 {
            info!(sessions, audit, "old web sessions and audit rows removed");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_and_emails_are_checked_and_normalized() {
        assert!(check_handle("Pilot_7-x").is_ok());
        for bad in ["ab", "has space", "../evil", "émile", &"x".repeat(33)] {
            assert!(check_handle(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize_email(" Jane.Doe@Example.COM ").unwrap(), "jane.doe@example.com");
        for bad in ["nope", "@example.com", "a@b", "a@.b.c", "a@b..c", "a b@c.d", "a@b@c.d", "<a>@b.c"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize_login("  PiLot "), "pilot");
    }

    #[test]
    fn tokens_are_checked_for_shape_and_stored_hashed() {
        let token = URL_SAFE_NO_PAD.encode([7u8; TOKEN_BYTES]);
        assert!(well_formed_token(&token));
        assert!(!well_formed_token("short"));
        assert!(!well_formed_token(&format!("{}=", &token[..42])));
        assert_eq!(token_hash(&token).len(), 32);
        assert_ne!(token_hash(&token), token.as_bytes());
    }

    #[test]
    fn registered_accounts_stay_clear_of_issued_ones_and_of_the_hangars() {
        // sc-admin's accounts count up from index 1, and the DGS's hangar of the last index stays below the GEIDs.
        const { assert!(FIRST_ACCOUNT_ID + 100_001 > 1_000_000 + 99_999) };
        const { assert!(220_000_000_000 + (LAST_REGISTERED_INDEX as u64) * 0x10000 < 300_000_000_000) };
        assert!(u32::try_from(FIRST_ACCOUNT_ID + LAST_REGISTERED_INDEX).is_ok());
    }
}

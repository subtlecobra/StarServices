//! Persistent server state: accounts and the JWT signing secret, in one JSON file.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};

use sc_proto::identity::{player, Player};

const STATE_FILE: &str = "state.json";
/// How long a game token is valid.
pub const TOKEN_LIFETIME_SECS: u64 = 30 * 24 * 3600;
/// Entity ids up to this value are local to the client, and it refuses to spawn a replicated
/// entity with one ("Replicated entities must not have a local entity id", check at 0x1467b35ac).
const LAST_LOCAL_ENTITY_ID: u64 = 0x2e_90cf_4b7f;
const FIRST_GEID: u64 = 300_000_000_000;
const FIRST_ACCOUNT_ID: u32 = 1_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub account_id: u32,
    pub handle: String,
    pub geid: u64,
    pub created_at_ms: u64,
}

impl Account {
    pub fn player(&self) -> Player {
        Player {
            created_at: self.created_at_ms,
            updated_at: self.created_at_ms,
            geid: self.geid,
            account_id: self.account_id,
            name: self.handle.clone(),
            state: player::State::Current as i32,
            is_fulfilled: true,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    jwt_secret: String,
    accounts: Vec<Account>,
}

pub struct State {
    path: PathBuf,
    key: hmac::Key,
    data: Mutex<Persisted>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl State {
    /// Opens or creates the state, for tools that change it.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir).map_err(|e| format!("create {}: {e}", data_dir.display()))?;
        let state = Self::read(data_dir.join(STATE_FILE))?;
        state.save()?;
        Ok(state)
    }

    /// Reads existing state without writing, for services. In a cluster the file is a read-only
    /// Secret mount.
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join(STATE_FILE);
        if !path.exists() {
            return Err(format!("{} not found: run sc-admin init", path.display()));
        }
        Self::read(path)
    }

    fn read(path: PathBuf) -> Result<Self, String> {
        let mut data: Persisted = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?
        } else {
            let mut secret = [0u8; 32];
            SystemRandom::new().fill(&mut secret).map_err(|_| "no system randomness".to_string())?;
            Persisted { jwt_secret: URL_SAFE_NO_PAD.encode(secret), accounts: Vec::new() }
        };
        // Early versions handed out ids in the client's local range.
        for account in data.accounts.iter_mut().filter(|a| a.geid <= LAST_LOCAL_ENTITY_ID) {
            account.geid = FIRST_GEID + u64::from(account.account_id - FIRST_ACCOUNT_ID);
        }
        let secret = URL_SAFE_NO_PAD.decode(&data.jwt_secret).map_err(|e| format!("jwt_secret: {e}"))?;
        Ok(Self { path, key: hmac::Key::new(hmac::HMAC_SHA256, &secret), data: Mutex::new(data) })
    }

    fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&*self.data.lock().unwrap())
            .map_err(|e| format!("encode state: {e}"))?;
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path).map_err(|e| format!("replace {}: {e}", self.path.display()))
    }

    /// Returns the account with this handle, creating it on first use.
    pub fn ensure_account(&self, handle: &str) -> Result<Account, String> {
        if handle.is_empty() || handle.len() > 64 || !handle.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c)) {
            return Err(format!("invalid handle '{handle}': use 1-64 of A-Z a-z 0-9 _ -"));
        }
        let account = {
            let mut data = self.data.lock().unwrap();
            if let Some(found) = data.accounts.iter().find(|a| a.handle == handle) {
                return Ok(found.clone());
            }
            let index = data.accounts.len() as u32 + 1;
            let account = Account {
                account_id: FIRST_ACCOUNT_ID + index,
                handle: handle.to_string(),
                geid: FIRST_GEID + u64::from(index),
                created_at_ms: now_secs() * 1000,
            };
            data.accounts.push(account.clone());
            account
        };
        self.save()?;
        Ok(account)
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.data.lock().unwrap().accounts.clone()
    }

    pub fn account(&self, account_id: u32) -> Option<Account> {
        self.data.lock().unwrap().accounts.iter().find(|a| a.account_id == account_id).cloned()
    }

    pub fn account_by_handle(&self, handle: &str) -> Option<Account> {
        self.data.lock().unwrap().accounts.iter().find(|a| a.handle == handle).cloned()
    }

    /// HS256 token. The client only reads the `account_id` claim (CSystem::InitLoginData).
    pub fn issue_token(&self, account: &Account) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let now = now_secs();
        let claims = serde_json::json!({
            "account_id": account.account_id,
            "sub": account.handle,
            "geid": account.geid,
            "created_at_ms": account.created_at_ms,
            "iat": now,
            "exp": now + TOKEN_LIFETIME_SECS,
        });
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{payload}");
        let signature = hmac::sign(&self.key, signing_input.as_bytes());
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }

    /// Verifies signature and expiry, and returns the token's account.
    pub fn verify_token(&self, token: &str) -> Option<Account> {
        let (signing_input, signature) = token.rsplit_once('.')?;
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        hmac::verify(&self.key, signing_input.as_bytes(), &signature).ok()?;
        let payload = URL_SAFE_NO_PAD.decode(signing_input.split_once('.')?.1).ok()?;
        let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
        if claims["exp"].as_u64()? <= now_secs() {
            return None;
        }
        let account_id = u32::try_from(claims["account_id"].as_u64()?).ok()?;
        // The signature vouches for the claims, so services need no account list. Tokens from before
        // the claims existed fall back to the list.
        match claims["geid"].as_u64() {
            Some(geid) => Some(Account {
                account_id,
                handle: claims["sub"].as_str()?.to_string(),
                geid,
                created_at_ms: claims["created_at_ms"].as_u64().unwrap_or(0),
            }),
            None => self.account(account_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(name: &str) -> (State, PathBuf) {
        let dir = std::env::temp_dir().join(format!("scbackend-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (State::open(&dir).unwrap(), dir)
    }

    #[test]
    fn tokens_verify_and_tampering_fails() {
        let (state, dir) = temp_state("token");
        let account = state.ensure_account("pilot").unwrap();
        let token = state.issue_token(&account);
        assert_eq!(state.verify_token(&token).unwrap().account_id, account.account_id);
        let mut forged = token.clone();
        forged.replace_range(token.len() - 3.., "AAA");
        assert!(state.verify_token(&forged).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_token_carries_its_account() {
        let (state, dir) = temp_state("claims");
        let account = state.ensure_account("pilot").unwrap();
        let token = state.issue_token(&account);
        let secret = fs::read_to_string(dir.join(STATE_FILE)).unwrap();
        let mut persisted: serde_json::Value = serde_json::from_str(&secret).unwrap();
        persisted["accounts"] = serde_json::json!([]);
        fs::write(dir.join(STATE_FILE), persisted.to_string()).unwrap();
        let verified = State::load(&dir).unwrap().verify_token(&token).unwrap();
        assert_eq!((verified.handle.as_str(), verified.geid), ("pilot", account.geid));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn accounts_and_secret_survive_a_restart() {
        let (state, dir) = temp_state("persist");
        let account = state.ensure_account("pilot").unwrap();
        let token = state.issue_token(&account);
        drop(state);
        let reopened = State::open(&dir).unwrap();
        assert_eq!(reopened.verify_token(&token).unwrap().handle, "pilot");
        assert_eq!(reopened.ensure_account("pilot").unwrap().geid, account.geid);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn geids_stay_out_of_the_local_entity_range() {
        let (state, dir) = temp_state("geid");
        assert!(state.ensure_account("pilot").unwrap().geid > LAST_LOCAL_ENTITY_ID);
        drop(state);

        let path = dir.join(STATE_FILE);
        let text = fs::read_to_string(&path).unwrap().replace("300000000001", "100000000001");
        fs::write(&path, text).unwrap();
        let migrated = State::open(&dir).unwrap().account_by_handle("pilot").unwrap();
        assert_eq!(migrated.geid, FIRST_GEID + 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_unsafe_handles() {
        let (state, dir) = temp_state("handle");
        assert!(state.ensure_account("../evil").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}

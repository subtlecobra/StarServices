//! Account lookups that `State` does not offer: by GEID and by part of a name. Accounts still live in
//! state.json, so this reads the same file, again whenever a lookup misses.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use sc_proto::services::contacts::v1::AccountInfo;
use sc_runtime::state::Account;
use serde_json::Value;
use tonic::Status;
use tracing::warn;

use crate::urn;

pub struct Directory {
    path: PathBuf,
    accounts: RwLock<Vec<Account>>,
}

fn read(path: &Path) -> Result<Vec<Account>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut state: Value = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    serde_json::from_value(state["accounts"].take()).map_err(|e| format!("accounts: {e}"))
}

impl Directory {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("state.json");
        let accounts = RwLock::new(read(&path)?);
        Ok(Self { path, accounts })
    }

    fn reload(&self) {
        match read(&self.path) {
            Ok(accounts) => *self.accounts.write().unwrap() = accounts,
            Err(error) => warn!(%error, "accounts not reloaded"),
        }
    }

    fn find(&self, matches: impl Fn(&Account) -> bool) -> Option<Account> {
        let cached = || self.accounts.read().unwrap().iter().find(|a| matches(a)).cloned();
        cached().or_else(|| {
            self.reload();
            cached()
        })
    }

    pub fn by_id(&self, account_id: u32) -> Option<Account> {
        self.find(|a| a.account_id == account_id)
    }

    pub fn by_geid(&self, geid: u64) -> Option<Account> {
        self.find(|a| a.geid == geid)
    }

    pub fn by_urn(&self, player_urn: &str) -> Result<Account, Status> {
        let geid = urn::player_geid(player_urn)
            .ok_or_else(|| Status::invalid_argument(format!("not a player urn: {player_urn}")))?;
        self.by_geid(geid).ok_or_else(|| Status::not_found(format!("no player {geid}")))
    }

    /// Accounts whose handle contains `text`, ignoring case.
    pub fn search(&self, text: &str) -> Vec<Account> {
        self.reload();
        let text = text.to_lowercase();
        let accounts = self.accounts.read().unwrap();
        accounts.iter().filter(|a| a.handle.to_lowercase().contains(&text)).cloned().collect()
    }
}

/// How a player appears to others.
pub fn info(account: &Account) -> AccountInfo {
    AccountInfo {
        nickname: account.handle.clone(),
        display_name: account.handle.clone(),
        avatar_url: String::new(),
        account_id: account.account_id,
        player_id: account.geid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> (Directory, PathBuf) {
        let dir = std::env::temp_dir().join(format!("sc-social-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let state = r#"{"jwt_secret":"x","accounts":[
            {"account_id":1000001,"handle":"Pilot","geid":300000000001,"created_at_ms":5},
            {"account_id":1000002,"handle":"Copilot","geid":300000000002,"created_at_ms":6}]}"#;
        fs::write(dir.join("state.json"), state).unwrap();
        (Directory::load(&dir).unwrap(), dir)
    }

    #[test]
    fn finds_by_geid_urn_and_part_of_a_name() {
        let (directory, dir) = directory("find");
        assert_eq!(directory.by_geid(300000000002).unwrap().handle, "Copilot");
        assert_eq!(directory.by_urn("urn:sc:global:player:geid:300000000001").unwrap().handle, "Pilot");
        assert_eq!(directory.search("pil").len(), 2);
        assert_eq!(directory.search("copi").len(), 1);
        assert!(directory.by_id(7).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_new_account_is_found_after_a_miss() {
        let (directory, dir) = directory("reload");
        let state = r#"{"accounts":[{"account_id":9,"handle":"New","geid":10,"created_at_ms":0}]}"#;
        fs::write(dir.join("state.json"), state).unwrap();
        assert_eq!(directory.by_id(9).unwrap().handle, "New");
        fs::remove_dir_all(dir).unwrap();
    }
}

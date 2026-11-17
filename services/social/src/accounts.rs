//! Account lookups that `State` does not offer: by GEID and by part of a name. Accounts still live in
//! state.json, so this reads the same file, again whenever a lookup misses.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use sc_api::internal::{identity_internal_client::IdentityInternalClient, SearchAccountsRequest};
use sc_proto::services::contacts::v1::AccountInfo;
use sc_runtime::state::Account;
use serde_json::Value;
use tonic::{transport::Channel, Status};
use tracing::warn;

use crate::urn;

/// How often the names are read again from sc-identity, which owns them.
const REFRESH_EVERY: Duration = Duration::from_secs(10);

/// Cloning shares the lists.
#[derive(Clone)]
pub struct Directory {
    path: PathBuf,
    accounts: Arc<RwLock<Vec<Account>>>,
    /// The name each player shows, by GEID, as sc-identity has it. Players it has not named show their handle.
    display_names: Arc<RwLock<HashMap<u64, String>>>,
}

fn read(path: &Path) -> Result<Vec<Account>, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut state: Value = serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    serde_json::from_value(state["accounts"].take()).map_err(|e| format!("accounts: {e}"))
}

impl Directory {
    pub fn load(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join("state.json");
        let accounts = Arc::new(RwLock::new(read(&path)?));
        Ok(Self { path, accounts, display_names: Arc::default() })
    }

    fn reload(&self) {
        match read(&self.path) {
            // state.json only adds accounts: those sc-identity gave keep their current handles.
            Ok(read) => {
                let mut accounts = self.accounts.write().unwrap();
                for account in read {
                    if !accounts.iter().any(|known| known.geid == account.geid) {
                        accounts.push(account);
                    }
                }
            }
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

    /// Reads every account's handle and name from sc-identity, the owner. A failure keeps what is known.
    pub async fn refresh(&self, mut identity: IdentityInternalClient<Channel>) -> Result<(), Status> {
        let mut listed = Vec::new();
        let mut page_token = String::new();
        loop {
            let page = identity
                .search_accounts(SearchAccountsRequest { query: String::new(), limit: 200, page_token })
                .await?
                .into_inner();
            listed.extend(page.accounts);
            if page.next_page_token.is_empty() {
                break;
            }
            page_token = page.next_page_token;
        }
        let mut accounts = self.accounts.write().unwrap();
        let mut names = self.display_names.write().unwrap();
        for found in listed {
            names.insert(found.geid, found.display_name);
            match accounts.iter_mut().find(|known| known.geid == found.geid) {
                Some(known) => known.handle = found.handle,
                None => accounts.push(Account {
                    account_id: found.account_id,
                    handle: found.handle,
                    geid: found.geid,
                    created_at_ms: found.created_at.max(0) as u64,
                }),
            }
        }
        Ok(())
    }

    /// Reads from sc-identity every `REFRESH_EVERY`, for as long as the service runs.
    pub fn keep_fresh(&self, identity: IdentityInternalClient<Channel>) {
        let directory = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(status) = directory.refresh(identity.clone()).await {
                    warn!(%status, "names not refreshed from identity");
                }
                tokio::time::sleep(REFRESH_EVERY).await;
            }
        });
    }

    /// The account with the handle it has now, whatever a token said.
    pub fn current(&self, account: &Account) -> Account {
        self.by_geid(account.geid).unwrap_or_else(|| account.clone())
    }

    /// How a player appears to others: the handle as the nickname, the name as the display name.
    pub fn info(&self, account: &Account) -> AccountInfo {
        let display_name = self.display_names.read().unwrap().get(&account.geid).filter(|name| !name.is_empty()).cloned();
        AccountInfo { display_name: display_name.unwrap_or_else(|| account.handle.clone()), ..info(account) }
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

/// How a player appears to others, with the handle for the name. [`Directory::info`] knows the name.
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
    fn a_player_shows_the_name_identity_gave_or_else_the_handle() {
        let (directory, dir) = directory("names");
        let pilot = directory.by_geid(300000000001).unwrap();
        assert_eq!((info(&pilot).nickname.as_str(), directory.info(&pilot).display_name.as_str()), ("Pilot", "Pilot"));
        directory.display_names.write().unwrap().insert(300000000001, "Captain Pilot".into());
        let shown = directory.info(&pilot);
        assert_eq!((shown.nickname.as_str(), shown.display_name.as_str()), ("Pilot", "Captain Pilot"));
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

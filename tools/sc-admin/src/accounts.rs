//! Web account administration through sc-identity, which owns accounts: `grant-role`, `revoke-role` and
//! `set-password`. Bootstraps the first admin, who then uses the admin pages. `scripts/identity-admin.sh` port-forwards
//! sc-identity and runs these.

use sc_api::internal::{
    identity_internal_client::IdentityInternalClient, AccountSummary, SearchAccountsRequest, SetAccountStateRequest,
};
use tonic::transport::{Channel, Endpoint};

async fn client(address: &str) -> Result<IdentityInternalClient<Channel>, String> {
    let endpoint = Endpoint::from_shared(address.to_string()).map_err(|e| format!("identity address: {e}"))?;
    let channel = endpoint.connect().await.map_err(|e| format!("sc-identity at {address}: {e}"))?;
    Ok(IdentityInternalClient::new(channel))
}

/// The account with exactly this handle, case ignored, through the search the admin pages use.
async fn find(identity: &mut IdentityInternalClient<Channel>, handle: &str) -> Result<AccountSummary, String> {
    let mut page_token = String::new();
    loop {
        let request = SearchAccountsRequest { query: handle.to_string(), limit: 200, page_token };
        let page = identity.search_accounts(request).await.map_err(|s| format!("search: {}", s.message()))?.into_inner();
        if let Some(found) = page.accounts.into_iter().find(|account| account.handle.eq_ignore_ascii_case(handle)) {
            return Ok(found);
        }
        if page.next_page_token.is_empty() {
            return Err(format!("no account with handle '{handle}'"));
        }
        page_token = page.next_page_token;
    }
}

/// The roles after granting (or revoking) `role`, sorted.
pub fn roles_after(current: &[String], role: &str, grant: bool) -> Vec<String> {
    let mut roles: Vec<String> = current.iter().filter(|r| *r != role).cloned().collect();
    if grant {
        roles.push(role.to_string());
    }
    roles.sort();
    roles
}

pub async fn set_role(address: &str, handle: &str, role: &str, grant: bool) -> Result<(), String> {
    let mut identity = client(address).await?;
    let account = find(&mut identity, handle).await?;
    let roles = roles_after(&account.roles, role, grant);
    let request = SetAccountStateRequest { account_id: account.account_id, roles, set_roles: true, ..Default::default() };
    let updated = identity.set_account_state(request).await.map_err(|s| format!("set roles: {}", s.message()))?;
    let updated = updated.into_inner().account.unwrap_or_default();
    println!("{} (account {}): roles [{}]", updated.handle, updated.account_id, updated.roles.join(", "));
    Ok(())
}

/// Sets a password, for an account sc-admin issued (which has none) or when no admin can reset it. Ends the account's
/// web sessions.
pub async fn set_password(address: &str, handle: &str, password: String) -> Result<(), String> {
    let mut identity = client(address).await?;
    let account = find(&mut identity, handle).await?;
    let request =
        SetAccountStateRequest { account_id: account.account_id, new_password: Some(password), ..Default::default() };
    identity.set_account_state(request).await.map_err(|s| format!("set password: {}", s.message()))?;
    println!("{} (account {}): password set, web sessions ended", account.handle, account.account_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::roles_after;

    #[test]
    fn roles_are_added_once_and_removed() {
        let none: Vec<String> = Vec::new();
        assert_eq!(roles_after(&none, "admin", true), ["admin"]);
        assert_eq!(roles_after(&["admin".to_string()], "admin", true), ["admin"]);
        assert!(roles_after(&["admin".to_string()], "admin", false).is_empty());
    }
}

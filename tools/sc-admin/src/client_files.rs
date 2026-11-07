//! Files the game needs in its private directory.

use std::{fs, path::Path};

use serde_json::json;

use sc_runtime::state::{Account, State};

use crate::pki;

pub fn login_data(account: &Account, token: &str, host: &str, grpc_port: u16, hub_port: u16) -> serde_json::Value {
    json!({
        "username": account.handle,
        "token": token,
        "auth_token": token,
        "star_network": {
            "hostname": host,
            "port": hub_port,
            "servicesEndpoint": format!("https://{host}:{grpc_port}"),
        },
    })
}

pub fn write(
    out: &Path,
    data_dir: &Path,
    state: &State,
    handle: &str,
    host: &str,
    grpc_port: u16,
    hub_port: u16,
) -> Result<(), String> {
    let account = state.ensure_account(handle)?;
    let token = state.issue_token(&account);
    fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let write_file = |name: &str, text: String| {
        let path = out.join(name);
        fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
    };
    let login = login_data(&account, &token, host, grpc_port, hub_port);
    write_file("LoginData.json", serde_json::to_string_pretty(&login).unwrap_or_default())?;
    write_file("client.crt", pki::ca_pem(data_dir)?)
}

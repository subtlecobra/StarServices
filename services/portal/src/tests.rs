//! The portal against an in-memory stand-in for sc-identity's `IdentityInternal`, through the real router.

use std::{collections::HashMap, sync::Mutex};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use sc_api::internal::{
    identity_internal_server::{IdentityInternal, IdentityInternalServer},
    ChangePasswordResponse, GetAccountResponse, GetAccountsRequest, GetAccountsResponse, IssueGameTokenResponse,
    ListWebSessionsResponse, PasswordLoginResponse, RegisterAccountResponse, RevokeWebSessionResponse,
    SearchAccountsRequest, SearchAccountsResponse, SetAccountStateRequest, SetAccountStateResponse,
    VerifyWebSessionResponse,
};
use tokio::net::TcpListener;
use tonic::{transport::server::TcpIncoming, Request as RpcRequest, Response as RpcResponse};
use tower::ServiceExt;

use super::*;

#[derive(Default)]
struct FakeIdentity {
    /// handle, password, roles
    accounts: Mutex<Vec<(String, String, Vec<String>)>>,
    /// token to account index
    sessions: Mutex<HashMap<String, usize>>,
}

type Rpc<T> = Result<RpcResponse<T>, Status>;

fn account_id(index: usize) -> u32 {
    1_100_001 + index as u32
}

#[tonic::async_trait]
impl IdentityInternal for FakeIdentity {
    async fn get_accounts(&self, _: RpcRequest<GetAccountsRequest>) -> Rpc<GetAccountsResponse> {
        Err(Status::unimplemented(""))
    }

    async fn register_account(&self, request: RpcRequest<RegisterAccountRequest>) -> Rpc<RegisterAccountResponse> {
        let request = request.into_inner();
        if request.password.len() < 10 {
            return Err(Status::invalid_argument("The password needs at least 10 characters."));
        }
        let mut accounts = self.accounts.lock().unwrap();
        if accounts.iter().any(|(handle, _, _)| handle.eq_ignore_ascii_case(&request.handle)) {
            return Err(Status::already_exists("That handle or email cannot be used."));
        }
        let roles = if request.handle == "boss" { vec!["admin".to_string()] } else { Vec::new() };
        accounts.push((request.handle.clone(), request.password, roles));
        Ok(RpcResponse::new(RegisterAccountResponse { account_id: account_id(accounts.len() - 1), handle: request.handle }))
    }

    async fn password_login(&self, request: RpcRequest<PasswordLoginRequest>) -> Rpc<PasswordLoginResponse> {
        let request = request.into_inner();
        assert_eq!(request.client.as_ref().unwrap().ip, "203.0.113.7", "the address Envoy appended");
        let accounts = self.accounts.lock().unwrap();
        let index = accounts
            .iter()
            .position(|(handle, password, _)| handle.eq_ignore_ascii_case(&request.login) && *password == request.password)
            .ok_or_else(|| Status::unauthenticated("Wrong login or password."))?;
        let token = random_token();
        self.sessions.lock().unwrap().insert(token.clone(), index);
        Ok(RpcResponse::new(PasswordLoginResponse {
            session_token: token,
            account_id: account_id(index),
            handle: accounts[index].0.clone(),
            roles: accounts[index].2.clone(),
            expires_at: 0,
        }))
    }

    async fn verify_web_session(&self, request: RpcRequest<VerifyWebSessionRequest>) -> Rpc<VerifyWebSessionResponse> {
        let token = request.into_inner().token;
        let Some(index) = self.sessions.lock().unwrap().get(&token).copied() else {
            return Ok(RpcResponse::new(VerifyWebSessionResponse::default()));
        };
        let accounts = self.accounts.lock().unwrap();
        Ok(RpcResponse::new(VerifyWebSessionResponse {
            valid: true,
            account_id: account_id(index),
            handle: accounts[index].0.clone(),
            roles: accounts[index].2.clone(),
        }))
    }

    async fn revoke_web_session(&self, request: RpcRequest<RevokeWebSessionRequest>) -> Rpc<RevokeWebSessionResponse> {
        let revoked = self.sessions.lock().unwrap().remove(&request.into_inner().token).is_some();
        Ok(RpcResponse::new(RevokeWebSessionResponse { revoked }))
    }

    async fn list_web_sessions(&self, request: RpcRequest<ListWebSessionsRequest>) -> Rpc<ListWebSessionsResponse> {
        let request = request.into_inner();
        let sessions = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, index)| account_id(**index) == request.account_id)
            .map(|(token, _)| WebSession {
                session_id: token[..8].to_string(),
                ip: "<script>".into(),
                current: *token == request.current_token,
                ..Default::default()
            })
            .collect();
        Ok(RpcResponse::new(ListWebSessionsResponse { sessions }))
    }

    async fn change_password(&self, _: RpcRequest<ChangePasswordRequest>) -> Rpc<ChangePasswordResponse> {
        Ok(RpcResponse::new(ChangePasswordResponse {}))
    }

    async fn issue_game_token(&self, request: RpcRequest<IssueGameTokenRequest>) -> Rpc<IssueGameTokenResponse> {
        let id = request.into_inner().account_id;
        let accounts = self.accounts.lock().unwrap();
        let handle = accounts[(id - 1_100_001) as usize].0.clone();
        Ok(RpcResponse::new(IssueGameTokenResponse { token: format!("game-token-{id}"), handle, expires_at: 0 }))
    }

    async fn get_account(&self, request: RpcRequest<GetAccountRequest>) -> Rpc<GetAccountResponse> {
        let id = request.into_inner().account_id;
        let accounts = self.accounts.lock().unwrap();
        let (handle, _, roles) = accounts[(id - 1_100_001) as usize].clone();
        let account = AccountSummary { account_id: id, handle, roles, email: "e@example.com".into(), ..Default::default() };
        Ok(RpcResponse::new(GetAccountResponse { account: Some(account) }))
    }

    async fn search_accounts(&self, _: RpcRequest<SearchAccountsRequest>) -> Rpc<SearchAccountsResponse> {
        Err(Status::unimplemented(""))
    }

    async fn set_account_state(&self, _: RpcRequest<SetAccountStateRequest>) -> Rpc<SetAccountStateResponse> {
        Err(Status::unimplemented(""))
    }
}

async fn portal(registration_open: bool) -> Router {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tonic::transport::Server::builder().add_service(IdentityInternalServer::new(FakeIdentity::default()));
    tokio::spawn(server.serve_with_incoming(TcpIncoming::from(listener)));
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}")).unwrap();
    let app = App {
        identity: IdentityInternalClient::new(endpoint.connect_lazy()),
        game_host: "127.0.0.2".into(),
        game_names: vec!["alexandria.local".into()],
        game_grpc_port: 50051,
        game_hub_port: 8000,
        ca_pem: Some("-----BEGIN CERTIFICATE-----\n".into()),
        hops: 1,
        registration_open,
        mailer: mail::from_config("off").unwrap(),
        posts: Limiter::new(1000, Duration::from_secs(60)),
    };
    router(Arc::new(app))
}

/// A browser: keeps the cookies it is given, and sends them back.
#[derive(Default)]
struct Browser {
    cookies: HashMap<String, String>,
    /// The `Host` it reached the site by; 127.0.0.2:50051 when unset.
    host: Option<String>,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Browser {
    fn host(&self) -> &str {
        self.host.as_deref().unwrap_or("127.0.0.2:50051")
    }

    fn request(&self, method: &str, path: &str) -> axum::http::request::Builder {
        let cookies: Vec<String> = self.cookies.iter().map(|(name, value)| format!("{name}={value}")).collect();
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", self.host())
            .header("x-forwarded-for", "198.51.100.1, 203.0.113.7")
            .header("cookie", cookies.join("; "))
            .extension(ConnectInfo::<SocketAddr>("10.244.0.9:1234".parse().unwrap()))
    }

    async fn send(&mut self, router: &Router, request: Request<Body>) -> Reply {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for value in headers.get_all(header::SET_COOKIE) {
            let value = value.to_str().unwrap();
            let (pair, _) = value.split_once(';').unwrap();
            let (name, content) = pair.split_once('=').unwrap();
            if content.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.to_string(), content.to_string());
            }
        }
        let body = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        Reply { status, headers, body }
    }

    async fn get(&mut self, router: &Router, path: &str) -> Reply {
        let request = self.request("GET", path).body(Body::empty()).unwrap();
        self.send(router, request).await
    }

    fn csrf(&self) -> String {
        self.cookies.get(CSRF_COOKIE).cloned().unwrap_or_default()
    }

    async fn form(&mut self, router: &Router, path: &str, fields: &[(&str, &str)]) -> Reply {
        let mut body = format!("csrf={}", self.csrf());
        for (name, value) in fields {
            body.push_str(&format!("&{name}={value}"));
        }
        let request = self
            .request("POST", path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", format!("https://{}", self.host()))
            .body(Body::from(body))
            .unwrap();
        self.send(router, request).await
    }
}

fn set_cookies(reply: &Reply) -> Vec<String> {
    reply.headers.get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn a_player_registers_gets_the_game_login_and_logs_out() {
    let router = portal(true).await;
    let mut browser = Browser::default();

    let page = browser.get(&router, "/register").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains(&format!(r#"name="csrf" value="{}""#, browser.csrf())));
    assert!(set_cookies(&page)[0].ends_with("; Path=/; Secure; SameSite=Strict"));
    assert!(page.headers["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    assert_eq!(page.headers["cache-control"], "no-store");

    let planted = browser.csrf();
    let made = browser
        .form(&router, "/register", &[("handle", "Nova"), ("email", "n%40example.com"), ("password", "violet+tangerine")])
        .await;
    assert_eq!(made.status, StatusCode::SEE_OTHER, "{}", made.body);
    assert_eq!(made.headers["location"], "/play");
    let cookies = set_cookies(&made);
    assert!(cookies.iter().any(|c| c.starts_with("sc_session=") && c.ends_with("; HttpOnly; Secure; SameSite=Lax")));
    assert_ne!(browser.csrf(), planted, "logging in gives a new CSRF token");

    let account = browser.get(&router, "/account").await;
    assert_eq!(account.status, StatusCode::OK);
    assert!(account.body.contains("Nova") && account.body.contains("this browser"));
    assert!(account.body.contains("&lt;script&gt;") && !account.body.contains("<script>"));
    assert!(!account.body.contains(r#"href="/admin""#), "players get no admin link");

    let login = browser.form(&router, "/play/login-data", &[]).await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.headers["content-disposition"], "attachment; filename=\"LoginData.json\"");
    let data: serde_json::Value = serde_json::from_str(&login.body).unwrap();
    assert_eq!(data["username"], "Nova");
    assert_eq!(data["token"], "game-token-1100001");
    assert_eq!(data["star_network"]["servicesEndpoint"], "https://127.0.0.2:50051");
    assert_eq!(data["star_network"]["port"], 8000);
    assert_eq!(browser.get(&router, "/play/client.crt").await.status, StatusCode::OK);

    let out = browser.form(&router, "/logout", &[]).await;
    assert_eq!(out.status, StatusCode::SEE_OTHER);
    assert!(!browser.cookies.contains_key(SESSION_COOKIE));
    assert_eq!(browser.get(&router, "/account").await.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn posts_without_the_token_or_from_elsewhere_are_refused() {
    let router = portal(true).await;
    let mut browser = Browser::default();
    browser.get(&router, "/login").await;

    let mut forged = browser.request("POST", "/login").header("content-type", "application/x-www-form-urlencoded");
    forged = forged.header("origin", "https://127.0.0.2:50051");
    let reply = browser.send(&router, forged.body(Body::from("csrf=wrong&login=a&password=b")).unwrap()).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);

    let token = browser.csrf();
    let elsewhere = browser
        .request("POST", "/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("origin", "https://evil.example")
        .body(Body::from(format!("csrf={token}&login=a&password=b")))
        .unwrap();
    assert_eq!(browser.send(&router, elsewhere).await.status, StatusCode::FORBIDDEN);

    let big = browser
        .request("POST", "/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf={token}&login={}&password=b", "a".repeat(64 * 1024))))
        .unwrap();
    assert_eq!(browser.send(&router, big).await.status, StatusCode::PAYLOAD_TOO_LARGE);

    let wrong = browser.form(&router, "/login", &[("login", "nobody"), ("password", "whatever123")]).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.body.contains("Wrong login or password."));
}

#[tokio::test]
async fn the_api_needs_the_header_and_admins_get_their_link() {
    let router = portal(true).await;
    let mut browser = Browser::default();
    let me = browser.get(&router, "/api/me").await;
    let me: serde_json::Value = serde_json::from_str(&me.body).unwrap();
    assert!(me["user"].is_null());
    assert_eq!(me["csrf_token"], browser.csrf());

    let body = r#"{"handle":"boss","email":"b@example.com","password":"violet tangerine"}"#;
    let without = browser.request("POST", "/api/register").header("content-type", "application/json");
    assert_eq!(browser.send(&router, without.body(Body::from(body)).unwrap()).await.status, StatusCode::FORBIDDEN);
    let with = browser
        .request("POST", "/api/register")
        .header("content-type", "application/json")
        .header("x-csrf-token", browser.csrf())
        .body(Body::from(body))
        .unwrap();
    assert_eq!(browser.send(&router, with).await.status, StatusCode::CREATED);

    let me = browser.get(&router, "/api/me").await;
    let me: serde_json::Value = serde_json::from_str(&me.body).unwrap();
    assert_eq!(me["user"]["roles"][0], "admin");
    assert!(browser.get(&router, "/").await.body.contains(r#"href="/admin""#));
    assert!(serde_json::from_str::<serde_json::Value>(&browser.get(&router, "/api/sessions").await.body).unwrap()["sessions"]
        .is_array());
}

#[tokio::test]
async fn closed_registration_refuses_new_accounts() {
    let router = portal(false).await;
    let mut browser = Browser::default();
    assert!(browser.get(&router, "/register").await.body.contains("Registration is closed"));
    let made = browser.form(&router, "/register", &[("handle", "x"), ("email", "x"), ("password", "x")]).await;
    assert_eq!(made.status, StatusCode::FORBIDDEN);
}

/// LoginData.json names the backend as the site was reached: by a listed name when it is one, and by the game host for
/// any other name, so a forged `Host` never puts a name of its own in it.
#[tokio::test]
async fn the_login_data_uses_the_name_the_site_was_reached_by() {
    let router = portal(true).await;
    let mut browser = Browser { host: Some("Alexandria.local:8443".into()), ..Browser::default() };
    browser.get(&router, "/register").await;
    let made = browser
        .form(&router, "/register", &[("handle", "Vega"), ("email", "v%40example.com"), ("password", "violet+tangerine")])
        .await;
    assert_eq!(made.status, StatusCode::SEE_OTHER, "{}", made.body);

    let login = browser.form(&router, "/play/login-data", &[]).await;
    let data: serde_json::Value = serde_json::from_str(&login.body).unwrap();
    assert_eq!(data["star_network"]["hostname"], "alexandria.local");
    assert_eq!(data["star_network"]["servicesEndpoint"], "https://alexandria.local:50051");

    browser.host = Some("elsewhere.example:8443".into());
    let login = browser.form(&router, "/play/login-data", &[]).await;
    let data: serde_json::Value = serde_json::from_str(&login.body).unwrap();
    assert_eq!(data["star_network"]["hostname"], "127.0.0.2");
}

#[test]
fn a_host_header_loses_only_its_port() {
    assert_eq!(super::host_without_port("alexandria.local:8443"), "alexandria.local");
    assert_eq!(super::host_without_port("alexandria.local"), "alexandria.local");
    assert_eq!(super::host_without_port("[fd00::2]:8443"), "fd00::2");
    assert_eq!(super::host_without_port("fd00::2"), "fd00::2");
}

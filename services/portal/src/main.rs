//! sc-portal: the public website and JSON API, where players register, log in, manage their account and get what the
//! game needs to log in to this server. It keeps no data: accounts, passwords and sessions belong to sc-identity, which
//! it asks over `IdentityInternal`. Pages are rendered here, and the one stylesheet is in the binary.
//!
//! Pages: `/`, `/register`, `/login`, `POST /logout`, `/account` (`POST /account/password`,
//! `POST /account/sessions/revoke`), `/play` (`POST /play/login-data`, `/play/client.crt`), `/forgot`.
//! API, JSON in and out: `POST /api/register`, `POST /api/login`, `POST /api/logout`, `GET /api/me`,
//! `POST /api/password`, `GET /api/sessions`, `POST /api/sessions/revoke`, `POST /api/game-login`.
//! `/admin` is sc-admin-web's, routed to it by the gateway; this links there for admins.

mod html;
mod mail;
mod security;

use std::{net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use clap::Parser;
use sc_api::internal::{
    identity_internal_client::IdentityInternalClient, AccountSummary, ChangePasswordRequest, GetAccountRequest,
    IssueGameTokenRequest, ListWebSessionsRequest, PasswordLoginRequest, RegisterAccountRequest,
    RevokeWebSessionRequest, VerifyWebSessionRequest, WebClient, WebSession,
};
use sc_runtime::CommonArgs;
use serde::Deserialize;
use serde_json::json;
use tonic::{transport::Channel, Code, Status};
use tracing::{info, warn};

use security::{
    clear_session_cookie, cookie, csrf_cookie, csrf_ok, origin_ok, random_token, session_cookie, well_formed, Limiter,
    CSRF_COOKIE, SESSION_COOKIE,
};

/// Forms and JSON bodies are small; anything bigger is refused before it is read.
const MAX_BODY_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);
const CSS: &str = include_str!("assets/site.css");
const CA_FILE: &str = "ca.crt";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    /// sc-identity, which owns accounts and sessions.
    #[arg(long, env = "SCBACKEND_IDENTITY", default_value = "http://sc-identity:50051")]
    identity: String,
    /// The address the game reaches this backend at, for LoginData.json.
    #[arg(long, env = "SCBACKEND_GAME_HOST", default_value = "127.0.0.2")]
    game_host: String,
    /// Other names the game may reach the backend by (DNS names, comma separated). LoginData.json carries the name the
    /// site was reached by when it is one of these (or the game host), and the game host otherwise.
    #[arg(long, env = "SCBACKEND_GAME_NAMES", value_delimiter = ',', default_value = "")]
    game_names: Vec<String>,
    #[arg(long, env = "SCBACKEND_GAME_GRPC_PORT", default_value_t = 50051)]
    game_grpc_port: u16,
    #[arg(long, env = "SCBACKEND_GAME_HUB_PORT", default_value_t = 8000)]
    game_hub_port: u16,
    /// Proxies in front that append to X-Forwarded-For: 1 for the Envoy gateway; 0 to use the peer address.
    #[arg(long, env = "SCBACKEND_TRUSTED_PROXY_HOPS", default_value_t = 1)]
    trusted_proxy_hops: usize,
    /// `open`, or `closed` to stop new registrations.
    #[arg(long, env = "SCBACKEND_REGISTRATION", default_value = "open")]
    registration: String,
    /// Mail sender for verification and reset: `off`, the only one so far.
    #[arg(long, env = "SCBACKEND_MAIL", default_value = "off")]
    mail: String,
    /// Form and API posts per client address per minute, on this replica.
    #[arg(long, env = "SCBACKEND_PORTAL_POSTS_PER_MINUTE", default_value_t = 30)]
    posts_per_minute: u32,
}

pub struct App {
    identity: IdentityInternalClient<Channel>,
    game_host: String,
    game_names: Vec<String>,
    game_grpc_port: u16,
    game_hub_port: u16,
    ca_pem: Option<String>,
    hops: usize,
    registration_open: bool,
    mailer: Box<dyn mail::Mailer>,
    posts: Limiter,
}

/// The logged-in account of a request.
pub struct User {
    pub account_id: u32,
    pub handle: String,
    pub roles: Vec<String>,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.roles.iter().any(|role| role == "admin")
    }
}

/// What every handler knows about its request.
struct Ctx {
    user: Option<User>,
    session: Option<String>,
    csrf: String,
    /// The CSRF cookie was missing, so `csrf` is new and goes out with the response.
    new_csrf: bool,
    ip: String,
    user_agent: String,
}

impl Ctx {
    fn client(&self) -> Option<WebClient> {
        Some(WebClient { ip: self.ip.clone(), user_agent: self.user_agent.clone() })
    }

    /// A page, with the CSRF cookie when it is new.
    fn page(&self, status: StatusCode, title: &str, body: &str) -> Response {
        let page = Html(html::layout(title, self.user.as_ref(), &self.csrf, body));
        let mut response = (status, page).into_response();
        if self.new_csrf {
            if let Ok(value) = csrf_cookie(&self.csrf).parse() {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
        response
    }
}

fn identity_message(status: &Status) -> (StatusCode, String) {
    match status.code() {
        Code::InvalidArgument => (StatusCode::BAD_REQUEST, status.message().to_string()),
        Code::AlreadyExists => (StatusCode::CONFLICT, status.message().to_string()),
        Code::Unauthenticated => (StatusCode::UNAUTHORIZED, status.message().to_string()),
        Code::PermissionDenied => (StatusCode::FORBIDDEN, status.message().to_string()),
        Code::ResourceExhausted => (StatusCode::TOO_MANY_REQUESTS, status.message().to_string()),
        Code::NotFound => (StatusCode::NOT_FOUND, "Not found.".to_string()),
        _ => {
            warn!(code = ?status.code(), message = status.message(), "identity call failed");
            (StatusCode::SERVICE_UNAVAILABLE, "Something went wrong. Try again later.".to_string())
        }
    }
}

impl App {
    fn identity(&self) -> IdentityInternalClient<Channel> {
        self.identity.clone()
    }

    async fn ctx(&self, headers: &HeaderMap, peer: SocketAddr) -> Ctx {
        let session = cookie(headers, SESSION_COOKIE).filter(|token| well_formed(token));
        let user = match &session {
            Some(token) => {
                let request = VerifyWebSessionRequest { token: token.clone() };
                match self.identity().verify_web_session(request).await {
                    Ok(answer) => {
                        let answer = answer.into_inner();
                        answer.valid.then_some(User {
                            account_id: answer.account_id,
                            handle: answer.handle,
                            roles: answer.roles,
                        })
                    }
                    Err(status) => {
                        warn!(code = ?status.code(), "session check failed");
                        None
                    }
                }
            }
            None => None,
        };
        let existing = cookie(headers, CSRF_COOKIE).filter(|token| well_formed(token));
        let new_csrf = existing.is_none();
        let user_agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        Ctx {
            user,
            session,
            csrf: existing.unwrap_or_else(random_token),
            new_csrf,
            ip: security::client_ip(headers, peer, self.hops),
            user_agent,
        }
    }

    /// The checks before any state change: this site, the CSRF token, and the address's rate.
    fn guard(&self, ctx: &Ctx, headers: &HeaderMap, submitted: Option<&str>) -> Result<(), (StatusCode, &'static str)> {
        if !origin_ok(headers) || !csrf_ok(headers, submitted) {
            return Err((StatusCode::FORBIDDEN, "The form expired. Go back, reload the page and try again."));
        }
        if !self.posts.allow(&ctx.ip) {
            return Err((StatusCode::TOO_MANY_REQUESTS, "Too many requests. Wait a minute and try again."));
        }
        Ok(())
    }

    /// The name the game should use: the one this request reached the site by (its `Host`, without the port) when it
    /// is the game host or one of the game names, so a player who browsed to a DNS name gets that name; otherwise the
    /// game host. A name outside the list never reaches LoginData.json.
    pub fn game_host_for(&self, headers: &HeaderMap) -> &str {
        let requested = headers.get(header::HOST).and_then(|value| value.to_str().ok()).map(host_without_port);
        match requested {
            Some(name) if name.eq_ignore_ascii_case(&self.game_host) => &self.game_host,
            Some(name) => self.game_names.iter().find(|known| known.eq_ignore_ascii_case(name)).unwrap_or(&self.game_host),
            None => &self.game_host,
        }
    }

    fn login_data(&self, token: &str, handle: &str, host: &str) -> serde_json::Value {
        // As sc-admin's client-files writes it.
        json!({
            "username": handle,
            "token": token,
            "auth_token": token,
            "star_network": {
                "hostname": host,
                "port": self.game_hub_port,
                "servicesEndpoint": format!("https://{}:{}", host, self.game_grpc_port),
            },
        })
    }
}

/// A `Host` header's name: `name:port` and `[v6]:port` lose the port, and a bare IPv6 address stays whole.
fn host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

type Shared = State<Arc<App>>;
type Peer = ConnectInfo<SocketAddr>;

fn see_other(to: &str) -> Response {
    Redirect::to(to).into_response()
}

fn with_cookies(mut response: Response, cookies: &[String]) -> Response {
    for value in cookies {
        if let Ok(value) = value.parse() {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

// ---- Pages ----

async fn home(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    ctx.page(StatusCode::OK, "Home", &html::home(ctx.user.as_ref()))
}

async fn css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS)
}

async fn register_page(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if ctx.user.is_some() {
        return see_other("/account");
    }
    ctx.page(StatusCode::OK, "Register", &html::register(&ctx.csrf, None, "", "", app.registration_open))
}

#[derive(Deserialize)]
struct RegisterForm {
    #[serde(default)]
    csrf: String,
    handle: String,
    email: String,
    password: String,
}

/// Registers, then logs in, so the new player lands on the play page.
async fn register_post(
    State(app): Shared,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    let again = |status: StatusCode, error: &str| {
        let body = html::register(&ctx.csrf, Some(error), &form.handle, &form.email, app.registration_open);
        ctx.page(status, "Register", &body)
    };
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return again(status, error);
    }
    if !app.registration_open {
        return again(StatusCode::FORBIDDEN, "Registration is closed.");
    }
    match register_and_login(&app, &ctx, &form.handle, &form.email, &form.password).await {
        Ok(cookies) => with_cookies(see_other("/play"), &cookies),
        Err((status, error)) => again(status, &error),
    }
}

/// The cookies of the new session: the session, and a fresh CSRF token, so one planted before login is useless after.
async fn register_and_login(
    app: &App,
    ctx: &Ctx,
    handle: &str,
    email: &str,
    password: &str,
) -> Result<Vec<String>, (StatusCode, String)> {
    let request = RegisterAccountRequest {
        handle: handle.to_string(),
        email: email.to_string(),
        password: password.to_string(),
        client: ctx.client(),
    };
    app.identity().register_account(request).await.map_err(|status| identity_message(&status))?;
    log_in(app, ctx, handle, password).await
}

async fn log_in(app: &App, ctx: &Ctx, login: &str, password: &str) -> Result<Vec<String>, (StatusCode, String)> {
    let request = PasswordLoginRequest { login: login.to_string(), password: password.to_string(), client: ctx.client() };
    let answer = app.identity().password_login(request).await.map_err(|status| identity_message(&status))?;
    Ok(vec![session_cookie(&answer.into_inner().session_token), csrf_cookie(&random_token())])
}

async fn login_page(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if ctx.user.is_some() {
        return see_other("/account");
    }
    ctx.page(StatusCode::OK, "Log in", &html::login(&ctx.csrf, None, ""))
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    csrf: String,
    login: String,
    password: String,
}

async fn login_post(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Form(form): Form<LoginForm>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    let again = |status: StatusCode, error: &str| ctx.page(status, "Log in", &html::login(&ctx.csrf, Some(error), &form.login));
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return again(status, error);
    }
    match log_in(&app, &ctx, &form.login, &form.password).await {
        Ok(cookies) => with_cookies(see_other("/account"), &cookies),
        Err((status, error)) => again(status, &error),
    }
}

#[derive(Deserialize)]
struct CsrfForm {
    #[serde(default)]
    csrf: String,
}

async fn log_out(app: &App, ctx: &Ctx) {
    if let Some(token) = &ctx.session {
        let request = RevokeWebSessionRequest { token: token.clone(), ..Default::default() };
        if let Err(status) = app.identity().revoke_web_session(request).await {
            warn!(code = ?status.code(), "logout not recorded");
        }
    }
}

async fn logout_post(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Form(form): Form<CsrfForm>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return ctx.page(status, "Log out", &html::problem("Log out", error));
    }
    log_out(&app, &ctx).await;
    with_cookies(see_other("/"), &[clear_session_cookie()])
}

async fn account_view(app: &App, ctx: &Ctx, status: StatusCode, notice: Option<&str>, error: Option<&str>) -> Response {
    let Some(user) = &ctx.user else { return see_other("/login") };
    let loaded = load_account(app, ctx, user).await;
    match loaded {
        Ok((account, sessions)) => {
            let body = html::account(&ctx.csrf, &account, &sessions, notice, error, app.mailer.enabled());
            ctx.page(status, "Account", &body)
        }
        Err((status, error)) => ctx.page(status, "Account", &html::problem("Account", &error)),
    }
}

async fn load_account(app: &App, ctx: &Ctx, user: &User) -> Result<(AccountSummary, Vec<WebSession>), (StatusCode, String)> {
    let account = app
        .identity()
        .get_account(GetAccountRequest { account_id: user.account_id })
        .await
        .map_err(|status| identity_message(&status))?
        .into_inner()
        .account
        .unwrap_or_default();
    let request = ListWebSessionsRequest {
        account_id: user.account_id,
        current_token: ctx.session.clone().unwrap_or_default(),
    };
    let sessions = app.identity().list_web_sessions(request).await.map_err(|status| identity_message(&status))?;
    Ok((account, sessions.into_inner().sessions))
}

async fn account_page(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    account_view(&app, &ctx, StatusCode::OK, None, None).await
}

#[derive(Deserialize)]
struct PasswordForm {
    #[serde(default)]
    csrf: String,
    current_password: String,
    new_password: String,
}

async fn change_password(app: &App, ctx: &Ctx, current: &str, new: &str) -> Result<(), (StatusCode, String)> {
    let user = ctx.user.as_ref().ok_or((StatusCode::UNAUTHORIZED, "Log in first.".to_string()))?;
    let request = ChangePasswordRequest {
        account_id: user.account_id,
        current_password: current.to_string(),
        new_password: new.to_string(),
        keep_session_token: ctx.session.clone().unwrap_or_default(),
        client: ctx.client(),
    };
    app.identity().change_password(request).await.map_err(|status| identity_message(&status))?;
    Ok(())
}

async fn password_post(
    State(app): Shared,
    ConnectInfo(peer): Peer,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if ctx.user.is_none() {
        return see_other("/login");
    }
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return account_view(&app, &ctx, status, None, Some(error)).await;
    }
    match change_password(&app, &ctx, &form.current_password, &form.new_password).await {
        Ok(()) => account_view(&app, &ctx, StatusCode::OK, Some("Password changed. Other browsers are logged out."), None).await,
        Err((status, error)) => account_view(&app, &ctx, status, None, Some(&error)).await,
    }
}

#[derive(Deserialize)]
struct RevokeForm {
    #[serde(default)]
    csrf: String,
    session_id: String,
}

async fn revoke(app: &App, ctx: &Ctx, session_id: &str) -> Result<bool, (StatusCode, String)> {
    let user = ctx.user.as_ref().ok_or((StatusCode::UNAUTHORIZED, "Log in first.".to_string()))?;
    let request = RevokeWebSessionRequest {
        account_id: user.account_id,
        session_id: session_id.to_string(),
        ..Default::default()
    };
    let answer = app.identity().revoke_web_session(request).await.map_err(|status| identity_message(&status))?;
    Ok(answer.into_inner().revoked)
}

async fn revoke_post(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Form(form): Form<RevokeForm>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if ctx.user.is_none() {
        return see_other("/login");
    }
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return account_view(&app, &ctx, status, None, Some(error)).await;
    }
    match revoke(&app, &ctx, &form.session_id).await {
        Ok(true) => account_view(&app, &ctx, StatusCode::OK, Some("Session revoked."), None).await,
        Ok(false) => account_view(&app, &ctx, StatusCode::NOT_FOUND, None, Some("No such session.")).await,
        Err((status, error)) => account_view(&app, &ctx, status, None, Some(&error)).await,
    }
}

fn endpoint<'a>(app: &'a App, headers: &HeaderMap) -> html::Endpoint<'a> {
    html::Endpoint {
        host: app.game_host_for(headers),
        grpc_port: app.game_grpc_port,
        hub_port: app.game_hub_port,
        has_ca: app.ca_pem.is_some(),
    }
}

async fn play_page(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    let Some(user) = &ctx.user else { return see_other("/login") };
    ctx.page(StatusCode::OK, "Play", &html::play(&ctx.csrf, &user.handle, &endpoint(&app, &headers)))
}

async fn game_login(app: &App, ctx: &Ctx, headers: &HeaderMap) -> Result<serde_json::Value, (StatusCode, String)> {
    let user = ctx.user.as_ref().ok_or((StatusCode::UNAUTHORIZED, "Log in first.".to_string()))?;
    let answer = app
        .identity()
        .issue_game_token(IssueGameTokenRequest { account_id: user.account_id })
        .await
        .map_err(|status| identity_message(&status))?
        .into_inner();
    info!(account_id = user.account_id, "game login issued");
    Ok(app.login_data(&answer.token, &answer.handle, app.game_host_for(headers)))
}

/// A POST with the CSRF token, not a link: it hands out a credential.
async fn login_data_post(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Form(form): Form<CsrfForm>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if ctx.user.is_none() {
        return see_other("/login");
    }
    if let Err((status, error)) = app.guard(&ctx, &headers, Some(&form.csrf)) {
        return ctx.page(status, "Play", &html::problem("Play", error));
    }
    match game_login(&app, &ctx, &headers).await {
        Ok(data) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"LoginData.json\""),
            ],
            serde_json::to_string_pretty(&data).unwrap_or_default(),
        )
            .into_response(),
        Err((status, error)) => ctx.page(status, "Play", &html::problem("Play", &error)),
    }
}

async fn client_crt(State(app): Shared) -> Response {
    match &app.ca_pem {
        Some(pem) => (
            [
                (header::CONTENT_TYPE, "application/x-pem-file"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"client.crt\""),
            ],
            pem.clone(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no certificate").into_response(),
    }
}

async fn forgot_page(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    ctx.page(StatusCode::OK, "Forgot password", &html::forgot(app.mailer.enabled()))
}

async fn not_found(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    ctx.page(StatusCode::NOT_FOUND, "Not found", &html::problem("Not found", "There is no such page."))
}

// ---- JSON API: the same, with the CSRF token in `x-csrf-token` and errors as {"error": "..."} ----

fn api_error(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

#[allow(clippy::result_large_err)]
fn api_guard(app: &App, ctx: &Ctx, headers: &HeaderMap) -> Result<(), Response> {
    app.guard(ctx, headers, None).map_err(|(status, error)| api_error(status, error))
}

fn user_json(user: &User) -> serde_json::Value {
    json!({ "account_id": user.account_id, "handle": user.handle, "roles": user.roles })
}

#[derive(Deserialize)]
struct ApiRegister {
    handle: String,
    email: String,
    password: String,
}

async fn api_register(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Json(body): Json<ApiRegister>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    if !app.registration_open {
        return api_error(StatusCode::FORBIDDEN, "Registration is closed.");
    }
    match register_and_login(&app, &ctx, &body.handle, &body.email, &body.password).await {
        Ok(cookies) => with_cookies((StatusCode::CREATED, Json(json!({ "handle": body.handle }))).into_response(), &cookies),
        Err((status, error)) => api_error(status, &error),
    }
}

#[derive(Deserialize)]
struct ApiLogin {
    login: String,
    password: String,
}

async fn api_login(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Json(body): Json<ApiLogin>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    match log_in(&app, &ctx, &body.login, &body.password).await {
        Ok(cookies) => with_cookies(Json(json!({ "ok": true })).into_response(), &cookies),
        Err((status, error)) => api_error(status, &error),
    }
}

async fn api_logout(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    log_out(&app, &ctx).await;
    with_cookies(Json(json!({ "ok": true })).into_response(), &[clear_session_cookie()])
}

/// The account, and the CSRF token to send back, which also sets the cookie for a client that has none yet.
async fn api_me(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    let user = ctx.user.as_ref().map(user_json);
    let response = Json(json!({ "user": user, "csrf_token": ctx.csrf })).into_response();
    if ctx.new_csrf {
        return with_cookies(response, &[csrf_cookie(&ctx.csrf)]);
    }
    response
}

#[derive(Deserialize)]
struct ApiPassword {
    current_password: String,
    new_password: String,
}

async fn api_password(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Json(body): Json<ApiPassword>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    match change_password(&app, &ctx, &body.current_password, &body.new_password).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err((status, error)) => api_error(status, &error),
    }
}

async fn api_sessions(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    let Some(user) = &ctx.user else { return api_error(StatusCode::UNAUTHORIZED, "Log in first.") };
    match load_account(&app, &ctx, user).await {
        Ok((_, sessions)) => {
            let sessions: Vec<_> = sessions
                .iter()
                .map(|session| {
                    json!({
                        "session_id": session.session_id,
                        "created_at": session.created_at,
                        "last_seen_at": session.last_seen_at,
                        "expires_at": session.expires_at,
                        "ip": session.ip,
                        "user_agent": session.user_agent,
                        "current": session.current,
                    })
                })
                .collect();
            Json(json!({ "sessions": sessions })).into_response()
        }
        Err((status, error)) => api_error(status, &error),
    }
}

#[derive(Deserialize)]
struct ApiRevoke {
    session_id: String,
}

async fn api_revoke(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap, Json(body): Json<ApiRevoke>) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    match revoke(&app, &ctx, &body.session_id).await {
        Ok(revoked) => Json(json!({ "revoked": revoked })).into_response(),
        Err((status, error)) => api_error(status, &error),
    }
}

async fn api_game_login(State(app): Shared, ConnectInfo(peer): Peer, headers: HeaderMap) -> Response {
    let ctx = app.ctx(&headers, peer).await;
    if let Err(response) = api_guard(&app, &ctx, &headers) {
        return response;
    }
    match game_login(&app, &ctx, &headers).await {
        Ok(data) => Json(data).into_response(),
        Err((status, error)) => api_error(status, &error),
    }
}

async fn timeout(request: axum::http::Request<axum::body::Body>, next: middleware::Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "timed out").into_response(),
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/assets/site.css", get(css))
        .route("/register", get(register_page).post(register_post))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", post(logout_post))
        .route("/forgot", get(forgot_page))
        .route("/account", get(account_page))
        .route("/account/password", post(password_post))
        .route("/account/sessions/revoke", post(revoke_post))
        .route("/play", get(play_page))
        .route("/play/login-data", post(login_data_post))
        .route("/play/client.crt", get(client_crt))
        .route("/api/register", post(api_register))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/me", get(api_me))
        .route("/api/password", post(api_password))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/revoke", post(api_revoke))
        .route("/api/game-login", post(api_game_login))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(timeout))
        .layer(middleware::from_fn(security::headers))
        .with_state(app)
}

fn app(args: &Args) -> Result<App, String> {
    let endpoint = tonic::transport::Endpoint::from_shared(args.identity.clone())
        .map_err(|e| format!("--identity: {e}"))?
        .timeout(IDENTITY_TIMEOUT)
        .connect_timeout(IDENTITY_TIMEOUT);
    let ca_path = args.common.data.join(CA_FILE);
    let ca_pem = std::fs::read_to_string(&ca_path).ok();
    if ca_pem.is_none() {
        warn!(path = %ca_path.display(), "no CA certificate: client.crt cannot be downloaded");
    }
    let registration_open = match args.registration.as_str() {
        "open" => true,
        "closed" => false,
        other => return Err(format!("SCBACKEND_REGISTRATION: '{other}' is neither open nor closed")),
    };
    Ok(App {
        identity: IdentityInternalClient::new(endpoint.connect_lazy()),
        game_host: args.game_host.clone(),
        game_names: args.game_names.iter().map(|name| name.trim().to_ascii_lowercase()).filter(|name| !name.is_empty()).collect(),
        game_grpc_port: args.game_grpc_port,
        game_hub_port: args.game_hub_port,
        ca_pem,
        hops: args.trusted_proxy_hops,
        registration_open,
        mailer: mail::from_config(&args.mail)?,
        posts: Limiter::new(args.posts_per_minute, Duration::from_secs(60)),
    })
}

async fn serve(args: Args) -> Result<(), String> {
    let app = Arc::new(app(&args)?);
    let listener = tokio::net::TcpListener::bind(args.listen).await.map_err(|e| format!("bind {}: {e}", args.listen))?;
    info!(listen = %args.listen, identity = %args.identity, mail = %args.mail, "portal listening");
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests;

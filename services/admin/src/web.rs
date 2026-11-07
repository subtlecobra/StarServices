//! The admin site: routes, the session and CSRF guard, and the pages.
//!
//! Every request needs an `sc_session` cookie that sc-identity accepts for an account with the `admin` role. Without a
//! session the browser goes to the portal's login; with one that is not an admin's it gets a bare 403 and no data.
//! Every state-changing request needs the `sc_csrf` cookie's value back in its `x-csrf-token` form field or header.
//!
//! An action is audited (audit.rs), then sent to the service that owns the data, with a key made from the form's
//! request id, so a resubmitted form changes nothing more.

use std::{
    collections::HashMap,
    fmt::Write,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    extract::{Path, RawQuery, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Extension, Router,
};
use ring::rand::{SecureRandom, SystemRandom};
use sc_economy::ledger::{AUEC_LEDGER, REC_LEDGER};
use sc_proto::{
    internal::{
        AccountSummary, AdminAddItemRequest, AdminInventory, AdminInventoryItem, AdminMoveItemRequest,
        AdminRemoveItemRequest, CreateRentalRequest, EndRentalRequest, ExtendRentalRequest, GrantPurchaseRequest,
        LedgerAdjustRequest, PutEntityDocumentRequest, RevokeEntitlementRequest, SearchAccountsRequest,
        SetAccountStateRequest, SetRentalExpiryRequest, StoredEntityDocument,
    },
    services::{
        entitlement::v2::{Entitlement, EntitlementFilter, EntitlementLogAction, EntitlementSourceType, EntitlementStatus},
        entity_document::v1::{
            entity_document_body::Class, player_spawn_location_document::{PlayerSpawnLocation, SpawnInfo},
            EntityDocumentBody, PlayerHomeLocationDocument, PlayerSpawnLocationDocument,
        },
        entitygraph::v1::InventoryNodeProperties,
        ledger::v1::Currency,
        presence::v1::Status as PresenceStatus,
        rental::v1::{Rental, RentalStatus},
    },
};
use serde_json::{json, Value};
use tonic::Status;
use tracing::warn;

use crate::{
    audit::{AuditLog, Entry, OK},
    backend::{Identity, Owners},
    html::{account_tabs, amount, escape, notice, query_value, table, time, Frame, CSS},
    places::places,
};

pub const SESSION_COOKIE: &str = "sc_session";
pub const CSRF_COOKIE: &str = "sc_csrf";
pub const CSRF_FIELD: &str = "x-csrf-token";
pub const ADMIN_ROLE: &str = "admin";
const FORM_LIMIT: usize = 64 * 1024;
const DAY_MS: i64 = 86_400_000;
/// Far beyond any price, and well inside a BIGINT.
const MAX_ADJUSTMENT: i64 = 1_000_000_000_000;
const MAX_RENTAL_DAYS: u32 = 3_650;
const MAX_REASON: usize = 500;
/// The `Type` of a vehicle class's attach definition, as `entitlements.item_type` stores it. Assumed from the game
/// data's naming; rentals are listed as ships whatever their type.
const VEHICLE_ITEM_TYPE: &str = "NOITEM_Vehicle";
const VEHICLE_STATES_SHOWN: usize = 20;

pub struct App {
    pub identity: Arc<dyn Identity>,
    pub owners: Arc<dyn Owners>,
    pub audit: Arc<dyn AuditLog>,
    /// Where the site is served, such as `/admin`.
    pub prefix: String,
    /// The portal's login page.
    pub login: String,
    pub secure_cookies: bool,
}

type Shared = Arc<App>;

/// The admin making the request, as sc-identity vouched for them.
#[derive(Clone, Debug)]
pub struct Admin {
    pub account_id: u32,
    pub handle: String,
    pub csrf: String,
}

/// A state-changing request's form fields, read and checked by the guard.
#[derive(Clone, Debug, Default)]
pub struct Form(pub HashMap<String, String>);

impl Form {
    fn get(&self, name: &str) -> &str {
        self.0.get(name).map_or("", |value| value.trim())
    }
}

/// A location by its `LocationId`, named from the home locations of the game build when it is one (`places`).
fn place_name(id: u32) -> String {
    match places().iter().find(|place| place.id == id) {
        Some(place) => format!("{} ({}, {id})", place.name, place.system),
        None => format!("location {id}"),
    }
}

/// A random id for a form, 32 hex digits.
pub fn request_id() -> String {
    hex(&random::<16>())
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).expect("system randomness");
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

fn parse_pairs(text: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(text).into_owned().collect()
}

fn valid_request_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "Forbidden").into_response()
}

/// Lets through only an admin's requests, and only state changes that carry the CSRF token.
async fn guard(State(app): State<Shared>, mut request: Request, next: Next) -> Response {
    let headers = request.headers().clone();
    let target = request.uri().path_and_query().map_or_else(|| app.prefix.clone(), |path| path.to_string());
    let to_login = || Redirect::to(&format!("{}?next={}", app.login, query_value(&target))).into_response();
    let Some(token) = cookie(&headers, SESSION_COOKIE) else { return to_login() };
    let session = match app.identity.verify_session(token).await {
        Ok(session) => session,
        Err(status) => {
            warn!(%status, "session not verified");
            return (StatusCode::SERVICE_UNAVAILABLE, "The account service is not answering.").into_response();
        }
    };
    if !session.valid {
        return to_login();
    }
    if !session.roles.iter().any(|role| role == ADMIN_ROLE) {
        return forbidden();
    }
    let existing = cookie(&headers, CSRF_COOKIE).map(str::to_string);
    let changes = !matches!(*request.method(), Method::GET | Method::HEAD);
    if changes {
        let Some(expected) = &existing else { return forbidden() };
        let body = std::mem::replace(request.body_mut(), Body::empty());
        let Ok(bytes) = to_bytes(body, FORM_LIMIT).await else {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Form too large").into_response();
        };
        let form = parse_pairs(&bytes);
        let sent = headers
            .get(CSRF_FIELD)
            .and_then(|value| value.to_str().ok())
            .or_else(|| form.get(CSRF_FIELD).map(String::as_str))
            .unwrap_or_default();
        if !same(sent, expected) {
            return forbidden();
        }
        request.extensions_mut().insert(Form(form));
    }
    let csrf = existing.clone().unwrap_or_else(|| hex(&random::<32>()));
    request.extensions_mut().insert(Admin { account_id: session.account_id, handle: session.handle, csrf: csrf.clone() });
    let mut response = next.run(request).await;
    if existing.is_none() {
        let secure = if app.secure_cookies { "; Secure" } else { "" };
        let value = format!("{CSRF_COOKIE}={csrf}; Path=/; SameSite=Strict{secure}");
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert("x-frame-options", HeaderValue::from_static("DENY"));
    response
}

async fn stylesheet() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS).into_response()
}

pub fn router(app: Shared) -> Router {
    let p = app.prefix.clone();
    let guarded = Router::new()
        .route(&format!("{p}/"), get(search))
        .route(&p, get(search))
        .route(&format!("{p}/audit"), get(audit_all))
        .route(&format!("{p}/accounts/{{id}}"), get(overview))
        .route(&format!("{p}/accounts/{{id}}/state"), axum::routing::post(account_state))
        .route(&format!("{p}/accounts/{{id}}/money"), get(money).post(money_adjust))
        .route(&format!("{p}/accounts/{{id}}/assets"), get(assets))
        .route(&format!("{p}/accounts/{{id}}/assets/grant"), axum::routing::post(asset_grant))
        .route(&format!("{p}/accounts/{{id}}/assets/revoke"), axum::routing::post(asset_revoke))
        .route(&format!("{p}/accounts/{{id}}/ships"), get(ships))
        .route(&format!("{p}/accounts/{{id}}/ships/end"), axum::routing::post(rental_end))
        .route(&format!("{p}/accounts/{{id}}/ships/extend"), axum::routing::post(rental_extend))
        .route(&format!("{p}/accounts/{{id}}/location"), get(location))
        .route(&format!("{p}/accounts/{{id}}/location/home"), axum::routing::post(set_home))
        .route(&format!("{p}/accounts/{{id}}/location/spawn"), axum::routing::post(set_spawn))
        .route(&format!("{p}/accounts/{{id}}/inventory"), get(inventory))
        .route(&format!("{p}/accounts/{{id}}/inventory/move"), axum::routing::post(item_move))
        .route(&format!("{p}/accounts/{{id}}/inventory/add"), axum::routing::post(item_add))
        .route(&format!("{p}/accounts/{{id}}/inventory/remove"), axum::routing::post(item_remove))
        .route(&format!("{p}/accounts/{{id}}/audit"), get(audit_account))
        .layer(middleware::from_fn_with_state(app.clone(), guard));
    Router::new()
        .route(&format!("{p}/static/admin.css"), get(stylesheet))
        .merge(guarded)
        .with_state(app)
}

fn html(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

fn frame<'a>(app: &'a App, admin: &'a Admin) -> Frame<'a> {
    Frame { prefix: &app.prefix, admin_handle: &admin.handle, csrf: &admin.csrf }
}

fn query(raw: Option<String>) -> HashMap<String, String> {
    raw.map(|raw| parse_pairs(raw.as_bytes())).unwrap_or_default()
}

/// The notice a redirect after an action carries.
fn flash(query: &HashMap<String, String>) -> String {
    match query.get("notice") {
        Some(text) => notice(text, query.get("error").is_some()),
        None => String::new(),
    }
}

fn failed(what: &str, status: &Status) -> String {
    notice(&format!("{what}: {}", status.message()), true)
}

/// The account a page is about, with its player when sc-identity knows it.
struct Target {
    account: AccountSummary,
    geid: Option<u64>,
}

async fn target(app: &App, admin: &Admin, account_id: u32) -> Result<Target, Response> {
    let page = |title: &str, text: String| html(frame(app, admin).page(title, &text));
    let account = match app.identity.account(account_id).await {
        Ok(Some(account)) => account,
        Ok(None) => {
            let mut response = page("Not found", notice(&format!("No account {account_id}."), true));
            *response.status_mut() = StatusCode::NOT_FOUND;
            return Err(response);
        }
        Err(status) => return Err(page("Account", failed("sc-identity", &status))),
    };
    let geid = (account.geid != 0).then_some(account.geid);
    Ok(Target { account, geid })
}

/// The player of an account, as sc-identity has it (`AccountSummary.geid`), for an action.
async fn player(app: &App, account_id: u32) -> Option<u64> {
    match app.identity.account(account_id).await {
        Ok(account) => account.map(|account| account.geid).filter(|geid| *geid != 0),
        Err(status) => {
            warn!(%status, account_id, "account not read");
            None
        }
    }
}

fn account_header(app: &App, target: &Target, tab: &str) -> String {
    let account = &target.account;
    let player = target.geid.map_or_else(|| "unknown".to_string(), |geid| geid.to_string());
    format!(
        r#"<p><strong>{}</strong> · account {} · player <span class="mono">{}</span>{}</p>{}"#,
        escape(&account.handle),
        account.account_id,
        player,
        if account.disabled { r#" · <span class="warn">disabled</span>"# } else { "" },
        account_tabs(&app.prefix, account.account_id, tab)
    )
}

fn no_player() -> String {
    notice("sc-identity knows no player for this account, so there is nothing to show here.", true)
}

/// Where the player is, as far as the owners know.
enum Playing {
    /// Not on a shard. `menu` when presence says the client is online.
    No { menu: bool },
    /// On a shard: the DGS holds the player's live state.
    Yes { shard: String, location: u64 },
    /// sc-hub did not answer, so it is not known.
    Unknown(String),
}

async fn playing(app: &App, account_id: u32, geid: u64) -> Playing {
    match app.owners.player_shard(geid).await {
        Ok(Some(shard)) => return Playing::Yes { shard: shard.shard_id, location: shard.location_id },
        Ok(None) => {}
        Err(status) => return Playing::Unknown(status.message().to_string()),
    }
    let menu = match app.owners.presence(account_id).await {
        Ok(Some(presence)) => presence.presence.is_some_and(|presence| {
            !matches!(
                PresenceStatus::try_from(presence.status),
                Ok(PresenceStatus::Offline | PresenceStatus::Unspecified) | Err(_)
            )
        }),
        _ => false,
    };
    Playing::No { menu }
}

fn playing_text(playing: &Playing) -> String {
    match playing {
        Playing::No { menu: false } => r#"<span class="offline">offline</span>"#.into(),
        Playing::No { menu: true } => r#"<span class="online">online</span>, in the menu (on no shard)"#.into(),
        Playing::Yes { shard, location } => format!(
            r#"<span class="online">playing</span> on shard <span class="mono">{}</span> at {}"#,
            escape(shard),
            escape(&place_name(*location as u32))
        ),
        Playing::Unknown(error) => format!("unknown (sc-hub: {})", escape(error)),
    }
}

/// The redirect after an action, back to `page` with the outcome.
fn back(app: &App, account_id: u32, page: &str, result: Result<String, String>) -> Response {
    let (text, error) = match result {
        Ok(text) => (text, ""),
        Err(text) => (text, "&error=1"),
    };
    let to = format!("{}/accounts/{account_id}{page}?notice={}{error}", app.prefix, query_value(&text));
    Redirect::to(&to).into_response()
}

/// What an owner said, before and after, for the audit.
type Change = (Option<Value>, Option<Value>);

/// Audits an action, then runs it. Nothing runs unless its audit row is written first.
async fn act<F>(app: &App, admin: &Admin, form: &Form, entry: Entry, run: F) -> Result<String, String>
where
    F: std::future::Future<Output = Result<(String, Change), Status>>,
{
    let request_id = form.get("request_id").to_string();
    if !valid_request_id(&request_id) {
        return Err("The form has no request id: reload the page.".into());
    }
    let entry = Entry { request_id: request_id.clone(), actor_account_id: admin.account_id, actor_handle: admin.handle.clone(), ..entry };
    if let Err(error) = app.audit.begin(&entry).await {
        warn!(%error, "audit not written");
        return Err("The audit could not be written, so nothing was done.".into());
    }
    let (outcome, result, change) = match run.await {
        Ok((text, change)) => (OK.to_string(), Ok(text), change),
        Err(status) => (format!("error: {}", status.message()), Err(format!("{}: {}", entry.owner, status.message())), (None, None)),
    };
    if let Err(error) = app.audit.finish(&request_id, &outcome, change.0.as_ref(), change.1.as_ref()).await {
        warn!(%error, %request_id, "audit outcome not written");
    }
    result
}

/// The key every owner applies once, from the form's request id.
fn key(form: &Form) -> String {
    format!("admin:{}", form.get("request_id"))
}

fn reason(form: &Form) -> Result<String, String> {
    let reason = form.get("reason");
    if reason.is_empty() || reason.len() > MAX_REASON {
        return Err(format!("A reason of 1 to {MAX_REASON} characters is needed."));
    }
    Ok(reason.to_string())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------------------------------------------------
// Accounts

async fn search(State(app): State<Shared>, Extension(admin): Extension<Admin>, RawQuery(raw): RawQuery) -> Response {
    let query = query(raw);
    let q = query.get("q").cloned().unwrap_or_default();
    let page_token = query.get("page").cloned().unwrap_or_default();
    let frame = frame(&app, &admin);
    let mut body = format!(
        r#"<form method="get" action="{}/"><input name="q" value="{}" placeholder="Handle, email or account id" autofocus size="40"> <button>Search</button></form>"#,
        app.prefix,
        escape(&q)
    );
    match app.identity.search_accounts(SearchAccountsRequest { query: q.clone(), limit: 50, page_token }).await {
        Ok(found) => {
            let rows: Vec<Vec<String>> = found
                .accounts
                .iter()
                .map(|account| {
                    vec![
                        format!(r#"<a href="{}/accounts/{id}">{id}</a>"#, app.prefix, id = account.account_id),
                        escape(&account.handle),
                        escape(&account.email),
                        escape(&account.roles.join(", ")),
                        time(account.created_at),
                        if account.disabled { "disabled".into() } else { String::new() },
                    ]
                })
                .collect();
            body.push_str(&table(&["Account", "Handle", "Email", "Roles", "Created", "State"], &rows));
            if !found.next_page_token.is_empty() {
                let _ = write!(
                    body,
                    r#"<p><a href="{}/?q={}&page={}">Next page</a></p>"#,
                    app.prefix,
                    query_value(&q),
                    query_value(&found.next_page_token)
                );
            }
        }
        Err(status) => body.push_str(&failed("sc-identity", &status)),
    }
    html(frame.page("Accounts", &body))
}

async fn overview(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let account = &target.account;
    let mut body = flash(&query(raw)) + &account_header(&app, &target, "");
    let status = match target.geid {
        Some(geid) => playing_text(&playing(&app, id, geid).await),
        None => "unknown".into(),
    };
    let _ = write!(
        body,
        "<dl><dt>Handle</dt><dd>{}</dd><dt>Email</dt><dd>{}</dd><dt>Roles</dt><dd>{}</dd><dt>Created</dt><dd>{}</dd>\
         <dt>State</dt><dd>{}</dd><dt>Now</dt><dd>{status}</dd></dl>",
        escape(&account.handle),
        escape(&account.email),
        escape(&account.roles.join(", ")),
        time(account.created_at),
        if account.disabled { "disabled" } else { "enabled" },
    );
    body.push_str("<h2>Account</h2><fieldset><legend>Access</legend>");
    let toggle = if account.disabled { ("enable", "Enable account") } else { ("disable", "Disable account") };
    body.push_str(&frame.form(
        &format!("/accounts/{id}/state"),
        &format!(r#"<input type="hidden" name="op" value="{}">"#, toggle.0),
        toggle.1,
    ));
    body.push_str("</fieldset><fieldset><legend>Roles</legend>");
    body.push_str(&frame.form(
        &format!("/accounts/{id}/state"),
        &format!(
            r#"<input type="hidden" name="op" value="roles"><label>Roles, comma separated <input name="roles" value="{}" size="30"></label>"#,
            escape(&account.roles.join(", "))
        ),
        "Set roles",
    ));
    body.push_str("</fieldset><fieldset><legend>Password reset</legend>");
    body.push_str(&frame.form(
        &format!("/accounts/{id}/state"),
        r#"<input type="hidden" name="op" value="password"><label>New password <input type="password" name="new_password" autocomplete="new-password"></label>"#,
        "Reset password",
    ));
    body.push_str(r#"<p class="muted">Disabling and a password reset end the account's web sessions.</p></fieldset>"#);
    html(frame.page(&format!("Account {}", account.handle), &body))
}

async fn account_state(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let mut request = SetAccountStateRequest { account_id: id, ..Default::default() };
    let detail;
    match form.get("op") {
        op @ ("disable" | "enable") => {
            if op == "disable" && id == admin.account_id {
                return back(&app, id, "", Err("You cannot disable your own account.".into()));
            }
            request.disabled = Some(op == "disable");
            detail = json!({ "disabled": op == "disable" });
        }
        "roles" => {
            let mut roles: Vec<String> = form
                .get("roles")
                .split(',')
                .map(|role| role.trim().to_string())
                .filter(|role| !role.is_empty())
                .collect();
            roles.sort();
            roles.dedup();
            if id == admin.account_id && !roles.iter().any(|role| role == ADMIN_ROLE) {
                return back(&app, id, "", Err("You cannot take the admin role from yourself.".into()));
            }
            detail = json!({ "roles": roles });
            request.roles = roles;
            request.set_roles = true;
        }
        "password" => {
            let password = form.get("new_password");
            if password.is_empty() {
                return back(&app, id, "", Err("A new password is needed.".into()));
            }
            request.new_password = Some(password.to_string());
            // Never the password itself.
            detail = json!({ "password_reset": true });
        }
        _ => return back(&app, id, "", Err("Unknown change.".into())),
    }
    let before = app.identity.account(id).await.ok().flatten();
    let entry = Entry {
        action: format!("account.{}", form.get("op")),
        owner: "sc-identity".into(),
        target_account_id: Some(id),
        detail,
        ..Default::default()
    };
    let result = act(&app, &admin, &form, entry, async {
        let after = app.identity.set_account_state(request).await?;
        let summary = |account: &AccountSummary| json!({ "disabled": account.disabled, "roles": account.roles });
        Ok(("Account updated.".to_string(), (before.as_ref().map(summary), Some(summary(&after)))))
    })
    .await;
    back(&app, id, "", result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Money

fn currency_name(currency: i32) -> &'static str {
    match Currency::try_from(currency) {
        Ok(Currency::Auec) => "aUEC",
        Ok(Currency::Rec) => "REC",
        Ok(Currency::Uec) => "UEC",
        Ok(Currency::Mer) => "MER",
        _ => "?",
    }
}

async fn money(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let query = query(raw);
    let mut body = flash(&query) + &account_header(&app, &target, "/money");
    let Some(geid) = target.geid else { return html(frame.page("Money", &(body + &no_player()))) };
    body.push_str("<h2>Balances</h2>");
    match app.owners.balances(geid).await {
        Ok(balances) => {
            let rows: Vec<Vec<String>> = balances
                .iter()
                .map(|balance| {
                    vec![
                        escape(&balance.ledger_name),
                        currency_name(balance.currency).into(),
                        amount(balance.amount),
                        balance.version.to_string(),
                        time(balance.updated_at_ms),
                    ]
                })
                .collect();
            body.push_str(&table(&["Ledger", "Currency", "Balance", "Version", "Updated"], &rows));
            if balances.is_empty() {
                body.push_str(r#"<p class="muted">The ledgers open with their opening balance on first use.</p>"#);
            }
        }
        Err(status) => body.push_str(&failed("sc-ledger", &status)),
    }
    body.push_str("<h2>Credit or debit</h2><fieldset>");
    body.push_str(&frame.form(
        &format!("/accounts/{id}/money"),
        &format!(
            r#"<select name="ledger"><option value="{AUEC_LEDGER}">aUEC</option><option value="{REC_LEDGER}">REC</option></select>
<select name="direction"><option value="credit">credit</option><option value="debit">debit</option></select>
<input name="amount" type="number" min="1" max="{MAX_ADJUSTMENT}" required placeholder="Amount">
<input name="reason" required maxlength="{MAX_REASON}" size="40" placeholder="Reason">"#
        ),
        "Apply",
    ));
    body.push_str(r#"<p class="muted">Applies at once: the ledger is the authority, and a playing client is told its new balance.</p></fieldset>"#);
    body.push_str("<h2>Transactions</h2>");
    let before_ms = query.get("before").and_then(|before| before.parse().ok()).unwrap_or(0);
    match app.owners.operations(geid, before_ms).await {
        Ok(operations) => {
            let rows: Vec<Vec<String>> = operations
                .iter()
                .map(|operation| {
                    vec![
                        time(operation.created_at_ms),
                        escape(&operation.ledger_name),
                        escape(&operation.state),
                        amount(operation.change),
                        operation.balance_after.map_or_else(String::new, amount),
                        escape(&operation.reason),
                        escape(&operation.actor),
                        format!(r#"<code>{}</code>"#, escape(&operation.key)),
                    ]
                })
                .collect();
            body.push_str(&table(&["When", "Ledger", "Kind", "Change", "Balance after", "Reason", "By", "Key"], &rows));
            if let Some(last) = operations.last().filter(|_| operations.len() >= 100) {
                let _ = write!(body, r#"<p><a href="{}/accounts/{id}/money?before={}">Older</a></p>"#, app.prefix, last.created_at_ms);
            }
            body.push_str(r#"<p class="muted">Keyed operations only: purchases, refunds and adjustments. The opening balance is not one.</p>"#);
        }
        Err(status) => body.push_str(&failed("sc-ledger", &status)),
    }
    html(frame.page("Money", &body))
}

async fn money_adjust(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let ledger = form.get("ledger");
    if ![AUEC_LEDGER, REC_LEDGER].contains(&ledger) {
        return back(&app, id, "/money", Err("Unknown ledger.".into()));
    }
    let size = match form.get("amount").parse::<i64>() {
        Ok(size) if (1..=MAX_ADJUSTMENT).contains(&size) => size,
        _ => return back(&app, id, "/money", Err(format!("The amount must be from 1 to {MAX_ADJUSTMENT}."))),
    };
    let delta = match form.get("direction") {
        "credit" => size,
        "debit" => -size,
        _ => return back(&app, id, "/money", Err("Credit or debit?".into())),
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/money", Err(text)),
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/money", Err("No player for this account.".into()));
    };
    let entry = Entry {
        action: "money.adjust".into(),
        owner: "sc-ledger".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "ledger": ledger, "delta": delta, "reason": reason }),
        ..Default::default()
    };
    let request = LedgerAdjustRequest {
        key: key(&form),
        owner_geid: geid,
        account_id: id,
        ledger_name: ledger.to_string(),
        delta,
        reason,
        actor: format!("admin:{}:{}", admin.account_id, admin.handle),
    };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.adjust(request).await?;
        if !done.replayed {
            app.owners.funds_changed(id, geid, ledger, delta, done.version).await;
        }
        let text = format!("{ledger}: {} → {}{}", amount(done.balance_before), amount(done.balance_after), if done.replayed { " (already applied)" } else { "" });
        Ok((text, (Some(json!({ "balance": done.balance_before })), Some(json!({ "balance": done.balance_after, "version": done.version })))))
    })
    .await;
    back(&app, id, "/money", result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Assets

fn status_name(status: i32) -> &'static str {
    match EntitlementStatus::try_from(status) {
        Ok(EntitlementStatus::Packaged) => "packaged",
        Ok(EntitlementStatus::Undelivered) => "claimed, undelivered",
        Ok(EntitlementStatus::Delivered) => "delivered",
        Ok(EntitlementStatus::Revoked) => "revoked",
        Ok(EntitlementStatus::Failed) => "failed",
        _ => "?",
    }
}

fn source_name(source: i32) -> &'static str {
    match EntitlementSourceType::try_from(source) {
        Ok(EntitlementSourceType::Platform) => "platform",
        Ok(EntitlementSourceType::IngamePersistent) => "in game",
        Ok(EntitlementSourceType::IngameNonpersistent) => "in game, not kept",
        Ok(EntitlementSourceType::Rental) => "rental",
        Ok(EntitlementSourceType::ArenaCommander) => "Arena Commander",
        _ => "?",
    }
}

fn log_action_name(action: i32) -> String {
    EntitlementLogAction::try_from(action)
        .map(|action| action.as_str_name().trim_start_matches("ENTITLEMENT_LOG_ACTION_").to_lowercase())
        .unwrap_or_else(|_| action.to_string())
}

fn seconds_ms(timestamp: Option<&prost_types::Timestamp>) -> i64 {
    timestamp.map_or(0, |t| t.seconds * 1000 + i64::from(t.nanos) / 1_000_000)
}

fn entitlement_rows(frame: &Frame, id: u32, entitlements: &[Entitlement], revoke: bool) -> Vec<Vec<String>> {
    entitlements
        .iter()
        .map(|entitlement| {
            let name = entitlement.metadata.as_ref().map_or("", |metadata| metadata.name.as_str());
            let mut row = vec![
                escape(name),
                format!("<code>{}</code>", escape(&entitlement.entity_class_guid)),
                source_name(entitlement.source_type).into(),
                status_name(entitlement.status).into(),
                entitlement.entity_geid.map_or_else(String::new, |geid| geid.to_string()),
                time(seconds_ms(entitlement.rental_expiry.as_ref())),
                time(seconds_ms(entitlement.created_at.as_ref())),
                format!("<code>{}</code>", escape(&entitlement.urn)),
            ];
            if revoke {
                row.push(if entitlement.status == EntitlementStatus::Revoked as i32 {
                    String::new()
                } else {
                    frame.form(
                        &format!("/accounts/{id}/assets/revoke"),
                        &format!(
                            r#"<input type="hidden" name="urn" value="{}"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#,
                            escape(&entitlement.urn)
                        ),
                        "Revoke",
                    )
                });
            }
            row
        })
        .collect()
}

const ENTITLEMENT_HEADERS: [&str; 8] = ["Name", "Class", "Source", "Status", "Entity", "Rental expiry", "Granted", "URN"];

async fn assets(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let mut body = flash(&query(raw)) + &account_header(&app, &target, "/assets");
    let Some(geid) = target.geid else { return html(frame.page("Assets", &(body + &no_player()))) };
    body.push_str("<h2>Grant</h2><fieldset>");
    body.push_str(&frame.form(
        &format!("/accounts/{id}/assets/grant"),
        &format!(
            r#"<input name="class_guid" required size="38" placeholder="Entity class GUID">
<select name="source"><option value="platform">platform (pledge)</option><option value="ingame">in game (bought)</option><option value="rental">rental</option></select>
<input name="days" type="number" min="1" max="{MAX_RENTAL_DAYS}" placeholder="Rental days">
<input name="reason" required maxlength="{MAX_REASON}" size="30" placeholder="Reason">"#
        ),
        "Grant",
    ));
    body.push_str(
        r#"<p class="muted">Granted as a purchase is (sc-entitlement <code>GrantPurchase</code>): packaged and claimable, or for a rental claimed at once with its rental (sc-rental <code>CreateRental</code>). The game sees it on its next query.</p></fieldset>"#,
    );
    body.push_str("<h2>Entitlements</h2>");
    let filter = EntitlementFilter { include_revoked: Some(true), ..Default::default() };
    match app.owners.entitlements(id, geid, filter).await {
        Ok(entitlements) => {
            let mut headers = ENTITLEMENT_HEADERS.to_vec();
            headers.push("");
            body.push_str(&table(&headers, &entitlement_rows(&frame, id, &entitlements, true)));
        }
        Err(status) => body.push_str(&failed("sc-entitlement", &status)),
    }
    body.push_str("<h2>History</h2>");
    match app.owners.entitlement_history(geid).await {
        Ok(entries) => {
            let rows: Vec<Vec<String>> = entries
                .iter()
                .map(|entry| {
                    vec![
                        time(entry.created_at_ms),
                        log_action_name(entry.log_action),
                        escape(&entry.log_message),
                        format!("<code>{}</code>", escape(&entry.urn)),
                        format!("<code>{}</code>", escape(&entry.command_id)),
                    ]
                })
                .collect();
            body.push_str(&table(&["When", "Action", "Message", "URN", "Command"], &rows));
        }
        Err(status) => body.push_str(&failed("sc-entitlement", &status)),
    }
    html(frame.page("Assets", &body))
}

fn valid_class(guid: &str) -> bool {
    (1..=64).contains(&guid.len()) && guid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

async fn asset_grant(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let class = form.get("class_guid").to_lowercase();
    if !valid_class(&class) {
        return back(&app, id, "/assets", Err("The class must be a GUID.".into()));
    }
    let source = match form.get("source") {
        "platform" => EntitlementSourceType::Platform,
        "ingame" => EntitlementSourceType::IngamePersistent,
        "rental" => EntitlementSourceType::Rental,
        _ => return back(&app, id, "/assets", Err("Unknown source.".into())),
    };
    let days = match (source, form.get("days").parse::<u32>()) {
        (EntitlementSourceType::Rental, Ok(days)) if (1..=MAX_RENTAL_DAYS).contains(&days) => days,
        (EntitlementSourceType::Rental, _) => {
            return back(&app, id, "/assets", Err(format!("A rental needs 1 to {MAX_RENTAL_DAYS} days.")))
        }
        _ => 0,
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/assets", Err(text)),
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/assets", Err("No player for this account.".into()));
    };
    let key = key(&form);
    let expiry = if days > 0 { now_ms() + i64::from(days) * DAY_MS } else { 0 };
    let log_message = format!("admin {}: {reason}", admin.handle);
    let entry = Entry {
        action: "assets.grant".into(),
        owner: if days > 0 { "sc-entitlement, sc-rental" } else { "sc-entitlement" }.into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "class": class, "source": source.as_str_name(), "days": days, "reason": reason }),
        ..Default::default()
    };
    let grant = GrantPurchaseRequest {
        key: key.clone(),
        account_id: id,
        player_id: geid,
        source_type: source as i32,
        entity_class_guid: class.clone(),
        rental_expiry_ms: expiry,
        log_message: log_message.clone(),
    };
    let result = act(&app, &admin, &form, entry, async {
        // As sc-eatransaction does once paid: the entitlement, then its rental.
        let urn = app.owners.grant(grant).await?;
        if days > 0 {
            let rental = CreateRentalRequest {
                id: key.clone(),
                account_id: id,
                player_id: geid,
                entitlement_urn: urn.clone(),
                entity_class_guid: class.clone(),
                days,
                expire_at_ms: expiry,
                log_message,
            };
            app.owners.create_rental(rental).await?;
        }
        Ok((format!("Granted {urn}."), (None, Some(json!({ "urn": urn, "rental": (days > 0).then_some(&key) })))))
    })
    .await;
    back(&app, id, "/assets", result)
}

async fn asset_revoke(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let urn = form.get("urn").to_string();
    if urn.is_empty() {
        return back(&app, id, "/assets", Err("Which entitlement?".into()));
    }
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/assets", Err(text)),
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/assets", Err("No player for this account.".into()));
    };
    let entry = Entry {
        action: "assets.revoke".into(),
        owner: "sc-entitlement".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "urn": urn, "reason": reason }),
        ..Default::default()
    };
    let log_message = format!("admin {}: {reason}", admin.handle);
    let revocation = RevokeEntitlementRequest { urn: urn.clone(), player_id: geid, log_message: log_message.clone(), command_id: key(&form) };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.revoke(revocation).await?;
        // A rental of it would revoke it again at its expiry, and stay listed as active until then: end it too.
        let rentals = app.owners.rentals(geid).await?;
        let mut ended = Vec::new();
        for rental in rentals.iter().filter(|rental| rental.entitlement_urn == urn && rental.status == RentalStatus::Active as i32) {
            let request = EndRentalRequest { id: rental.id.clone(), player_id: geid, log_message: log_message.clone() };
            app.owners.end_rental(request).await?;
            ended.push(rental.id.clone());
        }
        let text = if done.revoked { format!("Revoked {urn}.") } else { format!("{urn} was already revoked.") };
        let before = json!({ "status": status_name(done.previous_status) });
        Ok((text, (Some(before), Some(json!({ "status": "revoked", "rentals_ended": ended })))))
    })
    .await;
    back(&app, id, "/assets", result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Ships

fn rental_status_name(status: i32) -> &'static str {
    match RentalStatus::try_from(status) {
        Ok(RentalStatus::Available) => "available",
        Ok(RentalStatus::Active) => "active",
        Ok(RentalStatus::Finished) => "finished",
        _ => "?",
    }
}

fn rental_rows(frame: &Frame, id: u32, rentals: &[Rental]) -> Vec<Vec<String>> {
    rentals
        .iter()
        .map(|rental| {
            let active = rental.status == RentalStatus::Active as i32;
            let hidden = format!(r#"<input type="hidden" name="rental" value="{}">"#, escape(&rental.id));
            let actions = if active {
                frame.form(
                    &format!("/accounts/{id}/ships/extend"),
                    &format!(r#"{hidden}<input name="days" type="number" min="1" max="{MAX_RENTAL_DAYS}" required placeholder="Days" style="width:5em"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#),
                    "Extend",
                ) + &frame.form(
                    &format!("/accounts/{id}/ships/end"),
                    &format!(r#"{hidden}<input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#),
                    "End now",
                )
            } else {
                frame.form(
                    &format!("/accounts/{id}/ships/extend"),
                    &format!(r#"{hidden}<input name="days" type="number" min="1" max="{MAX_RENTAL_DAYS}" required placeholder="Days" style="width:5em"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#),
                    "Renew",
                )
            };
            vec![
                format!("<code>{}</code>", escape(&rental.entity_class_guid)),
                rental_status_name(rental.status).into(),
                time(seconds_ms(rental.began_at.as_ref())),
                time(seconds_ms(rental.expire_at.as_ref())),
                rental.duration.to_string(),
                rental.renewals.to_string(),
                format!("<code>{}</code>", escape(&rental.id)),
                actions,
            ]
        })
        .collect()
}

async fn ships(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let mut body = flash(&query(raw)) + &account_header(&app, &target, "/ships");
    let Some(geid) = target.geid else { return html(frame.page("Ships", &(body + &no_player()))) };
    body.push_str("<h2>Rentals</h2>");
    match app.owners.rentals(geid).await {
        Ok(rentals) => body.push_str(&table(
            &["Class", "Status", "Began", "Expires", "Days", "Renewals", "Rental", ""],
            &rental_rows(&frame, id, &rentals),
        )),
        Err(status) => body.push_str(&failed("sc-rental", &status)),
    }
    body.push_str(r#"<p class="muted">Ending a rental makes it expire now: sc-rental revokes its entitlement, then finishes it. Extending is a renewal without the charge.</p>"#);
    body.push_str("<h2>Owned ships</h2>");
    let filter = EntitlementFilter { item_type: Some(VEHICLE_ITEM_TYPE.into()), include_revoked: Some(true), ..Default::default() };
    let rented = EntitlementFilter {
        source_type: vec![EntitlementSourceType::Rental as i32],
        include_revoked: Some(true),
        ..Default::default()
    };
    match (app.owners.entitlements(id, geid, filter).await, app.owners.entitlements(id, geid, rented).await) {
        (Ok(mut owned), Ok(rented)) => {
            for entitlement in rented {
                if !owned.iter().any(|known| known.urn == entitlement.urn) {
                    owned.push(entitlement);
                }
            }
            let mut rows = entitlement_rows(&frame, id, &owned, false);
            let mut states = 0;
            for (row, entitlement) in rows.iter_mut().zip(&owned) {
                let state = match entitlement.entity_geid {
                    Some(entity) if states < VEHICLE_STATES_SHOWN => {
                        states += 1;
                        match app.owners.documents(entity, vec!["vehicle_state".into()]).await {
                            Ok(documents) if documents.is_empty() => "no saved state".into(),
                            Ok(documents) => documents.iter().map(document_summary).collect::<Vec<_>>().join("<br>"),
                            Err(status) => format!("sc-entity-document: {}", escape(status.message())),
                        }
                    }
                    Some(_) => "-".into(),
                    None => "no entity yet".into(),
                };
                row.push(state);
            }
            let mut headers = ENTITLEMENT_HEADERS.to_vec();
            headers.push("Saved state");
            body.push_str(&table(&headers, &rows));
        }
        (Err(status), _) | (_, Err(status)) => body.push_str(&failed("sc-entitlement", &status)),
    }
    body.push_str(r#"<p class="muted">Ships are the vehicle entitlements and every rental. Where a ship is in the world is the DGS's while it is spawned, and is not stored yet.</p>"#);
    html(frame.page("Ships", &body))
}

async fn rental_end(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let rental = form.get("rental").to_string();
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/ships", Err(text)),
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/ships", Err("No player for this account.".into()));
    };
    let entry = Entry {
        action: "rental.end".into(),
        owner: "sc-rental".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "rental": rental, "reason": reason }),
        ..Default::default()
    };
    let request = EndRentalRequest { id: rental.clone(), player_id: geid, log_message: format!("admin {}: {reason}", admin.handle) };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.end_rental(request).await?;
        let text = if done.ended { "Rental ended." } else { "The rental had already ended." };
        Ok((text.to_string(), (Some(json!({ "expire_at_ms": done.previous_expire_at_ms })), Some(json!({ "ended": done.ended })))))
    })
    .await;
    back(&app, id, "/ships", result)
}

async fn rental_extend(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let rental = form.get("rental").to_string();
    let days = match form.get("days").parse::<u32>() {
        Ok(days) if (1..=MAX_RENTAL_DAYS).contains(&days) => days,
        _ => return back(&app, id, "/ships", Err(format!("1 to {MAX_RENTAL_DAYS} days."))),
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/ships", Err(text)),
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/ships", Err("No player for this account.".into()));
    };
    let entry = Entry {
        action: "rental.extend".into(),
        owner: "sc-rental, sc-entitlement".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "rental": rental, "days": days, "reason": reason }),
        ..Default::default()
    };
    let log_message = format!("admin {}: {reason}", admin.handle);
    let extension = ExtendRentalRequest { key: key(&form), id: rental.clone(), player_id: geid, days, log_message: log_message.clone() };
    let result = act(&app, &admin, &form, entry, async {
        // As sc-eatransaction renews, without the charge.
        let extended = app.owners.extend_rental(extension).await?;
        let expiry = SetRentalExpiryRequest {
            urn: extended.entitlement_urn.clone(),
            player_id: geid,
            rental_expiry_ms: extended.expire_at_ms,
            reactivate: true,
            log_message,
            command_id: key(&form),
        };
        app.owners.set_rental_expiry(expiry).await?;
        Ok((format!("Rental runs until {}.", time(extended.expire_at_ms)), (None, Some(json!({ "expire_at_ms": extended.expire_at_ms })))))
    })
    .await;
    back(&app, id, "/ships", result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Location

fn document_summary(stored: &StoredEntityDocument) -> String {
    let revision = stored.document.as_ref().and_then(|document| document.header.as_ref()).map_or("?", |header| header.revision.as_str());
    let body = stored.document.as_ref().and_then(|document| document.body.as_ref()).and_then(|body| body.class.as_ref());
    let text = match body {
        Some(Class::PlayerHomeLocation(home)) => place_name(home.home_location_id),
        Some(Class::PlayerSpawnLocation(spawn)) => spawn_text(spawn),
        Some(other) => format!("{other:?}"),
        None => "only prepared, not committed".into(),
    };
    format!("{} <span class=\"muted\">(revision {})</span>", escape(&text), escape(revision))
}

fn spawn_text(spawn: &PlayerSpawnLocationDocument) -> String {
    match &spawn.spawn_info {
        Some(SpawnInfo::SpawnLocation(at)) => format!("at {}", place_name(at.location_id)),
        Some(SpawnInfo::SpawnZonePosition(zone)) => format!("in a zone of {}", place_name(zone.location_id)),
        Some(SpawnInfo::SpawnInShip(ship)) => format!("in a ship: {ship:?}"),
        Some(SpawnInfo::SpawnInPlace(place)) => format!("in place: {place:?}"),
        Some(SpawnInfo::SpawnStowedParent(parent)) => format!("stowed: {parent:?}"),
        None => "nothing".into(),
    }
}

fn find<'a>(documents: &'a [StoredEntityDocument], class: &str) -> Option<&'a StoredEntityDocument> {
    documents.iter().find(|document| document.class == class)
}

fn revision_of(document: Option<&StoredEntityDocument>) -> String {
    document
        .filter(|document| document.document.as_ref().is_some_and(|document| document.body.is_some()))
        .and_then(|document| document.document.as_ref()?.header.as_ref())
        .map_or_else(|| "0".to_string(), |header| header.revision.clone())
}

const HOME: &str = "player_home_location";
const SPAWN: &str = "player_spawn_location";

async fn location(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let mut body = flash(&query(raw)) + &account_header(&app, &target, "/location");
    let Some(geid) = target.geid else { return html(frame.page("Location", &(body + &no_player()))) };
    let now = playing(&app, id, geid).await;
    let _ = write!(body, "<h2>Now</h2><p>{}</p>", playing_text(&now));
    let locked = match &now {
        Playing::Yes { .. } => Some("The player is on a shard. The DGS holds where they are in memory and would not see a change, so changes are refused until they leave."),
        Playing::Unknown(_) => Some("sc-hub did not say whether the player is on a shard, so changes are refused."),
        Playing::No { .. } => None,
    };
    if let Some(text) = locked {
        let _ = write!(body, r#"<p class="warn">{}</p>"#, escape(text));
    } else {
        body.push_str(r#"<p class="muted">Changes go to the stored documents and take effect when the player next joins a shard.</p>"#);
    }
    match app.owners.documents(geid, vec![HOME.into(), SPAWN.into()]).await {
        Ok(documents) => {
            let home = find(&documents, HOME);
            let spawn = find(&documents, SPAWN);
            let _ = write!(
                body,
                "<h2>Home location</h2><p>{}</p>",
                home.map_or_else(|| "None stored: the DGS gives the default.".into(), document_summary)
            );
            if locked.is_none() {
                let places = places();
                if places.is_empty() {
                    body.push_str(r#"<p class="error">The home locations of the game build are not loaded yet (sc-gamedata); try again shortly.</p>"#);
                }
                let options = places.iter().fold(String::new(), |mut out, place| {
                    let _ = write!(out, r#"<option value="{}">{} ({})</option>"#, place.id, escape(&place.name), escape(&place.system));
                    out
                });
                body.push_str(&frame.form(
                    &format!("/accounts/{id}/location/home"),
                    &format!(
                        r#"<input type="hidden" name="revision" value="{}"><select name="location">{options}</select><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#,
                        escape(&revision_of(home))
                    ),
                    "Set home",
                ));
            }
            let _ = write!(
                body,
                "<h2>Spawn location</h2><p>{}</p>",
                spawn.map_or_else(|| "None stored: the player spawns at home.".into(), document_summary)
            );
            if locked.is_none() {
                body.push_str(&frame.form(
                    &format!("/accounts/{id}/location/spawn"),
                    &format!(
                        r#"<input type="hidden" name="revision" value="{}"><input name="location" type="number" min="1" required placeholder="Location id"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#,
                        escape(&revision_of(spawn))
                    ),
                    "Set spawn location",
                ));
            }
        }
        Err(status) => body.push_str(&failed("sc-entity-document", &status)),
    }
    body.push_str(r#"<p class="muted">Location ids are <code>LocationId</code>s, the class CRC of a <code>StarMapObject</code>. Home locations are those with a hangar in the DGS's table.</p>"#);
    html(frame.page("Location", &body))
}

async fn set_home(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let location = match form.get("location").parse::<u32>() {
        Ok(location) if places().iter().any(|place| place.id == location) => location,
        _ => return back(&app, id, "/location", Err("Pick a home location from the list.".into())),
    };
    let body = EntityDocumentBody { class: Some(Class::PlayerHomeLocation(PlayerHomeLocationDocument { home_location_id: location })) };
    put_location(app, admin, form, id, "location.home", body, place_name(location)).await
}

async fn set_spawn(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let location = match form.get("location").parse::<u32>() {
        Ok(location) if location > 0 => location,
        _ => return back(&app, id, "/location", Err("A location id is needed.".into())),
    };
    let spawn = PlayerSpawnLocationDocument { spawn_info: Some(SpawnInfo::SpawnLocation(PlayerSpawnLocation { location_id: location })) };
    let body = EntityDocumentBody { class: Some(Class::PlayerSpawnLocation(spawn)) };
    put_location(app, admin, form, id, "location.spawn", body, place_name(location)).await
}

/// Replaces a location document of an offline player, if nobody changed it since the page was drawn.
async fn put_location(
    app: Shared,
    admin: Admin,
    form: Form,
    id: u32,
    action: &str,
    body: EntityDocumentBody,
    described: String,
) -> Response {
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, "/location", Err(text)),
    };
    let Ok(expected) = form.get("revision").parse::<u64>() else {
        return back(&app, id, "/location", Err("Reload the page.".into()));
    };
    let Some(geid) = player(&app, id).await else {
        return back(&app, id, "/location", Err("No player for this account.".into()));
    };
    match playing(&app, id, geid).await {
        Playing::No { .. } => {}
        Playing::Yes { .. } => return back(&app, id, "/location", Err("The player is on a shard: nothing was changed.".into())),
        Playing::Unknown(error) => return back(&app, id, "/location", Err(format!("sc-hub did not answer ({error}): nothing was changed."))),
    }
    let entry = Entry {
        action: action.into(),
        owner: "sc-entity-document".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "set": described, "reason": reason, "expected_revision": expected }),
        ..Default::default()
    };
    let request = PutEntityDocumentRequest { entity_id: geid, owner_id: geid, body: Some(body), expected_revision: Some(expected) };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.put_document(request).await?;
        let before = done.previous.as_ref().and_then(|previous| previous.body.as_ref()).map(|body| json!(format!("{body:?}")));
        let text = format!("Set to {described}. It applies when the player next joins.");
        Ok((text, (before, Some(json!({ "set": described, "revision": done.revision })))))
    })
    .await;
    back(&app, id, "/location", result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Inventory

/// Of an added item, as sc-entitygraph allows.
const MAX_STACK: u32 = 1_000_000;
const MAX_CLASS_QUERY: usize = 100;

fn inventory_id(inventory: &AdminInventory) -> &str {
    inventory.inventory.as_ref().map_or("", |properties| properties.id.as_str())
}

/// What an inventory is, in words: the player's own, a location bank, or the container of an entity.
fn inventory_label(inventory: &AdminInventory) -> String {
    let Some(properties) = inventory.inventory.as_ref() else { return "?".into() };
    let holder = || {
        let class = if inventory.holder_class_name.is_empty() { "entity" } else { inventory.holder_class_name.as_str() };
        format!("{class} {}", inventory.holder_id)
    };
    match properties.context.as_str() {
        "PlayerInventory" => "Personal inventory".into(),
        "Location" => format!("Item bank at {}", place_name(properties.subject_id as u32)),
        "Container" => format!("Container of {}", holder()),
        "PersonalEntityInventory" => format!("Own space in {}", holder()),
        "" => properties.id.clone(),
        context => format!("{context} ({})", properties.subject_id),
    }
}

/// A volume in micro-SCU as SCU.
fn scu(micro: i64) -> String {
    let text = format!("{:.6}", micro as f64 / 1_000_000.0);
    format!("{} SCU", text.trim_end_matches('0').trim_end_matches('.'))
}

fn capacity_text(properties: &InventoryNodeProperties) -> String {
    use sc_proto::services::entitygraph::v1::inventory_configuration::Configuration;
    match properties.configuration.as_ref().and_then(|configuration| configuration.configuration) {
        Some(Configuration::Physical(physical)) if physical.capacity > 0 => {
            format!("{} of {}", scu(i64::from(physical.occupancy)), scu(i64::from(physical.capacity)))
        }
        Some(Configuration::Physical(physical)) => format!("{} used, no limit", scu(i64::from(physical.occupancy))),
        None => "no limit".into(),
    }
}

/// Why changes are refused now, if they are.
fn inventory_lock(now: &Playing) -> Option<&'static str> {
    match now {
        Playing::Yes { .. } => Some("The player is on a shard, where the DGS holds their items in memory and would not see a change, so changes are refused until they leave."),
        Playing::Unknown(_) => Some("sc-hub did not say whether the player is on a shard, so changes are refused."),
        Playing::No { .. } => None,
    }
}

/// The inventories an item may go to: those not live, but its own and those it holds.
fn target_options(tree: &[AdminInventory], item: &AdminInventoryItem) -> String {
    tree.iter()
        .filter(|inventory| !inventory.live)
        .filter(|inventory| inventory_id(inventory) != item.inventory_id && !item.held_inventory_ids.iter().any(|held| held == inventory_id(inventory)))
        .fold(String::new(), |mut out, inventory| {
            let _ = write!(
                out,
                r#"<option value="{}">{}{}</option>"#,
                escape(inventory_id(inventory)),
                "· ".repeat(inventory.depth as usize),
                escape(&inventory_label(inventory))
            );
            out
        })
}

fn item_name(item: &AdminInventoryItem) -> String {
    match (item.display_name.is_empty(), item.class_name.is_empty()) {
        (_, true) => format!(r#"<span class="muted">class {:#010x}, not seeded</span>"#, item.class_guid_crc),
        (true, false) => escape(&item.class_name),
        (false, false) => format!("{}<br><span class=\"muted\">{}</span>", escape(&item.display_name), escape(&item.class_name)),
    }
}

fn item_rows(frame: &Frame, id: u32, tree: &[AdminInventory], inventory: &AdminInventory, editable: bool) -> Vec<Vec<String>> {
    inventory
        .items
        .iter()
        .map(|item| {
            let holds = item.held_inventory_ids.iter().fold(String::new(), |mut out, held| {
                let _ = write!(out, r##"<a href="#inv-{0}"><code>{0}</code></a> "##, escape(held));
                out
            });
            let actions = if item.on_shard {
                format!(r#"<span class="warn">on shard <code>{}</code></span>"#, escape(&item.shard_id))
            } else if editable {
                let hidden = format!(
                    r#"<input type="hidden" name="geid" value="{}"><input type="hidden" name="from" value="{}">"#,
                    item.geid,
                    escape(&item.inventory_id)
                );
                let options = target_options(tree, item);
                let mover = if options.is_empty() {
                    String::new()
                } else {
                    frame.form(
                        &format!("/accounts/{id}/inventory/move"),
                        &format!(
                            r#"{hidden}<select name="to">{options}</select><input name="quantity" type="number" min="0" max="{}" placeholder="All" class="narrow"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#,
                            item.stack_size.max(1)
                        ),
                        "Move",
                    )
                };
                mover
                    + &frame.form(
                        &format!("/accounts/{id}/inventory/remove"),
                        &format!(r#"{hidden}<input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#),
                        "Remove",
                    )
            } else {
                String::new()
            };
            vec![
                item.geid.to_string(),
                item_name(item),
                format!("<code>{}</code>", escape(&item.class_guid)),
                item.stack_size.to_string(),
                item.owner_id.to_string(),
                holds,
                actions,
            ]
        })
        .collect()
}

/// The classes found for `query`, each with a link that puts its GUID in the add form.
async fn class_search(app: &App, id: u32, query: &str) -> String {
    let mut out = String::new();
    match app.owners.search_classes(query).await {
        Ok(classes) => {
            let rows: Vec<Vec<String>> = classes
                .iter()
                .map(|class| {
                    vec![
                        escape(&class.display_name),
                        escape(&class.class_name),
                        format!("<code>{}</code>", escape(&class.guid)),
                        escape(&format!("{} {}", class.item_type, class.item_sub_type)),
                        scu(class.occupancy_micro_scu as i64),
                        if class.container { format!("holds {}", scu(class.capacity_micro_scu as i64)) } else { String::new() },
                        format!(
                            r##"<a href="{}/accounts/{id}/inventory?class={}&guid={}#add">Use</a>"##,
                            app.prefix,
                            query_value(query),
                            query_value(&class.guid)
                        ),
                    ]
                })
                .collect();
            out.push_str(&table(&["Name", "Class", "GUID", "Type", "Room", "Container", ""], &rows));
            if classes.len() >= 50 {
                out.push_str(r#"<p class="muted">Only the first 50: narrow the search.</p>"#);
            }
        }
        Err(status) => out.push_str(&failed("sc-entitygraph", &status)),
    }
    out
}

async fn inventory(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Path(id): Path<u32>,
    RawQuery(raw): RawQuery,
) -> Response {
    let target = match target(&app, &admin, id).await {
        Ok(target) => target,
        Err(response) => return response,
    };
    let frame = frame(&app, &admin);
    let query = query(raw);
    let mut body = flash(&query) + &account_header(&app, &target, "/inventory");
    let Some(geid) = target.geid else { return html(frame.page("Inventory", &(body + &no_player()))) };
    let now = playing(&app, id, geid).await;
    let _ = write!(body, "<h2>Now</h2><p>{}</p>", playing_text(&now));
    let locked = inventory_lock(&now);
    match locked {
        Some(text) => {
            let _ = write!(body, r#"<p class="warn">{}</p>"#, escape(text));
        }
        None => body.push_str(
            r#"<p class="muted">Changes go to sc-entitygraph at once, checked as the player's own would be: capacity, loops, ownership. The client sees them on its next query of its inventories.</p>"#,
        ),
    }
    let tree = match app.owners.inventory_tree(geid).await {
        Ok(tree) => tree,
        Err(status) => return html(frame.page("Inventory", &(body + &failed("sc-entitygraph", &status)))),
    };
    if tree.is_empty() {
        body.push_str(r#"<p class="muted">No inventory yet: sc-entitygraph makes them on first need.</p>"#);
    }
    if locked.is_none() {
        let class = query.get("class").map_or("", |class| class.trim());
        let guid = query.get("guid").map_or("", |guid| guid.trim());
        body.push_str(r#"<h2 id="add">Add an item</h2><fieldset>"#);
        let _ = write!(
            body,
            r#"<form method="get" action="{}/accounts/{id}/inventory#add" class="inline"><input name="class" value="{}" maxlength="{MAX_CLASS_QUERY}" size="30" placeholder="Class name, English name or GUID"><button type="submit">Find class</button></form>"#,
            app.prefix,
            escape(class)
        );
        if !class.is_empty() {
            body.push_str(&class_search(&app, id, class).await);
        }
        let inventories = tree.iter().filter(|inventory| !inventory.live).fold(String::new(), |mut out, inventory| {
            let _ = write!(
                out,
                r#"<option value="{}">{}{}</option>"#,
                escape(inventory_id(inventory)),
                "· ".repeat(inventory.depth as usize),
                escape(&inventory_label(inventory))
            );
            out
        });
        let personal = format!("{geid}:PlayerInventory:0");
        let inventories = if inventories.is_empty() {
            format!(r#"<option value="{0}">Personal inventory</option>"#, escape(&personal))
        } else {
            inventories
        };
        body.push_str(&frame.form(
            &format!("/accounts/{id}/inventory/add"),
            &format!(
                r#"<select name="inventory">{inventories}</select><input name="class_guid" required size="38" value="{}" placeholder="Entity class GUID"><input name="stack" type="number" min="1" max="{MAX_STACK}" value="1" class="narrow"><input name="reason" required maxlength="{MAX_REASON}" placeholder="Reason">"#,
                escape(guid)
            ),
            "Add",
        ));
        body.push_str(r#"<p class="muted">A new item with a GEID from sc-entitygraph, owned by the player. A crate, a backpack or any class with an inventory container gets its own inventory with it.</p></fieldset>"#);
    }
    body.push_str("<h2>Inventories</h2>");
    for inventory in &tree {
        let Some(properties) = inventory.inventory.as_ref() else { continue };
        let depth = inventory.depth.min(6);
        let _ = write!(
            body,
            r#"<section class="inventory depth-{depth}" id="inv-{id_attr}"><h3>{label} <code>{id_text}</code></h3><p class="muted">{capacity} · {count} item{s}{parent}{shared}</p>"#,
            id_attr = escape(&properties.id),
            label = escape(&inventory_label(inventory)),
            id_text = escape(&properties.id),
            capacity = capacity_text(properties),
            count = inventory.item_count,
            s = if inventory.item_count == 1 { "" } else { "s" },
            parent = if inventory.parent_inventory_id.is_empty() {
                String::new()
            } else {
                format!(r##" · in <a href="#inv-{0}"><code>{0}</code></a>"##, escape(&inventory.parent_inventory_id))
            },
            shared = if properties.shared { " · shared" } else { "" },
        );
        if inventory.live {
            body.push_str(r#"<p class="warn">Held by something live on a shard: the DGS has it, so it is not changed here.</p>"#);
        }
        let editable = locked.is_none() && !inventory.live;
        body.push_str(&table(
            &["Entity", "Class", "Class GUID", "Stack", "Owner", "Holds", ""],
            &item_rows(&frame, id, &tree, inventory, editable),
        ));
        if inventory.item_count as usize > inventory.items.len() {
            let _ = write!(body, r#"<p class="muted">Only the first {} of {} items are shown.</p>"#, inventory.items.len(), inventory.item_count);
        }
        body.push_str("</section>");
    }
    body.push_str(r#"<p class="muted">The player's own inventories (personal, item banks, ...), the containers of what it owns (a ship's holds, crates, backpacks) and everything nested in them, parents before children. Moving part of a stack splits it into a new item; removing an item erases everything it holds.</p>"#);
    html(frame.page("Inventory", &body))
}

/// The player of an account, when it may be changed now: not on a shard, as far as sc-hub knows.
async fn offline_player(app: &App, id: u32) -> Result<u64, String> {
    let Some(geid) = player(app, id).await else { return Err("No player for this account.".into()) };
    match playing(app, id, geid).await {
        Playing::No { .. } => Ok(geid),
        Playing::Yes { .. } => Err("The player is on a shard: nothing was changed.".into()),
        Playing::Unknown(error) => Err(format!("sc-hub did not answer ({error}): nothing was changed.")),
    }
}

/// The item as sc-entitygraph has it now, for the audit's before.
async fn item_now(app: &App, geid: u64, item: u64) -> Option<Value> {
    let tree = app.owners.inventory_tree(geid).await.ok()?;
    let found = tree.iter().flat_map(|inventory| &inventory.items).find(|found| found.geid == item)?;
    Some(json!({
        "geid": found.geid,
        "class": found.class_name,
        "class_guid": found.class_guid,
        "stack": found.stack_size,
        "inventory": found.inventory_id,
        "holds": found.held_inventory_ids,
    }))
}

fn actor(admin: &Admin) -> String {
    format!("admin:{}:{}", admin.account_id, admin.handle)
}

fn valid_inventory_id(id: &str) -> bool {
    let mut parts = id.split(':');
    let (holder, context, subject) = (parts.next(), parts.next(), parts.next());
    parts.next().is_none()
        && holder.is_some_and(|holder| holder.parse::<u64>().is_ok())
        && context.is_some_and(|context| !context.is_empty() && context.bytes().all(|b| b.is_ascii_alphanumeric()))
        && subject.is_some_and(|subject| subject.parse::<u64>().is_ok())
}

const INVENTORY_PAGE: &str = "/inventory";

async fn item_move(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let item = match form.get("geid").parse::<u64>() {
        Ok(item) if item > 0 => item,
        _ => return back(&app, id, INVENTORY_PAGE, Err("Which item?".into())),
    };
    let (from, to) = (form.get("from").to_string(), form.get("to").to_string());
    if !valid_inventory_id(&from) || !valid_inventory_id(&to) || from == to {
        return back(&app, id, INVENTORY_PAGE, Err("Pick another inventory to move to.".into()));
    }
    let quantity = match form.get("quantity") {
        "" => 0,
        text => match text.parse::<u32>() {
            Ok(quantity) => quantity,
            Err(_) => return back(&app, id, INVENTORY_PAGE, Err("The quantity is a whole number, or empty for all.".into())),
        },
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let geid = match offline_player(&app, id).await {
        Ok(geid) => geid,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let before = item_now(&app, geid, item).await;
    let entry = Entry {
        action: "inventory.move".into(),
        owner: "sc-entitygraph".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "item": item, "from": from, "to": to, "quantity": quantity, "reason": reason }),
        ..Default::default()
    };
    let request = AdminMoveItemRequest {
        command_id: key(&form),
        owner_id: geid,
        geid: item,
        from_inventory_id: from.clone(),
        to_inventory_id: to.clone(),
        quantity,
        actor: actor(&admin),
        reason,
    };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.move_item(request).await?;
        let what = if done.moved_geid == item { format!("Moved {item}") } else { format!("Moved {quantity} of {item} as {}", done.moved_geid) };
        let text = format!("{what} to {to}.{}", if done.replayed { " (already applied)" } else { "" });
        let after = json!({ "geid": done.moved_geid, "inventory": to, "split_from": (done.moved_geid != item).then_some(item) });
        Ok((text, (before, Some(after))))
    })
    .await;
    back(&app, id, INVENTORY_PAGE, result)
}

async fn item_add(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let inventory = form.get("inventory").to_string();
    if !valid_inventory_id(&inventory) {
        return back(&app, id, INVENTORY_PAGE, Err("Pick an inventory.".into()));
    }
    let class = form.get("class_guid").to_lowercase();
    if !valid_class(&class) {
        return back(&app, id, INVENTORY_PAGE, Err("The class must be a GUID: find it with the class search.".into()));
    }
    let stack = match form.get("stack") {
        "" => 1,
        text => match text.parse::<u32>() {
            Ok(stack) if (1..=MAX_STACK).contains(&stack) => stack,
            _ => return back(&app, id, INVENTORY_PAGE, Err(format!("A stack of 1 to {MAX_STACK}."))),
        },
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let geid = match offline_player(&app, id).await {
        Ok(geid) => geid,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let entry = Entry {
        action: "inventory.add".into(),
        owner: "sc-entitygraph".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "class": class, "inventory": inventory, "stack": stack, "reason": reason }),
        ..Default::default()
    };
    let request = AdminAddItemRequest {
        command_id: key(&form),
        owner_id: geid,
        inventory_id: inventory.clone(),
        entity_class_guid: class.clone(),
        stack_size: stack,
        actor: actor(&admin),
        reason,
    };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.add_item(request).await?;
        let text = format!("Added {} to {inventory}.{}", done.geid, if done.replayed { " (already applied)" } else { "" });
        let after = json!({ "geid": done.geid, "class": class, "stack": stack, "inventory": inventory });
        Ok((text, (None, Some(after))))
    })
    .await;
    back(&app, id, INVENTORY_PAGE, result)
}

async fn item_remove(
    State(app): State<Shared>,
    Extension(admin): Extension<Admin>,
    Extension(form): Extension<Form>,
    Path(id): Path<u32>,
) -> Response {
    let item = match form.get("geid").parse::<u64>() {
        Ok(item) if item > 0 => item,
        _ => return back(&app, id, INVENTORY_PAGE, Err("Which item?".into())),
    };
    let reason = match reason(&form) {
        Ok(reason) => reason,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let geid = match offline_player(&app, id).await {
        Ok(geid) => geid,
        Err(text) => return back(&app, id, INVENTORY_PAGE, Err(text)),
    };
    let before = item_now(&app, geid, item).await;
    let entry = Entry {
        action: "inventory.remove".into(),
        owner: "sc-entitygraph".into(),
        target_account_id: Some(id),
        target_geid: Some(geid),
        detail: json!({ "item": item, "from": form.get("from"), "reason": reason }),
        ..Default::default()
    };
    let request = AdminRemoveItemRequest { command_id: key(&form), owner_id: geid, geid: item, actor: actor(&admin), reason };
    let result = act(&app, &admin, &form, entry, async {
        let done = app.owners.remove_item(request).await?;
        let text = format!(
            "Removed {item} and what it held: {} entit{} erased.{}",
            done.erased,
            if done.erased == 1 { "y" } else { "ies" },
            if done.replayed { " (already applied)" } else { "" }
        );
        Ok((text, (before, Some(json!({ "removed": done.removed, "erased": done.erased })))))
    })
    .await;
    back(&app, id, INVENTORY_PAGE, result)
}

// ---------------------------------------------------------------------------------------------------------------------
// Audit

async fn audit_page(app: &App, admin: &Admin, account: Option<&Target>) -> Response {
    let frame = frame(app, admin);
    let mut body = account.map(|target| account_header(app, target, "/audit")).unwrap_or_default();
    match app.audit.list(account.map(|target| target.account.account_id), 200).await {
        Ok(records) => {
            let rows: Vec<Vec<String>> = records
                .iter()
                .map(|record| {
                    let target = record.entry.target_account_id.map_or_else(String::new, |id| {
                        format!(r#"<a href="{}/accounts/{id}">{id}</a>"#, app.prefix)
                    });
                    let show = |value: &Option<Value>| value.as_ref().map_or_else(String::new, |value| format!("<code>{}</code>", escape(&value.to_string())));
                    vec![
                        time(record.created_at_ms),
                        escape(&record.entry.actor_handle),
                        escape(&record.entry.action),
                        target,
                        format!("<code>{}</code>", escape(&record.entry.detail.to_string())),
                        show(&record.before),
                        show(&record.after),
                        escape(&record.outcome),
                    ]
                })
                .collect();
            body.push_str(&table(&["When", "By", "Action", "Account", "Asked", "Before", "After", "Outcome"], &rows));
        }
        Err(error) => body.push_str(&notice(&format!("The audit could not be read: {error}"), true)),
    }
    html(frame.page("Audit", &body))
}

async fn audit_all(State(app): State<Shared>, Extension(admin): Extension<Admin>) -> Response {
    audit_page(&app, &admin, None).await
}

async fn audit_account(State(app): State<Shared>, Extension(admin): Extension<Admin>, Path(id): Path<u32>) -> Response {
    match target(&app, &admin, id).await {
        Ok(target) => audit_page(&app, &admin, Some(&target)).await,
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_found_by_name() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("a=1; sc_session=tok; sc_csrf="));
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("tok"));
        assert_eq!(cookie(&headers, CSRF_COOKIE), None, "an empty cookie is none");
        assert_eq!(cookie(&headers, "b"), None);
    }

    #[test]
    fn the_home_location_table_is_read() {
        assert!(places().len() >= 2);
        assert!(place_name(2_026_442_305).starts_with("RR_P6_LEO (Pyro"));
        assert_eq!(place_name(1), "location 1");
    }

    #[test]
    fn request_ids_are_checked() {
        assert!(valid_request_id(&request_id()));
        assert!(!valid_request_id("short"));
        assert!(!valid_request_id("has spaces in it"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}

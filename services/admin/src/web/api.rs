//! The developer API: the admin operations as JSON under `{prefix}/api/v1`, for tools and scripts that should not
//! depend on the pages. It reuses the pages' logic (`do_*`, `class_catalog`) and so their guards, limits and audit.
//!
//! Authentication is an access token (`Authorization: Bearer scat_...`) made on the tokens page. It acts as the admin who
//! made it: the account's roles are asked of sc-identity on every request. There are no cookies and no CSRF token, so
//! nothing here accepts a browser session. On a TLS-only site (`--secure-cookies`) a request that did not come over HTTPS
//! (`X-Forwarded-Proto`) is refused before its token is read.
//!
//! Errors are `{"error": "..."}` with the status that fits: 400 a bad request, 401 no or a bad token, 403 not an admin, 404
//! an unknown account, class or player, 409 the player is on a shard or an owner refused, 429 and 5xx as the owners said.
//! Actions take an optional `request_id` (8 to 64 letters, digits, `-`, `_`): repeating one is applied once.

use axum::{body::to_bytes, http::header::AUTHORIZATION, routing::post};
use serde_json::Map;

use super::*;
use crate::tokens;

/// The JSON object a state-changing request sent (empty when it sent nothing).
#[derive(Clone, Debug)]
struct Fields(Map<String, Value>);

fn reply(status: StatusCode, body: Value) -> Response {
    let mut response = (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, text: &str) -> Response {
    reply(status, json!({ "error": text }))
}

fn failed(failure: Failure) -> Response {
    error(failure.status, &failure.text)
}

fn unauthorized(text: &str) -> Response {
    let mut response = error(StatusCode::UNAUTHORIZED, text);
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static(r#"Bearer realm="sc-admin-api""#));
    response
}

pub fn router(app: Shared) -> Router<Shared> {
    let p = format!("{}/api/v1", app.prefix);
    let routes = Router::new()
        .route(&format!("{p}/accounts"), get(accounts))
        .route(&format!("{p}/accounts/{{id}}"), get(account))
        .route(&format!("{p}/accounts/{{id}}/inventory"), get(inventory))
        .route(&format!("{p}/accounts/{{id}}/inventory/add-batch"), post(add_batch))
        .route(&format!("{p}/accounts/{{id}}/inventory/equip"), post(equip))
        .route(&format!("{p}/accounts/{{id}}/inventory/unequip"), post(unequip))
        .route(&format!("{p}/accounts/{{id}}/ships"), get(ships))
        .route(&format!("{p}/accounts/{{id}}/ships/grant"), post(ships_grant))
        .route(&format!("{p}/accounts/{{id}}/ships/remove"), post(ships_remove))
        .route(&format!("{p}/accounts/{{id}}/badges"), get(badges).put(set_badges).post(set_badges))
        .route(&format!("{p}/accounts/{{id}}/names"), post(set_names).put(set_names))
        .route(&format!("{p}/accounts/{{id}}/audit"), get(audit_account))
        .route(&format!("{p}/inventory/classes"), get(classes))
        .route(&format!("{p}/audit"), get(audit_all))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "no such endpoint") })
        .layer(middleware::from_fn_with_state(app, guard));
    routes
}

/// Lets through only requests with the token of an admin's account, over HTTPS where the site is TLS-only.
async fn guard(State(app): State<Shared>, mut request: Request, next: Next) -> Response {
    if app.secure_cookies {
        let https = request.headers().get("x-forwarded-proto").and_then(|value| value.to_str().ok()).is_some_and(|proto| proto.eq_ignore_ascii_case("https"));
        if !https {
            return error(StatusCode::FORBIDDEN, "The API is served over HTTPS only.");
        }
    }
    let token = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim().to_string());
    let Some(token) = token.filter(|token| tokens::well_formed(token)) else {
        return unauthorized("An access token is needed: Authorization: Bearer <token>.");
    };
    let account_id = match app.tokens.verify(&tokens::hash(&token), now_ms()).await {
        Ok(Some(account_id)) => account_id,
        Ok(None) => return unauthorized("The token is not valid, or was revoked."),
        Err(error_text) => {
            warn!(error = %error_text, "token not verified");
            return error(StatusCode::SERVICE_UNAVAILABLE, "The token store is not answering.");
        }
    };
    let account = match app.identity.account(account_id).await {
        Ok(Some(account)) if !account.disabled => account,
        Ok(_) => return unauthorized("The token's account is gone or disabled."),
        Err(status) => {
            warn!(%status, "account not read");
            return error(StatusCode::SERVICE_UNAVAILABLE, "The account service is not answering.");
        }
    };
    if !account.roles.iter().any(|role| role == ADMIN_ROLE) {
        return error(StatusCode::FORBIDDEN, "The token's account is not an admin.");
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let body = std::mem::replace(request.body_mut(), Body::empty());
        let Ok(bytes) = to_bytes(body, FORM_LIMIT).await else { return error(StatusCode::PAYLOAD_TOO_LARGE, "The body is too large.") };
        let parsed = if bytes.iter().all(u8::is_ascii_whitespace) { Ok(Value::Object(Map::new())) } else { serde_json::from_slice::<Value>(&bytes) };
        match parsed {
            Ok(Value::Object(map)) => {
                request.extensions_mut().insert(Fields(map));
            }
            _ => return error(StatusCode::BAD_REQUEST, "The body must be a JSON object."),
        }
    }
    request.extensions_mut().insert(Admin { account_id, handle: account.handle, csrf: String::new() });
    let mut response = next.run(request).await;
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn query_of(raw: Option<String>) -> HashMap<String, String> {
    query(raw)
}

fn time_json(ms: i64) -> Value {
    if ms == 0 { Value::Null } else { json!(ms) }
}

fn account_json(account: &AccountSummary) -> Value {
    json!({
        "id": account.account_id,
        "handle": account.handle,
        "display_name": account.display_name,
        "email": account.email,
        "roles": account.roles,
        "created_at_ms": time_json(account.created_at),
        "disabled": account.disabled,
        "player": (account.geid != 0).then_some(account.geid),
        "badges": account.game_badges.iter().map(|badge| json!({ "id": badge.id, "name": badge.name })).collect::<Vec<_>>(),
    })
}

fn playing_json(playing: &Playing) -> Value {
    match playing {
        Playing::No { menu } => json!({ "state": if *menu { "menu" } else { "offline" } }),
        Playing::Yes { shard, location } => json!({ "state": "playing", "shard": shard, "location": location }),
        Playing::Unknown(error) => json!({ "state": "unknown", "error": error }),
    }
}

/// The account, or the response to send for it.
async fn load(app: &App, id: u32) -> Result<AccountSummary, Response> {
    match app.identity.account(id).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(error(StatusCode::NOT_FOUND, &format!("No account {id}."))),
        Err(status) => Err(failed(Failure::owner("sc-identity", &status))),
    }
}

/// `?limit=` from 1, at most `max`.
fn limit(query: &HashMap<String, String>, default: u32, max: u32) -> u32 {
    query.get("limit").and_then(|text| text.parse::<u32>().ok()).filter(|n| *n >= 1).map_or(default, |n| n.min(max))
}

async fn accounts(State(app): State<Shared>, RawQuery(raw): RawQuery) -> Response {
    let query = query_of(raw);
    let request = SearchAccountsRequest {
        query: query.get("q").cloned().unwrap_or_default(),
        limit: limit(&query, 50, 200),
        page_token: query.get("page").cloned().unwrap_or_default(),
    };
    match app.identity.search_accounts(request).await {
        Ok(found) => reply(
            StatusCode::OK,
            json!({
                "accounts": found.accounts.iter().map(account_json).collect::<Vec<_>>(),
                "next_page": (!found.next_page_token.is_empty()).then_some(found.next_page_token),
            }),
        ),
        Err(status) => failed(Failure::owner("sc-identity", &status)),
    }
}

async fn account(State(app): State<Shared>, Path(id): Path<u32>) -> Response {
    let account = match load(&app, id).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    let mut body = account_json(&account);
    if account.geid != 0 {
        body["now"] = playing_json(&playing(&app, id, account.geid).await);
    }
    reply(StatusCode::OK, body)
}

async fn classes(State(app): State<Shared>, RawQuery(raw): RawQuery) -> Response {
    let (status, body) = class_catalog(&app, &query_of(raw)).await;
    reply(status, body)
}

async fn inventory(State(app): State<Shared>, Path(id): Path<u32>) -> Response {
    let account = match load(&app, id).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    if account.geid == 0 {
        return error(StatusCode::NOT_FOUND, "No player for this account.");
    }
    let geid = account.geid;
    let now = playing(&app, id, geid).await;
    let tree = match app.owners.inventory_tree(geid).await {
        Ok(tree) => tree,
        Err(status) => return failed(Failure::owner("sc-entitygraph", &status)),
    };
    let inventories: Vec<Value> = tree
        .iter()
        .map(|inventory| {
            let node = inventory.inventory.clone().unwrap_or_default();
            json!({
                "id": node.id,
                "context": node.context,
                "subject": node.subject_id,
                "name": node.name,
                "parent": (!inventory.parent_inventory_id.is_empty()).then_some(&inventory.parent_inventory_id),
                "holder": inventory.holder_id,
                "holder_class": inventory.holder_class_name,
                "depth": inventory.depth,
                "live": inventory.live,
                "item_count": inventory.item_count,
                "items": inventory.items.iter().map(|item| json!({
                    "geid": item.geid,
                    "class": item.class_name,
                    "class_guid": item.class_guid,
                    "name": item.display_name,
                    "stack": item.stack_size,
                    "inventory": item.inventory_id,
                    "holds": item.held_inventory_ids,
                    "on_shard": item.on_shard,
                    "shard": (!item.shard_id.is_empty()).then_some(&item.shard_id),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut body = json!({ "player": geid, "now": playing_json(&now), "inventories": inventories });
    // The equipment is best effort, as on the page.
    match app.owners.equipment(geid).await {
        Ok(equipment) => {
            body["equipment"] = json!({
                "worn": equipment.worn.iter().map(|worn| json!({
                    "geid": worn.geid,
                    "class": worn.class_name,
                    "name": worn.display_name,
                    "type": worn.item_type,
                    "sub": worn.item_sub_type,
                    "parent": worn.parent_geid,
                    "port": worn.port,
                    "removable": worn.removable,
                })).collect::<Vec<_>>(),
                "ports": equipment.ports.iter().map(|port| json!({
                    "parent": port.parent_geid,
                    "parent_name": port.parent_name,
                    "port": port.port,
                    "occupant": (port.occupant_geid != 0).then_some(port.occupant_geid),
                    "candidates": port.candidates,
                })).collect::<Vec<_>>(),
            });
        }
        Err(status) => body["equipment_error"] = json!(format!("sc-entitygraph: {}", status.message())),
    }
    reply(StatusCode::OK, body)
}

/// A JSON field as the text a form field would hold: strings and numbers; anything else is no value.
fn text(body: &Map<String, Value>, name: &str) -> String {
    match body.get(name) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// The form the pages' logic reads, from the request's fields: `names` are taken as they are.
fn form_of(body: &Map<String, Value>, names: &[&str]) -> Result<Form, Failure> {
    let mut fields: HashMap<String, String> = names.iter().map(|name| (name.to_string(), text(body, name))).collect();
    let request_id = match text(body, "request_id") {
        id if id.is_empty() => request_id(),
        id => id,
    };
    if !valid_request_id(&request_id) {
        return Err(bad("request_id: 8 to 64 letters, digits, - or _."));
    }
    fields.insert("request_id".into(), request_id);
    Ok(Form(fields))
}

fn done(form: &Form, done: Done) -> Response {
    reply(StatusCode::OK, json!({ "ok": true, "message": done.text, "request_id": form.get("request_id"), "result": done.after }))
}

/// `{"inventory": "<id>", "items": [{"class": "<guid>", "amount": 1, "copies": 1}], "reason": "..."}`. `items` may also be
/// the page's `guid:amount:copies,...` text.
async fn add_batch(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &["inventory", "reason"]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    let items = match body.get("items") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => {
            let mut parts = Vec::new();
            for line in lines {
                let Some(line) = line.as_object() else { return error(StatusCode::BAD_REQUEST, "items: each item is an object with class, amount and copies.") };
                let number = |name: &str| match line.get(name) {
                    None | Some(Value::Null) => Ok(String::new()),
                    Some(Value::Number(n)) if n.is_u64() => Ok(n.to_string()),
                    _ => Err(()),
                };
                let (Some(class), Ok(amount), Ok(copies)) = (line.get("class").and_then(Value::as_str), number("amount"), number("copies")) else {
                    return error(StatusCode::BAD_REQUEST, "items: each item needs a class GUID, and whole numbers for amount and copies.");
                };
                parts.push(format!("{}:{amount}:{copies}", class.trim()));
            }
            parts.join(",")
        }
        _ => return error(StatusCode::BAD_REQUEST, "items: a list of {class, amount, copies}."),
    };
    form.0.insert("items".into(), items);
    match do_add_batch(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

/// `{"item": <geid>, "port": "<worn item geid>:<port>", "reason": "..."}`; or `parent` and `port` apart.
async fn equip(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &["reason"]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    form.0.insert("geid".into(), text(&body, "item"));
    let port = match text(&body, "parent") {
        parent if parent.is_empty() => text(&body, "port"),
        parent => format!("{parent}:{}", text(&body, "port")),
    };
    form.0.insert("port".into(), port);
    match do_equip(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

/// `{"item": <geid>, "to": "<inventory id>", "reason": "..."}`.
async fn unequip(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &["to", "reason"]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    form.0.insert("geid".into(), text(&body, "item"));
    match do_unequip(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

/// The ships in the player's hangars: `{"player", "ships": [{"geid", "class_guid", "name", "state", "pad_size", "location",
/// "insurance", "entitlement_urn", "created_at_ms"}], "repaired", "orphans"}`.
async fn ships(State(app): State<Shared>, Path(id): Path<u32>) -> Response {
    let account = match load(&app, id).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    if account.geid == 0 {
        return error(StatusCode::NOT_FOUND, "No player for this account.");
    }
    match super::ships::ships_json(&app, account.geid).await {
        Ok(body) => reply(StatusCode::OK, body),
        Err(failure) => failed(failure),
    }
}

/// `{"ships": "<guid>:<copies>,..." or [{"class": "<guid>", "copies": 1}], "location": <hangar id>, "insurance":
/// "none"|"lifetime"|"duration", "days": 30, "pad": <pad size number, optional>, "reason": "..."}`.
async fn ships_grant(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &["location", "insurance", "days", "pad", "reason"]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    let ships = match body.get("ships") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => {
            let mut parts = Vec::new();
            for line in lines {
                let part = match line {
                    Value::String(text) => text.clone(),
                    Value::Object(line) => {
                        let copies = match line.get("copies") {
                            None | Some(Value::Null) => 1,
                            Some(Value::Number(n)) if n.is_u64() => n.as_u64().unwrap_or(0),
                            _ => return error(StatusCode::BAD_REQUEST, "ships: copies is a number."),
                        };
                        let Some(class) = line.get("class").and_then(Value::as_str) else { return error(StatusCode::BAD_REQUEST, "ships: each ship is an object with class and copies.") };
                        format!("{class}:{copies}")
                    }
                    _ => return error(StatusCode::BAD_REQUEST, "ships: each ship is an object with class and copies."),
                };
                parts.push(part);
            }
            parts.join(",")
        }
        _ => String::new(),
    };
    form.0.insert("ships".into(), ships);
    match super::ships::do_grant(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

/// `{"ship": <geid>, "reason": "..."}`. Refused with 409 while the ship is deployed or the player on a shard.
async fn ships_remove(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &["reason"]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    form.0.insert("geid".into(), text(&body, "ship"));
    match super::ships::do_remove(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

async fn badges(State(app): State<Shared>, Path(id): Path<u32>) -> Response {
    let account = match load(&app, id).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    match app.identity.game_badges().await {
        Ok(known) => reply(
            StatusCode::OK,
            json!({
                "known": known.iter().map(|badge| json!({ "id": badge.id, "name": badge.name, "developer": DEVELOPER_BADGES.contains(&badge.id) })).collect::<Vec<_>>(),
                "held": account.game_badges.iter().map(|badge| badge.id).collect::<Vec<_>>(),
            }),
        ),
        Err(status) => failed(Failure::owner("sc-identity", &status)),
    }
}

/// `{"badges": [ids]}`: the whole set the account will hold.
async fn set_badges(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &[]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    let Some(Value::Array(wanted)) = body.get("badges") else { return error(StatusCode::BAD_REQUEST, "badges: a list of badge ids.") };
    let known = match app.identity.game_badges().await {
        Ok(known) => known,
        Err(status) => return failed(Failure::owner("sc-identity", &status)),
    };
    for badge in wanted {
        match badge.as_u64().filter(|badge| known.iter().any(|known| u64::from(known.id) == *badge)) {
            Some(badge) => {
                form.0.insert(format!("badge_{badge}"), "1".into());
            }
            None => return error(StatusCode::BAD_REQUEST, &format!("badges: {badge} is not a badge id the game has.")),
        }
    }
    form.0.insert("op".into(), "badges".into());
    match do_account_state(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

/// `{"handle": "...", "display_name": "..."}`: either or both; a field left out stays. An empty `display_name` goes back to
/// the handle. Refused (409) while the player is on a shard; the game shows the change from the next login.
async fn set_names(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(Fields(body)): Extension<Fields>, Path(id): Path<u32>) -> Response {
    let mut form = match form_of(&body, &[]) {
        Ok(form) => form,
        Err(failure) => return failed(failure),
    };
    let account = match load(&app, id).await {
        Ok(account) => account,
        Err(response) => return response,
    };
    if !body.contains_key("handle") && !body.contains_key("display_name") {
        return error(StatusCode::BAD_REQUEST, "handle and/or display_name: text.");
    }
    for field in ["handle", "display_name"] {
        if body.get(field).is_some_and(|value| !value.is_string()) {
            return error(StatusCode::BAD_REQUEST, &format!("{field}: text."));
        }
    }
    let handle = if body.contains_key("handle") { text(&body, "handle") } else { account.handle.clone() };
    let display_name = if body.contains_key("display_name") { text(&body, "display_name") } else { account.display_name.clone() };
    form.0.insert("op".into(), "names".into());
    form.0.insert("handle".into(), handle);
    form.0.insert("display_name".into(), display_name);
    match do_account_state(&app, &admin, &form, id).await {
        Ok(result) => done(&form, result),
        Err(failure) => failed(failure),
    }
}

async fn audit(app: &App, account: Option<u32>, raw: Option<String>) -> Response {
    let query = query_of(raw);
    match app.audit.list(account, limit(&query, 100, 500)).await {
        Ok(records) => reply(
            StatusCode::OK,
            json!({
                "records": records.iter().map(|record| json!({
                    "request_id": record.entry.request_id,
                    "at_ms": record.created_at_ms,
                    "actor": { "id": record.entry.actor_account_id, "handle": record.entry.actor_handle },
                    "action": record.entry.action,
                    "owner": record.entry.owner,
                    "account": record.entry.target_account_id,
                    "player": record.entry.target_geid,
                    "detail": record.entry.detail,
                    "before": record.before,
                    "after": record.after,
                    "outcome": record.outcome,
                })).collect::<Vec<_>>(),
            }),
        ),
        Err(text) => error(StatusCode::SERVICE_UNAVAILABLE, &format!("The audit could not be read: {text}")),
    }
}

async fn audit_all(State(app): State<Shared>, RawQuery(raw): RawQuery) -> Response {
    audit(&app, None, raw).await
}

async fn audit_account(State(app): State<Shared>, Path(id): Path<u32>, RawQuery(raw): RawQuery) -> Response {
    match load(&app, id).await {
        Ok(_) => audit(&app, Some(id), raw).await,
        Err(response) => response,
    }
}

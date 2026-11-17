//! The API tokens page: an admin makes and revokes the access tokens of the developer API (api.rs). A token acts as the
//! admin who made it. It is shown once, when made; only its hash is kept (tokens.rs).

use super::*;
use crate::tokens::{self, TokenInfo, MAX_LIVE, MAX_NAME};

fn list(app: &App, tokens: &[TokenInfo], frame: &Frame) -> String {
    let rows: Vec<Vec<String>> = tokens
        .iter()
        .map(|token| {
            let state = match token.revoked_at_ms {
                Some(at) => format!("revoked {}", time(at)),
                None => frame.form("/tokens/revoke", &format!(r#"<input type="hidden" name="token_id" value="{}">"#, escape(&token.token_id)), "Revoke"),
            };
            vec![
                escape(&token.name),
                format!("<code>{}</code>", escape(&token.token_id)),
                time(token.created_at_ms),
                token.last_used_at_ms.map_or_else(|| "never".to_string(), time),
                state,
            ]
        })
        .collect();
    let _ = app;
    table(&["Name", "Id", "Created", "Last used", ""], &rows)
}

async fn render(app: &App, admin: &Admin, lead: String) -> Response {
    let frame = frame(app, admin);
    let mut body = lead;
    body.push_str(
        r#"<p class="muted">Access tokens for the developer API (<code>Authorization: Bearer &lt;token&gt;</code>, see docs/API.md). A token acts as you, with your admin role, and stops working when you lose it or your account is disabled.</p>"#,
    );
    match app.tokens.list(admin.account_id).await {
        Ok(tokens) => body.push_str(&list(app, &tokens, &frame)),
        Err(error) => body.push_str(&notice(&format!("The tokens could not be read: {error}"), true)),
    }
    body.push_str("<h2>New token</h2>");
    body.push_str(&frame.form(
        "/tokens",
        &format!(r#"<label>Name <input name="name" required maxlength="{MAX_NAME}" size="30" placeholder="What it is for"></label>"#),
        "Make a token",
    ));
    html(frame.page("API tokens", &body))
}

pub async fn page(State(app): State<Shared>, Extension(admin): Extension<Admin>, RawQuery(raw): RawQuery) -> Response {
    render(&app, &admin, flash(&query(raw))).await
}

pub async fn create(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>) -> Response {
    let name = form.get("name").to_string();
    let notice_page = |text: String| render(&app, &admin, notice(&text, true));
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return notice_page(format!("A name of 1 to {MAX_NAME} characters is needed.")).await;
    }
    match app.tokens.list(admin.account_id).await {
        Ok(tokens) if tokens.iter().filter(|token| token.revoked_at_ms.is_none()).count() >= MAX_LIVE => {
            return notice_page(format!("At most {MAX_LIVE} live tokens: revoke one first.")).await;
        }
        Ok(_) => {}
        Err(error) => return notice_page(format!("The tokens could not be read: {error}")).await,
    }
    let token_id = hex(&random::<8>());
    let token = format!("{}{}", tokens::PREFIX, hex(&random::<32>()));
    let entry = Entry {
        action: "token.create".into(),
        owner: "sc-admin-web".into(),
        target_account_id: Some(admin.account_id),
        // Never the token itself.
        detail: json!({ "name": name, "token_id": token_id }),
        ..Default::default()
    };
    let (account_id, hashed) = (admin.account_id, tokens::hash(&token));
    let result = act_status(&app, &admin, &form, entry, async {
        app.tokens.create(&token_id, &hashed, account_id, &name, now_ms()).await.map_err(Status::internal)?;
        Ok((format!("Token {token_id} made."), (None, Some(json!({ "token_id": token_id, "name": name })))))
    })
    .await;
    match result {
        Ok(_) => {
            let lead = format!(
                r#"<div class="notice"><p>Token <strong>{}</strong> made. Copy it now: it is not shown again.</p><p><code id="token">{}</code></p></div>"#,
                escape(&name),
                escape(&token)
            );
            render(&app, &admin, lead).await
        }
        Err(failure) => {
            let mut response = notice_page(failure.text).await;
            *response.status_mut() = failure.status;
            response
        }
    }
}

pub async fn revoke(State(app): State<Shared>, Extension(admin): Extension<Admin>, Extension(form): Extension<Form>) -> Response {
    let token_id = form.get("token_id").to_string();
    let entry = Entry {
        action: "token.revoke".into(),
        owner: "sc-admin-web".into(),
        target_account_id: Some(admin.account_id),
        detail: json!({ "token_id": token_id }),
        ..Default::default()
    };
    let account_id = admin.account_id;
    let result = act(&app, &admin, &form, entry, async {
        match app.tokens.revoke(account_id, &token_id, now_ms()).await {
            Ok(true) => Ok((format!("Token {token_id} revoked."), (None, Some(json!({ "revoked": token_id }))))),
            Ok(false) => Err(Status::not_found("no such live token")),
            Err(error) => Err(Status::internal(error)),
        }
    })
    .await;
    with_flash(&app, Redirect::to(&format!("{}/tokens", app.prefix)).into_response(), &result)
}

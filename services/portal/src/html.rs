//! Server-rendered pages. Every value from outside goes through `escape`, and there is no script.

use std::fmt::Write;

use sc_api::internal::{AccountSummary, WebSession};

use crate::User;

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A hidden field with the CSRF token, for every form.
pub fn csrf_field(csrf: &str) -> String {
    format!(r#"<input type="hidden" name="csrf" value="{}">"#, escape(csrf))
}

pub fn layout(title: &str, user: Option<&User>, csrf: &str, body: &str) -> String {
    let mut nav = String::new();
    match user {
        Some(user) => {
            nav.push_str(r#"<a href="/play">Play</a><a href="/account">Account</a>"#);
            if user.is_admin() {
                nav.push_str(r#"<a href="/admin">Admin</a>"#);
            }
            let _ = write!(
                nav,
                r#"<span class="hint">{}</span><form method="post" action="/logout">{}<button class="link">Log out</button></form>"#,
                escape(&user.handle),
                csrf_field(csrf)
            );
        }
        None => nav.push_str(r#"<a href="/login">Log in</a><a href="/register">Register</a>"#),
    }
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · scbackend</title>
<link rel="stylesheet" href="/assets/site.css">
</head>
<body>
<header><a class="brand" href="/">scbackend</a>{nav}</header>
<main>
{body}
</main>
</body>
</html>"#,
        title = escape(title),
    )
}

pub fn message(class: &str, text: Option<&str>) -> String {
    text.map(|text| format!(r#"<p class="{class}">{}</p>"#, escape(text))).unwrap_or_default()
}

pub fn home(user: Option<&User>) -> String {
    let next = match user {
        Some(user) => format!(
            r#"<p>Logged in as <strong>{}</strong>. <a class="button" href="/play">Play</a></p>"#,
            escape(&user.handle)
        ),
        None => r#"<p><a class="button" href="/register">Create an account</a> or <a href="/login">log in</a>.</p>"#
            .to_string(),
    };
    format!(
        r#"<h1>A private Star Citizen server</h1>
<p>Make an account here, then download your game login and play on this server with your own copy of the game.</p>
{next}"#
    )
}

pub fn register(csrf: &str, error: Option<&str>, handle: &str, email: &str, open: bool) -> String {
    if !open {
        return "<h1>Register</h1><p>Registration is closed. Ask an admin for an account.</p>".to_string();
    }
    format!(
        r#"<h1>Create an account</h1>
{error}
<form method="post" action="/register">
{csrf}
<label for="handle">Handle</label>
<input type="text" id="handle" name="handle" value="{handle}" required minlength="3" maxlength="32" pattern="[A-Za-z0-9_\-]+" autocomplete="username">
<p class="hint">Your name in the game: 3 to 32 letters, digits, '_' and '-'.</p>
<label for="email">Email</label>
<input type="email" id="email" name="email" value="{email}" required maxlength="254" autocomplete="email">
<label for="password">Password</label>
<input type="password" id="password" name="password" required minlength="10" maxlength="128" autocomplete="new-password">
<p class="hint">At least 10 characters, not a common password, and not your handle or email.</p>
<button>Create account</button>
</form>"#,
        error = message("error", error),
        csrf = csrf_field(csrf),
        handle = escape(handle),
        email = escape(email),
    )
}

pub fn login(csrf: &str, error: Option<&str>, login: &str) -> String {
    format!(
        r#"<h1>Log in</h1>
{error}
<form method="post" action="/login">
{csrf}
<label for="login">Handle or email</label>
<input type="text" id="login" name="login" value="{login}" required maxlength="254" autocomplete="username">
<label for="password">Password</label>
<input type="password" id="password" name="password" required maxlength="128" autocomplete="current-password">
<button>Log in</button>
</form>
<p class="hint"><a href="/forgot">Forgot your password?</a></p>"#,
        error = message("error", error),
        csrf = csrf_field(csrf),
        login = escape(login),
    )
}

pub fn forgot(mail: bool) -> String {
    let how = if mail {
        "Password reset by email is not built yet. Ask an admin to reset your password."
    } else {
        "This server sends no email, so passwords cannot be reset by email. Ask an admin to reset yours; then log in \
         and change it on your account page."
    };
    format!("<h1>Forgot your password?</h1><p>{}</p>", escape(how))
}

fn when(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm), so no date crate is needed.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02} UTC", rest / 3600, rest % 3600 / 60)
}

pub fn account(
    csrf: &str,
    account: &AccountSummary,
    sessions: &[WebSession],
    notice: Option<&str>,
    error: Option<&str>,
    mail: bool,
) -> String {
    let roles = if account.roles.is_empty() { "player".to_string() } else { account.roles.join(", ") };
    let email = if account.email.is_empty() {
        "none".to_string()
    } else if mail {
        format!("{} (not verified)", account.email)
    } else {
        format!("{} (not verified: this server sends no email)", account.email)
    };
    let mut rows = String::new();
    for session in sessions {
        let action = if session.current {
            "this browser".to_string()
        } else {
            format!(
                r#"<form method="post" action="/account/sessions/revoke">{}<input type="hidden" name="session_id" value="{}"><button class="small">Revoke</button></form>"#,
                csrf_field(csrf),
                escape(&session.session_id)
            )
        };
        let _ = write!(
            rows,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{action}</td></tr>",
            when(session.created_at),
            when(session.last_seen_at),
            escape(&session.ip),
            escape(&session.user_agent),
        );
    }
    format!(
        r#"<h1>Your account</h1>
{notice}{error}
<dl>
<dt>Handle</dt><dd>{handle}</dd>
<dt>Email</dt><dd>{email}</dd>
<dt>Account</dt><dd>{id}</dd>
<dt>Roles</dt><dd>{roles}</dd>
<dt>Created</dt><dd>{created}</dd>
</dl>
<h2>Change password</h2>
<form method="post" action="/account/password">
{csrf_field}
<label for="current_password">Current password</label>
<input type="password" id="current_password" name="current_password" required maxlength="128" autocomplete="current-password">
<label for="new_password">New password</label>
<input type="password" id="new_password" name="new_password" required minlength="10" maxlength="128" autocomplete="new-password">
<p class="hint">Changing it logs out every other browser.</p>
<button>Change password</button>
</form>
<h2>Sessions</h2>
<table><thead><tr><th>Started</th><th>Last seen</th><th>Address</th><th>Browser</th><th></th></tr></thead>
<tbody>{rows}</tbody></table>"#,
        notice = message("notice", notice),
        error = message("error", error),
        handle = escape(&account.handle),
        email = escape(&email),
        id = account.account_id,
        roles = escape(&roles),
        created = when(account.created_at),
        csrf_field = csrf_field(csrf),
    )
}

pub struct Endpoint<'a> {
    pub host: &'a str,
    pub grpc_port: u16,
    pub hub_port: u16,
    pub has_ca: bool,
}

pub fn play(csrf: &str, handle: &str, endpoint: &Endpoint) -> String {
    let ca = if endpoint.has_ca {
        r#"<a class="button" href="/play/client.crt" download>Download client.crt</a>"#.to_string()
    } else {
        r#"<span class="hint">client.crt is not available on this server; ask an admin.</span>"#.to_string()
    };
    format!(
        r#"<h1>Play</h1>
<p>You play with your own copy of Star Citizen ({build_note}), started so that it connects to this server instead of
the official one. The game needs two files from here:</p>
<ul>
<li><strong>LoginData.json</strong>: your login for <strong>{handle}</strong>. It holds a token that lets the game act
as you for 30 days, so keep it private, and download a new one when it runs out.</li>
<li><strong>client.crt</strong>: this server's certificate authority, which the game must trust to connect.</li>
</ul>
<form method="post" action="/play/login-data">
{csrf}
<button>Download LoginData.json</button>
</form>
{ca}
<h2>Start the game</h2>
<ol>
<li>Make a private game folder: links to (or a copy of) your installed game's <code>Data.p4k</code>, <code>Bin64</code>
and the other files of the game folder, so the official folder and your real login stay untouched.</li>
<li>Put <code>LoginData.json</code> and <code>client.crt</code> in that folder and in its <code>Bin64</code>.</li>
<li>Start <code>Bin64\StarCitizen.exe</code> from that folder, with the environment variable
<code>GRPC_DEFAULT_SSL_ROOTS_FILE_PATH</code> set to the full path of <code>client.crt</code>, and these arguments:
<pre>-no_login_dialog -envtag PUB --client-login-show-dialog 0 --services-config-enabled 1
--system-trace-service-enabled 0 --grpc-client-endpoint-override https://{host}:{grpc}
--grpc-client-use-system-certificates 0 --grpc-client-use-native-dns-resolver 1 -no-vr</pre></li>
<li>The game then logs in as {handle} at <code>{host}</code> (services on port {grpc}, the Hub on port {hub}).</li>
</ol>
<p class="hint">A launcher for private servers does all of this. Never use these files or settings against the
official servers.</p>"#,
        build_note = "the build this server runs",
        handle = escape(handle),
        csrf = csrf_field(csrf),
        host = escape(endpoint.host),
        grpc = endpoint.grpc_port,
        hub = endpoint.hub_port,
    )
}

pub fn problem(title: &str, text: &str) -> String {
    format!("<h1>{}</h1><p>{}</p>", escape(title), escape(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_markup_and_attributes() {
        assert_eq!(escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
    }

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        assert_eq!(when(1_794_009_600_000), "2026-11-07 00:00 UTC");
        assert_eq!(when(951_825_600_000), "2000-02-29 12:00 UTC");
    }

    #[test]
    fn pages_escape_what_users_typed() {
        let page = register("t", Some("<bad>"), "<h>", "\"e\"", true);
        assert!(page.contains("&lt;bad&gt;") && page.contains("&lt;h&gt;") && page.contains("&quot;e&quot;"));
        assert!(!page.contains("<h>"));
    }
}

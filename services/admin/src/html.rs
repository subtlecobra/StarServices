//! Server-rendered pages: no script, a little CSS, and every value escaped.

use std::fmt::Write;

pub const CSS: &str = include_str!("admin.css");

/// Escapes text for an element or a quoted attribute.
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

/// Percent-encodes a query value.
pub fn query_value(text: &str) -> String {
    form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// Milliseconds since the epoch as UTC, `2026-11-07 12:00:00`. 0 or less is a dash.
pub fn time(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let seconds = ms.div_euclid(1000);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil from days, after Howard Hinnant.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Whole units with thousands separators.
pub fn amount(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 { format!("-{out}") } else { out }
}

/// What every page needs to draw its frame and its forms.
pub struct Frame<'a> {
    pub prefix: &'a str,
    pub admin_handle: &'a str,
    pub csrf: &'a str,
}

impl Frame<'_> {
    /// The hidden fields of a state-changing form: the CSRF token and a fresh request id, which makes a resubmission
    /// the same request.
    pub fn hidden(&self) -> String {
        format!(
            r#"<input type="hidden" name="x-csrf-token" value="{}"><input type="hidden" name="request_id" value="{}">"#,
            escape(self.csrf),
            crate::web::request_id()
        )
    }

    /// A form posting to `action` under the prefix.
    pub fn form(&self, action: &str, fields: &str, button: &str) -> String {
        format!(
            r#"<form method="post" action="{}{}" class="inline">{}{}<button type="submit">{}</button></form>"#,
            self.prefix,
            escape(action),
            self.hidden(),
            fields,
            escape(button)
        )
    }

    pub fn page(&self, title: &str, body: &str) -> String {
        let prefix = self.prefix;
        format!(
            r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - admin</title><link rel="stylesheet" href="{prefix}/static/admin.css"></head>
<body><header><a class="brand" href="{prefix}/">Admin</a> <a href="{prefix}/audit">Audit</a>
<span class="who">{handle}</span></header><main><h1>{title}</h1>
{body}</main></body></html>"#,
            title = escape(title),
            handle = escape(self.admin_handle),
        )
    }
}

/// The tabs of an account's pages.
pub fn account_tabs(prefix: &str, account_id: u32, current: &str) -> String {
    let mut out = String::from(r#"<nav class="tabs">"#);
    for (path, label) in [
        ("", "Overview"),
        ("/money", "Money"),
        ("/assets", "Assets"),
        ("/ships", "Ships"),
        ("/location", "Location"),
        ("/inventory", "Inventory"),
        ("/audit", "Audit"),
    ] {
        let class = if path == current { r#" class="current""# } else { "" };
        let _ = write!(out, r#"<a{class} href="{prefix}/accounts/{account_id}{path}">{label}</a>"#);
    }
    out.push_str("</nav>");
    out
}

/// A table of already escaped cells.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return r#"<p class="muted">None.</p>"#.into();
    }
    let mut out = String::from("<table><thead><tr>");
    for header in headers {
        let _ = write!(out, "<th>{}</th>", escape(header));
    }
    out.push_str("</tr></thead><tbody>");
    for row in rows {
        out.push_str("<tr>");
        for cell in row {
            let _ = write!(out, "<td>{cell}</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

pub fn notice(text: &str, error: bool) -> String {
    let class = if error { "notice error" } else { "notice" };
    format!(r#"<p class="{class}">{}</p>"#, escape(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_is_escaped() {
        assert_eq!(escape(r#"<a href="x">'&'</a>"#), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(time(0), "-");
        assert_eq!(time(1_000), "1970-01-01 00:00:01");
        assert_eq!(time(1_794_052_800_000), "2026-11-07 12:00:00");
        assert_eq!(time(951_782_400_000), "2000-02-29 00:00:00");
    }

    #[test]
    fn amounts_are_grouped() {
        assert_eq!(amount(0), "0");
        assert_eq!(amount(1_234_567), "1,234,567");
        assert_eq!(amount(-1_000), "-1,000");
        assert_eq!(amount(999), "999");
    }
}

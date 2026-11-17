//! Cookies, CSRF, client addresses, rate limits and response headers.
//!
//! - The session is the `sc_session` cookie: an opaque token from sc-identity, `HttpOnly; Secure; SameSite=Lax`.
//! - CSRF is a double submit: a random `sc_csrf` cookie, which every form repeats in its `csrf` field and every API call
//!   in its `x-csrf-token` header. A cross-site page can make the browser send the cookie but cannot read it. A request
//!   with an `Origin` must also come from this site. sc-admin-web uses the same cookie and the same check.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::rand::{SecureRandom, SystemRandom};

pub const SESSION_COOKIE: &str = "sc_session";
pub const CSRF_COOKIE: &str = "sc_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The session cookie lives as long as the session can, 30 days; sc-identity ends it sooner when it idles.
const SESSION_MAX_AGE: u32 = 30 * 24 * 3600;

/// The value of cookie `name`, from every `Cookie` header.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).expect("system randomness");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 32 bytes, base64url: what `random_token` and sc-identity issue. Anything else is ignored, so a cookie can never
/// carry more into a page or a header.
pub fn well_formed(token: &str) -> bool {
    token.len() == 43 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_MAX_AGE}; HttpOnly; Secure; SameSite=Lax")
}

pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Readable by scripts on purpose, so that a page's script can send it as `x-csrf-token`.
pub fn csrf_cookie(token: &str) -> String {
    format!("{CSRF_COOKIE}={token}; Path=/; Secure; SameSite=Strict")
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// The double submit: the cookie, and the same value in the form or header.
pub fn csrf_ok(headers: &HeaderMap, submitted: Option<&str>) -> bool {
    let Some(cookie) = cookie(headers, CSRF_COOKIE).filter(|token| well_formed(token)) else { return false };
    let header = headers.get(CSRF_HEADER).and_then(|value| value.to_str().ok());
    submitted.or(header).is_some_and(|submitted| same(submitted.as_bytes(), cookie.as_bytes()))
}

/// No `Origin` (an old browser, or a tool) passes, as the token still has to match. One from elsewhere does not.
pub fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()) else { return true };
    let Some(host) = headers.get(header::HOST).and_then(|value| value.to_str().ok()) else { return false };
    let authority = origin.split_once("://").map_or("", |(_, rest)| rest);
    let site = matches!(headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()), None | Some("same-origin"));
    authority.eq_ignore_ascii_case(host) && site
}

/// The client's address. Envoy appends the address it was reached from to `X-Forwarded-For`, so with `hops` trusted
/// proxies in front the client is that many entries from the end; anything before them is the client's own claim.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, hops: usize) -> String {
    if hops == 0 {
        return peer.ip().to_string();
    }
    let forwarded: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    match forwarded.len().checked_sub(hops) {
        Some(index) => forwarded[index].parse::<std::net::IpAddr>().map_or_else(|_| peer.ip().to_string(), |ip| ip.to_string()),
        None => peer.ip().to_string(),
    }
}

/// A fixed window per key, in memory: each replica counts its own requests. sc-identity keeps the limits that matter
/// (failed logins, registrations) in its database, for every replica at once.
pub struct Limiter {
    max: u32,
    window: Duration,
    counts: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Limiter {
    pub fn new(max: u32, window: Duration) -> Self {
        Self { max, window, counts: Mutex::new(HashMap::new()) }
    }

    /// Counts one request, and says whether it is within the limit.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut counts = self.counts.lock().unwrap();
        if counts.len() > 100_000 {
            counts.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = counts.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max
    }
}

/// On every response: no framing, no sniffing, no inline script or style, nothing cached but the assets.
pub async fn headers(request: Request<Body>, next: Next) -> Response {
    let asset = request.uri().path().starts_with("/assets/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let set = |headers: &mut HeaderMap, name: &'static str, value: &'static str| {
        headers.insert(name, HeaderValue::from_static(value));
    };
    set(
        headers,
        "content-security-policy",
        "default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    set(headers, "x-frame-options", "DENY");
    set(headers, "x-content-type-options", "nosniff");
    set(headers, "referrer-policy", "same-origin");
    set(headers, "cross-origin-opener-policy", "same-origin");
    set(headers, "strict-transport-security", "max-age=31536000");
    if asset {
        set(headers, "cache-control", "public, max-age=3600");
    } else {
        set(headers, "cache-control", "no-store");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn cookies_are_found_among_others() {
        let found = headers(&[("cookie", "a=1; sc_session=abc"), ("cookie", "sc_csrf=xyz")]);
        assert_eq!(cookie(&found, SESSION_COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&found, CSRF_COOKIE).as_deref(), Some("xyz"));
        assert_eq!(cookie(&found, "sc"), None);
    }

    #[test]
    fn csrf_needs_the_cookie_and_the_same_value_submitted() {
        let token = random_token();
        let with_cookie = headers(&[("cookie", &format!("sc_csrf={token}"))]);
        assert!(csrf_ok(&with_cookie, Some(&token)));
        assert!(!csrf_ok(&with_cookie, Some(&random_token())));
        assert!(!csrf_ok(&with_cookie, None));
        let with_header = headers(&[("cookie", &format!("sc_csrf={token}")), ("x-csrf-token", &token)]);
        assert!(csrf_ok(&with_header, None));
        assert!(!csrf_ok(&headers(&[("x-csrf-token", &token)]), Some(&token)), "no cookie, no pass");
        let short = headers(&[("cookie", "sc_csrf=abc")]);
        assert!(!csrf_ok(&short, Some("abc")), "only tokens as issued count");
    }

    #[test]
    fn origins_must_be_this_site() {
        assert!(origin_ok(&headers(&[("host", "127.0.0.2:50051")])));
        assert!(origin_ok(&headers(&[("host", "127.0.0.2:50051"), ("origin", "https://127.0.0.2:50051")])));
        assert!(!origin_ok(&headers(&[("host", "127.0.0.2:50051"), ("origin", "https://evil.example")])));
        assert!(!origin_ok(&headers(&[
            ("host", "127.0.0.2:50051"),
            ("origin", "https://127.0.0.2:50051"),
            ("sec-fetch-site", "cross-site"),
        ])));
    }

    #[test]
    fn the_client_is_the_entry_the_trusted_proxy_added() {
        let peer: SocketAddr = "10.244.0.5:4000".parse().unwrap();
        let forged = headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9")]);
        assert_eq!(client_ip(&forged, peer, 1), "203.0.113.9");
        assert_eq!(client_ip(&forged, peer, 0), "10.244.0.5");
        assert_eq!(client_ip(&forged, peer, 3), "10.244.0.5");
        assert_eq!(client_ip(&headers(&[("x-forwarded-for", "junk")]), peer, 1), "10.244.0.5");
    }

    #[test]
    fn the_limiter_counts_per_key_within_the_window() {
        let limiter = Limiter::new(2, Duration::from_secs(60));
        assert!(limiter.allow("a") && limiter.allow("a"));
        assert!(!limiter.allow("a"));
        assert!(limiter.allow("b"));
    }
}

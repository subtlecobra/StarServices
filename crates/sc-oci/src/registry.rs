//! Enough of the OCI distribution API to pull an image anonymously: a manifest by tag (an index or manifest list picks
//! the linux/amd64 image, or the first that is not an attestation), its layers as blobs. A 401 with a `Bearer` challenge
//! is answered with an anonymous token from the challenge's realm, as Docker Hub, ECR public, Artifact Registry and Harbor
//! ask. Redirects are followed, without the token when they leave the registry (blob storage on S3 or GCS). Every blob is
//! checked against its digest.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use http::{header, Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Asked for in this order of preference; podman pushes either the Docker or the OCI ones.
const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";
/// A manifest is a few kilobytes; this bounds what a broken registry can make a caller hold.
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

#[derive(Deserialize)]
struct Platform {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    os: String,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<Platform>,
}

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion", default)]
    schema_version: u32,
    /// An index or manifest list.
    #[serde(default)]
    manifests: Option<Vec<Descriptor>>,
    #[serde(default)]
    layers: Option<Vec<Descriptor>>,
}

/// A layer of an image: a tar, gzip compressed or not.
#[derive(Clone, Debug)]
pub struct Layer {
    pub digest: String,
    pub size: u64,
}

#[derive(Debug)]
pub struct Image {
    /// The digest of what the tag names: the index if it is one, else the manifest.
    pub digest: String,
    pub layers: Vec<Layer>,
}

#[derive(Clone)]
pub struct Registry {
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    /// `scheme://host:port`.
    origin: String,
    repository: String,
    /// The anonymous pull token, once a challenge asked for one.
    token: Arc<Mutex<Option<String>>>,
}

fn origin_of(uri: &Uri) -> String {
    format!("{}://{}", uri.scheme_str().unwrap_or("http"), uri.authority().map_or("", |a| a.as_str()))
}

/// The parameters of a `Bearer realm="…",service="…",scope="…"` challenge.
fn bearer_challenge(value: &str) -> Option<Vec<(String, String)>> {
    let rest = value.trim().strip_prefix("Bearer ").or_else(|| value.trim().strip_prefix("bearer "))?;
    let mut parameters = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.peek().copied().filter(|c| *c != ',') {
                value.push(c);
                chars.next();
            }
        }
        parameters.push((key.trim().to_ascii_lowercase(), value));
    }
    Some(parameters)
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b':' | b'/' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

impl Registry {
    /// `registry` is `host:port`, a scheme in front is allowed and wins over `plain_http`.
    pub fn new(registry: &str, repository: &str, plain_http: bool) -> Result<Self, String> {
        let origin = if registry.starts_with("http://") || registry.starts_with("https://") {
            registry.trim_end_matches('/').to_string()
        } else {
            format!("{}://{}", if plain_http { "http" } else { "https" }, registry.trim_end_matches('/'))
        };
        let connector = HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(Arc::new(rustls::crypto::ring::default_provider()))
            .map_err(|error| format!("TLS roots: {error}"))?
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            origin,
            repository: repository.trim_matches('/').to_string(),
            token: Arc::default(),
        })
    }

    async fn send(&self, method: &Method, url: &str, accept: Option<&str>, authorize: bool) -> Result<Response<Incoming>, String> {
        let mut request = Request::builder().method(method.clone()).uri(url).header(header::USER_AGENT, "scbackend");
        if let Some(accept) = accept {
            request = request.header(header::ACCEPT, accept);
        }
        if authorize {
            if let Some(token) = self.token.lock().unwrap().as_ref() {
                request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
        }
        let request = request.body(Empty::new()).map_err(|error| format!("{url}: {error}"))?;
        self.client.request(request).await.map_err(|error| format!("{url}: {error}"))
    }

    /// Gets an anonymous token as a `WWW-Authenticate: Bearer` challenge asks.
    async fn authenticate(&self, challenge: &str) -> Result<(), String> {
        let parameters = bearer_challenge(challenge).ok_or_else(|| format!("registry asks for credentials: {challenge}"))?;
        let get = |key: &str| parameters.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let realm = get("realm").ok_or("bearer challenge without a realm")?;
        let scope = get("scope").map_or_else(|| format!("repository:{}:pull", self.repository), str::to_string);
        let mut url = format!("{realm}{}scope={}", if realm.contains('?') { '&' } else { '?' }, query_escape(&scope));
        if let Some(service) = get("service") {
            url.push_str(&format!("&service={}", query_escape(service)));
        }
        let response = self.send(&Method::GET, &url, None, false).await?;
        if !response.status().is_success() {
            return Err(format!("token from {realm}: HTTP {}", response.status()));
        }
        let body = read_small(response).await?;
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let token: Token = serde_json::from_slice(&body).map_err(|error| format!("token from {realm}: {error}"))?;
        let token = token.token.or(token.access_token).ok_or_else(|| format!("token from {realm}: none in the answer"))?;
        *self.token.lock().unwrap() = Some(token);
        Ok(())
    }

    /// GET `/v2/<repository>/<path>`, through a token challenge and redirects. Only a success is returned.
    async fn get(&self, path: &str, accept: Option<&str>) -> Result<Response<Incoming>, String> {
        self.request(&Method::GET, path, accept).await
    }

    /// As `get`, with another method.
    async fn request(&self, method: &Method, path: &str, accept: Option<&str>) -> Result<Response<Incoming>, String> {
        let mut url = format!("{}/v2/{}/{path}", self.origin, self.repository);
        let mut authorize = true;
        let mut authenticated = false;
        for _ in 0..=MAX_REDIRECTS {
            let response = self.send(method, &url, accept, authorize).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && authorize && !authenticated {
                let challenge = response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| format!("{url}: 401 without a challenge"))?
                    .to_string();
                self.authenticate(&challenge).await?;
                authenticated = true;
                continue;
            }
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| format!("{url}: {status} without a location"))?;
                url = if location.starts_with('/') {
                    format!("{}{location}", origin_of(&url.parse::<Uri>().map_err(|e| e.to_string())?))
                } else {
                    location.to_string()
                };
                // Blob storage gets a signed URL; the registry's token is not for it.
                authorize = url.parse::<Uri>().is_ok_and(|uri| origin_of(&uri) == self.origin);
                continue;
            }
            if !status.is_success() {
                return Err(format!("{url}: HTTP {status}"));
            }
            return Ok(response);
        }
        Err(format!("{url}: too many redirects"))
    }

    async fn manifest(&self, reference: &str) -> Result<(Manifest, String), String> {
        let body = read_small(self.get(&format!("manifests/{reference}"), Some(MANIFEST_TYPES)).await?).await?;
        let digest = sha256_digest(&body);
        if reference.starts_with("sha256:") && reference != digest {
            return Err(format!("manifest {reference} has digest {digest}"));
        }
        let manifest: Manifest = serde_json::from_slice(&body).map_err(|error| format!("manifest {reference}: {error}"))?;
        if manifest.schema_version != 2 {
            return Err(format!("manifest {reference}: schema version {}", manifest.schema_version));
        }
        Ok((manifest, digest))
    }

    /// The digest of what `tag` names now, as `image` would give it: from a HEAD's `Docker-Content-Digest`, which
    /// registries send for a manifest, else from the manifest itself. Lets a replica notice a build pushed again under the
    /// same tag without pulling it.
    pub async fn digest(&self, tag: &str) -> Result<String, String> {
        let response = self.request(&Method::HEAD, &format!("manifests/{tag}"), Some(MANIFEST_TYPES)).await?;
        let header = response.headers().get("docker-content-digest").and_then(|value| value.to_str().ok());
        match header.filter(|digest| digest.starts_with("sha256:") && digest.len() == 71) {
            Some(digest) => Ok(digest.to_string()),
            None => Ok(self.manifest(tag).await?.1),
        }
    }

    /// What `tag` names, down to the layers of one image.
    pub async fn image(&self, tag: &str) -> Result<Image, String> {
        let (mut manifest, digest) = self.manifest(tag).await?;
        if let Some(manifests) = manifest.manifests.take() {
            let platform = |d: &Descriptor, os: &str, architecture: Option<&str>| {
                d.platform.as_ref().is_some_and(|p| p.os == os && architecture.is_none_or(|a| p.architecture == a))
            };
            let chosen = manifests
                .iter()
                .find(|d| platform(d, "linux", Some("amd64")))
                .or_else(|| manifests.iter().find(|d| !platform(d, "unknown", None)))
                .ok_or_else(|| format!("{tag}: empty index"))?;
            manifest = self.manifest(&chosen.digest).await?.0;
        }
        let layers = manifest.layers.ok_or_else(|| format!("{tag}: manifest without layers"))?;
        let layers = layers.into_iter().map(|d| Layer { digest: d.digest, size: d.size }).collect();
        Ok(Image { digest, layers })
    }

    /// Downloads a blob to `path`, checking its size and digest as it streams. A blob that does not match is deleted.
    pub async fn blob_to_file(&self, layer: &Layer, path: &Path) -> Result<(), String> {
        let expected = layer.digest.strip_prefix("sha256:").ok_or_else(|| format!("unsupported digest {}", layer.digest))?;
        let mut body = self.get(&format!("blobs/{}", layer.digest), None).await?.into_body();
        let mut file = tokio::fs::File::create(path).await.map_err(|error| format!("{}: {error}", path.display()))?;
        let (mut hasher, mut size) = (Sha256::new(), 0u64);
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| format!("blob {}: {error}", layer.digest))?;
            if let Some(data) = frame.data_ref() {
                hasher.update(data);
                size += data.len() as u64;
                file.write_all(data).await.map_err(|error| format!("{}: {error}", path.display()))?;
            }
        }
        file.flush().await.map_err(|error| format!("{}: {error}", path.display()))?;
        drop(file);
        let actual = hex(&hasher.finalize());
        if actual != expected || (layer.size != 0 && size != layer.size) {
            let _ = tokio::fs::remove_file(path).await;
            return Err(format!("blob {} is sha256:{actual}, {size} bytes of {}", layer.digest, layer.size));
        }
        Ok(())
    }
}

async fn read_small(response: Response<Incoming>) -> Result<Bytes, String> {
    let limited = http_body_util::Limited::new(response.into_body(), MAX_MANIFEST_BYTES);
    Ok(limited.collect().await.map_err(|error| error.to_string())?.to_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_parse() {
        let parameters =
            bearer_challenge(r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull""#)
                .unwrap();
        assert_eq!(parameters[0], ("realm".into(), "https://auth.example/token".into()));
        assert_eq!(parameters[1], ("service".into(), "registry.example".into()));
        assert_eq!(parameters[2], ("scope".into(), "repository:a/b:pull".into()));
        assert!(bearer_challenge(r#"Basic realm="x""#).is_none());
    }
}

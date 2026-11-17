//! ArangoDB, over its HTTP API.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::Bytes;
use http::{header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use serde_json::{json, Value};
use tracing::{info, warn};

const DATABASE: &str = "scbackend";
const CONNECT_ATTEMPTS: u32 = 60;
const CONNECT_RETRY: Duration = Duration::from_secs(2);
const SHARDS: u32 = 6;
const REPLICATION_FACTOR: u32 = 2;

#[derive(Clone)]
pub struct Arango {
    base: String,
    authorization: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl Arango {
    /// A handle that has not contacted the server, and creates nothing. For tests of what fails before a query.
    pub fn lazy(url: &str) -> Self {
        Self {
            base: url.trim_end_matches('/').to_string(),
            authorization: format!("Basic {}", STANDARD.encode("root:")),
            client: Client::builder(TokioExecutor::new()).build_http(),
        }
    }

    /// Connects, waiting for the database while the cluster starts, and creates the collections
    /// that are missing. An edge collection is named with a `edge:` prefix.
    pub async fn connect(url: &str, collections: &[&str]) -> Result<Self, String> {
        let db = Self {
            base: url.trim_end_matches('/').to_string(),
            authorization: format!("Basic {}", STANDARD.encode("root:")),
            client: Client::builder(TokioExecutor::new()).build_http(),
        };
        for attempt in 1..=CONNECT_ATTEMPTS {
            match db.prepare(collections).await {
                Ok(()) => {
                    info!("arango ready");
                    return Ok(db);
                }
                Err(error) if attempt < CONNECT_ATTEMPTS => {
                    warn!(%error, attempt, "arango not ready");
                    tokio::time::sleep(CONNECT_RETRY).await;
                }
                Err(error) => return Err(format!("arango: {error}")),
            }
        }
        unreachable!("the last attempt returns")
    }

    async fn prepare(&self, collections: &[&str]) -> Result<(), String> {
        let (status, body) = self
            .call(Method::POST, "/_db/_system/_api/database", Some(json!({ "name": DATABASE })))
            .await?;
        if !status.is_success() && status != StatusCode::CONFLICT {
            return Err(format!("create database: {status} {body}"));
        }
        for collection in collections {
            let (name, kind) = match collection.strip_prefix("edge:") {
                Some(name) => (name, 3),
                None => (*collection, 2),
            };
            let spec = json!({
                "name": name,
                "type": kind,
                "numberOfShards": SHARDS,
                "replicationFactor": REPLICATION_FACTOR,
            });
            let (status, body) = self.call(Method::POST, &self.path("/_api/collection"), Some(spec)).await?;
            if !status.is_success() && status != StatusCode::CONFLICT {
                return Err(format!("create collection {name}: {status} {body}"));
            }
        }
        Ok(())
    }

    fn path(&self, api: &str) -> String {
        format!("/_db/{DATABASE}{api}")
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, Value), String> {
        self.send(method, path, body, None).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        transaction: Option<&str>,
    ) -> Result<(StatusCode, Value), String> {
        let bytes = body.map_or_else(Bytes::new, |value| Bytes::from(value.to_string()));
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base))
            .header(header::AUTHORIZATION, &self.authorization)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(id) = transaction {
            request = request.header("x-arango-trx-id", id);
        }
        let request = request.body(Full::new(bytes)).map_err(|e| e.to_string())?;
        let response = self.client.request(request).await.map_err(|e| e.to_string())?;
        let status = response.status();
        let data = response.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        Ok((status, serde_json::from_slice(&data).unwrap_or(Value::Null)))
    }

    /// Creates a persistent index on `fields` of `collection` unless one exists already (ArangoDB answers an
    /// identical index with the existing one). A sparse index leaves out documents where a field is null or missing.
    pub async fn ensure_index(&self, collection: &str, fields: &[&str], sparse: bool) -> Result<(), String> {
        let spec = json!({ "type": "persistent", "fields": fields, "sparse": sparse, "unique": false });
        let path = self.path(&format!("/_api/index?collection={collection}"));
        let (status, body) = self.call(Method::POST, &path, Some(spec)).await?;
        if !status.is_success() {
            return Err(format!("index {collection} {fields:?}: {status} {body}"));
        }
        Ok(())
    }

    /// Runs an AQL query and returns every result. Traversals must start with `WITH <collections>`.
    pub async fn aql(&self, query: &str, bind: Value) -> Result<Vec<Value>, String> {
        self.aql_in(query, bind, None).await
    }

    async fn aql_in(&self, query: &str, bind: Value, transaction: Option<&str>) -> Result<Vec<Value>, String> {
        let body = json!({ "query": query, "bindVars": bind, "batchSize": 10000 });
        let (status, reply) = self.send(Method::POST, &self.path("/_api/cursor"), Some(body), transaction).await?;
        if !status.is_success() {
            return Err(format!("aql: {status} {}", reply["errorMessage"]));
        }
        let mut results = reply["result"].as_array().cloned().unwrap_or_default();
        let mut more = reply["hasMore"].as_bool().unwrap_or(false);
        let id = reply["id"].as_str().unwrap_or_default().to_string();
        while more {
            let path = self.path(&format!("/_api/cursor/{id}"));
            let (status, next) = self.send(Method::PUT, &path, None, transaction).await?;
            if !status.is_success() {
                return Err(format!("aql cursor: {status}"));
            }
            results.extend(next["result"].as_array().cloned().unwrap_or_default());
            more = next["hasMore"].as_bool().unwrap_or(false);
        }
        Ok(results)
    }

    /// Runs `queries` as one atomic transaction across shards: all commit, or none do.
    pub async fn transaction(&self, write: &[&str], queries: &[(&str, Value)]) -> Result<(), String> {
        let spec = json!({ "collections": { "write": write } });
        let (status, reply) = self.call(Method::POST, &self.path("/_api/transaction/begin"), Some(spec)).await?;
        if !status.is_success() {
            return Err(format!("begin transaction: {status} {}", reply["errorMessage"]));
        }
        let id = reply["result"]["id"].as_str().unwrap_or_default().to_string();
        let path = self.path(&format!("/_api/transaction/{id}"));
        for (query, bind) in queries {
            if let Err(error) = self.aql_in(query, bind.clone(), Some(&id)).await {
                let _ = self.call(Method::DELETE, &path, None).await;
                return Err(error);
            }
        }
        let (status, reply) = self.call(Method::PUT, &path, None).await?;
        if !status.is_success() {
            return Err(format!("commit: {status} {}", reply["errorMessage"]));
        }
        Ok(())
    }
}

//! sc.external.services.blueprint_library.v1.BlueprintLibraryService. The client filters on what an
//! entry used to carry, which the proto no longer returns, so those stay columns. `sc-admin seed` hands
//! `blueprint_defaults`, the blueprints every player knows, to `BlueprintLibraryInternal` (seed.rs), and a
//! player receives them on first query.

mod seed;

use std::{
    collections::HashSet,
    net::SocketAddr,
    process::ExitCode,
    sync::{Arc, Mutex},
};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row, ToSql};
use sc_economy::{
    query::{compile, Compiled, Field, Kind},
    reply_stream, timestamp_of, ReplyStream,
};
use sc_api::internal::blueprint_library_internal_server::BlueprintLibraryInternalServer;
use sc_proto::{
    common::api::v1::Query,
    services::blueprint_library::v1::{
    blueprint_library_service_server::{BlueprintLibraryService, BlueprintLibraryServiceServer},
    BlueprintEntry, BlueprintProcessType, BlueprintSource, QueryBlueprintEntriesRequest,
    QueryBlueprintEntriesResponse, QueryBlueprintEntriesStreamRequest, QueryBlueprintEntriesStreamResponse,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, metadata::MetadataMap, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS blueprint_entries (
    player_id BIGINT NOT NULL,
    blueprint_id TEXT NOT NULL,
    tier INT NOT NULL DEFAULT 0,
    remaining_uses INT NOT NULL DEFAULT -1,
    source INT NOT NULL DEFAULT 0,
    last_used_at TIMESTAMPTZ,
    category_id TEXT NOT NULL DEFAULT '',
    item_class_id TEXT NOT NULL DEFAULT '',
    process_type INT NOT NULL DEFAULT 0,
    PRIMARY KEY (player_id, blueprint_id)
);
CREATE TABLE IF NOT EXISTS blueprint_defaults (
    blueprint_id TEXT PRIMARY KEY,
    category_id TEXT NOT NULL DEFAULT '',
    item_class_id TEXT NOT NULL DEFAULT '',
    process_type INT NOT NULL DEFAULT 0
);";
const GRANT_DEFAULTS: &str = "
INSERT INTO blueprint_entries (player_id, blueprint_id, source, category_id, item_class_id, process_type)
SELECT $1::bigint, blueprint_id, $2::int, category_id, item_class_id, process_type FROM blueprint_defaults
ON CONFLICT DO NOTHING";

const FIELDS: &[Field] = &[
    Field { property: "blueprint_id", expression: "blueprint_id", kind: Kind::Text },
    Field { property: "tier", expression: "tier", kind: Kind::Number },
    Field { property: "remaining_uses", expression: "remaining_uses", kind: Kind::Number },
    Field {
        property: "source",
        expression: "source",
        kind: Kind::Enum(|name| BlueprintSource::from_str_name(name).map(|value| value as i32)),
    },
    Field {
        property: "last_used_at",
        expression: "(extract(epoch FROM last_used_at) * 1000)::bigint",
        kind: Kind::Number,
    },
    Field { property: "category_id", expression: "category_id", kind: Kind::Text },
    Field { property: "item_class_id", expression: "item_class_id", kind: Kind::Text },
    Field {
        property: "process_type",
        expression: "process_type",
        kind: Kind::Enum(|name| BlueprintProcessType::from_str_name(name).map(|value| value as i32)),
    },
    Field { property: "player_id", expression: "player_id", kind: Kind::Number },
];

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Library {
    state: State,
    db: Postgres,
    granted: Mutex<HashSet<u64>>,
}

fn entry(row: &Row) -> BlueprintEntry {
    BlueprintEntry {
        blueprint_id: row.get("blueprint_id"),
        tier: row.get::<_, i32>("tier") as u32,
        remaining_uses: row.get("remaining_uses"),
        source: row.get("source"),
        last_used_at: timestamp_of(row, "last_used_ms"),
    }
}

impl Library {
    async fn player(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        let account = caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        if !self.granted.lock().unwrap().contains(&account.geid) {
            let source = BlueprintSource::Gameplay as i32;
            self.db.execute(GRANT_DEFAULTS, &[&(account.geid as i64), &source]).await.map_err(internal)?;
            let seeded = self.db.query_opt("SELECT 1 FROM blueprint_defaults LIMIT 1", &[]).await.map_err(internal)?;
            if seeded.is_some() {
                self.granted.lock().unwrap().insert(account.geid);
            }
        }
        Ok(account)
    }

    async fn entries(&self, geid: u64, query: Option<&Query>) -> Result<(Compiled, Vec<BlueprintEntry>), Status> {
        let compiled = compile(query, FIELDS, 2, "blueprint_id")?;
        let geid = geid as i64;
        let mut parameters: Vec<&(dyn ToSql + Sync)> = vec![&geid];
        parameters.extend(compiled.parameters.iter().map(|value| value as &(dyn ToSql + Sync)));
        let sql = format!(
            "SELECT blueprint_id, tier, remaining_uses, source,
                    (extract(epoch FROM last_used_at) * 1000)::bigint AS last_used_ms
             FROM blueprint_entries WHERE player_id = $1 AND {} {}",
            compiled.condition, compiled.tail
        );
        let rows = self.db.query(&sql, &parameters).await.map_err(internal)?;
        let entries = rows.iter().map(entry).collect();
        Ok((compiled, entries))
    }
}

#[tonic::async_trait]
impl BlueprintLibraryService for Library {
    type QueryBlueprintEntriesStreamStream = ReplyStream<QueryBlueprintEntriesStreamResponse>;

    async fn query_blueprint_entries(
        &self,
        request: Request<QueryBlueprintEntriesRequest>,
    ) -> Result<Response<QueryBlueprintEntriesResponse>, Status> {
        let account = self.player(request.metadata()).await?;
        let query = request.into_inner().query;
        let (compiled, mut results) = self.entries(account.geid, query.as_ref()).await?;
        let page_info = compiled.page_of(&mut results);
        Ok(Response::new(QueryBlueprintEntriesResponse { page_info: Some(page_info), results }))
    }

    async fn query_blueprint_entries_stream(
        &self,
        request: Request<QueryBlueprintEntriesStreamRequest>,
    ) -> Result<Response<Self::QueryBlueprintEntriesStreamStream>, Status> {
        let account = self.player(request.metadata()).await?;
        let mut query = request.into_inner().query.unwrap_or_default();
        let mut entries = Vec::new();
        // A stream has no way to ask for the next page, so it walks them all.
        loop {
            let (compiled, mut page) = self.entries(account.geid, Some(&query)).await?;
            let info = compiled.page_of(&mut page);
            entries.append(&mut page);
            if !info.has_next_page {
                break;
            }
            query.pagination.get_or_insert_with(Default::default).after = info.end_cursor;
        }
        Ok(Response::new(reply_stream(entries, |entries| QueryBlueprintEntriesStreamResponse { entries })))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Library {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        granted: Mutex::default(),
    });
    let server = BlueprintLibraryServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = BlueprintLibraryInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "blueprint library listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

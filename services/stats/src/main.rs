//! sc.external.services.stats.v1.StatsService. Stats are written outside this API, so this binary only
//! reads them.


use std::{net::SocketAddr, process::ExitCode};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_proto::services::stats::v1::{
    stats_service_server::{StatsService, StatsServiceServer},
    FindStatsRequest, FindStatsResponse, RollingAverage, Stat,
};
use sc_runtime::player_urn;
use sc_runtime::query;
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS stats (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    subject TEXT NOT NULL,
    stat_def_id TEXT NOT NULL,
    season TEXT NOT NULL DEFAULT '',
    value BIGINT NOT NULL DEFAULT 0,
    best BIGINT NOT NULL DEFAULT 0,
    type TEXT NOT NULL DEFAULT '',
    unit TEXT NOT NULL DEFAULT '',
    game_mode TEXT NOT NULL DEFAULT '',
    category TEXT NOT NULL DEFAULT '',
    rolling_avg BYTEA,
    version BIGINT NOT NULL DEFAULT 1,
    rankable BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (subject, stat_def_id, season)
);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Stats {
    state: State,
    db: Postgres,
}

fn stat(row: &Row) -> Result<Stat, Status> {
    let rolling: Option<Vec<u8>> = row.get("rolling_avg");
    Ok(Stat {
        id: row.get("id"),
        subject: row.get("subject"),
        stat_def_id: row.get("stat_def_id"),
        season: row.get("season"),
        value: row.get::<_, i64>("value") as u32,
        r#type: row.get("type"),
        unit: row.get("unit"),
        game_mode: row.get("game_mode"),
        category: row.get("category"),
        best: row.get::<_, i64>("best") as u32,
        rolling_avg: rolling.map(|bytes| RollingAverage::decode(bytes.as_slice())).transpose().map_err(internal)?,
        version: row.get::<_, i64>("version") as u32,
        rankable: row.get("rankable"),
    })
}

fn property(stat: &Stat, name: &str) -> Option<String> {
    Some(match name {
        "id" => stat.id.clone(),
        "subject" => stat.subject.clone(),
        "stat_def_id" => stat.stat_def_id.clone(),
        "season" => stat.season.clone(),
        "value" => stat.value.to_string(),
        "best" => stat.best.to_string(),
        "type" => stat.r#type.clone(),
        "unit" => stat.unit.clone(),
        "game_mode" => stat.game_mode.clone(),
        "category" => stat.category.clone(),
        "rankable" => stat.rankable.to_string(),
        _ => return None,
    })
}

#[tonic::async_trait]
impl StatsService for Stats {
    /// Reads the subjects the filter names, or the caller's own stats when it names none.
    async fn find_stats(&self, request: Request<FindStatsRequest>) -> Result<Response<FindStatsResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let query = request.into_inner().query.unwrap_or_default();
        let mut subjects = query::required(query.filter.as_ref(), "subject");
        if subjects.is_empty() {
            subjects.push(player_urn(account.geid));
        }
        let rows = self
            .db
            .query(
                "SELECT id::text AS id, subject, stat_def_id, season, value, best, type, unit, game_mode, category,
                        rolling_avg, version, rankable
                 FROM stats WHERE subject = ANY($1) ORDER BY subject, stat_def_id, season",
                &[&subjects],
            )
            .await
            .map_err(internal)?;
        let mut stats = rows.iter().map(stat).collect::<Result<Vec<_>, _>>()?;
        stats.retain(|stat| query::matches(query.filter.as_ref(), &|name| property(stat, name)));
        query::sort(&mut stats, query.sort.as_ref(), property);
        let (results, page_info) = query::page(stats, query.pagination.as_ref())?;
        Ok(Response::new(FindStatsResponse { results, page_info: Some(page_info) }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Stats {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    };
    let server = StatsServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "stats listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

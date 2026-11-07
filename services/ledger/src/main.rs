//! sc.external.services.ledger.v1.LedgerService. Every player has an aUEC and a REC ledger, opened on first
//! use. The wallet reads an unfiltered reply by `Funds.currency`, while a filtered read takes the first
//! ledger, so aUEC comes first and the names are the client's.
//!
//! This service owns `ledgers`. Other services charge through `LedgerInternal` (internal.rs), whose
//! operations are keyed so that a retry never charges twice. The admin site reads balances and history and adjusts
//! them through it too, keyed the same way.

mod internal;

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_economy::ledger::{AUEC_LEDGER, OPENING_AUEC, OPENING_REC, REC_LEDGER};
use sc_proto::{
    internal::ledger_internal_server::LedgerInternalServer,
    services::ledger::v1::{
        ledger_service_server::{LedgerService, LedgerServiceServer},
        Currency, Funds, GetFundsRequest, GetFundsResponse, Ledger,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ledgers (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_geid BIGINT NOT NULL,
    account_id BIGINT NOT NULL,
    name TEXT NOT NULL,
    amount BIGINT NOT NULL CHECK (amount >= 0),
    currency INT NOT NULL,
    version BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_geid, name)
);
UPDATE ledgers SET name = 'AUEC_LEDGER'
WHERE name = 'default' AND NOT EXISTS (
    SELECT 1 FROM ledgers other WHERE other.owner_geid = ledgers.owner_geid AND other.name = 'AUEC_LEDGER'
);
CREATE TABLE IF NOT EXISTS ledger_operations (
    key TEXT PRIMARY KEY,
    owner_geid BIGINT NOT NULL,
    ledger_name TEXT NOT NULL,
    amount BIGINT NOT NULL,
    state TEXT NOT NULL,
    version BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE ledger_operations ADD COLUMN IF NOT EXISTS reason TEXT;
ALTER TABLE ledger_operations ADD COLUMN IF NOT EXISTS actor TEXT;
ALTER TABLE ledger_operations ADD COLUMN IF NOT EXISTS balance_before BIGINT;
ALTER TABLE ledger_operations ADD COLUMN IF NOT EXISTS balance_after BIGINT;
CREATE INDEX IF NOT EXISTS ledger_operations_by_owner ON ledger_operations (owner_geid, created_at);";

const OPENED: [(&str, i64, Currency); 2] =
    [(AUEC_LEDGER, OPENING_AUEC, Currency::Auec), (REC_LEDGER, OPENING_REC, Currency::Rec)];

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
}

struct Ledgers {
    state: State,
    db: Postgres,
}

fn millis(row: &Row, column: &str) -> u64 {
    row.get::<_, i64>(column) as u64
}

fn ledger(row: &Row, account: &Account) -> Ledger {
    Ledger {
        id: row.get::<_, String>("id"),
        owner_urn: format!("urn:sc:global:player:geid:{}", account.geid),
        account_id: account.account_id as i32,
        name: row.get("name"),
        funds: Some(Funds { amount: row.get::<_, i64>("amount") as u64, currency: row.get("currency") }),
        version: row.get::<_, i64>("version") as u32,
        created_at: millis(row, "created_ms"),
        updated_at: millis(row, "updated_ms"),
        ..Default::default()
    }
}

#[tonic::async_trait]
impl LedgerService for Ledgers {
    async fn get_funds(&self, request: Request<GetFundsRequest>) -> Result<Response<GetFundsResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let name = request.into_inner().ledger_name;
        let geid = account.geid as i64;
        for (ledger, opening, currency) in OPENED {
            self.db
                .execute(
                    "INSERT INTO ledgers (owner_geid, account_id, name, amount, currency)
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                    &[&geid, &i64::from(account.account_id), &ledger, &opening, &(currency as i32)],
                )
                .await
                .map_err(internal)?;
        }
        let rows = self
            .db
            .query(
                "SELECT id::text AS id, name, amount, currency, version,
                        (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
                        (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms
                 FROM ledgers WHERE owner_geid = $1 AND ($2 = '' OR name = $2)
                 ORDER BY currency <> $3, name",
                &[&geid, &name, &(Currency::Auec as i32)],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(GetFundsResponse { ledgers: rows.iter().map(|row| ledger(row, &account)).collect() }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Ledgers {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    });
    let server = LedgerServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = LedgerInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "ledger listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_ledger_is_opened_in_its_own_currency() {
        assert_eq!(OPENED[0].0, "AUEC_LEDGER");
        assert_eq!(OPENED[0].2 as i32, 3);
        assert_eq!(OPENED[1].0, "REC_LEDGER");
        assert_eq!(OPENED[1].2 as i32, 2);
    }

    /// Against a real YugabyteDB, in a private schema:
    /// `SC_TEST_POSTGRES=<url> cargo test -p sc-ledger -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn an_unfiltered_read_puts_auec_first() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let data = std::env::temp_dir().join(format!("sc-ledger-{}", std::process::id()));
        let state = State::open(&data).unwrap();
        let account = state.ensure_account("ledgertest").unwrap();
        let token = state.issue_token(&account);
        let service = Ledgers { state, db: Postgres::connect(&url, SCHEMA).await.unwrap() };
        let ask = |name: &str| {
            let mut request = Request::new(GetFundsRequest { ledger_name: name.to_string() });
            request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            request
        };
        let all = service.get_funds(ask("")).await.unwrap().into_inner().ledgers;
        let currencies: Vec<i32> = all.iter().map(|ledger| ledger.funds.unwrap().currency).collect();
        assert_eq!(currencies, [Currency::Auec as i32, Currency::Rec as i32]);
        let rec = service.get_funds(ask(REC_LEDGER)).await.unwrap().into_inner().ledgers;
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].funds.unwrap().currency, Currency::Rec as i32);

        let insert = "INSERT INTO ledgers (owner_geid, account_id, name, amount, currency) VALUES (-1, -1, 'default', 5, 3)
                      ON CONFLICT DO NOTHING";
        service.db.execute(insert, &[]).await.unwrap();
        Postgres::connect(&url, SCHEMA).await.unwrap();
        let renamed = service.db.query("SELECT name, amount FROM ledgers WHERE owner_geid = -1", &[]).await.unwrap();
        assert_eq!(renamed.len(), 1);
        assert_eq!((renamed[0].get::<_, &str>(0), renamed[0].get::<_, i64>(1)), ("AUEC_LEDGER", 5));
    }
}

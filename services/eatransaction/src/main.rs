//! sc.external.services.eatransaction.v1.ExternalEATransactionService: Arena Commander rentals.
//!
//! An orchestrator: the money, the entitlement, the rental and the price each belong to another service
//! (sc-ledger, sc-entitlement, sc-rental, sc-solar-system-shop), which this one calls. Its own tables are
//! only the purchase journal, which makes a purchase end charged and granted or neither (purchase.rs),
//! and lets a retried command return the first result instead of charging again.
//!
//! The game prices these in REC, so they are paid from the player's REC ledger.

mod journal;
mod owners;
mod purchase;

use std::{net::SocketAddr, process::ExitCode, sync::Arc, time::Duration};

use clap::Parser;
use prost::Message;
use sc_db::{DatabaseArgs, Postgres};
use sc_economy::command::{command_id, Push};
use sc_proto::services::eatransaction::v1::{
    external_ea_transaction_service_server::{ExternalEaTransactionService, ExternalEaTransactionServiceServer},
    EaRentRequest, EaRentResponse, RenewEaRentalRequest, RenewEaRentalResponse, TransactionLogContext,
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::{info, warn};

use journal::PgJournal;
use owners::{Addresses, GrpcOwners};
use purchase::{Order, Purchase, Purchases};

const RENT_RESPONSE: &str = "sc.external.services.eatransaction.v1.EARentResponse";
const RENEW_RESPONSE: &str = "sc.external.services.eatransaction.v1.RenewEARentalResponse";
/// How often purchases left halfway are picked up again.
const RESUME_EVERY: Duration = Duration::from_secs(30);

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of the push service.
    #[arg(long, env = "SCBACKEND_PUSH", default_value = "http://sc-push:50051")]
    push: String,
    /// Address of sc-ledger, which charges.
    #[arg(long, env = "SCBACKEND_LEDGER", default_value = "http://sc-ledger:50051")]
    ledger: String,
    /// Address of sc-entitlement, which grants.
    #[arg(long, env = "SCBACKEND_ENTITLEMENT", default_value = "http://sc-entitlement:50051")]
    entitlement: String,
    /// Address of sc-rental, which keeps the rentals.
    #[arg(long, env = "SCBACKEND_RENTAL", default_value = "http://sc-rental:50051")]
    rental: String,
    /// Address of sc-solar-system-shop, which prices.
    #[arg(long, env = "SCBACKEND_SHOP", default_value = "http://sc-solar-system-shop:50051")]
    shop: String,
    /// Refuse rentals the shop has no offering for, instead of taking the client's price. Rental prices
    /// are not in the game files, so this needs a price list seeded into the shop.
    #[arg(long, env = "SCBACKEND_LISTED_PRICES_ONLY")]
    listed_prices_only: bool,
}

struct Transactions {
    state: State,
    purchases: Arc<Purchases<GrpcOwners, PgJournal>>,
    push: Push,
}

fn log_parts(log_context: Option<&TransactionLogContext>) -> (Vec<u8>, String) {
    let encoded = log_context.map(Message::encode_to_vec).unwrap_or_default();
    (encoded, log_context.map(|context| context.message.clone()).unwrap_or_default())
}

impl Transactions {
    fn player(&self, metadata: &tonic::metadata::MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }
}

#[tonic::async_trait]
impl ExternalEaTransactionService for Transactions {
    async fn ea_rent(&self, request: Request<EaRentRequest>) -> Result<Response<EaRentResponse>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let command = command_id(request.arguments.as_ref())?;
        let body = request.body.as_ref().ok_or_else(|| Status::invalid_argument("missing body"))?;
        if body.entity_class_guid.is_empty() {
            return Err(Status::invalid_argument("missing entity class"));
        }
        let (log_context, log_message) = log_parts(body.log_context.as_ref());
        let order = Order {
            purchase: Purchase::Rent { entity_class_guid: &body.entity_class_guid },
            price: body.price,
            days: body.duration,
            log_context,
            log_message,
        };
        let receipt = self.purchases.purchase(account.geid, account.account_id, command, &order).await?;
        info!(handle = %account.handle, rental = %receipt.rental_id, class = %body.entity_class_guid, "rented");
        let response = EaRentResponse { rental_id: receipt.rental_id };
        let arguments = request.arguments.clone().unwrap_or_default();
        self.push.complete(account.account_id, &arguments, RENT_RESPONSE, &response).await;
        Ok(Response::new(response))
    }

    async fn renew_ea_rental(
        &self,
        request: Request<RenewEaRentalRequest>,
    ) -> Result<Response<RenewEaRentalResponse>, Status> {
        let account = self.player(request.metadata())?;
        let request = request.into_inner();
        let command = command_id(request.arguments.as_ref())?;
        let body = request.body.as_ref().ok_or_else(|| Status::invalid_argument("missing body"))?;
        if body.rental_id.is_empty() {
            return Err(Status::invalid_argument("missing rental"));
        }
        let (log_context, log_message) = log_parts(body.log_context.as_ref());
        let order = Order {
            purchase: Purchase::Renew { rental_id: &body.rental_id },
            price: body.price,
            days: body.duration,
            log_context,
            log_message,
        };
        let receipt = self.purchases.purchase(account.geid, account.account_id, command, &order).await?;
        info!(handle = %account.handle, rental = %receipt.rental_id, "rental renewed");
        let response = RenewEaRentalResponse { rental_id: receipt.rental_id };
        let arguments = request.arguments.clone().unwrap_or_default();
        self.push.complete(account.account_id, &arguments, RENEW_RESPONSE, &response).await;
        Ok(Response::new(response))
    }
}

/// Finishes purchases that a crash or an unreachable owner left halfway.
async fn resume_unfinished(purchases: Arc<Purchases<GrpcOwners, PgJournal>>) {
    let mut ticks = tokio::time::interval(RESUME_EVERY);
    loop {
        ticks.tick().await;
        match purchases.resume().await {
            Ok(0) => {}
            Ok(finished) => info!(finished, "unfinished purchases resumed"),
            Err(error) => warn!(%error, "resuming purchases failed"),
        }
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let addresses = Addresses {
        ledger: args.ledger.clone(),
        entitlement: args.entitlement.clone(),
        rental: args.rental.clone(),
        shop: args.shop.clone(),
        push: args.push.clone(),
    };
    let purchases = Arc::new(Purchases {
        owners: GrpcOwners::connect(&addresses)?,
        journal: PgJournal { db: Postgres::connect(&args.database.postgres, journal::SCHEMA).await? },
        listed_prices_only: args.listed_prices_only,
    });
    tokio::spawn(resume_unfinished(purchases.clone()));
    let service = Transactions { state: State::load(&args.common.data)?, purchases, push: Push::connect(&args.push)? };
    let server = ExternalEaTransactionServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "eatransaction listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

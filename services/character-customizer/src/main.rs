//! sc.external.services.character_customizer.v1.CharacterCustomizerService. The frontend's character creator saves here: the
//! gender, the DNA, the material parameters and the head's items. A row per character GEID, which the DGSs ask
//! for over `CharacterCustomizerInternal`.

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_api::internal::{ character_customizer_internal_server::{CharacterCustomizerInternal, CharacterCustomizerInternalServer}, GetCustomizationRequest, GetCustomizationResponse, };
use sc_proto::{
    common::types::v1::CharacterCustomization,
    services::character_customizer::v1::{
    character_customizer_service_server::{CharacterCustomizerService, CharacterCustomizerServiceServer},
    SaveCharacterCustomizationsRequest, SaveCharacterCustomizationsResponse,
    },
};
use sc_runtime::{caller, state::State, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::{info, warn};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS character_customizations (
    geid BIGINT PRIMARY KEY,
    customization BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
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

struct Customizer {
    state: State,
    db: Postgres,
}

#[tonic::async_trait]
impl CharacterCustomizerService for Customizer {
    async fn save_character_customizations(
        &self,
        request: Request<SaveCharacterCustomizationsRequest>,
    ) -> Result<Response<SaveCharacterCustomizationsResponse>, Status> {
        let account = caller(&self.state, request.metadata())
            .ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        let saved = request.into_inner().character_customizations.unwrap_or_default();
        let (gender, dna, materials) = (saved.gender, saved.dna_matrix.len(), saved.custom_material_params.len());
        let items = saved.customizations.as_ref().map_or(0, |loadout| loadout.params.len());
        // Stored as it came: a DGS asks for it at each join and puts it on the player's binds (`sc-dgs` `appearance`).
        let customization = saved.encode_to_vec();
        let geid = account.geid as i64;
        self.db
            .execute(
                "INSERT INTO character_customizations (geid, customization) VALUES ($1, $2)
                 ON CONFLICT (geid) DO UPDATE SET customization = excluded.customization, updated_at = now()",
                &[&geid, &customization],
            )
            .await
            .map_err(internal)?;
        info!(handle = %account.handle, geid = account.geid, gender, dna, materials, items, "customization saved");
        Ok(Response::new(SaveCharacterCustomizationsResponse::default()))
    }
}

/// What other backend services ask: the DGSs, for a joining player's appearance. Only this service reads the table.
#[tonic::async_trait]
impl CharacterCustomizerInternal for Customizer {
    async fn get_customization(
        &self,
        request: Request<GetCustomizationRequest>,
    ) -> Result<Response<GetCustomizationResponse>, Status> {
        let geid = request.into_inner().geid as i64;
        let row = self
            .db
            .query_opt("SELECT customization FROM character_customizations WHERE geid = $1", &[&geid])
            .await
            .map_err(internal)?;
        let Some(row) = row else { return Ok(Response::new(GetCustomizationResponse::default())) };
        let saved: Vec<u8> = row.try_get(0).map_err(internal)?;
        let customization = CharacterCustomization::decode(saved.as_slice())
            .inspect_err(|error| warn!(%error, geid, "stored customization does not decode"))
            .map_err(|_| Status::data_loss("stored customization does not decode"))?;
        Ok(Response::new(GetCustomizationResponse { customization: Some(customization) }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Customizer {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
    });
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = CharacterCustomizerInternalServer::from_arc(service.clone()).max_decoding_message_size(MAX_MESSAGE_BYTES);
    let server = CharacterCustomizerServiceServer::from_arc(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "character customizer listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

//! sc.external.services.entity_document.v1.ExternalEntityDocumentService.
//!
//! One document per entity and class. Bodies are stored as the protobuf bytes the client sent, which
//! keeps them exact, including fields not decoded here. Protobuf merges by concatenation, so an Update
//! appends the partial document to the stored bytes.
//!
//! `EntityDocumentInternal` (internal.rs), on the same port, lets the admin site read and replace documents, such as a
//! player's `player_home_location`.

mod internal;
mod ships;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::{
    encoding::{decode_key, WireType},
    Message,
};
use prost_reflect::DescriptorPool;
use sc_db::{internal, DatabaseArgs, Postgres, Row};
use sc_api::internal::{entity_document_internal_server::EntityDocumentInternalServer, ship_records_internal_server::ShipRecordsInternalServer};
use sc_proto::services::entity_document::v1::{
    external_entity_document_service_server::{ExternalEntityDocumentService, ExternalEntityDocumentServiceServer},
    CommitRequest, CommitResponse, CreateRequest, CreateResponse, DocumentHeader, EntityDocument, EntityDocumentBody,
    EraseRequest, EraseResponse, FetchRequest, FetchResponse, PrepareRequest, PrepareResponse, UpdateRequest,
    UpdateResponse, UpsertRequest, UpsertResponse,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const BODY_MESSAGE: &str = "sc.external.services.entity_document.v1.EntityDocumentBody";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entity_documents (
    entity_id BIGINT NOT NULL,
    class TEXT NOT NULL,
    generation BIGINT NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    owner_id BIGINT NOT NULL DEFAULT 0,
    body BYTEA,
    prepared BYTEA,
    PRIMARY KEY (entity_id, class)
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

struct Documents {
    db: Postgres,
    /// Document class names by field number of the `class` oneof.
    classes: HashMap<u32, String>,
}

impl Documents {
    /// A body holds one oneof member, so its first field number names the class.
    fn class_of(&self, body: &[u8]) -> Result<&str, Status> {
        let mut buffer = body;
        let (number, _): (u32, WireType) =
            decode_key(&mut buffer).map_err(|_| Status::invalid_argument("empty document body"))?;
        self.classes
            .get(&number)
            .map(String::as_str)
            .ok_or_else(|| Status::invalid_argument(format!("unknown document class {number}")))
    }

    fn encode(body: Option<EntityDocumentBody>) -> Result<Vec<u8>, Status> {
        body.map(|body| body.encode_to_vec()).ok_or_else(|| Status::invalid_argument("missing document body"))
    }
}

fn document(row: &Row) -> Result<EntityDocument, Status> {
    let entity_id: i64 = row.get("entity_id");
    let generation: i64 = row.get("generation");
    let revision: i64 = row.get("revision");
    let body: Vec<u8> = row.get("body");
    Ok(EntityDocument {
        header: Some(DocumentHeader {
            entity_id: entity_id as u64,
            generation: generation as u32,
            revision: revision.to_string(),
        }),
        body: Some(EntityDocumentBody::decode(body.as_slice()).map_err(internal)?),
    })
}

#[tonic::async_trait]
impl ExternalEntityDocumentService for Documents {
    async fn create(&self, request: Request<CreateRequest>) -> Result<Response<CreateResponse>, Status> {
        let request = request.into_inner();
        let owner = request.owner_id as i64;
        for create in request.creates {
            let command = create.command.unwrap_or_default();
            let body = Self::encode(create.body)?;
            let class = self.class_of(&body)?;
            let added = self
                .db
                .execute(
                    "INSERT INTO entity_documents (entity_id, class, generation, owner_id, body)
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                    &[&(command.entity_id as i64), &class, &i64::from(command.generation), &owner, &body],
                )
                .await
                .map_err(internal)?;
            if added == 0 {
                return Err(Status::already_exists(format!("{class} of {}", command.entity_id)));
            }
        }
        Ok(Response::new(CreateResponse::default()))
    }

    async fn upsert(&self, request: Request<UpsertRequest>) -> Result<Response<UpsertResponse>, Status> {
        let request = request.into_inner();
        let owner = request.owner_id as i64;
        for upsert in request.upserts {
            let command = upsert.command.unwrap_or_default();
            let body = Self::encode(upsert.body)?;
            let class = self.class_of(&body)?;
            self.db
                .execute(
                    "INSERT INTO entity_documents (entity_id, class, generation, owner_id, body)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (entity_id, class) DO UPDATE SET generation = excluded.generation,
                         body = excluded.body, revision = entity_documents.revision + 1",
                    &[&(command.entity_id as i64), &class, &i64::from(command.generation), &owner, &body],
                )
                .await
                .map_err(internal)?;
        }
        Ok(Response::new(UpsertResponse::default()))
    }

    async fn update(&self, request: Request<UpdateRequest>) -> Result<Response<UpdateResponse>, Status> {
        for update in request.into_inner().updates {
            let command = update.command.unwrap_or_default();
            let partial = Self::encode(update.partial)?;
            let class = self.class_of(&partial)?;
            let changed = self
                .db
                .execute(
                    "UPDATE entity_documents SET body = body || $3, generation = $4, revision = revision + 1
                     WHERE entity_id = $1 AND class = $2",
                    &[&(command.entity_id as i64), &class, &partial, &i64::from(command.generation)],
                )
                .await
                .map_err(internal)?;
            if changed == 0 {
                return Err(Status::not_found(format!("{class} of {}", command.entity_id)));
            }
        }
        Ok(Response::new(UpdateResponse::default()))
    }

    /// Stages a document without changing what Fetch returns, until Commit.
    async fn prepare(&self, request: Request<PrepareRequest>) -> Result<Response<PrepareResponse>, Status> {
        let request = request.into_inner();
        let owner = request.owner_id as i64;
        for prepare in request.prepares {
            let command = prepare.command.unwrap_or_default();
            let body = Self::encode(prepare.body)?;
            let class = self.class_of(&body)?;
            self.db
                .execute(
                    "INSERT INTO entity_documents (entity_id, class, generation, owner_id, prepared)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (entity_id, class) DO UPDATE SET prepared = excluded.prepared",
                    &[&(command.entity_id as i64), &class, &i64::from(command.generation), &owner, &body],
                )
                .await
                .map_err(internal)?;
        }
        Ok(Response::new(PrepareResponse::default()))
    }

    async fn commit(&self, request: Request<CommitRequest>) -> Result<Response<CommitResponse>, Status> {
        for commit in request.into_inner().commits {
            let command = commit.command.unwrap_or_default();
            let changed = self
                .db
                .execute(
                    "UPDATE entity_documents SET body = prepared, prepared = NULL, generation = $3,
                         revision = revision + 1
                     WHERE entity_id = $1 AND class = $2 AND prepared IS NOT NULL",
                    &[&(command.entity_id as i64), &command.class, &i64::from(command.generation)],
                )
                .await
                .map_err(internal)?;
            if changed == 0 {
                return Err(Status::failed_precondition(format!("nothing prepared for {}", command.class)));
            }
        }
        Ok(Response::new(CommitResponse::default()))
    }

    async fn erase(&self, request: Request<EraseRequest>) -> Result<Response<EraseResponse>, Status> {
        for erasure in request.into_inner().erasures {
            let command = erasure.command.unwrap_or_default();
            self.db
                .execute(
                    "DELETE FROM entity_documents WHERE entity_id = $1 AND class = $2",
                    &[&(command.entity_id as i64), &command.class],
                )
                .await
                .map_err(internal)?;
        }
        Ok(Response::new(EraseResponse::default()))
    }

    async fn fetch(&self, request: Request<FetchRequest>) -> Result<Response<FetchResponse>, Status> {
        let mut documents = Vec::new();
        for body in request.into_inner().queries {
            let query = body.query.unwrap_or_default();
            let rows = self
                .db
                .query(
                    "SELECT entity_id, generation, revision, body FROM entity_documents
                     WHERE entity_id = $1 AND ($2 = '' OR class = $2) AND body IS NOT NULL",
                    &[&(query.entity_id as i64), &query.class],
                )
                .await
                .map_err(internal)?;
            for row in &rows {
                documents.push(document(row)?);
            }
        }
        Ok(Response::new(FetchResponse { documents }))
    }
}

fn class_names() -> Result<HashMap<u32, String>, String> {
    let pool = DescriptorPool::decode(sc_proto::DESCRIPTORS).map_err(|e| format!("descriptors: {e}"))?;
    let body = pool.get_message_by_name(BODY_MESSAGE).ok_or("no document body descriptor")?;
    Ok(body.fields().map(|field| (field.number(), field.name().to_string())).collect())
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Documents {
        db: Postgres::connect(&args.database.postgres, &format!("{SCHEMA}\n{}", ships::SCHEMA)).await?,
        classes: class_names()?,
    });
    let server = ExternalEntityDocumentServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let ships = ShipRecordsInternalServer::from_arc(service.clone()).max_decoding_message_size(MAX_MESSAGE_BYTES);
    let internal = EntityDocumentInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "entity document listening");
    Server::builder().add_service(server).add_service(internal).add_service(ships).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_come_from_the_descriptors() {
        let classes = class_names().unwrap();
        assert_eq!(classes[&5], "player_home_location");
    }

    #[test]
    fn concatenated_bodies_merge() {
        let mut stored = EntityDocumentBody::default().encode_to_vec();
        stored.extend_from_slice(&EntityDocumentBody::default().encode_to_vec());
        assert!(EntityDocumentBody::decode(stored.as_slice()).is_ok());
    }
}

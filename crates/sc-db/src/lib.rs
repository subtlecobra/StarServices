//! Access to the two databases: YugabyteDB for relational data, ArangoDB for the entity graph. See
//! docs/DATA.md for what goes where.

mod arango;
mod postgres;

pub use arango::Arango;
pub use postgres::{Postgres, Row};
pub use tokio_postgres::types::ToSql;

use clap::Args;

#[derive(Args, Clone, Debug)]
pub struct DatabaseArgs {
    /// YugabyteDB connection string.
    #[arg(long, env = "SCBACKEND_POSTGRES", default_value = "postgres://yugabyte@yb-tservers.scbackend-data:5433/yugabyte")]
    pub postgres: String,
    /// ArangoDB address.
    #[arg(long, env = "SCBACKEND_ARANGO", default_value = "http://arangodb.scbackend-data:8529")]
    pub arango: String,
}

/// Turns any database error into the Status a gRPC handler returns. The detail goes to the log.
pub fn internal<E: std::fmt::Display>(error: E) -> tonic::Status {
    tracing::warn!(%error, "database error");
    tonic::Status::internal("database error")
}

//! `scbackend.internal.ContractBrokerInternal`: the mission contracts in the game data, which
//! `sc-admin seed` reads from the archive and hands over, in several calls when there are many. Rows are
//! replaced by key.

use prost::Message;
use sc_db::{internal, Postgres};
use sc_proto::{
    common::mission::v1::MissionContract,
    internal::{contract_broker_internal_server::ContractBrokerInternal, SeedMissionContractsRequest, SeedResponse},
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Broker;

const SEED_ROWS: usize = 500;

pub async fn seed(db: &Postgres, request: &SeedMissionContractsRequest) -> Result<u64, Status> {
    for contract in &request.contracts {
        if contract.id.is_empty() {
            return Err(Status::invalid_argument("a contract with no id"));
        }
        MissionContract::decode(contract.body.as_slice())
            .map_err(|e| Status::invalid_argument(format!("contract {}: {e}", contract.id)))?;
    }
    let mut written = 0;
    for rows in request.contracts.chunks(SEED_ROWS) {
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        let bodies: Vec<&[u8]> = rows.iter().map(|row| row.body.as_slice()).collect();
        written += db
            .execute(
                "INSERT INTO mission_contracts (id, definition_id, body)
                 SELECT id, id, body FROM unnest($1::text[], $2::bytea[]) AS t(id, body)
                 ON CONFLICT (id) DO UPDATE SET definition_id = excluded.definition_id, body = excluded.body",
                &[&ids, &bodies],
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl ContractBrokerInternal for Broker {
    async fn seed_mission_contracts(
        &self,
        request: Request<SeedMissionContractsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = seed(&self.db, &request.into_inner()).await?;
        info!(rows, "mission contracts seeded");
        Ok(Response::new(SeedResponse { rows }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_proto::internal::MissionContractSeed;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-contract-broker -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn seeding_replaces_by_key_and_refuses_what_is_no_contract() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let id = format!("test-contract-{}", std::process::id());
        let body = MissionContract::default().encode_to_vec();
        let request = SeedMissionContractsRequest { contracts: vec![MissionContractSeed { id: id.clone(), body }] };
        assert_eq!(seed(&db, &request).await.unwrap(), 1);
        assert_eq!(seed(&db, &request).await.unwrap(), 1);
        let garbage = SeedMissionContractsRequest {
            contracts: vec![MissionContractSeed { id: id.clone(), body: vec![0xff, 0xff, 0xff] }],
        };
        assert_eq!(seed(&db, &garbage).await.unwrap_err().code(), tonic::Code::InvalidArgument);
        db.execute("DELETE FROM mission_contracts WHERE id = $1", &[&id]).await.unwrap();
    }
}

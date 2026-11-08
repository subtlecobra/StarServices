//! sc.external.services.contract_broker.v1.ContractBrokerService.
//!
//! Contracts are stored whole as MissionContract protobuf, with the columns that narrow a read beside it,
//! and are written outside this API: the game data's come through `ContractBrokerInternal` (seed.rs), from
//! `sc-admin seed`. Tokens and reputation stay with the services that own them, so
//! prerequisites are checked by asking those services with the caller's own token.

mod seed;

use std::{collections::HashMap, net::SocketAddr, pin::Pin, process::ExitCode, sync::Arc};

use clap::Parser;
use futures_util::{stream, Stream};
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres};
use sc_api::internal::contract_broker_internal_server::ContractBrokerInternalServer;
use sc_proto::{
    common::{
    api::v1::{PaginationArguments, Query},
    mission::v1::{ContractPrerequisites, FilterOptions, MissionContract, ReputationRequirement, TokenRequirement},
    },
    services::{
    contract_broker::v1::{
    contract_broker_service_server::{ContractBrokerService, ContractBrokerServiceServer},
    query_contracts_response::{self, Body, ContractsWithEligibility, EligibilityInfo},
    QueryAvailableContractsRequest, QueryAvailableContractsResponse, QueryContractsRequest,
    QueryContractsResponse,
    },
    mission_token::v1::{mission_token_service_client::MissionTokenServiceClient, QueryTokensRequest},
    reputation::v1::{reputation_service_client::ReputationServiceClient, QueryReputationsRequest},
    },
};
use sc_runtime::query;
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{
    codec::CompressionEncoding,
    metadata::{AsciiMetadataValue, MetadataMap},
    transport::{Channel, Endpoint, Server},
    Request, Response, Status,
};
use tracing::info;

const STREAM_CHUNK: usize = 256;
const REPUTATION_PAGE: u32 = 500;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mission_contracts (
    id TEXT PRIMARY KEY,
    definition_id TEXT NOT NULL,
    shard_id TEXT NOT NULL DEFAULT '',
    expires_at TIMESTAMPTZ,
    body BYTEA NOT NULL
);
CREATE INDEX IF NOT EXISTS mission_contracts_definition ON mission_contracts (definition_id);
CREATE TABLE IF NOT EXISTS player_mission_salts (
    player_id BIGINT PRIMARY KEY,
    salt BIGINT NOT NULL
);";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
    #[command(flatten)]
    database: DatabaseArgs,
    #[arg(long, env = "SCBACKEND_LISTEN", default_value = "0.0.0.0:50051")]
    listen: SocketAddr,
    /// Address of the mission token service.
    #[arg(long, env = "SCBACKEND_MISSION_TOKEN", default_value = "http://sc-mission-token:50051")]
    mission_token: String,
    /// Address of the reputation service.
    #[arg(long, env = "SCBACKEND_REPUTATION", default_value = "http://sc-reputation:50051")]
    reputation: String,
}

struct Broker {
    state: State,
    db: Postgres,
    tokens: MissionTokenServiceClient<Channel>,
    reputation: ReputationServiceClient<Channel>,
}

/// What a player has, for checking prerequisites.
#[derive(Default)]
struct Holdings {
    tokens: HashMap<String, i32>,
    /// (entity, scope, standing id) of every reputation.
    standings: Vec<(String, String, String)>,
}

impl Holdings {
    fn has_token(&self, requirement: &TokenRequirement) -> bool {
        self.tokens.get(&requirement.id).is_some_and(|count| *count >= requirement.count.max(1))
    }

    fn has_standing(&self, requirement: &ReputationRequirement) -> bool {
        self.standings.iter().any(|(entity, scope, standing)| {
            *entity == requirement.organisation_urn
                && *scope == requirement.scope_name
                && requirement.standing_ids.contains(standing)
        })
    }
}

fn eligible(
    prerequisites: &ContractPrerequisites,
    shard: &str,
    locations: &[u32],
    holdings: &Holdings,
    options: &FilterOptions,
) -> bool {
    let shard_ok = prerequisites.shard_id.is_empty() || prerequisites.shard_id == shard;
    let location_ok = options.disable_location_prerequisites
        || prerequisites.location_id == 0
        || locations.contains(&prerequisites.location_id);
    let tokens_ok = options.disable_token_prerequisites
        || (prerequisites.required_token.iter().all(|token| holdings.has_token(token))
            && !prerequisites.excluded_token.iter().any(|token| holdings.has_token(token)));
    let reputation_ok = options.disable_reputation_prerequisites
        || (prerequisites.required_reputation.iter().all(|standing| holdings.has_standing(standing))
            && !prerequisites.excluded_reputation.iter().any(|standing| holdings.has_standing(standing)));
    shard_ok && location_ok && tokens_ok && reputation_ok
}

fn needs_tokens(prerequisites: &ContractPrerequisites) -> bool {
    !prerequisites.required_token.is_empty() || !prerequisites.excluded_token.is_empty()
}

fn needs_reputation(prerequisites: &ContractPrerequisites) -> bool {
    !prerequisites.required_reputation.is_empty() || !prerequisites.excluded_reputation.is_empty()
}

fn prerequisites(contract: &MissionContract) -> ContractPrerequisites {
    contract.prerequisites.clone().unwrap_or_default()
}

fn definition(contract: &MissionContract) -> String {
    contract.mission_template.as_ref().map(|template| template.contract_definition_id.clone()).unwrap_or_default()
}

fn property(contract: &MissionContract, name: &str) -> Option<String> {
    Some(match name {
        "id" => contract.id.clone(),
        "contract_definition_id" => definition(contract),
        "contact_type" => contract.contact_type.clone(),
        "shared_mission_id" => contract.shared_mission_id.clone(),
        "suspendable" => contract.suspendable.to_string(),
        "count" => contract.count.to_string(),
        "shard_id" => prerequisites(contract).shard_id,
        "location_id" => prerequisites(contract).location_id.to_string(),
        _ => return None,
    })
}

fn with_token<T>(message: T, authorization: Option<&AsciiMetadataValue>) -> Request<T> {
    let mut request = Request::new(message);
    if let Some(value) = authorization {
        request.metadata_mut().insert("authorization", value.clone());
    }
    request
}

impl Broker {
    fn account(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))
    }

    /// `player_id` must be the caller: holdings are read with the caller's token. 0 means the caller.
    fn check_player(account: &Account, player_id: u64) -> Result<(), Status> {
        if player_id != 0 && player_id != account.geid {
            return Err(Status::permission_denied("not your player"));
        }
        Ok(())
    }

    async fn salt(&self, account: &Account) -> Result<u64, Status> {
        let player = account.geid as i64;
        self.db
            .execute(
                "INSERT INTO player_mission_salts (player_id, salt)
                 VALUES ($1, (random() * 9000000000000000000)::bigint) ON CONFLICT DO NOTHING",
                &[&player],
            )
            .await
            .map_err(internal)?;
        let row = self
            .db
            .query_opt("SELECT salt FROM player_mission_salts WHERE player_id = $1", &[&player])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("salt row disappeared"))?;
        Ok(row.get::<_, i64>("salt") as u64)
    }

    async fn contracts(&self, definitions: &[String], shard: Option<&str>) -> Result<Vec<MissionContract>, Status> {
        let rows = self
            .db
            .query(
                "SELECT body FROM mission_contracts
                 WHERE (expires_at IS NULL OR expires_at > now())
                   AND (cardinality($1::text[]) = 0 OR definition_id = ANY($1))
                   AND ($2::text IS NULL OR shard_id = '' OR shard_id = $2)
                 ORDER BY id",
                &[&definitions, &shard],
            )
            .await
            .map_err(internal)?;
        rows.iter()
            .map(|row| MissionContract::decode(row.get::<_, &[u8]>("body")).map_err(internal))
            .collect()
    }

    /// Reads only what the contracts' prerequisites ask about.
    async fn holdings(
        &self,
        contracts: &[MissionContract],
        options: &FilterOptions,
        authorization: Option<&AsciiMetadataValue>,
    ) -> Result<Holdings, Status> {
        let mut holdings = Holdings::default();
        let all: Vec<ContractPrerequisites> = contracts.iter().map(prerequisites).collect();
        if !options.disable_token_prerequisites && all.iter().any(needs_tokens) {
            let reply = self.tokens.clone().query_tokens(with_token(QueryTokensRequest {}, authorization)).await?;
            for token in reply.into_inner().tokens {
                *holdings.tokens.entry(token.id).or_default() += token.count;
            }
        }
        if !options.disable_reputation_prerequisites && all.iter().any(needs_reputation) {
            let mut after = String::new();
            loop {
                let pagination = PaginationArguments { first: REPUTATION_PAGE, after };
                let query = Query { pagination: Some(pagination), ..Default::default() };
                let request = with_token(QueryReputationsRequest { query: Some(query) }, authorization);
                let reply = self.reputation.clone().query_reputations(request).await?.into_inner();
                for versioned in reply.results {
                    if let (Some(reputation), Some(standing)) = (versioned.reputation, versioned.standing) {
                        holdings.standings.push((reputation.entity, reputation.scope, standing.id));
                    }
                }
                let page = reply.page_info.unwrap_or_default();
                if !page.has_next_page {
                    break;
                }
                after = page.end_cursor;
            }
        }
        Ok(holdings)
    }
}

#[tonic::async_trait]
impl ContractBrokerService for Broker {
    type QueryAvailableContractsStream =
        Pin<Box<dyn Stream<Item = Result<QueryAvailableContractsResponse, Status>> + Send>>;

    /// Sends what is available now in chunks and ends. There is always one message, because it carries
    /// the salt.
    async fn query_available_contracts(
        &self,
        request: Request<QueryAvailableContractsRequest>,
    ) -> Result<Response<Self::QueryAvailableContractsStream>, Status> {
        let account = self.account(request.metadata())?;
        let authorization = request.metadata().get("authorization").cloned();
        let request = request.into_inner();
        Self::check_player(&account, request.player_id)?;
        let options = request.options.unwrap_or_default();
        let mut contracts =
            self.contracts(&options.filter_by_contract_definition_ids, Some(&request.shard_id)).await?;
        let holdings = self.holdings(&contracts, &options, authorization.as_ref()).await?;
        contracts.retain(|contract| {
            eligible(&prerequisites(contract), &request.shard_id, &request.location_ids, &holdings, &options)
        });
        let player_mission_salt = self.salt(&account).await?;
        let mut chunks: Vec<Vec<MissionContract>> =
            contracts.chunks(STREAM_CHUNK).map(<[MissionContract]>::to_vec).collect();
        if chunks.is_empty() {
            chunks.push(Vec::new());
        }
        let replies = chunks
            .into_iter()
            .map(move |contracts| Ok(QueryAvailableContractsResponse { contracts, player_mission_salt }));
        Ok(Response::new(Box::pin(stream::iter(replies))))
    }

    async fn query_contracts(
        &self,
        request: Request<QueryContractsRequest>,
    ) -> Result<Response<QueryContractsResponse>, Status> {
        let account = self.account(request.metadata())?;
        let authorization = request.metadata().get("authorization").cloned();
        let request = request.into_inner();
        let query = request.query.unwrap_or_default();
        let definitions = query::required(query.filter.as_ref(), "contract_definition_id");
        let mut contracts = self.contracts(&definitions, None).await?;
        contracts.retain(|contract| query::matches(query.filter.as_ref(), &|name| property(contract, name)));
        query::sort(&mut contracts, query.sort.as_ref(), property);
        let (contracts, page_info) = query::page(contracts, query.pagination.as_ref())?;
        let body = match request.eligibility {
            None => contracts
                .into_iter()
                .map(|contract| Body { contracts: Some(query_contracts_response::body::Contracts::Contract(contract)) })
                .collect(),
            Some(eligibility) => {
                Self::check_player(&account, eligibility.player_id)?;
                let options = FilterOptions::default();
                let holdings = self.holdings(&contracts, &options, authorization.as_ref()).await?;
                contracts
                    .into_iter()
                    .map(|contract| {
                        let needed = prerequisites(&contract);
                        let info = EligibilityInfo {
                            eligible: eligible(
                                &needed,
                                &eligibility.shard_id,
                                &eligibility.location_ids,
                                &holdings,
                                &options,
                            ),
                            prerequisites: Some(needed),
                        };
                        let pair = ContractsWithEligibility { contract: Some(contract), eligibility_info: Some(info) };
                        Body { contracts: Some(query_contracts_response::body::Contracts::ContractWithEligibility(pair)) }
                    })
                    .collect()
            }
        };
        let player_mission_salt = self.salt(&account).await?;
        Ok(Response::new(QueryContractsResponse { body, page_info: Some(page_info), player_mission_salt }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let tokens = Endpoint::from_shared(args.mission_token.clone()).map_err(|e| format!("--mission-token: {e}"))?;
    let reputation = Endpoint::from_shared(args.reputation.clone()).map_err(|e| format!("--reputation: {e}"))?;
    let service = Arc::new(Broker {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        tokens: MissionTokenServiceClient::new(tokens.connect_lazy()),
        reputation: ReputationServiceClient::new(reputation.connect_lazy()),
    });
    let server = ContractBrokerServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = ContractBrokerInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "contract broker listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(id: &str, count: i32) -> TokenRequirement {
        TokenRequirement { id: id.into(), count }
    }

    fn standing(org: &str, standing: &str) -> ReputationRequirement {
        ReputationRequirement {
            organisation_urn: org.into(),
            scope_name: "scope".into(),
            standing_ids: vec![standing.into()],
        }
    }

    fn holdings() -> Holdings {
        Holdings {
            tokens: HashMap::from([("badge".to_string(), 2)]),
            standings: vec![("org".into(), "scope".into(), "trusted".into())],
        }
    }

    fn passes(needed: &ContractPrerequisites, options: &FilterOptions) -> bool {
        eligible(needed, "shard", &[7], &holdings(), options)
    }

    #[test]
    fn no_prerequisites_pass() {
        assert!(passes(&ContractPrerequisites::default(), &FilterOptions::default()));
    }

    #[test]
    fn shard_and_location_must_match_unless_disabled() {
        let elsewhere = ContractPrerequisites { location_id: 9, ..Default::default() };
        assert!(!passes(&elsewhere, &FilterOptions::default()));
        let options = FilterOptions { disable_location_prerequisites: true, ..Default::default() };
        assert!(passes(&elsewhere, &options));
        let other_shard = ContractPrerequisites { shard_id: "other".into(), ..Default::default() };
        assert!(!passes(&other_shard, &options));
    }

    #[test]
    fn tokens_count_and_exclude() {
        let enough = ContractPrerequisites { required_token: vec![token("badge", 2)], ..Default::default() };
        let short = ContractPrerequisites { required_token: vec![token("badge", 3)], ..Default::default() };
        let barred = ContractPrerequisites { excluded_token: vec![token("badge", 0)], ..Default::default() };
        assert!(passes(&enough, &FilterOptions::default()));
        assert!(!passes(&short, &FilterOptions::default()));
        assert!(!passes(&barred, &FilterOptions::default()));
    }

    #[test]
    fn reputation_requires_and_excludes_standings() {
        let wanted = ContractPrerequisites { required_reputation: vec![standing("org", "trusted")], ..Default::default() };
        let missing = ContractPrerequisites { required_reputation: vec![standing("org", "hero")], ..Default::default() };
        let barred = ContractPrerequisites { excluded_reputation: vec![standing("org", "trusted")], ..Default::default() };
        assert!(passes(&wanted, &FilterOptions::default()));
        assert!(!passes(&missing, &FilterOptions::default()));
        assert!(!passes(&barred, &FilterOptions::default()));
        let options = FilterOptions { disable_reputation_prerequisites: true, ..Default::default() };
        assert!(passes(&missing, &options));
    }
}

//! sc.external.services.entitlement.v1.ExternalEntitlementService. Owns `entitlements`,
//! `entitlement_history` and `starter_entitlements`, which store v2's status and source values. Every other
//! service reaches them through `EntitlementInternal` (internal.rs), sc-entitlement-v2 included.

mod internal;
mod starter;

use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use clap::Parser;
use prost::Message;
use sc_db::{internal, DatabaseArgs, Postgres, Row, ToSql};
use sc_economy::{
    account_urn, command::{command_id, Push}, decoded, player_urn,
    query::{compile, Field, Kind},
};
use sc_proto::{
    common::platform::v1::PortConfig,
    internal::entitlement_internal_server::EntitlementInternalServer,
    services::entitlement::{
        v1::{
            external_entitlement_service_server::{ExternalEntitlementService, ExternalEntitlementServiceServer},
            new_entity_configuration_parameters::Param, new_entity_port_configuration,
            ClaimRequest, ClaimResponse, Entitlement, EntitlementItemType, EntitlementSource, EntitlementStatus,
            EntitlementType, Insurance, InsuranceItemPortLoadout, NewEntityConfiguration,
            NewEntityConfigurationParameters, NewEntityLoadoutKitConfiguration, NewEntityNameConfiguration,
            NewEntityPortConfiguration, NewEntitySerialConfiguration, QueryRequest, QueryResponse,
            UpdateArenaCommanderLoadoutRequest, UpdateArenaCommanderLoadoutResponse,
        },
        v2::{entity_configuration_parameters, EntityConfiguration, EntitlementLogAction},
        v2::EntitlementStatus as StoredStatus,
    },
};
use sc_runtime::{caller, state::{Account, State}, CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, metadata::MetadataMap, transport::Server, Request, Response, Status};
use tracing::info;

use internal::update_loadout;
use starter::Starter;

const CLAIM_RESPONSE: &str = "sc.external.services.entitlement.v1.ClaimResponse";
const LOADOUT_RESPONSE: &str = "sc.external.services.entitlement.v1.UpdateArenaCommanderLoadoutResponse";
/// Until 2026-11 these were created by every economy service from one shared bundle. The definitions are
/// unchanged, so an existing database is left as it is.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entitlements (
    urn TEXT PRIMARY KEY,
    account_id BIGINT NOT NULL,
    player_id BIGINT NOT NULL,
    source_urn TEXT NOT NULL DEFAULT '',
    source_type INT NOT NULL,
    status INT NOT NULL,
    entity_class_guid TEXT NOT NULL DEFAULT '',
    item_type TEXT NOT NULL DEFAULT '',
    item_sub_type TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    stack_size INT NOT NULL DEFAULT 1,
    source_sku TEXT NOT NULL DEFAULT '',
    real_money BOOLEAN NOT NULL DEFAULT FALSE,
    entity_configuration BYTEA,
    entity_geid BIGINT,
    claimed_at TIMESTAMPTZ,
    claim_version TEXT,
    rental_expiry TIMESTAMPTZ,
    arena_commander_loadout BYTEA,
    insurance BYTEA,
    persistent_properties BYTEA,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    modified_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS entitlements_by_player ON entitlements (player_id);
CREATE TABLE IF NOT EXISTS entitlement_history (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    urn TEXT NOT NULL,
    player_id BIGINT NOT NULL,
    log_action INT NOT NULL,
    log_message TEXT NOT NULL DEFAULT '',
    command_id TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS starter_entitlements (
    class_guid TEXT PRIMARY KEY,
    item_type TEXT NOT NULL DEFAULT '',
    item_sub_type TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL,
    port_path TEXT NOT NULL DEFAULT ''
);";

/// What a read selects, with times as milliseconds.
const ENTITLEMENT_COLUMNS: &str = "urn, account_id, player_id, source_urn, source_type, status,
    entity_class_guid, item_type, name, stack_size, source_sku, real_money, entity_configuration,
    entity_geid, claim_version, arena_commander_loadout, insurance, persistent_properties,
    (extract(epoch FROM claimed_at) * 1000)::bigint AS claimed_ms,
    (extract(epoch FROM rental_expiry) * 1000)::bigint AS rental_expiry_ms,
    (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
    (extract(epoch FROM modified_at) * 1000)::bigint AS modified_ms";

/// Packaged is claimable, undelivered is claimed and waiting for its entity, delivered has one.
const V1_STATUS: &str = "CASE status WHEN 1 THEN 4 WHEN 2 THEN 1 WHEN 3 THEN 2 WHEN 4 THEN 3 WHEN 5 THEN 5 ELSE 0 END";
/// Rentals come from Arena Commander, the only place that rents.
const V1_SOURCE: &str = "CASE source_type WHEN 1 THEN 1 WHEN 2 THEN 4 WHEN 3 THEN 2 WHEN 4 THEN 2 WHEN 5 THEN 2 ELSE 0 END";
const V1_TYPE: &str = "CASE source_type WHEN 4 THEN 2 ELSE 1 END";

const FIELDS: &[Field] = &[
    Field { property: "urn", expression: "urn", kind: Kind::Text },
    Field {
        property: "status",
        expression: V1_STATUS,
        kind: Kind::Enum(|name| EntitlementStatus::from_str_name(name).map(|value| value as i32)),
    },
    Field {
        property: "type",
        expression: V1_TYPE,
        kind: Kind::Enum(|name| EntitlementType::from_str_name(name).map(|value| value as i32)),
    },
    Field {
        property: "source",
        expression: V1_SOURCE,
        kind: Kind::Enum(|name| EntitlementSource::from_str_name(name).map(|value| value as i32)),
    },
    Field { property: "name", expression: "name", kind: Kind::Text },
    Field { property: "entity_class_guid", expression: "entity_class_guid", kind: Kind::Text },
    Field { property: "source_sku", expression: "source_sku", kind: Kind::Text },
    Field { property: "item_type", expression: "item_type", kind: Kind::Text },
    Field { property: "stack_size", expression: "stack_size", kind: Kind::Number },
    Field { property: "account_urn", expression: "'urn:sc:platform:account:integer:' || account_id", kind: Kind::Text },
    Field { property: "player_urn", expression: "'urn:sc:global:player:geid:' || player_id", kind: Kind::Text },
    Field {
        property: "created_at",
        expression: "(extract(epoch FROM created_at) * 1000)::bigint",
        kind: Kind::Number,
    },
    Field {
        property: "modified_at",
        expression: "(extract(epoch FROM modified_at) * 1000)::bigint",
        kind: Kind::Number,
    },
];

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
}

struct Entitlements {
    state: State,
    db: Postgres,
    starter: Starter,
    push: Push,
}

impl Entitlements {
    async fn player(&self, metadata: &MetadataMap) -> Result<Account, Status> {
        let account = caller(&self.state, metadata).ok_or_else(|| Status::unauthenticated("missing or invalid token"))?;
        self.starter.ensure(&self.db, account.account_id, account.geid).await.map_err(internal)?;
        Ok(account)
    }
}

fn item_type(name: &str) -> EntitlementItemType {
    match name.to_ascii_lowercase().as_str() {
        "" => EntitlementItemType::Unspecified,
        "ship" | "vehicle" => EntitlementItemType::Ship,
        "hangar" => EntitlementItemType::Hangar,
        "hangar_decoration" | "hangardecoration" => EntitlementItemType::HangarDecoration,
        _ => EntitlementItemType::Other,
    }
}

fn port_config(port: PortConfig) -> new_entity_port_configuration::PortConfig {
    new_entity_port_configuration::PortConfig {
        r#ref: port.r#ref,
        ports: port.ports.into_iter().map(port_config).collect(),
    }
}

/// The two versions lay out these parameters differently, so unlike the loadouts they need converting.
fn entity_config(configuration: EntityConfiguration) -> NewEntityConfiguration {
    use entity_configuration_parameters::Param as Stored;
    let params = configuration.params.into_iter().filter_map(|parameter| {
        let param = match parameter.param? {
            Stored::WebCustomizations(port) => {
                Param::EntityPort(NewEntityPortConfiguration { ports: vec![port_config(port)] })
            }
            Stored::LoadoutKit(kit) => {
                Param::EntityLoadoutKit(NewEntityLoadoutKitConfiguration { code: kit.code, name: kit.name })
            }
            Stored::CustomName(name) => Param::EntityName(NewEntityNameConfiguration { name }),
            Stored::Serial(serial) => Param::EntitySerial(NewEntitySerialConfiguration { serial }),
        };
        Some(NewEntityConfigurationParameters { param: Some(param) })
    });
    NewEntityConfiguration { id: configuration.id, params: params.collect() }
}

fn entitlement(row: &Row) -> Result<Entitlement, Status> {
    let insurance: Option<Insurance> = decoded(row, "insurance")?;
    Ok(Entitlement {
        urn: row.get("urn"),
        created_at: row.get::<_, i64>("created_ms") as u64,
        modified_at: row.get::<_, i64>("modified_ms") as u64,
        status: row.get("v1_status"),
        account_urn: account_urn(row.get::<_, i64>("account_id") as u32),
        player_urn: player_urn(row.get::<_, i64>("player_id") as u64),
        r#type: row.get("v1_type"),
        source: row.get("v1_source"),
        real_money: row.get("real_money"),
        source_sku: row.get("source_sku"),
        name: row.get("name"),
        entity_class_guid: row.get("entity_class_guid"),
        stack_size: row.get::<_, i32>("stack_size") as u32,
        insurance,
        arena_commander_loadout: decoded::<InsuranceItemPortLoadout>(row, "arena_commander_loadout")?,
        entity_config: decoded::<EntityConfiguration>(row, "entity_configuration")?.map(entity_config),
        item_type: item_type(row.get("item_type")) as i32,
    })
}

#[tonic::async_trait]
impl ExternalEntitlementService for Entitlements {
    async fn query(&self, request: Request<QueryRequest>) -> Result<Response<QueryResponse>, Status> {
        let account = self.player(request.metadata()).await?;
        let query = request.into_inner().query;
        let compiled = compile(query.as_ref(), FIELDS, 2, "created_at, urn")?;
        let geid = account.geid as i64;
        let mut parameters: Vec<&(dyn ToSql + Sync)> = vec![&geid];
        parameters.extend(compiled.parameters.iter().map(|value| value as &(dyn ToSql + Sync)));
        let sql = format!(
            "SELECT {ENTITLEMENT_COLUMNS}, {V1_STATUS} AS v1_status, {V1_SOURCE} AS v1_source, {V1_TYPE} AS v1_type
             FROM entitlements WHERE player_id = $1 AND {} {}",
            compiled.condition, compiled.tail
        );
        let mut rows = self.db.query(&sql, &parameters).await.map_err(internal)?;
        let page_info = compiled.page_of(&mut rows);
        let results = rows.iter().map(entitlement).collect::<Result<_, _>>()?;
        Ok(Response::new(QueryResponse { page_info: Some(page_info), results }))
    }

    /// Claiming moves a packaged entitlement to undelivered, which is what makes the game create its
    /// entity. Claiming again changes nothing.
    async fn claim(&self, request: Request<ClaimRequest>) -> Result<Response<ClaimResponse>, Status> {
        let account = self.player(request.metadata()).await?;
        let request = request.into_inner();
        let command = command_id(request.arguments.as_ref())?;
        let urns: Vec<&str> = request.claims.iter().map(|claim| claim.urn.as_str()).collect();
        let geid = account.geid as i64;
        let revoked = StoredStatus::Revoked as i32;
        let failed = StoredStatus::Failed as i32;
        let claimable = self
            .db
            .query_opt(
                "SELECT count(DISTINCT urn) AS found FROM entitlements
                 WHERE player_id = $1 AND urn = ANY($2) AND status <> $3 AND status <> $4",
                &[&geid, &urns, &revoked, &failed],
            )
            .await
            .map_err(internal)?
            .map_or(0, |row| row.get::<_, i64>("found"));
        let mut wanted = urns.clone();
        wanted.sort_unstable();
        wanted.dedup();
        if claimable != wanted.len() as i64 {
            return Err(Status::not_found("entitlement not claimable"));
        }
        self.db
            .execute(
                "WITH claimed AS (
                     UPDATE entitlements SET status = $3, claimed_at = now(), modified_at = now()
                     WHERE player_id = $1 AND urn = ANY($2) AND status = $4 RETURNING urn)
                 INSERT INTO entitlement_history (urn, player_id, log_action, command_id)
                 SELECT urn, $1, $5, $6 FROM claimed",
                &[
                    &geid,
                    &urns,
                    &(StoredStatus::Undelivered as i32),
                    &(StoredStatus::Packaged as i32),
                    &(EntitlementLogAction::Updated as i32),
                    &command,
                ],
            )
            .await
            .map_err(internal)?;
        info!(handle = %account.handle, claims = urns.len(), "entitlements claimed");
        let arguments = request.arguments.unwrap_or_default();
        self.push.complete(account.account_id, &arguments, CLAIM_RESPONSE, &ClaimResponse::default()).await;
        Ok(Response::new(ClaimResponse::default()))
    }

    async fn update_arena_commander_loadout(
        &self,
        request: Request<UpdateArenaCommanderLoadoutRequest>,
    ) -> Result<Response<UpdateArenaCommanderLoadoutResponse>, Status> {
        let account = self.player(request.metadata()).await?;
        let request = request.into_inner();
        let command = command_id(request.arguments.as_ref())?;
        let loadout = request.loadout.unwrap_or_default().encode_to_vec();
        update_loadout(&self.db, account.geid, &request.urn, &loadout, command).await?;
        let response = UpdateArenaCommanderLoadoutResponse::default();
        let arguments = request.arguments.unwrap_or_default();
        self.push.complete(account.account_id, &arguments, LOADOUT_RESPONSE, &response).await;
        Ok(Response::new(response))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Arc::new(Entitlements {
        state: State::load(&args.common.data)?,
        db: Postgres::connect(&args.database.postgres, SCHEMA).await?,
        starter: Starter::default(),
        push: Push::connect(&args.push)?,
    });
    let server = ExternalEntitlementServiceServer::from_arc(service.clone())
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    // On the same port: the gateway routes by service name, and no GRPCRoute lists the internal one.
    let internal = EntitlementInternalServer::from_arc(service).max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "entitlement listening");
    Server::builder().add_service(server).add_service(internal).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_proto::{
        common::platform::v1::LoadoutKitConfiguration,
        services::entitlement::{
            v1::InsuranceItemPortParams,
            v2::{item_port_loadout::ItemPortParams, EntitlementSourceType, EntityConfigurationParameters, ItemPortLoadout},
        },
    };

    #[test]
    fn a_v1_loadout_reads_as_v2_on_the_wire() {
        let item = InsuranceItemPortParams {
            port_name: "hardpoint".into(),
            item_class: "gun".into(),
            game_flags: 3,
            ownership: 4,
            parent_urn: "urn".into(),
            ..Default::default()
        };
        let parent = InsuranceItemPortParams { items: vec![item], ..Default::default() };
        let bytes = InsuranceItemPortLoadout { items: vec![parent] }.encode_to_vec();
        let loadout = ItemPortLoadout::decode(bytes.as_slice()).unwrap();
        let ItemPortParams { port_name, item_class_name, game_flags, ownership, parent_urn, .. } =
            loadout.items[0].items[0].clone();
        assert_eq!((port_name.as_str(), item_class_name.as_str()), ("hardpoint", "gun"));
        assert_eq!((game_flags, ownership, parent_urn.as_deref()), (3, 4, Some("urn")));
    }

    #[test]
    fn stored_configuration_converts_to_v1() {
        use entity_configuration_parameters::Param as Stored;
        let parameter = |param| EntityConfigurationParameters { param: Some(param) };
        let child = PortConfig { r#ref: "child".into(), ports: Vec::new() };
        let configuration = EntityConfiguration {
            id: "id".into(),
            params: vec![
                parameter(Stored::WebCustomizations(PortConfig { r#ref: "root".into(), ports: vec![child] })),
                parameter(Stored::LoadoutKit(LoadoutKitConfiguration { code: "c".into(), name: "n".into() })),
                parameter(Stored::CustomName("name".into())),
                parameter(Stored::Serial("serial".into())),
                EntityConfigurationParameters { param: None },
            ],
        };
        let converted = entity_config(configuration);
        assert_eq!(converted.params.len(), 4);
        let Some(Param::EntityPort(port)) = &converted.params[0].param else { panic!("port expected") };
        assert_eq!(port.ports[0].ports[0].r#ref, "child");
        let Some(Param::EntityName(name)) = &converted.params[2].param else { panic!("name expected") };
        assert_eq!(name.name, "name");
        let Some(Param::EntitySerial(serial)) = &converted.params[3].param else { panic!("serial expected") };
        assert_eq!(serial.serial, "serial");
    }

    #[test]
    fn item_types_map_from_their_names() {
        assert_eq!(item_type("Ship"), EntitlementItemType::Ship);
        assert_eq!(item_type(""), EntitlementItemType::Unspecified);
        assert_eq!(item_type("WeaponPersonal"), EntitlementItemType::Other);
    }

    #[test]
    fn the_status_mapping_follows_the_enums() {
        for (stored, v1) in [
            (StoredStatus::Packaged as i32, EntitlementStatus::Unclaimed as i32),
            (StoredStatus::Undelivered as i32, EntitlementStatus::Pending as i32),
            (StoredStatus::Delivered as i32, EntitlementStatus::Fulfilled as i32),
            (StoredStatus::Revoked as i32, EntitlementStatus::Revoked as i32),
            (StoredStatus::Failed as i32, EntitlementStatus::Failed as i32),
        ] {
            assert!(V1_STATUS.contains(&format!("WHEN {stored} THEN {v1}")));
        }
        assert!(V1_TYPE.contains(&format!("WHEN {} THEN 2", EntitlementSourceType::Rental as i32)));
    }
}

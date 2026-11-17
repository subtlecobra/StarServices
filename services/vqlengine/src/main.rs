//! sc.game.editor.vqlengine.v1.VqlEngineService, for CIG's editor tools.
//!
//! The client holds neither the VQL grammar nor the tag database behind this service, so the language in
//! vql.rs is invented and the vql_* tables are filled by an import outside this API.

mod vql;

use std::{collections::HashMap, net::SocketAddr, process::ExitCode};

use clap::Parser;
use sc_db::{internal, DatabaseArgs, Postgres, Row, ToSql};
use sc_proto::sc::game::editor::vqlengine::v1::{
    vql_engine_service_server::{VqlEngineService, VqlEngineServiceServer},
    BrowseTagPathRequest, BrowseTagPathResponse, CompleteVqlRequest, CompleteVqlResponse, DescribeDomainsRequest,
    DescribeDomainsResponse, DomainDescription, FindLocationsRequest, FindLocationsResponse, GetManualRequest,
    GetManualResponse, GetRecordRequest, GetRecordResponse, GetStatusRequest, GetTagNameRequest, GetTagNameResponse,
    LocationResult, ResolvedField, SearchTagsRequest, SearchTagsResponse, StatusResponse, TagPathEntry, TagSearchTerm,
    TagspaceAlias, ValidateRequest, ValidateResponse, ValidationViolation, VqlError, VqlQueryRequest, VqlQueryResponse,
    VqlRow,
};
use sc_runtime::{CommonArgs, MAX_MESSAGE_BYTES};
use tonic::{codec::CompressionEncoding, transport::Server, Request, Response, Status};
use tracing::info;

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
const DB_PATH: &str = "yugabyte:vql_*";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS vql_tags (
    guid TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    parent_guid TEXT NOT NULL DEFAULT '',
    full_path TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS vql_tags_parent ON vql_tags (parent_guid);
CREATE INDEX IF NOT EXISTS vql_tags_path ON vql_tags (full_path);
CREATE TABLE IF NOT EXISTS vql_tagspace_aliases (
    alias TEXT PRIMARY KEY,
    root_path TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS vql_records (
    guid TEXT PRIMARY KEY,
    record_type TEXT NOT NULL,
    file_path TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS vql_record_fields (
    record_guid TEXT NOT NULL,
    field_path TEXT NOT NULL,
    value TEXT NOT NULL,
    value_type TEXT NOT NULL DEFAULT '',
    inherited BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (record_guid, field_path)
);
CREATE TABLE IF NOT EXISTS vql_locations (
    location_def_guid TEXT PRIMARY KEY,
    oc_asset_key TEXT NOT NULL DEFAULT '',
    display_name TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS vql_location_tags (
    location_def_guid TEXT NOT NULL,
    tag_guid TEXT NOT NULL,
    PRIMARY KEY (location_def_guid, tag_guid)
);
CREATE TABLE IF NOT EXISTS vql_violations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    domain TEXT NOT NULL,
    source_file TEXT NOT NULL DEFAULT '',
    element_id TEXT NOT NULL DEFAULT '',
    rule TEXT NOT NULL,
    severity TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT ''
);";

const MANUAL: &[(&str, &str)] = &[
    ("overview", "This backend answers a small query language over tags, records, locations and violations."),
    ("syntax", "[SELECT * | SELECT field, ...] FROM domain [WHERE field op value [AND field op value ...]]"),
    ("operators", "= and != compare text. LIKE takes % for any run of characters and _ for one character."),
    ("values", "A value is a bare word or text in single quotes."),
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

struct Engine {
    db: Postgres,
}

fn limit_of(limit: u32) -> i64 {
    i64::from(if limit == 0 { DEFAULT_LIMIT } else { limit.min(MAX_LIMIT) })
}

/// Text for LIKE, matched literally.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn entry(row: &Row) -> TagPathEntry {
    TagPathEntry {
        name: row.get("name"),
        full_path: row.get("full_path"),
        guid: row.get("guid"),
        child_count: row.get::<_, i64>("child_count") as i32,
    }
}

/// The tags of `tags` the term asks for, if the location satisfies it.
fn term_matches(term: &TagSearchTerm, tags: &[String]) -> Option<Vec<String>> {
    let wanted = term.positive_tag_guids.iter().all(|tag| tags.contains(tag));
    let barred = term.negative_tag_guids.iter().any(|tag| tags.contains(tag));
    (wanted && !barred).then(|| term.positive_tag_guids.clone())
}

/// Terms are alternatives. With no terms every location matches.
fn location_matches(terms: &[TagSearchTerm], tags: &[String]) -> Option<Vec<String>> {
    if terms.is_empty() {
        return Some(Vec::new());
    }
    let mut found: Option<Vec<String>> = None;
    for matched in terms.iter().filter_map(|term| term_matches(term, tags)) {
        let all = found.get_or_insert_with(Vec::new);
        for tag in matched {
            if !all.contains(&tag) {
                all.push(tag);
            }
        }
    }
    found
}

fn manual(section: &str) -> Option<String> {
    if section.is_empty() {
        return Some(MANUAL.iter().map(|(_, text)| *text).collect::<Vec<_>>().join("\n\n"));
    }
    MANUAL.iter().find(|(name, _)| *name == section).map(|(_, text)| text.to_string())
}

impl Engine {
    async fn count(&self, table: &str) -> Result<i64, Status> {
        let row = self
            .db
            .query_opt(&format!("SELECT count(*) AS total FROM {table}"), &[])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("count returned nothing"))?;
        Ok(row.get("total"))
    }
}

#[tonic::async_trait]
impl VqlEngineService for Engine {
    async fn query(&self, request: Request<VqlQueryRequest>) -> Result<Response<VqlQueryResponse>, Status> {
        let request = request.into_inner();
        let parsed = vql::parse(&request.vql).map_err(|error| {
            let (line, column) = vql::position(&request.vql, error.offset);
            Status::invalid_argument(format!("{line}:{column}: {}", error.message))
        })?;
        let limit = limit_of(request.limit);
        let offset = i64::from(request.offset);
        let mut parameters: Vec<&(dyn ToSql + Sync)> =
            parsed.values.iter().map(|value| value as &(dyn ToSql + Sync)).collect();
        let total = self
            .db
            .query_opt(&parsed.count_sql(), &parameters)
            .await
            .map_err(internal)?
            .map_or(0, |row| row.get::<_, i64>("total"));
        parameters.push(&limit);
        parameters.push(&offset);
        let translated_sql = parsed.sql();
        let rows = self.db.query(&translated_sql, &parameters).await.map_err(internal)?;
        let rows = rows
            .iter()
            .map(|row| VqlRow {
                fields: parsed
                    .fields
                    .iter()
                    .map(|field| (field.to_string(), row.get::<_, Option<String>>(*field).unwrap_or_default()))
                    .collect(),
            })
            .collect();
        Ok(Response::new(VqlQueryResponse {
            rows,
            domain: parsed.domain.name.to_string(),
            total_count: total as u32,
            translated_sql,
        }))
    }

    async fn get_tag_name(&self, request: Request<GetTagNameRequest>) -> Result<Response<GetTagNameResponse>, Status> {
        let guid = request.into_inner().tag_guid;
        let row = self
            .db
            .query_opt("SELECT name, parent_guid, full_path FROM vql_tags WHERE guid = $1", &[&guid])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no tag {guid}")))?;
        Ok(Response::new(GetTagNameResponse {
            tag_name: row.get("name"),
            parent_guid: row.get("parent_guid"),
            full_path: row.get("full_path"),
        }))
    }

    async fn get_status(&self, _request: Request<GetStatusRequest>) -> Result<Response<StatusResponse>, Status> {
        let row = self
            .db
            .query_opt("SELECT count(*) AS total, max(updated_at)::text AS updated FROM vql_tags", &[])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("count returned nothing"))?;
        let tag_count = row.get::<_, i64>("total") as u64;
        Ok(Response::new(StatusResponse {
            ready: tag_count > 0,
            tag_count,
            last_updated: row.get::<_, Option<String>>("updated").unwrap_or_default(),
            domains: vql::DOMAINS.iter().map(|domain| domain.name.to_string()).collect(),
            db_path: DB_PATH.into(),
        }))
    }

    async fn find_locations(
        &self,
        request: Request<FindLocationsRequest>,
    ) -> Result<Response<FindLocationsResponse>, Status> {
        let request = request.into_inner();
        let rows = self
            .db
            .query("SELECT location_def_guid, oc_asset_key, display_name FROM vql_locations ORDER BY 1", &[])
            .await
            .map_err(internal)?;
        let tag_rows = self.db.query("SELECT location_def_guid, tag_guid FROM vql_location_tags", &[]).await.map_err(internal)?;
        let mut tags: HashMap<String, Vec<String>> = HashMap::new();
        for row in &tag_rows {
            tags.entry(row.get("location_def_guid")).or_default().push(row.get("tag_guid"));
        }
        let matched: Vec<LocationResult> = rows
            .iter()
            .filter_map(|row| {
                let guid: String = row.get("location_def_guid");
                let found = location_matches(&request.terms, tags.get(&guid).map_or(&[][..], Vec::as_slice))?;
                Some(LocationResult {
                    location_def_guid: guid,
                    oc_asset_key: row.get("oc_asset_key"),
                    display_name: row.get("display_name"),
                    matched_tag_guids: found,
                })
            })
            .collect();
        let total_count = matched.len() as u32;
        let locations = matched
            .into_iter()
            .skip(request.offset as usize)
            .take(limit_of(request.limit) as usize)
            .collect();
        Ok(Response::new(FindLocationsResponse { locations, total_count }))
    }

    async fn get_record(&self, request: Request<GetRecordRequest>) -> Result<Response<GetRecordResponse>, Status> {
        let guid = request.into_inner().guid;
        let record = self
            .db
            .query_opt("SELECT record_type, file_path FROM vql_records WHERE guid = $1", &[&guid])
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no record {guid}")))?;
        let rows = self
            .db
            .query(
                "SELECT field_path, value, value_type, inherited FROM vql_record_fields
                 WHERE record_guid = $1 ORDER BY field_path",
                &[&guid],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(GetRecordResponse {
            guid,
            record_type: record.get("record_type"),
            file_path: record.get("file_path"),
            fields: rows
                .iter()
                .map(|row| ResolvedField {
                    field_path: row.get("field_path"),
                    value: row.get("value"),
                    value_type: row.get("value_type"),
                    inherited: row.get("inherited"),
                })
                .collect(),
        }))
    }

    async fn validate(&self, request: Request<ValidateRequest>) -> Result<Response<ValidateResponse>, Status> {
        let ValidateRequest { domain, rule } = request.into_inner();
        let rows = self
            .db
            .query(
                "SELECT source_file, element_id, rule, severity, description FROM vql_violations
                 WHERE ($1 = '' OR domain = $1) AND ($2 = '' OR rule = $2) ORDER BY source_file, element_id",
                &[&domain, &rule],
            )
            .await
            .map_err(internal)?;
        let violations: Vec<ValidationViolation> = rows
            .iter()
            .map(|row| ValidationViolation {
                source_file: row.get("source_file"),
                element_id: row.get("element_id"),
                rule: row.get("rule"),
                severity: row.get("severity"),
                description: row.get("description"),
            })
            .collect();
        Ok(Response::new(ValidateResponse { total_count: violations.len() as u32, violations }))
    }

    /// An empty prefix lists the root tags.
    async fn browse_tag_path(
        &self,
        request: Request<BrowseTagPathRequest>,
    ) -> Result<Response<BrowseTagPathResponse>, Status> {
        let path = request.into_inner().path_prefix;
        let parent = if path.is_empty() {
            None
        } else {
            let row = self
                .db
                .query_opt("SELECT guid FROM vql_tags WHERE full_path = $1", &[&path])
                .await
                .map_err(internal)?
                .ok_or_else(|| Status::not_found(format!("no tag at {path}")))?;
            Some(row.get::<_, String>("guid"))
        };
        let parent_guid = parent.clone().unwrap_or_default();
        let rows = self
            .db
            .query(
                "SELECT t.name, t.full_path, t.guid,
                        (SELECT count(*) FROM vql_tags c WHERE c.parent_guid = t.guid) AS child_count
                 FROM vql_tags t WHERE t.parent_guid = $1 ORDER BY t.name",
                &[&parent_guid],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(BrowseTagPathResponse {
            resolved_guid: parent_guid,
            resolved_path: path,
            children: rows.iter().map(entry).collect(),
        }))
    }

    async fn complete_vql(&self, request: Request<CompleteVqlRequest>) -> Result<Response<CompleteVqlResponse>, Status> {
        let request = request.into_inner();
        let mut cursor = (request.cursor_offset as usize).min(request.vql.len());
        while !request.vql.is_char_boundary(cursor) {
            cursor -= 1;
        }
        let prefix = &request.vql[..cursor];
        let (context, current_domain, expected_keywords) = vql::complete(prefix);
        let rows = self
            .db
            .query("SELECT alias, root_path FROM vql_tagspace_aliases ORDER BY alias", &[])
            .await
            .map_err(internal)?;
        let tagspace_aliases: Vec<TagspaceAlias> =
            rows.iter().map(|row| TagspaceAlias { alias: row.get("alias"), root_path: row.get("root_path") }).collect();
        let word = prefix.rsplit(char::is_whitespace).next().unwrap_or_default();
        let alias = word.split(':').next().and_then(|name| tagspace_aliases.iter().find(|alias| alias.alias == name));
        let errors = match vql::tokenize(prefix) {
            Err(error) => {
                let (line, column) = vql::position(prefix, error.offset);
                vec![VqlError { line, column, message: error.message }]
            }
            Ok(_) => Vec::new(),
        };
        Ok(Response::new(CompleteVqlResponse {
            context: context.into(),
            current_domain,
            alias_name: alias.map(|alias| alias.alias.clone()).unwrap_or_default(),
            alias_root_path: alias.map(|alias| alias.root_path.clone()).unwrap_or_default(),
            tagspace_aliases,
            expected_keywords,
            errors,
        }))
    }

    async fn search_tags(&self, request: Request<SearchTagsRequest>) -> Result<Response<SearchTagsResponse>, Status> {
        let request = request.into_inner();
        let pattern = format!("%{}%", escape_like(&request.query));
        let limit = limit_of(request.limit);
        let rows = self
            .db
            .query(
                "SELECT t.name, t.full_path, t.guid,
                        (SELECT count(*) FROM vql_tags c WHERE c.parent_guid = t.guid) AS child_count
                 FROM vql_tags t WHERE t.name ILIKE $1 OR t.full_path ILIKE $1 ORDER BY t.full_path LIMIT $2",
                &[&pattern, &limit],
            )
            .await
            .map_err(internal)?;
        Ok(Response::new(SearchTagsResponse { results: rows.iter().map(entry).collect() }))
    }

    async fn describe_domains(
        &self,
        _request: Request<DescribeDomainsRequest>,
    ) -> Result<Response<DescribeDomainsResponse>, Status> {
        let mut domains = Vec::with_capacity(vql::DOMAINS.len());
        for domain in vql::DOMAINS {
            domains.push(DomainDescription {
                name: domain.name.into(),
                description: domain.description.into(),
                fields: domain.fields.iter().map(|field| field.to_string()).collect(),
                row_count: self.count(domain.table).await? as u32,
            });
        }
        Ok(Response::new(DescribeDomainsResponse { domains }))
    }

    async fn get_manual(&self, request: Request<GetManualRequest>) -> Result<Response<GetManualResponse>, Status> {
        let section = request.into_inner().section;
        let content = manual(&section).ok_or_else(|| Status::not_found(format!("no manual section {section}")))?;
        Ok(Response::new(GetManualResponse {
            content,
            sections: MANUAL.iter().map(|(name, _)| name.to_string()).collect(),
        }))
    }
}

async fn serve(args: Args) -> Result<(), String> {
    let service = Engine { db: Postgres::connect(&args.database.postgres, SCHEMA).await? };
    let server = VqlEngineServiceServer::new(service)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(MAX_MESSAGE_BYTES);
    info!(listen = %args.listen, "vql engine listening");
    Server::builder().add_service(server).serve(args.listen).await.map_err(|e| e.to_string())
}

fn main() -> ExitCode {
    let args = Args::parse();
    sc_runtime::run(args.common.health, serve(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(positive: &[&str], negative: &[&str]) -> TagSearchTerm {
        TagSearchTerm {
            positive_tag_guids: positive.iter().map(|tag| tag.to_string()).collect(),
            negative_tag_guids: negative.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    #[test]
    fn locations_match_any_term_and_honour_negatives() {
        let tags = vec!["a".to_string(), "b".to_string()];
        assert_eq!(location_matches(&[term(&["a"], &[])], &tags), Some(vec!["a".to_string()]));
        assert_eq!(location_matches(&[term(&["a"], &["b"])], &tags), None);
        assert_eq!(location_matches(&[term(&["z"], &[]), term(&["b"], &[])], &tags), Some(vec!["b".to_string()]));
        assert_eq!(location_matches(&[term(&["a"], &[]), term(&["a", "b"], &[])], &tags).unwrap().len(), 2);
        assert_eq!(location_matches(&[], &tags), Some(Vec::new()));
    }

    #[test]
    fn like_text_is_escaped() {
        assert_eq!(escape_like("50%_a\\"), "50\\%\\_a\\\\");
    }

    #[test]
    fn limits_have_a_default_and_a_ceiling() {
        assert_eq!(limit_of(0), 100);
        assert_eq!(limit_of(5000), 1000);
    }

    #[test]
    fn the_manual_lists_its_sections() {
        assert!(manual("").unwrap().contains("FROM domain"));
        assert!(manual("syntax").is_some());
        assert!(manual("nothing").is_none());
    }
}

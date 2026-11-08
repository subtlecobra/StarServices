//! `scbackend.internal.BlueprintLibraryInternal`: the blueprints every player knows, from the game data,
//! which `sc-admin seed` reads from the archive and hands over. Rows are replaced by key.

use sc_db::{internal, Postgres, ToSql};
use sc_api::internal::{
    blueprint_library_internal_server::BlueprintLibraryInternal, SeedBlueprintDefaultsRequest, SeedResponse,
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Library;

const SEED_ROWS: usize = 500;

pub async fn seed(db: &Postgres, request: &SeedBlueprintDefaultsRequest) -> Result<u64, Status> {
    if request.blueprints.iter().any(|blueprint| blueprint.blueprint_id.is_empty()) {
        return Err(Status::invalid_argument("a blueprint with no id"));
    }
    let mut written = 0;
    for rows in request.blueprints.chunks(SEED_ROWS) {
        let ids: Vec<&str> = rows.iter().map(|row| row.blueprint_id.as_str()).collect();
        let categories: Vec<&str> = rows.iter().map(|row| row.category_id.as_str()).collect();
        let classes: Vec<&str> = rows.iter().map(|row| row.item_class_id.as_str()).collect();
        let processes: Vec<i32> = rows.iter().map(|row| row.process_type).collect();
        let params: [&(dyn ToSql + Sync); 4] = [&ids, &categories, &classes, &processes];
        written += db
            .execute(
                "INSERT INTO blueprint_defaults (blueprint_id, category_id, item_class_id, process_type)
                 SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::int[])
                 ON CONFLICT (blueprint_id) DO UPDATE SET category_id = excluded.category_id,
                     item_class_id = excluded.item_class_id, process_type = excluded.process_type",
                &params,
            )
            .await
            .map_err(internal)?;
    }
    Ok(written)
}

#[tonic::async_trait]
impl BlueprintLibraryInternal for Library {
    async fn seed_blueprint_defaults(
        &self,
        request: Request<SeedBlueprintDefaultsRequest>,
    ) -> Result<Response<SeedResponse>, Status> {
        let rows = seed(&self.db, &request.into_inner()).await?;
        info!(rows, "default blueprints seeded");
        Ok(Response::new(SeedResponse { rows }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_api::internal::BlueprintDefault;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-blueprint-library -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn seeding_replaces_by_key() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let id = format!("test-blueprint-{}", std::process::id());
        let blueprint = |process_type| BlueprintDefault { blueprint_id: id.clone(), process_type, ..Default::default() };
        let request = |process_type| SeedBlueprintDefaultsRequest { blueprints: vec![blueprint(process_type)] };
        assert_eq!(seed(&db, &request(1)).await.unwrap(), 1);
        seed(&db, &request(2)).await.unwrap();
        let row = db.query_opt("SELECT process_type FROM blueprint_defaults WHERE blueprint_id = $1", &[&id]).await;
        assert_eq!(row.unwrap().unwrap().get::<_, i32>(0), 2);
        db.execute("DELETE FROM blueprint_defaults WHERE blueprint_id = $1", &[&id]).await.unwrap();
    }
}

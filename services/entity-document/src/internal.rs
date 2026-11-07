//! `scbackend.internal.EntityDocumentInternal`: the admin site reads a player's documents and replaces one, such as
//! `player_home_location`. A write replaces the document whole, so repeating it changes nothing but the revision, and
//! an expected revision keeps it from overwriting a change it did not see.

use prost::Message;
use sc_db::{internal, Row};
use sc_proto::{
    internal::{
        entity_document_internal_server::EntityDocumentInternal, GetEntityDocumentsRequest,
        GetEntityDocumentsResponse, PutEntityDocumentRequest, PutEntityDocumentResponse, StoredEntityDocument,
    },
    services::entity_document::v1::{DocumentHeader, EntityDocument, EntityDocumentBody},
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::Documents;

fn stored(row: &Row) -> Result<StoredEntityDocument, Status> {
    let body: Option<Vec<u8>> = row.get("body");
    let body = body.map(|bytes| EntityDocumentBody::decode(bytes.as_slice())).transpose().map_err(internal)?;
    Ok(StoredEntityDocument {
        class: row.get("class"),
        document: Some(EntityDocument {
            header: Some(DocumentHeader {
                entity_id: row.get::<_, i64>("entity_id") as u64,
                generation: row.get::<_, i64>("generation") as u32,
                revision: row.get::<_, i64>("revision").to_string(),
            }),
            body,
        }),
        owner_id: row.get::<_, i64>("owner_id") as u64,
        prepared: row.get("prepared"),
    })
}

/// Whether a document at `found` (None: there is none) may be replaced by a writer that expects `expected`.
fn revision_matches(expected: Option<u64>, found: Option<i64>) -> bool {
    match (expected, found) {
        (None, _) => true,
        (Some(0), None) => true,
        (Some(expected), Some(found)) => expected as i64 == found,
        (Some(_), None) => false,
    }
}

#[tonic::async_trait]
impl EntityDocumentInternal for Documents {
    async fn get_documents(
        &self,
        request: Request<GetEntityDocumentsRequest>,
    ) -> Result<Response<GetEntityDocumentsResponse>, Status> {
        let request = request.into_inner();
        let rows = self
            .db
            .query(
                "SELECT entity_id, class, generation, revision, owner_id, body, prepared IS NOT NULL AS prepared
                 FROM entity_documents WHERE entity_id = $1 AND (cardinality($2::text[]) = 0 OR class = ANY($2))
                 ORDER BY class",
                &[&(request.entity_id as i64), &request.classes],
            )
            .await
            .map_err(internal)?;
        let documents = rows.iter().map(stored).collect::<Result<_, _>>()?;
        Ok(Response::new(GetEntityDocumentsResponse { documents }))
    }

    async fn put_document(
        &self,
        request: Request<PutEntityDocumentRequest>,
    ) -> Result<Response<PutEntityDocumentResponse>, Status> {
        let request = request.into_inner();
        if request.entity_id == 0 {
            return Err(Status::invalid_argument("missing entity"));
        }
        let body = Self::encode(request.body)?;
        let class = self.class_of(&body)?.to_string();
        let entity = request.entity_id as i64;
        let mut client = self.db.client().await.map_err(internal)?;
        let transaction = client.transaction().await.map_err(internal)?;
        let before = transaction
            .query_opt(
                "SELECT entity_id, class, generation, revision, owner_id, body, prepared IS NOT NULL AS prepared
                 FROM entity_documents WHERE entity_id = $1 AND class = $2 FOR UPDATE",
                &[&entity, &class],
            )
            .await
            .map_err(internal)?;
        let found = before.as_ref().filter(|row| row.get::<_, Option<Vec<u8>>>("body").is_some());
        if !revision_matches(request.expected_revision, found.map(|row| row.get("revision"))) {
            return Err(Status::aborted(format!("{class} of {entity} changed meanwhile")));
        }
        let previous = found.map(stored).transpose()?.and_then(|stored| stored.document);
        let revision: i64 = transaction
            .query_one(
                "INSERT INTO entity_documents (entity_id, class, generation, owner_id, body) VALUES ($1, $2, 1, $3, $4)
                 ON CONFLICT (entity_id, class) DO UPDATE SET body = excluded.body,
                     owner_id = CASE WHEN $3 = 0 THEN entity_documents.owner_id ELSE $3 END,
                     revision = entity_documents.revision + 1
                 RETURNING revision",
                &[&entity, &class, &(request.owner_id as i64), &body],
            )
            .await
            .map_err(internal)?
            .get(0);
        transaction.commit().await.map_err(internal)?;
        info!(entity, %class, revision, "document replaced");
        Ok(Response::new(PutEntityDocumentResponse { previous, revision: revision as u64 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expected_revision_guards_the_write() {
        assert!(revision_matches(None, None));
        assert!(revision_matches(None, Some(4)));
        assert!(revision_matches(Some(0), None));
        assert!(!revision_matches(Some(0), Some(1)));
        assert!(revision_matches(Some(3), Some(3)));
        assert!(!revision_matches(Some(3), Some(4)));
        assert!(!revision_matches(Some(3), None));
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-entity-document -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn a_document_is_replaced_only_at_the_expected_revision() {
        use sc_proto::services::entity_document::v1::{entity_document_body::Class, PlayerHomeLocationDocument};
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let documents = Documents { db: sc_db::Postgres::connect(&url, crate::SCHEMA).await.unwrap(), classes: crate::class_names().unwrap() };
        let entity = 950_000_000 + u64::from(std::process::id());
        let home = |id| EntityDocumentBody { class: Some(Class::PlayerHomeLocation(PlayerHomeLocationDocument { home_location_id: id })) };
        let put = |id, expected| PutEntityDocumentRequest { entity_id: entity, owner_id: entity, body: Some(home(id)), expected_revision: expected };
        let first = documents.put_document(Request::new(put(5, Some(0)))).await.unwrap().into_inner();
        assert!(first.previous.is_none());
        let stale = documents.put_document(Request::new(put(6, Some(0)))).await.unwrap_err();
        assert_eq!(stale.code(), tonic::Code::Aborted);
        let second = documents.put_document(Request::new(put(6, Some(first.revision)))).await.unwrap().into_inner();
        assert_eq!(second.previous.unwrap().body, Some(home(5)));
        let read = GetEntityDocumentsRequest { entity_id: entity, classes: vec!["player_home_location".into()] };
        let found = documents.get_documents(Request::new(read)).await.unwrap().into_inner().documents;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].document.as_ref().unwrap().body, Some(home(6)));
        documents.db.execute("DELETE FROM entity_documents WHERE entity_id = $1", &[&(entity as i64)]).await.unwrap();
    }
}

//! Changing a score. A reputation moves by a number of points, never to a value: the change is one statement, so two
//! changes at once both count, and the result is kept under the scope's ceiling and inside the reputation's lock. Each
//! change is a row of the score history.

use sc_api::internal::{
    AdjustReputationRequest, AdjustReputationResponse, ListReputationScopesResponse, LadderStanding, ListReputationsResponse, ReputationEntry, ScopeSummary,
    SetReputationLockRequest, SetReputationLockResponse, DeleteReputationRequest, DeleteReputationResponse,
};
use sc_db::internal;
use tonic::Status;
use tracing::info;

use crate::Reputations;

/// The longest actor or reason kept.
const MAX_TEXT_BYTES: usize = 200;
/// The bounds of a score, in the `bigint` the statement computes in.
const SCORE_MIN: i64 = i32::MIN as i64;
const SCORE_MAX: i64 = i32::MAX as i64;

/// The reputation moves by `$4` (clamped by lock and ceiling), the new score is recorded in the history, and the old and
/// the new score are returned.
const ADJUST: &str = "
WITH target AS (
    SELECT r.id, r.score AS previous, r.score::bigint + $4::bigint AS raw, COALESCE(c.ceiling, 0) AS ceiling, r.lock_min, r.lock_max
    FROM reputations r LEFT JOIN reputation_scopes c ON c.scope = r.scope
    WHERE r.subject = $1 AND r.entity = $2 AND r.scope = $3
    FOR UPDATE OF r
), updated AS (
    UPDATE reputations r SET
        score = GREATEST($5::bigint, COALESCE(t.lock_min, $5::bigint),
                         LEAST($6::bigint, COALESCE(t.lock_max, $6::bigint), CASE WHEN t.ceiling > 0 THEN LEAST(t.ceiling::bigint, t.raw) ELSE t.raw END)),
        version = r.version + 1, updated_at = now()
    FROM target t WHERE r.id = t.id
    RETURNING r.id, r.score, t.previous
), recorded AS (
    INSERT INTO reputation_scores (reputation_id, score) SELECT id, score FROM updated
    ON CONFLICT (reputation_id, recorded_at) DO UPDATE SET score = EXCLUDED.score
)
SELECT score, previous FROM updated";

/// A player's reputations with the name of the standing each score is in: the ladder's standing with the greatest minimum
/// the score reaches (the lowest one when it reaches none).
const LIST: &str = "
SELECT r.entity, r.scope, COALESCE(c.display_name, '') AS display_name, r.score, r.lock_min, r.lock_max,
       COALESCE((SELECT s.name FROM reputation_standings s WHERE s.standings_id = COALESCE(NULLIF(r.standings_id, ''), c.standings_id)
                 ORDER BY (s.min <= r.score) DESC, CASE WHEN s.min <= r.score THEN -s.min ELSE s.min END LIMIT 1), '') AS standing
FROM reputations r LEFT JOIN reputation_scopes c ON c.scope = r.scope
WHERE r.subject = $1 ORDER BY r.scope, r.entity";

/// Locks the score inside the range and brings it there, recording the new score in the history.
const LOCK: &str = "
WITH updated AS (
    UPDATE reputations SET lock_min = $4::int, lock_max = $5::int,
        score = LEAST(COALESCE($5::int, score), GREATEST(COALESCE($4::int, score), score)), version = version + 1, updated_at = now()
    WHERE subject = $1 AND entity = $2 AND scope = $3
    RETURNING id, score
), recorded AS (
    INSERT INTO reputation_scores (reputation_id, score) SELECT id, score FROM updated
    ON CONFLICT (reputation_id, recorded_at) DO UPDATE SET score = EXCLUDED.score
)
SELECT score FROM updated";

/// Removes the reputation and its history in one statement.
const DELETE: &str = "
WITH gone AS (
    DELETE FROM reputations WHERE subject = $1 AND entity = $2 AND scope = $3 RETURNING id, score
), history AS (
    DELETE FROM reputation_scores WHERE reputation_id IN (SELECT id FROM gone)
)
SELECT score FROM gone";

fn short(text: &str) -> &str {
    let mut end = text.len().min(MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Reputations {
    pub async fn adjust(&self, request: &AdjustReputationRequest) -> Result<AdjustReputationResponse, Status> {
        if request.subject.is_empty() || request.entity.is_empty() || request.scope.is_empty() {
            return Err(Status::invalid_argument("a subject, an entity and a scope are needed"));
        }
        self.ensure(&request.subject, &request.entity, &request.scope).await?;
        let rows = self
            .db
            .query(ADJUST, &[&request.subject, &request.entity, &request.scope, &i64::from(request.delta), &SCORE_MIN, &SCORE_MAX])
            .await
            .map_err(internal)?;
        let row = rows.first().ok_or_else(|| Status::not_found("no such reputation"))?;
        let (score, previous): (i32, i32) = (row.get("score"), row.get("previous"));
        info!(
            subject = %request.subject, entity = %request.entity, scope = %request.scope, delta = request.delta, previous, score,
            actor = short(&request.actor), reason = short(&request.reason), "reputation changed",
        );
        Ok(AdjustReputationResponse { score, previous })
    }
}

impl Reputations {
    pub async fn list(&self, subject: &str) -> Result<ListReputationsResponse, Status> {
        if subject.is_empty() {
            return Err(Status::invalid_argument("a subject is needed"));
        }
        let rows = self.db.query(LIST, &[&subject]).await.map_err(internal)?;
        let reputations = rows
            .iter()
            .map(|row| ReputationEntry {
                entity: row.get("entity"),
                scope: row.get("scope"),
                display_name: row.get("display_name"),
                score: row.get("score"),
                standing: row.get("standing"),
                lock_min: row.get("lock_min"),
                lock_max: row.get("lock_max"),
            })
            .collect();
        Ok(ListReputationsResponse { reputations })
    }
}

impl Reputations {
    pub async fn scopes(&self) -> Result<ListReputationScopesResponse, Status> {
        let rows = self
            .db
            .query("SELECT scope, display_name, standings_id, initial_reputation, ceiling FROM reputation_scopes ORDER BY scope", &[])
            .await
            .map_err(internal)?;
        let standings = self.db.query("SELECT standings_id, name, min FROM reputation_standings ORDER BY standings_id, min, name", &[]).await.map_err(internal)?;
        let scopes = rows
            .iter()
            .map(|row| {
                let standings_id: String = row.get("standings_id");
                let ladder = standings
                    .iter()
                    .filter(|standing| standing.get::<_, String>("standings_id") == standings_id)
                    .map(|standing| LadderStanding { name: standing.get("name"), min: standing.get("min") })
                    .collect();
                ScopeSummary {
                    scope: row.get("scope"),
                    display_name: row.get("display_name"),
                    standings_id,
                    initial_reputation: row.get("initial_reputation"),
                    ceiling: row.get("ceiling"),
                    ladder,
                }
            })
            .collect();
        Ok(ListReputationScopesResponse { scopes })
    }

    pub async fn delete(&self, request: &DeleteReputationRequest) -> Result<DeleteReputationResponse, Status> {
        if request.subject.is_empty() || request.entity.is_empty() || request.scope.is_empty() {
            return Err(Status::invalid_argument("a subject, an entity and a scope are needed"));
        }
        let rows = self.db.query(DELETE, &[&request.subject, &request.entity, &request.scope]).await.map_err(internal)?;
        let row = rows.first().ok_or_else(|| Status::not_found("no such reputation"))?;
        let score: i32 = row.get("score");
        info!(
            subject = %request.subject, entity = %request.entity, scope = %request.scope, score,
            actor = short(&request.actor), reason = short(&request.reason), "reputation deleted",
        );
        Ok(DeleteReputationResponse { score })
    }

    pub async fn lock(&self, request: &SetReputationLockRequest) -> Result<SetReputationLockResponse, Status> {
        if request.subject.is_empty() || request.entity.is_empty() || request.scope.is_empty() {
            return Err(Status::invalid_argument("a subject, an entity and a scope are needed"));
        }
        if let (Some(low), Some(high)) = (request.lock_min, request.lock_max) {
            if low > high {
                return Err(Status::invalid_argument("the lock's minimum is above its maximum"));
            }
        }
        let rows = self.db.query(LOCK, &[&request.subject, &request.entity, &request.scope, &request.lock_min, &request.lock_max]).await.map_err(internal)?;
        let row = rows.first().ok_or_else(|| Status::not_found("no such reputation"))?;
        let score: i32 = row.get("score");
        info!(
            subject = %request.subject, entity = %request.entity, scope = %request.scope, lock_min = ?request.lock_min, lock_max = ?request.lock_max, score,
            actor = short(&request.actor), reason = short(&request.reason), "reputation locked",
        );
        Ok(SetReputationLockResponse { score })
    }
}

#[cfg(test)]
mod tests {
    use sc_api::internal::{ReputationScope, SeedReputationRequest};
    use sc_db::Postgres;

    use super::*;

    fn request(subject: &str, scope: &str, delta: i32) -> AdjustReputationRequest {
        AdjustReputationRequest { subject: subject.into(), entity: "faction".into(), scope: scope.into(), delta, ..Default::default() }
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-reputation -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn a_change_is_clamped_recorded_and_counted_once() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let state_dir = std::env::temp_dir().join(format!("sc-reputation-test-{}", std::process::id()));
        let reputations = Reputations { state: sc_runtime::state::State::open(&state_dir).unwrap(), db };
        let scope = format!("adjust-scope-{}", std::process::id());
        let subject = format!("urn:test:{}", std::process::id());
        let scopes = vec![ReputationScope { scope: scope.clone(), standings_id: "l".into(), initial_reputation: 10, ceiling: 50, ..Default::default() }];
        crate::seed::seed(&reputations.db, &SeedReputationRequest { scopes, ..Default::default() }).await.unwrap();
        // It starts at the initial value and moves by the delta.
        let first = reputations.adjust(&request(&subject, &scope, 15)).await.unwrap();
        assert_eq!((first.previous, first.score), (10, 25));
        // The ceiling holds, and a fall is not stopped by it.
        assert_eq!(reputations.adjust(&request(&subject, &scope, 1000)).await.unwrap().score, 50);
        assert_eq!(reputations.adjust(&request(&subject, &scope, -70)).await.unwrap().score, -20);
        // The history has a row for each change.
        let rows = reputations
            .db
            .query("SELECT count(*) AS n FROM reputation_scores s JOIN reputations r ON r.id = s.reputation_id WHERE r.subject = $1", &[&subject])
            .await
            .unwrap();
        assert!(rows[0].get::<_, i64>("n") >= 3);
        // A lock keeps it inside.
        reputations.db.execute("UPDATE reputations SET lock_min = -5, lock_max = 5 WHERE subject = $1", &[&subject]).await.unwrap();
        assert_eq!(reputations.adjust(&request(&subject, &scope, -100)).await.unwrap().score, -5);
        assert_eq!(reputations.adjust(&request(&subject, &scope, 100)).await.unwrap().score, 5);
        assert!(reputations.adjust(&request("", &scope, 1)).await.is_err());
        // The list shows it with its lock.
        let listed = reputations.list(&subject).await.unwrap().reputations;
        assert!(listed.len() == 1 && listed[0].score == 5 && listed[0].lock_min == Some(-5) && listed[0].lock_max == Some(5));
        assert!(reputations.list("").await.is_err());
        // Scopes are listed; a lock brings the score inside and can be removed.
        assert!(reputations.scopes().await.unwrap().scopes.iter().any(|known| known.scope == scope && known.ceiling == 50));
        let lock = |low, high| SetReputationLockRequest { subject: subject.clone(), entity: "faction".into(), scope: scope.clone(), lock_min: low, lock_max: high, ..Default::default() };
        assert_eq!(reputations.lock(&lock(Some(10), Some(20))).await.unwrap().score, 10);
        assert_eq!(reputations.lock(&lock(None, None)).await.unwrap().score, 10);
        assert_eq!(reputations.adjust(&request(&subject, &scope, 100)).await.unwrap().score, 50);
        assert!(reputations.lock(&lock(Some(9), Some(1))).await.is_err());
        let gone = DeleteReputationRequest { subject: subject.clone(), entity: "faction".into(), scope: scope.clone(), ..Default::default() };
        assert_eq!(reputations.delete(&gone).await.unwrap().score, 50);
        assert_eq!(reputations.delete(&gone).await.unwrap_err().code(), tonic::Code::NotFound);
        assert!(reputations.list(&subject).await.unwrap().reputations.is_empty());
        assert_eq!(reputations.lock(&SetReputationLockRequest { entity: "nobody".into(), ..lock(None, None) }).await.unwrap_err().code(), tonic::Code::NotFound);
    }
}

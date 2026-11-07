//! The purchase journal, in this service's own tables. `ea_transactions` holds the purchases made before
//! eatransaction became an orchestrator, each of them complete. It is only read now, so that a retried
//! old command still finds its rental. New purchases go to `ea_purchases`, a new table, so no existing
//! table changes shape.

use sc_db::{internal, Postgres, Row};
use tonic::Status;

use crate::purchase::{Journal, Record, Step};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ea_transactions (
    player_id BIGINT NOT NULL,
    command_id TEXT NOT NULL,
    rental_id TEXT NOT NULL,
    amount BIGINT NOT NULL,
    log_context BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, command_id)
);
CREATE TABLE IF NOT EXISTS ea_purchases (
    player_id BIGINT NOT NULL,
    command_id TEXT NOT NULL,
    account_id BIGINT NOT NULL,
    step TEXT NOT NULL,
    renewal BOOLEAN NOT NULL,
    rental_id TEXT NOT NULL,
    entity_class_guid TEXT NOT NULL,
    days INT NOT NULL,
    amount BIGINT NOT NULL,
    expire_at_ms BIGINT NOT NULL,
    log_context BYTEA NOT NULL,
    log_message TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    attempts INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (player_id, command_id)
);
CREATE INDEX IF NOT EXISTS ea_purchases_unfinished ON ea_purchases (updated_at)
    WHERE step <> 'done' AND step <> 'failed';";

const COLUMNS: &str = "player_id, command_id, account_id, step, renewal, rental_id, entity_class_guid, days, amount,
    expire_at_ms, log_context, log_message, error";

pub struct PgJournal {
    pub db: Postgres,
}

fn record(row: &Row) -> Result<Record, Status> {
    let step: String = row.get("step");
    Ok(Record {
        geid: row.get::<_, i64>("player_id") as u64,
        account_id: row.get::<_, i64>("account_id") as u32,
        command: row.get("command_id"),
        step: Step::from_name(&step).ok_or_else(|| Status::internal(format!("unknown purchase step {step}")))?,
        renewal: row.get("renewal"),
        rental_id: row.get("rental_id"),
        entity_class_guid: row.get("entity_class_guid"),
        days: row.get::<_, i32>("days") as u32,
        amount: row.get("amount"),
        expire_at_ms: row.get("expire_at_ms"),
        log_context: row.get("log_context"),
        log_message: row.get("log_message"),
        error: row.get("error"),
    })
}

#[tonic::async_trait]
impl Journal for PgJournal {
    async fn find(&self, geid: u64, command: &str) -> Result<Option<Record>, Status> {
        let geid = geid as i64;
        let sql = format!("SELECT {COLUMNS} FROM ea_purchases WHERE player_id = $1 AND command_id = $2");
        if let Some(row) = self.db.query_opt(&sql, &[&geid, &command]).await.map_err(internal)? {
            return record(&row).map(Some);
        }
        let legacy = self
            .db
            .query_opt(
                "SELECT rental_id, amount, log_context FROM ea_transactions WHERE player_id = $1 AND command_id = $2",
                &[&geid, &command],
            )
            .await
            .map_err(internal)?;
        Ok(legacy.map(|row| Record {
            geid: geid as u64,
            account_id: 0,
            command: command.to_string(),
            step: Step::Done,
            renewal: false,
            rental_id: row.get("rental_id"),
            entity_class_guid: String::new(),
            days: 0,
            amount: row.get("amount"),
            expire_at_ms: 0,
            log_context: row.get("log_context"),
            log_message: String::new(),
            error: String::new(),
        }))
    }

    async fn open(&self, record: &Record) -> Result<Record, Status> {
        self.db
            .execute(
                "INSERT INTO ea_purchases (player_id, command_id, account_id, step, renewal, rental_id, entity_class_guid,
                                           days, amount, expire_at_ms, log_context, log_message)
                 VALUES ($1, $2, $3, $4, $5, COALESCE(NULLIF($6, ''), gen_random_uuid()::text), $7, $8, $9, $10, $11, $12)
                 ON CONFLICT DO NOTHING",
                &[
                    &(record.geid as i64),
                    &record.command,
                    &i64::from(record.account_id),
                    &record.step.name(),
                    &record.renewal,
                    &record.rental_id,
                    &record.entity_class_guid,
                    &(record.days as i32),
                    &record.amount,
                    &record.expire_at_ms,
                    &record.log_context,
                    &record.log_message,
                ],
            )
            .await
            .map_err(internal)?;
        self.find(record.geid, &record.command).await?.ok_or_else(|| Status::internal("purchase not recorded"))
    }

    async fn advance(&self, record: &Record, to: Step) -> Result<bool, Status> {
        let moved = self
            .db
            .execute(
                "UPDATE ea_purchases SET step = $4, error = $5, updated_at = now()
                 WHERE player_id = $1 AND command_id = $2 AND step = $3",
                &[&(record.geid as i64), &record.command, &record.step.name(), &to.name(), &record.error],
            )
            .await
            .map_err(internal)?;
        Ok(moved > 0)
    }

    async fn note(&self, record: &Record, error: &str) -> Result<(), Status> {
        self.db
            .execute(
                "UPDATE ea_purchases SET error = $3, attempts = attempts + 1, updated_at = now()
                 WHERE player_id = $1 AND command_id = $2",
                &[&(record.geid as i64), &record.command, &error],
            )
            .await
            .map_err(internal)?;
        Ok(())
    }

    /// Those untouched for 30 s, so a purchase still running in a request is left to it.
    async fn unfinished(&self, limit: i64) -> Result<Vec<Record>, Status> {
        let sql = format!(
            "SELECT {COLUMNS} FROM ea_purchases
             WHERE step <> 'done' AND step <> 'failed' AND updated_at < now() - interval '30 seconds'
             ORDER BY updated_at LIMIT $1"
        );
        let rows = self.db.query(&sql, &[&limit]).await.map_err(internal)?;
        rows.iter().map(record).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-eatransaction -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn records_move_one_step_at_a_time_and_old_commands_are_found() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let journal = PgJournal { db: Postgres::connect(&url, SCHEMA).await.unwrap() };
        let geid = 920_000_000 + std::process::id() as u64;
        let order = Record {
            geid,
            account_id: 1,
            command: "c1".into(),
            step: Step::Pending,
            renewal: false,
            rental_id: String::new(),
            entity_class_guid: "ship".into(),
            days: 2,
            amount: 100,
            expire_at_ms: 5,
            log_context: vec![1],
            log_message: "m".into(),
            error: String::new(),
        };
        let opened = journal.open(&order).await.unwrap();
        assert!(!opened.rental_id.is_empty());
        assert_eq!(journal.open(&order).await.unwrap(), opened, "a repeat returns the first");
        assert!(journal.advance(&opened, Step::Charged).await.unwrap());
        assert!(!journal.advance(&opened, Step::Charged).await.unwrap(), "already moved on");
        assert_eq!(journal.find(geid, "c1").await.unwrap().unwrap().step, Step::Charged);

        journal
            .db
            .execute(
                "INSERT INTO ea_transactions (player_id, command_id, rental_id, amount, log_context)
                 VALUES ($1, 'old', 'r-old', 7, '')",
                &[&(geid as i64)],
            )
            .await
            .unwrap();
        let old = journal.find(geid, "old").await.unwrap().unwrap();
        assert_eq!((old.step, old.rental_id.as_str()), (Step::Done, "r-old"));

        journal.db.execute("DELETE FROM ea_purchases WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
        journal.db.execute("DELETE FROM ea_transactions WHERE player_id = $1", &[&(geid as i64)]).await.unwrap();
    }
}

//! `scbackend.internal.LedgerInternal`: charging and refunding for other services, sc-eatransaction first.
//!
//! Every operation has a key, recorded in `ledger_operations` in the same transaction as the balance
//! change. A debit repeated with its key returns the first result. A refund names the debit's key and
//! gives back what it took, once. A refund that finds no debit spends the key, so a debit that arrives
//! late, after its caller gave up and refunded, is refused instead of charging for nothing.

use sc_db::{internal, Postgres};
use sc_economy::{
    db,
    transaction::{retrying, Failure},
};
use sc_proto::internal::{
    ledger_internal_server::LedgerInternal, LedgerAdjustRequest, LedgerAdjustResponse, LedgerBalance,
    LedgerBalancesRequest, LedgerBalancesResponse, LedgerDebitRequest, LedgerDebitResponse, LedgerOperation,
    LedgerOperationsRequest, LedgerOperationsResponse, LedgerRefundRequest, LedgerRefundResponse,
};
use tonic::{Request, Response, Status};
use tracing::info;

use crate::{Ledgers, OPENED};

const DEBITED: &str = "debited";
const REFUNDED: &str = "refunded";
/// A refund came first, so the key can no longer charge.
const CANCELLED: &str = "cancelled";
/// An admin's credit or debit, with its signed amount.
const ADJUSTED: &str = "adjusted";
const DEFAULT_OPERATIONS: u32 = 50;
const MAX_OPERATIONS: u32 = 500;

/// The opening balance and currency of a ledger this service opens.
fn opening(ledger_name: &str) -> Option<(i64, i32)> {
    OPENED.iter().find(|(name, ..)| *name == ledger_name).map(|(_, amount, currency)| (*amount, *currency as i32))
}

fn check_debit(request: &LedgerDebitRequest) -> Result<(i64, i32), Status> {
    if request.key.is_empty() || request.owner_geid == 0 {
        return Err(Status::invalid_argument("a debit needs a key and an owner"));
    }
    if request.amount <= 0 {
        return Err(Status::invalid_argument("a debit needs a positive amount"));
    }
    opening(&request.ledger_name).ok_or_else(|| Status::invalid_argument(format!("no ledger {}", request.ledger_name)))
}

pub async fn debit(db: &Postgres, request: &LedgerDebitRequest) -> Result<LedgerDebitResponse, Failure> {
    let (opening_amount, currency) = check_debit(request)?;
    let geid = request.owner_geid as i64;
    let mut client = db.client().await.map_err(internal)?;
    let transaction = db!(client.transaction());
    let recorded = db!(transaction.query_opt(
        "INSERT INTO ledger_operations (key, owner_geid, ledger_name, amount, state) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT DO NOTHING RETURNING key",
        &[&request.key, &geid, &request.ledger_name, &request.amount, &DEBITED],
    ));
    if recorded.is_none() {
        let done = db!(transaction.query_one(
            "SELECT owner_geid, ledger_name, amount, state, version FROM ledger_operations WHERE key = $1",
            &[&request.key],
        ));
        let state: String = done.get("state");
        if state != DEBITED {
            return Err(Status::failed_precondition(format!("debit {} was {state}", request.key)).into());
        }
        let same = done.get::<_, i64>("owner_geid") == geid
            && done.get::<_, String>("ledger_name") == request.ledger_name
            && done.get::<_, i64>("amount") == request.amount;
        if !same {
            return Err(Status::invalid_argument(format!("debit {} was for something else", request.key)).into());
        }
        return Ok(LedgerDebitResponse { version: done.get::<_, i64>("version") as u64, replayed: true });
    }
    db!(transaction.execute(
        "INSERT INTO ledgers (owner_geid, account_id, name, amount, currency)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        &[&geid, &i64::from(request.account_id), &request.ledger_name, &opening_amount, &currency],
    ));
    // Dropping the transaction rolls the key back too, so a refused debit leaves nothing behind.
    let version: i64 = db!(transaction.query_opt(
        "UPDATE ledgers SET amount = amount - $1, version = version + 1, updated_at = now()
         WHERE owner_geid = $2 AND name = $3 AND amount >= $1 RETURNING version",
        &[&request.amount, &geid, &request.ledger_name],
    ))
    .ok_or_else(|| Status::failed_precondition("insufficient funds"))?
    .get(0);
    db!(transaction.execute("UPDATE ledger_operations SET version = $2 WHERE key = $1", &[&request.key, &version]));
    db!(transaction.commit());
    Ok(LedgerDebitResponse { version: version as u64, replayed: false })
}

pub async fn refund(db: &Postgres, request: &LedgerRefundRequest) -> Result<LedgerRefundResponse, Failure> {
    if request.key.is_empty() || request.owner_geid == 0 {
        return Err(Status::invalid_argument("a refund needs a key and an owner").into());
    }
    let geid = request.owner_geid as i64;
    let mut client = db.client().await.map_err(internal)?;
    let transaction = db!(client.transaction());
    let spent = db!(transaction.query_opt(
        "INSERT INTO ledger_operations (key, owner_geid, ledger_name, amount, state) VALUES ($1, $2, '', 0, $3)
         ON CONFLICT DO NOTHING RETURNING key",
        &[&request.key, &geid, &CANCELLED],
    ));
    if spent.is_some() {
        db!(transaction.commit());
        return Ok(LedgerRefundResponse { refunded: false, amount: 0, version: 0 });
    }
    let operation = db!(transaction.query_one(
        "SELECT owner_geid, ledger_name, amount, state, version FROM ledger_operations WHERE key = $1 FOR UPDATE",
        &[&request.key],
    ));
    if operation.get::<_, i64>("owner_geid") != geid {
        return Err(Status::invalid_argument(format!("debit {} is not this player's", request.key)).into());
    }
    let amount: i64 = operation.get("amount");
    let state: String = operation.get("state");
    match state.as_str() {
        REFUNDED => {
            let version = operation.get::<_, i64>("version") as u64;
            return Ok(LedgerRefundResponse { refunded: true, amount, version });
        }
        CANCELLED => return Ok(LedgerRefundResponse { refunded: false, amount: 0, version: 0 }),
        _ => {}
    }
    let ledger_name: String = operation.get("ledger_name");
    let version: i64 = db!(transaction.query_one(
        "UPDATE ledgers SET amount = amount + $1, version = version + 1, updated_at = now()
         WHERE owner_geid = $2 AND name = $3 RETURNING version",
        &[&amount, &geid, &ledger_name],
    ))
    .get(0);
    db!(transaction.execute(
        "UPDATE ledger_operations SET state = $2, version = $3, updated_at = now() WHERE key = $1",
        &[&request.key, &REFUNDED, &version],
    ));
    db!(transaction.commit());
    Ok(LedgerRefundResponse { refunded: true, amount, version: version as u64 })
}

fn check_adjust(request: &LedgerAdjustRequest) -> Result<(i64, i32), Status> {
    if request.key.is_empty() || request.owner_geid == 0 {
        return Err(Status::invalid_argument("an adjustment needs a key and an owner"));
    }
    if request.delta == 0 {
        return Err(Status::invalid_argument("an adjustment needs an amount"));
    }
    opening(&request.ledger_name).ok_or_else(|| Status::invalid_argument(format!("no ledger {}", request.ledger_name)))
}

/// Credits or debits by `delta`, once per key, recording the balance before and after with the reason and who asked.
pub async fn adjust(db: &Postgres, request: &LedgerAdjustRequest) -> Result<LedgerAdjustResponse, Failure> {
    let (opening_amount, currency) = check_adjust(request)?;
    let geid = request.owner_geid as i64;
    let mut client = db.client().await.map_err(internal)?;
    let transaction = db!(client.transaction());
    let recorded = db!(transaction.query_opt(
        "INSERT INTO ledger_operations (key, owner_geid, ledger_name, amount, state, reason, actor)
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING RETURNING key",
        &[&request.key, &geid, &request.ledger_name, &request.delta, &ADJUSTED, &request.reason, &request.actor],
    ));
    if recorded.is_none() {
        let done = db!(transaction.query_one(
            "SELECT owner_geid, ledger_name, amount, state, version, balance_before, balance_after
             FROM ledger_operations WHERE key = $1",
            &[&request.key],
        ));
        let same = done.get::<_, String>("state") == ADJUSTED
            && done.get::<_, i64>("owner_geid") == geid
            && done.get::<_, String>("ledger_name") == request.ledger_name
            && done.get::<_, i64>("amount") == request.delta;
        if !same {
            return Err(Status::failed_precondition(format!("key {} was used for something else", request.key)).into());
        }
        return Ok(LedgerAdjustResponse {
            balance_before: done.get::<_, Option<i64>>("balance_before").unwrap_or_default(),
            balance_after: done.get::<_, Option<i64>>("balance_after").unwrap_or_default(),
            version: done.get::<_, i64>("version") as u64,
            replayed: true,
        });
    }
    db!(transaction.execute(
        "INSERT INTO ledgers (owner_geid, account_id, name, amount, currency)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        &[&geid, &i64::from(request.account_id), &request.ledger_name, &opening_amount, &currency],
    ));
    // Dropping the transaction rolls the key back too, so a refused debit leaves nothing behind.
    let changed = db!(transaction.query_opt(
        "UPDATE ledgers SET amount = amount + $1, version = version + 1, updated_at = now()
         WHERE owner_geid = $2 AND name = $3 AND amount + $1 >= 0 RETURNING amount, version",
        &[&request.delta, &geid, &request.ledger_name],
    ))
    .ok_or_else(|| Status::failed_precondition("insufficient funds"))?;
    let after: i64 = changed.get("amount");
    let version: i64 = changed.get("version");
    let before = after - request.delta;
    db!(transaction.execute(
        "UPDATE ledger_operations SET version = $2, balance_before = $3, balance_after = $4 WHERE key = $1",
        &[&request.key, &version, &before, &after],
    ));
    db!(transaction.commit());
    Ok(LedgerAdjustResponse { balance_before: before, balance_after: after, version: version as u64, replayed: false })
}

pub async fn balances(db: &Postgres, owner_geid: u64) -> Result<Vec<LedgerBalance>, Status> {
    let rows = db
        .query(
            "SELECT name, amount, currency, version, (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms
             FROM ledgers WHERE owner_geid = $1 ORDER BY name",
            &[&(owner_geid as i64)],
        )
        .await
        .map_err(internal)?;
    Ok(rows
        .iter()
        .map(|row| LedgerBalance {
            ledger_name: row.get("name"),
            amount: row.get("amount"),
            currency: row.get("currency"),
            version: row.get::<_, i64>("version") as u64,
            updated_at_ms: row.get("updated_ms"),
        })
        .collect())
}

/// What an operation in `state` did to the balance: a debit took its amount, a refund gave it back.
fn change(state: &str, amount: i64) -> i64 {
    match state {
        DEBITED => -amount,
        ADJUSTED => amount,
        _ => 0,
    }
}

pub async fn operations(db: &Postgres, request: &LedgerOperationsRequest) -> Result<Vec<LedgerOperation>, Status> {
    let limit = match request.limit {
        0 => DEFAULT_OPERATIONS,
        limit => limit.min(MAX_OPERATIONS),
    };
    let rows = db
        .query(
            "SELECT key, ledger_name, amount, state, version, coalesce(reason, '') AS reason,
                    coalesce(actor, '') AS actor, balance_before, balance_after,
                    (extract(epoch FROM created_at) * 1000)::bigint AS created_ms,
                    (extract(epoch FROM updated_at) * 1000)::bigint AS updated_ms
             FROM ledger_operations
             WHERE owner_geid = $1 AND ($2::bigint = 0 OR created_at < to_timestamp($2::bigint / 1000.0))
             ORDER BY created_at DESC, key LIMIT $3",
            &[&(request.owner_geid as i64), &request.before_ms, &i64::from(limit)],
        )
        .await
        .map_err(internal)?;
    Ok(rows
        .iter()
        .map(|row| {
            let state: String = row.get("state");
            let amount: i64 = row.get("amount");
            LedgerOperation {
                key: row.get("key"),
                ledger_name: row.get("ledger_name"),
                amount,
                change: change(&state, amount),
                state,
                version: row.get::<_, i64>("version") as u64,
                reason: row.get("reason"),
                actor: row.get("actor"),
                created_at_ms: row.get("created_ms"),
                updated_at_ms: row.get("updated_ms"),
                balance_before: row.get("balance_before"),
                balance_after: row.get("balance_after"),
            }
        })
        .collect())
}

#[tonic::async_trait]
impl LedgerInternal for Ledgers {
    async fn debit(&self, request: Request<LedgerDebitRequest>) -> Result<Response<LedgerDebitResponse>, Status> {
        let request = request.into_inner();
        let response = retrying(|| debit(&self.db, &request)).await?;
        if !response.replayed {
            info!(owner = request.owner_geid, key = %request.key, amount = request.amount, "debited");
        }
        Ok(Response::new(response))
    }

    async fn refund(&self, request: Request<LedgerRefundRequest>) -> Result<Response<LedgerRefundResponse>, Status> {
        let request = request.into_inner();
        let response = retrying(|| refund(&self.db, &request)).await?;
        info!(owner = request.owner_geid, key = %request.key, refunded = response.refunded, "refund");
        Ok(Response::new(response))
    }

    async fn balances(&self, request: Request<LedgerBalancesRequest>) -> Result<Response<LedgerBalancesResponse>, Status> {
        let balances = balances(&self.db, request.into_inner().owner_geid).await?;
        Ok(Response::new(LedgerBalancesResponse { balances }))
    }

    async fn adjust(&self, request: Request<LedgerAdjustRequest>) -> Result<Response<LedgerAdjustResponse>, Status> {
        let request = request.into_inner();
        let response = retrying(|| adjust(&self.db, &request)).await?;
        if !response.replayed {
            info!(owner = request.owner_geid, key = %request.key, delta = request.delta, actor = %request.actor,
                reason = %request.reason, "adjusted");
        }
        Ok(Response::new(response))
    }

    async fn operations(
        &self,
        request: Request<LedgerOperationsRequest>,
    ) -> Result<Response<LedgerOperationsResponse>, Status> {
        let operations = operations(&self.db, &request.into_inner()).await?;
        Ok(Response::new(LedgerOperationsResponse { operations }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use sc_economy::ledger::REC_LEDGER;
    use sc_proto::services::ledger::v1::Currency;

    fn request(amount: i64) -> LedgerDebitRequest {
        LedgerDebitRequest {
            key: "k".into(),
            owner_geid: 7,
            account_id: 1,
            ledger_name: REC_LEDGER.into(),
            amount,
        }
    }

    #[test]
    fn a_debit_needs_a_known_ledger_and_a_positive_amount() {
        assert_eq!(check_debit(&request(5)).unwrap().1, Currency::Rec as i32);
        for refused in [
            request(0),
            request(-5),
            LedgerDebitRequest { ledger_name: "OTHER".into(), ..request(5) },
            LedgerDebitRequest { key: String::new(), ..request(5) },
            LedgerDebitRequest { owner_geid: 0, ..request(5) },
        ] {
            assert_eq!(check_debit(&refused).unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn an_adjustment_needs_a_key_an_owner_an_amount_and_a_known_ledger() {
        let full = LedgerAdjustRequest {
            key: "k".into(),
            owner_geid: 7,
            ledger_name: REC_LEDGER.into(),
            delta: -5,
            ..Default::default()
        };
        assert!(check_adjust(&full).is_ok());
        for refused in [
            LedgerAdjustRequest { delta: 0, ..full.clone() },
            LedgerAdjustRequest { key: String::new(), ..full.clone() },
            LedgerAdjustRequest { owner_geid: 0, ..full.clone() },
            LedgerAdjustRequest { ledger_name: "OTHER".into(), ..full.clone() },
        ] {
            assert_eq!(check_adjust(&refused).unwrap_err().code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn history_shows_what_each_operation_did_to_the_balance() {
        assert_eq!(change(DEBITED, 150), -150);
        assert_eq!(change(REFUNDED, 150), 0);
        assert_eq!(change(CANCELLED, 0), 0);
        assert_eq!(change(ADJUSTED, -20), -20);
        assert_eq!(change(ADJUSTED, 20), 20);
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-ledger -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn adjustments_apply_once_and_never_go_below_zero() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let geid = 920_000_000 + std::process::id() as u64;
        let credit = LedgerAdjustRequest {
            key: format!("admin:{geid}:1"),
            owner_geid: geid,
            account_id: 1,
            ledger_name: REC_LEDGER.into(),
            delta: 500,
            reason: "test".into(),
            actor: "tester".into(),
        };
        let first = retrying(|| adjust(&db, &credit)).await.unwrap();
        assert_eq!((first.balance_before, first.balance_after, first.replayed), (crate::OPENING_REC, crate::OPENING_REC + 500, false));
        let again = retrying(|| adjust(&db, &credit)).await.unwrap();
        assert_eq!((again.balance_after, again.version, again.replayed), (first.balance_after, first.version, true));
        let other = LedgerAdjustRequest { delta: 1, ..credit.clone() };
        assert_eq!(retrying(|| adjust(&db, &other)).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let too_much = LedgerAdjustRequest { key: format!("admin:{geid}:2"), delta: -(1 << 40), ..credit.clone() };
        assert_eq!(retrying(|| adjust(&db, &too_much)).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let found = balances(&db, geid).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].amount, crate::OPENING_REC + 500);
        let history = operations(&db, &LedgerOperationsRequest { owner_geid: geid, ..Default::default() }).await.unwrap();
        assert_eq!(history.len(), 1, "the refused debit left nothing");
        assert_eq!((history[0].change, history[0].reason.as_str(), history[0].balance_after), (500, "test", Some(first.balance_after)));
        db.execute("DELETE FROM ledger_operations WHERE owner_geid = $1", &[&(geid as i64)]).await.unwrap();
        db.execute("DELETE FROM ledgers WHERE owner_geid = $1", &[&(geid as i64)]).await.unwrap();
    }

    /// Against a real database: `SC_TEST_POSTGRES=<url> cargo test -p sc-ledger -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn debits_charge_once_and_refunds_give_back_once() {
        let url = std::env::var("SC_TEST_POSTGRES").expect("SC_TEST_POSTGRES");
        let db = Postgres::connect(&url, crate::SCHEMA).await.unwrap();
        let geid = 900_000_000 + std::process::id() as u64;
        let key = format!("test:{geid}");
        let balance = |db: Postgres| async move {
            let row = db
                .query_opt("SELECT amount FROM ledgers WHERE owner_geid = $1 AND name = $2", &[&(geid as i64), &REC_LEDGER])
                .await
                .unwrap();
            row.map_or(0, |row| row.get::<_, i64>(0))
        };
        let charge = LedgerDebitRequest { key: key.clone(), owner_geid: geid, amount: 150, ..request(0) };
        let first = retrying(|| debit(&db, &charge)).await.unwrap();
        assert!(!first.replayed);
        let again = retrying(|| debit(&db, &charge)).await.unwrap();
        assert!(again.replayed);
        assert_eq!(again.version, first.version);
        assert_eq!(balance(db.clone()).await, crate::OPENING_REC - 150);

        let other = LedgerDebitRequest { amount: 1, ..charge.clone() };
        assert_eq!(retrying(|| debit(&db, &other)).await.unwrap_err().code(), tonic::Code::InvalidArgument);

        let too_much = LedgerDebitRequest { key: format!("{key}:big"), amount: 1 << 40, ..charge.clone() };
        assert_eq!(retrying(|| debit(&db, &too_much)).await.unwrap_err().code(), tonic::Code::FailedPrecondition);
        let unused = LedgerRefundRequest { key: format!("{key}:never"), owner_geid: geid };
        // The refused debit left its key free, and a refund with no debit spends one.
        assert!(!retrying(|| refund(&db, &unused)).await.unwrap().refunded);
        let late = LedgerDebitRequest { key: unused.key.clone(), amount: 1, ..charge.clone() };
        assert_eq!(retrying(|| debit(&db, &late)).await.unwrap_err().code(), tonic::Code::FailedPrecondition);

        let back = LedgerRefundRequest { key: key.clone(), owner_geid: geid };
        let refunded = retrying(|| refund(&db, &back)).await.unwrap();
        assert!(refunded.refunded);
        assert_eq!(refunded.amount, 150);
        assert!(retrying(|| refund(&db, &back)).await.unwrap().refunded);
        assert_eq!(balance(db.clone()).await, crate::OPENING_REC);
        assert_eq!(retrying(|| debit(&db, &charge)).await.unwrap_err().code(), tonic::Code::FailedPrecondition);

        db.execute("DELETE FROM ledger_operations WHERE owner_geid = $1", &[&(geid as i64)]).await.unwrap();
        db.execute("DELETE FROM ledgers WHERE owner_geid = $1", &[&(geid as i64)]).await.unwrap();
    }
}

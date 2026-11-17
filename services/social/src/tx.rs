//! Group changes as transactions. A change reads the group, decides and writes, so it runs SERIALIZABLE:
//! under snapshot isolation a row lock alone does not help, because a second join that waited for the
//! lock still counts the members of its old snapshot. YugabyteDB answers a conflict with a serialization
//! error, and [`transaction!`](crate::transaction) runs the change again. Pushes wait for the commit, so
//! that a change that is rolled back or run again never reaches a client.
//!
//! Inside a change, every query goes through its `Tx`: a second pooled connection could wait forever
//! for a pool that the running transactions hold.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sc_db::{internal, Row, ToSql};
use tonic::{Code, Status};
use tracing::debug;

use crate::{push::Event, Context};

pub const BEGIN: &str = "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE";

const ATTEMPTS: u32 = 16;
const BACKOFF: Duration = Duration::from_millis(10);
const MAX_BACKOFF: Duration = Duration::from_millis(250);
const SERIALIZATION_FAILURE: &str = "40001";
const DEADLOCK_DETECTED: &str = "40P01";
const CONFLICT: &str = "transaction conflict";

pub type Pending<'a, T> = Pin<Box<dyn Future<Output = Result<T, Status>> + Send + 'a>>;
pub type Params<'a> = &'a [&'a (dyn ToSql + Sync)];

/// Turns a database error into a Status by its SQLSTATE. A conflict stays recognisable for [`Retry`].
pub fn failure(code: Option<&str>, message: String) -> Status {
    match code {
        Some(SERIALIZATION_FAILURE | DEADLOCK_DETECTED) => {
            debug!(error = %message, "transaction conflict");
            Status::aborted(CONFLICT)
        }
        _ => internal(message),
    }
}

/// What reads and writes rows: the pool through a [`Context`], or a running transaction.
pub trait Queries: Sync {
    fn query<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, Vec<Row>>;
    fn execute<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, u64>;
    /// The service's context, for what is not in the database.
    fn context(&self) -> &Context;

    /// The first row, for statements that match at most one.
    fn query_opt<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, Option<Row>> {
        Box::pin(async move { Ok(self.query(sql, params).await?.into_iter().next()) })
    }
}

impl Queries for Context {
    fn query<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, Vec<Row>> {
        Box::pin(async move { self.db.query(sql, params).await.map_err(internal) })
    }

    fn execute<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, u64> {
        Box::pin(async move { self.db.execute(sql, params).await.map_err(internal) })
    }

    fn context(&self) -> &Context {
        self
    }
}

/// Where events go: straight to the push service, or into a transaction until it commits.
pub trait Outbox {
    fn send(&self, account_id: u32, topic: String, event: &Event);
}

impl Outbox for Context {
    fn send(&self, account_id: u32, topic: String, event: &Event) {
        self.push.send(account_id, topic, event);
    }
}

// sc-db does not re-export the driver's transaction type, so nothing here can name it, and a `Tx`
// holds the transaction as closures. The `&'a &'t ()` tells the compiler that 't outlives 'a.
type Run<'t, T> = Box<dyn for<'a> Fn(&'a &'t (), &'a str, Params<'a>) -> Pending<'a, T> + Send + Sync + 't>;

/// A running transaction. Made by [`transaction!`](crate::transaction).
pub struct Tx<'t> {
    pub ctx: &'t Context,
    query: Run<'t, Vec<Row>>,
    execute: Run<'t, u64>,
    events: Mutex<Vec<(u32, String, Event)>>,
    wake: AtomicBool,
}

impl<'t> Tx<'t> {
    pub fn new<Q, E>(ctx: &'t Context, query: Q, execute: E) -> Self
    where
        Q: for<'a> Fn(&'a &'t (), &'a str, Params<'a>) -> Pending<'a, Vec<Row>> + Send + Sync + 't,
        E: for<'a> Fn(&'a &'t (), &'a str, Params<'a>) -> Pending<'a, u64> + Send + Sync + 't,
    {
        Self { ctx, query: Box::new(query), execute: Box::new(execute), events: Mutex::default(), wake: AtomicBool::new(false) }
    }

    /// Wakes whoever waits on [`Context::committed`] once this transaction has committed, such as a
    /// worker that carries out what the transaction recorded.
    pub fn notify_after_commit(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }

    /// Gives up the transaction's closures, so that it can commit, and returns what is to be sent and
    /// whether to notify.
    pub fn finish(self) -> (Vec<(u32, String, Event)>, bool) {
        (self.events.into_inner().unwrap(), self.wake.into_inner())
    }
}

impl Queries for Tx<'_> {
    fn query<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, Vec<Row>> {
        (self.query)(&&(), sql, params)
    }

    fn execute<'a>(&'a self, sql: &'a str, params: Params<'a>) -> Pending<'a, u64> {
        (self.execute)(&&(), sql, params)
    }

    fn context(&self) -> &Context {
        self.ctx
    }
}

impl Outbox for Tx<'_> {
    fn send(&self, account_id: u32, topic: String, event: &Event) {
        self.events.lock().unwrap().push((account_id, topic, event.clone()));
    }
}

/// Decides whether a failed attempt runs again, and waits before it does.
#[derive(Default)]
pub struct Retry {
    attempts: u32,
}

impl Retry {
    pub async fn again(&mut self, status: &Status) -> bool {
        self.attempts += 1;
        if !is_conflict(status) || self.attempts >= ATTEMPTS {
            return false;
        }
        tokio::time::sleep(backoff(self.attempts, jitter())).await;
        true
    }
}

pub fn is_conflict(status: &Status) -> bool {
    status.code() == Code::Aborted && status.message() == CONFLICT
}

/// Racing attempts that wait equally long would meet again, so each waits a different part of the step.
fn backoff(attempt: u32, jitter: u32) -> Duration {
    let step = BACKOFF.saturating_mul(1 << attempt.min(8)).min(MAX_BACKOFF);
    step / 2 + step.mul_f64(f64::from(jitter % 1000) / 2000.0)
}

fn jitter() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() / 1000)
}

/// Fixes the error type of a block that only says `?`.
pub fn fallible<T>(block: impl Future<Output = Result<T, Status>>) -> impl Future<Output = Result<T, Status>> {
    block
}

/// Turns a driver error into a Status.
#[doc(hidden)]
#[macro_export]
macro_rules! driver_failure {
    () => {
        |error| $crate::tx::failure(error.code().map(|code| code.code()), error.to_string())
    };
}

/// Runs a block as a transaction and evaluates to its `Result<_, Status>`. `tx` names the [`tx::Tx`]
/// in the block. The block commits by returning `Ok`, rolls back by returning `Err`, and can run more
/// than once, so it must leave what it captures as it found it.
///
/// [`tx::Tx`]: crate::tx::Tx
#[macro_export]
macro_rules! transaction {
    ($ctx:expr, $tx:ident, $body:block) => {{
        let context: &$crate::Context = $ctx;
        let mut retry = $crate::tx::Retry::default();
        loop {
            let outcome: Result<_, ::tonic::Status> = async {
                let mut client = context.db.client().await.map_err(|error| $crate::tx::failure(None, error))?;
                let running = client.transaction().await.map_err($crate::driver_failure!())?;
                running.batch_execute($crate::tx::BEGIN).await.map_err($crate::driver_failure!())?;
                let shared = &running;
                let $tx = $crate::tx::Tx::new(
                    context,
                    move |_, sql, params| {
                        Box::pin(async move { shared.query(sql, params).await.map_err($crate::driver_failure!()) })
                    },
                    move |_, sql, params| {
                        Box::pin(async move { shared.execute(sql, params).await.map_err($crate::driver_failure!()) })
                    },
                );
                let value = $crate::tx::fallible(async { $body }).await?;
                let (events, wake) = $tx.finish();
                running.commit().await.map_err($crate::driver_failure!())?;
                for (account_id, topic, event) in events {
                    context.push.send(account_id, topic, &event);
                }
                if wake {
                    context.committed.notify_one();
                }
                Ok(value)
            }
            .await;
            match outcome {
                Err(status) if retry.again(&status).await => continue,
                outcome => break outcome,
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_conflicts_are_recognised() {
        assert!(is_conflict(&failure(Some("40001"), "restart read".into())));
        assert!(is_conflict(&failure(Some("40P01"), "deadlock".into())));
        assert!(!is_conflict(&failure(Some("23505"), "duplicate".into())));
        assert!(!is_conflict(&failure(None, "no connection".into())));
        assert!(!is_conflict(&Status::aborted("invitation changed")));
    }

    #[tokio::test]
    async fn conflicts_run_again_until_the_attempts_run_out() {
        let conflict = failure(Some("40001"), "restart".into());
        let mut retry = Retry { attempts: ATTEMPTS - 2 };
        assert!(retry.again(&conflict).await);
        assert!(!retry.again(&conflict).await);
        assert!(!Retry::default().again(&Status::not_found("no group")).await);
    }

    #[test]
    fn backoff_grows_within_its_bounds() {
        assert!(backoff(1, 0) < backoff(3, 0));
        assert!(backoff(1, 999) >= backoff(1, 0));
        assert!(backoff(30, 999) <= MAX_BACKOFF);
    }
}

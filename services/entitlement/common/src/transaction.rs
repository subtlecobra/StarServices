//! Retrying transactions. Under contention YugabyteDB aborts a transaction with a serialization
//! error, and the caller is expected to run it again.

use std::{future::Future, time::Duration};

use tonic::Status;
use tracing::warn;

const ATTEMPTS: u32 = 6;
const BACKOFF: Duration = Duration::from_millis(25);
const SERIALIZATION_FAILURE: &str = "40001";
const DEADLOCK_DETECTED: &str = "40P01";

pub enum Failure {
    Conflict(String),
    Refused(Status),
}

impl From<Status> for Failure {
    fn from(status: Status) -> Self {
        Self::Refused(status)
    }
}

/// Classifies a database error by its SQLSTATE, for the `db!` macro.
pub fn database_failure(code: Option<&str>, message: String) -> Failure {
    match code {
        Some(SERIALIZATION_FAILURE | DEADLOCK_DETECTED) => Failure::Conflict(message),
        _ => {
            warn!(error = %message, "database error");
            Failure::Refused(Status::internal("database error"))
        }
    }
}

/// Awaits a database call inside a function that returns `Result<_, Failure>`.
#[macro_export]
macro_rules! db {
    ($call:expr) => {
        $call.await.map_err(|error| {
            $crate::transaction::database_failure(error.code().map(|code| code.code()), error.to_string())
        })?
    };
}

/// Runs `attempt` until it commits, is refused, or has conflicted too often. A conflict means the
/// attempt rolled back, so running it again cannot apply anything twice.
pub async fn retrying<T, A, F>(mut attempt: A) -> Result<T, Status>
where
    A: FnMut() -> F,
    F: Future<Output = Result<T, Failure>>,
{
    for number in 1..=ATTEMPTS {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(Failure::Refused(status)) => return Err(status),
            Err(Failure::Conflict(error)) => {
                warn!(%error, number, "transaction conflict");
                tokio::time::sleep(BACKOFF * number).await;
            }
        }
    }
    Err(Status::aborted("too many transaction conflicts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    #[tokio::test]
    async fn conflicts_run_again_and_refusals_do_not() {
        let runs = Cell::new(0);
        let result = retrying(|| {
            runs.set(runs.get() + 1);
            let run = runs.get();
            async move {
                match run {
                    1 => Err(database_failure(Some("40001"), "restart".into())),
                    _ => Ok(run),
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);

        runs.set(0);
        let result: Result<(), Status> = retrying(|| {
            runs.set(runs.get() + 1);
            async { Err(database_failure(Some("23505"), "duplicate".into())) }
        })
        .await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::Internal);
        assert_eq!(runs.get(), 1);
    }

    #[tokio::test]
    async fn gives_up_on_endless_conflicts() {
        let result: Result<(), Status> =
            retrying(|| async { Err(Failure::Conflict("restart".into())) }).await;
        assert_eq!(result.unwrap_err().code(), tonic::Code::Aborted);
    }
}

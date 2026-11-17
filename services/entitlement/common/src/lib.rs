//! What the economy services share: code, never tables. Each service creates and accesses only its own
//! tables, and asks the owner of any other over gRPC. It lives under services/entitlement because every
//! directory directly in services/ is built into an image, and this crate has no binary.

pub mod command;
pub mod ledger;
pub mod query;
pub mod transaction;

pub use sc_runtime::player_urn;
use std::pin::Pin;

use futures_util::{stream, Stream};
use prost::Message;
use prost_types::Timestamp;
use sc_db::{internal, Row};
use tonic::Status;

/// Rows per message of a server stream. The client appends every message to one result.
const STREAM_BATCH: usize = 500;

pub type ReplyStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

pub fn account_urn(account_id: u32) -> String {
    format!("urn:sc:platform:account:integer:{account_id}")
}

pub fn timestamp(milliseconds: i64) -> Timestamp {
    Timestamp {
        seconds: milliseconds.div_euclid(1000),
        nanos: milliseconds.rem_euclid(1000) as i32 * 1_000_000,
    }
}

/// A column selected as milliseconds since the epoch.
pub fn timestamp_of(row: &Row, column: &str) -> Option<Timestamp> {
    row.get::<_, Option<i64>>(column).map(timestamp)
}

/// A column holding an encoded message.
pub fn decoded<M: Message + Default>(row: &Row, column: &str) -> Result<Option<M>, Status> {
    row.get::<_, Option<Vec<u8>>>(column)
        .map(|bytes| M::decode(bytes.as_slice()).map_err(internal))
        .transpose()
}

/// `rows` as a server stream, wrapped into its reply type in batches.
pub fn reply_stream<T, R>(rows: Vec<T>, wrap: impl Fn(Vec<T>) -> R) -> ReplyStream<R>
where
    T: Send + 'static,
    R: Send + 'static,
{
    let mut rows = rows.into_iter().peekable();
    let mut replies = Vec::new();
    while rows.peek().is_some() {
        replies.push(Ok(wrap(rows.by_ref().take(STREAM_BATCH).collect())));
    }
    Box::pin(stream::iter(replies))
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt;

    #[test]
    fn timestamps_split_milliseconds() {
        assert_eq!(timestamp(1_500), Timestamp { seconds: 1, nanos: 500_000_000 });
        assert_eq!(timestamp(-1), Timestamp { seconds: -1, nanos: 999_000_000 });
    }

    #[tokio::test]
    async fn streams_batch_rows_and_send_nothing_for_none() {
        let replies: Vec<_> = reply_stream((0..1001).collect(), |rows: Vec<i32>| rows.len()).collect().await;
        let sizes: Vec<usize> = replies.into_iter().map(Result::unwrap).collect();
        assert_eq!(sizes, [500, 500, 1]);
        assert_eq!(reply_stream(Vec::<i32>::new(), |rows| rows.len()).count().await, 0);
    }
}

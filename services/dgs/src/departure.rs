//! What a DGS does for a player whose connection ended (see docs/DISCONNECT.md): it hands the entities it held live back to
//! sc-entitygraph, which owns them, by clearing their live markers (`EntityGraphInternal.ReleasePlayer`). The call never
//! holds up the leave: it runs on its own task, tries a few times, and gives up with a warning, the Hub being the backstop
//! (it sends the same event, from its own record, until sc-entitygraph has heard it).
//!
//! The DGS's other state for the session (holdings, movement, call panels, the hangar elevator, the outbox) is dropped by
//! `leave_player` itself, synchronously.

use std::time::Duration;

use sc_api::internal::{entity_graph_internal_client::EntityGraphInternalClient, DisconnectReason, PlayerDisconnect, ReleasePlayerRequest};
use tonic::transport::Channel;
use tracing::{debug, warn};

/// How many times the call is tried, and how long the first wait is. Each wait doubles.
const ATTEMPTS: u32 = 4;
const FIRST_WAIT: Duration = Duration::from_millis(500);

/// The event for a session that ended, as a DGS reports it: the key is the one the replicant used for the same connection,
/// so the Hub's report of it and this one are the same event.
pub fn event(shard_id: &str, replicant: u64, session: u64, account: u64, geid: u64, reason: i32, client_code: u32, at_ms: u64) -> PlayerDisconnect {
    PlayerDisconnect {
        event_id: format!("{replicant}:{session}"),
        reason,
        account_id: u32::try_from(account).unwrap_or_default(),
        geid,
        shard_id: shard_id.to_string(),
        client_code,
        connected_at_ms: 0,
        at_ms,
    }
}

/// Whether a leave with this reason hands anything back. A replaced connection is the same player on a newer one.
pub fn releases(reason: i32) -> bool {
    reason != DisconnectReason::Replaced as i32
}

/// Sends the event on a task of its own and returns at once.
pub fn release(graph: EntityGraphInternalClient<Channel>, event: PlayerDisconnect) {
    tokio::spawn(async move {
        send(event, FIRST_WAIT, |event| {
            let mut graph = graph.clone();
            async move { graph.release_player(ReleasePlayerRequest { event: Some(event) }).await.map(|reply| reply.into_inner().released) }
        })
        .await
    });
}

/// Tries `call` up to `ATTEMPTS` times, waiting longer after each failure. Whether it was heard.
async fn send<F: std::future::Future<Output = Result<u32, tonic::Status>>>(event: PlayerDisconnect, first_wait: Duration, mut call: impl FnMut(PlayerDisconnect) -> F) -> bool {
    let mut wait = first_wait;
    for attempt in 1..=ATTEMPTS {
        match call(event.clone()).await {
            Ok(reply) => {
                debug!(player = event.geid, event = %event.event_id, released = reply, "live markers released");
                return true;
            }
            Err(status) if attempt < ATTEMPTS => {
                debug!(player = event.geid, attempt, %status, "release not heard, trying again");
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            Err(status) => warn!(player = event.geid, event = %event.event_id, %status, "release not heard: the Hub will send it again"),
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use tonic::Status;

    use super::*;

    #[test]
    fn the_event_carries_the_connection_it_ends() {
        let event = event("shard-a", 7, 9, 1_000_001, 300_000_000_001, DisconnectReason::Clean as i32, 30016, 123);
        assert_eq!((event.event_id.as_str(), event.shard_id.as_str(), event.geid, event.client_code, event.at_ms), ("7:9", "shard-a", 300_000_000_001, 30016, 123));
        assert_eq!(event.account_id, 1_000_001);
    }

    #[test]
    fn only_a_replaced_connection_hands_nothing_back() {
        assert!(!releases(DisconnectReason::Replaced as i32));
        for reason in [DisconnectReason::Clean, DisconnectReason::Timeout, DisconnectReason::Kicked, DisconnectReason::ConnectionLost, DisconnectReason::Unspecified] {
            assert!(releases(reason as i32));
        }
    }

    fn sample() -> PlayerDisconnect {
        event("shard-a", 1, 2, 3, 4, DisconnectReason::Timeout as i32, 0, 5)
    }

    #[tokio::test]
    async fn a_release_that_is_not_heard_at_first_is_tried_again() {
        let calls = AtomicU32::new(0);
        let heard = send(sample(), Duration::from_millis(1), |_| async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 { Err(Status::unavailable("restarting")) } else { Ok(1) }
        })
        .await;
        assert!(heard);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_release_nobody_hears_gives_up_after_its_attempts() {
        let calls = AtomicU32::new(0);
        let heard = send(sample(), Duration::from_millis(1), |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Status::unavailable("gone"))
        })
        .await;
        assert!(!heard);
        assert_eq!(calls.load(Ordering::SeqCst), ATTEMPTS);
    }
}

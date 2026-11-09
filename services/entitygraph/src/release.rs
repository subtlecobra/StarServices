//! `ReleasePlayer` of `EntityGraphInternal`: a player left a shard, so what the DGS held live for them is no longer live.
//!
//! An entity is live on a shard when it is unstowed with a `shardId` (the DGS has authority over it, and the admin
//! portal and the item RPCs refuse to change it). The DGS clears that itself when it hands the entities back, but a DGS
//! that died, a leave nobody heard or a Hub that saw the connection go first would leave the marker for ever. This is
//! the call that clears it, from either of them. See docs/DISCONNECT.md.
//!
//! It is idempotent by nature: it only clears markers, so a repeat finds none. A late report of an older connection
//! must not clear a marker a newer one set, so only markers last written at or before the disconnect are cleared.
//! A `REPLACED` connection is the same player on a newer one, and changes nothing.

use sc_api::internal::{DisconnectReason, ReleasePlayerRequest, ReleasePlayerResponse};
use sc_db::{internal, Arango};
use serde_json::json;
use tonic::Status;
use tracing::info;

use crate::now_ms;

/// Markers carry this service's clock and the disconnect the reporter's, so a marker counts as newer than the report only
/// when it is later by more than this.
const CLOCK_SKEW_MS: u64 = 2_000;

const RELEASE: &str = "
FOR e IN entities
    FILTER TO_STRING(e.ownerId) == TO_STRING(@owner) AND e.stowed != true
        AND e.shardId != null AND e.shardId != ''
        AND (@shard == '' OR e.shardId == @shard)
        AND (e.updatedAt == null OR e.updatedAt <= @upto)
    UPDATE e WITH { shardId: null, updatedAt: @now, version: (e.version == null ? 0 : e.version) + 1 } IN entities
    RETURN 1";

pub async fn release(db: &Arango, request: ReleasePlayerRequest) -> Result<ReleasePlayerResponse, Status> {
    let event = request.event.ok_or_else(|| Status::invalid_argument("no event"))?;
    if event.geid == 0 {
        return Err(Status::invalid_argument("the player's GEID is needed"));
    }
    if event.reason() == DisconnectReason::Replaced {
        return Ok(ReleasePlayerResponse { released: 0 });
    }
    let at = if event.at_ms == 0 { now_ms() } else { event.at_ms };
    let bind = json!({ "owner": event.geid.to_string(), "shard": event.shard_id, "upto": at + CLOCK_SKEW_MS, "now": now_ms() });
    let released = db.aql(RELEASE, bind).await.map_err(internal)?.len() as u32;
    info!(player = event.geid, shard = %event.shard_id, event = %event.event_id, reason = ?event.reason(), released, "live markers released");
    Ok(ReleasePlayerResponse { released })
}

#[cfg(test)]
mod tests {
    use sc_api::internal::PlayerDisconnect;

    use super::*;

    fn request(geid: u64, shard: &str, reason: DisconnectReason, at_ms: u64) -> ReleasePlayerRequest {
        let event = PlayerDisconnect {
            event_id: format!("1:{geid}"),
            reason: reason as i32,
            geid,
            shard_id: shard.into(),
            at_ms,
            ..Default::default()
        };
        ReleasePlayerRequest { event: Some(event) }
    }

    fn offline() -> Arango {
        Arango::lazy("http://127.0.0.1:1")
    }

    #[tokio::test]
    async fn a_report_with_no_player_is_refused() {
        let status = release(&offline(), request(0, "s", DisconnectReason::Clean, 1)).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        let status = release(&offline(), ReleasePlayerRequest::default()).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_replaced_connection_releases_nothing_and_asks_nothing() {
        // The database is not there: the answer comes before it is asked.
        let reply = release(&offline(), request(5, "s", DisconnectReason::Replaced, 1)).await.unwrap();
        assert_eq!(reply.released, 0);
    }

    /// `SC_TEST_ARANGO=http://127.0.0.1:18602 cargo test -p sc-entitygraph -- --include-ignored --test-threads=1`.
    #[tokio::test]
    #[ignore]
    async fn live_markers_of_the_player_on_that_shard_are_cleared_once() {
        let db = crate::open(&std::env::var("SC_TEST_ARANGO").expect("SC_TEST_ARANGO")).await.unwrap();
        let player = now_ms() * 1000 + 17;
        let doc = |key: u64, owner: u64, stowed: bool, shard: Option<&str>, updated: u64| {
            json!({ "_key": key.to_string(), "geid": key.to_string(), "ownerId": owner.to_string(), "stowed": stowed,
                    "shardId": shard, "updatedAt": updated, "version": 1 })
        };
        let at = now_ms();
        let docs = vec![
            doc(player + 1, player, false, Some("shard-a"), at - 10_000),  // a ship out on the shard: released
            doc(player + 2, player, false, Some("shard-a"), at - 10_000),  // so is a second one
            doc(player + 3, player, false, Some("shard-b"), at - 10_000),  // on another shard: stays
            doc(player + 4, player, true, None, at - 10_000),              // stowed: not live
            doc(player + 5, player + 99, false, Some("shard-a"), at - 10_000),  // somebody else's: stays
            doc(player + 6, player, false, Some("shard-a"), at + 60_000),  // set after the disconnect: stays
        ];
        db.aql("FOR d IN @docs INSERT d INTO entities OPTIONS { overwriteMode: 'replace' }", json!({ "docs": docs })).await.unwrap();
        let marker = |key: u64| {
            let db = &db;
            async move {
                let rows = db.aql("RETURN DOCUMENT('entities', @key).shardId", json!({ "key": key.to_string() })).await.unwrap();
                rows[0].as_str().map(str::to_string)
            }
        };

        let first = release(&db, request(player, "shard-a", DisconnectReason::Clean, at)).await.unwrap();
        assert_eq!(first.released, 2);
        assert_eq!(marker(player + 1).await, None);
        assert_eq!(marker(player + 2).await, None);
        assert_eq!(marker(player + 3).await.as_deref(), Some("shard-b"));
        assert_eq!(marker(player + 5).await.as_deref(), Some("shard-a"));
        assert_eq!(marker(player + 6).await.as_deref(), Some("shard-a"), "a newer marker is not an older connection's");

        // A repeat, and a late duplicate from the other reporter, find nothing to do.
        assert_eq!(release(&db, request(player, "shard-a", DisconnectReason::Timeout, at)).await.unwrap().released, 0);

        // No shard named: every shard the player was live on.
        assert_eq!(release(&db, request(player, "", DisconnectReason::ConnectionLost, at)).await.unwrap().released, 1);
        assert_eq!(marker(player + 3).await, None);
        db.aql("FOR e IN entities FILTER e._key IN @keys REMOVE e IN entities", json!({ "keys": (1..=6).map(|n| (player + n).to_string()).collect::<Vec<_>>() }))
            .await
            .unwrap();
    }
}

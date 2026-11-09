//! A player's connection to a shard ending (see docs/DISCONNECT.md). The Hub owns sessions and the shard slot
//! (`player_shards`), so it is where every report of a disconnect arrives, from the replicant (`ReportDisconnect`) and from
//! its own connection to the client closing.
//!
//! An event is recorded once per `event_id`, in `player_disconnects`, which also is the queue of what is still to be told to
//! sc-entitygraph: `ReleasePlayer` clears the live markers of what the player's DGS held. The Hub tries at once, and a loop
//! tries again every `RETRY_EVERY` until sc-entitygraph has heard it (or a day has passed), so a missed notification
//! cannot leave a player live for ever. Nothing here is waited for by the caller: the report is answered after the two
//! quick database writes.

use std::time::Duration;

use sc_api::internal::{
    entity_graph_internal_client::EntityGraphInternalClient, DisconnectReason, PlayerDisconnect, ReleasePlayerRequest,
};
use sc_db::Postgres;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS player_disconnects (
    event_id TEXT PRIMARY KEY,
    geid BIGINT NOT NULL,
    account_id BIGINT NOT NULL DEFAULT 0,
    shard_id TEXT NOT NULL DEFAULT '',
    reason INT NOT NULL,
    client_code INT NOT NULL DEFAULT 0,
    connected_at_ms BIGINT NOT NULL DEFAULT 0,
    at_ms BIGINT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    fanned_out BOOLEAN NOT NULL DEFAULT false,
    attempts INT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS player_disconnects_pending ON player_disconnects (fanned_out, received_at);
";

/// How often the events sc-entitygraph has not heard are sent again, and how many are sent at a time.
pub const RETRY_EVERY: Duration = Duration::from_secs(15);
const BATCH: i64 = 100;
/// How long an event is kept: the time in which a report can be repeated and still be known as one, and the time sc-entitygraph
/// has to come back before the Hub gives up on it.
const KEEP: &str = "1 day";
/// A player's slot is freed only when it was written before the connection began, give or take the clocks of the Hub's
/// database and the replicant.
const SKEW_MS: i64 = 2_000;

/// What the Hub needs of a report. Invalid ones are refused before anything is written.
pub fn check(event: &PlayerDisconnect) -> Result<(), String> {
    if event.event_id.is_empty() {
        return Err("an event id is needed".into());
    }
    if event.geid == 0 {
        return Err("the player's GEID is needed".into());
    }
    Ok(())
}

/// Records the event. False for one recorded already, which is then not handled again.
pub async fn record(db: &Postgres, event: &PlayerDisconnect) -> Result<bool, String> {
    let rows = db
        .execute(
            "INSERT INTO player_disconnects (event_id, geid, account_id, shard_id, reason, client_code, connected_at_ms, at_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (event_id) DO NOTHING",
            &[
                &event.event_id,
                &(event.geid as i64),
                &i64::from(event.account_id),
                &event.shard_id,
                &event.reason,
                &(event.client_code as i32),
                &(event.connected_at_ms as i64),
                &(event.at_ms as i64),
            ],
        )
        .await?;
    Ok(rows == 1)
}

/// Frees the player's shard slot, if it is the one that ended: on the shard the event names, and written before the
/// connection began, so that a player who has joined again since keeps their new slot. Whether a slot was freed.
pub async fn free_slot(db: &Postgres, event: &PlayerDisconnect) -> Result<bool, String> {
    let began = if event.connected_at_ms > 0 { event.connected_at_ms } else { event.at_ms } as i64;
    let rows = db
        .execute(
            "DELETE FROM player_shards WHERE geid = $1 AND ($2 = '' OR shard_id = $2)
                 AND joined_at <= to_timestamp(($3::bigint + $4::bigint) / 1000.0)",
            &[&(event.geid as i64), &event.shard_id, &began, &SKEW_MS],
        )
        .await?;
    Ok(rows > 0)
}

/// Tells sc-entitygraph, once, and marks the event told when it answers. Whether it was heard. With no sc-entitygraph
/// there is nobody to tell.
pub async fn fan_out(db: &Postgres, graph: Option<&EntityGraphInternalClient<Channel>>, event: &PlayerDisconnect) -> bool {
    let heard = match graph {
        Some(graph) => match graph.clone().release_player(ReleasePlayerRequest { event: Some(event.clone()) }).await {
            Ok(reply) => {
                debug!(event = %event.event_id, released = reply.into_inner().released, "sc-entitygraph released the player");
                true
            }
            Err(status) => {
                debug!(event = %event.event_id, %status, "sc-entitygraph did not hear the disconnect");
                false
            }
        },
        None => true,
    };
    let marked = if heard { "fanned_out = true, attempts = attempts + 1" } else { "attempts = attempts + 1" };
    if let Err(error) = db.execute(&format!("UPDATE player_disconnects SET {marked} WHERE event_id = $1"), &[&event.event_id]).await {
        warn!(%error, event = %event.event_id, "could not note the fan out");
    }
    heard
}

/// The whole of a report: record it, free the slot, and tell sc-entitygraph on a task of its own. `Ok((recorded, freed))`.
pub async fn handle(db: &Postgres, graph: Option<EntityGraphInternalClient<Channel>>, event: PlayerDisconnect) -> Result<(bool, bool), String> {
    check(&event)?;
    // The same player on a newer connection has not left.
    if event.reason() == DisconnectReason::Replaced {
        return Ok((false, false));
    }
    if !record(db, &event).await? {
        return Ok((false, false));
    }
    let freed = free_slot(db, &event).await?;
    info!(event = %event.event_id, player = event.geid, shard = %event.shard_id, reason = ?event.reason(), freed, "player disconnected");
    let db = db.clone();
    tokio::spawn(async move {
        fan_out(&db, graph.as_ref(), &event).await;
    });
    Ok((true, freed))
}

/// The events not yet told to sc-entitygraph, oldest first.
async fn pending(db: &Postgres) -> Result<Vec<PlayerDisconnect>, String> {
    let rows = db
        .query(
            &format!(
                "SELECT event_id, geid, account_id, shard_id, reason, client_code, connected_at_ms, at_ms FROM player_disconnects
                 WHERE NOT fanned_out AND received_at > now() - interval '{KEEP}' ORDER BY received_at LIMIT {BATCH}"
            ),
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| PlayerDisconnect {
            event_id: row.get(0),
            geid: row.get::<_, i64>(1) as u64,
            account_id: row.get::<_, i64>(2) as u32,
            shard_id: row.get(3),
            reason: row.get(4),
            client_code: row.get::<_, i32>(5) as u32,
            connected_at_ms: row.get::<_, i64>(6) as u64,
            at_ms: row.get::<_, i64>(7) as u64,
        })
        .collect())
}

/// One pass of the retry: what is still to be told, and the old events forgotten. How many were heard.
pub async fn retry_once(db: &Postgres, graph: Option<&EntityGraphInternalClient<Channel>>) -> Result<usize, String> {
    let mut heard = 0;
    for event in pending(db).await? {
        heard += usize::from(fan_out(db, graph, &event).await);
    }
    db.execute(&format!("DELETE FROM player_disconnects WHERE received_at < now() - interval '{KEEP}'"), &[]).await?;
    Ok(heard)
}

/// Retries for as long as the Hub runs.
pub async fn retry_loop(db: Postgres, graph: Option<EntityGraphInternalClient<Channel>>) {
    loop {
        tokio::time::sleep(RETRY_EVERY).await;
        match retry_once(&db, graph.as_ref()).await {
            Ok(0) => {}
            Ok(heard) => info!(heard, "disconnects told to sc-entitygraph again"),
            Err(error) => warn!(%error, "could not retry the disconnects"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, geid: u64, shard: &str, reason: DisconnectReason, connected_at_ms: u64, at_ms: u64) -> PlayerDisconnect {
        PlayerDisconnect {
            event_id: id.into(),
            reason: reason as i32,
            account_id: 7,
            geid,
            shard_id: shard.into(),
            client_code: 30016,
            connected_at_ms,
            at_ms,
        }
    }

    #[test]
    fn a_report_needs_its_key_and_its_player() {
        assert!(check(&event("1:2", 9, "s", DisconnectReason::Clean, 0, 1)).is_ok());
        assert!(check(&event("", 9, "s", DisconnectReason::Clean, 0, 1)).is_err());
        assert!(check(&event("1:2", 0, "s", DisconnectReason::Clean, 0, 1)).is_err());
    }

    /// Against a real PostgreSQL or YugabyteDB, in a private schema of the server in `SCBACKEND_TEST_POSTGRES`. Without it the
    /// test does nothing, so `cargo test` stays offline.
    #[tokio::test]
    async fn a_disconnect_is_recorded_once_frees_the_slot_of_that_connection_and_is_told_again_until_heard() {
        let Ok(base) = std::env::var("SCBACKEND_TEST_POSTGRES") else { return };
        let schema = format!("hub_disconnect_test_{}", std::process::id());
        let admin = Postgres::connect(&base, &format!("CREATE SCHEMA {schema}")).await.unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let db = Postgres::connect(&format!("{base}{separator}options=-c%20search_path%3D{schema}"), &crate::schema()).await.unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let slot = |geid: i64, shard: &'static str, age_ms: i64| {
            let db = db.clone();
            async move {
                db.execute("DELETE FROM player_shards WHERE geid = $1", &[&geid]).await.unwrap();
                db.execute(
                    "INSERT INTO player_shards (geid, shard_id, public_address, public_port, location_id, joined_at)
                     VALUES ($1, $2, '10.0.0.1', 7000, 5, now() - ($3::bigint * interval '1 millisecond'))",
                    &[&geid, &shard, &age_ms],
                )
                .await
                .unwrap();
            }
        };
        let slots = |geid: i64| {
            let db = db.clone();
            async move { db.query("SELECT 1 FROM player_shards WHERE geid = $1", &[&geid]).await.unwrap().len() }
        };

        // A clean leave frees the slot of its connection, once.
        slot(1, "shard-a", 60_000).await;
        let clean = event("10:1", 1, "shard-a", DisconnectReason::Clean, now - 30_000, now);
        assert_eq!(handle(&db, None, clean.clone()).await.unwrap(), (true, true));
        assert_eq!(slots(1).await, 0);
        // The same report again, or the Hub's own copy of it under another key, changes nothing and is not recorded twice.
        assert_eq!(handle(&db, None, clean).await.unwrap(), (false, false));

        // A late report of an older connection keeps the slot of a join made since.
        slot(2, "shard-a", 1_000).await;
        let late = event("10:2", 2, "shard-a", DisconnectReason::Timeout, now - 30_000, now);
        assert_eq!(handle(&db, None, late).await.unwrap(), (true, false));
        assert_eq!(slots(2).await, 1);

        // A slot on another shard is not the connection's.
        slot(3, "shard-b", 60_000).await;
        assert_eq!(handle(&db, None, event("10:3", 3, "shard-a", DisconnectReason::Clean, now - 30_000, now)).await.unwrap(), (true, false));
        assert_eq!(slots(3).await, 1);

        // A replaced connection is no one's leave, and nothing is recorded for it.
        slot(4, "shard-a", 60_000).await;
        assert_eq!(handle(&db, None, event("10:4", 4, "shard-a", DisconnectReason::Replaced, 0, now)).await.unwrap(), (false, false));
        assert_eq!(slots(4).await, 1);
        assert!(handle(&db, None, event("10:5", 0, "shard-a", DisconnectReason::Clean, 0, now)).await.is_err());

        // Told to a sc-entitygraph that does not answer, the event stays to be told again; once it is there, it is.
        let endpoint = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_timeout(Duration::from_millis(200));
        let away = EntityGraphInternalClient::new(endpoint.connect_lazy());
        let lost = event("10:6", 6, "shard-a", DisconnectReason::Timeout, 0, now);
        assert!(record(&db, &lost).await.unwrap());
        assert!(!fan_out(&db, Some(&away), &lost).await);
        let waiting = |db: Postgres| async move { db.query("SELECT attempts FROM player_disconnects WHERE NOT fanned_out", &[]).await.unwrap() };
        assert_eq!(waiting(db.clone()).await.len(), 1, "10:6 waits (10:1..10:3 were told to nobody, which is hearing)");
        assert_eq!(waiting(db.clone()).await[0].get::<_, i32>(0), 1);
        assert_eq!(retry_once(&db, Some(&away)).await.unwrap(), 0);
        assert_eq!(waiting(db.clone()).await[0].get::<_, i32>(0), 2);
        assert_eq!(retry_once(&db, None).await.unwrap(), 1);
        assert!(waiting(db.clone()).await.is_empty());

        admin.execute(&format!("DROP SCHEMA {schema} CASCADE"), &[]).await.unwrap();
    }
}

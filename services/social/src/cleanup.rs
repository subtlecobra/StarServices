//! The end of a group in the services that keep rows for it: sc-chat, sc-squad, sc-custom-lobby and
//! sc-rtc own those rows, so sc-social asks each of them through `GroupDataInternal`. The change that
//! destroys a group records one row per service in `social_group_cleanups`, in the same transaction,
//! and a row goes once its service has answered. A service that is away is asked again later, so no
//! group's rows outlive it for good, whatever restarts in between. Asking twice is harmless.
//!
//! A service that writes a row for a group asks sc-social afterwards whether the group still exists,
//! and deletes the row itself if not: a row written after the cleanup ran is then not left behind.
//!
//! Part of the sc-social binary.

use std::{collections::BTreeMap, time::Duration};

use sc_social::{tx::Queries, Context};
use tonic::Status;
use tracing::{info, warn};

/// How often rows that a service did not take are offered again.
const RETRY: Duration = Duration::from_secs(30);
const BATCH: i64 = 500;

/// Asks each service to delete the rows of the groups recorded for it, and forgets those it took.
/// Returns how many recorded rows are left.
pub async fn drain(ctx: &Context) -> Result<usize, Status> {
    let rows = ctx
        .query(
            "SELECT group_id, target FROM social_group_cleanups ORDER BY created_at, group_id LIMIT $1",
            &[&BATCH],
        )
        .await?;
    let mut by_target: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        by_target.entry(row.get("target")).or_default().push(row.get("group_id"));
    }
    let mut left = 0;
    for (target, ids) in by_target {
        match ctx.peers.delete_group_data(&target, &ids).await {
            Ok(deleted) => {
                ctx.execute(
                    "DELETE FROM social_group_cleanups WHERE target = $1 AND group_id = ANY($2)",
                    &[&target, &ids],
                )
                .await?;
                info!(%target, groups = ids.len(), deleted, "group rows deleted");
            }
            Err(error) => {
                warn!(%target, groups = ids.len(), %error, "group rows not deleted yet");
                left += ids.len();
            }
        }
    }
    Ok(left)
}

/// Runs for as long as the service: after each change that destroyed a group, and every [`RETRY`].
pub async fn run(ctx: &Context) {
    loop {
        if let Err(error) = drain(ctx).await {
            warn!(%error, "group cleanup failed");
        }
        tokio::select! {
            () = ctx.committed.notified() => {}
            () = tokio::time::sleep(RETRY) => {}
        }
    }
}

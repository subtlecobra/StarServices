//! A client that leaves: the shard answers the notice, stops sending, and (when the DGS can be asked) the DGS lets the player
//! go. A client that is killed sends nothing and is dropped after a minute of silence, which this does not wait for
//! (docs/DISCONNECT.md).

use std::time::Duration;

use sc_api::internal::{dgs_control_client::DgsControlClient, DgsStatusRequest};

use crate::scenario::{Ctx, Done};

/// What a client sends on `disconnect light ExitToMenu`: the user's quit reason code, 30016.
const REASON_EXIT_TO_MENU: u32 = 0x7540;

async fn players(url: &str) -> Result<u32, String> {
    let mut client = DgsControlClient::connect(url.to_string()).await.map_err(|e| format!("dgs {url}: {e}"))?;
    Ok(client.status(DgsStatusRequest {}).await.map_err(|e| format!("dgs status: {e}"))?.into_inner().players)
}

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let before = match &ctx.opts.dgs_url {
        Some(url) => Some(players(url).await?),
        None => None,
    };
    if let Some(before) = before {
        ctx.fact("dgs_players_before", before);
    }

    let acked = session.leave(REASON_EXIT_TO_MENU, "DisconnectCmd: disconnect light ExitToMenu", Duration::from_secs(5)).await?;
    ctx.check("the notice is answered with a disconnect ack (type 6)", acked, "");
    // The client resends its notice until it is answered; the shard answers every copy for a while.
    let again = session.leave_again(REASON_EXIT_TO_MENU, "DisconnectCmd: disconnect light ExitToMenu", Duration::from_secs(5)).await?;
    ctx.check("a repeated notice is answered again", again, "");

    // Nothing more is sent to a connection that has ended.
    let (events, snapshots) = (session.events.len(), session.snapshot_updates);
    session.pump(Duration::from_secs(3)).await?;
    ctx.check("the shard sends nothing more to the client", (session.events.len(), session.snapshot_updates) == (events, snapshots), "3 s after the ack");

    // The Hub connection ends with the game session.
    session.close_hub();

    if let (Some(url), Some(before)) = (ctx.opts.dgs_url.clone(), before) {
        let mut now = players(&url).await?;
        for _ in 0..30 {
            if now < before {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            now = players(&url).await?;
        }
        ctx.check("the DGS no longer holds the player", now < before, format!("{before} players before, {now} after"));
    } else {
        ctx.fact("dgs_cleanup", "not checked: --dgs-url is not set");
    }
    Ok(())
}

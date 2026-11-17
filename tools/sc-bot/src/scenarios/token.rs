//! The access token of an inventory (messages 21 and 23): the DGS grants it for an item the player wears or owns, denies it
//! for another player's, and leaves the arbitrated tokens of a binding entity unanswered.

use std::time::Duration;

use crate::{
    scenario::{Ctx, Done},
    wire,
    world::EventKind,
};

/// The container component's `m_userToken` is index 2 in the serialized components of an item with an inventory; its request
/// carries the flags 0xcf (docs/INVENTORY.md, "Inventory access tokens").
const USER_TOKEN: u32 = 2;
const USER_FLAGS: u8 = 0xcf;
const ARBITRATED_FLAGS: u8 = 0xce;
const GRANTED: u8 = 1;
const DENIED: u8 = 0;

fn decision(session: &crate::session::Session, mark: usize, entity: u64, request: u32) -> Option<u8> {
    session.events_since(mark).iter().find_map(|event| match &event.kind {
        EventKind::Token { entity: e, request: r, decision, .. } if *e == entity && *r == request => Some(*decision),
        _ => None,
    })
}

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let wait = ctx.opts.reply_timeout;
    let worn = session.world.worn();
    let Some(item) = worn.first().map(|bind| bind.entity_id) else { return Err(crate::scenario::Stop::Skip("the player wears nothing to ask a token for".into())) };
    ctx.fact("item", item);

    // A worn item: the user token is granted, and the event echoes the client's own token and request.
    let mark = session.mark();
    session.send(&wire::token_request(item, USER_TOKEN, 1, USER_FLAGS)).await?;
    let answered = session.run_until(wait, |s| decision(s, mark, item, 1).is_some()).await?;
    ctx.check("the user token of a worn item is answered", answered, "message 23");
    ctx.check("and granted", decision(&session, mark, item, 1) == Some(GRANTED), format!("{:?}", decision(&session, mark, item, 1)));
    let echoed = session.events_since(mark).iter().any(|e| matches!(e.kind, EventKind::Token { entity, token, request, .. } if entity == item && token == USER_TOKEN && request == 1));
    ctx.check("the answer echoes the token and request", echoed, "token 2, request 1");

    // The arbitrated token a client asks for as an entity binds is left as it was: no answer.
    let mark = session.mark();
    session.send(&wire::token_request(item, 0, 3, ARBITRATED_FLAGS)).await?;
    session.pump(Duration::from_millis(1500)).await?;
    ctx.check("an arbitrated token (0xce) gets no answer", decision(&session, mark, item, 3).is_none(), "left as before");

    // Another player's item is denied.
    match ctx.opts.foreign_item {
        Some(foreign) => {
            let mark = session.mark();
            session.send(&wire::token_request(foreign, USER_TOKEN, 5, USER_FLAGS)).await?;
            session.run_until(wait, |s| decision(s, mark, foreign, 5).is_some()).await?;
            ctx.check("the user token of another player's item is denied", decision(&session, mark, foreign, 5) == Some(DENIED), format!("{:?}", decision(&session, mark, foreign, 5)));
        }
        None => ctx.fact("denied_case", "skipped: --foreign-item is not set"),
    }
    Ok(())
}

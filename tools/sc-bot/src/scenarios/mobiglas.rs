//! The mobiGlas request (action 0x56): the replicant answers with the additive state (index 411) and the mobiGlas state
//! (index 471) of the player's variables, in one update with the frame's timestamp variables, and sends none of them before
//! a request.

use std::time::Duration;

use super::{send_frame, PLAYER_VARIABLES};
use crate::{
    requests::{Action, MOBIGLAS_CLOSE_REQUEST, MOBIGLAS_OPEN_REQUEST},
    scenario::{Ctx, Done, Stop},
    session::Session,
};

/// `m_additiveState.m_baseData`: 9 bytes, the state byte (0x13 while the mobiGlas is open) and 8 time bytes.
const ADDITIVE_OPEN: u8 = 0x13;
const ADDITIVE_CLOSED: u8 = 0;
const ADDITIVE_TIME_LEN: usize = 8;
/// The mobiGlas state value for open.
const STATE_OPEN: u8 = 2;
const STATE_CLOSED: u8 = 0;
/// A moment to wait to be sure that nothing comes.
const QUIET: Duration = Duration::from_millis(1500);

fn additive(state: u8) -> Vec<u8> {
    let mut value = vec![state];
    value.extend([0; ADDITIVE_TIME_LEN]);
    value
}

/// Sends the request and waits for the mobiGlas state variable; checks the variables that came.
async fn request(ctx: &mut Ctx, session: &mut Session, player: u64, tick: u64, what: &str, kind: u8, additive_value: Vec<u8>, state: u8) -> Result<(), Stop> {
    let mark = session.mark();
    send_frame(session, player, tick, &[Action::mobiglas(kind)]).await?;
    let arrived = session.run_until(ctx.opts.reply_timeout, |s| s.variable_since(mark, player, PLAYER_VARIABLES.mobiglas_state).is_some()).await?;
    ctx.check(&format!("{what}: the mobiGlas state arrives"), arrived, format!("{:?}", session.variables_since(mark, player)));
    session.pump(Duration::from_millis(300)).await?;
    ctx.check(&format!("{what}: the additive state is {additive_value:02x?}"), session.variable_since(mark, player, PLAYER_VARIABLES.additive_state) == Some(additive_value), "index 411");
    ctx.check(&format!("{what}: the mobiGlas state is {state}"), session.variable_since(mark, player, PLAYER_VARIABLES.mobiglas_state) == Some(vec![state]), "index 471");
    // The state and the frame's timestamps share an update.
    let variables = session.variables_since(mark, player);
    let update = variables.iter().find(|(_, index, _)| *index == PLAYER_VARIABLES.mobiglas_state).map(|(update, _, _)| *update);
    let same = |index: usize| variables.iter().any(|(u, i, _)| Some(*u) == update && *i == index);
    ctx.check(&format!("{what}: the timestamp variables come in the same update"), same(PLAYER_VARIABLES.frame_end_base) && same(PLAYER_VARIABLES.frame_end_offset), "indexes 996 and 908");
    let base = variables.iter().find(|(_, index, _)| *index == PLAYER_VARIABLES.frame_end_base).map(|(_, _, value)| value.len());
    ctx.check(&format!("{what}: the timestamp base is 8 bytes"), base == Some(8), format!("{base:?}"));
    Ok(())
}

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let player = session.world.player().map(|bind| bind.entity_id).ok_or_else(|| Stop::Error("no player entity".to_string()))?;

    // Without a request, a frame (or none) changes nothing of it.
    let mark = session.mark();
    send_frame(&mut session, player, 1, &[]).await?;
    send_frame(&mut session, player, 2, &[]).await?;
    session.pump(QUIET).await?;
    let early = session.variables_since(mark, player).into_iter().filter(|(_, index, _)| [PLAYER_VARIABLES.additive_state, PLAYER_VARIABLES.mobiglas_state].contains(index)).count();
    ctx.check("no mobiGlas variable is sent before a request", early == 0, format!("{early} seen"));

    request(ctx, &mut session, player, 3, "open", MOBIGLAS_OPEN_REQUEST, additive(ADDITIVE_OPEN), STATE_OPEN).await?;
    request(ctx, &mut session, player, 4, "close", MOBIGLAS_CLOSE_REQUEST, additive(ADDITIVE_CLOSED), STATE_CLOSED).await?;

    // Idle frames after it add nothing.
    let mark = session.mark();
    send_frame(&mut session, player, 5, &[]).await?;
    session.pump(QUIET).await?;
    let after = session.variables_since(mark, player).into_iter().filter(|(_, index, _)| [PLAYER_VARIABLES.additive_state, PLAYER_VARIABLES.mobiglas_state].contains(index)).count();
    ctx.check("an idle frame afterwards sends no mobiGlas variable", after == 0, format!("{after} seen"));
    Ok(())
}

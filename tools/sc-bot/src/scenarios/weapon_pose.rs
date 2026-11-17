//! The weapon pose (actions 0x76 `SUnholster` and 0x4d `SHolster`): the replicant replicates the pose state (index 164, the
//! pose byte then the 8-byte end time of the frame) of the player's variables.

use super::{send_frame, PLAYER_VARIABLES};
use crate::{
    requests::{Action, SAMPLE_HOLSTER_OWNER},
    scenario::{Ctx, Done, Stop},
};

/// `m_poseState.m_baseData`: the pose byte, then the frame's end time.
const POSE_WEAPON: u8 = 2;
const POSE_NO_WEAPON: u8 = 1;
const POSE_LEN: usize = 9;

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let player = session.world.player().map(|bind| bind.entity_id).ok_or_else(|| Stop::Error("no player entity".to_string()))?;

    let mark = session.mark();
    send_frame(&mut session, player, 1, &[]).await?;
    session.pump(std::time::Duration::from_millis(1000)).await?;
    ctx.check("no pose is sent before a request", session.variable_since(mark, player, PLAYER_VARIABLES.pose_state).is_none(), "");

    for (what, tick, action, pose) in [
        ("unholster", 2, Action::unholster(), POSE_WEAPON),
        ("holster", 3, Action::holster(SAMPLE_HOLSTER_OWNER), POSE_NO_WEAPON),
    ] {
        let mark = session.mark();
        send_frame(&mut session, player, tick, &[action]).await?;
        let arrived = session.run_until(ctx.opts.reply_timeout, |s| s.variable_since(mark, player, PLAYER_VARIABLES.pose_state).is_some()).await?;
        ctx.check(&format!("{what}: the pose state arrives"), arrived, format!("{:?}", session.variables_since(mark, player)));
        let value = session.variable_since(mark, player, PLAYER_VARIABLES.pose_state).unwrap_or_default();
        ctx.check(&format!("{what}: the pose is {pose}"), value.first() == Some(&pose), format!("{value:02x?}"));
        ctx.check(&format!("{what}: the value is the pose and an 8-byte time"), value.len() == POSE_LEN, format!("{} bytes", value.len()));
        let update = session.variables_since(mark, player).iter().find(|(_, index, _)| *index == PLAYER_VARIABLES.pose_state).map(|(update, _, _)| *update);
        let stamped = session.variables_since(mark, player).iter().any(|(u, i, _)| Some(*u) == update && *i == PLAYER_VARIABLES.frame_end_base);
        ctx.check(&format!("{what}: the frame time comes in the same update"), stamped, "index 996");
    }
    Ok(())
}

//! A press of the hangar elevator's call panel. The panel is an entity the DGS binds; its `SvRequestInteract` (method 10) goes
//! to the DGS, which answers `ClOnInteract` (method 11) with the request's arguments and sends the line a call: a carriage
//! comes, and binds (and rebinds, for its doors) follow. The layout of the arguments is from one real press
//! (`services/dgs/src/interaction.rs`); the interaction point is the captured panel's, which the DGS does not check for a call.

use std::time::Duration;

use crate::{
    requests::{self, CALL_ELEVATOR, CAPTURED_POINT, PANEL_ON_INTERACT, PANEL_REQUEST_INTERACT},
    scenario::{Ctx, Done, Stop},
    world::{EventKind, CALL_PANEL_CLASS},
};

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let player = session.world.player().map(|bind| bind.entity_id).ok_or_else(|| Stop::Error("no player entity".to_string()))?;
    let Some(panel) = session.world.of_class(CALL_PANEL_CLASS).first().map(|bind| bind.entity_id) else {
        return Err(Stop::Skip("the join bound no call panel (is the player's home a hangar with an elevator?)".into()));
    };
    ctx.fact("panel", panel);

    // The press.
    let args = requests::interaction(1, CAPTURED_POINT, CALL_ELEVATOR, player, panel);
    let mark = session.mark();
    session.call(panel, PANEL_REQUEST_INTERACT, &args).await?;
    let answered = session.run_until(ctx.opts.reply_timeout, |s| s.method_since(mark, panel, PANEL_ON_INTERACT).is_some()).await?;
    ctx.check("the press is answered with ClOnInteract (method 11)", answered, format!("panel {panel}"));
    if answered {
        let reply = session.method_since(mark, panel, PANEL_ON_INTERACT).unwrap_or_default();
        ctx.check("the answer carries the request's own arguments", reply == args, format!("{} bytes", reply.len()));
    }

    // An interaction the panel does not take gets no answer: the client waits for none.
    let other = requests::interaction(2, CAPTURED_POINT, CALL_ELEVATOR ^ 1, player, panel);
    let mark_other = session.mark();
    session.call(panel, PANEL_REQUEST_INTERACT, &other).await?;
    session.pump(Duration::from_millis(1500)).await?;
    ctx.check("another interaction on the panel gets no answer", session.method_since(mark_other, panel, PANEL_ON_INTERACT).is_none(), "");

    // The line's response: a carriage is bound to this client, and its doors change.
    if ctx.opts.elevator_wait.is_zero() {
        ctx.fact("carriage", "not waited for (--elevator-wait 0)");
        return Ok(());
    }
    let arrived = session.run_until(ctx.opts.elevator_wait, |s| s.events_since(mark).iter().any(|e| matches!(e.kind, EventKind::Bind { .. }))).await?;
    let binds: Vec<_> = session.events_since(mark).iter().filter_map(|e| match e.kind {
        EventKind::Bind { entity, class_crc, .. } => Some((entity, class_crc, e.at_ms)),
        _ => None,
    }).collect();
    ctx.check("the elevator line answers the call with binds (a carriage, its doors)", arrived, format!("{} binds within {:?}", binds.len(), ctx.opts.elevator_wait));
    if let Some((entity, class_crc, at)) = binds.first() {
        ctx.fact("first_bind_after_press", format!("entity {entity} class {class_crc} at +{}ms", at.saturating_sub(session.events_since(mark).first().map_or(0, |e| e.at_ms))));
    }
    Ok(())
}

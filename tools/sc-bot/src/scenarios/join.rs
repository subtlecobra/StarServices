//! The join: what the server must have sent by the time the client is in game.

use crate::{
    scenario::{Ctx, Done},
    wire::hex,
    world::*,
};

pub async fn run(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    ctx.check("reached eCVS_InGame", session.server_state >= 17, format!("server state {}", session.server_state));

    let shard = session.shard.clone();
    ctx.check("ShardId received", shard.is_some(), format!("{shard:?}"));
    if let Some((id, _)) = &shard {
        ctx.fact("shard", id);
    }
    let context = session.context.clone();
    ctx.check("GameContextInfo names the map and mode", context == Some(("megamap".into(), "SC_Default".into())), format!("{context:?}"));
    ctx.check("the update bubble was created", session.events.iter().any(|e| e.kind == EventKind::Bubble), "message 42");
    ctx.check("the DGS named itself (DebugAuthorityChangeInfo)", session.server_name.is_some(), format!("{:?}", session.server_name));

    // The universe.
    let root = session.world.get(UNIVERSE_ROOT_ID).cloned();
    ctx.check("the universe root is bound as GameRulesSingleton", root.as_ref().is_some_and(|bind| bind.class_crc == GAME_RULES_CLASS && bind.parent_id == 0), format!("{:?}", root.map(|b| b.class_crc)));
    let systems = session.world.of_class(SOLAR_SYSTEM_CLASS);
    ctx.check("solar systems are bound under the root", !systems.is_empty() && systems.iter().all(|s| s.parent_id == UNIVERSE_ROOT_ID), format!("{} bound", systems.len()));

    // Binds come parents first, as the client requires.
    let mut orphan = None;
    let mut batch_members: Vec<(u32, u64, u64)> = Vec::new();
    for event in &session.events {
        if let EventKind::Bind { entity, parent, batch, .. } = &event.kind {
            batch_members.push((*batch, *entity, *parent));
        }
    }
    for (index, (batch, entity, parent)) in batch_members.iter().enumerate() {
        let known = *parent == 0 || batch_members[..index].iter().any(|(_, id, _)| id == parent) || batch_members.iter().any(|(b, id, _)| id == parent && b == batch);
        if !known {
            orphan = Some((*entity, *parent));
            break;
        }
    }
    ctx.check("every bind's parent was bound before it or in its batch", orphan.is_none(), format!("{orphan:?}"));

    // The player.
    let player = session.world.player().cloned();
    ctx.check("the player is bound", player.is_some(), format!("{} entities bound", session.world.len()));
    let Some(player) = player else { return Ok(()) };
    ctx.fact("player", player.entity_id);
    if let Some(login) = &session.login {
        ctx.fact("account", &login.handle);
        ctx.fact("login_geid", login.geid);
    }
    ctx.check("the player has a parent that is bound", session.world.get(player.parent_id).is_some(), format!("parent {}", player.parent_id));
    let worn = session.world.worn();
    ctx.check("the player wears gear (bound below it)", !worn.is_empty(), format!("{} items", worn.len()));
    ctx.check("worn items hang by a port key", worn.iter().all(|item| item.parent_attach_id != 0), "parent_attach_id set");
    ctx.fact("worn_items", worn.len());

    // The hangar's content: bound for a home whose hangar holds the ASOP kiosks' container. Checked whenever it is there, and
    // required with `--expect-atc`.
    let atcs = session.world.of_class(ATC_CLASS);
    let panels = session.world.of_class(CALL_PANEL_CLASS);
    if ctx.opts.expect_atc {
        ctx.check("an ATC is bound", atcs.len() == 1, format!("{} bound", atcs.len()));
        ctx.check("the elevator call panels are bound", !panels.is_empty(), format!("{} bound", panels.len()));
    } else if atcs.is_empty() {
        ctx.fact("atc", "none bound (the hangar has no kiosk container; --expect-atc requires one)");
    }
    ctx.fact("call_panels", panels.len());
    if let Some(atc) = atcs.first() {
        let hangar = atc.parent_id;
        ctx.check("the ATC hangs from the hangar", session.world.get(hangar).is_some(), format!("hangar {hangar}"));
        ctx.check("the player stands in the hangar", session.world.descends_from(player.entity_id, hangar), "player below the hangar");
        let kiosks = session.world.of_class(KIOSK_CLASS);
        ctx.check("kiosks are bound", !kiosks.is_empty(), format!("{} bound", kiosks.len()));
        let linked = kiosks.iter().filter(|k| k.snapshot(OWNER_ATC_KEY).is_some_and(|s| s.value == atc.entity_id.to_le_bytes())).count();
        ctx.check("every kiosk is linked to the ATC (OwnerATCId)", linked == kiosks.len() && linked > 0, format!("{linked}/{} link {}", kiosks.len(), hex(&atc.entity_id.to_le_bytes())));
        ctx.check("the call panels are bound in the hangar", panels.iter().all(|p| session.world.descends_from(p.entity_id, hangar)), format!("{} bound", panels.len()));
    }

    // The clock the pong carries is the server's wall time in 100 ns ticks since 1970.
    let clock = session.ping(std::time::Duration::from_secs(5)).await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64 / 100;
    ctx.check("a ping gets a pong with the server's clock", clock.is_some_and(|c| c.abs_diff(now) < 6_000_000_000), format!("{clock:?}"));
    ctx.fact("entities", session.world.len());
    ctx.fact("snapshot_updates", session.snapshot_updates);
    Ok(())
}

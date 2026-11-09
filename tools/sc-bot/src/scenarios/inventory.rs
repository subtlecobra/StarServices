//! Inventory requests: the argument layout is `services/dgs/src/inventory.rs`, the semantics `docs/INVENTORY.md`. The client
//! sends one request at a time and waits for `pi.RmTokenMoveRequestCompleted` (the process method plus 10); so does the bot.
//! Only the equip (Interaction) and take-off (Store) layouts were seen from a real client; the other kinds use the same
//! decoded common fields as the DGS reads them, which is inferred (docs/BOT.md).

use crate::{
    requests::{self, kind, Detail, InventoryId, COMPLETED_AFTER_PROCESS},
    scenario::{Ctx, Done, Stop},
    session::Session,
    wire::port_crc,
    world::EventKind,
};

/// The inventory sort key of the real Store request; any string the client's ranking made would do.
const RANK: &str = "amslxylwbxiyn";

pub struct Completion {
    pub result: bool,
    pub item: u64,
    /// The event index from which the request's consequences (binds, unbinds) are read.
    pub mark: usize,
}

/// Sends one request on the player and waits for its completion, then lets the binds and unbinds it caused arrive.
async fn request(ctx: &Ctx, session: &mut Session, id: u32, request_kind: u8, source: InventoryId, target: InventoryId, detail: Detail) -> Result<Option<Completion>, Stop> {
    let player = session.world.player().map(|bind| bind.entity_id).ok_or_else(|| Stop::Error("no player entity to send requests as".to_string()))?;
    let answer_method = ctx.opts.process_method + COMPLETED_AFTER_PROCESS;
    let args = requests::inventory_request(id, request_kind, player, source, target, &detail);
    let mark = session.mark();
    session.call(player, ctx.opts.process_method, &args).await?;
    let found = session
        .run_until(ctx.opts.reply_timeout, |s| s.method_since(mark, player, answer_method).and_then(|args| requests::parse_completed(&args)).is_some_and(|(_, request, _)| request == id))
        .await?;
    if !found {
        return Ok(None);
    }
    session.settle(std::time::Duration::from_millis(600), std::time::Duration::from_secs(5)).await?;
    let (result, _, item) = session.method_since(mark, player, answer_method).and_then(|args| requests::parse_completed(&args)).unwrap_or_default();
    Ok(Some(Completion { result, item, mark }))
}

fn last_port(path: &str) -> &str {
    path.rsplit(':').next().unwrap_or(path)
}

/// The bank of the item the scenarios use, and the player.
fn setup(ctx: &Ctx, session: &Session) -> Result<(u64, InventoryId), Stop> {
    let location = ctx.require(ctx.opts.bank_location, "--bank-location (the item bank's location id)")?;
    let player = session.world.player().map(|bind| bind.entity_id).ok_or_else(|| Stop::Error("no player entity".to_string()))?;
    Ok((player, InventoryId::bank(player, location)))
}

/// Takes a worn item of `class` off into `bank`, when the player wears one, so that a scenario starts from it stowed.
async fn ensure_stowed(ctx: &mut Ctx, session: &mut Session, id: &mut u32, class: u32, bank: InventoryId) -> Result<(), Stop> {
    let player = session.world.player().map(|bind| bind.entity_id).unwrap_or_default();
    let worn = session.world.worn().into_iter().find(|bind| bind.class_crc == class).map(|bind| bind.entity_id);
    if let Some(entity) = worn {
        *id += 1;
        let detail = Detail { target_rank: RANK.into(), item: entity, ..Detail::default() };
        let done = request(ctx, session, *id, kind::STORE, InventoryId::NONE, bank, detail).await?;
        ctx.check("the worn item the scenario needs stowed is taken off first", done.as_ref().is_some_and(|d| d.result), format!("{} worn by {player}", entity));
    }
    Ok(())
}

/// Takes off what is worn in the port at the end of `path`, so that another item can go in it. Returns the class it took off.
async fn clear_port(ctx: &mut Ctx, session: &mut Session, id: &mut u32, path: &str, bank: InventoryId) -> Result<Option<u32>, Stop> {
    let key = port_crc(last_port(path));
    let Some((entity, class)) = session.world.worn().into_iter().find(|bind| bind.parent_attach_id == key).map(|bind| (bind.entity_id, bind.class_crc)) else { return Ok(None) };
    *id += 1;
    let detail = Detail { target_rank: RANK.into(), item: entity, ..Detail::default() };
    let done = request(ctx, session, *id, kind::STORE, InventoryId::NONE, bank, detail).await?;
    ctx.check("the item in the target port is taken off first", done.as_ref().is_some_and(|d| d.result), format!("class {class:#x} ({entity})"));
    Ok(Some(class))
}

pub async fn equip(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let (player, bank) = setup(ctx, &session)?;
    let (class, path) = (ctx.opts.helmet_class, ctx.opts.helmet_port.clone());
    let port = port_crc(last_port(&path));
    let mut id = 0;
    ensure_stowed(ctx, &mut session, &mut id, class, bank).await?;
    let displaced = clear_port(ctx, &mut session, &mut id, &path, bank).await?;

    // Equip from the bank: the client names the item by class and inventory, never by entity.
    id += 1;
    let detail = Detail { class_id: class, amount: 1, unstow_root: player, port_path: path.clone(), ..Detail::default() };
    let Some(done) = request(ctx, &mut session, id, kind::INTERACTION, bank, InventoryId::NONE, detail).await? else {
        ctx.check("the equip request is answered (RmTokenMoveRequestCompleted)", false, format!("method {}, no answer in {:?}", ctx.opts.process_method + COMPLETED_AFTER_PROCESS, ctx.opts.reply_timeout));
        return Ok(());
    };
    ctx.check("the equip request is answered", true, format!("request {id}"));
    ctx.check("the equip succeeded (result bit)", done.result, format!("result {}", done.result));
    ctx.check("the answer names the item", done.item != 0, format!("item {}", done.item));
    let bound = session.events_since(done.mark).iter().find_map(|event| match event.kind {
        EventKind::Bind { entity, class_crc, attach, parent, .. } if class_crc == class => Some((entity, attach, parent)),
        _ => None,
    });
    ctx.check("the item is bound to the client", bound.is_some(), format!("{bound:?}"));
    let Some((entity, attach, parent)) = bound else { return Ok(()) };
    ctx.check("it hangs from the port's key", attach == port, format!("{attach:#x} vs {port:#x} ({})", last_port(&path)));
    ctx.check("under a worn item", session.world.worn().iter().any(|bind| bind.entity_id == parent), format!("parent {parent}"));
    ctx.check("the answer's item is the bound entity", done.item == entity, format!("{} vs {entity}", done.item));

    // Take it off into the bank again.
    id += 1;
    let detail = Detail { target_rank: RANK.into(), item: entity, ..Detail::default() };
    let Some(off) = request(ctx, &mut session, id, kind::STORE, InventoryId::NONE, bank, detail).await? else {
        ctx.check("the take-off request is answered", false, format!("no answer in {:?}", ctx.opts.reply_timeout));
        return Ok(());
    };
    ctx.check("the take-off succeeded", off.result, format!("result {}", off.result));
    let unbound = session.events_since(off.mark).iter().any(|e| e.kind == EventKind::Unbind { entity });
    ctx.check("the item is unbound from the client", unbound && session.world.get(entity).is_none(), format!("entity {entity}"));

    // Wear again what was worn before.
    if let Some(displaced) = displaced {
        id += 1;
        let detail = Detail { class_id: displaced, amount: 1, unstow_root: player, port_path: path.clone(), ..Detail::default() };
        let back = request(ctx, &mut session, id, kind::INTERACTION, bank, InventoryId::NONE, detail).await?;
        ctx.check("what was worn before is worn again", back.is_some_and(|b| b.result), format!("class {displaced:#x}"));
    }
    Ok(())
}

pub async fn moves(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let (player, bank) = setup(ctx, &session)?;
    let class = ctx.opts.helmet_class;
    let personal = InventoryId::personal(player);
    let mut id = 0;
    ensure_stowed(ctx, &mut session, &mut id, class, bank).await?;
    for (label, from, to) in [("bank to personal", bank, personal), ("personal to bank", personal, bank)] {
        id += 1;
        let detail = Detail { class_id: class, amount: 1, target_rank: RANK.into(), ..Detail::default() };
        let done = request(ctx, &mut session, id, kind::MOVE, from, to, detail).await?;
        ctx.check(&format!("move {label} is answered"), done.is_some(), format!("request {id}"));
        let Some(done) = done else { return Ok(()) };
        ctx.check(&format!("move {label} succeeded"), done.result, format!("result {}, item {}", done.result, done.item));
        let bound = session.events_since(done.mark).iter().any(|e| matches!(e.kind, EventKind::Bind { class_crc, .. } if class_crc == class));
        ctx.check(&format!("move {label} binds nothing (a stowed item has no entity)"), !bound, "");
    }
    Ok(())
}

pub async fn stack(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let (_, bank) = setup(ctx, &session)?;
    let class = ctx.require(ctx.opts.stack_class, "--stack-class (a stackable item stowed in the bank)")?;
    let detail = |amount: u32, item: u64| Detail { class_id: class, amount, target_rank: RANK.into(), item, ..Detail::default() };

    let split = request(ctx, &mut session, 1, kind::SPLIT, bank, bank, detail(1, 0)).await?;
    ctx.check("the split is answered", split.is_some(), "request 1");
    let Some(split) = split else { return Ok(()) };
    ctx.check("the split succeeded", split.result, format!("result {}", split.result));
    ctx.check("the split names the new item", split.item != 0, format!("item {}", split.item));

    let stacked = request(ctx, &mut session, 2, kind::STACK, bank, bank, detail(0, split.item)).await?;
    ctx.check("the stack is answered", stacked.is_some(), "request 2");
    if let Some(stacked) = stacked {
        ctx.check("the stack succeeded", stacked.result, format!("result {}", stacked.result));
        ctx.check("the stack names the stack that took it", stacked.item != 0, format!("item {}", stacked.item));
    }
    Ok(())
}

pub async fn pocket(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let (player, bank) = setup(ctx, &session)?;
    let class = ctx.opts.helmet_class;
    let mut id = 0;
    ensure_stowed(ctx, &mut session, &mut id, class, bank).await?;
    id += 1;
    let detail = Detail { class_id: class, amount: 1, unstow_root: player, port_path: "Body_ItemPort:inventory_pocket".into(), ..Detail::default() };
    let done = request(ctx, &mut session, id, kind::INTERACTION, bank, InventoryId::NONE, detail).await?;
    ctx.check("the pocket request is answered", done.is_some(), format!("request {id}"));
    let Some(done) = done else { return Ok(()) };
    ctx.check("the pocket request succeeded", done.result, format!("result {}", done.result));
    ctx.check("the answer names the item", done.item != 0, format!("item {}", done.item));
    let bound = session.events_since(done.mark).iter().any(|e| matches!(e.kind, EventKind::Bind { .. }));
    ctx.check("nothing is bound (the pocket is storage, nothing is worn)", !bound, "");

    // Put it back where it was.
    id += 1;
    let detail = Detail { class_id: class, amount: 1, target_rank: RANK.into(), ..Detail::default() };
    let back = request(ctx, &mut session, id, kind::MOVE, InventoryId::personal(player), bank, detail).await?;
    ctx.check("the item is moved back to the bank", back.is_some_and(|b| b.result), "");
    Ok(())
}

pub async fn equip_from_stack(ctx: &mut Ctx) -> Done {
    let mut session = ctx.join().await?;
    let (player, bank) = setup(ctx, &session)?;
    let class = ctx.require(ctx.opts.stack_equip_class, "--stack-equip-class (a stack of wearable items in the bank)")?;
    let path = ctx.opts.stack_equip_port.clone();
    let port = port_crc(last_port(&path));
    let detail = Detail { class_id: class, amount: 1, unstow_root: player, port_path: path.clone(), ..Detail::default() };
    let Some(done) = request(ctx, &mut session, 1, kind::INTERACTION, bank, InventoryId::NONE, detail).await? else {
        ctx.check("the equip-from-stack request is answered", false, format!("no answer in {:?}", ctx.opts.reply_timeout));
        return Ok(());
    };
    ctx.check("the equip-from-stack succeeded", done.result, format!("result {}", done.result));
    let bound = session.events_since(done.mark).iter().find_map(|event| match event.kind {
        EventKind::Bind { entity, class_crc, attach, .. } if class_crc == class => Some((entity, attach)),
        _ => None,
    });
    ctx.check("one item of the stack is bound", bound.is_some(), format!("{bound:?}"));
    let Some((entity, attach)) = bound else { return Ok(()) };
    ctx.check("it hangs from the port's key", attach == port, format!("{attach:#x} vs {port:#x}"));
    ctx.check("the answer names it", done.item == entity, format!("{} vs {entity}", done.item));
    let detail = Detail { target_rank: RANK.into(), item: entity, ..Detail::default() };
    let off = request(ctx, &mut session, 2, kind::STORE, InventoryId::NONE, bank, detail).await?;
    ctx.check("it is taken off again", off.as_ref().is_some_and(|o| o.result), "");
    if let Some(off) = off {
        ctx.check("and unbound", session.events_since(off.mark).iter().any(|e| e.kind == EventKind::Unbind { entity }), "");
    }
    Ok(())
}

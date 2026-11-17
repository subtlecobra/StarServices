//! Items carried in the hands. The body (`Char_Body`) has two ports for them, `weapon_attach_hand_left` and
//! `weapon_attach_hand_right` (flags `selfphys select usegripdata`), which the client's held entity controller
//! (the held entity controller) reads: the item attached to one is the item held in that hand. So carrying is
//! equipping to a hand port, with the same durable record (an `ATTACHED_TO` edge) and the same bind as any worn item;
//! putting it away is an unequip (the store and move requests of `equip` already do that) and dropping it is an unequip
//! followed by a drop.
//!
//! An item lying in the world is picked up (stowed in the personal inventory) first, and then equipped, as two commands
//! of one request. The request that holds an item in a hand is the equip interaction with a port path that ends in a hand
//! port, e.g. `Body_ItemPort:weapon_attach_hand_right`.

use tonic::Code;
use tracing::{info, warn};

use crate::{
    entity::{Bind, Transform},
    equip::{ItemStore, Origin, Outcome},
    inventory::{Detail, InventoryId, Request, KIND_DROP, KIND_INTERACTION},
    loadout::{port_attach_id, ITEM_FLAGS, ZONE_ATTACH_ID},
    worn::{PathError, Worn, WornItem},
};

/// The ports of the body that hold what is carried.
pub const HAND_PORTS: [&str; 2] = ["weapon_attach_hand_left", "weapon_attach_hand_right"];

/// Whether `port` is a hand port.
pub fn is_hand(port: &str) -> bool {
    HAND_PORTS
        .iter()
        .any(|hand| hand.eq_ignore_ascii_case(port))
}

/// Whether the request is an equip into a hand port.
pub fn wants_hand(request: &Request) -> bool {
    request
        .detail
        .as_ref()
        .and_then(|detail| detail.port_path.rsplit(':').next())
        .is_some_and(is_hand)
}

/// Whether `geid` is worn in a hand port.
pub fn held(worn: &Worn, geid: u64) -> bool {
    worn.get(geid).is_some_and(|item| {
        HAND_PORTS
            .iter()
            .any(|hand| port_attach_id(hand) == item.port_crc)
    })
}

/// The item actions of an input frame (`STake` 0x6c, `SSwap` 0x68, `SDrop` 0x3a), and the action numbers `STake` and `SSwap` carry.
pub const KIND_TAKE: u32 = 0x6c;
pub const KIND_SWAP: u32 = 0x68;
pub const KIND_DROP_ALL: u32 = 0x3a;
/// The request of a toggle of an item that is worn already (the helmet from its hook to the head and back). No client sends
/// it: the DGS makes it of the item action, far above the kinds of the game's inventory requests.
pub const KIND_RELOCATE: i32 = 100;
pub const ACTION_CARRY: u32 = 1;
pub const ACTION_DROP: u32 = 3;
pub const ACTION_EQUIP: u32 = 4;
pub const ACTION_STOW: u32 = 0x12;
/// The numbers of the inventory requests made of an item action start here, far above those a client counts from 1.
const FIRST_REQUEST_ID: u32 = 1 << 30;

/// Whether `request` was made of an item action of the input stream: the client dropped or took the item itself, so it
/// holds a copy that the server's bind replaces.
pub fn from_item_action(request: &Request) -> bool {
    request.request_id >= FIRST_REQUEST_ID
}
/// The amount of a request that is the whole item, not one of a stack.
const ALL: u32 = 0;
/// The first port path element of a hand: the body's own port.
const BODY_PORT: &str = "Body_ItemPort";
/// The request id of the next request made of an item action.
static NEXT_REQUEST_ID: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(FIRST_REQUEST_ID);

fn request(player: u64, kind: i32, item: u64, port_path: String) -> Request {
    let detail = Detail {
        class_id: 0,
        amount: ALL,
        source_rank: String::new(),
        target_rank: String::new(),
        item,
        unstow_root: player,
        port_path,
    };
    Request {
        request_id: NEXT_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        kind,
        player,
        source: InventoryId::default(),
        target: InventoryId::default(),
        detail: Some(detail),
    }
}

/// The hand port path of the first free hand (the right one, the client's preferred hand, first), else `None`.
fn free_hand(worn: &Worn) -> Option<String> {
    [HAND_PORTS[1], HAND_PORTS[0]]
        .into_iter()
        .map(|hand| format!("{BODY_PORT}:{hand}"))
        .find(|path| {
            worn.resolve(path)
                .is_ok_and(|(parent, port)| worn.child_at(parent, port_attach_id(port)).is_none())
        })
}

/// What is held in the hands, as the geids.
pub fn held_items(worn: &Worn) -> Vec<u64> {
    HAND_PORTS
        .iter()
        .filter_map(|hand| {
            let path = format!("{BODY_PORT}:{hand}");
            let (parent, port) = worn.resolve(&path).ok()?;
            worn.child_at(parent, port_attach_id(port))
                .map(|item| item.geid)
        })
        .collect()
}

/// The inventory requests an item action of the input stream comes to: a take or a swap to carry puts the item in a free
/// hand, one to stow picks it up, a swap to drop drops it, and `SDrop` drops what the hands hold. Anything else (an
/// inspect, a swap of an item worn) comes to none.
pub fn requests(
    kind: u32,
    item: u64,
    action: u32,
    worn: &Worn,
    player: u64,
    pocketed: Option<u64>,
) -> Vec<Request> {
    match (kind, action) {
        (KIND_TAKE | KIND_SWAP, ACTION_CARRY | ACTION_EQUIP)
            if item != 0 && !worn.contains(item) =>
        {
            free_hand(worn)
                .map(|path| request(player, KIND_INTERACTION, item, path))
                .into_iter()
                .collect()
        }
        (KIND_TAKE | KIND_SWAP, ACTION_EQUIP)
            if item != 0 && worn.contains(item) && !held(worn, item) =>
        {
            vec![request(player, KIND_RELOCATE, item, String::new())]
        }
        (KIND_TAKE, ACTION_STOW) if item != 0 && !worn.contains(item) => {
            vec![request(player, KIND_INTERACTION, item, String::new())]
        }
        (KIND_SWAP, ACTION_DROP) if item != 0 => {
            vec![request(player, KIND_DROP, item, String::new())]
        }
        // What the hands hold, else the item last taken in hand, which the client keeps as pocketed.
        (KIND_DROP_ALL, _) => {
            let mut held = held_items(worn);
            held.extend(pocketed.filter(|_| held.is_empty()));
            held.into_iter()
                .map(|held| request(player, KIND_DROP, held, String::new()))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// A drag of an item of an inventory to the ground: the client logs it as a drop and sends an interaction naming the
/// inventory and the class and no port path (the unstow with no port to put it in). The same
/// request as a drop of that item from the inventory, else `None`.
pub fn ground_drop(request: &Request) -> Option<Request> {
    let detail = request
        .detail
        .as_ref()
        .filter(|detail| detail.port_path.is_empty() && detail.class_id != 0 && detail.item == 0)?;
    request.source.id()?;
    Some(Request {
        kind: KIND_DROP,
        detail: Some(detail.clone()),
        ..request.clone()
    })
}

/// The item `geid`, now in the personal inventory, goes into a free hand and is bound there. The client takes an item in
/// hand as a put in a pocket with a carry to follow, and shows its own placeholder (a local id) until the server binds the
/// real item to the hand port. `None` when no hand is free or the item cannot be held (it stays in the pocket).
pub async fn into_free_hand<S: ItemStore>(
    store: &S,
    worn: &Worn,
    origin: Origin<'_>,
    request: &Request,
    geid: u64,
    class_crc: u32,
) -> Option<Outcome> {
    let path = free_hand(worn)?;
    let (parent, port) = worn.resolve(&path).ok()?;
    let port_crc = port_attach_id(port);
    let command = format!("{}:hand", origin.command(request));
    if let Err(status) = store
        .equip(&command, request.player, geid, parent, port)
        .await
    {
        warn!(player = request.player, request_id = request.request_id, item = geid, %status, "the pocketed item could not be put in a hand");
        return None;
    }
    let mut after = worn.clone();
    after.attach(WornItem {
        geid,
        class_crc,
        parent,
        port_crc,
    });
    let bind = Bind {
        entity_id: geid,
        class_crc,
        flags: ITEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: parent,
        parent_attach_id: port_crc,
        components: Vec::new(),
        snapshots: Vec::new(),
    };
    info!(
        player = request.player,
        request_id = request.request_id,
        item = geid,
        parent,
        port,
        "pocketed item carried in hand"
    );
    Some(Outcome {
        result: true,
        item: geid,
        worn: Some(after),
        binds: vec![bind],
        unbinds: Vec::new(),
    })
}

/// An equip into a hand: the item (a stowed one, else one lying in the world) is attached to the hand port of the body.
pub async fn carry<S: ItemStore>(
    store: &S,
    worn: &Worn,
    origin: Origin<'_>,
    request: &Request,
) -> Outcome {
    let Some(detail) = request.detail.as_ref() else {
        return Outcome::failed("a carry with no readable detail", request);
    };
    let owner = request.player;
    let (parent, port) = match worn.resolve(&detail.port_path) {
        Ok(found) => found,
        Err(PathError::TooShort) => return Outcome::failed("the port path is too short", request),
        Err(PathError::NothingThere) => {
            return Outcome::failed(
                "the port path leads through a port with nothing worn in it",
                request,
            )
        }
    };
    let port_crc = port_attach_id(port);
    if let Some(taken) = worn.child_at(parent, port_crc) {
        return Outcome::failed(
            &format!("the hand is full, with {}: put it away first", taken.geid),
            request,
        );
    }
    let command = origin.command(request);
    let personal = format!("{owner}:PlayerInventory:0");
    // Where the item is: in the source inventory, or lying in the world (no inventory).
    let (mut geid, mut class_crc, inventory) = match (request.source.id(), detail.item) {
        (Some(inventory), 0) => match store
            .find_stowed(owner, &inventory, detail.class_id, true)
            .await
        {
            Ok(Some(stowed)) => (stowed.geid, stowed.class_crc, Some(inventory)),
            Ok(None) => {
                return Outcome::failed(
                    &format!(
                        "no stowed item of class {:#010x} in {inventory}",
                        detail.class_id
                    ),
                    request,
                )
            }
            Err(status) => {
                return Outcome::failed(
                    &format!("the stowed item could not be looked up: {status}"),
                    request,
                )
            }
        },
        (inventory, item) if item != 0 => (item, detail.class_id, inventory),
        _ => return Outcome::failed("a carry of no item", request),
    };
    if worn.contains(geid) {
        return Outcome::failed(&format!("item {geid} is worn already"), request);
    }
    let mut unbinds = Vec::new();
    let (mut equip_command, mut split_off) = (command.clone(), false);
    match inventory {
        None => {
            // Lying in the world: it is the player's in the personal inventory first.
            match store
                .pickup(
                    &format!("{command}:pickup"),
                    owner,
                    geid,
                    origin.shard,
                    &personal,
                )
                .await
            {
                Ok(done) => {
                    unbinds.push(geid);
                    if done.geid != 0 {
                        geid = done.geid;
                    }
                    if done.class_crc != 0 {
                        class_crc = done.class_crc;
                    }
                    equip_command = format!("{command}:equip");
                }
                Err(status) => {
                    return Outcome::failed(
                        &format!("the item could not be picked up: {status}"),
                        request,
                    )
                }
            }
        }
        Some(inventory) => {
            // One of a stack is carried: it is split off, and put back onto the stack when it cannot be held.
            if store
                .stack_size(owner, &inventory, geid)
                .await
                .is_ok_and(|size| size > 1)
            {
                match store
                    .split(&format!("{command}:split"), owner, geid, 1, &inventory)
                    .await
                {
                    Ok(done) if done.geid != 0 => {
                        geid = done.geid;
                        split_off = true;
                        equip_command = format!("{command}:equip");
                    }
                    Ok(_) => {
                        return Outcome::failed(
                            "splitting one off a stack answered no item",
                            request,
                        )
                    }
                    Err(status) => {
                        return Outcome::failed(
                            &format!("one could not be split off the stack: {status}"),
                            request,
                        )
                    }
                }
            }
        }
    }
    if let Err(status) = store.equip(&equip_command, owner, geid, parent, port).await {
        if split_off {
            if let Some(inventory) = request.source.id() {
                if let Err(back) = store
                    .stack(&format!("{command}:undo"), owner, geid, &inventory, 0)
                    .await
                {
                    warn!(player = owner, request_id = request.request_id, item = geid, %back, "the item split off could not be put back on its stack");
                }
            }
        }
        // A picked up item stays in the personal inventory.
        let mut failed = Outcome::failed(&format!("the item cannot be carried: {status}"), request);
        failed.unbinds = unbinds;
        return failed;
    }
    let mut after = worn.clone();
    after.attach(WornItem {
        geid,
        class_crc,
        parent,
        port_crc,
    });
    let bind = Bind {
        entity_id: geid,
        class_crc,
        flags: ITEM_FLAGS,
        transform: Transform::IDENTITY,
        parent_id: parent,
        parent_attach_id: port_crc,
        components: Vec::new(),
        snapshots: Vec::new(),
    };
    info!(
        player = owner,
        request_id = request.request_id,
        item = geid,
        class = format_args!("{class_crc:#010x}"),
        parent,
        port,
        "item carried"
    );
    Outcome {
        result: true,
        item: geid,
        worn: Some(after),
        binds: vec![bind],
        unbinds,
    }
}

/// A drop of an item held in a hand: it is taken off into the personal inventory and put in the world where the player
/// stands, bound under the player's zone (the client's copy under the hand is unbound first).
pub async fn drop_held<S: ItemStore>(
    store: &S,
    worn: &Worn,
    origin: Origin<'_>,
    request: &Request,
) -> Outcome {
    let Some(detail) = request.detail.as_ref() else {
        return Outcome::failed("a drop with no readable detail", request);
    };
    let Some(spot) = origin.spot else {
        return Outcome::failed("a drop of a player whose position is not known", request);
    };
    let geid = detail.item;
    let command = origin.command(request);
    let personal = format!("{}:PlayerInventory:0", request.player);
    if let Err(status) = store
        .unequip(
            &format!("{command}:release"),
            request.player,
            geid,
            &personal,
        )
        .await
    {
        return Outcome::failed(
            &format!("the held item could not be released: {status}"),
            request,
        );
    }
    let mut after = worn.clone();
    let mut gone = after.detach(geid);
    gone.reverse();
    match store
        .drop_item(
            &format!("{command}:drop"),
            request.player,
            geid,
            0,
            origin.shard,
            &spot,
        )
        .await
    {
        Ok(done) => {
            let bind = Bind {
                entity_id: done.geid,
                class_crc: done.class_crc,
                flags: ITEM_FLAGS,
                transform: Transform {
                    position: spot.position,
                    rotation: spot.rotation,
                },
                parent_id: spot.zone,
                parent_attach_id: ZONE_ATTACH_ID,
                components: Vec::new(),
                snapshots: Vec::new(),
            };
            info!(
                player = request.player,
                request_id = request.request_id,
                item = done.geid,
                zone = spot.zone,
                "held item dropped"
            );
            Outcome {
                result: true,
                item: done.geid,
                worn: Some(after),
                binds: vec![bind],
                unbinds: gone,
            }
        }
        Err(status) => {
            // Released but not dropped: it is in the personal inventory and no longer in the hand.
            let code = if status.code() == Code::Unavailable {
                "sc-entitygraph did not answer"
            } else {
                "the drop was refused"
            };
            warn!(player = request.player, request_id = request.request_id, item = geid, %status, "{code}: the held item is stowed instead");
            Outcome {
                result: false,
                item: 0,
                worn: Some(after),
                binds: Vec::new(),
                unbinds: gone,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tonic::Status;

    use super::*;
    use crate::{
        equip::{apply, Done, Spot, Stowed},
        inventory::{
            tests::{build, detail, BANK, PLAYER},
            InventoryId, KIND_DROP,
        },
        loadout::{loadout_binds, DEFAULT_LOADOUT},
    };

    const CRATE_CLASS: u32 = 0x0c0f_fee1;
    const CRATE: u64 = 400_000_000_777;
    const NONE: InventoryId = InventoryId {
        holder: 0,
        context: 11,
        subject: 0,
    };
    const BANK_ID: &str = "300000100001:Location:1105811199";
    const RIGHT: &str = "Body_ItemPort:weapon_attach_hand_right";
    const SPOT: Spot = Spot {
        zone: 5,
        position: [1.0, 2.0, 3.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
    };
    const ORIGIN: Origin<'static> = Origin {
        replicant: 1,
        session: 2,
        shard: "shard-a",
        spot: Some(SPOT),
    };

    #[derive(Default)]
    struct Fake {
        stack: u32,
        refuse_equip: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn note(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ItemStore for Fake {
        async fn find_stowed(
            &self,
            _: u64,
            inventory: &str,
            class_crc: u32,
            _: bool,
        ) -> Result<Option<Stowed>, Status> {
            Ok(
                (inventory == BANK_ID && class_crc == CRATE_CLASS).then_some(Stowed {
                    geid: CRATE,
                    class_crc,
                }),
            )
        }
        async fn stack_size(&self, _: u64, _: &str, _: u64) -> Result<u32, Status> {
            Ok(self.stack.max(1))
        }
        async fn equip(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            parent: u64,
            port: &str,
        ) -> Result<Vec<crate::equip::Placed>, Status> {
            self.note(format!("equip {command} {geid} {parent} {port}"));
            if self.refuse_equip {
                Err(Status::failed_precondition("no"))
            } else {
                Ok(Vec::new())
            }
        }
        async fn relocate(&self, _: &str, _: u64, _: u64) -> Result<(u64, u32), Status> {
            Err(Status::failed_precondition("no"))
        }
        async fn unequip(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            inventory: &str,
        ) -> Result<(), Status> {
            self.note(format!("unequip {command} {geid} {inventory}"));
            Ok(())
        }
        async fn move_item(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: &str,
            _: &str,
            _: u32,
        ) -> Result<(), Status> {
            Ok(())
        }
        async fn stack(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            _: &str,
            _: u32,
        ) -> Result<Done, Status> {
            self.note(format!("stack {command} {geid}"));
            Ok(Done::default())
        }
        async fn stack_all(&self, _: &str, _: u64, _: &str, _: u32) -> Result<Done, Status> {
            Ok(Done::default())
        }
        async fn split(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            amount: u32,
            _: &str,
        ) -> Result<Done, Status> {
            self.note(format!("split {command} {geid} {amount}"));
            Ok(Done {
                geid: CRATE + 1,
                class_crc: CRATE_CLASS,
                ..Done::default()
            })
        }
        async fn swap(&self, _: &str, _: u64, _: u64, _: u64) -> Result<Done, Status> {
            Ok(Done::default())
        }
        async fn repool_ammo(&self, _: &str, _: u64, _: &str, _: u32) -> Result<Done, Status> {
            Ok(Done::default())
        }
        async fn drop_item(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            amount: u32,
            _: &str,
            spot: &Spot,
        ) -> Result<Done, Status> {
            self.note(format!("drop {command} {geid} {amount} {}", spot.zone));
            Ok(Done {
                geid,
                class_crc: CRATE_CLASS,
                ..Done::default()
            })
        }
        async fn pickup(
            &self,
            command: &str,
            _: u64,
            geid: u64,
            shard: &str,
            to: &str,
        ) -> Result<Done, Status> {
            self.note(format!("pickup {command} {geid} {shard} {to}"));
            Ok(Done {
                geid,
                class_crc: CRATE_CLASS,
                ..Done::default()
            })
        }
    }

    fn worn() -> (Worn, u64) {
        let binds = loadout_binds(PLAYER, 0, &DEFAULT_LOADOUT);
        let worn = Worn::from_binds(PLAYER, &binds);
        let body = worn
            .child_at(PLAYER, port_attach_id("Body_ItemPort"))
            .unwrap()
            .geid;
        (worn, body)
    }

    fn interaction(source: InventoryId, item: u64, class: u32, path: &str) -> Request {
        Request::parse(
            &build(5, 1, PLAYER, source, NONE, &detail(class, item, path)),
            PLAYER,
        )
        .unwrap()
    }

    #[test]
    fn only_the_hand_ports_are_hands() {
        assert!(is_hand("weapon_attach_hand_left") && is_hand("WEAPON_ATTACH_HAND_RIGHT"));
        assert!(!is_hand("inventory_pocket") && !is_hand("backpack"));
        assert!(wants_hand(&interaction(BANK, 0, CRATE_CLASS, RIGHT)));
        assert!(!wants_hand(&interaction(
            BANK,
            0,
            CRATE_CLASS,
            "Body_ItemPort:inventory_pocket"
        )));
    }

    #[tokio::test]
    async fn a_stowed_item_is_carried_in_the_hand_and_bound_under_the_body() {
        let (worn, body) = worn();
        let store = Fake::default();
        let outcome = apply(
            &store,
            &worn,
            ORIGIN,
            &interaction(BANK, 0, CRATE_CLASS, RIGHT),
        )
        .await;
        assert!(outcome.result);
        assert_eq!(outcome.item, CRATE);
        assert_eq!(
            store.calls()[0],
            format!("equip live:1:2:{PLAYER}:5 {CRATE} {body} weapon_attach_hand_right")
        );
        let bind = &outcome.binds[0];
        assert_eq!(
            (bind.entity_id, bind.parent_id, bind.parent_attach_id),
            (CRATE, body, port_attach_id("weapon_attach_hand_right"))
        );
        assert_eq!(bind.flags, ITEM_FLAGS);
        let after = outcome.worn.unwrap();
        assert!(held(&after, CRATE) && !held(&worn, CRATE));
        // The hand is full now.
        let again = apply(
            &store,
            &after,
            ORIGIN,
            &interaction(BANK, CRATE + 5, CRATE_CLASS, RIGHT),
        )
        .await;
        assert!(!again.result);
    }

    #[tokio::test]
    async fn an_item_in_the_world_is_picked_up_then_carried() {
        let (worn, body) = worn();
        let store = Fake::default();
        let outcome = apply(
            &store,
            &worn,
            ORIGIN,
            &interaction(NONE, CRATE, CRATE_CLASS, RIGHT),
        )
        .await;
        assert!(outcome.result);
        assert_eq!(outcome.unbinds, vec![CRATE], "the world copy goes");
        let calls = store.calls();
        assert!(
            calls[0].starts_with("pickup live:1:2:300000100001:5:pickup 400000000777 shard-a "),
            "{calls:?}"
        );
        assert_eq!(
            calls[1],
            format!("equip live:1:2:{PLAYER}:5:equip {CRATE} {body} weapon_attach_hand_right")
        );
        assert!(held(&outcome.worn.unwrap(), CRATE));
    }

    #[tokio::test]
    async fn one_of_a_stack_is_split_off_and_put_back_when_it_cannot_be_held() {
        let (worn, _) = worn();
        let store = Fake {
            stack: 3,
            refuse_equip: true,
            ..Fake::default()
        };
        let outcome = apply(
            &store,
            &worn,
            ORIGIN,
            &interaction(BANK, CRATE, CRATE_CLASS, RIGHT),
        )
        .await;
        assert!(!outcome.result && outcome.worn.is_none() && outcome.binds.is_empty());
        let calls = store.calls();
        assert!(
            calls[0].starts_with("split")
                && calls[1].starts_with("equip")
                && calls[2].starts_with("stack"),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn a_held_item_is_put_away_with_a_store_and_dropped_with_a_drop() {
        let (worn, _) = worn();
        let store = Fake::default();
        let held_worn = apply(
            &store,
            &worn,
            ORIGIN,
            &interaction(BANK, 0, CRATE_CLASS, RIGHT),
        )
        .await
        .worn
        .unwrap();
        let personal = format!("{PLAYER}:PlayerInventory:0");
        // Put away: the existing store request takes it off.
        let away = Request::parse(
            &build(6, 7, PLAYER, NONE, BANK, &detail(0, CRATE, "")),
            PLAYER,
        )
        .unwrap();
        let outcome = apply(&store, &held_worn, ORIGIN, &away).await;
        assert!(
            outcome.result
                && outcome.unbinds == vec![CRATE]
                && !held(&outcome.worn.unwrap(), CRATE)
        );
        // Dropped: released into the personal inventory, then put in the world under the zone.
        let drop = Request::parse(
            &build(
                7,
                KIND_DROP as u8,
                PLAYER,
                NONE,
                NONE,
                &detail(0, CRATE, ""),
            ),
            PLAYER,
        )
        .unwrap();
        let outcome = apply(&store, &held_worn, ORIGIN, &drop).await;
        assert!(outcome.result);
        assert_eq!(outcome.unbinds, vec![CRATE]);
        assert_eq!(
            (outcome.binds[0].entity_id, outcome.binds[0].parent_id),
            (CRATE, 5)
        );
        assert_eq!(
            outcome.binds[0].parent_attach_id, ZONE_ATTACH_ID,
            "a dropped item lies in the zone, not in a port"
        );
        assert!(!held(&outcome.worn.unwrap(), CRATE));
        let calls = store.calls();
        assert_eq!(
            calls[calls.len() - 2],
            format!("unequip live:1:2:{PLAYER}:7:release {CRATE} {personal}")
        );
        assert!(calls[calls.len() - 1]
            .starts_with(&format!("drop live:1:2:{PLAYER}:7:drop {CRATE} 0 5")));
    }

    #[test]
    fn a_drag_of_an_inventory_item_to_the_ground_is_a_drop() {
        let click = Request::parse(
            &build(
                9,
                KIND_INTERACTION as u8,
                PLAYER,
                BANK,
                NONE,
                &detail(CRATE_CLASS, 0, ""),
            ),
            PLAYER,
        )
        .unwrap();
        let drop = ground_drop(&click).unwrap();
        assert_eq!((drop.kind, drop.source), (KIND_DROP, click.source));
        let with_port = Request::parse(
            &build(
                9,
                KIND_INTERACTION as u8,
                PLAYER,
                BANK,
                NONE,
                &detail(CRATE_CLASS, 0, "Body_ItemPort:Armor_Undersuit:Armor_Helmet"),
            ),
            PLAYER,
        )
        .unwrap();
        assert!(ground_drop(&with_port).is_none());
    }

    #[tokio::test]
    async fn item_actions_come_to_carry_and_drop_requests() {
        let (worn, _) = worn();
        // A take to carry fills the right hand, the next one the left, then none is free.
        let take = requests(KIND_TAKE, CRATE, ACTION_CARRY, &worn, PLAYER, None);
        assert_eq!(take.len(), 1);
        let detail = take[0].detail.as_ref().unwrap();
        assert_eq!(
            (take[0].kind, detail.item, detail.port_path.as_str()),
            (KIND_INTERACTION, CRATE, RIGHT)
        );
        let store = Fake::default();
        let held_worn = apply(&store, &worn, ORIGIN, &take[0]).await.worn.unwrap();
        let second = requests(KIND_SWAP, CRATE + 1, ACTION_EQUIP, &held_worn, PLAYER, None);
        assert_eq!(
            second[0].detail.as_ref().unwrap().port_path,
            "Body_ItemPort:weapon_attach_hand_left"
        );
        // `SDrop` drops what is held, a swap to drop drops that item, an inspect does nothing.
        let dropped = requests(KIND_DROP_ALL, 0, 1, &held_worn, PLAYER, None);
        assert_eq!(
            (
                dropped.len(),
                dropped[0].kind,
                dropped[0].detail.as_ref().unwrap().item
            ),
            (1, KIND_DROP, CRATE)
        );
        assert_eq!(
            requests(KIND_SWAP, CRATE, ACTION_DROP, &held_worn, PLAYER, None)[0].kind,
            KIND_DROP
        );
        assert!(requests(KIND_SWAP, CRATE, 8, &held_worn, PLAYER, None).is_empty());
        assert!(
            requests(KIND_DROP_ALL, 0, 1, &worn, PLAYER, None).is_empty(),
            "nothing held, nothing dropped"
        );
    }

    #[test]
    fn a_drop_with_nothing_in_hand_drops_the_item_last_pocketed() {
        let (worn, _) = worn();
        let dropped = requests(KIND_DROP_ALL, 0, 1, &worn, PLAYER, Some(CRATE));
        assert_eq!(
            (
                dropped.len(),
                dropped[0].kind,
                dropped[0].detail.as_ref().unwrap().item
            ),
            (1, KIND_DROP, CRATE)
        );
    }
}

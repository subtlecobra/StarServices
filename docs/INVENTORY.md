# Inventories

sc-entitygraph keeps every kind of inventory in one model in ArangoDB: a player's personal inventory, a player's
item bank at each location, a ship's holds, and the inventory of any item that holds others (a crate, a box, a
backpack), nested to any depth. Only sc-entitygraph reads or writes these collections (see [DATA.md](DATA.md)). Each
ShardRegion has its own ArangoDB and sc-entitygraph, so a region holds the inventories of what is in it.

## How the game names an inventory

An inventory id is `<holder>:<context>:<subject>`, which the game formats with `"%llu:%s:%llu"` (0x1406cc590) from
`SInventoryId` (holder entity, `EInventoryContext`, subject). The contexts are `Container`, `Debug`, `Entitlement`,
`Location`, `PlayerInventory`, `PlayerEscrowInventory`, `Prison`, `PersonalEntityInventory`, `Hangar`, `Mission`,
`ClientOnly` and `Undefined`.

| Kind | Id | Owner (`SInventoryConfiguration::Create`, 0x140649510) |
|---|---|---|
| Personal | `<player>:PlayerInventory:0` | the player (the holder) |
| Location item bank | `<player>:Location:<location id>` | the player. The location id is the class CRC of the `StarMapObject` (`CPlayerInventoryManager::GetPersonalLocationInventoryConfiguration`) |
| Ship hold, crate, box, backpack | `<entity>:Container:0` | none: whoever owns what holds it |
| A player's own inventory inside an entity | `<entity>:PersonalEntityInventory:<player>` | the subject |
| Hangar, entitlement, escrow, prison | `<player>:<context>:<subject>` | the player (the holder) |

When the game serializes a new inventory (`IEntityGraphDatabase::SerializeCreateInventoryArguments`, 0x140f373e0) it
sends the id, the context's name, the owner and the subject, and the physical configuration's two numbers unscaled.
A ship's holds are not the vehicle's own inventory: the cargo grids are item entities attached to the vehicle, each
with its own `Container` inventory, as is any item with `SCItemInventoryContainerComponentParams`.

## The model

Edges run from what is held to what holds it, in `entity_edges`:

```
item --STOWED_IN(2)--> inventory --ATTACHED_TO(1)--> holder entity --ATTACHED_TO(1)--> parent entity ...
```

- `inventories`: one document per inventory, `_key` the escaped id. Fields as the client names them (`id`, `ownerId`,
  `context`, `subjectId`, `inventoryType`, `configuration.physical.{capacity, occupancy}`, `shared`, `name`), plus
  `holderId` and `dirty` (only while a transaction waits to count it).
- The holder edge, key `1_<inventory id>`, ties an inventory to its holder entity, so the contents of a ship are
  reachable from the ship by walking edges, and erasing an entity erases its inventories and what they hold.
- An item's stowed edge, key `2_<geid>`: an entity is in one inventory at a time. Its `physical.occupancy`, when the
  caller sets one, is the room the item takes; otherwise the room comes from its class.
- A container item is an entity stowed in one inventory and the holder of another, so a crate in a ship's hold with
  a box in it is `box -> crate's inventory -> crate -> hold -> cargo grid -> ship`.

Indexes, created at start: `inventories` on `id`, (`ownerId`, `context`), `holderId`, and a sparse one on `dirty`;
`entities` on `ownerId`, `parentUrn` and a sparse one on `inventoryId`; `entity_classes` on `guidHashCrc`. The edge
index covers `_from` and `_to`.

## Item type restrictions

A container refuses some item types: its `InventoryContainer` record lists `excludedItemSubTypes` (a type, or some of its
subtypes) and `overrideStorableItemTypes` (when present, the only types it takes). The seed keeps both on the class of
the item that holds the inventory (`entity_classes.inventory.excluded` and `.only`), and the filters of the
`ItemBankContainer` record on the pseudo class `itembankcontainer` (`itemBank`), which `Location` inventories use. At
the end of every plan, with the capacity count, a dirty inventory holding an item its rules refuse stops the transaction.

## Capacity

Volumes are in micro-SCU, the game's own unit for container capacity (`InventoryContainer.capacityMicroSCU`) and
item room. The messages carry them in 32 bits, so a hold above about 2147 SCU reads as full scale there; the database
keeps the whole number. Which unit the game puts in `PhysicalInventoryConfiguration` is not read: our DGS is the only
writer that matters, and it uses micro-SCU.

- The seed (read by `sc-admin seed-export`, written by sc-entitygraph) puts on each entity class `occupancyMicroScu` (`inventoryOccupancyVolume` of the item
  definition) and, for a class with an inventory container, `inventory.capacityMicroScu`: a closed container's own
  capacity, or the whole 1.25 m cubes that fit in an open container's interior, as `InventoryContainer::PostLoad`
  computes it.
- An item takes the occupancy of its stowed edge, or else its class's `occupancyMicroScu` times its stack.
- An infinite inventory (type 1) has no configuration and no limit; a capacity of 0 is no limit either.

Every write that changes what an inventory holds marks it `dirty`: a stow, a move (source and target), a move-all, a
stack change, an erasure and a configuration change. The last statement of the transaction counts each dirty
inventory again from its stowed edges, writes the occupancy and refuses the whole transaction when one is over
capacity (`FAILED_PRECONDITION`, `inventory is full: <id>`). Counting at the end keeps stack changes and moves right in
any order, and since it writes the inventory document, two transactions filling the same inventory conflict instead
of both passing.

## Who may use an inventory

A command is refused (`inventory of another owner`) when it writes into, or takes an item out of, an inventory whose
effective owner is someone else:

- An inventory's effective owner is its `ownerId`, or, when that is `0` (a container), the first owner found walking up
  from it through its holder, the entities holding that one and the inventories they are stowed in. A crate's
  inventory belongs to whoever owns the crate, or the inventory it sits in.
- An inventory that does not exist yet is checked by the owner its id implies, so nobody can open
  `<someone else>:Location:<x>`.
- A `shared` inventory is open to everybody, and so is one with no owner anywhere up the chain (loot in the world).
- Entities keep their own check: one owned by someone else cannot be changed.

`GetInventories` answers the caller's own inventories and the containers of the entities it owns, at any depth. For
another player's id it answers only the shared ones; a request without a token is answered as before.

## Loops

Stowing an entity into an inventory that is inside it is refused (`an item cannot be stowed inside itself`): the
statement walks up from the target through holders, parents and the inventories they are in, and fails if it meets
the entity. `MoveAll` checks the same for the source inventory, and refuses a source equal to the target.

## Created on first need

- The personal inventory, with the player entity, on the account's first call (as before).
- A player's inventory whose id names the caller as its owner (a location, a hangar, a personal entity inventory)
  is created, infinite, the first time something is stowed or moved into it, with its holder edge to the player.
- A player's own `Location` and `Hangar` inventory is also created by the first `EntityQuery` that asks for it by id (the
  ASOP ship list's two inventory reads, see SHIPS.md), even if nothing was ever stowed there; nobody else's is.
- A `Container` inventory is created with every created entity whose class has an inventory container: a ship's cargo
  grids, crates, backpacks. Stowing into `<entity>:Container:0` of an existing entity whose class is not known also
  creates it, infinite; a class known to have no container gets none.
- A `CreateCommand` with an inventory id stows the entities of each graph that nothing in the graph holds there,
  with stowed edges, so the tree walks find them.
- A move's `target_inventory_lazy_creation` and `CreateInventoryCommand` work as the client sends them; the context,
  subject, name and owner it leaves out are filled from the id.

## What the client asks when it opens an item

Opening an item with storage (`OpenNestedInventory`) is run by the client itself, as the token holder. It first looks the
item up in the inventory it is in: `EntityQuery` with `inventory_id`, a filter on `classGuidCrc`, a tree projection and
`outgoing_edges`. The inventory's items come from the `STOWED_IN` edges in the answer. A tree walk follows edges from
what is held to what holds it, so it never returns the edge a root leaves by; `outgoing_edges` therefore also adds the
outgoing edges of the nodes of a tree. Without the stowed edge the client finds "0 items" in the inventory and the
request fails (`GetGridItem ... doesn't have item requested`) with nothing shown. Only then does it query the item's
own `<geid>:Container:0`, which is made on that first query when it is missing and the item's class has a container
(an item made before the class was seeded). The inventory queries are logged (`inventory entity query`, nodes and edges).

## Inventory access tokens

The client asks for access to an inventory that is not the personal one (the container of a worn item or of one it opens)
before it uses it, and queues every request on that inventory until it gets it. The request is an `EntityTokenRequest` (replication
message 21) on the item's entity for the container component's user token, and the answer an `EntityTokenEvent` (23), granted or
denied; the layouts are in the protocol notes, "Inventory access tokens". The replicant relays: it reads the request and asks the
DGS (`DgsControl.EntityToken`, with the token, request and flag byte as the client sent them), then sends the decision as the event.
The DGS decides (`services/dgs/src/token.rs`):
- A user token (flag bit 0) of an item in the player's worn tree is granted; of any other item the player owns (sc-entitygraph
  `GetOwners`) too; of one another owner has, denied. Arbitrated tokens (flag 0xce), an entity without owner, a session the DGS
  does not hold and an unreachable sc-entitygraph get no answer, as before.
- Every request is logged (`entity token request`, with the player, entity, token, request, flags, decision and why), and the
  replicant logs `entity token decided by the DGS` and `entity token event to the client`.

## Commands and queries

| Call | Inventory behaviour |
|---|---|
| `MutateEntities`: `CreateInventory`, `EraseInventory`, `ChangeInventoryConfiguration` | create (owner from the id when 0), erase with everything inside, change the capacity (the occupancy is counted, not taken) |
| `MutateEntities`: `Create`, `Move`, `MoveAll`, `Stack`/`Unstack`/`SetStack`, `Erase` | the checks above, in one transaction |
| `Move` | the same as the `Move` command |
| `InventoryQuery` | by filter on any inventory field, or a tree from an entity or an inventory, which walks down through items into their inventories |
| `GetInventories` | as above |
| `EntityQuery` with an inventory id, `GetEntityStowContexts*`, `StreamStowContextsByOwner` | the items of an inventory, and where each entity is stowed |

## Admin calls

sc-admin-web changes a player's inventories through the "Admin inventories" block of `EntityGraphInternal`
(`proto/internal/scbackend/internal/entitygraph/v1/entitygraph.proto`), never through the database. sc-entitygraph acts for the player `owner_id`: each
mutation is a plan as the player's own command would be, so the checks above apply unchanged (entity and inventory of
another owner, capacity, loops, inventories created on first need), and two more:

- **Live on a shard**: an item unstowed onto a shard (`stowed` false with a `shardId`), or an inventory any of whose
  holders up the chain is, is the DGS's, and is refused (`FAILED_PRECONDITION`, `the item is live on a shard`, `the
  inventory is held by something live on a shard`). The player entity itself is left out of that walk: whether the
  player is playing is sc-hub's to say, and sc-admin-web refuses every change while sc-hub has the player on a shard
  or does not answer, as for locations.
- **Once per command id**: sc-admin-web sends `admin:<request id>`. The record in `applied_commands` (key
  `admin-rpc:<command id>`, apart from the client's ids) is written in the same transaction and keeps the answer, so a
  repeat gets the same GEID with `replayed`, and a twin request that loses the race answers from the winner's record.

| Call | What |
|---|---|
| `AdminInventoryTree` | The player's own inventories, the containers of the entities it owns, and every inventory below those and below its entities that are in no inventory (a ship and its attached cargo grids), with each one's holder, parent (the nearest inventory up the holder chain), depth, whether it is live, and its items (class, English name, stack, owner, the inventories each holds, whether it is on a shard). Personal first, then each inventory followed by its children |
| `AdminMoveItem` | `Move` from the inventory the item is in. A `quantity` below the stack splits it: the stack shrinks where it is, and a new item of the same class and owner with a new GEID takes `quantity` to the target |
| `AdminAddItem` | `Create` of one item of a seeded class (by GUID), owned by the player, stowed in the inventory; a container class gets its inventory with it |
| `AdminRemoveItem` | `Erase` of a stowed item and everything it holds; the answer counts the entities erased. A player is not an item |
| `AdminSearchEntityClasses` | `entity_classes` by part of the class name or English name (case ignored, `%` and `_` literal) or by GUID, with room and container capacity |

### Item GEIDs

A new item takes the next GEID of a counter in `entitygraph_meta` (`item_geid`), from 400,000,000,000: above the
players (300,000,000,000 plus the account index) and the container tree (from 0x2e90cf4b80, about 200,000,000,000),
and below 2^53, so ArangoDB's doubles keep it whole. A GEID of a transaction that failed is skipped, and the plan
refuses one that is in use. The DGS's items and entitlement claims (TODO.md) can take theirs from the same counter.

## Deploying

Existing data needs no manual step. At start sc-entitygraph creates the indexes and fills `context`, `subjectId` and
`holderId` of any inventory that lacks them from its id (the player inventories written before had them). Occupancies
written before are counted again on each inventory's next change. Seed again (the `sc-seed` Job, or `scripts/seed.sh`) to add
`occupancyMicroScu` and `inventory` to the entity classes; until then, container items get no inventory when created
and items take no room unless their stowed edge names it. The admin calls need nothing new in the database: the GEID
counter is made on the first item added, and sc-admin-web needs this sc-entitygraph before it (the admin calls are
new RPCs). Adding by class needs the seeded classes.

Entities made before the item type numbers were seeded have 0 for them; at start and after each class seed sc-entitygraph
gives them the numbers of their class (idempotent), which also ends the client's "Entity Item Types Mismatch" errors.

## Equipping

How a player's worn gear is modelled, persisted and changed. Marked *(inferred)* where the game's data does not say.

### What worn means

A worn item is an entity with an `ATTACHED_TO(1)` edge to a port of another worn item, the body to the player's
`Body_ItemPort`. It is not stowed (`stowed` false, no `STOWED_IN` edge, no inventory). A stowed item is in an inventory
(`STOWED_IN(2)`). An item is one or the other: equipping is moving it from a stowed edge to an attached edge in one
transaction, taking it off is the way back. The edge keeps only the CRC of the port's name (`attachmentPortCrc`, CRC-32C of
the lowercased name, what the client keys its ports by); the name comes from the parent's class.

```
player --Body_ItemPort-- body --Armor_Undersuit-- undersuit --Armor_Torso-- torso --backpack-- backpack
                                                          \--wep_sidearm-- pistol --magazine_attach-- magazine
```

What hangs from an item stays on it when it is taken off: a torso piece taken off arrives in the inventory with its
backpack and weapons still attached to it. Its occupancy is its own class's.

### Ports, from the game's data

Each item class has `SItemPortContainerComponentParams.Ports` (`SItemPortDef`): `Name`, `MinSize`..`MaxSize` (the sizes of
`AttachDef.Size` it takes), `Types` (an item type, optionally some sub types), `RequiredPortTags` (tags the item must have),
`PortTags` (what the port offers items whose `AttachDef.RequiredTags` ask for it) and `Flags`. The seed keeps these on the
class (`entity_classes.ports`) for the classes of what a character carries (`Char_*`, `WeaponPersonal`, `MobiGlas`,
`Gadget`; not the ports of ships), with the item's `attachSize`, `attachTags` and `requiredTags`. 4.10.2, DataCore:

| Parent class | Ports (sizes) and what they take |
|---|---|
| Player (`Body_ItemPort`) | the body `Char_Body` (the loadout's root; the player entity has no class of ports in the seed) |
| Body (`body_01_noMagicPocket`) | `Armor_Undersuit` 1..1 `Char_Armor_Undersuit`; `Clothing_Feet`/`Hands`/`Legs`/`Torso_0` 1..1 their `Char_Clothing_*` type; `Clothing_Torso_1` 1..2 `Char_Clothing_Torso_1`; `Head_ItemPort` `Char_Head`; `lens_attach`; `mobiglas_attach` `MobiGlas`; `weapon_attach_hand_left/right`; `inventory_pocket(_2)`; `restraint_attach*` |
| Undersuit (`rsi_odyssey_undersuit_01_01_01`) | `Armor_Torso`, `Armor_Arms`, `Armor_Legs`, `Armor_Helmet` (1..1, the `Char_Armor_*` type), `wep_sidearm` 1..1 `WeaponPersonal` Small, `wep_stocked_3` 2..5 `WeaponPersonal` Medium/Large, `medPen_attach_1` and `oxyPen_attach_1` `FPS_Consumable`, `magazine_attach_1/2` `WeaponAttachment` Magazine, `utility_attach_1` `WeaponPersonal` Gadget or `RemovableChip` Hacking |
| Torso armor (`..._core_...`) | `wep_stocked_2` 2..4 and `wep_stocked_3` 2..5 (`WeaponPersonal` Medium (and Large on 3), `FPS_Deployable` Medium, `WeaponPersonal` Gadget), `magazine_attach_1..6`, `grenade_attach_1..3` 1..1 (`WeaponPersonal` Grenade, `FPS_Deployable` Small), `backpack` 1..2 `Char_Armor_Backpack` requiring tag `$backpack`, `gadget_attach_1` `Gadget` |
| Backpack | `wep_stocked_2/3`, `gadget_attach_1` |
| A weapon | `magazine_attach` `WeaponAttachment` Magazine requiring the weapon's own tag, `optics_attach` IronSight, ... |

Sizes of items read: pistol `WeaponPersonal` Small size 1, rifle Medium size 2, medpen `FPS_Consumable` MedPack size 1,
magazine size 1, armor size 1. The default loadout's port names (`Armor_Undersuit`, `wep_sidearm`, `medPen_attach_1`,
`magazine_attach_1`) are all ports of these classes.

`equip::fits` (sc-entitygraph): the item's `itemType` is one of the port's `Types`, and its sub type one of that type's
sub types when it lists some; its `attachSize` is within `minSize..maxSize` (not checked for a class seeded without one); it carries each of the
port's `requiredPortTags` (tags compared without case, a leading `$` dropped *(inferred: `$backpack` against the tag
`backpack`)*), and the port offers each of the item's `requiredTags`. A sub type of `''` in the data (`Grenade`, `''`) is dropped.
Ports a person does not equip to are left out *(inferred from the names and flags)*: `Head_ItemPort`, `lens_attach`,
`mobiglas_attach`, `weapon_attach_hand_*` (what is in a hand), ports flagged `inventoryinternalonly` (the pockets) or
`restraint_attach`. Only single items equip: a stack is moved out one at a time first.

### Who changes it

- **Offline** (sc-hub says the player is not on a shard): sc-entitygraph, through `AdminEquipItem` and `AdminUnequipItem`
  (below). Once per command id, in one transaction, with the ownership, loop and capacity checks of every other
  command: the item must be the player's and stowed, the parent worn by the player, the port free (an occupied port is
  refused, take the item off first), and a taken-off item must fit the inventory it goes to. Neither call knows whether the
  player is live (a worn item is not marked with a shard): the caller refuses, as for the other admin calls.
- **Live**: the DGS has authority (`services/dgs/src/equip.rs`, `worn.rs`, `inventory.rs`). The client sends
  `pi.RmAuthorityProcessInventoryManagementOperation` for every request it does not run itself, and waits for
  `pi.RmTokenMoveRequestCompleted(result, requestId, item)`. The DGS decodes the request (the layout is in the protocol
  notes, "Inventory requests of the player"), and applies it to the tree of what the player wears, which it builds when the
  player joins (the loadout's binds and the equipped items sc-entitygraph lists):
  - **Interaction with an `itemPortPath`** (equip from an inventory): the client names the item by class CRC
    (`classId`, the CRC of the class GUID) and the source inventory, not by entity (`itemEntityId` is 0). The DGS finds the
    player's stowed single item of that class there (`AdminInventoryTree`), resolves the path (`Body_ItemPort:Armor_Undersuit:
    Armor_Helmet`: from the player's port down through the worn items, the last element the free port of the last item),
    refuses a port that is taken, and asks sc-entitygraph to attach it (`AdminEquipItem`, once per command id
    `live:<replicant>:<session>:<player>:<request id>`), which checks ownership, that it is stowed, and the class rules
    (`equip::fits`). On success the item is bound to the client under its parent with the port's CRC (the loadout's own
    bind), and the answer carries its GEID.
  - **Store** (a worn item into the inventory in `targetInventory`): `AdminUnequipItem`, then the item and what hangs from
    it are unbound from the client (children first).
  - **Move**: a worn item is a Store; a stowed one goes through `AdminMoveItem` (by entity, or by class in the source
    inventory), and nothing is bound.
  - The other request types are in "Live inventory operations" below. Anything not listed there (OpenNestedInventory) is still
    answered as completed without a change. A request that cannot be applied is answered with result `false`, never left to
    hang.
  What hangs from an equipped stowed item (a backpack on a torso piece) is not bound yet: only the item itself is.

### On join

`EnsureLoadout` still makes the default gear once per loadout (`loadout:v2:<player>:<hash>`), and every join asks for it.
The answer is now what is worn: the loadout's GEIDs with `0` for an item that was taken off (the DGS leaves it out, and
whatever hung from it), and `equipped`: the items attached below the loadout's that it did not make, parents before
children, each with its class CRC, port CRC and parent. The DGS binds those too. A changed loadout (another head) builds new
default gear and erases the old; what was equipped on the old items is not lost: it is stowed in the player's
personal inventory. *Not done*: putting it back on the new gear.

A character that never equipped anything gets the default gear as before. Until the entitlements put the
player's items in the game, the default gear stays what a new player wears.

### Calls

| Call | What |
|---|---|
| `AdminEquipment` | The worn items (parents before children, with the port each hangs from, and whether it can be taken off) and the equippable ports of each, with the occupant and the player's stowed single items that fit |
| `AdminEquipItem` | Attach a stowed item to `<worn item>:<port name>` |
| `AdminUnequipItem` | Detach a worn item and stow it, with what hangs from it, in an inventory (created on first need as for a move) |

sc-admin-web shows the worn items on the Inventory tab, an "Equip on" chooser in the row of each item that fits a free
port (the picker lists only free compatible ports), and a "Take off" form with the inventory tree to put it in.

### Deploying

Seed again so that classes have `attachSize`, `attachTags`, `requiredTags` and `ports` (until then nothing can be equipped:
the port list is empty). sc-entitygraph before sc-admin-web and sc-dgs (new RPCs, the new `EnsureLoadout` fields).

### Persisting live equips

The effect in the game and the stored state are made by the same step: the DGS asks sc-entitygraph to attach or detach
the item before it changes its own tree and binds anything, so nothing the player sees is missing from the graph, and a
rejection (a class that does not fit, an item that is not the player's) fails the request. This is the existing
`AdminEquipItem`, `AdminUnequipItem` and `AdminMoveItem`, which `Plan::equip`/`unequip`/`moves` apply in one transaction
and record once per command id. They are named for the admin portal but are the commands of any caller; the `actor` the
DGS gives is `dgs`. The seam is `equip::ItemStore`; `GraphItems` is its sc-entitygraph implementation and the tests use a
fake.

- **Idempotency**: the command id is `live:<replicant>:<session>:<player>:<request id>`: a retry of the same request is a
  replay, and a request id is not reused within a session.
- **Disconnect**: nothing to flush: every change is stored as it is made. `LeavePlayer` only drops the DGS's tree, and the
  `ReleasePlayer` path (docs/DISCONNECT.md) clears the live markers as before.
- **Crash of the DGS** between the graph call and the bind: the graph has the change and the client was not shown it; the
  replicant rejoins the player (a new `boot_id`), and `EnsureLoadout` rebuilds the worn tree from the graph, so the player
  sees what is stored. A crash before the call changes nothing, and the client's request is lost with it (the replicant
  answers nothing; the client reconnects).
- **Join**: `EnsureLoadout` answers the loadout's GEIDs (0 for one taken off) and `equipped`, the rest of what is attached;
  the DGS binds both, so an equip made in a session is worn again at the next join.
- **Not stored yet**: what hangs from an item equipped in a session is bound only after the next join; dropped items are bound only to the client of the player who dropped them (not streamed to others, not rebuilt after a DGS restart); a failure of sc-entitygraph fails the request instead of being retried (the client can ask
  again).
- **Open**: a call that lets a DGS mark the change with the shard (so that the admin portal refuses it while the player is
  live, as for the other live entities) is not needed for worn items, which carry no shard id; revisit with item entities
  unstowed on a shard.


## Live inventory operations

The requests of a live player that are not an equip, a take-off or a move. The DGS (`services/dgs/src/equip.rs`) decodes the
request, asks sc-entitygraph for the change with `InventoryOperation` (`ItemStore`: `stack`, `stack_all`, `split`, `swap`,
`repool_ammo`, `drop_item`, `pickup`), and answers the client with the result bit and an item. Each is one transaction
(`services/entitygraph/src/operations.rs`) applied once per command id `live:<replicant>:<session>:<player>:<request id>`
with its answer recorded, so a repeat gets the same one (`replayed`). The ownership, capacity and loop checks are those of
every other command; a stack that changed between the read and the write fails the request. Stack limits are the game's:
99, 999 for currency, 1 (no stacking) for a class that is not `stackable`.

| Type | What the DGS asks | Result item |
|---|---|---|
| Stack (5) | The item (`itemEntityId`, else the stowed item of `classId` in the source inventory) goes onto the fullest other stack of its class with room, in the target inventory (else its own); `amount` of it (0: all). An emptied stack is erased | the stack that took |
| StackAll (13) | The stacks of the source inventory (else the target) of `classId` (0: every class) are merged into as few as the limit allows | the first surviving stack |
| Split (4) | A new item (new GEID, same class and owner) takes `amount` from a stack, in the target inventory (else the stack's own); `amount` must be below the stack | the new item |
| Swap (6), SwapOutfit (11) | The item trades places with the worn item at `itemPortPath`, else with the entity in `unstowAggregateRootId`. Stowed with stowed exchange inventories; stowed with worn is `Plan::unequip` plus `equip` (the stowed item must fit the port, single); worn with worn exchange ports (both must fit; neither may hang from the other). What hangs from an item goes with it | the item asked for |
| StoreAndHide (8) | As Store/Move: a worn item is taken off (unbound with what hangs from it), a stowed one moved | as Move |
| Sort (3) | Nothing: the order is the client's display. Answered as completed | none |
| Drop (0) | A stowed item (or `amount` of a stack, as a new item) is unstowed on the DGS's shard, owned by no one, with `droppedBy`, `zoneId`, `position` and `orientation` of the player; the DGS binds it under the player's zone. A worn item is refused | the world item |
| Interaction with a port path ending in a body `inventory_pocket(_2)` *(inferred from the live log: the client sends it to put an item into the personal storage; the port is `inventoryinternalonly`, so nothing is worn)* | The item (`itemEntityId`, else the stowed item of `classId` in the source inventory) is moved (`amount`, 0: all) from the source inventory to `<player>:PlayerInventory:0`; one already in it stays; a worn item is taken off into it. Nothing is bound | the item |
| Interaction with a port path of a wearable port, the item a stack | One is split off the stack (command id `<id>:split`, into the stack's inventory) and that new item is equipped (`<id>:equip`); when equipping it fails it is stacked back (`<id>:undo`). Sequential, not one transaction: a DGS crash between the steps leaves a stack of one next to the stack | the new item |
| Interaction with an entity and no port | Pickup: an item dropped on this shard (`droppedBy` set, owner none or the player) is stowed in the target inventory (else the personal one), owned by the player, and unbound. An entity that is no such item (NOT_FOUND) leaves the request completed without a change, as before | the item |
| AmmoRepool (12) | The rounds (`ammoCount`) of the stowed magazines of `classId` in the inventory are poured into as few magazines as possible, fullest first. Capacity is the class's `ammoCapacity` when the seed has it (it does not yet), else the fullest magazine; magazines with no count are left alone | the fullest magazine |

Binding after a swap: the swapped worn items are unbound with what hangs from them (children first) and every item the
graph says is now worn (`placed`, parents first, with the children that moved) is bound under its parent with the port's key.

Not verified in game, and inferred: which request field names the second item of a Swap/SwapOutfit, what SwapOutfit moves
beyond Swap, the meaning of `amount` and of the target in Stack/Split, that a dropped item bound with the item flags and no
port shows in the world, that a pickup is an Interaction with the world entity and no port, and what AmmoRepool's fields mean
(the layout of the common fields is decoded, see the protocol notes). Dropped items have no owner and so are not in any
admin tree; nothing streams them to other clients or rebinds them after a DGS restart yet. Seeding the magazine size
(`ammoCapacity`) needs the sc-admin seed export.

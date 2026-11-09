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
- A `Container` inventory is created with every created entity whose class has an inventory container: a ship's cargo
  grids, crates, backpacks. Stowing into `<entity>:Container:0` of an existing entity whose class is not known also
  creates it, infinite; a class known to have no container gets none.
- A `CreateCommand` with an inventory id stows the entities of each graph that nothing in the graph holds there,
  with stowed edges, so the tree walks find them.
- A move's `target_inventory_lazy_creation` and `CreateInventoryCommand` work as the client sends them; the context,
  subject, name and owner it leaves out are filled from the id.

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
- **Live**: the DGS has authority. The client asks with `pi.RmAuthorityProcessInventoryManagementOperation`, request type
  Interaction (a port to inventory move) or Store; the DGS decodes `itemEntityId` and `itemPortPath`, checks the same way
  against its own state, and tells sc-entitygraph with a `Move`/`Attach`/`Detach` command carrying the port CRC (the commands
  exist; `Plan::equip` and `Plan::unequip` are their equivalents). **Not done**: the request is not decoded beyond its
  head (see the protocol notes, "Inventory requests of the player"), so the DGS answers "completed" and nothing is stored.
  The game tests it needs: equip a held pistol and a stowed torso piece in the client, log the request bytes, and
  compare with `itemEntityId`/`itemPortPath`.

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

# Ships

A player's ships ("hangar" ships): how they are kept, listed at an ASOP kiosk, claimed after a loss and retrieved into
the hangar. First slice; what is inferred or needs the game is marked, and listed in TODO.md.

## What the game does (4.10.2, read from the client and the game data)

- The kiosk is `CEntityComponentShipInsuranceProvider` on an entity of the hangar/ATC area. Its authority (the DGS) runs the
  work through remote methods `RmAuthority{StoreVehicle, Incinerate, PayImpoundedFee, Deimpound, RequestDeliver,
  ExpediteDelivery, ClaimWindowOpened}`; the client side only shows the UI (`SCEvtNotify_ShipSelectorOnShipsListReceived`).
- The server side asks the backend: ownership and the snapshots of a ship (`QueryEntitlements`,
  `QuerySnapshotsWithEntitlements`: entitlement v2, snapshot slot DEFAULT or CUSTOMIZED), delivery timers and expediting
  (`delivery`: `RescheduleDelivery`), and the impound state (a "free form variable"). Whether a ship is stored is read from
  the entity (`GetInformation`: "ship persistent inventory", "is ship stored", the ATC and its landing area). So the real
  list is the entitlement (ownership, insurance, loadout) joined with the entity (where it is, whether it exists).
- States seen in the client's messages: stored, retrieving (`RequestVehicle called while already retrieving`), claim
  pending / claim chain with a timer (`Claim chain will expire in $$s`), deliver pending, impounded, destroyed.
- Game data: each ship class has `shipInsuranceParams` (`baseWaitTimeMinutes`, `mandatoryWaitTimeMinutes`,
  `baseExpeditingFee`; the Avenger Titan: 4.05, 1.35, 2343). `ItemRecoveryEconomyParams`: `claimCostMultiplier` 0.003,
  escalating claim chain limit 1 and every escalation multiplier 1.0. A ship class's `VehicleComponentParams.maxBoundingBox`
  (Avenger Titan 15 x 20 x 6.5 m) is compared with the `LandingPadSize` records (Tiny 12x16x6, XSmall 24x32x12, Small 48x48x16,
  Medium 56x88x18, Large 72x128x36, XLarge 160x272x64). Hangars are `Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`
  (`InstancedInteriorSizeEnum`). `LandingAreaComponentParams.alwaysLowerPlatformBeforeSpawn` and `CSCLoadingPlatformManager`
  (states ClosedIdle, LoweringPlatform, OpenIdle, RaisingPlatform) are the ship platform.

## Which store answers the kiosk (decision)

The user's rule stands: **in-game queries read the entity graph**; the document store is the durable record, written first.

Evidence that the game itself splits it this way: the real list needs the ownership and insurance (durable, entitlement)
and the physical state (entity). Neither store alone has both, so rows come from the graph and are cross-checked with the
record:

| Field | Source of truth | Why |
|---|---|---|
| exists, owner, class, insurance policy, claim timer and cost, price | the record (sc-entity-document) | must survive losing the graph; written at acquisition |
| state (stored/deployed/destroyed/claiming/expired), location, pad size | the record, copied to the graph | one writer: every change is made in the record first, then the graph |
| the entity, its ports and inventories | the graph | only the graph has them |

On a disagreement **the record wins**. Detected on every list: the graph row's `docRevision` differs from the record's
revision, the record is not `graph_synced`, or the entity/row is missing. The list then rewrites the graph from the record
(`repaired` in the answer). A graph ship with no record is an orphan: not listed, counted (`orphans`). If the records cannot
be asked, the graph answers alone (logged): the game keeps working. Cost: one extra gRPC read per list.

## Data model

- Record: `scbackend.internal.common.v1.ShipRecord`, kept by sc-entity-document in its own table `ship_records`
  (`ShipRecordsInternal`: `RecordShip` once per command id, `GetShips`, `ReplaceShip` at an expected revision). It is not
  one of the game's document classes (those are the game's protocol).
- Graph: the ship is an entity of its class, owned by the player, stowed in `<owner>:Hangar:<location id>` (the game's
  `Hangar` inventory context, owner the holder; inferred as the ship bank); beside it a `ships` document keyed by the ship's
  GEID with the record whole (hex) and the filter fields. GEIDs come from the item counter.
- `ShipRecord.pad_size` is the smallest `LandingPadSize` the class's bounding box fits (turned too, inferred). A class with
  no size seeded gets UNSPECIFIED and is listed for no hangar (a warning says so). Seed: `entity_classes.vehicle.maxBoundingBox`
  and `.insurance.{baseWaitTimeMinutes, mandatoryWaitTimeMinutes, baseExpeditingFee}` (not seeded yet, see TODO.md); until
  then the acquirer passes `pad_size` and `insurance`.

## Acquisition (sc-entitygraph `ShipsInternal.AcquireShip`, idempotent by command id)

1. Record in sc-entity-document (`ship_uid` = `ship:<command id>`). If this fails nothing was acquired.
2. GEID allocated and written into the record (compare and swap on its revision), so a retry makes the same entity.
3. Entity and `ships` row in one ArangoDB transaction.
4. Record marked `graph_synced`, the row updated.

A failure after 1 leaves a durable record not `graph_synced`; the next `ListShips` (or any later acquire) finishes it. A
repeat of the command returns the first result with `replayed`. Every later change (`ClaimShip`, `RetrieveShip`,
`ReportShip`) has the same order: replace the record at the revision read (ABORTED if it moved), then the graph, then mark
synced; `last_command_id` makes a repeat change nothing.

Callers of acquisition (to wire): the purchase flow when a ship entitlement is granted (`GrantPurchase`, hub claim of
entitlements), a rental starting (EXPIRED when it ends). None calls it yet.

## States

```
STORED --retrieve--> DEPLOYED --report(stored, location)--> STORED
                        |--report(destroyed)--> DESTROYED --claim(default|insured)--> CLAIMING --ready_at--> STORED
EXPIRED: a rental that ended / an entitlement revoked; kept, never retrievable
```

CLAIMING becomes STORED when `claim.ready_at_ms` passes; this is computed when read (no timer to lose). Claim kinds:
DEFAULT (snapshot slot DEFAULT, no policy needed) and INSURED (slot CUSTOMIZED, needs a LIFETIME policy or a DURATION one
not expired). Time = the class's `baseWaitTimeMinutes`; it can be paid to finish early from `mandatoryWaitTimeMinutes` on
(`expedite_after_ms`; the early finish itself is not built); cost = price x 0.003. Both kinds take the same time and cost
(inferred: the default-loadout multipliers of the game data, 200 and 100, are not understood). The claim chain does not escalate
(limit 1, multipliers 1.0).

## Hangar size matching

A ship fits a hangar when its pad class is not larger than the hangar's (Small = pad Small, and so on; Tiny, XSmall and Small
ships fit a Small hangar). `exact_size` restricts to ships of the hangar's own size. The kiosk lists, and `RetrieveShip` allows,
only fitting ships for the hangar of the kiosk used (FAILED_PRECONDITION otherwise). Whether the game puts a small ship in a larger
hangar is not known (inferred: it does, as the ATC logs `NoValidAreaSize` only for too small); `exact_size` is the other reading.

## Retrieve and the platform (DGS, `services/dgs/src/ships.rs`)

`Kiosk::retrieve`: sc-entitygraph approves (STORED, fits; state becomes DEPLOYED) -> `RequestOpen` (the platform lowers) ->
after the open time (33 s, small hangars) the platform is `OpenIdle` and the ship entity is spawned on it -> `RequestClose`
(it rises, 31.6667 s) -> at `ClosedIdle` the ship is in the bay. A spawn that fails, or `Kiosk::abort` (the client left
mid-retrieval), stores the ship again (`ReportShip`); abort also rests the platform. One retrieval at a time, and none
while the platform moves. The medium, large and XL durations are not read and use the small ones.

The platform on the wire is `services/dgs/src/platform.rs` (findings: the RE repository's `netcode/asop_ships.md`, "4.10.2
re-derivation"). The manager (`CSCLoadingPlatformManager`) has `currentPlatformState` (key 0xdf5419be) and
`plannedEndPlatformState` (0xe90238ce), enum type 0xc8ab346b, one byte; the platform is an `SCItemDoor` whose
`Door_OpenState` (0x7895447b, type 0x2cf99cab) drives the manager (Opened 3 -> OpenIdle, Closed 0 -> ClosedIdle). Sent
through the outbox to the hangar's client, in this order (door first, then the manager):

| Step | Door_OpenState (platform) | manager current / planned |
|---|---|---|
| RequestOpen | 5 Opening | 2 / 3 |
| open time over | 3 Opened | 3 (OpenIdle) |
| RequestClose | 2 Closing | 6 / 7 |
| close time over | 0 Closed | 7 (ClosedIdle) |

Entity ids: the hangar shell's first static id plus the per-size indices of `platform::LAYOUTS` (small front: manager 70,
platform 74; Orison's shell starts at the hangar's id + 10, so + 80 and + 84; the older + 133 / + 131 were
`hangar_smlfrnt_002`'s). The shell is found among the hangar's containers by its static count. The table is a const, not
a generated gamedata table: producing it needs the converter to read each shell's `.soc` for the manager and platform
classes, which `tools/gamedata-build` does not do yet (the fallback is the proven table).

Assumed, for the game test: the absolute variable indices of an `EntitySnapshotUpdate` are not known, so an update is built
only with `SC_DGS_PLATFORM_INDICES=current,planned,door`; without it each change is logged (`platform variable not sent`,
with key, type and value) and the platform runs on its clock. Whether the manager runs authority on a replicated entity, or
the client computes it from the door, and whether an update reaches a client-made static (it does not: `Snapshot Update For
Missing Entity`) are open; `platform::ANNOUNCE_MANAGER` also sends the manager's variables. Look in the DGS log for
`platform variable sent` / `not sent`, and in the client's `Game.log` for `Snapshot Update For Missing Entity` on the
manager or platform id, and for `RequestVehicle called while already retrieving`.

## Who owns what

- sc-entity-document: the record. sc-entitygraph: the entity, the bank, the `ships` row, the rules, the transitions.
- The DGS: everything about a retrieved ship once live (it has authority), the kiosk component and the platform, so
  `Kiosk` lives there. The replicant (pu-frontend) only relays the client; it has no part.
- sc-entitygraph asks sc-entity-document over `ShipRecordsInternal`; the DGS asks sc-entitygraph over `ShipsInternal`. No
  service reads another's tables.

## Not done / needs the game

See the list in TODO.md ("Ships"): the kiosk's remote methods and their arguments, the platform's entity variables, the
ship entity's spawn with its loadout from the snapshot, the callers of acquisition, the seed of sizes and insurance, the
claim buttons of the portal's view.

## Portal

The admin portal's Ships tab (the old Hangar tab is part of it) lists a player's ships (`ListShips`, all locations), grants
ships (`AcquireShip`, command id `admin:<request id>:<n>`, home hangar from the home locations table, insurance
none/days/lifetime, optional pad size for a class the seed gives no size) and puts away or marks destroyed a deployed ship
(`ReportShip`); all refused while the player is on a shard. The same page has the vehicle entitlements and the rentals. See
docs/PORTAL.md.

### Cost of the calls

What a call costs in round trips, kept short because the portal makes them in a row for a grant:

- `AcquireShip`: 3 calls to sc-entity-document (record, GEID, synced), 1 class lookup by key (`DOCUMENT('entity_classes', guid)`,
  not a scan: the `guid` field has no index; a class stored under another key is still found by a scan), and 2 ArangoDB
  transactions (the entity and row, then the row alone with the synced record). It used to look the class up twice by a
  scan of every class and to check twice whether the entity existed.
- `ListShips`: the `ships` rows (index on `ownerId`) and the records (`GetShips`) are asked together; the records get 3 s,
  then the list answers from the graph alone and says so in the log. A list repairs only the ships whose graph row is at
  another revision than the record, so a list after a list repairs nothing.
- sc-entitygraph reaches sc-entity-document with a 3 s connect and a 10 s call timeout (`--entity-document`, default the
  fully qualified `http://sc-entity-document.scbackend.svc:50051`: a short name does not resolve from another namespace, and an
  unresolvable name made every call wait for the resolver).

## The kiosk's ATC link (why the ASOP shows "no ATC link")

Seen on a running client (4.10.2): the first press on the kiosk of the home hangar (`ShipSelectorKiosk_Hangar_HighTech_1_a-032`,
entity `hangar + 177`, in `hangar_smlfrnt_001.socpak`) logs `CEntityComponentShipInsuranceProvider::OnRequestOpen ... [NO ATC] ...
NO LINK TO ATC - Verify and export OC` and the UI does not open. The kiosk's entity variable `OwnerATCId` (key CRC-32 of
`owneratcid`) is 0. It is set only by the kiosk's handler of the component event `SEventATCDataManager_LinkWithATC`, which an
ATC entity (`SCAirTrafficControllerDataManager`, class `ATC_DataManager`) sends from its `Register*` functions; the id it
sets must name such an entity streamed in on the client. The hangar's socpaks contain no ATC and the kiosks no entity link,
and the client makes the hangar's statics itself as local entities, so nothing sends the event.

### What the evidence says (read from the 4.10.2 client)

- The ATC links its kiosks on the authority, not on the client: the ATC's `PostInitialize` (0x1451acd70) runs
  `LoadFromXml` (0x14515b7e0) only with authority; it resolves the exported super GUIDs of its `ShipInsuranceProviders`
  and calls `RegisterInsuranceProvider` (0x1451db260), which sends `LinkWithATC` to each kiosk, whose handler writes
  `OwnerATCId`. That variable is replicated (registered with flags 5, serializer `Struct EntityId m_value uint64`,
  0x728ae36a, same type as the zone host), so a client learns the link as a variable of the kiosk, and never from the
  ATC. The kiosk calls `RegisterInsuranceProvider` back on its ATC when it is given the link.
- An `EntitySnapshotUpdate` for an entity id the client made itself is rejected: 0x1415ad7c0 finds the entity in the
  replication view's own bind table (0x141579740) and logs `Snapshot Update For Missing Entity`. So the client-made
  kiosk (a static of `hangar_smlfrnt_001`, authority flag `0x10`, entity id hangar + 177 for kiosk 032) cannot get
  `OwnerATCId`, and the variable's index in an update is never needed.
- On a client that does not have authority over the kiosk (a replicated one), `OnRequestOpen` takes the other half
  of 0x144c25cb0: with the ATC linked and streamed in it fills the kiosk's UI and the ATC's data. With authority (the
  client-made kiosk) it runs the server half, and wants the player's landing zone inventory next (below).

### What the DGS sends (sc-dgs 0.5.0-rc.3, `atc.rs`, switch `SC_DGS_KIOSK_ATC`, on unless `0`/`false`/`off`/`no`)

For a home hangar whose content has `hangar_smlfrnt_001` (the content table lists it for the hangar socpaks 427479476, Orison, and 2223157161, New Babbage; only Orison is read) the join binds, after the hangar's content:

1. The ATC: entity id `HangarIds::end`, class `ATC_DataManager` (CRC 1225794389), flags `0x1000`, under the hangar.
2. A replicated copy of each of the container's three kiosks (`-021`, `-020`, `-032`) at the same place (ids ATC + 1..3),
   class CRC 1689243590, flags `0x1000`, with one variable, `OwnerATCId` (key `0xf9df8d76`, type `0x728ae36a`, the ATC's
   id as 8 bytes). A bind carries variables by key and type, so no variable index is involved.

The client's own kiosks stay (the `ocIdRange` of the hangar cannot be cut without losing its other 208 statics), so
at the same place there are two kiosks. Which one a press reaches shows in the client's log by its entity id.
The DGS logs (`info!`) both ids of each kiosk at the join, and every call on an entity it does not know with the
raw argument bytes in hex (`args_hex`), which is where the kiosk's remote methods arrive.

Not done, and why: the ATC has no `LoadFromXml` on a client, and its landing zone inventory (below) and the kiosk's
remote methods (`RmAuthorityFetchPrices`, `RmAuthorityRequestClaim`, `RmAuthorityStoreVehicle`, `RmAuthorityRequestdDeliver`,
`RmTakeSnapshot`, `RmAuthoritySetInteractionState`, ...; their indices follow the registration order of the whole
entity and are not counted) are not answered; `ships.rs` is still not wired to them.

### First game test (sc-dgs 0.5.0-rc.3) and what follows

The press reached the client's own kiosk (`[NO ATC] ... [226553665713]`, `Streamable Immovable`), not the copy
(226553665868); the binds went out without error and no call on the copy was logged. So the overlap is real and
the client picks its own kiosk.

Binding the kiosk at its own id is not the elevator gateways' pattern: those prefabs are sent **no** `ocIdRange`, so the client
makes none of their statics and their ids are free. The hangar's `hangar_smlfrnt_001` has an `ocIdRange` of its 209 statics
and cannot lose it (a shorter range does not skip an entity: `CreateImmovableEntities` makes all and gives id 0 to those past
the range). A second entity with a used id is not known to be accepted: `AddEntityInternal` (0x1467b5750) treats a duplicate
id in one batch as fatal, and the remote bind's own check (0x1415995a0) looks only at replicated ids. `SC_DGS_KIOSK_ATC=local`
binds the kiosks at the client's ids (226553665614, 226553665619, 226553665713 for hangar 226553665536) as an experiment, off by
default; the DGS logs a warning. Expect either the client's `Entity already bound`/duplicate error or a close; a pass would show
`[NO ATC]` replaced by the `ATC [..] not streamed in` / `[NO INVENTORY]` lines above with the local id.
The way that is known to work is the gateways': no range for `hangar_smlfrnt_001` and a bind of each of its 209 statics
(class, pose, geometry) from a table like `transit_prefab_entities.tsv`; not done.

### The next hurdle: the landing zone inventory

After the link `OnRequestOpen` asks the player (`ISCPlayer`, slot 0x740) for its inventory at the ATC's location
(0x145122050; the ATC's location id is 0x14511a420). `[NO INVENTORY] ... doesn't have an inventory. Setup issue? Check if
location is landingZoneInventory.xml` is logged when there is none: the location must have a `landingZoneInventory`
(a `StarMapObject`/`SSolarSystem` field in the DataCore, and `PlayerInventory.landingZoneInventory`) and the player must
hold an inventory for it. That inventory is not modelled (entitygraph/DGS) yet.

### Game test checklist (client `Game.log`, DGS log)

DGS, at the join: `binding the hangar's ATC and kiosk copies` and three `a replicated copy of the kiosk` lines with
`copy=` and `local=` ids. Nothing of this means the switch is off or the hangar has no `hangar_smlfrnt_001`.

Client, at the press on the kiosk by the pad:
- `Snapshot Update For Missing Entity`: not from this change (no update is sent).
- `[NO ATC] ... NO LINK TO ATC` with the kiosk id `= local`: the press reached the client-made kiosk, the copy was not hit
  (or not visible). Try the other kiosk, or look at the entity name in the log; then the copy needs to be put where
  the local one is not interactable (open).
- `[NO ATC] ... NO LINK TO ATC` with the id `= copy`: `OwnerATCId` was not applied (key or type not registered on a
  replicated bind: send a report with the `Snapshot` lines of the bind).
- `[NO ATC] ... ATC [..] not streamed in`: the link arrived and the ATC entity is not (or has no
  `SCAirTrafficControllerDataManager` component): the ATC bind failed (look for bind errors of its class).
- `[NO INVENTORY] ...`: the link works and the ATC is accepted; the next piece is the player's landing zone inventory.
  The line prints the ATC's location id and name.
- No `[NO ...]` line, and kiosk UI events (`ShipSelector` events, `SCEvtNotify_ShipSelectorOnShipsListReceived`):
  the kiosk is open; the kiosk's calls then appear in the DGS log as `a call on an entity that is not a call panel`
  with `entity=<copy id>` and `args_hex`.


## The kiosk's ship list is the client's own query (4.10.2)

The DGS does not push the ASOP's list. `CEntityComponentShipListProvider::OnRequestFetchVehicles` starts five futures and
joins them (`FUN_14567d390`, "Building ASOP Vehicle List"); the details and what is proven are in the protocol notes
(`asop_ships.md`, "Ship list: the five futures"). What the entity graph answers:

- Two `EntityQuery` calls with `inventory_id` (the client's `CInventoryApi::AsyncQueryInventoryAutoPagination`, the id made
  of `SInventoryId` {holder, context, subject} as `<holder>:<context>:<subject>`): the player's `Location` inventory (context
  kind 3) and `Hangar` inventory (kind 8) at the location id the client computes from its local data. They are the same calls
  as any other inventory read (tree projection, `STOWED_IN` edges, see INVENTORY.md). A ship is an entity stowed in
  `<owner>:Hangar:<location id>`, so the hangar query finds the player's ships there.
- **Made on first need** (`read::ensure_player_inventory`): the first `EntityQuery` of the caller's own `Location` or
  `Hangar` id creates it (infinite, owned and held by the player) instead of failing; other players' ids, other contexts
  and calls without a token make nothing. Logged as `player inventory made on first query`.
- **Cross-check on a hangar read** (`Graph::prepare_player_inventory` -> `Ships::synced_rows`): the same reconciliation as
  `ListShips`, the record (sc-entity-document) wins and the graph row and entity are rewritten when they differ, three
  seconds at most for the records, then the graph answers alone. Logged as `hangar read for the ship list` with the number of
  ships, repaired and orphans (an orphan is still in the inventory the query reads; it is counted, not hidden).
- **Not done here, open**: the hangar size filter (`fits`) is not applied to this query, since an inventory query carries
  no hangar size (the client filters with the kiosk's `MaxShipDockingClass`, `g_vehicleFilterFromASOP`); `RetrieveShip`
  still checks it. The states and timers the UI shows (stored, retrieving, impounded, claim, deliver, destroyed) are not in
  the inventory query: the client takes them from the `SQueryServerInfos` answer, which comes from the player's
  `CInsuranceComponent` on the authority (the DGS), and from the entitlements. The `ShipRecord` states and claim timers are
  ready for that answer (`rules::at`), but the DGS side and the entitlement/stow-context side (below) are not built.
- Entitlements: `QueryEntitlements` (v2) of the player's vehicle entitlements and `GetEntityStowContextsByParentUrns`
  (parent URN = the entitlement URN) are the likely fifth future (inferred). The join compares the stow context of each ship
  with what the inventory queries found (entity id, entitlement URN, class CRC, owner id, inventory id, is-stowed, shard id)
  and logs `Mismatch information found for stow context ...` otherwise: ships therefore need an entitlement whose URN is the
  stow context's parent for the list to take them as consistent.

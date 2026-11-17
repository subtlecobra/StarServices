# Ships

A player's ships ("hangar" ships): how they are kept, listed at an ASOP kiosk, claimed after a loss and retrieved into
the hangar. First slice; what is assumed or needs the game is marked, and listed in TODO.md.

## What the game does (4.10.2, read from the client and the game data)

- The kiosk is the kiosk's insurance component on an entity of the hangar/ATC area. Its authority (the DGS) runs the
  work through remote methods `RmAuthority{StoreVehicle, Incinerate, PayImpoundedFee, Deimpound, RequestDeliver,
  ExpediteDelivery, ClaimWindowOpened}`; the client side only shows the UI (the ship list received event).
- The server side asks the backend: ownership and the snapshots of a ship (`QueryEntitlements`,
  `QuerySnapshotsWithEntitlements`: entitlement v2, snapshot slot DEFAULT or CUSTOMIZED), delivery timers and expediting
  (`delivery`: `RescheduleDelivery`), and the impound state (a "free form variable"). Whether a ship is stored is read from
  the entity (`GetInformation`: "ship persistent inventory", "is ship stored", the ATC and its landing area). So the real
  list is the entitlement (ownership, insurance, loadout) joined with the entity (where it is, whether it exists).
- The states the client names: stored, retrieving (`RequestVehicle called while already retrieving`), claim
  pending / claim chain with a timer (`Claim chain will expire in $$s`), deliver pending, impounded, destroyed.
- Game data: each ship class has `shipInsuranceParams` (`baseWaitTimeMinutes`, `mandatoryWaitTimeMinutes`,
  `baseExpeditingFee`; the Avenger Titan: 4.05, 1.35, 2343). `ItemRecoveryEconomyParams`: `claimCostMultiplier` 0.003,
  escalating claim chain limit 1 and every escalation multiplier 1.0. A ship class's `VehicleComponentParams.maxBoundingBox`
  (Avenger Titan 15 x 20 x 6.5 m) is compared with the `LandingPadSize` records (Tiny 12x16x6, XSmall 24x32x12, Small 48x48x16,
  Medium 56x88x18, Large 72x128x36, XLarge 160x272x64). Hangars are `Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`
  (`InstancedInteriorSizeEnum`). `LandingAreaComponentParams.alwaysLowerPlatformBeforeSpawn` and the loading platform manager
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
  `Hangar` inventory context, owner the holder; assumed as the ship bank); beside it a `ships` document keyed by the ship's
  GEID with the record whole (hex) and the filter fields. GEIDs come from the item counter.
- `ShipRecord.pad_size` is the smallest `LandingPadSize` the class's bounding box fits (turned too, assumed). A class with
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
(assumed: the default-loadout multipliers of the game data, 200 and 100, are not understood). The claim chain does not escalate
(limit 1, multipliers 1.0).

## Hangar size matching

A ship fits a hangar when its pad class is not larger than the hangar's (Small = pad Small, and so on; Tiny, XSmall and Small
ships fit a Small hangar). `exact_size` restricts to ships of the hangar's own size. The kiosk lists, and `RetrieveShip` allows,
only fitting ships for the hangar of the kiosk used (FAILED_PRECONDITION otherwise). Whether the game puts a small ship in a larger
hangar is not known (assumed: it does, as the ATC rejects only a hangar too small); `exact_size` is the other reading.

## Retrieve and the platform (DGS, `services/dgs/src/ships.rs`)

The request is the kiosk's `RequestVehicle`: **one** call on the hangar's ATC, `RmAuthorityRequestPermission` (rel 3), a take-off
`SATCActionRequest` with `isSpawn` and the ship's geid as the vehicle. `retrieve.rs`
recognises it by its arguments (the ATC's absolute method indices are not read yet) and answers `RmMulticastNotifyPlayer`
(request index + 2) with type 3 `TakingOffAreaGranted` (9 `AlreadyGranted` for the ship already on the pad, 16 waiting when
the pad moves, 15 / 14 when it does not fit) naming the hangar's `LandingArea_ShipElevator_*` static as the token.

The platform has two rest states: **down** (`ClosedIdle`, below the floor) and **up** (`OpenIdle`, the loading gate open and a
ship on it at the floor). `Kiosk::retrieve`: sc-entitygraph approves (STORED, fits; state becomes DEPLOYED), then
- pad **up** with a ship on it: `RequestClose` (the pad goes down, 31.7 s) -> at `ClosedIdle` that ship's entity is unbound and
  its record is STORED again at this hangar (`ReportShip`) -> as below;
- pad **down**: the ship's entity is bound on the landing area (class from its entity class GUID, at the area's pose in the
  hangar's frame) -> `RequestOpen` (the gate opens and the pad rises, 33 s) -> at `OpenIdle` the ship is ready and the ATC
  token is set occupied (`RmMulticastChangePermissionToken`, request index + 6). The bind's class CRC is the seed's
  `guidHashCrc` (CRC-32C of the GUID's 16 bytes, each half reversed, `loadout::class_crc`), not the CRC-32C of the GUID's
  text: with the text's one the client reports `No Entity Class` / `UNKNOWN ENTITY CLASS` and the ship never appears
  (DRAK_Marauder is 0x34c5df90, against 0x0c612965 from the text).

A spawn that fails stores the new ship again (the pad never moved); `Kiosk::abort` (the client left mid-retrieval) stores it
again too and rests the platform down. One retrieval at a time, and none while the platform moves. How long the platform
takes each way comes from the game data's `tables/platform_times.tsv` (docs/DATA.md): the slower of the pad and gate doors'
animations for the hangar class, plus a margin (the small hangar: 13 s down, 13.5 s up), as the doors end the manager's state and its
lights; a class the table lacks keeps those small-hangar times (`PLATFORM_OPEN_TIME`, `PLATFORM_CLOSE_TIME`). (Until this order was fixed the model lowered first and spawned at `OpenIdle`,
the reverse of the manager's own states.)

Lights and sounds: nothing is sent for them. The light group of the platform and the platform's audio follow the manager's state
on the client (the manager's light transition; no light or audio variable is replicated), so driving the door and the manager's
`currentPlatformState` / `plannedEndPlatformState` below is what makes them follow, if the client's manager reacts to a replicated
update (open, see below). The loading gate's own `Door_OpenState` follows by default (`SC_DGS_PLATFORM_GATE=off` stops it) and with the fourth value of
`SC_DGS_PLATFORM_INDICES` (`current,planned,door,gate`).

The platform on the wire is `services/dgs/src/platform.rs`. The manager (the loading platform manager) has `currentPlatformState` (key 0xdf5419be) and
`plannedEndPlatformState` (0xe90238ce), enum type 0xc8ab346b, one byte; the platform is an door whose
`Door_OpenState` (0x7895447b, type 0x2cf99cab) drives the manager (Opened 3 -> OpenIdle, Closed 0 -> ClosedIdle). Sent
through the outbox to the hangar's client, in this order (door first, then the manager):

| Step | Door_OpenState (platform) | manager current / planned |
|---|---|---|
| RequestOpen (pad rises, ship on it) | 5 Opening | 2 / 3 |
| open time over (pad up) | 3 Opened | 3 (OpenIdle) |
| RequestClose (pad lowers) | 2 Closing | 6 / 7 |
| close time over (pad down) | 0 Closed | 7 (ClosedIdle) |

Entity ids: the hangar shell's first static id plus the per-size indices of `platform::LAYOUTS` (small front: manager 70,
platform 74; Orison's shell starts at the hangar's id + 10, so + 80 and + 84; the older + 133 / + 131 were
`hangar_smlfrnt_002`'s). The shell is found among the hangar's containers by its static count. The table is a const, not
a generated gamedata table: producing it needs the converter to read each shell's `.soc` for the manager and platform
classes, which `tools/gamedata-build` does not do yet (the fallback is the fixed table).

Assumed, for the game test: the absolute variable indices of an `EntitySnapshotUpdate` are not known, so an update is built
only with `SC_DGS_PLATFORM_INDICES=current,planned,door[,gate]`; without it each change is logged (`platform variable not sent`,
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

## Removal (sc-entitygraph `ShipsInternal.RemoveShip`, idempotent)

Takes `command_id`, `owner_id`, `geid`, `actor`, `reason`. A ship that is DEPLOYED (spawned in the world) is refused
(FAILED_PRECONDITION). Otherwise, in this order: the durable record is deleted (sc-entity-document
`ShipRecordsInternal.DeleteShip`, first, as the source of truth), then the ship's entity with everything it holds is erased
(the admin removal, which also refuses anything live on a shard) and its `ships` row. A removal that failed after the record
went is finished by a repeat, which finds the row without a record. A ship with no record, entity or row answers
`already_removed`; one of another owner is NOT_FOUND. The response carries the record's `entitlement_urn`: the portal and the
API revoke it first (sc-entitlement `RevokeEntitlement`, command id `<request key>:entitlement`, a repeat changes nothing),
so a failed revocation keeps the ship and a retry finishes. Portal: the Remove action on the Ships tab
(`POST /admin/accounts/{id}/ships/remove`: `request_id`, `geid`, `reason`), audited as `ship.remove` with the ship's state
before; also `POST /admin/api/v1/accounts/{id}/ships/remove` (see API.md).

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

On the 4.10.2 client the first press on the kiosk of the home hangar (`ShipSelectorKiosk_Hangar_HighTech_1_a-032`,
entity `hangar + 177`, in `hangar_smlfrnt_001.socpak`) logs `OnRequestOpen ... [NO ATC] ...
NO LINK TO ATC - Verify and export OC` and the UI does not open. The kiosk's entity variable `OwnerATCId` (key CRC-32 of
`owneratcid`) is 0. It is set only by the kiosk's handler of the component event the ATC link event, which an
ATC entity (`SCAirTrafficControllerDataManager`, class `ATC_DataManager`) sends from its `Register*` functions; the id it
sets must name such an entity streamed in on the client. The hangar's socpaks contain no ATC and the kiosks no entity link,
and the client makes the hangar's statics itself as local entities, so nothing sends the event.

### What the evidence says (read from the 4.10.2 client)

- The ATC links its kiosks on the authority, not on the client: the ATC's `PostInitialize` runs
  `LoadFromXml` only with authority; it resolves the exported super GUIDs of its `ShipInsuranceProviders`
  and calls `RegisterInsuranceProvider`, which sends `LinkWithATC` to each kiosk, whose handler writes
  `OwnerATCId`. That variable is replicated (registered with flags 5, serializer `Struct EntityId m_value uint64`,
  0x728ae36a, same type as the zone host), so a client learns the link as a variable of the kiosk, and never from the
  ATC. The kiosk calls `RegisterInsuranceProvider` back on its ATC when it is given the link.
- An `EntitySnapshotUpdate` for an entity id the client made itself is rejected: it finds the entity in the
  replication view's own bind table and logs `Snapshot Update For Missing Entity`. So the client-made
  kiosk (a static of `hangar_smlfrnt_001`, authority flag `0x10`, entity id hangar + 177 for kiosk 032) cannot get
  `OwnerATCId`, and the variable's index in an update is never needed.
- On a client that does not have authority over the kiosk (a replicated one), `OnRequestOpen` takes the other half
  of its code: with the ATC linked and streamed in it fills the kiosk's UI and the ATC's data. With authority (the
  client-made kiosk) it runs the server half, and wants the player's landing zone inventory next (below).

### Merge mode (`SC_DGS_KIOSK_ATC=merge`, opt-in, failed in game as the default)

**Result (merge as the default, usa-030):** the user reported the kiosk gone. The DGS logged the ATC bind
and no error; the client log of the bot that joined the same shard
has no line about the kiosk at all, where a session before merge logged `[NO ATC] ShipSelectorKiosk_Hangar_LowTech_1_a-112`
on open. So the merged record did not become an entity the player can use, and the client gives no log of
why. Merge is therefore opt-in again (default `bind`, as it works for the kiosk), and `SC_DGS_TRANSIT_CAB_STATICS` defaults to `client`.
To retry on one deployment, set the shard operator chart's `dgsExtraEnv` (`SC_DGS_KIOSK_ATC: merge`). The same bot session
ended with a client crash, `CSerializedComponent::ReceiveDirtyValuesFromNetwork ... CAlignmentSlot::m_lockedByEntityId.0
streamSize=64 replicaSize=32` (a 64-bit id sent for a 32-bit variable), which merge may or may not cause.

The two modes above trade the kiosk's ATC link against the container's lights, doors and loading platform. `merge` is the
route derived from the client's `.soc` load: the kiosk container keeps its
`ocIdRange`, so the client makes every static from the `.soc` as in the default mode, and the DGS binds the ATC and **only
the kiosks**, each at the id the client gives its static (`range.first + index`), after the container in the same batch,
with flags `0x1800` (replicated `0x1000` plus `OBJECTCONTAINER_SPAWNED` `0x800`), the container as parent, the zone
attachment and the static's pose in the container's frame, and `OwnerATCId`. The flag makes the bind a record of the
container's content batch; when `ParseFromXml` reaches that id it keeps the record and merges the `<Entity>` XML into it,
without `LOCAL_ONLY 0x10` and without applying the XML's transform. The kiosk is then server-authoritative with its
`.soc` components. To check in game: the client log has no `[NO ATC]`, no `id collision ... with same id`, the lights are on,
and the DGS logs a kiosk call. Risks: the content batch may already be finished when the
`.soc` loads, so the statics might not spawn; a crash if the container is not pending in the same batch.

### Overlay mode (`SC_DGS_KIOSK_ATC=overlay`, opt-in, untested in game)

The aim is the kiosk bound and the hangar's lights, doors and loading platform intact, which `bind` (the default) cannot give: a
bound entity gets no `<Entity>` XML, and the `.soc` holds the lights' parameters and the platform manager's links. `overlay`
keeps the container's `ocIdRange` (the client makes every static) and binds the ATC and the three kiosks as in `bind` (flags
`0x1000`, parent the hangar, the pose in the hangar's frame, `OwnerATCId`) at the ids the client gives their statics, with no
`0x800`. Whichever entity of an id spawns second is rejected by `SpawnEntityImpl` (`id collision`, a loss of that entity only). If the bind spawns first the kiosk is bound and the rest of the hangar is the client's; if the
static comes first the kiosk is the local `[NO ATC]` one (as `off`). To check: the DGS logs a kiosk call, the client log has no
`[NO ATC]`, and `id collision` names the static (not the bind). Unlike `merge`, nothing joins the container's content batch, so
the vanishing of the merge default cannot repeat by that route. If the static wins the race, use `bind`.

### What the DGS sends (`atc.rs` and `location.rs`, switch `SC_DGS_KIOSK_ATC`, default `bind`; `off` and `merge` opt in)

Game test (Lorville's hangar, `ShipSelectorKiosk_Hangar_LowTech_1_a-112`, container 2684151953, 246 statics; switch on): the
ATC links and streams in and the press no longer logs `[NO ATC]`; it reaches the DGS as method 39 on the kiosk (51 bytes, retried
about once a second, unanswered: the method and its reply are not identified yet). The cost: a container the DGS binds itself
loses the per-instance data of its `.soc` entities (lights, doors, the loading platform manager, sequences), which no
`entSocpak`/`entCrc` can carry, so the hangar's doors and lights went
dark. The default is `bind` (the user wants the kiosk bound and visible); the lights, doors and platform are the open cost (`off` brings them back with `NO ATC LINK`). The shell container
of the rest stops (`hangar_smlfrnt_002`, 2136596536) is never bound this way, as its `.soc` holds the floor and walls.
The kiosk classes covered by the tables are all five `ShipSelectorKiosk_Hangar_*` ones.

The first try bound a replicated copy of each kiosk beside the client's own and the press reached the
client's own kiosk (`[NO ATC] ... [226553665713]`, `Streamable Immovable`; the copy at 226553665868 got no call). The overlap was
real and the client picks its own kiosk, so the copies are gone. What is sent now, for a home hangar whose content has
`hangar_smlfrnt_001` (socpak 2066649194; the content table lists it for the hangar socpaks 427479476, Orison, and
2223157161, New Babbage; only Orison is read):

1. The container is bound without `ocIdRange`/`ocIdRangeMov`, so the client makes none of its statics, and the DGS binds
   each of its 209 statics itself as a replicated entity (class, pose, the instance's geometry and material), at the id the
   client would have given it: the container's first static id plus the entity's index in document order. This is the
   gateway prefabs' route (`server_bound`; `tables/transit_prefab_entities.tsv` lists the container since gamedata-build
   0.2.2, as it holds a kiosk). Any other container with a kiosk is listed in the table too, and keeps its ranges.
2. The three kiosks (`-021`, `-020`, `-032`, class CRC 1689243590, indices 68, 73 and 167) carry `OwnerATCId` (key
   `0xf9df8d76`, type `0x728ae36a`, the ATC's id as 8 bytes), as the ATC's `LinkWithATC` would leave it.
3. The ATC: entity id `HangarIds::end`, class `ATC_DataManager` (CRC 1225794389), flags `0x1000`, under the hangar.

The container's 3 dynamics and 2 tile item ports (`entity_counts.tsv`) lose their ranges with it: what the three dynamics
are, and whether the tile ports' fire extinguisher cabinets still work, is not known. The DGS logs (`info!`) the ATC bind
at the join, and every call on an entity it does not know with the raw argument bytes in hex (`args_hex`), which is where the
kiosk's remote methods arrive. Not done: the ATC has no `LoadFromXml` on a client, and its landing zone inventory (below) and
the kiosk's remote methods (`RmAuthorityFetchPrices`, `RmAuthorityRequestClaim`, `RmAuthorityStoreVehicle`,
`RmAuthorityRequestdDeliver`, `RmTakeSnapshot`, `RmAuthoritySetInteractionState`, ...; their indices follow the registration
order of the whole entity and are not counted) are not answered; `ships.rs` is still not wired to them.

### The next hurdle: the landing zone inventory

After the link `OnRequestOpen` asks the player (`ISCPlayer`) for its inventory at the ATC's location. `[NO INVENTORY] ... doesn't have an inventory. Setup issue? Check if
location is landingZoneInventory.xml` is logged when there is none: the location must have a `landingZoneInventory`
(a `StarMapObject`/`SSolarSystem` field in the DataCore, and `PlayerInventory.landingZoneInventory`) and the player must
hold an inventory for it. That inventory is not modelled (entitygraph/DGS) yet.

### Game test checklist (client `Game.log`, DGS log)

DGS, at the join: `binding the hangar's ATC`. Without it the switch is off or the hangar has no `hangar_smlfrnt_001`.

Client, at the press on the kiosk by the pad:
- `Snapshot Update For Missing Entity`: not from this change (no update is sent).
- `[NO ATC] ... NO LINK TO ATC` with the kiosk id of the hangar's id + 177 (kiosk 032): the client still made its own kiosk
  (the container kept its ranges, or the table lacks it), or the bind was refused (look for bind errors).
- `[NO ATC] ... NO LINK TO ATC` with the replicated kiosk: `OwnerATCId` was not applied (key or type not registered on a
  replicated bind: send a report with the `Snapshot` lines of the bind).
- `[NO ATC] ... ATC [..] not streamed in`: the link arrived and the ATC entity is not (or has no
  `SCAirTrafficControllerDataManager` component): the ATC bind failed (look for bind errors of its class).
- `[NO INVENTORY] ...`: the link works and the ATC is accepted; the next piece is the player's landing zone inventory.
  The line prints the ATC's location id and name.
- No `[NO ...]` line, and kiosk UI events (`ShipSelector` events, the ship list received event):
  the kiosk is open; the kiosk's calls then appear in the DGS log as `a call on an entity that is not a call panel`
  with the kiosk's `entity` id and `args_hex`.


## The kiosk's ship list is the client's own query (4.10.2)

The DGS does not push the ASOP's list. the kiosk's list request starts five futures and
joins them. What the entity graph answers:

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
  the insurance component on the authority (the DGS), and from the entitlements. The `ShipRecord` states and claim timers are
  ready for that answer (`rules::at`), but the DGS side and the entitlement/stow-context side (below) are not built.
- Entitlements: `QueryEntitlements` (v2) of the player's vehicle entitlements and `GetEntityStowContextsByParentUrns`
  (parent URN = the entitlement URN) are the likely fifth future (assumed). The join compares the stow context of each ship
  with what the inventory queries found (entity id, entitlement URN, class CRC, owner id, inventory id, is-stowed, shard id)
  and logs `Mismatch information found for stow context ...` otherwise: ships therefore need an entitlement whose URN is the
  stow context's parent for the list to take them as consistent.

## Why the ASOP listed 0 ships (game test 2026-11-11, scbot, Lorville hangar)

The ships existed (`Hangar:4005457614` held an AEGS Javelin and an ANVL Lightning) but the client's log said `Found [0]
vehicle(s) at location`. The edge access log (Envoy) of the three presses shows what the client really sends within the 30 ms
of "Fetching player vehicle list": **no `EntityGraphService/EntityQuery` at all** (the entitygraph never logged the
`player inventory made on first query` / `hangar read for the ship list` lines the earlier notes expected), but
`ExternalEntityDocumentService/Fetch` (empty answer) and two `ExternalEntitlementService/QueryEntitlementsStream` (empty
streams). The only inventory query near it is the spawn-time `Location:<landing id>` read of the personal inventory manager
(4 reward items, no vehicle). So the ASOP's vehicle list is driven by the player's ship **entitlements**, and the admin's ship
grant made none (`AcquireShip` was called without `entitlement_urn`), hence nothing to list.

Change: the ships grant first grants each ship a
delivered `IngamePersistent` entitlement (`GrantPurchaseRequest.item_type/item_sub_type/name/delivered`, new fields; the
item type is the class's, `NOITEM_Vehicle` for ships) keyed by the ship's command id, and passes its URN to `AcquireShip`.
sc-entitlement-v2 now logs each `QueryEntitlementsStream` filter and count (`entitlements queried`): the next press shows the
exact filter the ASOP uses, which is **open** (if it filters on another item type string than the class's, set it there).
Ships granted before this change have no entitlement: grant them again (or the hangar page's new grant) to get one.

### Why it still listed 0: the kiosk's `LocationId` was 0

With the entitlements answered (2 ships, filter `item_type NOITEM_Vehicle`, both streams) the client still logged `Found [0]`
~40 ms after the fetch and sent no `EntityQuery`, no stow context call. From the client (4.10.2): the
inventory half of the fetch logs "Querying location
inventory for player [$$] at location [$$]" and "Querying hangar inventory ..." and makes the two `EntityQuery` calls **only if
the location id (u32) is not 0**, else it returns empty successes, and `BuildInventoryStowedAggregateRoots`
counts 0 (the roots of the location result, then the children of the roots of the hangar result). Neither "Querying ..." line
is in the client log. The location comes from the request, which takes it from the kiosk's replicated `LocationId` variable
(kiosk +0x9e8); the DGS set only `OwnerATCId`. The DGS now also binds `LocationId` (CRC-32 `locationid` = `0x03530ccf`, type
`uint32`) with the home location's id, the one the ships' `Hangar:<id>` inventory is named by. Check after deploy: the client
log shows `Querying location inventory ... at location [<id>]`, the entitygraph logs two `inventory entity query` lines for
`Location:<id>` and `Hangar:<id>` plus `hangar read for the ship list`, then `Found [N]`. Open: whether the variable is accepted
from the bind snapshot (it is registered with flags 1, `OwnerATCId` with 5), and whether the ships (direct children of the
`Hangar` inventory node) are counted as the hangar's children.

## The kiosk's other remote methods (`kiosk_calls.rs`)

Before the default became `bind` every kiosk call except the interaction (39) fell through to the cab panel handler, which answered nothing, so the
ASOP never got its ship list. A press on `ShipSelectorKiosk_Hangar_LowTech_1_a` (usa-020) sends, after the interaction is
answered with `ClOnInteract`, five calls:

| Method | Name | Arguments | DGS |
| --- | --- | --- | --- |
| 37 | `KioskInteraction::RmAuthoritySetUserId` | player id | logged |
| 10 | `RmAuthorityCleanUserClaimChains` | none | logged |
| 0 | `RmAuthorityRequestVehicleInfos` | request GUID (16), ATC id (8) | answered on the player |
| 9 (twice) | `RmAuthoritySetInteractionState` | two u32 | logged |
| 38 | `KioskInteraction::RmAuthorityAckClientConnection` | none | logged |

The answer to 0 is `RmTokenResponseFetchServerInfos` on the player: the request GUID echoed, then empty
`canDockHereList`, `destroyedVehicles` and `landedVehiclesInfos` and a trailing varint 0. The client's pending future for that GUID
resolves and the list builds from its own entity graph and entitlement queries (stored ships need no landed record). The
player's method index of the answer is **not known, so the reply is off by default**. Live facts: the Player has 265 methods (a
client logged "Invalid Remote Method Id index=361 numMethods=265", which refutes 361), 248 is `pml.ClSetMissionHasError` (it crashed a
client), the mission log (18 methods) is 238-255 and the service beacon (9) 256-264 (a peer's call 263 is its 8th). the player's
registrar ends at 214, so 215-237 (23 methods) hold the later DataCore components: WeaponAttachmentUI 3, PlayerTrade 4,
RestrictedAreaHandler 1, CargoInterface 4, UIBindingsInteractions 2 and Insurance 7 (21; CargoDebugHelper's 19 debug methods cannot fit
and are taken as absent), plus 2 methods not identified. Insurance is the last DataCore component, so the answer is **231** if the two
come before it, 230 if one does, 229 if both come after it (`INSURANCE_RESPONSE_CANDIDATES`). `SC_DGS_INSURANCE_RESPONSE_METHOD=<n>`
(chart `dgsExtraEnv`) turns the reply on with that index; `0` or `off` keeps it off. The DGS logs "ship list request answered" with the
method and the request GUID. A wrong index calls another handler on the client and can crash it.

## The ship list answer at the kiosk

The kiosk's `RmAuthorityRequestVehicleInfos` is answered on the player with `RmTokenResponseFetchServerInfos` (method 156 of
the player). The DGS builds it from the player's ship records (`ShipsInternal.ListShips`, every location, filtered to the
size of the kiosk's hangar): `canDockHereList` holds the class GUID of each ship that is stored or deployed and fits the hangar
(once per class), `destroyedVehicles` the entities of the destroyed ships, and `landedVehiclesInfos` stays empty, as the
official service does (where the one class listed was the player's Aurora MR entitlement). Without sc-entitygraph, or for a kiosk
whose hangar is not known, both lists are empty. The records of landed ships (`SLandedVehicleInfos`) are not sent: their
string and flag encodings are assumed only, and a wrong size kills the client.

### Prices

`RmAuthorityFetchPrices` (kiosk method 4: `u32` request number, varint count, that many class GUIDs) is answered to the caller
with `RmOnFetchPrices` (method 13: the number, the count, then each class asked in order with its price as an `f64`). The
price of a class is the `price_auec` of the player's ship of that class; a class that is none of the player's is 0, as
the official service does (it answered all twelve classes it was asked, one unknown with 0).

### Where a ship is stored in the graph (corrected)

The client's ship list asks for two things (as the entitygraph log of build 13165946 shows): the `Location:<id>` inventory for
`itemTypeEnum` 136 (`NOITEM_Vehicle`) items `STOWED_IN` it, and the `Hangar:<id>` inventory for type 106 (`Hangar`) items.
A ship entity is therefore a `NOITEM_Vehicle` stowed in the player's `Location` inventory (the same inventory as the item
bank), not an untyped item in a `Hangar` inventory as it first was. A ship made before this is moved to the location bank and
given the type the first time the hangar is read. The `Hangar` query, for hangar items that hold vehicles, finds nothing
yet.

### The platform's statics are merged, not client-made

The loading platform's manager, platform and loading gate are statics of the hangar shell (`hangar_smlfrnt_001`: manager 70,
loading gate 72, platform 74). Left to the client they are local entities, so the DGS's updates to them were rejected
(`Snapshot Update For Missing Entity`) and the retrieved ship spawned inside the client's own platform
(`SPAWN BLOOPER ... colliding with pent LoadingPlatform_...`). With `atc::Mode::Merge` (the default) they are now bound like the
kiosks (`location::merged_kiosk_binds`): at the id the client gives the static, parent the container, `STATIC_ATTACHMENT`, the
geometry statics with `MERGED_STATIC_FLAGS` and the manager with `0x100840` (the official service's flags for a static with no
geometry). They carry the resting state by key (manager `currentPlatformState` and `plannedEndPlatformState` = 7 `ClosedIdle`,
doors `Door_OpenState` = 0), so no index is needed in the bind. The manager's updates use the indices 1 and 2; the doors' `Door_OpenState` index is not verified.

`SC_DGS_PLATFORM_BIND=off` (also `0`, `false`) keeps the three client-made.

Not verified: that the client applies a bound initial state, that the merge makes the manager's XML nodes (`ObstructionCheckBounds`,
`OpenTrackview`, `CloseTrackview`, `CargoGrid`) resolve, and that the ship then spawns without the collision.

## A ship's parts

The official server binds a ship as a tree in one batch, the ship first: the Cutlass Red (build 13165946) has 190
descendants (thrusters, seats, doors, cargo grids, mounts, the quantum drive, interior containers), every one at the identity
pose, placed by its port, with 5 to 100 snapshot variables each. A ship bound alone shows a hull with no internals.

`services/dgs/src/retrieve_loadout.rs` binds the parts after the ship in the same `Update::Binds` of a retrieval, and unbinds
them with it. They come from the game data, `tables/ship_parts.bin` (format in [DATA.md](DATA.md), "What a build holds"),
which `tools/gamedata-build` (`src/ship_parts.rs`) makes from the DataCore for every vehicle of the build: 1111 classes
in build 13165946, the Marauder (104 parts) and the F8C (106) among them. The rule:

- a part is every entry of the class's `SEntityComponentDefaultLoadoutParams` loadout that names a class (by
  `entityClassReference`, else by `entityClassName`), in the port `itemPortName` of its parent;
- its own parts are the entry's `loadout` when it has one, else its class's default loadout, at any depth; a loadout is
  a list of entries (`SItemPortLoadoutManualParams`) or an XML file (`SItemPortLoadoutXMLParams`, `<Item portName
  itemName>`) in the encrypted `Data/Scripts`, read with the archive's key (`gamedata-build --exe`; door panels, bed
  pillows and mattresses);
- a port of the class (`SItemPortContainerComponentParams`) the loadout leaves empty that has a `defaultItem` gets that
  item (the Cutlass's `engineeringBuff` holds `Engineering_Buff_Modifier_DRAK_Cutlass` this way);
- the attach id is CRC-32C of the port name in lower case (as for a player's items, `loadout::port_attach_id`), the class
  CRC that of the class GUID; the order is breadth first.

**Verified** against the reference table of 16 vehicles (`testdata/retrieve_loadout_ships.tsv` of gamedata-build, every root
ship bind, the one that adds component `0x1cadfa78`, one per class; `gamedata-build ... --ship-parts-reference <tsv>`, and the
test `GAMEDATA_TEST_P4K=<Data.p4k> cargo test -p gamedata-build --target x86_64-unknown-linux-gnu`): of the parts in a port
the rule fills, 1635 have the class the rule gives and 22 another, all components a player swapped (16 on the
Constellation Mk5 Centaurus, 6 on the Hornet F7CS Mk2: power plants, shields, guns, the nose turret). The rule has 45 parts
the table has not: 43 missiles of the two Constellations (other ships of the table do have their racks' missiles,
so these were fired) and two holographic MFDs of the Starfarer's pilot dashboard (not explained). What the table has
beyond the rule is the state of those ships (players in seats, cargo boxes in the grids, a Nova and carriages in the
Liberator, a Skylark docked, paints, a refuel nozzle's camera source).

The ship's interior and lights are its object containers (`VehicleComponentParams.objectContainers`, one per socpak; the
Marauder and the F8C have two each). The file has them and the DGS binds each, after the parts, as an entity of class
`VehicleObjectContainer` with flags `0x101000`, under the ship at its `Offset`, attached by CRC-32C of `<boneName>-<socpak
name without directory and extension>` in lower case (`NoBone` for none; the client's ship adds one
port per container by the same name). Its variables are: `uhGuid` (`0x494411f3`, the container's
GUID as on the wire), `ocFilename` (`0xffa1bce6`, the socpak's hash), `socRadius` (`0x160cc028`, the metadata's
`radius`), `socHasMetadata` (`0x06d05294`, 1), `uhParentId` (`0xf419b233`) and `m_tileManagerId` (`0x9df9884f`), both the
ship, and `ocIdRange`/`ocIdRangeMov` (`0xe35914ad`/`0x09f4d2d5`), `entityCountStatic` ids after the parts and containers
and none for dynamics. Verified on the 55 containers of the 15 reference vehicles: attach id, class, flags, `uhGuid`,
`ocFilename`, `socRadius` (to the bit), position, rotation, `uhParentId` and the size of `ocIdRange` all match (one STV
container has no `m_tileManagerId`; some have `network.autoStowPolicy`, `0x586c96d1`, not sent, nor the life time
variables). The 1490 entities under them in the table (lights, rooms, switches; flags `0x100840`) are the `.soc`'s
statics, which the client makes itself from `ocIdRange` (as for hangar
containers): the DGS binds none of them, where the official server binds some over the client's own. **Not tried on a
client**: that a vehicle's container finds its graph node and loads (for a hangar the node comes from the container's
own id). `SC_DGS_SHIP_CONTAINERS=off` binds no containers, `SC_DGS_SHIP_CONTAINER_CONTENT=off` binds them with no
`ocIdRange`, so the client makes none of their entities.

**Empirical**, the flags: every class of the reference table always has the same flags (617 classes), and the generator gives
them from its components: `0x1000` always, `0x200000` with an `SGeometryResourceParams` (exact for all 617), `0x100000`
with static physics or a rigid one of mass 0, `0x4000000` with an `EntityComponentRoomOccupantParams`, `0x80000000` with
geometry and a physics controller but for fuel intakes. 13 of the 1635 matched parts (12 classes) get other flags than the
table's. Which of these bits the client needs right is not known.

Not sent: the parts' snapshot variables, tags and the root's owner (the bare binds of a player's items work; not tried on a
ship). The part ids are the graph's when it holds the ship's loadout tree (next section) and made up otherwise (`600,000,000,000 + the ship's sequence x 0x2000 + the index`; the containers and their content always have made up ids after the parts). `SC_DGS_SHIP_PARTS=off` binds
the ship alone.

## The Vehicle Loadout Manager: state (2026-11-16)

Not implemented, and not yet traceable: nothing in the logs of 14-15 Nov names the Loadout
Editor or the Vehicle Loadout Manager (VLM) beyond the registration lines of `UpdateArenaCommanderLoadout`, and the edge
gateway's access log of the last 48 hours (`envoy-scbackend-sc-edge`) has no call that only the VLM makes. The calls it saw
(all answered 200): `EntityGraphService/EntityQuery` 443, `GetEntityStowContextsByGeids` 255,
`ExternalEntityDocumentService/Fetch` 321 and `Upsert` 30, `ExternalEntitlementService/QueryEntitlementsStream` 180,
`QuerySnapshotsWithEntitlementsStream` 8, `InventoryQuery` 6. The VLM was not opened in that window.

What the client's own strings say (the names are the client's, the flow is not known yet):

- the loadout component's inventory read reads the inventory with the client's local
  `GetPersonalLocationInventoryConfiguration` (no server call) and fails with "The stowedInInventoryId of the loadout entity
  must be same as the current locationInventoryId" when the ship is not stowed in the inventory of the player's current location
  (our `<owner>:Hangar:<location id>` rule, INVENTORY.md). `CheckClientOnly` errors if the component is on a server entity.
- `OnUILoadoutSave` ("failed to generate loadout") and `CLoadoutSnapshot::SetLoadoutSnapshot` ("Request to loadout snapshot
  document for N loadouts", `Unsupported loadout type`, `Null document returned`) write a loadout snapshot as an entity
  document; `QueryLoadoutSnapshot` reads it. The repo has no loadout snapshot document type (`entity_document` has the
  Arena Commander, Star Marine and character customizer loadouts only).
- The ship list is the kiosk's ship list component (the ASOP's, "The kiosk's ship list is the client's own query" above).
  Whether the VLM uses the same list is not verified.
- `UpdateArenaCommanderLoadout` (entitlement v2) is implemented (`update_loadout` of sc-entitlement) for the Arena
  Commander loadouts only.

To prove it: open the VLM once with the ship in the hangar and read the gateway's access log (the `x-envoy-origin-path` of the
calls around that time), the DGS log for unhandled player RMIs, and the client log; the flow is still to be found
(ship list source, selected ship's component list, save path). The loadout snapshot document and any answer the VLM needs beyond
the ASOP's queries are built after that, not assumed. The Starfarer screen-port errors of the 15 Nov log (21 `Unable to find
item port`, all `Display` type ports: screens) are parked: the official bind has the same parents, ports and attach
ids; the ports of the Starfarer's turrets and dashboards are `Display` ports (`Screen_Left`, `Screen_Radar`...), the items
accept (`AttachDef` Type Display, Size 1), and the guns in the other ports of the same parents attached without error. The
cause is not found.

## A ship's loadout tree in the graph (2026-11-16)

Why: selecting a ship in the Vehicle Manager showed no components. Client log 01:51:16Z: `AsyncGetInventoryItems inventoryId
[300000100001:Location:1105811199]`, then `AddLoadoutPersistent AsyncQueryEntities`; the graph got (a) an inventory query on that
inventory (root filter `STOWED_IN` it, `outgoing_edges`, `snapshots`, tree with `path_mode` and an `itemTypeEnum` In
[11,12,56,75,...] filter) and (b) an `EntityQuery` with filter `geid In [ship]`, tree enabled with no filter, `outgoing_edges`,
and answered the ship alone (nodes 1, edges 1) because no entity hung from it. The query asks for the ship's tree.
Assumed: the manager builds the component list from the tree's nodes and the port ids of their attachment edges, with the ship
class's local DataCore ports (the same model as a player's worn items, which the client reads: `AttachmentReceived ... Port[...]`).
Not verified in game: that these nodes give the list (see TODO.md).

The model, as `EnsureLoadout` makes a player's worn items (`services/entitygraph/src/loadout.rs`): a node per part (`geid`,
`classGuidCrc`, `ownerId` = the ship's owner, `itemTypeEnum`/`itemSubTypeEnum` of the seeded class, `stackSize` 1, no inventory,
`debug.createdBy` = `shipPart`, `debug.ship`, `debug.partIndex`) and an `ATTACHED_TO` edge from the part to its parent (the ship, or
another part) with `attachmentType` `ITEM_PORT` and `attachmentPortCrc` = the CRC-32C of the lower case port name (the table's
attach id). The tree walk of the graph's `EntityQuery` follows those edges, so both queries above return it with no change.

Who makes it: `services/entitygraph/src/ship_parts.rs`. The parts are those of `tables/ship_parts.bin` (the DGS binds the same
ones), read by sc-entitygraph from the region's `sc-gamedata` (`GameData.GetFile`, flag `--gamedata` / `SCBACKEND_GAMEDATA`, default
`http://sc-gamedata:50051`; the build `--game-build` / `SC_GAME_BUILD`, default the last build the service has ready; the format
reader is the crate `sc-ship-parts`). The tree is made once per ship, as one command (`ship-parts:<geid>`): at `AcquireShip`, at
start for every ship of the graph (retried every 30 s while the game data is not there), and when a player's ships are read
(the list, the hangar read), which covers a ship that has none. A part the player removed is not made again (the command
is recorded). Removing a ship erases its parts with it. Without game data nothing is made and the DGS keeps its made up ids.

The retrieval: `RetrieveShip` answers with `ShipView.parts` (`ShipPartRef`: geid, class CRC, attach id, in the table's order). The DGS
(`retrieve_loadout::remember`, from the view in `spawn`) binds the parts with those GEIDs when their classes and attach ids are the
table's, so the loadout the player edits is the one that spawns; otherwise it binds made up ids as before.

### Parts the client spawns itself (`DuplicatedItemPorts`)

Why: with the full tree the client's `CLoadoutDummy::InitLoadout` (Vehicle Manager) refused 6 of the Drake Marauder's 104 parts as
`DuplicatedItemPorts`: an entity already sits in that port, because the client spawns the default loadout of the parent item's class
itself. Verified on the DataCore: those 6 are exactly the parts whose only source is the `SEntityComponentDefaultLoadoutParams`
of a nested parent class.

The rule (`tools/gamedata-build/src/ship_parts.rs`, `default_of_parent`): a part is marked `FLAG_DEFAULT_OF_PARENT` (`0x2000_0000`, in
the table's `flags`, no class uses it) when its parent is itself a part (depth 2 or more), it comes from that parent class's default loadout
(its entry has no inline `loadout`), and not from a port's `defaultItem` nor from an inline loadout; and every descendant of a marked part
is marked (the client's default loadout recursion spawns them too). The Marauder: 74 parts at depth 1, 24 inline nested ones (under
parents with inline loadouts: `Pilot_Dashboard`, `Mount_Gimbal`), 6 marked. Not verified for other ships: on the Gladius and the Lightning
the rule also marks the sub-parts of the `GATS_*` guns (`bar1`, `mec`, `pow`, `ven`); that the client spawns those too is not checked.

The table keeps every part (it is checked against the reference table, whose comparison masks the bit). Who uses the mark:

- sc-dgs still binds every part (`retrieve_loadout`), with `Part::bound_flags()` (the bit never goes on the wire). The graph's ids go
  to the unmarked parts, matched in table order by class and attach id; the marked ones get made up ids (by table index). A graph that
  does not match falls back to made up ids for all.
- sc-entitygraph makes no entity for a marked part (`debug.partIndex` stays the table index), so `ShipView.parts` has only the unmarked
  parts. A tree made before the mark (all parts) is trimmed at the first `ensure` after the deploy: the nodes with a marked
  `partIndex` whose parent is unmarked are erased with their subtrees, in one command `ship-parts-trim:<geid>`; the ship is then done.

Deploy: the game data must be republished (the table is regenerated by `publish.sh`'s gamedata step) and `sc-gamedata` rolled to serve it
(the format version is still 2: a DGS or entitygraph older than this change reads the new table and treats the marked parts as plain, the
DGS would send the bit); then sc-entitygraph, then sc-dgs. Roll sc-entitygraph first: a DGS that gets all parts from the graph while
the table marks some falls back to made up ids (a warning) until the trees are trimmed.

## The ship on the pad and the Store button

The wire of `SLandedVehicleInfos` (4.10.3), what enables Store, what the Store button
sends and the answer it needs.

- **Listed as retrieved.** The list answer's `landedVehiclesInfos` carries one record for the ship on the raised pad
  (`Retrievals::landed`: the kiosk's `on_pad`, no retrieval running): entity id, class GUID, owner, inventory id (player,
  `Location` 3, location id), timestamp 0, **landingATCId = the hangar's ATC id** (must equal the kiosk's `OwnerATCId`), landing area
  id, empty name (`0x00`), bits `0b100` (not bricked, not root, can store). The client marks the ship landed whenever a record exists;
  Store shows when the ATC id matches and the ship is not stowed, Retrieve (which needs IsRoot) must not win, so IsRoot is 0.
  The ship stays `Deployed` in the graph, so its class is also in `canDockHereList`. The client's own list of the stowed
  ships at a location (`OnRequestFetchVehicles`, an entity query by the location inventory id) and its stow-context reads
  (by owner, entity id or parent URN) would show the deployed ship's node, still in the bank, as stowed, and Store would not
  show; sc-entitygraph therefore leaves a ship whose `ships` row is `Deployed` out of those reads (`read::not_deployed`; the
  node, the record and the row are untouched, so the admin list, the repair and the backfill see the ship as before). A ship
  that is stored shows again at once, and `RmMulticastNotifyVehicleStored` gives the client its stow context. Not verified:
  the token check that applies because the ship entity is bound on the client (the retrieval's
  `RmMulticastChangePermissionToken` occupied is sent at `ShipReady`). The DGS logs each answer to the kiosk's list request
  (the landed record's ship, ATC, area and flags; the can-dock, destroyed and landed counts) and a held retrieval with no
  landed ship.
- **Store.** The button sends the kiosk's `RmAuthorityStoreVehicle(EntityId)` (method 6 by the global numbering; no ATC call). `Kiosk::store` / `Retrievals::store`: the pad lowers (`LowerPlatform`, ship unbound as it starts down),
  at `ClosedIdle` the ship is reported Stored at the location (`StoreShip`), then `ShipStored` frees the token and sends
  `RmMulticastNotifyVehicleStored` (ATC method 16, at the request's index + 13: `u64 vehicle, u64 player, u32 location,
  SInventoryId <player>:Location:<location>, bool ok=1`) so the kiosk's `OnVehicleStored` writes the stow context and Retrieve
  appears. Needs a retrieval to have run since the join (the ATC's method base is learned from the request; else nothing is sent).
  Refused (logged `ship store refused`): a retrieval running, the pad moving or down, not the ship on the pad.

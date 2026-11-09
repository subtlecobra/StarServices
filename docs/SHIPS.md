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

`Kiosk::retrieve`: sc-entitygraph approves (STORED, fits; state becomes DEPLOYED) -> the platform is asked to open
(lower) -> at OpenIdle the ship entity is spawned on it -> the platform is asked to close (raise) -> at ClosedIdle the ship
is in the bay. A spawn that fails stores the ship again (`ReportShip`). One retrieval at a time, and none while the platform
moves. The platform is not the transit carriage of `transit/elevators.rs` (those carry people between gateways); the ship
lift is an entity of the hangar (`LoadingPlatformManager_ShipElevator_*`, at the hangar's id + 133, its platform at + 131 in
Orbituary's small one, see `location.rs`). Reuse of the elevators' machinery is limited to the way a line drives entity binds
and door states; the lift needs its own entity variable changes, whose keys are not read yet.

## Who owns what

- sc-entity-document: the record. sc-entitygraph: the entity, the bank, the `ships` row, the rules, the transitions.
- The DGS: everything about a retrieved ship once live (it has authority), the kiosk component and the platform, so
  `Kiosk` lives there. The replicant (pu-frontend) only relays the client; it has no part.
- sc-entitygraph asks sc-entity-document over `ShipRecordsInternal`; the DGS asks sc-entitygraph over `ShipsInternal`. No
  service reads another's tables.

## Not done / needs the game

See the list in TODO.md ("Ships"): the kiosk's remote methods and their arguments, the platform's entity variables, the
ship entity's spawn with its loadout from the snapshot, the callers of acquisition, the seed of sizes and insurance, the
portal view (a read of `ListShips` by owner and a state button via `ClaimShip`/`ReportShip`).

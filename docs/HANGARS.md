# Hangars

Status: in progress (see the plan for hangar instantiation). Built so far, in `services/dgs`, with unit tests and not wired
into a session yet:

- `hangar_slots::HangarSlots`: ids for hangar instances other than a player's home hangar (`location::hangar_id`). The pool
  runs from 225 000 000 000 to the carriages at 230 000 000 000, one `0x1_0000` stride per instance (room for the hangar's
  content ids), at most 256 open at once. An id is reused only after the unbind of its instance was acknowledged.
- `hangar_slots::Occupancy`: who is inside an instance or riding to it. An instance with nobody for `IDLE_GRACE_MS`
  (60 s) is due to close; coming back within it keeps the instance as it was.
- `hangar_access::Access`: the owner always enters; party members and friends enter when the owner allows; a stranger
  never; the owner can deny a person. Guests never manage the owner's ships or items.
- `hangar_relation::RelationResolver` / `SocialRelations` / `may_enter_hangar(relations, access, owner, visitor)`: the
  relation of a visitor to the owner (owner, or in the owner's party), asked of sc-social over
  `SocialInternal.GetPartyMembers(geid)` (the members of the party the player is in, empty when none), then checked with
  `Access`. Ids are character GEIDs. `ContactRelations` also asks sc-contacts (`ContactsInternal.ListFriends`, by account, mapped with
  `account_of_geid`) so a friend of the owner resolves to `Friend`; `SocialRelations` alone never does. An unreachable
  sc-social or sc-contacts resolves to `Stranger`. Not called from
  `hangar_instance` yet: `enter` takes no relation, so a caller must check `may_enter_hangar` before it.
- `location::HomeHangar::with_id`: a hangar entity of any instance id; `HomeHangar::new` is the home one.

- sc-entitygraph `hangars` collection and `HangarsInternal` (`entitygraph/v1/hangars.proto`): one row per (player,
  location), keyed `<owner>:<location>`, with `hangarClass`, `size`, `lastUsed`, `createdAt`. `ListHangars` returns a
  player's rows (one location or all); `EnsureHangar` records the default hangar of a location once and afterwards only
  moves `lastUsed` (the class of an existing row never changes). The size comes from the class name
  (`Hangar_<Small|Medium|Large|XL><Front|Top>_<place>`) unless given.

Gap: the location's hangar list (class, size) is in the game data tables (`home_locations.tsv`), which sc-entitygraph
does not read, and which locations a player has a home or lease at is sc-entity-document's `player_home_location`. So the
caller (the DGS or the hub, which have both) names the default class in `EnsureHangar`; the service does not choose one
and refuses a new hangar without a class that names a size. Only the default hangar per location is recorded, not a
choice among a location's sizes.

- `hangar_instance::HangarInstances`: the manager over the two. `enter(key, player, session, now_ms, elevators)` opens an
  instance on demand, binds it (hangar, content, ATC; key `hangar:<id>`) to the session of a player inside or riding to it,
  and opens the owner's elevator stop; `leave` unbinds that session (the owner's leave closes the stop); `session_ended`
  leaves everything a session held; `sweep(now_ms, elevators)` closes instances idle for `IDLE_GRACE_MS`, unbinds anyone
  still bound and returns their ids, which come back to the pool with `acknowledge(id)`. The clock is passed in.
- `transit::elevators::Elevators` holds several hangar stops per owner: the home one stays keyed by the owner
  (`open_hangar`, unchanged), an instance is keyed by its entity id (`open_instance`, returns the stop id for `call` and
  `close_hangar`). `stop_of(owner, location)`, `stop_of_panel` and `stops_of(session)` look stops up. Closing an instance
  leaves the owner on the line; closing the home stop still disconnects them.

The default join path is unchanged: the home hangar is bound by the join response and `Elevators::open_hangar`. Nothing
calls `HangarInstances` yet.

Not done:
- Calling `HangarInstances` from the control loop (an elevator pick, arrival from another location, `leave_player` ->
  `session_ended`, a periodic `sweep`), and a `HangarInstances` held by `Control`.
- The acknowledgement of an unbind: the outbox is fire and forget and the wire has no answer for it, so `acknowledge` has no
  caller; a closed id stays out of the pool until one exists (TODO: decide whether the replicant confirms the write of the
  update, or a grace delay is enough).
- A carriage and the elevator panel show only the owner's session: the stop has one session, so guests do not see
  carriages, and the panel's list (`panel_entries`) still lists only the viewed stop (plus the lobbies when served). Listing
  the owner's other hangars needs their labels (game knowledge, TODO) and the guests' sessions on the stop.
- A rider's `Load` of a stop is granted to the stop's owner only; guests need `hangar_access::Access` and the relation from
  sc-social and sc-squad (a `ListRelated` RPC in the owner's package).
- The planet entity is not bound with an instance (shared with the home join; unconfirmed whether a second bind is safe).
- The entitygraph record of which hangars a player has, persistence on close, the portal section.

## The size of a player's home hangar

A character lives in a hangar of one size at its home location, Large unless an operator chose another. The choice is the
character's residence, `player_residence` in sc-entity-document (`GetResidence`, `SetResidence`; the admin site's Location page
sets it, for a player who is off a shard). The Hub reads it at login and the session carries it (`ResolveSession.hangar_size`),
the replicant puts it in `JoinPlayerRequest.hangar_size`, and the DGS gives the player the hangar of that size at the location
(`HomeHangar::sized`): the class, socpak and spawn of the size's row of `tables/home_hangars.tsv`, which `gamedata-build` writes
for every hangar class of every home location (`defaultHangars`). The hangar's entity id stays the player's own.

Everything else follows the class: the kiosk's `MaxShipDockingClass` (`atc::SHIP_DOCKING_CLASSES`: 2, 4, 5, 6 for Small to XL),
the platform layout (`platform::LAYOUTS`) and which pad a ship fits (`ships::hangar_class`). A build with no `home_hangars.tsv`
keeps every player in the first hangar of the location, as before.

The client lists a ship in the ASOP only when its docking class (the ship's `AttachDef.Size`: 1 to 2 small, 3 medium) is within the
kiosk's limit, and when the ship's `VehicleComponentParams.dogfightEnabled` is true: the F8A has it false, so it is never listed,
in retail either.

A hangar's collision is over a million triangles, so a DGS loads only each location's first hangar at startup and builds the
collision of any other size when a player first joins in it (`Control::load_collision`); it stays loaded. A DGS pod asks for
1024Mi and may use 2048Mi (`shard-operator`'s `DGS_MEMORY_REQUEST_MIB` and `DGS_MEMORY_LIMIT_MIB`); at 512Mi, loading every
size at startup was killed for lack of memory.

# A player disconnects

What the client sends when it leaves, who learns of it, and what each service does. This is the backend's side.

## What the client sends

| Leaving | On the shard connection (UDP) | On the Hub connection (TLS) |
|---|---|---|
| Exit to menu | transport type 5 (reason 30016, `DisconnectCmd: disconnect light ExitToMenu`), repeated until the replicant answers with type 6 | stays open: the client is in the menu |
| Quit, Alt+F4, close window | type 5 (30016, `User closed the application`) five times at once, no wait for the 6 | closes with the process |
| Crash, kill, power or network loss | nothing | closes (a lost network: after the TCP timeout) |
| Kicked by the server | the replicant sends the type 5 (`Remote Disconnect - <text>`; 30028 inactive, 30015 duplicate login) | stays open |

There is no application-level logout message on the Hub's protocol (`proto/scbackend/hub_wire.proto`): its end is the
connection closing. A crash and a lost network are the same to the replicant (silence for the 60 s peer timeout), so both
are `TIMEOUT`.

## Reasons

`scbackend.internal.common.v1.DisconnectReason` (`proto/internal/.../common/v1/disconnect.proto`):
`CLEAN` (the client's notice), `KICKED` (the server ended it; nothing sends this yet, the enum has it for the first
kick), `TIMEOUT` (silence, crash or network loss), `CONNECTION_LOST` (the Hub's own connection closed), `REPLACED` (the
same player connected again from the address and took over: **the player has not left**, nothing is released or
reported). `PlayerDisconnect` carries the reason, the client's cause code, player, shard, `connected_at_ms`, `at_ms`
and the `event_id` every receiver deduplicates on (`<replicant>:<session>` from the replicant and the DGS, `hub:<hub
session>` from the Hub).

## Who owns what

- **sc-hub owns the session and the shard slot** (`sessions`, `player_shards`), so every report arrives there:
  `HubInternal.ReportDisconnect`. It records the event once per `event_id` in `player_disconnects`, frees the player's slot
  (only the slot of the connection that ended: on the named shard and written before the connection began, so a late
  report never frees the slot of a newer join), and fans out. Its own connection to the client closing is the same event
  (`CONNECTION_LOST`), and also ends the session.
- **A DGS owns the entities it holds**, so it releases them: `DgsControl.LeavePlayer` (now carrying reason, client code
  and time) drops the DGS's per-session state at once (holdings, movement, call panels, the hangar elevator, the outbox
  subscription) and sends `EntityGraphInternal.ReleasePlayer` for what it held. A session it does not hold (taken over,
  a repeat, a restarted DGS) sends nothing.
- **The replicant only forwards**: it learns of the end first (notice, timeout, replacement) and tells both owners, each
  on its own task so the packet loop never waits. It does not decide anything about the player.
- **sc-entitygraph owns the live markers** (`shardId` on an unstowed entity): `ReleasePlayer` clears the markers of the
  entities the player owns that were on the shard (or on any shard when none is named), except markers written after
  the disconnect. Once they are gone the item RPCs and the admin portal stop refusing changes as "live on a shard". The
  portal's "the player is on a shard" refusal comes from the Hub's slot, which the same event frees.

Why two reporters: the DGS is the fast path (it knows exactly what it held), the Hub is the backstop that cannot be
forgotten (a DGS that died or restarted holds nothing and sends nothing; a report the replicant could not deliver to
the DGS still reaches the Hub, or the Hub's own connection ends).

## Guarantees

- **Idempotent**: the Hub records an `event_id` once; clearing markers and freeing a slot find nothing the second time; a
  `REPLACED` report is dropped everywhere; a late report of an older connection changes nothing newer (slot join time,
  marker `updatedAt` against `at_ms`, a 2 s tolerance for clock skew).
- **Never blocks the disconnect path**: the replicant reports from tasks of their own (5 tries, the retry period apart);
  the DGS's release is a task of its own (4 tries, doubling from 0.5 s); the Hub answers after two database writes and
  fans out on a task.
- **Durable where it matters**: `player_disconnects` is the queue of events sc-entitygraph has not heard. A loop in the Hub
  sends them again every 15 s for a day (`attempts` counts). Without `--entitygraph` the Hub only logs a warning and
  marks them told.
- **No player stays live for ever**: a lost report to the Hub is covered by the Hub's connection closing; a lost one to
  the DGS by the Hub's fan out; sc-entitygraph being away by the retry loop. What this does not cover: a Hub that is
  down for more than a day, and a Hub connection that stays open while the client is gone (TCP keepalive decides).

## Per service

| Service | What it keeps per player | On disconnect |
|---|---|---|
| sc-hub | session, `player_shards` | slot freed, event recorded and fanned out; session ended with its own connection |
| sc-dgs | holdings, movement, panels, hangar elevator, outbox | dropped by `LeavePlayer`; `ReleasePlayer` sent |
| sc-entitygraph | `shardId` markers | cleared by `ReleasePlayer` |
| sc-presence | `player_presence`, one stream per client | already: the stream ending takes the player offline, and a row not refreshed for 3 minutes shows offline |
| sc-social, chat, friends, squad, notifications, push, rtc, matchmaking, mission, contacts | state tied to the client's own gRPC streams | not changed. They end with the client's connection; none keeps a per-shard marker the Hub's slot does not already answer (the player's instance comes from `GetPlayerShard`). A review of each for membership that outlives its stream (a squad, a queue ticket) was not done |
| sc-instance-manager | none per player found | the shard it reports for a player is the Hub's slot |

## Still open

- Writing the player's position and inventory back to the graph when they leave: the DGS holds no persistent player
  state yet (a join builds the player from the character and the loadout; `EnsureLoadout` is the only write), so there is
  nothing to flush today. When the DGS unstows items onto a shard, it should restow them in `LeavePlayer` before
  `ReleasePlayer`, which stays the backstop.
- A rider who leaves aboard an elevator carriage is saved at the stop of their trip (docs/DGS_TRANSIT.md): the hangar
  elevator is closed on leave, the simulation of rides is not wired into the DGS yet.
- The only sender of `KICKED` is the server error below (a duplicate login or an AFK kick would be the next).

## Server error: a DGS replaced under a player

A player in game whose DGS was replaced has a world that no longer exists: the new DGS has forgotten the player and
its entities. The replicant tells the client so, with the type 5 notice a real replicant sends (`Remote Disconnect -
server error`, cause code `SERVER_ERROR_CODE` 30000, text `SERVER_ERROR_TEXT`), and the client returns to the menu. The
connection ends as a kick (`KICKED`), which frees the player's slot at once, so the player joins the same shard again, on
the new DGS. The notice goes again every pruning period, at most four times in all, until the client acknowledges (type
6) or answers with its own notice.

It is sent when, for a player in game, either

- the DGS's boot id changed (the status poll sees a restarted DGS answer; a player still joining is joined again, as
  before), or
- the DGS's session updates ended and it was unreachable three times in a row (about 9 s), so the client is not left
  frozen while the new DGS loads its area.

`SC_REPLICANT_DGS_RESTART=rejoin` on the replicant keeps the old behaviour (joining the player to the new DGS again), and
`=pause` pauses the client instead of kicking it (below).
The cause code 30000 is the network layer's generic error and is assumed to suit: check what the client shows.

### Pause instead of kick (`SC_REPLICANT_DGS_RESTART=pause`, opt-in)

The game pauses the player and shows `net_dialog_server_error` while its server is away, and gives control back when a
new authority is assigned. The mode does that instead of a kick, with the message the official server stack sends:

1. The DGS is gone (updates ended and unreachable three times in a row), or its boot id changed: the client gets
   `BlockGameSimulation` with bit 0 set, `35 01 00 01` (`channel::block_game_simulation(true)`). A player with no entity yet
   is kicked as before. From now on what the DGS sends the client (binds, unbinds, variables, calls) is held.
2. The player is joined to the new DGS again (`Session::rejoin`). `Event::Rejoined` carries what the new DGS bound for it
   (`PlayerEntities`); not a joined player is a kick.
3. **Unbind**: one `UnbindBatch` (31, or 32..34 in parts of at most 15 ids) of every entity the old DGS bound to this client
   and the client still holds, children before parents. The replicant notes them as it forwards them (`Peer::dgs_bound`: the
   join's planet, hangar, containers, player and items, and the DGS's update binds, minus its unbinds). The world's roots
   (universe, solar systems) are the replicant's own and stay, as the new entities hang from them. What the old DGS sent
   before the rejoin and was held is dropped.
4. **Bind**: the new DGS's entities in the join's order, parents first, one batch at a time (planet, hangar, hangar
   containers, player, the player's items by depth), each `BindBatchBegin` .. adds .. `BindBatchEnd`, the next only after the
   client reported the batch spawned (`BindBatchAsyncSpawnsCompleted`, 29). Batch ids come from the connection's own counter
   (`Batches::open`, never reused), not from the new DGS, whose own counter restarts: the client takes a repeated batch id as
   fatal. A second rejoin in the middle replaces the recovery and unbinds what the first one bound.
5. **Settle**: `SC_REPLICANT_PAUSE_SETTLE_MS` (default 1500, an assumption) after the last batch ended. Then what the new DGS sent
   meanwhile is replayed (a bind of an id the recovery bound already is dropped, as the client disconnects on it), and last of
   all the client gets `BlockGameSimulation` with bit 0 clear, `35 01 00 00`. Neither is sent twice in a row.

**Signal for the end of the spawns:** the only one the client gives is the 29 of each batch, which means every entity of the
batch exists, has run its init and is physicalized (not that its meshes streamed in). There is no acknowledgement of the
pause and no message for what comes after, so the settle delay is the margin; raise it if the world is not there when the
client is released.

**What the message is** ("BlockGameSimulation", 4.10.3):

- Replication message type 0x35 (53), shard to client on the reliable layer. The client's dispatch takes types below 0x39; its handler disconnects with 0x7569 unless the view has the replicant role (which this replicant has), reads one bit, and queues `CGame::SetGamePaused(flag)`. Paused means the `player` action map is disabled (input frozen), fixed time mode is enabled, and `IsGamePaused()` is true; `@net_dialog_server_error` is shown while that is true. No timeout and no auto-unpause: one bit-1 pauses and one bit-0 resumes.
- The official server sends it with the reliable byte 1 and a budget of 1: block when a player entity is left with no authority view, unblock when a player entity with that mark gets a new non-null authority view.
- The framing is `35 01 00 <bit>`. It is the repo's framing of every message (`channel::message`: id, the length's low byte, the length's varint high part, the payload), with the payload size one byte from the one bit. The shard's own one-byte message `ViewChangeState` is framed the same way (`06 01 00 <state>`), which a test checks.
- That a DGS stall triggers it on the real stack is assumed (it fires when the authority view goes null versus non-null).

**Client side (4.10.3):** the client does NO clean-up of entities when the
authority or DGS changes: what the old DGS bound stays until an `UnbindBatch`. A bind of an id the client holds disconnects it
(0x7564, "Entity already bound"), so unbind first. A bind whose parent is not live disconnects (0x7538), except a parent of
0xfffffffffffffffc (the root), so parents first, in the same or an earlier batch. `ForceRebindEntity` (type 1) has no client
handler and is never sent. `BindBatchBegin` (26) needs a fresh, never used u32 batch id every time (a repeat is a
CryFatalError "multiple spawn batches"; an End without a Begin is fatal). An unknown id in an unbind is logged and skipped;
an unbind destroys the entity, its children go too unless their flags & 0x1800 == 0x1000, and unbinding the player's entity is
allowed. `ViewChangeState` is monotonic (a lower or equal state is ignored). `GameContextInfo` need not be re-sent and
`GameDataHashes` must not be sent with other values. Never sent from a replicant: `ReleasePlayerFromShard` (55),
`LocationResolutionResult` (52), Stow/Bury (7/8), `StreamingBubble` 39..41; an `UpdateBubbleRemove` (43) of an unknown bubble
disconnects.

**What is assumed** in the recovery: the 15-id cap on unbinds (kept); the settle delay; that
`BindBatchWaitingForAsyncSpawns` (28) is still right to send (the join and
the update binds use `bind_batch`, which sends 28, and the client answers 29 to it, so it is kept); that the
client needs the player's authority (`client_authority`, when the player is client-authoritative) sent again for the new
player entity, and an `UpdateBubbleCreate` when the player's entity id changed (a bubble is keyed to its player; the same id
keeps its bubble); that the world a client holds after the unbind and rebind is playable (hangar contents, a ship on the
platform: the new DGS has none unless it binds them). **Why it is still opt-in:** the whole flow is tested with synthetic
messages only (`shard::tests`, a model of the client's rules above), never against a client. Until a test shows the game
recovers, the kick is the default.

**Why not a variable:** an earlier attempt froze the client by writing the `EAPlayer.serverFrozen` variable at a replicated
index, which crashed the client (a stream size mismatch on another variable at that index). The `EAPlayer.*` variables are only
registered when the game rules are an electronic-access module, which the PU is not: `serverFrozen` is not in the PU client's
player variables at all, at any index. The variable freeze and its `SC_REPLICANT_SERVER_FROZEN_INDEX` setting are removed.

### The test loop for DGS-only changes

`scripts/dgs-roll.sh [--context C] [--dry-run] [shard...]` (after `scripts/publish.sh sc-dgs`) sets the image of the
`dgs` container of the DGS pods of the shards that have players (or the named ones) to the release manifest's sc-dgs
image, in place: same pod names and addresses, no new shard, no drain, no other service touched. A shard whose pods
already run it is refused. The restarted DGS makes the replicant send the clients the server error above, and the player
joins the same shard. It prints each pod's change before it writes (`--dry-run` stops there) and waits until the pods are
ready on the new digest. The shard operator does not compare a pod's live image (only the manifest it made it from, which
a normal deployment of the operator changes), so a rolled pod keeps its image until its shard is replaced; a shard made
later still gets the operator's image until the next deployment.

## What needs a game test

1. Exit to the menu: the replicant logs `client disconnected` and `connection ended reason=Clean client_code=30016`, the
   Hub `player disconnected`, the admin portal stops saying the player is on a shard, while the client stays in the menu on
   the Hub connection. Join again from the menu: the slot is written again and not freed by the old connection's report.
2. Alt+F4 (five notices, no wait for the 6): the same, once.
3. Task-manager kill: `Timeout` after 60 s on the replicant, and the Hub's connection closing (`CONNECTION_LOST`); one
   release, one slot freed.
4. The cause code values (30016 for exit and quit) match the logs of the 4.10.2 client; 30028 and 30015 are from official
   servers' kicks and are not sent by us; the server error's 30000 is assumed.
5. A client that reconnects at once from the same address (crash and restart): `Replaced`, the new connection keeps its
   entities and slot.

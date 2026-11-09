# A player disconnects

What the client sends when it leaves, who learns of it, and what each service does. The wire facts are in the
reverse-engineering notes (`docs/netcode/spawn_ack_and_disconnect.md` there); this is the backend's side.

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
- Nothing sends `KICKED`. The first kick (duplicate login, AFK) sets it with `Session::end` and sends the type 5.

## What needs a game test

1. Exit to the menu: the replicant logs `client disconnected` and `connection ended reason=Clean client_code=30016`, the
   Hub `player disconnected`, the admin portal stops saying the player is on a shard, while the client stays in the menu on
   the Hub connection. Join again from the menu: the slot is written again and not freed by the old connection's report.
2. Alt+F4 (five notices, no wait for the 6): the same, once.
3. Task-manager kill: `Timeout` after 60 s on the replicant, and the Hub's connection closing (`CONNECTION_LOST`); one
   release, one slot freed.
4. The cause code values (30016 for exit and quit) match the logs of the 4.10.2 client; 30028 and 30015 are from official
   servers' kicks and are not sent by us yet.
5. A client that reconnects at once from the same address (crash and restart): `Replaced`, the new connection keeps its
   entities and slot.

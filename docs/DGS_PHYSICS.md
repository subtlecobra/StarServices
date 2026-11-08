# DGS physics: a player against the world

The DGS owns where a player is (see "Player movement" in the protocol notes, kept outside this repository). The client moves itself and reports where it went
(`m_clientAuthoritative` 0x10, "Smooth movement" in the protocol notes), so the DGS's collision has two jobs: it judges each
reported position (see Client positions below), and it moves the player's body itself only when the client stops
reporting: walls stop it, stairs and ramps are climbed, it falls, jumps and crouches. Code: `services/dgs/src/physics/`.
The data comes from `tools/gamedata-build`, which converts Data.p4k once per game build (see Format and where it lives).
This covers hangars only so far.

Marks: **confirmed** means read from the game's data or code, or seen on a running client. **Inferred** means chosen
because it fits what was read, without being checked.

## Collision data

### What is solid

- **Confirmed.** A hangar is its socpak plus child containers at any depth (`<ChildObjectContainers>`, placed by `pos`
  and `rot`). Orbituary's `hangar_smlfrnt_002_rest_rund` has five: itself, `hangar_smlfrnt_002` (the shell and the
  floor), two `spawncloset_static_elev_rstop_hgr_rund_001`, `cargo_elevator_sml_001_rund_a` and `elev_util_dropoff`.
  Between them they hold 2027 brushes (render nodes of chunk 16 of each `.soc`).
- **Confirmed.** Of the rest stop's 638 render nodes, a parked client had physicalized 316 (Floor of a hangar, protocol notes).
  No field of the brush node separates the two groups. Bytes `+0x4` to `+0x34` are the box as six doubles, and the flags
  at `+0x38` take only three values, split 405/23/7. **Inferred:** the other brushes had not been physicalized yet.
  CryEngine physicalizes brushes on demand near the player, so the DGS treats every brush that has a collision proxy as solid.
- **Confirmed.** All but one of the 2027 brushes have collision geometry in their `.cgf` (physics chunk `0x58de1772`).
  The exception is a shield (`hangar_door_front_large_a_shield_a`), which has none a player collides with. Render
  meshes are never used.
- **Confirmed, from EditorSC's reading of `AddPhysGeomFromChunk` and the node flags.** What is left out: `$IP` interaction
  points, `$shield` volumes, interior volumes, `no_player_collide` nodes, and faces whose surface type does not collide
  with players (1978 triangles here). **Inferred:** group-0 hulls of nodes that also have a cluster mesh are left out
  (62,355 triangles), because they cut across walkable space on other models; whether the player collides with the hull
  or with the cluster mesh is open. Distance-field hulls are used only where a node has nothing else.
- Entities are left out: doors, elevators, lifts, the kiosk, ladders and lights (`--report` lists 47). Doors and lifts
  move, and the DGS will hold them as entities of their own. So the spawn closet's door is open to the DGS.

### Size

Cluster meshes are as dense as render meshes: 1.8 M triangles in the 572 distinct shapes, and 5.4 M once placed.
Each shape is simplified once, offline, with meshoptimizer's quadric simplifier in absolute error mode, to within 3 cm.
At 5 cm, 13 thin brushes vanish; at 3 cm none does. The result is 289 k triangles in shapes, 767 k once placed, and a
2.7 MB file.

### Format and where it lives

`collision/<socpak hash>.col` of a game build, where the hash is the CRC-32C that the build's `tables/home_locations.tsv`
uses (1200814982 for Orbituary).
The file is zlib compressed: `SCHC`, u32 version 1, u32 socpak hash, u32 shape count, u32 instance count, 6 f32 bounds;
per shape u32 vertex count, u32 triangle count, f32 `x y z` per vertex, u32 `a b c` per triangle; per instance u32
shape index and a row-major 3x4 f32 matrix into the hangar's frame. Little endian.

- `scripts/gamedata-publish.sh [Data.p4k] [--build <n>] [--out <dir>]` publishes a build for the regions: it runs
  `tools/gamedata-build` on the archive and `scripts/gamedata-push.sh` on the result. The build is the change number of
  `build_manifest.id` beside the archive (`Data.RequestedP4ChangeNum`, 12913470 for 4.10.2), the one shard ids carry.
- `gamedata-build <Data.p4k> <out> [--build <n>] [--jobs <n>] [--report] [--tables-only]` writes the DGS's tables
  first, `<out>/<build>/tables/home_locations.tsv`, `hangar_content.tsv`, `entity_counts.tsv` and the `transit_*.tsv`
  of the hangar elevators (see docs/DATA.md, Game data per region; `--tables-only` stops there), then `<out>/<build>/collision/<hash>.col` for every hangar of that `home_locations.tsv`, finding each
  socpak by hashing the paths of all socpaks in the archive, then `<out>/<build>/manifest.json` (path, size and sha256
  of each file, sorted by path).
  The manifest is removed first and written last by rename, so a partial run never looks complete; a `.col` that already
  inflates with the right header is kept, so a second run converts only what is missing. Socpaks run in parallel over
  one shared archive, with `min(CPUs, MemAvailable / 1.5 GB)` workers by default. The six hangars take 4.4 s and 3.6 GB
  peak RSS with six workers, 5.6 s and 1.5 GB with one (most of it the archive's mapped pages). It points
  `P4K_EXE` at nothing, so the archive reader never opens the game's executable for a key (only `Data/Scripts`
  entries are encrypted). Its output is deterministic: Orbituary's file is byte-identical to the one the DGS image
  embedded before, now the test fixture `services/dgs/testdata/collision/1200814982.col`, and the tables to the ones the
  images embedded.
  meshoptimizer is C++ and the repository's musl target has no g++, so it is built for the host:
  `cargo build --release -p gamedata-build --target x86_64-unknown-linux-gnu`.
- `scripts/gamedata-push.sh <out> [build]` checks the directory against its manifest and pushes it as
  `<registry>/scbackend/gamedata:<build>`: `FROM scratch`, the files under `/gamedata/<build>/` and nothing else, built
  with `--timestamp 0` so the same files give the same digest.
- `sc-gamedata` (`services/gamedata`) holds a region's game data in the region's Valkey cluster, whose `gd:` keys it
  alone reads and writes; it is a stateless Deployment. For each build it is given (`SC_GAMEDATA_BUILDS`) that is not
  complete in Valkey, the replica that takes the build's lock (`gd:<build>:lock`, expiring, renewed while it works; the
  others wait) pulls `<SC_GAMEDATA_REGISTRY>/<SC_GAMEDATA_REPOSITORY>:<build>` into its work directory (an emptyDir),
  layers in parallel, every blob checked against its digest and every file against the manifest, then writes each file
  as `gd:<build>:<path>` (in 1 MiB pieces; a file over `SC_GAMEDATA_CHUNK_BYTES`, 64 MiB, as parts
  `gd:<build>:<path>#<n>`, never over Valkey's 512 MiB value), the manifest with each file's parts as
  `gd:<build>:manifest`, and the image digest as `gd:<build>:complete` last. It serves the files over
  `scbackend.internal.GameData` (`GetFile` in 1 MiB chunks read with `GETRANGE`, `ListBuilds`): it hashes a file as it
  streams it and holds the last chunk back until the sha256 matches the manifest, so a changed value ends the stream with
  DATA_LOSS instead of a whole wrong file. Its gRPC health is SERVING once Valkey answers and every build is complete in
  it. A build that no replica is configured with any more is pruned (every key, logged with what it freed). Keys,
  locks and pruning are described in `services/gamedata/src/cache.rs`; the Valkey cluster in docs/DATA.md. A build
  pushed again under its tag (another image digest) is loaded again: each replica asks the registry what the tag names
  at start and every `SC_GAMEDATA_RECHECK_SECS` (300).
- A DGS with `SC_GAMEDATA_ADDR` and `SC_GAME_BUILD` asks `sc-gamedata` for `collision/<socpak hash>.col` of each hangar
  of its system (`GameData::hangar_collision`), after the tables. For a local run `SC_GAMEDATA_DIR` names a build
  directory of `gamedata-build` (`<out>/<build>`) instead; with neither it stops with an error. Nothing is embedded in
  the image. A hangar whose file cannot be had (service away, build not loaded, file missing or not decoding) is tried
  again with a backoff (1 s doubling to 30 s), and the DGS opens its port, which is its readiness, only once every one
  is loaded: a player is never placed in a hangar without its walls, and a broken build shows as DGSs that stay unready
  and log `game data not loaded`. The log line `hangar collision loaded` names the build and where it came from.
  `World::decode` only sees bytes; the tests build Orbituary's from `services/dgs/testdata/collision/`.
- At load, every instance is placed in one parry3d `TriMesh`: 767 k triangles, decoded and indexed in 0.33 s in release
  (about 3 s in debug). It is shared by every player in such a hangar, since each player's hangar is a zone of its own
  with the same frame.
- The DGS's dependencies stay pure Rust: parry3d 0.31 (glam), and flate2 with miniz_oxide. editorsc and meshoptimizer
  (C++) are only in the offline tool.

## Controller

`physics::step(world, state, input, dt, params) -> state` works in the zone's frame, Z up. The position is the feet,
as `SMovement::m_entityPosition` holds it. **Confirmed:** at Orbituary's spawn point the floor is at z 4.493, against the
4.51 of the spawn transform.

The model is CryEngine's living entity, which the records parameterize. The collider is a capsule that floats above the
feet, and the ground under it is traced. Anything lower than the gap passes under the capsule and is stepped onto once
the trace stands on it.

| Value | Used | Source |
| --- | --- | --- |
| Standing capsule | centre 1.05 m above the feet, radius 0.39, half length 0.15; top 1.59 | `ActorStanceDimensionsInfo.Stand`: `heightCollider`, `size` (0.39, 0.41, 0.15), `capsuleAxis` Z. Reading `size.x` as the radius and `size.z` as the half length follows CryEngine's `sizeCollider` (inferred) |
| Crouched capsule | centre 0.77, radius 0.37, half length 0.05; top 1.19 | `ActorStanceDimensionsInfo.Crouch` |
| Step height | 0.51 m standing, 0.35 m crouched | Inferred: the gap between the feet and the capsule, `heightCollider - size.z - size.x`. No record names a step height |
| Ground trace | ball of radius 0.125 | `groundTraceSpreadSizes` 0.25 x 0.25, read as the spread of the traces (inferred) |
| Slope limit | 50 degrees | `EntityClassDefinition.Player`, `SSCActorPhysicsControllerComponentParams`: `maxClimbAngle` 50, `minFallAngle` 50 |
| Run, sprint | 5.58, 7.2 m/s | `ActorStanceSpeedsInfo.Stand`: `runFastSpeed`, `sprintSpeed`. A client measured 5.7 and 7.3, which `movement` uses |
| Crouched | 2.3 m/s | `ActorStanceSpeedsInfo.Crouch`: `defaultSpeed` and `runFastSpeed` |
| Jump | 0.5 m high, so 3.13 m/s up | `JumpFallLandParams.ActorJumpFallLandPlayerParams` `jumpHeight`, named by the Player's `SActorComponentParams.jumpFallLandParams` |
| Acceleration | velocity approaches the wished one at 50/s on the ground, 0 in the air | `inertiaAccel` 50, `airControl` 0 of the same physics record |
| Gravity | 9.81 m/s² | A constant: no record read gives a station's gravity |
| Snap down | 0.45 m | A constant: how far the feet follow the ground down, as on stairs |

What one frame does:

1. The stance changes. Standing up only happens where the standing capsule fits.
2. The horizontal velocity moves toward the wish, turned by the yaw. A jump starts if the player is grounded.
3. The frame is cut into sub-steps of at most 0.2 m.
4. Each sub-step applies gravity (half before the move and half after, so a jump peaks at its height at any frame
   rate). It sweeps the capsule and slides it along what it hits; on the ground a wall's normal is kept horizontal, so
   sliding never lifts the player. It then pushes the capsule out of anything it overlaps.
5. The sub-step then traces the ground: a ball from the bottom of the capsule down by the gap, plus the snap-down
   distance when the player was grounded, or plus the fall otherwise. Where the ball touches ground steeper than the limit,
   such as the foot of a wall, a ray straight down from the middle decides instead.
6. Ground within the limit puts the feet on it, unless the capsule does not fit above a step, in which case the step is
   a wall. Landing on a walkable surface with the capsule itself also counts as ground.

The tests, against boxes, run at 30 Hz. They check:

- standing still stays put;
- walking moves at the run speed;
- a wall stops the player, and a slanted one is slid along;
- 0.2 m stairs are climbed and walked down without leaving the ground;
- a 30-degree ramp is climbed and a 65-degree one is not;
- a jump peaks at 0.5 m and lands after 0.64 s;
- a ceiling stops a jump;
- crouching passes under a 1.4 m beam that stops a standing player, and the player stands up only once clear of it;
- walking off an edge falls and lands;
- the air gives no control.

Against Orbituary's data:

- the spawn point stands still on the floor;
- the spawn closet stops the player within 3 m to +Y, -Y and +X;
- toward -X, where the door is, the player walks out over 20 m of hangar floor without falling.

A frame costs about 0.1 to 0.3 ms per player in release.

## In the DGS

`movement::Movement` takes a client's report when it passes the checks below, and keeps its state where it was when it
does not. Without reports it simulates: `advance` calls `physics::step` when the hangar's world is built, and the flat
plane (with the grid clamp) otherwise. Removing `with_collision` in `control::player_entities` reverts to the flat plane
and to the speed check alone.

- `main` builds the collision of its system's home-location hangars off the async runtime at startup
  (`location::hangar_socpaks`). The join never builds it (`World::loaded`). A player who joins before it is ready walks
  on the flat plane, and the body takes over from the next frame after it is.
- The speeds are the client's measured 5.7 and 7.3 m/s (`RUN_SPEED`, `SPRINT_SPEED`), not the records' 5.58 and 7.2,
  so the server tracks what the client did. Jump starts on bit 1 of `InputFrame.jump`. `SStance` 2 crouches, and 3
  (prone, inferred) is treated as a crouch.
- The tick keeps stepping a player whose client has sent nothing for 100 ms, with its buttons until `INPUT_TIMEOUT`, so a
  jump or a fall lands. The next frame does not simulate that time again. It does not for a client that was reporting:
  its next report says where it went.
- A frame without a report within 0.25 s (client time) of one does not simulate either: a client in charge skips the
  report in some states (dead, staging, a synced teleport, jumping off; RE repo `GatherClientAuthoritativePosition`).
  After that the simulation takes over from the last report, with the body's velocity and stance.
- The published position changes only on a move of 1 mm or more. Against a wall the body settles by micrometres a
  frame. The simplified walls lean by fractions of a degree, so pushing on one creeps along it at about 2 cm/s.

What the client itself predicts for jumps, crouching and stairs has not been read from its code. The values are the
records the client loads too, but its stepping and ground rules may differ. The 0.5 m and 0.1 s of `Validate` make any
difference visible as a snap back. Places to watch:

- the spawn closet's door frame, which holds the capsule for about half a second;
- steps between 0.3 and 0.51 m;
- the 1.7 cm the body settles on spawning (floor at 4.493, spawn at 4.51).

## Input

The frontend decodes these from `EntityRemoteMethod` method 7, and the DGS turns them into `physics::Input`. All are
**confirmed** in 4.10.2 code, except where marked:

| Action | Data | What |
| --- | --- | --- |
| `0x58` `SMoveButtonMask` | u32 | 1 forward, 2 back, 4 left, 8 right (see `movement`) |
| `0x9c` | none | Jump start. Inferred to be `SJump`: it opens each of the three press sequences of the capture, and its writer was not read |
| `0x9d` `SJumpHold` | none | Every frame while Space is held (writer 0x1462a7010) |
| `0x9e` `SJumpRelease` | none | Space released (0x1462a7160) |
| `0x64` `SStance` | u8 `ESCActorResultStanceState` | Written by 0x145bb17c0. The C presses of the capture alternate 2 and 1, so 2 is crouch and 1 is stand. A single 3 was seen, inferred to be prone |

From the replicant log of `scbackend-shard-usa-010`, 10:21:25 to 10:22:07 UTC: Space at 10:21:33.09 (held about 0.9 s),
10:21:49.69 and 10:21:58.83, and 13 stance changes between 10:21:50 and 10:21:59.

- `0x4a` is not crouch. It changes all session long, with values 00, 01, 40, 41, 74, 7a, 81, c1 and e1.
- Variable 375 of the client's own `EntitySnapshotUpdate` does not follow the stance in this log. It changed only at
  10:21:45.150 (to `00 00`) and 10:22:11.755 (to `01 00`), with no `SStance` near either, so what it is stays open.
- After 10:28 the log has actions the frontend cannot size (`0x45`, `0x6f`, `0x94`, `0xac`).

## Client positions

Each `SClientAuthoritativePosition` the client sends is judged against the server's current state, in this order. A
report that fails is refused: the state stays where it was and nothing is sent. Every threshold is chosen to be lenient,
as a wrong refusal costs the server's state lagging behind and a wrong acceptance is what a real server also accepts.

| Check | Refused when | Why so |
| --- | --- | --- |
| Zone | the report's zone host is not the state's | Leaving the zone is not followed yet |
| Speed | farther than 2 x 7.3 m/s over the client's time since the last report taken (at most 5 s), plus 0.5 m | Twice the fastest speed, as `DoMovementCheatCheck` allows. Measured from the last report taken, so a client that hitched catches up in one report |
| Wall | the straight move of the feet is blocked by the collision at 0.3, 0.9 and 1.5 m above the feet, all three | A wall, floor or ceiling blocks every height. A beam, counter, stair riser or table blocks only some, and a player passes those crouched, prone, by its step or a mantle. A step onto a 0.6 m crate is not a crossing |
| Air | nothing within 0.3 m under the feet (a ball of radius 0.3 traced down from 0.5 m above them), and the feet higher than the last ground taken plus 3.13 t - 4.9 t²/2 + 0.5 m, t seconds after it | A jump's arc at half the gravity with a margin: a jump (0.5 m) and any fall stay well under it. Hovering 0.3 m or more above the ground is refused within about 1.4 s, and rising more than 1.5 m above the last ground at once |

Refused reports are not a dead end. Once they have lasted 3 s, the server takes the client's word again if the latest
report stands on something and no step between the refused reports went through the collision (steps over 2 m, after a
hitch, are not judged). So a client that hitched around a corner, or rode something the collision leaves out, is taken
again; one that walked through a wall is not, until it comes back to where the state is. The DGS logs the first refusal
of a run (`client position refused`, with the reason) and the report that ends it (`client position taken again`).

What a real server does, from the RE repo (`SCActorActionHandlerComponent.cpp`): with the client in charge it takes the
client's frames, and `DoMovementCheatCheck` (0x145bc4c40) compares the average speed of the reports over more than 1 s
with twice the fastest allowed, and only reports `ActorMovementAnomaly` to analytics. It corrects nothing, and no
collision check of the reports was found. The checks above are therefore this DGS's own policy. The client is not told
of a refusal: `Validate` does not run while `m_clientAuthoritative` is set (`GetMovementMode` 0x145bc96e0 answers
networked for any value but 0 and 8), so it ignores the server's state for its own actor. Correcting a client would need
clearing the flag so that `Validate` runs, or a teleport (`RmTokenForceTeleportInStaging`); neither is done.

Tests: a walk the server's own body makes out of the closet and around the hangar (sprint, jumps, crouching), reported
every frame, every 2 and every 4 frames (the 133 ms network frame), and 1.2 cm higher or 2 cm lower than the
simplified floor, is never refused; stairs, a jump, a 1 m drop and a 0.6 m crate are not; a walk through the closet's back
wall is refused until the client comes back; rising 0.1 m a frame and hovering at 1 m are refused and taken again on
landing; a hitch through a wall's end is taken again after 3 s.

Seen in the data while writing these: the closet's back wall (-Y, at y -86.2) is solid at every height traced, but toward
+Y the collision has a 0.5 m ledge and something up to about 1.2 m, with nothing at 1.5 m. Whether a window there is
glass the client collides with is not known; a report through it is not refused.

## Open

- What the client collides with that the collision lacks (entities: doors, elevators, the ship lift, a parked ship and
  its ramp) is unknown ground to the Air check: riding a lift up more than 1.5 m, or walking up a ship's ramp, is refused
  until the client stands on the collision again for 3 s. A ship is also a zone of its own, which the Zone check refuses.
- Validate the checks on the real game: walk, sprint, jump, crouch, prone, stairs, the closet's door and the hangar's
  edges, and look for `client position refused` in the DGS's log.

- The step height comes from the collider's gap: 0.51 m lets a standing player step onto knee-high crates. If the client
  climbs less, it is still pulled to the server's position, which is the server's call.
- Moving entities (doors, elevators, the ship lift) are not in the collision. A closed door does not stop the player yet.
- Every hangar socpak of `home_locations.tsv` (six for 12913470) is converted into the build and loaded by the DGSs of
  its system, but only Orbituary's has tests against its geometry (the spawn closet, the floor); the others are unchecked.
- Whether the player collides with group-0 hulls or with cluster meshes is not read from code.

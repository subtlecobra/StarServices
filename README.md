# scbackend

A private backend for Star Citizen, as one static Rust binary per service, built from reverse
engineering of the game client. Project rules are in [AGENTS.md](AGENTS.md).

## Architecture

```
game ── gRPC/TLS ──> Envoy Gateway ──┬─> sc-login ──(publish)──> sc-push
                     (GRPCRoutes)    ├─> sc-identity, sc-push, sc-configuration, sc-discipline
                                     └─> ...one binary per proto service, 44 in all
game ── TLS/TCP ───> sc-hub             legacy Hub: login, destinations, join routing
game ── UDP ───────> shard pods         sc-pu-frontend, opened and closed by sc-shard-operator
browser ─ HTTPS ──> Envoy Gateway ──┬─> sc-portal       register, log in, account, game login (HTTPRoutes)
                                    └─> sc-admin-web    /admin
```

- **Envoy Gateway** is the single gRPC endpoint the game requires. It terminates TLS and routes by gRPC
  service. Each service declares its routes in its values in StarServices-CN (`values/`). `sc-hub` and `sc-pu-frontend` use
  NodePorts: `TCPRoute` and `UDPRoute` are still experimental, and proxying would hide client addresses.
- **`sc-shard-operator`** runs each `ShardCluster`: a region's pool of shards. It keeps one empty shard
  warm, opens more as players arrive, and closes surplus shards once they have stayed empty. A shard
  with players is never closed. `sc-hub` asks it for a shard when a player joins.
- **Between services**: gRPC with generated tonic clients and servers. Tokens are verified locally with
  the shared secret, so authentication needs no network call.


## Status

| Stage | State |
|---|---|
| Service binaries, Alpine images, Helm charts, helmfile (in StarServices-CN) | Working, see the cluster notes below |
| All 44 gRPC services answer with the right streaming shape | Working, tested |
| Login through to character customization | Working with the real game, before the service split |
| Destination list and join routing | Passes against the test client; **not yet run with the real game** |
| Shard connect handshake | Accepts a request captured from the real game; the full handshake is being confirmed with it |
| Playable world on a shard | **Not implemented.** See [docs/ROADMAP.md](docs/ROADMAP.md) |

The test client follows the protocol as reverse engineered, so a pass shows the server matches that
understanding, not that the game accepts it. Open assumptions are listed in
the protocol notes, kept with the reverse-engineering work outside this repository.

## Run it

Building needs the `x86_64-unknown-linux-musl` target and gcc. The tools that read the game archive (`sc-admin`,
`gamedata-build`) also need the `p4k` crates, which Cargo fetches from
<https://github.com/subtlecobra/p4ktools> (pinned in their `Cargo.lock`).

### Game files

Nothing in this repository comes from the game: every file that does is generated from your own copy of it. Generate
them once after cloning, and again after a game patch, before building:

```sh
scripts/game-files.sh [<game folder>/Data.p4k]   # or SC_P4K in local.env; SC_GAME_EXE for another binary
```

It reads `Data.p4k` and `Bin64/StarCitizen.exe` beside it, never writes in the game's folder, and can be run again at
any time (a second run of the same build converts nothing new). In order, it writes:

- `proto/sc/`, the game's protocol definitions, with `scripts/protos.sh`: `tools/protodump` reads the descriptors
  embedded in the binary, recompiles what it writes and fails if the result differs from them. Every build needs
  them, and stops with a pointer to this script while they are missing.
- `target/gamedata/<build>/`, the archive's build converted by `tools/gamedata-build`, as for publishing (below).
- `services/dgs/testdata/`, the test data of `sc-dgs` (also read by `sc-pu-frontend`'s and `sc-admin`'s tests): the
  build's tables and the collision of the hangar the tests use, in the layout of a build directory.
- `services/pu-frontend/testdata/golden_binds.tsv`, every bind the shard sends, written by the wire tests from that
  test data and the code as it is. A golden file made this way is a snapshot of this machine's game build and of the
  code at that time: it does not say the binds are right, only that a later edit changed the wire. So it is written
  only when it is missing or the build changed; `--golden` writes it again after a deliberate change to the wire.

Git ignores all of them. Tests stop with a pointer to the script while the test data is missing; release builds and
images need only `proto/sc/`.

```sh
cargo build --release
(cd tools/sc-admin && cargo build --release)
```

The deployment (helmfile, environments, per-service values, kind config, `cluster-up.sh`) lives in
[StarServices-CN](../StarServices-CN), which only deploys what this repository pushes. The deploy host (this PC or
another) runs StarServices-CN's scripts and makes its own backend state; this PC only populates its registry:

```sh
../StarServices-CN/scripts/registry-up.sh       # on the deploy host: the cluster and its registry, ready for a push
scripts/publish.sh                              # here: images, charts, game data and seed bundle, into that registry
../StarServices-CN/scripts/cluster-up.sh        # on the deploy host: Secrets, helmfile sync; the sc-seed Job seeds
# scripts/seed.sh                               # the manual seed: straight from the archive, through port-forwards
<launcher>/sc-launch-private.sh                 # starts the game (the private client launcher, kept outside this repository)
../StarServices-CN/scripts/cluster-down.sh
```

The services need the databases, so the cluster is the only way to run them. Re-run `publish.sh` (it pushes only the images whose binary changed, and the charts that changed;
`SC_FORCE_PUSH=1` pushes all)
(with service names to build only those images) and `cluster-up.sh` after rebuilding. The client script builds its private tree and starts the game. It stops if the backend is not
reachable, and refuses to start next to a running game. `--dry-run` prints the plan, and `--prepare-only`
builds the tree without launching. Logs are in `kubectl -n scbackend logs`, where every unimplemented call
is a `WARN` line, and `drive_c/sc-private/Game.log` in the game's Wine prefix.

This uses a 7-node kind cluster (1 control-plane, 6 workers) on rootless Podman.

`deploy/charts/` holds the `sc-service` chart, which every service is a release of, `sc-edge` for the
Gateway, and `sc-shard-operator`. `build-and-push.sh` packages them and pushes them to
`oci://<registry>/scbackend/charts`, versioned from each `Chart.yaml`; a chart change that StarServices-CN
should pin separately needs a new version there and in its helmfile. YugabyteDB runs in the `scbackend-data`
namespace, and each region's ArangoDB in its region's namespace (below); [docs/DATA.md](docs/DATA.md) says what goes where.

Images are built with Podman. Images and charts go to the deploy environment's registry, which
`scripts/registry.sh` reads from StarServices-CN's `environments/dev/registry.yaml` (`SC_DEPLOY_REPO`,
default `../StarServices-CN`): `registry.alexandria.local:5000`, plain HTTP. `SC_REGISTRY` and
`SC_REGISTRY_PLAIN_HTTP` override it; until Alexandria runs, use `SC_REGISTRY=localhost:5001`, the local
`kind-registry` container that StarServices-CN's `cluster-up.sh` starts, on every script (its README has
the first-run order). A deploy host's cluster holds its own registry (StarServices-CN's `registry.yaml` mode
`in-cluster`), where anyone pulls and pushes need a login as `push`, with the password the host's `registry-up.sh`
made: `scripts/publish.sh` asks for it once and logs podman and helm in. Base images come from Docker Hub. Follow logs
with `kubectl -n scbackend logs -f deploy/sc-hub`. Stream idle and request timeouts are off in `sc-edge`,
because the game holds streams open for a whole session. The deploy host's
backend state (`state.json` and the certificates, which its `cluster-up.sh` makes) reaches the pods as the
`scbackend-data` Secret. `ca.key` stays out of the cluster.

Shard clusters are listed in the `sc-shard-operator` chart values, and `kubectl -n scbackend get shc` shows
them. Shards get random names, such as `usa-crimson-falcon`, and ids in the game's format, such as
`pub_usa_12913470_000`: shard cluster, game build and shard number. A new cluster needs its own region and port ranges, plus matching ports in StarServices-CN's kind config.

A shard cluster's region names a `ShardRegion` (`shardRegions` in the same values, `kubectl get shr`): the namespace
`scbackend-region-<name>` with its game data, a Valkey cluster (`spec.gamedata.cache`) that holds each listed game build
and the stateless `sc-gamedata` in front of it, which loads the builds from the registry and owns them, and the
region's entity database: an ArangoDeployment `arangodb` (`spec.entityDb`, run by a
kube-arangodb operator for that namespace) and `sc-entitygraph`, its owner. The operator makes and deletes all of it
with the region. A shard opens only once its build and its region's entity database are ready, and its DGSs get
`SC_GAMEDATA_ADDR`, `SC_GAME_BUILD` and their region's `SCBACKEND_ENTITYGRAPH`. A build that shards still run on stays
in the region until they close. The game reaches the `sc-entitygraph` of the region with `entityDb.clientRoute`
through the gateway; see [docs/DATA.md](docs/DATA.md).

`sc-hub` offers the star systems of a built-in 4.10.2 list, or of a `destinations.json` in the backend state
(`sc-admin init --p4k` writes one there, from the game archive).

## What the launcher leaves alone

It works in `C:\sc-private` inside the prefix. The big files are symlinks into LIVE, while
`client.crt`, `LoginData.json`, `user/` and the logs are private copies. LIVE, your real login token,
the Wine certificate store and the stock `sc-launch.sh` are not changed.

`EOS_USE_ANTICHEATCLIENTNULL=1` is set by this launcher only, and it refuses any backend that is not
on a loopback or private address. Keep it that way: the null anti-cheat client is for your own
instance, never for official servers.

## Portal and admin

`sc-portal` is the public website: players register, log in, manage their password and sessions, and download the
`LoginData.json` and `client.crt` their game needs to play on this server. The admin pages, `sc-admin-web`, are at
`/admin`. Accounts, passwords, roles and web sessions are sc-identity's. Make the first admin with
`scripts/identity-admin.sh grant-role <handle> admin`. Pages, the API, how a new player plays, what the security
covers and what it does not, and the deploy order are in [docs/PORTAL.md](docs/PORTAL.md).

The admin site (`services/admin`) finds an account and acts on what each owner keeps for its player: money
(sc-ledger), entitlements and ships (sc-entitlement, sc-rental), home and spawn location (sc-entity-document), and
inventories (sc-entitygraph: the tree of a player's inventories and containers, moving, adding by class and removing
items). It keeps none of that, only `admin_audit`, where every action is written before its owner is asked, with who,
what, when, and the before and after. Grants, money and inventory changes are keyed by the form's request id, so a
resubmitted form changes nothing more. Locations and inventories are refused while the player is on a shard, whose
DGS holds the live state; offline, locations apply on the next join and inventories at once. Who it asks for what is in
[docs/DATA.md](docs/DATA.md).

## Security

Keep the backend on loopback or a trusted LAN. Web accounts have passwords (see docs/PORTAL.md), but the game's
own services trust any token signed with the backend's secret, and a Hub login with an
unknown token falls back to the default account. `data/ca.key` can mint certificates that the private
tree trusts, so keep it private.

## Layout

| Path | Contents |
|---|---|
| `services/` | One crate and one binary per service |
| `crates/sc-proto` | Rust types, servers and clients generated from `proto/` |
| `crates/sc-runtime` | Shared options, logging, health, shutdown, tokens and TLS |
| `crates/sc-oci` | Pulls an image of files over the OCI distribution API, for `sc-gamedata` and `sc-seeder` |
| `crates/sc-seed` | The seed bundle format and its hand-over to the owners' seed RPCs, for `sc-admin` and `sc-seeder` |
| `tools/sc-admin` | Creates state, certificates, destinations and client files; reads the seed from the archive |
| `tools/protodump` | Regenerates `proto/sc/` from `StarCitizen.exe` |
| `tools/gamedata-build` | Converts `Data.p4k` into a build's game files (hangar collision) for the regions' game data |
| `proto/sc/` | Generated from your game binary by `scripts/game-files.sh` (`scripts/protos.sh`); not in the repository. Do not edit |
| `services/*/testdata/` | Test data and golden files from the game, generated by `scripts/game-files.sh`; not in the repository |
| `proto/scbackend/` | Hand-written Hub and internal messages. Those with required fields are proto2, in `hub_wire.proto` |
| `deploy/charts/` | Helm charts, pushed to the registry; deployed by StarServices-CN |
| `scripts/` | `game-files.sh` (generates every file from the game: `proto/sc/`, test data, golden files), `protos.sh` (generates `proto/sc/` alone), `build-and-push.sh`, `build-images.sh`, `seed.sh`, `seed-publish.sh` (with `seed-push.sh`), `gamedata-publish.sh` (with `gamedata-push.sh`), `gamedata-valkey-test.sh`, `identity-admin.sh` for web account roles and passwords, and `registry.sh` for the registry setting |
| `Dockerfile` | Alpine image for any service, with `--build-arg SERVICE=<binary>` |
| `tests/e2e.py` | End-to-end check with an independent client stack |

## After a game patch

```sh
scripts/game-files.sh            # proto/sc/, the build's game data, test data and golden files, from the patched game
```

`proto/sc/` is recompiled and checked against the embedded descriptors. Field numbers in `proto/scbackend/`, and the
addresses cited there, must be rechecked by hand. The golden files are written again for the new build from the code as it
is, so run the tests before patching when edits are pending, or the new snapshot takes in their wire changes unchecked.
Tests that expect values of a build (counts of home locations, hangar triangles) may need the new build's values.

Then publish the new build's game files, which each region's `sc-gamedata` loads from the registry into its Valkey:

```sh
scripts/gamedata-publish.sh   # the archive: an argument, or SC_P4K; --build, --out, --jobs
```

It converts `Data.p4k` with `tools/gamedata-build` into `target/gamedata/<build>/` (a manifest, the DGS's tables
under `tables/` and `collision/<socpak hash>.col` per hangar), and pushes it as `<registry>/scbackend/gamedata:<build>`,
where the build is the change number in `build_manifest.id` (12913470 for 4.10.2). No service image holds game data, so
a patch needs no image rebuild: the shards of the new build read its files from `sc-gamedata`. Publishing a build again
with other files changes its image digest, and each region's `sc-gamedata` loads it again within
`SC_GAMEDATA_RECHECK_SECS` (300) or at its next start; restart the DGSs (new shards) and sc-admin-web to read it. See
docs/DATA.md, Game data per region. Both steps also run alone:
`tools/gamedata-build/target/x86_64-unknown-linux-gnu/release/gamedata-build <Data.p4k> <out>` and
`scripts/gamedata-push.sh <out> [build]`. The tool links meshoptimizer (C++), so it is built for the host target, not
the repository's musl one. See docs/DGS_PHYSICS.md. `--seed` also publishes the build's seed bundle (below), so one
command publishes everything a cluster needs from the archive.

## Seeding

The game data the owners keep in their databases (the entity classes and the container tree in each region's
sc-entitygraph, and the starter entitlements, reputation, default blueprints, scenarios, mission contracts and rental
prices of the global services) is read from `Data.p4k`. Only the build machine has the archive, so the seed is split:

```sh
scripts/seed-publish.sh       # the archive: an argument, or SC_P4K; --build, --out, --rental-prices
```

runs `sc-admin seed-export <Data.p4k> target/seed`, which writes the seed bundle `target/seed/<build>/` (one file per
table, length-delimited protobuf rows of the owner's seed RPC, and `manifest.json` with each file's size, sha256 and
row count; about 30 MB for 4.10.2, the same bytes for the same archive), and pushes it with `scripts/seed-push.sh` as
`<registry>/scbackend/seed:<build>` (`FROM scratch`, files under `/seed/<build>/`, `--timestamp 0`, so the digest is
stable). The rental prices are StarServices-CN's `environments/<env>/rental-prices.json` when present (`SC_RENTAL_PRICES`).

The cluster applies it with the `sc-seed` chart (pushed with the others by `build-and-push.sh`): a Job, named after the
build, that runs `sc-seeder`. It pulls the bundle itself, checks the image's digests and every file's sha256, waits for
every ShardRegion's entity database to be Ready (it lists them through the API, with a ServiceAccount that may only get
and list ShardRegions), streams the classes and the tree to each region's sc-entitygraph
(`EntityGraphInternal.SeedEntityClasses`, `SeedContainers`), hands the other tables to their owners once, and exits 0.
Every owner upserts and the tree is replaced whole, so a repeat changes nothing; a new build is a new Job. A finished
Job is deleted after a day. Values: `build` (required), `registry` (as pods reach it), `repository` (`scbackend/seed`),
`plainHttp`, `registryIp`, `image` (`{registry, name, tag, pullPolicy}`, or a whole reference), `regions` (default all),
`entitygraph` (`name=address` instead of asking the API), `owners`, `waitSeconds`, `backoffLimit`,
`ttlSecondsAfterFinished`, `workSize`.

On a machine with both the archive and the cluster, `scripts/seed.sh` does it all at once: `sc-admin seed --p4k`
reads the archive and applies it through port-forwards, as the Job does. `sc-admin seed --bundle target/seed/<build>`
applies an exported bundle the same way, and `sc-seeder --bundle <dir> --entitygraph <name>=<address>` runs the Job's
binary outside a cluster.

## Tests

```sh
scripts/game-files.sh            # once: the test data and golden files come from your copy of the game
cargo test --workspace
python3 -m venv .venv && .venv/bin/pip install grpcio protobuf cryptography
tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin dump-descriptors --out /path/to/sc.binpb
tools/sc-admin/target/x86_64-unknown-linux-musl/release/sc-admin client-files --out /path/to/files
.venv/bin/python -I tests/e2e.py /path/to/files <RE client checkout>/client --descriptors /path/to/sc.binpb
```

The e2e runs against a running backend, local or in the cluster.

`scripts/gamedata-valkey-test.sh` runs `sc-gamedata`'s ignored tests against a real Valkey cluster in podman, made
with the shard operator's `valkey.conf`, `start.sh` and `init.sh`, and removes it afterwards. With
`--image <build> --expect <file>` it also runs the `sc-gamedata` binary on that cluster against the local registry and
checks that a file of the build comes back byte for byte.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or
conditions.

## Disclaimer

Star Citizen is a trademark of Cloud Imperium Rights LLC. This project is not affiliated with, endorsed by or
sponsored by Cloud Imperium Games. It contains no game files, code or assets: everything it needs from the game is read
from or generated with a copy of the game that the user owns, for interoperability.

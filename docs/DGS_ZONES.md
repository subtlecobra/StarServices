# DGS zones

The real territory config is server data and is not in the game files: the client names it
`star-services/territories/sta1-pyr1-nyx2.json`. This is our own division, from the object container tree in
ArangoDB. Weight is the static and dynamic entity counts of a location and everything under it. It measures content,
not load: real load follows players.

## Current layout

10 DGSs per shard, shipped as the `default` `ShardTemplate` under `shardTemplates` in the `sc-shard-operator` chart
values:

| System | Entities | DGSs | Entities per DGS |
|---|---|---|---|
| Stanton | 3,169,701 | 7 | about 453,000 |
| Pyro | 1,087,590 | 2 | about 544,000 |
| Nyx | 775,818 | 1 | 775,818 |

| DGS | Roots | Containers | Entities |
|---|---|---|---|
| `stanton-1` | `stanton1`: the planet, its surface and `stanton1b` | 22,887 | 539,360 |
| `stanton-2` | `stanton1a`, `stanton1c`, `stanton1d` | 16,294 | 504,262 |
| `stanton-3` | `stanton2`, which keeps `stanton2a`, and the Lagrange points of `stanton2` | 15,356 | 426,717 |
| `stanton-4` | `stanton2b`, `stanton2c`, the Lagrange points of `stanton1`, the Terra jump point | 12,436 | 428,716 |
| `stanton-5` | `stanton4`, which keeps `stanton4b` | 15,223 | 411,523 |
| `stanton-6` | `stanton4a`, `stanton4c`, the Lagrange points of `stanton3` and `stanton4`, the Pyro and Magnus jump points | 10,400 | 394,445 |
| `stanton-7` | `stanton3`, and `stantonsystem` for what no other root claims | 13,011 | 464,678 |
| `pyro-1` | `pyro1`, `pyro2`, `pyro3`, and `pyrosystem` for what no other root claims | 35,769 | 553,438 |
| `pyro-2` | `pyro5`, `pyro6`, their Lagrange points, both jump points, and the `ab_pyro_final_set_shared_02_001`, `_shared_03_001`, `_encounter-001` and `ab_mine_pyro_cloud_sml_001` sites | 31,117 | 534,152 |
| `nyx-1` | `nyxsystem` | 25,317 | 775,818 |

Measured on the 4.10.2 tree: the ten add up to all 5,033,109 entities, and to every container but the universe root
and the three system nodes, which hold none.

## ShardTemplate

A ShardCluster names a `ShardTemplate` in `shardTemplate`, or gets `default`. It lists the DGSs of each system and their
roots:

- A root is a `name` from `containers`: the socpak file name without its extension. See [DATA.md](DATA.md).
- A root takes every container of that name in the system, and everything under them. Some names are placed many
  times, such as `ab_mine_pyro_cloud_sml_001`.
- A container belongs to the nearest root at or above it, across all DGSs of the system. That is how a body is split:
  `stanton-1` has `stanton1`, and `stanton-2` carves three moons out of it.
- A root is in exactly one DGS. The operator checks this and that systems are known, and leaves the DGSs of a shard
  alone while its template is missing or invalid.
- That the roots cover a system can only be told from the tree. A DGS with a root the tree lacks logs it and stays
  unready. A container under no root of a system has no DGS, which a root like `stantonsystem` prevents.

### Loading an area

sc-entitygraph owns `containers` and `container_edges`, so a DGS has no database access: it asks
`EntityGraphInternal.LoadArea` in `internal.proto` for its area, with its system, roots, the roots of the other DGSs of
the system as `excluded_roots`, and `SCBACKEND_COLLECTION_PREFIX` to read a test copy. sc-entitygraph finds the roots,
answering NOT_FOUND with the names of those the tree lacks, then walks down one level at a time with two plain queries per
level, not entering an excluded root, and streams each container's first entity id and entity count, parents first, 4,096
to a message (about 60 KB). The DGS keeps an id range per container. Until the load succeeds the DGS logs `area not
loaded` and asks again every 15 seconds, giving up an attempt after 300; its gRPC port, and so its readiness, opens
after.

The walk is the same queries the DGS ran itself before. On the dev cluster `pyro-1`, the largest area by containers
(35,769 containers, 553,438 entities), was ready 5.4 s after its container started, with the other nine loading beside it.
Streaming an area of that size through gRPC takes about 30 ms (`the_largest_area_arrives_whole_and_quickly` in
`services/dgs/src/area.rs`), so a load stays at about the time of the queries. The DGS logs `area loaded` with
`elapsed`.

A shard keeps the template it opened with. Changing the roots in a template replaces the DGS pods whose area changed.

The replicant gets the DGSs of its shard in `SCBACKEND_DGS_AREAS`, `<system>:<name>=<root>,<root>` separated by spaces,
when its pod is made. A player's home is governed by the DGS with its planet as a root or, with no planet, the
`<system>system` root, and that DGS's name is what the debug overlay shows, as `pub-usa-sc-4102-12913470-dgs-pyro-1`. A home
on a body that is no root of its own has no DGS, as telling those apart takes the tree, and a join there holds. None of the
home locations is one.

## Who owns what

The replicant keeps the protocol: connections, join stages and writing binds. What an entity is, is the DGS's to say:
`location.rs`, `player.rs`, `solar_system.rs`, `universe.rs` and the table of home locations are in `services/dgs`. They share
no crate, as `EntityBind` in `internal.proto` is all that passes between them.

| Entities | Owner |
|---|---|
| A solar system and its quantum grid | The DGS with the `<system>system` root |
| A planet | The DGS with the planet as a root. It is the shard's, so it stays when a player leaves |
| A hangar and the player in it | The DGS governing the hangar's planet, or the `<system>system` root with no planet |
| A player with no hangar | The `<system>system` root of Pyro, where it waits |
| A player's movement state | The DGS that holds the player. It moves it from the client's input, and a join puts it back at the spawn point |
| The game rules and game mode | No territory has them. Every `<system>system` DGS builds the same ones, and the replicant takes the first |

This is ownership on the server's side. No client gets authority, nothing about it is sent to a client, and how a running
client marks authority on entities has not been read.

`DgsControl` in `internal.proto`:

- `LocateHome`, asked of any DGS, as the table is theirs: what governs a home, which `Directory` turns into a DGS.
- `ShardEntities`, asked of each system's DGS for every connection: the batches that go to every client.
- `JoinPlayer`, asked of the governing DGS: the player's planet, hangar and player, which it records under the account.
- `LeavePlayer`, when the connection ends: by timeout, or a new handshake at its address. It frees the hangar and player. A
  session that was taken over by a reconnect holds nothing, so its late leave changes nothing.
- `Status`, polled every 5 seconds for the DGSs that hold a player here: frame times, population and `boot_id`.
- `PlayerInput`, for a player in game: the client's input frames, as the replicant decodes them, and the movement state
  they lead to, which the replicant sends to the client. See Player movement in the protocol notes, kept with the reverse-engineering work outside this repository.

The record is in memory. A restarted DGS has a new `boot_id`, and the replicant calls `JoinPlayer` again for its players,
which gives the same entities. A restarted replicant has a new `replicant` value, and a DGS frees what it held for the old
one at the next join.

### When a DGS is not there

A join never falls back on building entities in the replicant. The calls run beside the packet loop, so pongs keep going, and
a stage whose binds have not come waits, asking every 3 seconds and logging `join held for a DGS, asking again`. The client
waits up to 720 seconds in that stage. All three `<system>system` DGSs are needed for the first batch, and the governing DGS
for the solar systems' batch, which holds the planet. Any one DGS is enough to locate a home. Allocation does not wait for
DGSs, so a client sent to a shard whose DGSs are still loading waits there. A shard with no usable template has no DGS, so
its joins hold for good, where before they completed.

A DGS that goes away from a player in game shows as `Server FPS 0.0` and a server population of 0 until it comes back.

### Tick

A DGS runs a loop at 30 Hz and measures the time between frames, over windows of 150, in `frames.rs`. Players move on their
clients' frames, as they come, over the client's own time between frames, so that the state matches what the client did;
the tick only stops a player whose client has sent nothing for 0.25 s (`movement.rs`). See Debug overlay in the protocol notes for how this reaches a client.

## Shard namespaces

ShardClusters, ShardTemplates and Shards live in `scbackend`. Each shard's replicant pod, NodePort Service and DGS pods and Services
live in a namespace of its own, `scbackend-shard-<region>-<short id>`, such as `scbackend-shard-usa-010` for
`pub_usa_12913470_010`. A region that does not fit a DNS label is cleaned and cut, with a hash of it added. The name is
stored in `spec.namespace` when the shard opens, and `kubectl get shd` shows it from the status. Namespaces carry
`app.kubernetes.io/part-of`, `scbackend.io/shardcluster`, `scbackend.io/shard`, `scbackend.io/region` and
`scbackend.io/slot`.

DGS pods reach their region's sc-entitygraph, `http://sc-entitygraph.scbackend-region-<region>.svc:50051` (the operator
runs it, see docs/DATA.md), and sc-character-customizer at `dgsCustomizer` in the chart values, as full service names,
and are given no database address. The replicant reaches each DGS through a
ClusterIP Service with the pod's name, `dgs-<name>`, on port 50051, and is told how in `SCBACKEND_DGS_ADDRESS`,
`http://dgs-{name}.<namespace>.svc:50051`. A Service per DGS and not one headless Service, as it selects by labels DGS pods
have always had, so a running shard gets its Services with no pod changed, and a restarted pod keeps its address. A
`NetworkPolicy` named `dgs` selects the DGS pods and admits only the replicant, on that port. It is the only policy in a
shard namespace and covers ingress of DGS pods alone: nothing limits what the pods reach, and the replicant's own pod stays
open to its clients and to the operator, which reads its status port by pod IP and a DGS's readiness from its pod.
A running shard gets the Services, the policy and a bigger quota on the next pass. Shards still in `scbackend` get none. Shard pods mount no Secret or ConfigMap and have no service account token, so nothing needs
copying into the namespace.

A `ResourceQuota` named `shard` follows from the pods and Services of a shard: the replicant, each DGS and one spare pod,
by their requests and limits, a Service for each of those, and a single NodePort. With 10 DGSs that is 12 pods, 12 Services,
65m CPU and 192Mi requested, and 1536Mi of limits. It catches runaway pods and is not there to divide the nodes, which these sizes barely touch. It follows the DGS
count of the template.

### Closing

Owner references cannot cross namespaces, so a Shard holds the finalizer `scbackend.io/shard-namespace`. Deleting it
deletes its namespace, which removes everything inside. The Shard goes once the namespace is gone, and shows the phase
`Closing` and a message until then. This runs apart from the ShardClusters, so a deleted ShardCluster's shards still
finish, but it needs the operator: with the operator uninstalled, Shards wait forever. A namespace not labelled as the
Shard's is never deleted.

A slot stays taken while its closing Shard, its namespace or its Service remains, as NodePorts are global and the
namespace name comes from the slot. New shards take the next free slot instead of waiting, so a namespace stuck
Terminating costs one slot and allocation goes on. To see one: `kubectl get shd`, and `kubectl get ns <name> -o yaml`.
A live Shard whose namespace is deleted or taken by another Shard closes too, and is replaced the same way.

### Draining

A draining shard takes no new players and closes once it has been empty for a minute, which covers a player the
restarted operator has forgotten about. One that runs and does not answer may hold players, so it stays until someone
deletes it. Its replacement opens at once in another slot. A shard drains when it:

- has no `spec.namespace`, from before shards had namespaces. It runs on in `scbackend`, where its objects are still
  owned by the Shard, and needs no finalizer.
- has an id of an older game build.
- has the annotation `scbackend.io/drain`. The deployment sets this after a deploy, once the
  new operator runs and its old pod is gone, on the shards opened before that pod was made, so they are replaced by
  shards from the new images and settings. Marked earlier, the old operator would open the replacements with its own
  settings. A shard with players keeps its old images until it empties.
  `kubectl -n scbackend delete shd <name>` replaces one at once, players or not.

Each replicant and DGS pod carries `scbackend.io/pod-hash`, a hash of the manifest the operator made it from (image,
environment, resources, labels, area: everything its settings, the shard's region and spec put in it, not its owner
references). A pod whose hash differs from what the operator would make now, or that has none, is deleted and made
again, but only while nobody is on the shard or reserved for it and it has been seen empty for a minute, as for a
draining shard's close. A shard with players keeps its pods until it drains. This fixes the pods of a shard an older
operator opened with other settings.

## Later: a cap of 150,000 entities per DGS

A DGS would hold at most 150,000 entities, giving 38 DGSs per shard:

| System | Entities | Minimum | DGSs | Heaviest | Lightest |
|---|---|---|---|---|---|
| Stanton | 3,169,701 | 22 | 24 | 150,000 | 4,348 |
| Pyro | 1,087,590 | 8 | 8 | 149,998 | 93,387 |
| Nyx | 775,818 | 6 | 6 | 149,938 | 27,023 |

Rule: a body over the cap gets DGSs of its own, split by its children, so its pieces never share a DGS with another
body. Everything smaller is packed together. Crossing a DGS boundary costs a handover, so locations stay together where
they fit. Ignoring bodies would save 2 DGSs in Stanton, for 36 in total.

| Body | Entities | DGSs |
|---|---|---|
| `stanton1` | 1,043,622 | 7 |
| `stanton2` | 789,616 | 6 |
| `stanton4` | 718,087 | 5 |
| `stanton3` | 464,067 | 4 |
| Other Stanton locations | about 154,000 | 2 |
| `pyro5` | 244,231 | 2 |
| Other Pyro locations | about 843,000 | 6 |
| Nyx, mostly the Glaciem ring | 775,818 | 6 |

Packing here is by weight. Nyx and the shared bins should still be redrawn by position, so that neighbours share a DGS,
which needs the positions this analysis did not read.

## Unconfirmed

- `streaming_movable_count` is inferred, so the weights are approximate.
- The cap counts content, so a DGS at the limit is not necessarily busy, and an empty one may be.
- Dynamic workers for instances and streaming groups, which the client also has, are not planned here.

Inside a shard's namespace the replicant's pod and Service are `replicant-server`, and each DGS pod is
`dgs-<system>-<number>`, such as `dgs-stanton-1`.


# Data placement

Two distributed databases, chosen on 2026-11-06 for being actively released, resilient to split-brain
and operable on Kubernetes. ArangoDB is not open source: since 3.12 it is BUSL 1.1 with a Community
License that caps a deployment at 100 GiB and forbids redistribution. That is acceptable for a private
server, and would need revisiting if this is ever hosted for others or outgrows the cap.

| Database | Role | Why |
|---|---|---|
| YugabyteDB | Relational | Apache-2.0 core, PostgreSQL-compatible, distributed ACID transactions. CockroachDB's self-hosted edition moved to a restrictive licence, and TiDB is MySQL-flavoured. |
| ArangoDB | Graph | Shards by document key, so entities spread across servers, with a Raft-backed CP cluster and a first-party Kubernetes operator. It also stores documents. Dgraph shards by predicate, and every entity shares the same few predicates. NebulaGraph's last release was May 2024. JanusGraph needs a separate storage cluster. |

The game data of each region (files derived from a game build, such as hangar collision, not player data) is in a
Valkey cluster of the region, owned by `sc-gamedata`: see Game data per region below.

## The rule

- If it needs a transaction across rows, an audit trail, or a uniqueness guarantee, it is relational.
- If it is about what contains, owns or connects to what, and is read by walking relationships, it is
  graph.
- Each record has one owner. The link between the databases is the GEID, which both store.
- Only the owning service reads or writes its tables and collections. Another service asks it over gRPC, through an
  internal service in `proto/scbackend/internal.proto` that no GRPCRoute lists, such as:

| Asks | Owner | For |
|---|---|---|
| sc-dgs | entitygraph of its shard's region, `EntityGraphInternal.LoadArea` | The containers of its area. A DGS has no database access |
| entity_subscription | entitygraph of `entityRegion`, `EntityGraphInternal.GetOwners` | The owners of the entities a query names |
| delivery | identity, `IdentityInternal.GetAccounts` | The account a completed delivery is pushed to |
| sc-portal, sc-admin-web, sc-admin | identity, `IdentityInternal` (`RegisterAccount`, `PasswordLogin`, `VerifyWebSession`, `SearchAccounts`, `SetAccountState` and the rest) | Web accounts: passwords, roles, web sessions, game tokens. sc-portal keeps no data and sc-admin-web only its audit, see docs/PORTAL.md |
| social, contacts, friends | hub, `HubInternal.GetPlayerShard` | The shard a player is on, with its player counts |
| sc-dgs | character_customizer, `CharacterCustomizerInternal.GetCustomization` | A joining player's appearance |
| sc-dgs | sc-gamedata of its shard's region, `GameData.GetFile` | The files of its build: the home location tables and its hangars' collision. A DGS has no Valkey access |
| sc-admin-web | sc-gamedata of `entityRegion`, `GameData.GetFile` | `tables/home_locations.tsv` of the environment's build, the list of home locations |
| sc-shard-operator | sc-gamedata of each region, `GameData.ListBuilds` | Which builds are complete in the region, which gates its shards |
| chat, squad, custom_lobby, rtc, presence | social, `SocialInternal` | Groups and members; squad and custom_lobby also create, join, move between teams and end them |
| social | chat, squad, custom_lobby, rtc, `GroupDataInternal.DeleteGroupData` | Deleting the rows of a group social destroyed |
| social, contacts | presence, `PresenceInternal.GetPresences` | Presence as others see it, for group members and contacts |
| social, presence | contacts, `ContactsInternal.IsBlocked`, `ListFriends` | Blocks, for invitations and direct messages; friends, to announce presence |
| friends | contacts, `ContactsService` with the caller's token | Everything: FriendService is the older face of ContactsService and keeps no data |
| sc-admin-web | identity, `IdentityInternal.VerifyWebSession`, `GetAccount`, `SearchAccounts`, `SetAccountState` | The admin's session and role; finding, disabling, re-roling and resetting accounts; an account's player (`AccountSummary.geid`) |
| sc-admin-web | ledger, `LedgerInternal.Balances`, `Adjust`, `Operations` | Balances, keyed credits and debits with a reason, and the history of keyed operations |
| sc-admin-web | entitlement, `EntitlementInternal.QueryEntitlements`, `GrantPurchase`, `RevokeEntitlement`, `EntitlementHistory`, `SetRentalExpiry` | Entitlements, granted as a purchase is and revoked, with their history |
| sc-admin-web | rental, `RentalInternal.ListRentals`, `CreateRental`, `ExtendRental`, `EndRental` | Rentals, made with a granted rental, renewed without a charge, or ended now |
| sc-admin-web | entity_document, `EntityDocumentInternal.GetDocuments`, `PutDocument` | `player_home_location`, `player_spawn_location` and a ship's `vehicle_state`; written only while the player is on no shard |
| sc-admin-web | hub, `HubInternal.GetPlayerShard`; presence, `PresenceInternal.GetPresences` | Whether the player is on a shard, where the DGS holds their live state, or in the menu |
| sc-admin-web | entitygraph of `entityRegion`, `EntityGraphInternal.AdminInventoryTree`, `AdminMoveItem`, `AdminAddItem`, `AdminRemoveItem`, `AdminSearchEntityClasses` | A player's inventories and containers with their items; moving (or splitting), adding by entity class and removing items, as the player's own command is checked, once per command id; the seeded entity classes. See [INVENTORY.md](INVENTORY.md) |

## YugabyteDB

| Service | Data |
|---|---|
| identity, login | Accounts, characters, handles, session history. identity also owns the web accounts: `account_credentials` (email, password hash, disabled), `account_roles`, `web_sessions`, `account_audit` |
| hub | `player_shards`, the shard each connected player was sent to, and `persistent_items` |
| ledger | Balances (`ledgers`) and keyed debits, refunds and admin adjustments (`ledger_operations`, with the reason, who asked and the balance before and after an adjustment). Money needs strict ACID |
| entitlement | Ownership records (`entitlements`, `entitlement_history`) and the starter catalogue (`starter_entitlements`). sc-entitlement-v2 owns only its snapshots and reads entitlements through v1 |
| eatransaction | Its purchase journal only (`ea_purchases`, and `ea_transactions` from before it orchestrated) |
| rental | Rentals (`rentals`, `rental_history`) and keyed extensions (`rental_commands`) |
| delivery | Pending deliveries |
| entity_document | Per-entity documents as protobuf bytes, such as `player_home_location` and `player_tutorial`, with Prepare and Commit |
| character_customizer | Saved customizations |
| reputation, achievements, stats, blueprint_library | Per-player progress |
| mission_service, mission_scenario, contract_broker, mission_token | Mission and contract state |
| social | Groups, members and invitations (`social_groups`, `social_group_members`, `social_invitations`), and the cleanups still owed for destroyed groups (`social_group_cleanups`) |
| contacts | Relationships, friend requests and notes (`contact_relationships`, `contact_friend_requests`, `contact_notes`). A friendship is two rows written together. friends keeps no data |
| chat, squad, custom_lobby, rtc | Rows per group: `chat_messages`, `squads`, `custom_lobbies`, `rtc_channels` and `rtc_calls` |
| presence | `player_presence` |
| notifications | `notifications` |
| discipline, solar_system_shop, land_claim, configuration, dynamic_game_assets | Sanctions, shop inventory, claims, settings, screen assets |
| admin_web | `admin_audit`: every admin action, who, what, to whom, when, the owner asked, before and after, and the outcome |

## ArangoDB

One ArangoDB per region, see [One entity database per region](#one-entity-database-per-region).

| Service | Data |
|---|---|
| entitygraph | Entities and edges: the `Node` and `EntityEdge` model, stow contexts, and `Move`. `entitygraph_meta` also keeps the counter of item GEIDs it allocates (from 400,000,000,000), and `applied_commands` the answer of each admin command |
| entitygraph | Inventories and containers: a ship holds a locker that holds a weapon that holds a magazine. Personal, location, ship and container inventories, capacity and who may use them: see [INVENTORY.md](INVENTORY.md) |
| entity_subscription | What a client follows in that graph, `entity_subscriptions`. An entity's owner, which also follows it, comes from entitygraph. For now in the ArangoDB of `entityRegion` |
| instance_manager | Instances. For now in the ArangoDB of `entityRegion` |

Queries such as `GetEntityStowContextsByGeids`, `InventoryQuery` and `ContainerQuery` walk this structure
to arbitrary depth, which is what a graph database is for.

## Object container tree

`sc-admin seed-export` reads the tree from the game archive into the seed bundle, and sc-entitygraph, its owner, builds
`containers` and `container_edges` from what `EntityGraphInternal.SeedContainers` streams to it (sc-seeder, or
`sc-admin seed`; `sc-admin seed-containers` sends only the tree). The tree is replaced whole, so rows that a build no
longer has are removed.

The client asks when `IsShardPersisted` is 1: `ContainerQuery` for the nodes labelled `UNIVERSE_ROOT` with the
tree projection off, then `ContainerQueryStream` from each. It needs a node with that label in the stream, or
it fails with `Failed to find graph root in response`.

The tree has three kinds of node, and an edge goes from child to parent:

- The universe root.
- One node per solar system of `MegaMap.PU_All`, with the `SSolarSystem` record GUID as its `guid`. The client
  turns that GUID into the location id, so `Level.universeLocationUniqueId` is not stored here.
- A node for each socpak in that record, and below it one per `<Child>` of a `ChildObjectContainers` list. A
  node's children are those its parent listed inline, plus those of its own file that were not, by GUID.

For 4.10.2 that gives 197,814 nodes, the `nodeCount` the client logged on the live service.

| Field | Source |
|---|---|
| `geid` | Generated in walk order from `0x2e90cf4b80`, above the client's local range. A new build can renumber it |
| `guid` | `guid` of the `<Child>`. Empty on the root and on the socpak nodes under a system: see below |
| `socpak_file_path_hash` | CRC-32C of the path, lower case with forward slashes, from `objectcontainers` on. 0 with no socpak, and for a `<Child>` naming a socpak that is not in the archive: see below |
| `socpak_content_hash` | CRC-32C of the whole socpak file |
| `identifiers.start_id` | Generated: the ids that follow the node's GEID. The client derives three ranges from it, in the order unstreamable, immovable, movable |
| `identifiers.has_planet` | `entityCountPLA` is not 0 |
| `identifiers.streaming_immovable_count` | `entityCountStatic`. It equals the `<Entity>` count of the `.soc` in 9,662 of 9,668 socpaks, and the client takes one id per such entity |
| `identifiers.streaming_movable_count` | `entityCountDynamic`. Inferred from the name: it does not equal the `.entxml` count |
| `name` | File name of the socpak without its extension, such as `stanton1`, for a `ShardTemplate` root. Not unique, as a socpak can be placed many times, and empty with no socpak. Not sent to the client |
| `system` | `stanton`, `pyro` or `nyx`, from the system's socpak, and empty on the universe root. Not sent to the client |
| `labels` | `UNIVERSE_ROOT` and `STAR_SYSTEM` by position, `CONTAINER` on every other node, `HAS_PHYSICS_GRID` with a `<PhysicsGrid>` element. The client reads only labels 3 and 4 |

`ContainerQueryStream` with no prune or filter, which is the client's request, reads one level per query.
On the dev cluster that takes 8 s for the whole universe, in 198 messages of at most 95 KB, where a graph
traversal took 346 s.

With no source, so left empty:

- `identifiers.unstreamable_count`: always 0.
- `guid` of the socpak node under a solar system: the client derives it from the system GUID and the path, by
  a call that has not been read yet.
- `guid` of the universe root.
- The `LOCATION` label, `generation`, `created_at` and `updated_at`.

`socpak_file_path_hash` must be 0 or a hash the client knows. It lists `data/objectcontainers/*.*` at start and
keeps an entry per socpak, and `ProcessHierarchyNode` reads through the entry for any other value with no check
(0x142831410), so an unknown hash crashes it. A `<Child>` can name a socpak that is not in the archive, which two
do in 4.10.2 (`apexsandvalakkarevent`, `ow_entry_a18`). Those nodes keep their GUID and send 0, as
`GenerateObjectContainerCRCImpl` gives on the client for such a path. The client then finds the name in the parent's
XML and makes a placeholder entry (`GetOrCreateMissingObjectContainerData`).

`socpak_content_hash` is what the archive's central directory holds as each entry's CRC field: all 196,281 nodes
with a file agree. In the client only `SeedingProcessor` and the code that sends the graph read it.

## Ownership

Each table has one service that creates it and is the only one to read or write it. Any other service asks
that one over gRPC, through the `*Internal` services of `proto/scbackend/internal.proto`, which share the
owner's port and are listed by no GRPCRoute, so the game client cannot reach them.

### A purchase

sc-eatransaction owns no money, entitlement, rental or price. A rental or a renewal is a sequence of calls
to their owners, recorded in its own `ea_purchases` so that it ends charged and granted, or neither. Every
call carries the key `eatransaction:<geid>:<command id>`, and each owner applies a key once: the ledger
records it in `ledger_operations` with the balance change, the entitlement's URN is derived from it, and the
rental records an extension in `rental_commands`.

1. Price: the rental (`RentalInternal.GetRental`) for a renewal, then the shop's offerings
   (`SolarSystemShopService.GetGlobalInventory`). A refusal here charges nothing.
2. Journal the order as `pending`, with the rental id and, for a new rental, its expiry.
3. Charge: `LedgerInternal.Debit`. Refused (insufficient funds): `failed`. No answer: `cancelling`, then
   `LedgerInternal.Refund` with the same key, which gives back a debit that did happen, and spends the key
   if it did not, so a late debit cannot charge after all. Then `failed`.
4. Once charged, only forward: `EntitlementInternal.GrantPurchase` then `RentalInternal.CreateRental` for a
   rental, `RentalInternal.ExtendRental` then `EntitlementInternal.SetRentalExpiry` (reactivating a revoked
   entitlement) for a renewal. If an owner does not answer, the purchase stays `charged` and the client is
   told so. Its retry of the command, or the loop that resumes purchases idle for 30 s, finishes it.

So money is never granted before it is taken, and taken money always ends granted or refunded. Compensation
is only ever a refund of a charge whose outcome is unknown. Nothing is revoked, because nothing is granted
before the ledger confirms. Purchases journalled in `ea_transactions` before this were complete, and a retry
of one still returns its rental.

sc-rental ends rentals that have run out by asking sc-entitlement to revoke their entitlements
(`RevokeExpiredRentals`, which leaves one whose expiry has moved on) and only then marking them finished,
so a failure leaves them for the next sweep. Starting a rental gives its entitlement the new expiry through
`SetRentalExpiry`.

### A group's end

Groups belong to sc-social, and the rows that hang off a group to chat, squad, custom_lobby and rtc. The
change that destroys a group (and its teams) writes one row per service to `social_group_cleanups` in the
same transaction. A worker in sc-social then calls `GroupDataInternal.DeleteGroupData` on each service, at
once after the commit and again every 30 s, and drops the row once that service has answered. A service that
is away is asked again later, and asking twice is harmless. `SCBACKEND_GROUP_DATA` lists the services, as
`name=address` pairs.

A service writing a row for a group asks sc-social afterwards whether the group still exists, and deletes the
row itself if not. A row written before the cleanup ran is deleted by the cleanup, and one written after it
finds the group gone, so none outlives its group. squad and custom_lobby create the group first and their row
second: if their part fails, they ask sc-social to destroy the group again.

### Game data

`sc-admin seed-export` reads the archive on the build machine and writes each table's rows into the build's seed bundle,
and sc-seeder (the `sc-seed` chart's Job) or `sc-admin seed` hands them to their owners, in messages of at most 4 MiB.
Neither touches a database:

| Rows | Owner and RPC |
|---|---|
| `entity_classes`, replaced by GUID | sc-entitygraph of every region, `EntityGraphInternal.SeedEntityClasses` (client stream) |
| `containers` and `container_edges`, replaced whole | sc-entitygraph of every region, `EntityGraphInternal.SeedContainers` (client stream; written once the stream ends) |
| `starter_entitlements` | sc-entitlement, `EntitlementInternal.SeedStarterEntitlements` |
| `reputation_scopes`, `reputation_standings`, `reputation_ladder_standings` | sc-reputation, `ReputationInternal.SeedReputation` |
| `blueprint_defaults` | sc-blueprint-library, `BlueprintLibraryInternal.SeedBlueprintDefaults` |
| `scenarios`, only added | sc-mission-scenario, `MissionScenarioInternal.SeedScenarios` |
| `mission_contracts` | sc-contract-broker, `ContractBrokerInternal.SeedMissionContracts` |
| `shop_inventory` items, only added (an item an operator changed keeps its prices) | sc-solar-system-shop, `SolarSystemShopInternal.SeedShopItems`, before the rental prices |
| `shop_inventory` rental offerings, from the price list | sc-solar-system-shop, `SolarSystemShopInternal.SeedRentalPrices` |

The star systems, `destinations.json`, which `sc-admin init` writes, still reach the pods in the `scbackend-data`
Secret, not through the seed.

The seed bundle (`crates/sc-seed`) is the directory `<out>/<build>/`, pushed as `<registry>/scbackend/seed:<build>` with
the files under `/seed/<build>/`:

| Path | Rows, as length-delimited protobuf messages |
|---|---|
| `manifest.json` | `format` (2), `build`, and each other file's `path`, `size`, `sha256` and `rows`. Written last |
| `entitygraph/entity_classes.pb` | `EntityClassSeed` |
| `entitygraph/containers.pb` | `ContainerSeed`, parents before children |
| `entitlement/starter_entitlements.pb` | `StarterEntitlement` |
| `reputation/scopes.pb`, `standings.pb`, `ladder.pb` | `ReputationScope`, `ReputationStanding`, `ReputationLadderStanding` |
| `blueprint-library/blueprint_defaults.pb` | `BlueprintDefault` |
| `mission-scenario/scenarios.pb` | `ScenarioSeed` |
| `contract-broker/mission_contracts.pb` | `MissionContractSeed` |
| `solar-system-shop/rental_prices.pb` | `RentalPrice`, from the price list given to the export; empty without one |
| `solar-system-shop/shop_items.pb` | `ShopItem`, from `Scripts/ShopInventories/` (sale files left out, a priced listing wins) |

For 4.10.2 (build 12913470): 29,693 classes, 197,814 containers, 374 starter entitlements, 46 scopes, 355 standings,
377 ladder rows, 9 blueprints, 15 scenarios, 2,239 contracts; 30.6 MB, an 8.1 MB layer. The same archive gives the same
bytes. A reader refuses another format (format 1, before the shop items, included), a file the manifest does not list or one whose size or sha256 differs.

sc-seeder finds the regions through the Kubernetes API: every ShardRegion (or those its `regions` value names), once
`status.entityDb.state` is Ready, at `status.entityDb.service`. `scripts/seed.sh`, on a machine with the archive,
port-forwards the sc-entitygraph of each ShardRegion (all of them, or those named with `--region`), whose entity
database must be Ready, and the six owners, and runs `sc-admin seed`.

## What crosses the boundary

A purchase changes both: the ledger entry in YugabyteDB and the new entity in ArangoDB. There is no
transaction across them, so the relational write goes first and the entity is created from it
idempotently, keyed by the GEID. A retry finishes the job rather than charging twice.

## Measured on the dev cluster

ArangoDB 3.12.13 on 7 kind nodes, all on one machine, through a port-forward, with 196,000 entities and
195,000 edges in 6 shards. Treat these as a sanity check rather than a benchmark.

| Operation | p50 | p95 |
|---|---|---|
| Lookup by GEID | 1.6 ms | 2.1 ms |
| Inventory, depth 1 | 3.9 ms | 41 ms |
| Whole player tree, depth 4, 195 nodes | 67 ms | 110 ms |
| `Move`, atomic across shards | 8.2 ms | 14 ms |

- Bulk load ran at about 65,000 documents per second.
- An aborted cross-shard `Move` rolled back, and no entity ended up with two parents.
- SmartGraphs can be created on this edition. The server reports `enterprise`, and `/_admin/license`
  shows a 100 GiB limit, with read-only mode as the penalty for exceeding it.
- With continuous writes at replication factor 3 and write concern 2, killing a DB-Server and then an
  Agent cost 4 failed writes out of 3,846, a longest gap of 0.82 s, and no acknowledged write was lost.
  That tests node loss, not a network partition.

## One entity database per region

YugabyteDB stays global, in `scbackend-data`. The entity graph is per region: each `ShardRegion` (`kubectl get shr`)
has, in its namespace `scbackend-region-<name>`, an ArangoDeployment named `arangodb` and the `sc-entitygraph` that is
its only reader and writer, which `sc-shard-operator` creates, reconciles and deletes with the region, like its game
data. Nothing of a region is in helm: StarServices-CN installs the kube-arangodb release (its CRDs) and the
shard operator.

| Object, in `scbackend-region-<name>` unless said | What it is |
|---|---|
| ArangoDeployment `arangodb` | From `spec.entityDb`: `mode` (`Cluster` or `Single`), `agents`, `dbServers`, `coordinators`, `storageClass`, `size` (DB-Servers or the single server), `agentSize`, `image`. The defaults are the layout the dev cluster ran in `scbackend-data`: Cluster, 3 Agents (8Gi), 3 DB-Servers (32Gi), 2 Coordinators, ArangoDB 3.12.13, Production, no TLS. Auth as before: kube-arangodb makes the JWT secret `arangodb-jwt`, and root keeps an empty password, which `sc-db` logs in with. Mode and storage are fixed once it exists |
| Deployment `arango-operator`, ServiceAccount, RoleBindings `arango-operator` and `arango-member`, ClusterRoleBinding `scbackend-region-<name>-arango-operator` | kube-arangodb 1.4.7's deployment operator watches only the namespace its pod runs in (`MY_POD_NAMESPACE`; there is no cluster-wide mode), so each region runs its own, without CRD management. The rights are the kube-arangodb chart's, as ClusterRoles of the shard operator's chart (`templates/arango.yaml`) |
| Secret `scbackend-data` | A copy of the one in `scbackend`, for the token secret in `state.json`. A change restarts `sc-entitygraph` (checksum annotation) |
| Deployment and Service `sc-entitygraph` | The sc-service chart's settings: probes on 8080, 100m/256Mi, 2Gi limit, non-root, read-only root. `SCBACKEND_ARANGO` is the region's `arangodb`, `SCBACKEND_PUSH` sc-push in `scbackend` |
| ReferenceGrant `sc-entitygraph-gateway` | Lets GRPCRoutes in `scbackend` send to this `sc-entitygraph` |
| GRPCRoute `sc-entitygraph-<name>`, in `scbackend` | Only for the region with `spec.entityDb.clientRoute`: the game's `EntityGraphService` calls, through the one gateway, to this region |

`status.entityDb` has `state` (`Pending` or `Ready`: the ArangoDeployment's `Ready` condition and a ready
`sc-entitygraph`), `service`, `database` and `message`. A shard opens only once its build and its region's entity
database are both Ready. The ShardRegion holds the finalizer `scbackend.io/entity-db`: deleting it first deletes the
ArangoDeployment while the region's `arango-operator` still runs to take its members' finalizers off, then lets the
namespace go. Deleting a region deletes its entities.

Who finds which `sc-entitygraph`:

| Caller | How |
|---|---|
| sc-dgs | The operator sets `SCBACKEND_ENTITYGRAPH` to its shard's region's |
| The game client | Through the gateway, to the region with `clientRoute` |
| sc-entity-subscription, sc-admin-web | `SCBACKEND_ENTITYGRAPH`, set by StarServices-CN to the region `entityRegion` |
| sc-seeder (the `sc-seed` Job) | Every ShardRegion's `status.entityDb.service`, listed through the Kubernetes API |
| sc-admin seed | Each region's sc-entitygraph, port-forwarded by `scripts/seed.sh` |

sc-entity-subscription and sc-instance-manager keep their own collections in ArangoDB, and StarServices-CN points them
at `entityRegion`'s. With several regions, what each needs is in TODO.md.

### Replication between regions

Not implemented. Each region's database has the same name, `arangodb`, so a later replication pairs them. What exists,
as checked on 2026-11-07:

- ArangoDB's own Datacenter-to-Datacenter Replication (DC2DC, `arangosync`), which kube-arangodb drove with the
  `ArangoDeploymentReplication` resource, is gone: ArangoDB's 3.12 release notes list DC2DC and arangosync as no longer
  supported from 3.12 on, and kube-arangodb's changelog for 1.4.4 says "Remove Replication operator,
  ArangoDeploymentReplication CRD". We run 3.12.13 with kube-arangodb 1.4.7, so neither side has it. kube-arangodb's
  reference for that resource marked it Enterprise Edition only; that was not checked against a running 3.11.
- What remains: one ArangoDB cluster stretched across zones of one region (Agents and DB-Servers spread over the
  zones, replication factor 3, write concern 2, which is what the measurements above exercised on node loss). That is
  synchronous, so it only suits zones with low latency between them, not distant regions. Or
  replication in the application, which suits the ownership rule: each entity has a home region, its `sc-entitygraph`
  is the only writer, and the others get changes from it (a change feed or outbox, as `social_group_cleanups` is for
  groups) or ask it over gRPC. ArangoDB's WAL tailing API (`/_api/wal/tail`) could feed such a stream; whether it is
  usable for this in the Community Edition of 3.12 was not checked. `arangodump`/`arangorestore` copy a region, but only
  as a snapshot.

## Game data per region

Each `ShardRegion` holds its game builds in a Valkey cluster (Valkey is the BSD-licensed Linux Foundation fork of
Redis) in its namespace, `gamedata-valkey`, and runs `sc-gamedata` in front of it, which owns the data: it is the only
reader and writer of the `gd:` keys, loads each build from the registry (`<registry>/scbackend/gamedata:<build>`,
pushed by `scripts/gamedata-publish.sh`) and serves the files to the DGSs. `sc-gamedata` is a stateless Deployment;
Valkey keeps the data. `sc-shard-operator` creates and reconciles all of it from `spec.gamedata`, never helm, like the
region's ArangoDB.

| Object, in `scbackend-region-<name>` | What it is |
|---|---|
| StatefulSet `gamedata-valkey` | `spec.gamedata.cache`: `primaries` (default 3, at least 3) with `replicasPerPrimary` (default 1) replicas each, so 6 nodes by default, of `docker.io/valkey/valkey` (`image`, default the operator's, 9.0). Each node has a ReadWriteOnce volume (`storageClass`, `size`, default 2Gi) it persists to (`appendonly yes`), so a restarted node loads its share again, and `memory` (default 512Mi, request and limit), of which Valkey uses three quarters (`maxmemory`) with `maxmemory-policy noeviction`: game data is never evicted, a full cluster fails the load instead. Ready once the node sees the cluster `ok`. Nodes announce their pod's name (`cluster-announce-hostname`, `cluster-preferred-endpoint-type hostname`), and `start.sh` rewrites the IPs in `nodes.conf` from those names at start, so a cluster whose pods all restarted with new IPs finds itself again |
| Service `gamedata-valkey` | Headless, with the addresses of pods not ready yet, which give the nodes their names |
| ConfigMap `gamedata-valkey` | `valkey.conf`, `start.sh` and `init.sh`, from `services/shard-operator/valkey/`. A change rolls the nodes |
| Job `gamedata-valkey-init-<nodes>-<replicas>` | Runs `init.sh`: waits for every node, forms the cluster (`valkey-cli --cluster create --cluster-replicas`) when none is formed, and otherwise adds nodes a raise of `primaries` brought (as many new primaries as the layout lacks, the rest replicas of the primaries with the fewest, then `--cluster rebalance --cluster-use-empty-masters`). A formed cluster is left as it is. One Job per layout, the older ones deleted |
| NetworkPolicy `gamedata-valkey` | Only `sc-gamedata`, the nodes themselves and the init Job reach the nodes (6379 and the bus, 16379) |
| Deployment and Service `sc-gamedata` | `spec.gamedata.serviceReplicas` replicas (default 2), with `SC_GAMEDATA_VALKEY` listing every node, an emptyDir work directory (`workSize`, default 4Gi) for a build while it is loaded, and the registry settings. The address DGSs get as `SC_GAMEDATA_ADDR` |

The keys, by hash slot over the primaries, so a build's files are spread by path:

| Key | Value |
|---|---|
| `gd:<build>:<path>` | A file. Above 64 MiB (`SC_GAMEDATA_CHUNK_BYTES`, at most Valkey's 512 MiB per value) it is `gd:<build>:<path>#<n>` parts instead |
| `gd:<build>:manifest` | The build, image digest, chunk size, and every file's path, size, sha256 and parts |
| `gd:<build>:complete` | The image digest, written last |
| `gd:<build>:lock` | The `sc-gamedata` pod loading or pruning the build, `SET NX PX`, renewed while it works |
| `gd:builds`, `gd:replicas`, `gd:replica:<pod>`, `gd:prune:lock` | The builds ever started, the replicas and each one's configured builds (expiring), the pruner |

### What a build holds

`tools/gamedata-build` writes a build from `Data.p4k` on a workstation; `scripts/gamedata-publish.sh` runs it and
pushes the result. Every file is derived from the archive, none is curated by hand, and none is in a service image:
a game patch needs a new game data image, not new service images.

| Path in the build | What it is | Read by |
|---|---|---|
| `manifest.json` | The build and every other file's path, size and sha256 | sc-gamedata, when it loads the image |
| `tables/home_locations.tsv` | One row per home location that is not `devOnly`: location id, name, system, hangar class and its CRC, hangar socpak hash, the planet (name, class CRC, socpak hash, entity socpak hash, entity CRC), then the spawn, planet and hangar transforms (`x y z qx qy qz qw` each) | sc-dgs, at start; sc-admin-web (the first three columns) |
| `tables/hangar_content.tsv` | The containers inside each hangar socpak, at any depth: hangar socpak hash, socpak hash, class CRC, entity socpak hash, entity CRC, name, transform in the hangar's frame | sc-dgs, at start |
| `tables/entity_counts.tsv` | Per socpak of a hangar and its content: hash, `entityCountStatic`, `entityCountDynamic`, `entityCountALE`, tile item ports | sc-dgs, at start |
| `collision/<socpak hash>.col` | The static collision of each hangar socpak of `home_locations.tsv`, see [DGS_PHYSICS.md](DGS_PHYSICS.md) | sc-dgs, at start, for its system's hangars |

The tables are tab separated, one row per line, no header. A DGS loads the tables, then its area, then its system's
hangars' collision, retrying each with a backoff, and opens its port (which is its readiness) only once all are in,
so no player is placed in a hangar without its walls; a shard opens only once its build is complete in the region, so
that is normally at once. Outside a cluster `SC_GAMEDATA_DIR` points a DGS at a build directory (`<out>/<build>`)
instead; with neither that nor `SC_GAMEDATA_ADDR` and `SC_GAME_BUILD` it stops with an error. The tests read the copies
of build 12913470 in `services/dgs/testdata/`, laid out as a build directory, which no image holds.

A build pushed again under the same tag (a fixed converter, a new table) is another image digest. Each `sc-gamedata`
replica asks the registry what the tag names at start and every `SC_GAMEDATA_RECHECK_SECS` (300) after (a HEAD of the
manifest), and when that is not the digest in `gd:<build>:complete`, the replica that takes the build's lock pulls the
new image, replaces the files and writes the new digest; the others follow. DGSs and sc-admin-web read their files at
start (sc-admin-web every ten minutes too), so a running DGS keeps what it loaded until it restarts.

Only one replica loads a build; the others wait and serve what is complete. A build that no live replica is configured
with is pruned by the replica holding the prune lock: every key of it, logged with the files and bytes freed. The
operator keeps the builds of running shards in the list it gives `sc-gamedata` (`status.retainedBuilds`), so only
builds nobody runs go. `status.builds` comes from `sc-gamedata`'s `ListBuilds`; `status.cache` has the nodes, the ready
ones and whether the init Job formed the cluster.

The same on kind, on-premise, AWS and GCP: only a StorageClass with ReadWriteOnce volumes is needed. No Valkey operator
is used: on 2026-11-07 the project's own (`valkey-io/valkey-operator`, 0.5) says it is in early development and not
for production, Hyperspike's has had no release since October 2025 and SAP's does not do cluster mode, so the shard operator makes the StatefulSet
itself, as it would a CR. Lowering `primaries` is refused (the nodes stay, and the status says so), as it would drop
the slots of the nodes removed; `replicasPerPrimary`, `storageClass` and `size` are fixed once the cluster exists.
Before Valkey each `sc-gamedata` replica pulled every build into a volume of its own (a StatefulSet); the operator
deletes that StatefulSet and its volumes. Its `spec.cache` still reads: `cache.replicas` is taken as
`gamedata.serviceReplicas` when that is not set.

## Status

YugabyteDB runs in the cluster, in the `scbackend-data` namespace; each region's ArangoDB and Valkey in its own. Accounts are
still in `state.json`.

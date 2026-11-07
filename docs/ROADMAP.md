# Roadmap

## Where the hard limit is

CIG ships no dedicated server, so a shard cannot simply be started. The client contains replication
and simulation code for the *client* side of the connection, and the authority side (entity spawning,
persistence, physics authority, AI, missions) does not exist in any file available here. Everything
past the connect handshake has to be written from scratch against the reverse-engineered protocol.
This is the bulk of the project, far larger than all the service work.

## Stages

1. **Reach the main menu on private services.** Run both scripts, then work through the assumptions in
   the protocol notes, kept with the reverse-engineering work outside this repository. Each `WARN` line in scbackend.log is a candidate for a real handler.
2. **Character and persistence.** Character creation (`PreallocatePlayer`, customizer service),
   entitlements, ledger and hangar data. Replace the JSON state file with an embedded pure-Rust store
   so the binary stays static.
3. **Shard session.** Encrypted data path (AES-128-GCM; the IV byte order still needs confirming on
   real traffic), then the context establisher steps the client walks through on join.
4. **Minimal world.** Replicate one zone and one player entity: the first time you actually stand in
   your own shard. `docs/netcode/movement_protocol.md` in the RE repo covers movement.
5. **Gameplay systems**, one at a time, as far as the project goes.

An alternative worth testing before stage 3: the client already hosts its frontend on a local shard
(`hostType="Replicant"`, `<local>:12300`). If that local authority can load other levels, it could
provide a far cheaper first world than a from-scratch server. How much authority code ships in the
client is unknown.

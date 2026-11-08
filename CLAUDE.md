# CLAUDE.md

Follow the rules in [AGENTS.md](AGENTS.md).

New discoveries in the game that can be written into sources go into the reverse engineering repository named in `CLAUDE.local.md`, following that repo's CLAUDE.md rules.

Evaluate what needs to go in the replicant and what in the DGS: anything related to an entity the DGS should have authority over goes in the DGS, otherwise it goes in the replicant.

Every component has its own version, following [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html): each service, crate and tool in its `Cargo.toml`, each chart in its `Chart.yaml`. A change to a component increases its version by the kind of change: patch for a backwards compatible fix, minor for backwards compatible functionality, major for an incompatible change (its API, protocol, data, settings or chart values). While a version is 0.y.z, an incompatible change increases the minor version. A component that depends on a changed one, and changes because of it, gets its own increase too. A published version never changes: publishing refuses a component whose content changed without a new version.

Work that is not committed yet is published only as a release candidate, the semver pre-release `X.Y.Z-rc.N`: the version the change will have, and `N` counting from 1 for each publish of it (`0.1.1-rc.1`, then `0.1.1-rc.2` after another change). Once the change is committed, the component gets its plain version `X.Y.Z` and is published again from the clean tree. `scripts/publish.sh` refuses to push a plain version from a working tree with uncommitted changes, and a release manifest records its commit with `-dirty` when it was made from one.

The services' API to each other is in `proto/internal/`, one package per owning service (`scbackend.internal.<owner>.v1`, in `scbackend/internal/<owner>/v1/<owner>.proto`), and messages used by more than one owner in `scbackend.internal.common.v1`. It compiles into the `sc-api` crate; the game's protocol (`proto/sc/`, generated from the game, and the Hub's wire protocol in `proto/scbackend/`) is `sc-proto`. A new RPC or message goes in its owner's package, so a change to one API changes only the services that use it. `scripts/proto-check.sh` (Buf, `proto/buf.yaml`) lints the internal API and fails on a breaking change since the last commit: such a change needs the owner's and its callers' versions increased as an incompatible change, and `SC_ALLOW_BREAKING=1` to confirm it.

Each service owns its data and is the only one that reads or writes it in the database. Another service that needs that data asks the owning service over its API (gRPC), never by querying the owner's tables or collections. For example, the character customizer writes customizations, so a DGS that needs a character's appearance requests it from the character customizer service, not from the database.

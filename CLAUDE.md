# CLAUDE.md

Follow the rules in [AGENTS.md](AGENTS.md).

New discoveries in the game that can be written into sources go into the reverse engineering repository named in `CLAUDE.local.md`, following that repo's CLAUDE.md rules.

Evaluate what needs to go in the replicant and what in the DGS: anything related to an entity the DGS should have authority over goes in the DGS, otherwise it goes in the replicant.

Each service owns its data and is the only one that reads or writes it in the database. Another service that needs that data asks the owning service over its API (gRPC), never by querying the owner's tables or collections. For example, the character customizer writes customizations, so a DGS that needs a character's appearance requests it from the character customizer service, not from the database.

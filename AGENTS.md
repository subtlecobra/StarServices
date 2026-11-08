# Agent rules

These rules apply to all work in this repository.

## Language

- Rust is the preferred language.

## Services

- Every proto service is an independent binary.
- Services pass information using the most efficient and maintainable protocol or mechanism available.
- The PU frontend is a single service, the replicant clients connect to. Entities are owned by DGSs (`sc-dgs`), which each have authority over areas of a system, as a `ShardTemplate` describes. How instancing is handled is decided later.

## Infrastructure

- Infrastructure is cloud native and Helm based.
- This repository builds and publishes; it does not deploy. `scripts/publish.sh` pushes the images, the charts, the
  game data and the seed bundle to the registry `SC_REGISTRY` (local.env) names, and a release manifest of their
  versions and digests. Deployments (helmfile, environments, per-service values, cluster scripts) live elsewhere and
  pull from the registry: nothing here names, reads or writes a deployment's files. A new service needs its release in
  the deployment too.

## Commits


## Work tracking

- Do not stop until every task in TODO.md is marked as done.
- If a task is too complex, split it into several TODO.md entries and address them one by one.

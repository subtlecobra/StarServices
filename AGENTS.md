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
- The deployment (helmfile, environments, per-service values, kind config, cluster scripts) lives in
  the BackendDeploySC repository, checked out next to this one. This repository builds and pushes images and charts (`scripts/build-and-push.sh`)
  to the registry set in that repository's `environments/<env>/registry.yaml`; a new service also needs its values
  file there.

## Commits


## Work tracking

- Do not stop until every task in TODO.md is marked as done.
- If a task is too complex, split it into several TODO.md entries and address them one by one.

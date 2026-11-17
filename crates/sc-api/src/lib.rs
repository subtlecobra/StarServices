//! The backend's internal gRPC API: what the services ask each other, one package per owning service, generated from
//! proto/internal/ (`scbackend.internal.<owner>.v1`). The game's own protocol is sc-proto.
//!
//! `internal` has every package's types under one name, as the single package of before had them.
#![allow(clippy::all, dead_code, missing_docs)]

include!(concat!(env!("OUT_DIR"), "/api.rs"));

/// Every package's types, servers and clients, under one name.
pub mod internal {
    pub use super::scbackend::internal::{
        blueprint_library::v1::*, character_customizer::v1::*, common::v1::*, contacts::v1::*, contract_broker::v1::*,
        dgs::v1::*, entitlement::v1::*, entity_document::v1::*, entitygraph::v1::*, gamedata::v1::*, hub::v1::*,
        identity::v1::*, ledger::v1::*, mission_scenario::v1::*, notifications::v1::*, presence::v1::*, push::v1::*,
        rental::v1::*, reputation::v1::*, shard_operator::v1::*, social::v1::*, solar_system_shop::v1::*,
    };
}

/// The serialized FileDescriptorSet of proto/internal/, with the game's files it imports.
pub static DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/internal.binpb"));

//! Types, gRPC servers and clients generated from proto/.
#![allow(clippy::all, dead_code, missing_docs)]

include!(concat!(env!("OUT_DIR"), "/protos.rs"));

pub use self::sc::external::services::{
    configuration::v1 as config,
    discipline::v1 as discipline,
    identity::v1 as identity,
    login::v1 as login,
    push::v1 as push,
};
pub use self::sc::external::{common, services};
pub use self::scbackend::{hub, internal};

/// The serialized FileDescriptorSet of everything in proto/.
pub static DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sc.binpb"));

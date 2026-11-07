//! Images of plain files over the OCI distribution API, as `podman build FROM scratch` pushes them: the registry
//! client (anonymous pulls, token challenges, redirects, digests checked) and a pull that unpacks the files under one
//! directory of the image into a work directory. sc-gamedata pulls game builds with it, sc-seeder seed bundles.

mod pull;
mod registry;

pub use pull::{pull, unpack, Pulled};
pub use registry::{hex, sha256_digest, Image, Layer, Registry};

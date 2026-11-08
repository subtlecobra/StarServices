//! Compiles proto/internal/, the services' API to each other, into Rust types, gRPC servers and clients, and a descriptor
//! set. The game's types it uses (`sc.*`) are sc-proto's, so every service shares one copy of them. Each package is its
//! own generated file, so a change to one owner's API changes the code of only the services that use that API.

use std::{env, fs, path::{Path, PathBuf}};

use prost::Message;

fn proto_files(dir: &Path, root: &Path, found: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let path = entry.path();
        if path.is_dir() {
            proto_files(&path, root, found);
        } else if path.extension().is_some_and(|e| e == "proto") {
            found.push(path.strip_prefix(root).unwrap().to_string_lossy().into_owned());
        }
    }
}

fn main() {
    let proto = PathBuf::from("../../proto");
    let root = proto.join("internal");
    println!("cargo:rerun-if-changed={}", root.display());
    // The game's protocol, which these import, is generated from the user's copy of the game (scripts/game-files.sh).
    if !proto.join("sc").is_dir() {
        panic!("proto/sc/ is missing: generate it from your game with scripts/game-files.sh (see README, \"Game files\")");
    }
    let mut files = Vec::new();
    proto_files(&root, &root, &mut files);
    files.sort();

    let vendor = proto.join("vendor");
    let descriptors = protox::compile(&files, [root.as_path(), proto.as_path(), vendor.as_path()]).unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("internal.binpb"), descriptors.encode_to_vec()).unwrap();

    tonic_prost_build::configure()
        .extern_path(".sc", "::sc_proto::sc")
        .include_file("api.rs")
        .compile_fds(descriptors)
        .unwrap();
}

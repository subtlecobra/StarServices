//! Compiles proto/ into Rust types, gRPC servers and clients, and a descriptor set. protox is a
//! pure-Rust protobuf compiler, so no protoc is needed.

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
    let root = PathBuf::from("../../proto");
    println!("cargo:rerun-if-changed={}", root.display());
    // proto/sc/ is the game's own protocol, recovered from the user's copy of the game and not in the repository.
    if !root.join("sc").is_dir() {
        panic!("proto/sc/ is missing: generate it from your game with scripts/game-files.sh (see README, \"Game files\")");
    }
    let mut files = Vec::new();
    proto_files(&root.join("sc"), &root, &mut files);
    proto_files(&root.join("scbackend"), &root, &mut files);
    files.sort();

    let vendor = root.join("vendor");
    let descriptors = protox::compile(&files, [root.as_path(), vendor.as_path()]).unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("sc.binpb"), descriptors.encode_to_vec()).unwrap();

    tonic_prost_build::configure()
        .include_file("protos.rs")
        .compile_fds(descriptors)
        .unwrap();
}

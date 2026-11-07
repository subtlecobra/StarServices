//! A build's image, pulled into the replica's work directory (`<work>/.pull-<build>/`, scratch space that goes with the
//! pod): layers downloaded together and checked against their digests, unpacked in order, and every file checked against
//! the build's `manifest.json`. The cache then copies the files into Valkey and the directory is removed.

use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

use sc_oci::{hex, Registry};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const MANIFEST: &str = "manifest.json";

/// A file of a build, as its manifest lists it.
#[derive(Clone, Debug, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Deserialize)]
struct BuildManifest {
    build: String,
    files: Vec<ManifestFile>,
}

/// A pulled and checked build in the work directory, which goes when this does.
#[derive(Debug)]
pub struct Pulled {
    /// The digest of what the build's tag named.
    pub digest: String,
    /// The build's files, under their manifest paths.
    pub root: PathBuf,
    pub files: Vec<ManifestFile>,
    /// Holds the work directory, which goes with it.
    _image: sc_oci::Pulled,
}

/// A relative path of plain names: no root, no `..`, no `.`, not empty, and no `#`, which the keys of a chunked file
/// use. Nor one of the names the cache keeps beside a build's files.
pub fn safe_path(path: &str) -> Option<PathBuf> {
    if path.contains('#') || crate::cache::RESERVED.contains(&path) {
        return None;
    }
    let path = Path::new(path);
    let mut components = path.components().peekable();
    components.peek()?;
    components.all(|component| matches!(component, Component::Normal(_))).then(|| path.to_path_buf())
}

/// A build is a directory name: letters, digits, `-`, `_` and `.`, not starting with `.`.
pub fn valid_build(build: &str) -> bool {
    !build.is_empty()
        && !build.starts_with('.')
        && build.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn read_manifest(root: &Path, build: &str) -> Result<BuildManifest, String> {
    let path = root.join(MANIFEST);
    let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: BuildManifest = serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    if manifest.build != build {
        return Err(format!("{} is for build {}", path.display(), manifest.build));
    }
    for file in &manifest.files {
        if safe_path(&file.path).is_none() {
            return Err(format!("{} lists unsafe path {:?}", path.display(), file.path));
        }
    }
    Ok(manifest)
}

/// Checks every file of the manifest against its size and sha256.
fn verify(root: &Path, build: &str) -> Result<Vec<ManifestFile>, String> {
    let manifest = read_manifest(root, build)?;
    let mut buffer = vec![0u8; 1 << 20];
    for file in &manifest.files {
        let path = root.join(&file.path);
        let mut reader = File::open(&path).map_err(|error| format!("{}: {error}", file.path))?;
        let (mut hasher, mut size) = (Sha256::new(), 0u64);
        loop {
            let read = reader.read(&mut buffer).map_err(|error| format!("{}: {error}", file.path))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        let sha256 = hex(&hasher.finalize());
        if size != file.size || !sha256.eq_ignore_ascii_case(&file.sha256) {
            return Err(format!("{}: {size} bytes sha256 {sha256}, manifest says {} bytes {}", file.path, file.size, file.sha256));
        }
    }
    Ok(manifest.files)
}

/// Pulls a build's image into `<work>/.pull-<build>/`: the files under `/gamedata/<build>/`, every one checked against the
/// manifest. A failed pull leaves nothing behind.
pub async fn pull(registry: &Registry, work: &Path, build: &str) -> Result<Pulled, String> {
    let prefix = Path::new("gamedata").join(build);
    let image = sc_oci::pull(registry, build, &work.join(format!(".pull-{build}")), &prefix).await?;
    let build = build.to_string();
    tokio::task::spawn_blocking(move || {
        let files = verify(&image.root, &build)?;
        Ok(Pulled { digest: image.digest.clone(), root: image.root.clone(), files, _image: image })
    })
    .await
    .map_err(|error| error.to_string())?
}

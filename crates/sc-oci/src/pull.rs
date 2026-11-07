//! An image pulled into a work directory: layers downloaded together and checked against their digests, then unpacked
//! in order, keeping only what is under one directory of the image. The caller checks the files against its own
//! manifest; the work directory goes when the `Pulled` does.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Seek},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::registry::Registry;

/// A pulled image's files in the work directory, which is removed when this is dropped.
#[derive(Debug)]
pub struct Pulled {
    /// The digest of what the tag named.
    pub digest: String,
    /// What the image held under the prefix.
    pub root: PathBuf,
    work: PathBuf,
}

impl Drop for Pulled {
    fn drop(&mut self) {
        let _ = remove_dir(&self.work);
    }
}

fn remove_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(format!("{}: {error}", path.display())),
        _ => Ok(()),
    }
}

/// Unpacks what a layer has under `prefix` (a relative path in the image, such as `gamedata/<build>`) into `root`. An
/// entry with `..` or an absolute path refuses the whole layer; links and devices are skipped, the image is files only.
pub fn unpack(blob: &Path, root: &Path, prefix: &Path) -> Result<(), String> {
    let mut file = File::open(blob).map_err(|error| format!("{}: {error}", blob.display()))?;
    let mut magic = [0u8; 2];
    let gzip = file.read_exact(&mut magic).is_ok() && magic == [0x1f, 0x8b];
    file.rewind().map_err(|error| error.to_string())?;
    let reader: Box<dyn Read> =
        if gzip { Box::new(GzDecoder::new(BufReader::new(file))) } else { Box::new(BufReader::new(file)) };
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().map_err(|error| format!("layer: {error}"))? {
        let mut entry = entry.map_err(|error| format!("layer: {error}"))?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let path = Path::new(name.trim_start_matches("./"));
        if path.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_))) {
            return Err(format!("layer has unsafe path {name:?}"));
        }
        let Ok(relative) = path.strip_prefix(prefix) else { continue };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let file_name = relative.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if let Some(removed) = file_name.strip_prefix(".wh.") {
            // An image built in one step has no whiteouts, but a later layer may remove what an earlier one added.
            if removed != ".wh..opq" {
                let target = root.join(relative.with_file_name(removed));
                let _ = fs::remove_file(&target).or_else(|_| fs::remove_dir_all(&target));
            }
            continue;
        }
        let target = root.join(relative);
        match entry.header().entry_type() {
            tar::EntryType::Directory => fs::create_dir_all(&target).map_err(|error| format!("{name}: {error}"))?,
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|error| format!("{name}: {error}"))?;
                }
                let mut out = File::create(&target).map_err(|error| format!("{name}: {error}"))?;
                io::copy(&mut entry, &mut out).map_err(|error| format!("{name}: {error}"))?;
            }
            other => warn!(name, ?other, "layer entry skipped"),
        }
    }
    Ok(())
}

/// Pulls the image `tag` names into `work` (emptied first, removed with the result): layers downloaded together and
/// checked, unpacked in order, keeping what is under `prefix`. A failed pull leaves nothing behind.
pub async fn pull(registry: &Registry, tag: &str, work: &Path, prefix: &Path) -> Result<Pulled, String> {
    let image = registry.image(tag).await?;
    info!(tag, digest = image.digest, layers = image.layers.len(), "pulling");
    let (blobs, root) = (work.join("blobs"), work.join("root"));
    remove_dir(work)?;
    let pulled = Pulled { digest: image.digest, root: root.clone(), work: work.to_path_buf() };
    fs::create_dir_all(&blobs).map_err(|error| format!("{}: {error}", blobs.display()))?;
    fs::create_dir_all(&root).map_err(|error| format!("{}: {error}", root.display()))?;
    let mut downloads = JoinSet::new();
    let paths: Vec<PathBuf> = (0..image.layers.len()).map(|index| blobs.join(index.to_string())).collect();
    for (layer, path) in image.layers.iter().zip(&paths) {
        let (registry, layer, path) = (registry.clone(), layer.clone(), path.clone());
        downloads.spawn(async move { registry.blob_to_file(&layer, &path).await });
    }
    while let Some(result) = downloads.join_next().await {
        if let Err(error) = result.map_err(|error| error.to_string()).and_then(|result| result) {
            downloads.abort_all();
            return Err(error);
        }
    }
    let prefix = prefix.to_path_buf();
    tokio::task::spawn_blocking(move || {
        for blob in &paths {
            unpack(blob, &root, &prefix)?;
            // The layer is not needed once unpacked.
            let _ = fs::remove_file(blob);
        }
        Ok(pulled)
    })
    .await
    .map_err(|error| error.to_string())?
}

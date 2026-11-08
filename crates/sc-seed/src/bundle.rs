//! A seed bundle: what one game build gives each owner, read from `Data.p4k` once, on the build machine, by
//! `sc-admin seed-export`. A directory `<out>/<build>/` with one file per table, each a run of length-delimited protobuf
//! rows of the type the owner's seed RPC takes, and `manifest.json`, written last, with the format, the build and every
//! file's path, size, sha256 and row count. The same archive gives the same bytes. `scripts/seed-publish.sh` pushes it
//! as the image `<registry>/scbackend/seed:<build>`, files under `/seed/<build>/`, which sc-seeder applies.

use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path},
};

use prost::Message;
use sc_proto::internal::{
    BlueprintDefault, ContainerSeed, EntityClassSeed, MissionContractSeed, RentalPrice, ReputationLadderStanding,
    ReputationScope, ReputationStanding, ScenarioSeed, ShopItem, StarterEntitlement,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MANIFEST: &str = "manifest.json";
/// The layout described here. A reader refuses any other. 2 added the shop items.
pub const FORMAT: u32 = 2;

pub const ENTITY_CLASSES: &str = "entitygraph/entity_classes.pb";
pub const CONTAINERS: &str = "entitygraph/containers.pb";
pub const STARTER_ENTITLEMENTS: &str = "entitlement/starter_entitlements.pb";
pub const REPUTATION_SCOPES: &str = "reputation/scopes.pb";
pub const REPUTATION_STANDINGS: &str = "reputation/standings.pb";
pub const REPUTATION_LADDER: &str = "reputation/ladder.pb";
pub const BLUEPRINT_DEFAULTS: &str = "blueprint-library/blueprint_defaults.pb";
pub const SCENARIOS: &str = "mission-scenario/scenarios.pb";
pub const MISSION_CONTRACTS: &str = "contract-broker/mission_contracts.pb";
pub const RENTAL_PRICES: &str = "solar-system-shop/rental_prices.pb";
pub const SHOP_ITEMS: &str = "solar-system-shop/shop_items.pb";

/// Every file of a bundle, in the manifest's order.
pub const FILES: [&str; 11] = [
    BLUEPRINT_DEFAULTS,
    MISSION_CONTRACTS,
    STARTER_ENTITLEMENTS,
    CONTAINERS,
    ENTITY_CLASSES,
    SCENARIOS,
    REPUTATION_LADDER,
    REPUTATION_SCOPES,
    REPUTATION_STANDINGS,
    RENTAL_PRICES,
    SHOP_ITEMS,
];

/// Everything a build seeds: the rows of each owner.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Content {
    pub classes: Vec<EntityClassSeed>,
    pub containers: Vec<ContainerSeed>,
    pub rental_prices: Vec<RentalPrice>,
    pub shop_items: Vec<ShopItem>,
    pub starters: Vec<StarterEntitlement>,
    pub scopes: Vec<ReputationScope>,
    pub standings: Vec<ReputationStanding>,
    pub ladder: Vec<ReputationLadderStanding>,
    pub blueprints: Vec<BlueprintDefault>,
    pub scenarios: Vec<ScenarioSeed>,
    pub contracts: Vec<MissionContractSeed>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ManifestFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub rows: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    pub build: String,
    pub files: Vec<ManifestFile>,
}

impl Manifest {
    pub fn rows(&self, path: &str) -> u64 {
        self.files.iter().find(|file| file.path == path).map_or(0, |file| file.rows)
    }
}

/// A build is a directory name and an image tag: letters, digits, `-`, `_` and `.`, not starting with `.` or `-`.
pub fn valid_build(build: &str) -> bool {
    !build.is_empty()
        && build.len() <= 128
        && !build.starts_with(['.', '-'])
        && build.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn encode<T: Message>(rows: &[T]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(rows.iter().map(|row| row.encoded_len() + 5).sum());
    for row in rows {
        row.encode_length_delimited(&mut bytes).expect("a Vec grows");
    }
    bytes
}

fn decode<T: Message + Default>(path: &str, mut bytes: &[u8]) -> Result<Vec<T>, String> {
    let mut rows = Vec::new();
    while !bytes.is_empty() {
        rows.push(T::decode_length_delimited(&mut bytes).map_err(|error| format!("{path}: row {}: {error}", rows.len()))?);
    }
    Ok(rows)
}

impl Content {
    fn encoded(&self) -> Vec<(&'static str, Vec<u8>, usize)> {
        let mut files = vec![
            (ENTITY_CLASSES, encode(&self.classes), self.classes.len()),
            (CONTAINERS, encode(&self.containers), self.containers.len()),
            (STARTER_ENTITLEMENTS, encode(&self.starters), self.starters.len()),
            (REPUTATION_SCOPES, encode(&self.scopes), self.scopes.len()),
            (REPUTATION_STANDINGS, encode(&self.standings), self.standings.len()),
            (REPUTATION_LADDER, encode(&self.ladder), self.ladder.len()),
            (BLUEPRINT_DEFAULTS, encode(&self.blueprints), self.blueprints.len()),
            (SCENARIOS, encode(&self.scenarios), self.scenarios.len()),
            (MISSION_CONTRACTS, encode(&self.contracts), self.contracts.len()),
            (RENTAL_PRICES, encode(&self.rental_prices), self.rental_prices.len()),
            (SHOP_ITEMS, encode(&self.shop_items), self.shop_items.len()),
        ];
        files.sort_by_key(|(path, ..)| *path);
        files
    }
}

/// Writes `content` as the bundle of `build` in `<out>/<build>/`, replacing what was there. The manifest goes last, so a
/// directory without one is an export that did not finish.
pub fn write(out: &Path, build: &str, content: &Content) -> Result<Manifest, String> {
    if !valid_build(build) {
        return Err(format!("{build:?} is not a build name"));
    }
    let dir = out.join(build);
    match fs::remove_dir_all(&dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(format!("{}: {error}", dir.display())),
        _ => {}
    }
    let mut files = Vec::new();
    for (path, bytes, rows) in content.encoded() {
        let target = dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        fs::write(&target, &bytes).map_err(|error| format!("{}: {error}", target.display()))?;
        let sha256 = hex(&Sha256::digest(&bytes));
        files.push(ManifestFile { path: path.to_string(), size: bytes.len() as u64, sha256, rows: rows as u64 });
    }
    let manifest = Manifest { format: FORMAT, build: build.to_string(), files };
    let mut text = serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?;
    text.push('\n');
    let path = dir.join(MANIFEST);
    fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(manifest)
}

/// Every file under `dir`, as a relative path with forward slashes.
fn present(dir: &Path, base: &Path, found: &mut BTreeSet<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = entry.path();
        if entry.file_type().map_err(|error| format!("{}: {error}", path.display()))?.is_dir() {
            present(&path, base, found)?;
        } else {
            let relative = path.strip_prefix(base).map_err(|error| error.to_string())?;
            found.insert(relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"));
        }
    }
    Ok(())
}

/// A bundle that was checked whole: the manifest names exactly the files of this format, and every file in the
/// directory has the size and sha256 the manifest gives.
#[derive(Debug)]
pub struct Bundle {
    pub manifest: Manifest,
    dir: std::path::PathBuf,
}

impl Bundle {
    /// Opens and checks `dir` (a `<out>/<build>` directory). With `build`, the manifest must be for it.
    pub fn open(dir: &Path, build: Option<&str>) -> Result<Self, String> {
        let path = dir.join(MANIFEST);
        let text = fs::read(&path).map_err(|error| format!("{}: {error}; the export did not finish", path.display()))?;
        let manifest: Manifest = serde_json::from_slice(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        if manifest.format != FORMAT {
            return Err(format!("{}: format {}, this reads {FORMAT}", path.display(), manifest.format));
        }
        if let Some(build) = build.filter(|build| *build != manifest.build) {
            return Err(format!("{} is for build {}, not {build}", path.display(), manifest.build));
        }
        let listed: BTreeSet<String> = manifest.files.iter().map(|file| file.path.clone()).collect();
        let expected: BTreeSet<String> = FILES.iter().map(|path| path.to_string()).collect();
        if listed != expected || listed.len() != manifest.files.len() {
            return Err(format!("{}: lists {listed:?}, format {FORMAT} has {expected:?}", path.display()));
        }
        let mut found = BTreeSet::new();
        present(dir, dir, &mut found)?;
        found.remove(MANIFEST);
        if found != listed {
            let extra: Vec<_> = found.difference(&listed).collect();
            let missing: Vec<_> = listed.difference(&found).collect();
            return Err(format!("{}: files differ from the manifest: extra {extra:?}, missing {missing:?}", dir.display()));
        }
        let mut buffer = vec![0u8; 1 << 20];
        for file in &manifest.files {
            let relative = Path::new(&file.path);
            if !relative.components().all(|c| matches!(c, Component::Normal(_))) {
                return Err(format!("{}: unsafe path {:?}", path.display(), file.path));
            }
            let mut reader = fs::File::open(dir.join(relative)).map_err(|error| format!("{}: {error}", file.path))?;
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
                return Err(format!("{}: {size} bytes sha256 {sha256}, the manifest says {} bytes {}", file.path, file.size, file.sha256));
            }
        }
        Ok(Self { manifest, dir: dir.to_path_buf() })
    }

    fn rows<T: Message + Default>(&self, path: &str) -> Result<Vec<T>, String> {
        let bytes = fs::read(self.dir.join(path)).map_err(|error| format!("{path}: {error}"))?;
        let rows: Vec<T> = decode(path, &bytes)?;
        let listed = self.manifest.rows(path);
        if rows.len() as u64 != listed {
            return Err(format!("{path}: {} rows, the manifest says {listed}", rows.len()));
        }
        Ok(rows)
    }

    /// Every row of the bundle.
    pub fn content(&self) -> Result<Content, String> {
        Ok(Content {
            classes: self.rows(ENTITY_CLASSES)?,
            containers: self.rows(CONTAINERS)?,
            rental_prices: self.rows(RENTAL_PRICES)?,
            shop_items: self.rows(SHOP_ITEMS)?,
            starters: self.rows(STARTER_ENTITLEMENTS)?,
            scopes: self.rows(REPUTATION_SCOPES)?,
            standings: self.rows(REPUTATION_STANDINGS)?,
            ladder: self.rows(REPUTATION_LADDER)?,
            blueprints: self.rows(BLUEPRINT_DEFAULTS)?,
            scenarios: self.rows(SCENARIOS)?,
            contracts: self.rows(MISSION_CONTRACTS)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample() -> Content {
        Content {
            classes: vec![EntityClassSeed { guid: "g1".into(), class_name: "Crate".into(), ..Default::default() }],
            containers: vec![
                ContainerSeed { geid: 1, labels: vec![3], ..Default::default() },
                ContainerSeed { geid: 2, parent: Some(1), name: "stanton1".into(), ..Default::default() },
            ],
            starters: vec![StarterEntitlement { class_guid: "g1".into(), ..Default::default() }],
            scenarios: vec![ScenarioSeed { template_id: "t".into(), status: 1 }],
            contracts: vec![MissionContractSeed { id: "c".into(), body: vec![1, 2, 3] }],
            shop_items: vec![ShopItem { id: "g1".into(), buy_price: 10.0, max_inventory: 5, ..Default::default() }],
            ..Default::default()
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sc-seed-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_bundle_reads_back_what_was_written_and_the_same_content_gives_the_same_bytes() {
        let (one, two) = (scratch("one"), scratch("two"));
        let manifest = write(&one, "12913470", &sample()).unwrap();
        assert_eq!(write(&two, "12913470", &sample()).unwrap(), manifest);
        assert_eq!(fs::read(one.join("12913470").join(MANIFEST)).unwrap(), fs::read(two.join("12913470").join(MANIFEST)).unwrap());
        assert_eq!(manifest.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), FILES);
        assert_eq!(manifest.rows(CONTAINERS), 2);
        let bundle = Bundle::open(&one.join("12913470"), Some("12913470")).unwrap();
        assert_eq!(bundle.content().unwrap(), sample());
        assert!(Bundle::open(&one.join("12913470"), Some("1")).is_err());
        let _ = (fs::remove_dir_all(one), fs::remove_dir_all(two));
    }

    #[test]
    fn a_changed_missing_or_extra_file_is_refused() {
        let out = scratch("tamper");
        write(&out, "7", &sample()).unwrap();
        let dir = out.join("7");
        fs::write(dir.join(SCENARIOS), b"x").unwrap();
        assert!(Bundle::open(&dir, None).unwrap_err().contains("sha256"));
        write(&out, "7", &sample()).unwrap();
        fs::write(dir.join("entitygraph/extra.pb"), b"").unwrap();
        assert!(Bundle::open(&dir, None).unwrap_err().contains("extra"));
        write(&out, "7", &sample()).unwrap();
        fs::remove_file(dir.join(RENTAL_PRICES)).unwrap();
        assert!(Bundle::open(&dir, None).is_err());
        fs::remove_file(dir.join(MANIFEST)).unwrap();
        assert!(Bundle::open(&dir, None).unwrap_err().contains("did not finish"));
        let _ = fs::remove_dir_all(out);
    }

    #[test]
    fn builds_are_plain_names() {
        assert!(valid_build("12913470"));
        for build in ["", ".x", "-x", "a/b", "a b", ".."] {
            assert!(!valid_build(build), "{build}");
        }
    }
}

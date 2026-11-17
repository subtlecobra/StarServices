//! Converts Data.p4k into the game files of one build, once per build and on a workstation, as no cluster ever sees the
//! archive: `gamedata-build <Data.p4k> <out dir> [--build <n>] [--jobs <n>] [--report] [--tables-only] [--exe <StarCitizen.exe>]
//! [--ship-parts-reference <tsv>]`.
//! `scripts/gamedata-publish.sh` runs it and pushes the result as the image the region caches pull.
//!
//! The tree it writes is the contract with `scripts/gamedata-push.sh` and `sc-gamedata`:
//!
//! - `<out>/<build>/tables/home_locations.tsv`, `hangar_content.tsv`, `entity_counts.tsv` and the hangar elevators'
//!   `transit_managers.tsv`, `transit_gateways.tsv`, `transit_hangars.tsv`, `transit_prefab_entities.tsv`, `transit_brokers.tsv` and
//!   `platform_times.tsv`: the DGS's tables of the build, see
//!   tables.rs and transit.rs. Written first, on every run, as the hangars come from the first. `--tables-only` writes
//!   only them, in seconds, and leaves no manifest: the directory is a whole build again after a run without it, which
//!   then converts only the collision that is missing.
//! - `<out>/<build>/tables/ship_parts.bin`: the parts of every vehicle, which the DGS binds with a ship it spawns, see
//!   ship_parts.rs (format in docs/DATA.md). Written with the tables. The XML loadouts of a few parts (door panels, bed
//!   pillows) are in the encrypted `Data/Scripts`: `--exe` names the game's executable, which holds the archive's key, and
//!   without it they are left out and counted. `--ship-parts-reference` compares the vehicles with the reference ones
//!   (`tools/gamedata-build/testdata/retrieve_loadout_ships.tsv`) and prints how they differ.
//! - `<out>/<build>/collision/<socpak hash>.col`, one per hangar socpak of `home_locations.tsv` (its sixth column) and of `home_hangars.tsv` (its fourth: every hangar class of a location).
//! - `<out>/<build>/manifest.json`, `{"build":"…","files":[{"path":"collision/….col","size":…,"sha256":"…"}]}`, files
//!   sorted by path and relative to `<build>/`. It is removed first and written last, by rename, so a run that stops
//!   halfway never looks complete.
//!
//! The build is the change number of the game's `build_manifest.id` next to the archive (`Data.RequestedP4ChangeNum`),
//! which the shard ids carry too. A `.col` already there that reads back whole with the right header is kept, so a run
//! after a failure or after adding hangars only converts what is missing. Socpaks are converted in parallel over one
//! shared archive; a hangar peaks at about 1.3 GB, so the workers are capped by memory as well as by CPUs. The tables
//! come out the same for the same archive, so a second run gives the same files and the same image digest.
//!
//! One file per hangar socpak, `<socpak hash>.col`, in the frame of the hangar: the frame of its zone, where the spawn
//! points of `tables/home_locations.tsv` are. The hash is that table's, CRC-32C of the path from `objectcontainers` on.
//! See docs/DGS_PHYSICS.md for the format and for why each rule below holds.
//!
//! What is solid:
//!
//! - Brushes, the render nodes of chunk 16 of each `.soc`, of the hangar's socpak and of every child container at any
//!   depth, each placed by its `<Child>` `pos`/`rot` composed down from the hangar.
//! - Of a brush, only the collision geometry of its `.cgf` (physics chunk `0x58de1772`), never the render mesh. Every
//!   brush of the Orbituary hangar has some.
//! - Within that geometry, only what stops a player, by the walk classes (`AddPhysGeomFromChunk` and the node
//!   flags): no `$IP` interaction points, `$shield` volumes, interior volumes, `no_player_collide` nodes, group-0 hulls
//!   of nodes that also have a cluster mesh, or faces whose surface type does not collide with players. Distance-field
//!   hulls (group 7) only where the node has nothing else.
//! - Entities (`.entxml` and the `.soc`'s chunk 4) are left out: doors, elevators and lifts move, and the DGS will hold
//!   them as entities of their own. `--report` lists them.
//!
//! Each distinct geometry is simplified once, within `TOLERANCE` metres (meshoptimizer, quadric error), as cluster meshes
//! are as dense as render meshes and a capsule needs centimetres. meshoptimizer is C++, which is fine here: this tool runs
//! offline, and the DGS only reads its output.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use p4k_formats::ivo::{self, V3};
use p4k_formats::socpak::{self, compose, IDENT};
use p4k::{Entry, P4k};
use p4k_scene::builder::{build_scene_named, GeoSource, Options};
use p4k_scene::materials::MatRegistry;
use p4k_scene::socpak_scene::{data_path, zip_geometry, Stats};
use serde::Serialize;
use sha2::{Digest, Sha256};

mod platform_times;
mod ship_parts;
mod tables;
mod transit;

const MAX_DEPTH: usize = 16;
/// The walk class of geometry that stops a player, see above.
const SOLID: &str = "";
/// How far a simplified surface may move from the game's, in metres of the geometry's own frame. At 5 cm thin
/// geometry starts to vanish (13 of 2027 brushes of Orbituary); at 3 cm none does.
const TOLERANCE: f32 = 0.03;
const MAGIC: &[u8; 4] = b"SCHC";
const VERSION: u32 = 1;

/// A geometry placed in the hangar's frame.
struct Placed {
    geometry: String,
    matrix: [f64; 12],
    /// `None` for a brush, the entity class for an entity.
    class: Option<String>,
    container: String,
}

fn crc_byte(crc: u32, byte: u8) -> u32 {
    (0..8).fold(crc ^ u32::from(byte), |crc, _| if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 })
}

/// The client keys socpaks by the path in lower case with forward slashes, from `objectcontainers` on.
fn socpak_hash(path: &str) -> Option<u32> {
    let path = path.to_ascii_lowercase().replace('\\', "/");
    let start = path.find("objectcontainers")?;
    Some(!path[start..].bytes().fold(!0, crc_byte))
}

fn walk(archive: &P4k, entry: &Entry, world: &[f64; 12], depth: usize, seen: &mut Vec<String>, stats: &mut Stats, out: &mut Vec<Placed>) {
    let short = entry.name.rsplit('/').next().unwrap_or_default().to_string();
    let data = match archive.read(entry) {
        Ok(data) => data,
        Err(error) => return eprintln!("{short}: {error}"),
    };
    let entities = match socpak::read_entities(&data, None, None) {
        Ok((entities, _)) => entities,
        Err(error) => return eprintln!("{short}: {error}"),
    };
    seen.push(entry.name.to_string());
    for entity in &entities {
        let matrix = compose(world, &entity.matrix);
        if let Some(container) = entity.container.as_deref().filter(|c| !c.is_empty()) {
            match archive.find_path(&data_path(container), ".socpak") {
                Some(child) if depth < MAX_DEPTH && !seen.contains(&child.name.to_string()) => {
                    walk(archive, child, &matrix, depth + 1, seen, stats, out)
                }
                Some(_) => {}
                None => eprintln!("{short}: missing container {container}"),
            }
            continue;
        }
        let Some(geometry) = entity.geometry.as_deref().filter(|g| !g.is_empty()) else { continue };
        let name = if geometry.contains("//") {
            zip_geometry(archive, geometry, stats)
        } else {
            archive.find_path(&data_path(geometry), "").map(|found| found.name.to_string())
        };
        let Some(name) = name else {
            eprintln!("{short}: missing geometry {geometry}");
            continue;
        };
        let class = if entity.name == "rendernode" { None } else { Some(entity.cls.clone().unwrap_or_default()) };
        out.push(Placed { geometry: name, matrix, class, container: short.clone() });
    }
    seen.pop();
}

/// The solid triangles of one geometry in its own frame, welded and simplified.
#[derive(Default)]
struct Shape {
    verts: Vec<[f32; 3]>,
    tris: Vec<[u32; 3]>,
    /// Triangles before simplification.
    source_tris: usize,
    /// Triangles left out, by walk class.
    dropped: BTreeMap<String, usize>,
}

fn shape(source: &GeoSource, name: &str, materials: &mut MatRegistry) -> Result<Shape, String> {
    let options = Options { with_render: false, with_collision: true, max_render_tris: None, mtl_override: None, with_frames: false };
    let (_, parts) = build_scene_named(source, name, &options, materials)?;
    let mut shape = Shape::default();
    let solid = |part: &p4k_scene::builder::Part| part.cls.as_deref().unwrap_or("") == SOLID;
    let has_other = parts.iter().any(|part| part.group != Some(ivo::PHYS_FIELD) && solid(part));
    let mut verts: Vec<V3> = vec![];
    let mut tris: Vec<[u32; 3]> = vec![];
    for part in &parts {
        let field = part.group == Some(ivo::PHYS_FIELD);
        if !solid(part) || (field && has_other) {
            let key = if field { "distance field".to_string() } else { part.cls.clone().unwrap_or_default() };
            *shape.dropped.entry(key).or_default() += part.tris.len();
            continue;
        }
        let base = verts.len() as u32;
        verts.extend(&part.verts);
        tris.extend(part.tris.iter().map(|t| t.map(|i| i + base)));
    }
    shape.source_tris = tris.len();
    simplify(&mut shape, &verts, &tris);
    Ok(shape)
}

/// Welds at 1 mm, simplifies within `TOLERANCE`, and keeps only the vertices still used.
fn simplify(shape: &mut Shape, verts: &[V3], tris: &[[u32; 3]]) {
    let mut index: HashMap<[i64; 3], u32> = HashMap::new();
    let mut welded: Vec<[f32; 3]> = vec![];
    let ids: Vec<u32> = verts
        .iter()
        .map(|v| {
            let next = welded.len() as u32;
            *index.entry(v.map(|c| (c / 0.001).round() as i64)).or_insert_with(|| {
                welded.push(v.map(|c| c as f32));
                next
            })
        })
        .collect();
    let indices: Vec<u32> = tris.iter().flat_map(|t| t.map(|i| ids[i as usize])).collect();
    if indices.is_empty() {
        return;
    }
    let bytes: Vec<u8> = welded.iter().flatten().flat_map(|f| f.to_le_bytes()).collect();
    let adapter = meshopt::VertexDataAdapter::new(&bytes, 12, 0).expect("vertex adapter");
    let kept = meshopt::simplify(&indices, &adapter, 0, TOLERANCE, meshopt::SimplifyOptions::ErrorAbsolute, None);
    let mut remap: HashMap<u32, u32> = HashMap::new();
    for t in kept.chunks_exact(3) {
        if t[0] == t[1] || t[1] == t[2] || t[0] == t[2] {
            continue;
        }
        let tri = [t[0], t[1], t[2]].map(|i| {
            let next = shape.verts.len() as u32;
            *remap.entry(i).or_insert_with(|| {
                shape.verts.push(welded[i as usize]);
                next
            })
        });
        shape.tris.push(tri);
    }
}

fn transform(m: &[f32; 12], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[4 * r] * v[0] + m[4 * r + 1] * v[1] + m[4 * r + 2] * v[2] + m[4 * r + 3])
}

/// Little endian, zlib compressed: `SCHC`, u32 version, u32 socpak hash, u32 shape count, u32 instance count, 6 f32
/// bounds; per shape u32 vertex count, u32 triangle count, f32 x y z per vertex, u32 a b c per triangle; per instance
/// u32 shape index and a row-major 3x4 f32 matrix into the hangar's frame.
fn encode(hash: u32, shapes: &[&Shape], instances: &[(u32, [f32; 12])]) -> Vec<u8> {
    let mut bounds = [f32::MAX, f32::MAX, f32::MAX, f32::MIN, f32::MIN, f32::MIN];
    for (index, matrix) in instances {
        for v in &shapes[*index as usize].verts {
            let p = transform(matrix, *v);
            for k in 0..3 {
                bounds[k] = bounds[k].min(p[k]);
                bounds[k + 3] = bounds[k + 3].max(p[k]);
            }
        }
    }
    let mut out = Vec::new();
    out.extend(MAGIC);
    for word in [VERSION, hash, shapes.len() as u32, instances.len() as u32] {
        out.extend(word.to_le_bytes());
    }
    bounds.iter().for_each(|f| out.extend(f.to_le_bytes()));
    for shape in shapes {
        out.extend((shape.verts.len() as u32).to_le_bytes());
        out.extend((shape.tris.len() as u32).to_le_bytes());
        shape.verts.iter().flatten().for_each(|f| out.extend(f.to_le_bytes()));
        shape.tris.iter().flatten().for_each(|i| out.extend(i.to_le_bytes()));
    }
    for (index, matrix) in instances {
        out.extend(index.to_le_bytes());
        matrix.iter().for_each(|f| out.extend(f.to_le_bytes()));
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&out).expect("compress");
    encoder.finish().expect("compress")
}

/// The socpak entries of the archive by hash. Hashing every socpak's path is the robust way from the table to the
/// archive: the table holds only the hash, and the hangars' paths differ between locations and builds.
fn socpaks_by_hash(archive: &P4k) -> HashMap<u32, &Entry> {
    archive.find("objectcontainers", &[".socpak"]).filter_map(|entry| Some((socpak_hash(&entry.name)?, entry))).collect()
}

/// Whether a `.col` already written is whole and is the one for `hash`: it inflates (zlib checks its Adler-32) and its
/// header has the magic, this version and the hash. The build is the directory it is in.
fn valid(file: &Path, hash: u32) -> bool {
    let Ok(compressed) = std::fs::read(file) else { return false };
    let mut data = vec![];
    if flate2::read::ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut data).is_err() || data.len() < 44 {
        return false;
    }
    let word = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    &data[..4] == MAGIC && word(4) == VERSION && word(8) == hash
}

/// Converts one hangar socpak and writes `<hash>.col` into `dir`, through a temporary file and a rename so a stopped run
/// leaves nothing that looks whole. Returns what to print, so parallel workers do not interleave lines.
fn convert(archive: &P4k, entry: &Entry, hash: u32, dir: &Path, report: bool) -> String {
    let mut text = String::new();
    let mut stats = Stats::with_max(usize::MAX);
    let mut placed = vec![];
    walk(archive, entry, &IDENT, 0, &mut vec![], &mut stats, &mut placed);
    let source = GeoSource::with_overlay(archive, &stats.overlay);
    let mut materials = MatRegistry::with_options(archive, 0, false, true);
    let mut built: BTreeMap<String, Option<Shape>> = BTreeMap::new();
    let mut entities: BTreeMap<String, usize> = BTreeMap::new();
    let mut containers: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for p in &placed {
        if let Some(class) = &p.class {
            *entities.entry(format!("{class} {}", p.geometry.rsplit('/').next().unwrap_or_default())).or_default() += 1;
            continue;
        }
        built.entry(p.geometry.clone()).or_insert_with(|| {
            shape(&source, &p.geometry, &mut materials).map_err(|error| eprintln!("{}: {error}", p.geometry)).ok()
        });
    }
    let names: Vec<&String> = built.iter().filter(|(_, s)| s.as_ref().is_some_and(|s| !s.tris.is_empty())).map(|(n, _)| n).collect();
    let index: HashMap<&String, u32> = names.iter().enumerate().map(|(i, n)| (*n, i as u32)).collect();
    let shapes: Vec<&Shape> = names.iter().map(|n| built[*n].as_ref().unwrap()).collect();
    let mut instances = vec![];
    for p in placed.iter().filter(|p| p.class.is_none()) {
        let counts = containers.entry(p.container.clone()).or_default();
        counts.0 += 1;
        if let Some(i) = index.get(&p.geometry) {
            counts.1 += 1;
            instances.push((*i, p.matrix.map(|f| f as f32)));
        }
    }
    let bytes = encode(hash, &shapes, &instances);
    let file = dir.join(format!("{hash}.col"));
    let partial = dir.join(format!("{hash}.col.tmp"));
    std::fs::write(&partial, &bytes).expect("write collision");
    std::fs::rename(&partial, &file).expect("rename collision");
    let unique: usize = shapes.iter().map(|s| s.tris.len()).sum();
    let source_tris: usize = shapes.iter().map(|s| s.source_tris).sum();
    let placed_tris: usize = instances.iter().map(|(i, _)| shapes[*i as usize].tris.len()).sum();
    let brushes = containers.values().map(|c| c.0).sum::<usize>();
    let _ = writeln!(text, "{}: hash {hash}, {brushes} brushes, {} solid, {} shapes", entry.name, instances.len(), shapes.len());
    let _ = writeln!(text, "  {unique} triangles in shapes ({source_tris} before simplifying), {placed_tris} placed, {} bytes -> {}", bytes.len(), file.display());
    if report {
        for (container, (all, solid)) in &containers {
            let _ = writeln!(text, "  container {container}: {all} brushes, {solid} solid");
        }
        let mut dropped: BTreeMap<&str, usize> = BTreeMap::new();
        for shape in built.values().flatten() {
            shape.dropped.iter().for_each(|(k, n)| *dropped.entry(k).or_default() += n);
        }
        for (class, n) in &dropped {
            let _ = writeln!(text, "  left out by class {class:?}: {n} triangles of distinct geometries");
        }
        for (name, shape) in &built {
            if shape.as_ref().is_none_or(|s| s.tris.is_empty()) {
                let _ = writeln!(text, "  no solid geometry: {name}");
            }
        }
        for (entity, n) in &entities {
            let _ = writeln!(text, "  entity left out: {n} x {entity}");
        }
    }
    text
}

/// The change number of the game build the archive belongs to, from `build_manifest.id` beside it.
fn game_build(archive: &Path) -> String {
    let path = archive.parent().unwrap_or(Path::new(".")).join("build_manifest.id");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}; give --build", path.display()));
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    manifest["Data"]["RequestedP4ChangeNum"]
        .as_str()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| panic!("{}: no Data.RequestedP4ChangeNum; give --build", path.display()))
        .to_string()
}

const HOME_LOCATIONS_SOCPAK_COLUMN: usize = 5;
const HOME_HANGARS_SOCPAK_COLUMN: usize = 3;

/// The distinct hangar socpak hashes of a table, in the zero-based `column` (the sixth of the home locations table, the
/// fourth of the home hangars table).
fn hangars(table: &str, column: usize) -> BTreeSet<u32> {
    table
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let field = line.split('\t').nth(column).unwrap_or_default();
            field.parse().unwrap_or_else(|_| panic!("hangar table: no socpak hash in {line:?}"))
        })
        .collect()
}

/// Writes a file through a temporary one and a rename, so a stopped run leaves nothing that looks whole.
fn write_whole(path: &Path, data: &[u8]) {
    let partial = path.with_extension(format!("{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or_default()));
    std::fs::write(&partial, data).unwrap_or_else(|error| panic!("{}: {error}", partial.display()));
    std::fs::rename(&partial, path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
}

/// Removes the `.tmp` files a stopped run left in `dir`, as the image holds the whole directory.
fn remove_partial(dir: &Path) {
    for item in std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display())) {
        let path = item.expect("read dir").path();
        if path.extension().is_some_and(|e| e == "tmp") {
            std::fs::remove_file(&path).expect("remove a partial file");
        }
    }
}

/// A hangar peaks at about 1.3 GB, the rest is headroom for the allocator and the page cache of the archive.
const WORKER_MEMORY: u64 = 1536 << 20;

/// As many workers as there are CPUs and as `MemAvailable` holds at `WORKER_MEMORY` each, and at least one.
fn default_jobs() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let available = std::fs::read_to_string("/proc/meminfo").ok().and_then(|info| {
        let line = info.lines().find(|line| line.starts_with("MemAvailable:"))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kib| kib << 10)
    });
    available.map_or(1, |bytes| cpus.min((bytes / WORKER_MEMORY) as usize)).max(1)
}

#[derive(Serialize)]
struct Manifest {
    build: String,
    files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
}

/// Lists every `.col`, `.tsv` and `.bin` of the build, so a file left from an earlier table still ships with what it is: the
/// image holds the whole directory.
fn write_manifest(root: &Path, build: &str) -> usize {
    let mut files = vec![];
    for (dir, extensions) in [("collision", &[".col"][..]), ("tables", &[".tsv", ".bin"][..])] {
        for item in std::fs::read_dir(root.join(dir)).unwrap_or_else(|error| panic!("{dir}: {error}")) {
            let path = item.unwrap_or_else(|error| panic!("{dir}: {error}")).path();
            let wanted = |name: &&str| extensions.iter().any(|extension| name.ends_with(extension));
            let Some(name) = path.file_name().and_then(|n| n.to_str()).filter(wanted) else { continue };
            let data = std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            let digest = Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
            files.push(ManifestFile { path: format!("{dir}/{name}"), size: data.len() as u64, sha256: digest });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let count = files.len();
    let mut json = serde_json::to_vec(&Manifest { build: build.to_string(), files }).expect("manifest");
    json.push(b'\n');
    let partial = root.join("manifest.json.tmp");
    std::fs::write(&partial, json).expect("write manifest");
    std::fs::rename(&partial, root.join("manifest.json")).expect("rename manifest");
    count
}

/// `tables/ship_keels.tsv`: the class CRC and the origin-to-keel distance in metres (`landingOffset` of the vehicle's XML) of each ship that has one.
const SHIP_KEELS: &str = "tables/ship_keels.tsv";

/// Writes `tables/ship_parts.bin`, says what could not be read, and compares it with the reference table when `--ship-parts-reference` names its
/// table.
fn write_ship_parts(archive: &P4k, root: &Path, reference_path: Option<&str>) {
    let entry = archive.get(DATACORE_PATH).expect("no DataCore");
    let datacore = p4k_formats::dcb::Dcb::open(archive.read(entry).expect("read DataCore")).expect("parse DataCore");
    let xml = |path: &str| {
        let entry = archive.find_path(path, XML_EXTENSION)?;
        p4k_formats::xmlq::parse_any(&archive.read(entry).ok()?).ok()
    };
    let socpaks = tables::Socpaks::of(archive);
    let socpak = |path: &str| tables::metadata(&socpaks, path);
    let (ships, unread) = ship_parts::generate(&datacore, &xml, &socpak);
    let keels: String = ship_parts::keels(&datacore, &xml).iter().map(|(class_crc, z)| format!("{class_crc}\t{z}\n")).collect();
    write_whole(&root.join(SHIP_KEELS), keels.as_bytes());
    println!("{} ship keels -> {}", keels.lines().count(), root.join(SHIP_KEELS).display());
    let parts: usize = ships.iter().map(|ship| ship.parts.len()).sum();
    write_whole(&root.join(ship_parts::SHIP_PARTS), &ship_parts::encode(&ships));
    println!("{} vehicles, {parts} parts -> {}", ships.len(), root.join(ship_parts::SHIP_PARTS).display());
    if !unread.xml_loadouts.is_empty() {
        let uses: usize = unread.xml_loadouts.values().sum();
        println!("{} XML loadouts unread ({uses} uses; --exe reads them): {:?}", unread.xml_loadouts.len(), unread.xml_loadouts.keys().collect::<Vec<_>>());
    }
    if !unread.missing_socpaks.is_empty() {
        println!("{} socpaks of object containers are not in the archive: {:?}", unread.missing_socpaks.len(), unread.missing_socpaks);
    }
    if !unread.missing_classes.is_empty() {
        println!("{} classes named by loadouts are not in the DataCore: {:?}", unread.missing_classes.len(), unread.missing_classes);
    }
    let Some(reference_path) = reference_path else { return };
    let text = std::fs::read_to_string(reference_path).unwrap_or_else(|error| panic!("{reference_path}: {error}"));
    let reference = ship_parts::parse_reference_table(&text).unwrap_or_else(|error| panic!("{error}"));
    for (class_crc, parts) in &reference {
        let one = std::collections::BTreeMap::from([(*class_crc, parts.clone())]);
        let name = ships.iter().find(|ship| ship.class_crc == *class_crc).map_or("?", |ship| ship.name.as_str());
        let comparison = ship_parts::compare(&ships, &one);
        println!(
            "{name} {class_crc:#010x}: {} reference, same {} (other flags {}), other class {}, reference only {}, generated only {}",
            parts.len(), comparison.same, comparison.other_flags, comparison.other_class, comparison.reference_only, comparison.generated_only,
        );
    }
    let comparison = ship_parts::compare(&ships, &reference);
    println!(
        "all: {} ships ({:x?} not vehicles), same {} (other flags {}), other class {}, reference only {}, generated only {}",
        comparison.ships, comparison.missing_ships, comparison.same, comparison.other_flags, comparison.other_class,
        comparison.reference_only, comparison.generated_only,
    );
    println!(
        "object containers: {} reference with the attach id and class of the file's ({} with other flags), {} entities under \
         them, {} of the file's not reference",
        comparison.containers, comparison.container_other_flags, comparison.container_content, comparison.generated_only_containers,
    );
    for (class_crc, (reference, generated)) in &comparison.flag_differences {
        println!("flags of {class_crc:#010x}: reference {reference:#010x}, generated {generated:#010x}");
    }
}

const USAGE: &str = "usage: gamedata-build <Data.p4k> <out dir> [--build <n>] [--jobs <n>] [--report] [--tables-only] \
                     [--exe <StarCitizen.exe>] [--ship-parts-reference <tsv>]";
/// The variable the archive reader takes the game's executable from, for the key of encrypted entries.
const EXE_VARIABLE: &str = "P4K_EXE";
/// No executable: encrypted entries stay unread.
const NO_EXE: &str = "/nonexistent/StarCitizen.exe";
const XML_EXTENSION: &str = ".xml";
const DATACORE_PATH: &str = "Data/Game2.dcb";

fn main() {
    // The archive reader looks for its key in the game's executable for encrypted entries, which are only `Data/Scripts`
    // and none of what is converted here: point it at nothing, so the game's files beyond the archive are never opened.
    let mut arguments = std::env::args().skip(1);
    let (mut positional, mut build, mut jobs, mut report, mut tables_only) = (vec![], None, None, false, false);
    let (mut exe, mut reference_path) = (None, None);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().unwrap_or_else(|| panic!("{argument} needs a value\n{USAGE}"));
        match argument.as_str() {
            "--build" => build = Some(value()),
            "--jobs" => jobs = Some(value().parse::<usize>().expect("--jobs is a number").max(1)),
            "--report" => report = true,
            "--tables-only" => tables_only = true,
            "--exe" => exe = Some(value()),
            "--ship-parts-reference" => reference_path = Some(value()),
            _ if argument.starts_with("--") => panic!("unknown option {argument}\n{USAGE}"),
            _ => positional.push(argument),
        }
    }
    let [source, output] = positional.as_slice() else { panic!("{USAGE}") };
    match exe {
        Some(exe) => std::env::set_var(EXE_VARIABLE, exe),
        None if std::env::var_os(EXE_VARIABLE).is_none() => std::env::set_var(EXE_VARIABLE, NO_EXE),
        None => {}
    }
    let source = Path::new(source);
    let build = build.unwrap_or_else(|| game_build(source));
    assert!(!build.is_empty() && !build.contains(['/', '\\']) && build != "." && build != "..", "the game build names a directory");
    let root = Path::new(output).join(&build);
    let (collision, table_dir) = (root.join("collision"), root.join("tables"));
    std::fs::create_dir_all(&collision).expect("output dir");
    std::fs::create_dir_all(&table_dir).expect("output dir");
    match std::fs::remove_file(root.join("manifest.json")) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => panic!("remove the old manifest: {error}"),
        _ => {}
    }
    remove_partial(&collision);
    remove_partial(&table_dir);
    let archive = P4k::open(source).expect("open archive");
    let tables = tables::generate(&archive);
    for (path, text) in tables.files() {
        write_whole(&root.join(path), text.as_bytes());
        println!("{} rows -> {}", text.lines().count(), root.join(path).display());
    }
    write_ship_parts(&archive, &root, reference_path.as_deref());
    if tables_only {
        println!("build {build}: tables only, no manifest -> {}", root.display());
        return;
    }
    let mut hashes = hangars(&tables.home_locations, HOME_LOCATIONS_SOCPAK_COLUMN);
    hashes.extend(hangars(&tables.home_hangars, HOME_HANGARS_SOCPAK_COLUMN));
    let pending: Vec<u32> = hashes.iter().copied().filter(|hash| !valid(&collision.join(format!("{hash}.col")), *hash)).collect();
    println!("build {build}: {} hangars, {} to convert -> {}", hashes.len(), pending.len(), root.display());
    if !pending.is_empty() {
        let by_hash = socpaks_by_hash(&archive);
        let work: Vec<(u32, &Entry)> = pending
            .iter()
            .map(|hash| (*hash, *by_hash.get(hash).unwrap_or_else(|| panic!("no socpak with hash {hash} in the archive"))))
            .collect();
        let jobs = jobs.unwrap_or_else(default_jobs).min(work.len());
        println!("{jobs} workers");
        let next = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                scope.spawn(|| {
                    while let Some((hash, entry)) = work.get(next.fetch_add(1, Ordering::Relaxed)) {
                        print!("{}", convert(&archive, entry, *hash, &collision, report));
                    }
                });
            }
        });
    }
    let count = write_manifest(&root, &build);
    println!("{count} files -> {}", root.join("manifest.json").display());
}

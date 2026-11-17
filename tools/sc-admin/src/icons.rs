//! The admin site's reputation icons, read from the game archive. None of them is in the repository: the admin embeds
//! the files it finds in `services/admin/assets/reputation/` at build time (see its build.rs).

use std::path::Path;

use p4k::P4k;

/// Where the mobiGlas reputation manager keeps its vector icons in the archive.
const ICON_FOLDER: &str = "Data/UI/Textures/Vector/MobiGlas/App_ReputationManager";
/// The header icon of each scope the game draws one for. `services/admin/src/web/reputation.rs` names the same files.
const ICON_FILES: [&str; 11] = [
    "icon_reputationrank_assassination_header.svg",
    "icon_reputationrank_bountyhunter_header.svg",
    "icon_reputationrank_courier_header.svg",
    "icon_reputationrank_emergencyservices_header.svg",
    "icon_reputationrank_hiredmuscle_header.svg",
    "icon_reputationrank_investigationandretrieval_header.svg",
    "icon_reputationrank_security_header.svg",
    "icon_reputationrank_smuggling_header.svg",
    "icon_reputationrank_theft_header.svg",
    "icon_reputation_technician_header.svg",
    "icon_spaceshipracing_header.svg",
];

/// Writes the icons into `out`, replacing those of an earlier run. Returns how many.
pub fn extract(p4k: &Path, out: &Path) -> Result<usize, String> {
    let archive = P4k::open(p4k).map_err(|e| format!("open {}: {e}", p4k.display()))?;
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    for file in ICON_FILES {
        let path = format!("{ICON_FOLDER}/{file}");
        let entry = archive.get(&path).ok_or_else(|| format!("{path} not in archive"))?;
        let bytes = archive.read(entry).map_err(|e| format!("read {path}: {e}"))?;
        std::fs::write(out.join(file), bytes).map_err(|e| format!("write {file}: {e}"))?;
    }
    Ok(ICON_FILES.len())
}

/// The English localization of the archive: the `@key` texts of the game data (reputation scopes and standings, among others).
const LOCALIZATION_PATH: &str = "Data/Localization/english/global.ini";

/// Writes the archive's English `global.ini` to `out` as it is. Returns its size in bytes.
pub fn extract_localization(p4k: &Path, out: &Path) -> Result<usize, String> {
    let archive = P4k::open(p4k).map_err(|e| format!("open {}: {e}", p4k.display()))?;
    let entry = archive.get(LOCALIZATION_PATH).ok_or_else(|| format!("{LOCALIZATION_PATH} not in archive"))?;
    let bytes = archive.read(entry).map_err(|e| format!("read {LOCALIZATION_PATH}: {e}"))?;
    if let Some(folder) = out.parent() {
        std::fs::create_dir_all(folder).map_err(|e| format!("create {}: {e}", folder.display()))?;
    }
    std::fs::write(out, &bytes).map_err(|e| format!("write {}: {e}", out.display()))?;
    Ok(bytes.len())
}

/// The record type of a faction in the game data.
const FACTION_STRUCT: &str = "FactionReputation";
/// The game data file the factions are read from.
const DATACORE_PATH: &str = "Data/Game2.dcb";
/// The prefix of the paths in the archive, and the logo extensions it holds them under.
const ARCHIVE_ROOT: &str = "Data/";
const LOGO_SOURCE_EXTENSION: &str = ".dds";
/// The widest a logo is written, in pixels.
const LOGO_MAX_SIZE: usize = 256;
/// The file of the factions' table, beside their logos (`<geid>.png`).
pub const FACTIONS_FILE: &str = "factions.tsv";

/// The mean brightness (0 to 255) of a logo's visible pixels from which the logo counts as light, and is shown on a dark
/// backing; a darker logo is shown on a light one.
const LIGHT_LOGO_MEAN: f64 = 140.0;
/// The alpha above which a pixel is visible.
const VISIBLE_ALPHA: u8 = 40;
/// The words of the table's last column.
const TONE_LIGHT: &str = "light";
const TONE_DARK: &str = "dark";

/// Whether a logo's visible pixels are light on the whole.
fn is_light(pixels: &[u8]) -> bool {
    let (mut sum, mut count) = (0.0, 0u64);
    for pixel in pixels.chunks_exact(4).filter(|pixel| pixel[3] > VISIBLE_ALPHA) {
        sum += 0.3 * f64::from(pixel[0]) + 0.59 * f64::from(pixel[1]) + 0.11 * f64::from(pixel[2]);
        count += 1;
    }
    count > 0 && sum / count as f64 >= LIGHT_LOGO_MEAN
}

/// Writes the factions of the game data into `out`: a table `factions.tsv` of `geid`, `display name key`, `logo file`
/// (empty when the faction has no usable logo) and the logo's tone (`light` or `dark`), and the logos as PNG files. Returns how many factions and logos.
/// How deep a faction record is read to reach the scope records its reputation context references.
const SCOPE_DEPTH: usize = 6;
/// Separates the scope names in the last column of the factions table.
const SCOPE_SEPARATOR: &str = ",";
/// The field of a scope record that names the scope the reputation service stores.
/// The position of the target record's GUID in a reference value (`["ref", index, guid]`).
const REFERENCE_GUID_INDEX: usize = 2;
/// The struct names of the records a faction's reputation context leads to start with this.
const SCOPE_RECORD_PREFIX: &str = "SReputation";
const SCOPE_NAME_FIELD: &str = "scopeName";

/// Collects the `scopeName` of every scope record reachable from `value` through references, once each, in order.
/// Only the reputation context and scope records are followed.
fn scope_names(datacore: &p4k_formats::dcb::Dcb, by_guid: &std::collections::HashMap<String, usize>, value: &serde_json::Value, names: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(items) => {
            let target = items.get(REFERENCE_GUID_INDEX).and_then(|guid| guid.as_str()).and_then(|guid| by_guid.get(guid));
            if let Some(index) = target {
                let Ok(record) = datacore.record(*index) else { return };
                if !record.struct_name.starts_with(SCOPE_RECORD_PREFIX) {
                    return;
                }
                let Ok(data) = datacore.record_data(&record, 1) else { return };
                let data = data.to_json();
                if let Some(name) = data[SCOPE_NAME_FIELD].as_str().filter(|name| !name.is_empty()) {
                    if !names.iter().any(|known| known == name) {
                        names.push(name.to_string());
                    }
                }
                scope_names(datacore, by_guid, &data, names);
            } else {
                items.iter().for_each(|item| scope_names(datacore, by_guid, item, names));
            }
        }
        serde_json::Value::Object(fields) => fields.values().for_each(|field| scope_names(datacore, by_guid, field, names)),
        _ => {}
    }
}

pub fn extract_factions(p4k: &Path, out: &Path) -> Result<(usize, usize), String> {
    use p4k_formats::{dcb::Dcb, png::png_rgba};
    use p4k_scene::textures::{decode_rgba, DdsTexture};

    let archive = P4k::open(p4k).map_err(|e| format!("open {}: {e}", p4k.display()))?;
    let entry = archive.get(DATACORE_PATH).ok_or_else(|| format!("{DATACORE_PATH} not in archive"))?;
    let bytes = archive.read(entry).map_err(|e| format!("read {DATACORE_PATH}: {e}"))?;
    let datacore = Dcb::open(bytes).map_err(|e| format!("parse {DATACORE_PATH}: {e}"))?;
    let faction = *datacore.struct_names().get(FACTION_STRUCT).ok_or_else(|| format!("no {FACTION_STRUCT} struct in {DATACORE_PATH}"))?;
    let (_, count) = datacore.instance_base(faction).ok_or_else(|| format!("no {FACTION_STRUCT} records in {DATACORE_PATH}"))?;
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let (mut table, mut logos) = (String::new(), 0);
    let mut tone = TONE_DARK;
    let total: usize = datacore.record_index().values().map(Vec::len).sum();
    let by_guid: std::collections::HashMap<String, usize> = (0..total)
        .filter_map(|index| Some((p4k_formats::dcb::guid_str(datacore.record_guid_bytes(index).ok()?), index)))
        .collect();
    let records = (0..total).filter(|index| datacore.record(*index).is_ok_and(|record| record.struct_name == FACTION_STRUCT));
    let mut scopes: std::collections::HashMap<u64, Vec<String>> = std::collections::HashMap::new();
    for record in records {
        let Ok(record) = datacore.record(record) else { continue };
        let Ok(data) = datacore.record_data(&record, SCOPE_DEPTH) else { continue };
        let data = data.to_json();
        let (Some(geid), context) = (data["GEID"].as_u64(), &data["reputationContextPropertiesUI"]) else { continue };
        let mut names = Vec::new();
        scope_names(&datacore, &by_guid, context, &mut names);
        scopes.insert(geid, names);
    }
    for index in 0..count {
        let record = datacore.read_instance(faction, index, 0, 1).map_err(|e| format!("read {FACTION_STRUCT} {index}: {e}"))?.to_json();
        let Some(geid) = record["GEID"].as_u64().filter(|geid| *geid != 0) else { continue };
        let name = record["displayName"].as_str().unwrap_or_default();
        // The logo is named as a .tif; the archive holds the .dds.
        let logo = record["logo"].as_str().unwrap_or_default();
        let source = logo.rsplit_once('.').map(|(stem, _)| format!("{ARCHIVE_ROOT}{stem}{LOGO_SOURCE_EXTENSION}"));
        let file = format!("{geid}.png");
        let written = match source.as_deref().map(|path| (path, archive.get(path))) {
            None => false,
            Some((path, None)) => {
                eprintln!("faction {geid}: {path} is not in the archive");
                false
            }
            Some((path, Some(logo))) => match DdsTexture::new(&archive, logo, false) {
                Err(error) => {
                    eprintln!("faction {geid}: {path}: {error}");
                    false
                }
                Ok(texture) => match decode_rgba(&texture, LOGO_MAX_SIZE) {
                    None => {
                        eprintln!("faction {geid}: {path} cannot be decoded");
                        false
                    }
                    Some((width, height, pixels)) => {
                        tone = if is_light(&pixels) { TONE_LIGHT } else { TONE_DARK };
                        std::fs::write(out.join(&file), png_rgba(width as u32, height as u32, &pixels)).is_ok()
                    }
                },
            },
        };
        logos += usize::from(written);
        let scopes = scopes.get(&geid).map(|names| names.join(SCOPE_SEPARATOR)).unwrap_or_default();
        table.push_str(&format!("{geid}\t{name}\t{}\t{tone}\t{scopes}\n", if written { file } else { String::new() }));
        tone = TONE_DARK;
    }
    std::fs::write(out.join(FACTIONS_FILE), table).map_err(|e| format!("write {FACTIONS_FILE}: {e}"))?;
    Ok((count, logos))
}

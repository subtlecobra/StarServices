//! Embeds what `scripts/game-files.sh` extracted from the game into `assets/`: the reputation icons (`reputation/*.svg`)
//! and the English texts (`localization/global.ini`). They come from the game and are not in the repository: a build
//! without them is valid, its pages show no icons and the `@key` names as they are.

use std::{env, fmt::Write, fs, path::PathBuf};

/// The folders and files of the assets, relative to this package.
const ASSETS: &str = "assets";
const ICON_FOLDER: &str = "reputation";
const TEXTS_FILE: &str = "localization/global.ini";
const FACTION_FOLDER: &str = "factions";
const FACTIONS_FILE: &str = "factions.tsv";

fn main() {
    let assets = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join(ASSETS);
    println!("cargo:rerun-if-changed={ASSETS}");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    let mut files: Vec<String> = fs::read_dir(assets.join(ICON_FOLDER))
        .map(|entries| entries.filter_map(Result::ok).filter_map(|entry| entry.file_name().into_string().ok()).filter(|name| name.ends_with(".svg")).collect())
        .unwrap_or_default();
    files.sort();
    let mut table = String::from("/// The icons found at build time: the file name and the SVG text.\npub const ICONS: &[(&str, &str)] = &[\n");
    for file in &files {
        let _ = writeln!(table, "    ({file:?}, include_str!({:?})),", assets.join(ICON_FOLDER).join(file).display().to_string());
    }
    table.push_str("];\n");
    fs::write(out.join("reputation_icons.rs"), table).expect("write reputation_icons.rs");

    let folder = assets.join(FACTION_FOLDER);
    let table_file = folder.join(FACTIONS_FILE);
    let rows = if table_file.is_file() { format!("include_str!({:?})", table_file.display().to_string()) } else { String::from("\"\"") };
    let mut logos: Vec<String> = fs::read_dir(&folder)
        .map(|entries| entries.filter_map(Result::ok).filter_map(|entry| entry.file_name().into_string().ok()).filter(|name| name.ends_with(".png")).collect())
        .unwrap_or_default();
    logos.sort();
    let mut table = format!("/// The factions of the game data found at build time: lines of GEID, name key and logo file.\npub const FACTIONS: &str = {rows};\n/// Their logos: the file name and the PNG.\npub const FACTION_LOGOS: &[(&str, &[u8])] = &[\n");
    for file in &logos {
        let _ = writeln!(table, "    ({file:?}, include_bytes!({:?})),", folder.join(file).display().to_string());
    }
    table.push_str("];\n");
    fs::write(out.join("factions.rs"), table).expect("write factions.rs");

    let texts = assets.join(TEXTS_FILE);
    let source = if texts.is_file() { format!("include_str!({:?})", texts.display().to_string()) } else { String::from("\"\"") };
    let table = format!("/// The game's English texts (`key=text` lines) found at build time, empty without them.\npub const TEXTS: &str = {source};\n");
    fs::write(out.join("localization_texts.rs"), table).expect("write localization_texts.rs");
}

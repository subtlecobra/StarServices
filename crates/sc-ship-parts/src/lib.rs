//! The parts and object containers of every vehicle of a game build: `tables/ship_parts.bin`, which `tools/gamedata-build`
//! generates from the DataCore and the socpaks (docs/DATA.md has the format, docs/SHIPS.md the rules). The DGS binds a ship's
//! parts from it, and sc-entitygraph makes them entities attached to the ship (the Vehicle Loadout Manager reads that tree).

/// The path of the parts in a build of the game data.
pub const SHIP_PARTS_PATH: &str = "tables/ship_parts.bin";
/// The first bytes of the file and the version of its format this reader takes.
pub const MAGIC: [u8; 4] = *b"SCSP";
pub const VERSION: u32 = 2;
/// Bytes of the header (magic, version, ship, part and container counts), of a ship row (class CRC, first part, part count,
/// first container, container count), of a part row (parent, attach id, class CRC, flags) and of a container row (attach
/// id, class CRC, socpak hash, static and dynamic counts, radius, GUID, position, rotation), all little endian.
const HEADER_BYTES: usize = 20;
const SHIP_BYTES: usize = 20;
const PART_BYTES: usize = 16;
const CONTAINER_BYTES: usize = 68;
const WORD_BYTES: usize = 4;
const GUID_BYTES: usize = 16;
/// Where the GUID, the position and the rotation are in a container row.
const CONTAINER_GUID_OFFSET: usize = 24;
const CONTAINER_POSITION_OFFSET: usize = 40;
const CONTAINER_ROTATION_OFFSET: usize = 52;
/// The parent of a part that is in a port of the ship itself.
const PARENT_IS_SHIP: i32 = -1;

/// A part the client spawns itself: it comes only from the default loadout of its (nested) parent's class, so an entity
/// already sits in its port when the client's `CLoadoutDummy::InitLoadout` places it (`DuplicatedItemPorts`). The DGS still
/// binds it, but with this bit masked out (`Part::bound_flags`: it never goes on the wire), and the graph has no entity for
/// it. No class has this bit (they use `0x1000`, `0x100000`, `0x200000`, `0x4000000` and `0x80000000`).
pub const FLAG_DEFAULT_OF_PARENT: u32 = 0x2000_0000;

/// One part of a ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub class_crc: u32,
    /// The attach id of the port of its parent it is in.
    pub attach_id: u32,
    pub flags: u32,
    /// The index of its parent among the ship's parts; `None` for the ship.
    pub parent: Option<usize>,
}

impl Part {
    /// The flags a bind carries: the table's without `FLAG_DEFAULT_OF_PARENT`.
    pub fn bound_flags(&self) -> u32 {
        self.flags & !FLAG_DEFAULT_OF_PARENT
    }

    /// Whether the client spawns this part itself from its parent class's default loadout.
    pub fn is_default_of_parent(&self) -> bool {
        self.flags & FLAG_DEFAULT_OF_PARENT != 0
    }
}

/// One object container of a ship.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Container {
    /// CRC-32C of `<boneName>-<socpak name>` in lower case.
    pub attach_id: u32,
    pub class_crc: u32,
    /// `ocFilename`.
    pub socpak_hash: u32,
    /// `entityCountStatic` and `entityCountDynamic` of the socpak.
    pub statics: u32,
    pub dynamics: u32,
    /// `socRadius`.
    pub radius: f32,
    /// `uhGuid` as it goes on the wire.
    pub guid: [u8; GUID_BYTES],
    /// Its offset in the ship: position, and rotation `x y z w`.
    pub position: [f32; 3],
    pub rotation: [f32; 4],
}

/// The parts and containers of every vehicle class: the file, kept whole and read in place.
#[derive(Debug, Default)]
pub struct ShipParts {
    bytes: Vec<u8>,
    ships: usize,
    parts: usize,
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + WORD_BYTES].try_into().expect("a word"))
}

fn float(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(word(bytes, offset))
}

impl ShipParts {
    /// Checks the file: its header, that every ship's parts and containers are in it, ships sorted, and parents before
    /// children. An error says what is wrong.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() < HEADER_BYTES || bytes[..MAGIC.len()] != MAGIC {
            return Err(format!("{SHIP_PARTS_PATH}: not a ship parts file"));
        }
        let version = word(&bytes, WORD_BYTES);
        if version != VERSION {
            return Err(format!("{SHIP_PARTS_PATH}: version {version}, not {VERSION}"));
        }
        let count = |n: usize| word(&bytes, n * WORD_BYTES) as usize;
        let (ships, parts, containers) = (count(2), count(3), count(4));
        let size = HEADER_BYTES + ships * SHIP_BYTES + parts * PART_BYTES + containers * CONTAINER_BYTES;
        if bytes.len() != size {
            return Err(format!(
                "{SHIP_PARTS_PATH}: {} bytes for {ships} ships, {parts} parts and {containers} containers",
                bytes.len()
            ));
        }
        let table = Self { bytes, ships, parts };
        let mut previous = None;
        for index in 0..ships {
            let (class_crc, first, count, first_container, container_count) = table.ship_row(index);
            if previous.is_some_and(|previous| previous >= class_crc) {
                return Err(format!("{SHIP_PARTS_PATH}: ships not sorted at {class_crc:#010x}"));
            }
            previous = Some(class_crc);
            if first + count > parts || first_container + container_count > containers {
                return Err(format!("{SHIP_PARTS_PATH}: the parts of {class_crc:#010x} are past the end"));
            }
            for part in 0..count {
                let parent = table.part_row(first + part).0;
                if parent != PARENT_IS_SHIP && !(0..part as i32).contains(&parent) {
                    return Err(format!("{SHIP_PARTS_PATH}: part {part} of {class_crc:#010x} has parent {parent}"));
                }
            }
        }
        Ok(table)
    }

    fn ship_row(&self, index: usize) -> (u32, usize, usize, usize, usize) {
        let offset = HEADER_BYTES + index * SHIP_BYTES;
        let field = |n: usize| word(&self.bytes, offset + n * WORD_BYTES);
        (field(0), field(1) as usize, field(2) as usize, field(3) as usize, field(4) as usize)
    }

    fn part_row(&self, index: usize) -> (i32, u32, u32, u32) {
        let offset = HEADER_BYTES + self.ships * SHIP_BYTES + index * PART_BYTES;
        let field = |n: usize| word(&self.bytes, offset + n * WORD_BYTES);
        (field(0) as i32, field(1), field(2), field(3))
    }

    fn container_row(&self, index: usize) -> Container {
        let offset = HEADER_BYTES + self.ships * SHIP_BYTES + self.parts * PART_BYTES + index * CONTAINER_BYTES;
        let field = |n: usize| word(&self.bytes, offset + n * WORD_BYTES);
        let floats = |at: usize, n: usize| float(&self.bytes, offset + at + n * WORD_BYTES);
        let guid = offset + CONTAINER_GUID_OFFSET;
        Container {
            attach_id: field(0),
            class_crc: field(1),
            socpak_hash: field(2),
            statics: field(3),
            dynamics: field(4),
            radius: float(&self.bytes, offset + 5 * WORD_BYTES),
            guid: self.bytes[guid..guid + GUID_BYTES].try_into().expect("a GUID"),
            position: [0, 1, 2].map(|n| floats(CONTAINER_POSITION_OFFSET, n)),
            rotation: [0, 1, 2, 3].map(|n| floats(CONTAINER_ROTATION_OFFSET, n)),
        }
    }

    /// The number of vehicle classes.
    pub fn ships(&self) -> usize {
        self.ships
    }

    /// The row of a ship class by a binary search.
    fn find(&self, ship_class_crc: u32) -> Option<(u32, usize, usize, usize, usize)> {
        let (mut low, mut high) = (0, self.ships);
        while low < high {
            let middle = (low + high) / 2;
            let row = self.ship_row(middle);
            match row.0.cmp(&ship_class_crc) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(row),
            }
        }
        None
    }

    /// The parts of a ship class, parents first; `None` for a class the file does not have.
    pub fn parts(&self, ship_class_crc: u32) -> Option<Vec<Part>> {
        let (_, first, count, _, _) = self.find(ship_class_crc)?;
        Some(
            (first..first + count)
                .map(|index| {
                    let (parent, attach_id, class_crc, flags) = self.part_row(index);
                    Part { class_crc, attach_id, flags, parent: (parent != PARENT_IS_SHIP).then_some(parent as usize) }
                })
                .collect(),
        )
    }

    /// The object containers of a ship class; empty for a class the file does not have.
    pub fn containers(&self, ship_class_crc: u32) -> Vec<Container> {
        self.find(ship_class_crc)
            .map(|(_, _, _, first, count)| (first..first + count).map(|index| self.container_row(index)).collect())
            .unwrap_or_default()
    }

    /// The file of a list of ships, their parts and containers, as `tools/gamedata-build` writes it.
    pub fn encode(ships: &[(u32, Vec<Part>, Vec<Container>)]) -> Vec<u8> {
        let parts: usize = ships.iter().map(|(_, parts, _)| parts.len()).sum();
        let containers: usize = ships.iter().map(|(_, _, containers)| containers.len()).sum();
        let mut out = MAGIC.to_vec();
        for value in [VERSION, ships.len() as u32, parts as u32, containers as u32] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        let (mut first, mut first_container) = (0u32, 0u32);
        for (class_crc, parts, containers) in ships {
            for value in [*class_crc, first, parts.len() as u32, first_container, containers.len() as u32] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            first += parts.len() as u32;
            first_container += containers.len() as u32;
        }
        for part in ships.iter().flat_map(|(_, parts, _)| parts) {
            out.extend_from_slice(&part.parent.map_or(PARENT_IS_SHIP, |parent| parent as i32).to_le_bytes());
            for value in [part.attach_id, part.class_crc, part.flags] {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        for container in ships.iter().flat_map(|(_, _, containers)| containers) {
            for value in [container.attach_id, container.class_crc, container.socpak_hash, container.statics, container.dynamics] {
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&container.radius.to_le_bytes());
            out.extend_from_slice(&container.guid);
            for number in container.position.iter().chain(&container.rotation) {
                out.extend_from_slice(&number.to_le_bytes());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIP: u32 = 0x8edc_05e2;

    fn part(class_crc: u32) -> Part {
        Part { class_crc, attach_id: 2, flags: 3, parent: None }
    }

    fn container() -> Container {
        Container {
            attach_id: 1,
            class_crc: 2,
            socpak_hash: 3,
            statics: 15,
            dynamics: 0,
            radius: 1.5,
            guid: [7; GUID_BYTES],
            position: [0.5, -1.0, 2.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
        }
    }

    #[test]
    fn the_default_of_parent_flag_roundtrips_and_is_masked_for_binds() {
        let flagged = Part { flags: 0x8020_1000 | FLAG_DEFAULT_OF_PARENT, parent: Some(0), ..part(2) };
        let plain = part(1);
        let table = ShipParts::parse(ShipParts::encode(&[(SHIP, vec![plain, flagged], Vec::new())])).unwrap();
        let parts = table.parts(SHIP).unwrap();
        assert_eq!(parts, [plain, flagged]);
        assert!(!parts[0].is_default_of_parent() && parts[1].is_default_of_parent());
        assert_eq!(parts[1].bound_flags(), 0x8020_1000);
        assert_eq!(parts[0].bound_flags(), parts[0].flags);
    }

    #[test]
    fn a_broken_file_is_refused_with_why() {
        let good = ShipParts::encode(&[(SHIP, vec![part(1)], Vec::new())]);
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(ShipParts::parse(bad_magic).unwrap_err().contains("not a ship parts file"));
        let mut bad_version = good.clone();
        bad_version[WORD_BYTES] = 1;
        assert!(ShipParts::parse(bad_version).unwrap_err().contains("version 1"));
        assert!(ShipParts::parse(good[..good.len() - 1].to_vec()).unwrap_err().contains("bytes for"));
        let child_first = ShipParts::encode(&[(SHIP, vec![Part { parent: Some(0), ..part(1) }], Vec::new())]);
        assert!(ShipParts::parse(child_first).unwrap_err().contains("has parent 0"));
        let unsorted = ShipParts::encode(&[(2, Vec::new(), Vec::new()), (1, Vec::new(), Vec::new())]);
        assert!(ShipParts::parse(unsorted).unwrap_err().contains("not sorted"));
    }

    #[test]
    fn many_ships_are_found_by_class() {
        let ships: Vec<(u32, Vec<Part>, Vec<Container>)> = (0..100u32)
            .map(|n| {
                let parts = (0..n % 4).map(|k| Part { class_crc: n, attach_id: k, flags: 0, parent: None }).collect();
                let containers = (0..n % 3)
                    .map(|k| {
                        let mut other = container();
                        other.attach_id = n * 10 + k;
                        other
                    })
                    .collect();
                (n * 7, parts, containers)
            })
            .collect();
        let table = ShipParts::parse(ShipParts::encode(&ships)).unwrap();
        assert_eq!(table.ships(), 100);
        for (class_crc, parts, containers) in &ships {
            assert_eq!(table.parts(*class_crc).as_ref(), Some(parts));
            assert_eq!(&table.containers(*class_crc), containers);
        }
        assert_eq!(table.parts(1), None);
    }
}

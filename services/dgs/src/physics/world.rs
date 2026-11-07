//! The static collision of a zone: one triangle mesh in the frame of the zone host.
//!
//! A hangar's comes from `collision/<socpak hash>.col` of its game build, written by `tools/gamedata-build` from the
//! physics proxies of every brush of the hangar's socpak and its child containers. The file is zlib compressed: `SCHC`, u32 version 1, u32
//! socpak hash, u32 shape count, u32 instance count, 6 f32 bounds; per shape u32 vertex count, u32 triangle count, f32
//! `x y z` per vertex, u32 `a b c` per triangle; per instance u32 shape index and a row-major 3x4 f32 matrix into the
//! hangar's frame. All little endian.
//!
//! Where the bytes come from: the region's `sc-gamedata` for the game build the shard runs, or a build directory for a local
//! run, which `crate::gamedata` reads and hands to `World::from_bytes` at startup. Nothing is embedded in the image.
//! `World::decode` only sees bytes.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

use parry3d::math::Vector;
use parry3d::shape::TriMesh;

const MAGIC: &[u8; 4] = b"SCHC";
const VERSION: u32 = 1;

fn loaded() -> &'static Mutex<HashMap<u32, Arc<World>>> {
    static LOADED: OnceLock<Mutex<HashMap<u32, Arc<World>>>> = OnceLock::new();
    LOADED.get_or_init(Default::default)
}

/// `hangar_smlfrnt_002_rest_rund`, Orbituary's hangar, whose collision tests read from `testdata/`.
#[cfg(test)]
pub(crate) const REST_STOP: u32 = 1_200_814_982;
/// The rest stop hangar's collision file, as `tools/gamedata-build` writes it.
#[cfg(test)]
pub(crate) fn rest_stop_collision() -> &'static [u8] {
    crate::testdata::bytes(&crate::gamedata::collision_path(REST_STOP))
}

/// The rest stop hangar's collision for tests, built once.
#[cfg(test)]
pub(crate) fn rest_stop() -> Arc<World> {
    World::loaded(REST_STOP).unwrap_or_else(|| World::from_bytes(REST_STOP, rest_stop_collision()).unwrap())
}

/// Static triangles a capsule collides with.
pub struct World {
    pub mesh: TriMesh,
    /// The box of every vertex, `min` then `max`.
    pub bounds: ([f32; 3], [f32; 3]),
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u32(&mut self) -> Result<u32, String> {
        let word = self.bytes.get(self.at..self.at + 4).ok_or("collision file truncated")?;
        self.at += 4;
        Ok(u32::from_le_bytes(word.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, String> {
        self.u32().map(f32::from_bits)
    }
}

impl World {
    pub fn from_triangles(vertices: Vec<[f32; 3]>, triangles: Vec<[u32; 3]>) -> Result<Self, String> {
        if triangles.is_empty() {
            return Err("no triangles".into());
        }
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for vertex in &vertices {
            for axis in 0..3 {
                min[axis] = min[axis].min(vertex[axis]);
                max[axis] = max[axis].max(vertex[axis]);
            }
        }
        let vertices = vertices.into_iter().map(|[x, y, z]| Vector::new(x, y, z)).collect();
        let mesh = TriMesh::new(vertices, triangles).map_err(|error| format!("{error:?}"))?;
        Ok(Self { mesh, bounds: (min, max) })
    }

    /// Reads a collision file, placing every instance of its shapes in one mesh.
    pub fn decode(compressed: &[u8]) -> Result<Self, String> {
        let mut bytes = Vec::new();
        flate2::read::ZlibDecoder::new(compressed).read_to_end(&mut bytes).map_err(|error| error.to_string())?;
        if bytes.get(..4) != Some(MAGIC) {
            return Err("not a collision file".into());
        }
        let mut reader = Reader { bytes: &bytes, at: 4 };
        let version = reader.u32()?;
        if version != VERSION {
            return Err(format!("collision file version {version}"));
        }
        let _hash = reader.u32()?;
        let (shape_count, instance_count) = (reader.u32()?, reader.u32()?);
        for _ in 0..6 {
            reader.f32()?;
        }
        let mut shapes = Vec::with_capacity(shape_count as usize);
        for _ in 0..shape_count {
            let (vertex_count, triangle_count) = (reader.u32()?, reader.u32()?);
            let vertices: Vec<[f32; 3]> = (0..vertex_count).map(|_| Ok([reader.f32()?, reader.f32()?, reader.f32()?])).collect::<Result<_, String>>()?;
            let triangles: Vec<[u32; 3]> = (0..triangle_count).map(|_| Ok([reader.u32()?, reader.u32()?, reader.u32()?])).collect::<Result<_, String>>()?;
            if triangles.iter().flatten().any(|&index| index >= vertex_count) {
                return Err("triangle names a missing vertex".into());
            }
            shapes.push((vertices, triangles));
        }
        let (mut vertices, mut triangles) = (Vec::new(), Vec::new());
        for _ in 0..instance_count {
            let shape = reader.u32()? as usize;
            let mut matrix = [0f32; 12];
            for value in &mut matrix {
                *value = reader.f32()?;
            }
            let (shape_vertices, shape_triangles) = shapes.get(shape).ok_or("instance of a missing shape")?;
            let base = vertices.len() as u32;
            vertices.extend(shape_vertices.iter().map(|v| {
                std::array::from_fn(|row| matrix[4 * row] * v[0] + matrix[4 * row + 1] * v[1] + matrix[4 * row + 2] * v[2] + matrix[4 * row + 3])
            }));
            triangles.extend(shape_triangles.iter().map(|t| t.map(|index| index + base)));
        }
        Self::from_triangles(vertices, triangles)
    }

    /// Builds a hangar's collision from the bytes of its file (`crate::gamedata`) and keeps it: every player in such a
    /// hangar shares it, as each player's hangar is a zone of its own with the same frame. Takes a third of a second, so
    /// call it at startup or off the async runtime, and `loaded` elsewhere.
    pub fn from_bytes(hash: u32, bytes: &[u8]) -> Result<Arc<World>, String> {
        let world = Arc::new(World::decode(bytes)?);
        Ok(loaded().lock().unwrap().entry(hash).or_insert(world).clone())
    }

    /// The collision of a hangar socpak if it is already built, without building it.
    pub fn loaded(hash: u32) -> Option<Arc<World>> {
        loaded().lock().unwrap().get(&hash).cloned()
    }

    pub fn triangles(&self) -> usize {
        self.mesh.indices().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::physics::{step, Input, Params, State};

    /// The spawn points of the rest stop hangar, from the home locations table and the protocol notes (kept outside this repository).
    const SPAWN: [f64; 3] = [5.9, -85.14, 4.51];

    const DT: f64 = 1.0 / 30.0;

    fn walk(world: &World, mut state: State, input: &Input, frames: usize) -> (State, f64, f64) {
        let (mut low, mut high) = (f64::MAX, f64::MIN);
        for _ in 0..frames {
            state = step(world, &state, input, DT, &Params::default());
            low = low.min(state.position[2]);
            high = high.max(state.position[2]);
        }
        (state, low, high)
    }

    fn at_spawn(world: &World) -> State {
        walk(world, State::standing_at([SPAWN[0], SPAWN[1], SPAWN[2] + 0.3], 0.0), &Input::default(), 30).0
    }

    fn forward(yaw_degrees: f64) -> Input {
        Input { wish: [0.0, 1.0], yaw: yaw_degrees.to_radians(), ..Input::default() }
    }

    #[test]
    fn the_rest_stop_hangar_decodes() {
        let world = rest_stop();
        assert!(world.triangles() > 500_000, "{}", world.triangles());
        assert!(World::loaded(1).is_none());
        assert!(World::decode(b"not zlib").is_err());
    }

    /// The real floor: the spawn point stands on it, within the 3 cm the shapes were simplified by.
    #[test]
    fn the_spawn_point_stands_on_the_hangar_floor() {
        let world = rest_stop();
        let state = at_spawn(&world);
        assert!(state.grounded && (state.position[2] - SPAWN[2]).abs() < 0.05, "{state:?}");
        let (still, low, high) = walk(&world, state, &Input::default(), 150);
        assert!((0..3).all(|axis| (still.position[axis] - state.position[axis]).abs() < 1e-3), "{still:?}");
        assert!(high - low < 1e-3, "{low}..{high}");
    }

    /// The spawn closet: its walls stop a player within a few metres, and it opens towards -X, where its door is (an
    /// entity, so not in the collision).
    #[test]
    fn the_spawn_closet_has_walls_and_an_opening() {
        let world = rest_stop();
        let start = at_spawn(&world);
        for yaw in [0.0, 180.0, 270.0] {
            let (state, low, high) = walk(&world, start, &forward(yaw), 90);
            let moved = (state.position[0] - start.position[0]).hypot(state.position[1] - start.position[1]);
            assert!(moved < 3.0, "yaw {yaw}: {state:?}");
            assert!(state.grounded && high - low < 0.1, "yaw {yaw}: {low}..{high}");
        }
        let (state, low, _) = walk(&world, start, &forward(90.0), 150);
        assert!(state.position[0] < start.position[0] - 20.0, "{state:?}");
        assert!(low > SPAWN[2] - 0.5 && state.grounded, "walked out on the hangar's floor: {state:?}, lowest {low}");
    }
}

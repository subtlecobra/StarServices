//! What an entity is, apart from how it is sent: no sockets, keys or peers. Entity logic builds these, a DGS hands them
//! over as `EntityBind`, and the replicant writes them to a client.

use sc_api::internal::{EntityBind, EntitySnapshot, EntityTransform};

/// `IGameFramework::GetUniverseRootEntityId`. The client stores this entity as the root of its model,
/// and it alone may arrive without a bound parent.
pub const UNIVERSE_ROOT_ID: u64 = 0xffff_ffff_ffff_fffc;

/// One variable of a bind, as `FUN_141597530` stores it: `key` is the CRC-32 of the lowercased name and
/// `type_id` that of the value's serializer. The client decodes `value` when the entity spawns.
pub struct Snapshot {
    pub key: u32,
    pub type_id: u32,
    pub value: Vec<u8>,
}

/// A `QuatTS` with a scale of 1, relative to the entity's parent. The rotation is `x y z w`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub position: [f64; 3],
    pub rotation: [f64; 4],
}

impl Transform {
    pub const IDENTITY: Self = Self { position: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0] };

    /// `child`, given in this transform's frame, in this transform's parent's frame.
    pub fn then(&self, child: &Self) -> Self {
        let [x, y, z, w] = self.rotation;
        let [cx, cy, cz, cw] = child.rotation;
        let v = child.position;
        // v + 2w(u × v) + 2u × (u × v), u = (x, y, z).
        let cross = |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
        let u = [x, y, z];
        let uv = cross(u, v);
        let uuv = cross(u, uv);
        let position = [0, 1, 2].map(|axis| self.position[axis] + v[axis] + 2.0 * (w * uv[axis] + uuv[axis]));
        let rotation = [
            w * cx + x * cw + y * cz - z * cy,
            w * cy - x * cz + y * cw + z * cx,
            w * cz + x * cy - y * cx + z * cw,
            w * cw - x * cx - y * cy - z * cz,
        ];
        Self { position, rotation }
    }
}

/// The fields of a `BindBatchAddBind` a DGS chooses. The rest are the values of an entity with no tags or owner, and the
/// batch, which is the replicant's.
pub struct Bind {
    pub entity_id: u64,
    pub class_crc: u32,
    pub flags: u32,
    pub transform: Transform,
    pub parent_id: u64,
    pub parent_attach_id: u32,
    /// Name hashes of components to add to those of the class.
    pub components: Vec<u32>,
    pub snapshots: Vec<Snapshot>,
}

impl From<Snapshot> for EntitySnapshot {
    fn from(snapshot: Snapshot) -> Self {
        Self { key: snapshot.key, type_id: snapshot.type_id, value: snapshot.value }
    }
}

impl From<Transform> for EntityTransform {
    fn from(transform: Transform) -> Self {
        let ([x, y, z], [qx, qy, qz, qw]) = (transform.position, transform.rotation);
        Self { x, y, z, qx, qy, qz, qw }
    }
}

impl From<Bind> for EntityBind {
    fn from(bind: Bind) -> Self {
        Self {
            entity_id: bind.entity_id,
            class_crc: bind.class_crc,
            flags: bind.flags,
            transform: Some(bind.transform.into()),
            parent_id: bind.parent_id,
            parent_attach_id: bind.parent_attach_id,
            components: bind.components,
            snapshots: bind.snapshots.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_turns_and_moves_with_its_parent() {
        let half = std::f64::consts::FRAC_1_SQRT_2;
        let parent = Transform { position: [10.0, 0.0, 0.0], rotation: [0.0, 0.0, half, half] };
        let child = Transform { position: [1.0, 0.0, 0.0], rotation: [0.0, 0.0, half, half] };
        let both = parent.then(&child);
        let close = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-12);
        assert!(close(&both.position, &[10.0, 1.0, 0.0]), "{:?}", both.position);
        assert!(close(&both.rotation, &[0.0, 0.0, 1.0, 0.0]), "{:?}", both.rotation);
        assert_eq!(Transform::IDENTITY.then(&child), child);
    }
}

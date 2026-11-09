//! What the server has told the bot: the entities bound and unbound, and the events in the order they came.

use std::collections::HashMap;

use serde::Serialize;

use crate::wire::Bind;

/// `GameRulesSingleton`, the universe root, bound with this id (-4).
pub const UNIVERSE_ROOT_ID: u64 = 0xffff_ffff_ffff_fffc;
pub const GAME_RULES_CLASS: u32 = 0x42aa_0b07;
pub const SOLAR_SYSTEM_CLASS: u32 = 0x479c_0d1d;
/// `Player`, the class of the player's entity.
pub const PLAYER_CLASS: u32 = 2_961_494_058;
/// The zone attachment: the key a zone's children hang by.
pub const ZONE_ATTACHMENT: u32 = 0x46c0_1eb2;
/// `EntityClassDefinition.ATC_DataManager`.
pub const ATC_CLASS: u32 = 1_225_794_389;
/// `ShipSelectorKiosk_Hangar_HighTech_1_a`.
pub const KIOSK_CLASS: u32 = 1_689_243_590;
/// The kiosk's variable that names its ATC (CRC-32C of the lowercased name `ownerATCId`).
pub const OWNER_ATC_KEY: u32 = 0xf9df_8d76;
/// The transit call panel of a hangar's elevator.
pub const CALL_PANEL_CLASS: u32 = 3_016_690_202;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// The server's connection state (message 6).
    State { state: u8 },
    Bind { entity: u64, class_crc: u32, parent: u64, attach: u32, batch: u32 },
    Unbind { entity: u64 },
    BatchEnd { batch: u32 },
    /// An `EntityRemoteMethod` from the server.
    Method { entity: u64, method: u32, args_hex: String },
    /// An `EntityTokenEvent`.
    Token { entity: u64, token: u32, request: u32, decision: u8 },
    /// `UpdateBubbleCreate`.
    Bubble,
    Disconnect { reason: u32, text: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    /// Milliseconds since the session began.
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The entities the client has, by id, and the order they were first bound in.
#[derive(Default)]
pub struct World {
    binds: HashMap<u64, Bind>,
    order: Vec<u64>,
}

impl World {
    pub fn bind(&mut self, bind: Bind) {
        let id = bind.entity_id;
        if self.binds.insert(id, bind).is_none() {
            self.order.push(id);
        }
    }

    /// Removes an entity and, as the client does, everything that hangs from it.
    pub fn unbind(&mut self, entity: u64) {
        let mut gone = vec![entity];
        while let Some(id) = gone.pop() {
            if self.binds.remove(&id).is_some() {
                self.order.retain(|other| *other != id);
                gone.extend(self.binds.values().filter(|bind| bind.parent_id == id).map(|bind| bind.entity_id));
            }
        }
    }

    pub fn get(&self, entity: u64) -> Option<&Bind> {
        self.binds.get(&entity)
    }

    pub fn len(&self) -> usize {
        self.binds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.binds.is_empty()
    }

    /// The bound entities in the order they were first bound.
    pub fn all(&self) -> impl Iterator<Item = &Bind> {
        self.order.iter().filter_map(|id| self.binds.get(id))
    }

    pub fn of_class(&self, class_crc: u32) -> Vec<&Bind> {
        self.all().filter(|bind| bind.class_crc == class_crc).collect()
    }

    pub fn children(&self, parent: u64) -> Vec<&Bind> {
        self.all().filter(|bind| bind.parent_id == parent).collect()
    }

    /// Whether `entity` hangs, at any depth, from `ancestor`.
    pub fn descends_from(&self, entity: u64, ancestor: u64) -> bool {
        let mut current = self.binds.get(&entity);
        for _ in 0..64 {
            let Some(bind) = current else { return false };
            if bind.parent_id == ancestor {
                return true;
            }
            current = self.binds.get(&bind.parent_id);
        }
        false
    }

    pub fn player(&self) -> Option<&Bind> {
        self.all().find(|bind| bind.class_crc == PLAYER_CLASS)
    }

    /// The items the player wears: everything below the player entity, parents before children.
    pub fn worn(&self) -> Vec<&Bind> {
        match self.player() {
            Some(player) => self.all().filter(|bind| self.descends_from(bind.entity_id, player.entity_id)).collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(id: u64, parent: u64, class_crc: u32) -> Bind {
        Bind { entity_id: id, batch: 1, class_crc, components: vec![], flags: 0, position: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], scale: 1.0, parent_id: parent, parent_attach_id: 0, snapshots: vec![] }
    }

    #[test]
    fn an_unbind_takes_the_children_with_it() {
        let mut world = World::default();
        world.bind(bind(1, 0, PLAYER_CLASS));
        world.bind(bind(2, 1, 5));
        world.bind(bind(3, 2, 6));
        world.bind(bind(4, 0, 7));
        assert_eq!(world.worn().iter().map(|b| b.entity_id).collect::<Vec<_>>(), [2, 3]);
        world.unbind(2);
        assert_eq!(world.all().map(|b| b.entity_id).collect::<Vec<_>>(), [1, 4]);
    }

    #[test]
    fn a_bind_of_a_known_entity_replaces_it_in_place() {
        let mut world = World::default();
        world.bind(bind(1, 0, 5));
        world.bind(bind(2, 0, 5));
        world.bind(bind(1, 2, 6));
        assert_eq!((world.len(), world.get(1).unwrap().parent_id, world.all().next().unwrap().entity_id), (2, 2, 1));
    }
}

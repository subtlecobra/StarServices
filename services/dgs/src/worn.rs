//! What a live player wears, as the DGS holds it: the tree of the items bound under the player, each attached to a port of
//! its parent. It is the DGS's own state of an entity it has authority over (see `equip` for how a client's inventory
//! requests change it, and "Equipping" in docs/INVENTORY.md). A port is named by the CRC of its lowercased name, as the
//! binds carry it; the names a request uses come from the client.

use crate::{entity::Bind, loadout::port_attach_id};

/// One worn item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WornItem {
    pub geid: u64,
    pub class_crc: u32,
    /// The player for the body, otherwise the worn item whose port holds it.
    pub parent: u64,
    pub port_crc: u32,
}

/// Why a port path names no port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// Fewer than a parent's port and a port.
    TooShort,
    /// An element before the last names a port with nothing in it.
    NothingThere,
}

#[derive(Clone, Debug, Default)]
pub struct Worn {
    player: u64,
    /// Parents before children.
    items: Vec<WornItem>,
}

impl Worn {
    /// The tree of `binds` under `player`, which lists parents before children (a bind under a parent that is not in the
    /// tree or the player is left out).
    pub fn from_binds(player: u64, binds: &[Bind]) -> Self {
        let mut worn = Self { player, items: Vec::with_capacity(binds.len()) };
        for bind in binds {
            if bind.parent_id == player || worn.contains(bind.parent_id) {
                worn.items.push(WornItem { geid: bind.entity_id, class_crc: bind.class_crc, parent: bind.parent_id, port_crc: bind.parent_attach_id });
            }
        }
        worn
    }

    pub fn contains(&self, geid: u64) -> bool {
        self.items.iter().any(|item| item.geid == geid)
    }

    pub fn get(&self, geid: u64) -> Option<&WornItem> {
        self.items.iter().find(|item| item.geid == geid)
    }

    /// What `parent` holds in the port with CRC `port_crc`.
    pub fn child_at(&self, parent: u64, port_crc: u32) -> Option<&WornItem> {
        self.items.iter().find(|item| item.parent == parent && item.port_crc == port_crc)
    }

    /// The worn item a port path leads to, and the last element's port of it. A path starts at the player's own port
    /// (`Body_ItemPort:Armor_Undersuit:Armor_Helmet` is the helmet port of what is in the undersuit port of the body in the
    /// body port of the player): every element but the last is the port of the item found so far, the last is the free
    /// port the item goes to, named `port`.
    pub fn resolve<'a>(&self, path: &'a str) -> Result<(u64, &'a str), PathError> {
        let mut elements: Vec<&str> = path.split(':').filter(|element| !element.is_empty()).collect();
        let port = match elements.pop() {
            Some(port) if !elements.is_empty() => port,
            _ => return Err(PathError::TooShort),
        };
        let mut parent = self.player;
        for element in elements {
            parent = self.child_at(parent, port_attach_id(element)).ok_or(PathError::NothingThere)?.geid;
        }
        Ok((parent, port))
    }

    /// Attaches `item`, whose parent must be worn or the player.
    pub fn attach(&mut self, item: WornItem) {
        debug_assert!(item.parent == self.player || self.contains(item.parent));
        self.items.retain(|other| other.geid != item.geid);
        self.items.push(item);
    }

    /// `geid` and everything that hangs from it, parents first. Empty for an item that is not worn.
    pub fn subtree(&self, geid: u64) -> Vec<u64> {
        let mut found = Vec::new();
        if self.contains(geid) {
            found.push(geid);
        }
        let mut next = 0;
        while next < found.len() {
            let parent = found[next];
            found.extend(self.items.iter().filter(|item| item.parent == parent).map(|item| item.geid));
            next += 1;
        }
        found
    }

    /// Takes `geid` off, with what hangs from it, and says which items went (parents first).
    pub fn detach(&mut self, geid: u64) -> Vec<u64> {
        let gone = self.subtree(geid);
        self.items.retain(|item| !gone.contains(&item.geid));
        gone
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        loadout::{loadout_binds, DEFAULT_LOADOUT},
        player::player_index,
    };

    const PLAYER: u64 = 300_000_000_000;

    fn default_worn() -> (Worn, Vec<u64>) {
        let binds = loadout_binds(PLAYER, player_index(1_000_000).unwrap(), &DEFAULT_LOADOUT);
        let ids = binds.iter().map(|bind| bind.entity_id).collect();
        (Worn::from_binds(PLAYER, &binds), ids)
    }

    #[test]
    fn the_default_loadout_is_a_tree_under_the_player() {
        let (worn, ids) = default_worn();
        assert_eq!(worn.len(), 23);
        assert!(ids.iter().all(|id| worn.contains(*id)));
    }

    #[test]
    fn a_path_leads_to_the_item_whose_port_is_named_last() {
        let (worn, ids) = default_worn();
        // 0 body, 11 undersuit, 12 helmet.
        assert_eq!(worn.resolve("Body_ItemPort:Armor_Undersuit:Armor_Helmet"), Ok((ids[11], "Armor_Helmet")));
        assert_eq!(worn.resolve("Body_ItemPort:Armor_Undersuit:Armor_Helmet:helmet_visor"), Ok((ids[12], "helmet_visor")));
        assert_eq!(worn.resolve("body_itemport:ARMOR_UNDERSUIT:wep_sidearm"), Ok((ids[11], "wep_sidearm")));
        assert_eq!(worn.resolve("Body_ItemPort:Armor_Undersuit:Armor_Torso:backpack"), Err(PathError::NothingThere), "no torso armor is worn");
        assert_eq!(worn.resolve("Armor_Helmet"), Err(PathError::TooShort));
        assert_eq!(worn.resolve(""), Err(PathError::TooShort));
    }

    #[test]
    fn taking_an_item_off_takes_what_hangs_from_it() {
        let (mut worn, ids) = default_worn();
        // 12 helmet: 13 necksock and 14 visor hang from it.
        assert_eq!(worn.detach(ids[12]), vec![ids[12], ids[13], ids[14]]);
        assert!(!worn.contains(ids[14]));
        assert_eq!(worn.detach(ids[12]), Vec::<u64>::new());
        assert_eq!(worn.child_at(ids[11], port_attach_id("Armor_Helmet")), None);
    }

    #[test]
    fn an_attached_item_sits_in_its_port() {
        let (mut worn, ids) = default_worn();
        worn.detach(ids[12]);
        worn.attach(WornItem { geid: 400_000_000_900, class_crc: 7, parent: ids[11], port_crc: port_attach_id("Armor_Helmet") });
        assert_eq!(worn.child_at(ids[11], port_attach_id("armor_helmet")).map(|item| item.geid), Some(400_000_000_900));
        assert_eq!(worn.subtree(ids[11]).first(), Some(&ids[11]));
    }
}

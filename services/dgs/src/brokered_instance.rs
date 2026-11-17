//! Shared instances brokered by category, as the Siege of Orison one is (see docs/DGS_TRANSIT.md "Brokered instances").
//!
//! A hangar belongs to one player. A brokered instance belongs to nobody: a category (an `InstanceBroker` destination,
//! such as Siege's) names a location that many players use, each copy holds at most `capacity` of them, and a player
//! who asks for the category is placed in a copy that has room. A player asking with related players already inside a
//! copy (their party) is placed in that copy first, so a party lands together while the copy has room. Copies open on
//! demand and close after `IDLE_GRACE_MS` empty.
//!
//! Copies take ids from the same pool as the hangar instances (`HangarSlots`): a copy is an `InstanceKey` whose `owner`
//! is its number within the category and whose `location_id` is the category. Binding the location's entities to the
//! players' sessions is not done here; the caller does it for the copy this returns. Time is passed in.

use std::collections::BTreeMap;

use crate::hangar_slots::{HangarSlots, InstanceKey, Occupancy, Refused};

/// Where a player was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The copy, to bind and to pass to `leave`.
    pub key: InstanceKey,
    /// The id of the copy in the pool, the root of the ids of its entities.
    pub id: u64,
    /// Whether this placement opened the copy.
    pub opened: bool,
}

/// What a category allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Category {
    /// The category as the location's id (the instanced location's `locInstancedCategoryName`, hashed by the caller).
    pub id: u32,
    /// Players one copy holds.
    pub capacity: usize,
}

pub struct Brokers {
    slots: HangarSlots,
    occupancy: Occupancy,
    /// The copies open, with the next number to give in each category.
    open: BTreeMap<InstanceKey, ()>,
    next_number: BTreeMap<u32, u64>,
    /// The copy each player is in.
    placed: BTreeMap<u64, InstanceKey>,
}

impl Default for Brokers {
    fn default() -> Self {
        Self::new()
    }
}

impl Brokers {
    pub fn new() -> Self {
        Self {
            slots: HangarSlots::new(),
            occupancy: Occupancy::new(),
            open: BTreeMap::new(),
            next_number: BTreeMap::new(),
            placed: BTreeMap::new(),
        }
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// Who is in a copy.
    pub fn present(&self, key: &InstanceKey) -> Vec<u64> {
        self.occupancy.present(key)
    }

    /// The copy a player is in.
    pub fn copy_of(&self, player: u64) -> Option<InstanceKey> {
        self.placed.get(&player).copied()
    }

    /// Places `player` in a copy of `category`: the one they are in, else the one a related player is in when it has room,
    /// else the fullest copy with room (so copies fill before another opens), else a new copy. `related` are the player's
    /// party and the like.
    pub fn place(
        &mut self,
        category: Category,
        player: u64,
        related: &[u64],
        now_ms: u64,
    ) -> Result<Placement, Refused> {
        if let Some(key) = self
            .placed
            .get(&player)
            .copied()
            .filter(|key| key.location_id == category.id)
        {
            let id = self
                .slots
                .id_of(&key)
                .expect("a placed player's copy is open");
            return Ok(Placement {
                key,
                id,
                opened: false,
            });
        }
        let with_room = |brokers: &Self, key: &InstanceKey| {
            key.location_id == category.id
                && brokers.occupancy.present(key).len() < category.capacity
        };
        let chosen = related
            .iter()
            .filter_map(|other| self.placed.get(other).copied())
            .find(|key| with_room(self, key))
            .or_else(|| {
                self.open
                    .keys()
                    .copied()
                    .filter(|key| with_room(self, key))
                    .max_by_key(|key| {
                        (
                            self.occupancy.present(key).len(),
                            std::cmp::Reverse(key.owner),
                        )
                    })
            });
        let (key, id, opened) = match chosen {
            Some(key) => (
                key,
                self.slots.id_of(&key).expect("an open copy has an id"),
                false,
            ),
            None => {
                let number = self.next_number.get(&category.id).copied().unwrap_or(1);
                let key = InstanceKey {
                    owner: number,
                    location_id: category.id,
                };
                let id = self.slots.open(key)?;
                self.next_number.insert(category.id, number + 1);
                self.open.insert(key, ());
                self.occupancy.opened(key, now_ms);
                (key, id, true)
            }
        };
        self.leave(player, now_ms);
        self.occupancy.enter(key, player);
        self.placed.insert(player, key);
        Ok(Placement { key, id, opened })
    }

    /// A player left their copy (or their session ended).
    pub fn leave(&mut self, player: u64, now_ms: u64) {
        if let Some(key) = self.placed.remove(&player) {
            self.occupancy.leave(key, player, now_ms);
        }
    }

    /// Closes the copies empty for the grace period; their ids are returned and come back with `acknowledge`.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<(InstanceKey, u64)> {
        let mut closed = Vec::new();
        for key in self.occupancy.due(now_ms) {
            if self.open.remove(&key).is_none() {
                continue;
            }
            if let Some(id) = self.slots.close(&key) {
                closed.push((key, id));
            }
        }
        closed
    }

    /// The unbind of a closed copy reached its clients: its id can be used again.
    pub fn acknowledge(&mut self, id: u64) -> bool {
        self.slots.acknowledged(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hangar_slots::{IDLE_GRACE_MS, POOL_FIRST_ID};

    /// Siege of Orison's category, with room for three.
    const SIEGE: Category = Category {
        id: 0x51E6_E001,
        capacity: 3,
    };
    const OTHER: Category = Category {
        id: 0x51E6_E002,
        capacity: 3,
    };
    const START_MS: u64 = 1_000;

    #[test]
    fn copies_fill_before_another_opens() {
        let mut brokers = Brokers::new();
        let first = brokers.place(SIEGE, 1, &[], START_MS).unwrap();
        assert!(first.opened);
        assert_eq!(first.id, POOL_FIRST_ID);
        let (two, three) = (
            brokers.place(SIEGE, 2, &[], START_MS).unwrap(),
            brokers.place(SIEGE, 3, &[], START_MS).unwrap(),
        );
        assert_eq!((two.key, three.key), (first.key, first.key));
        let fourth = brokers.place(SIEGE, 4, &[], START_MS).unwrap();
        assert!(fourth.opened);
        assert_ne!(fourth.key, first.key);
        assert_eq!(brokers.open_count(), 2);
    }

    #[test]
    fn a_party_lands_together_while_there_is_room() {
        let mut brokers = Brokers::new();
        let first = brokers.place(SIEGE, 1, &[], START_MS).unwrap();
        brokers.place(SIEGE, 2, &[], START_MS).unwrap();
        brokers.place(SIEGE, 3, &[], START_MS).unwrap();
        let second = brokers.place(SIEGE, 4, &[], START_MS).unwrap();
        assert_ne!(second.key, first.key);
        // Player 5 is in 4's party: the second copy, though the first would take them last.
        brokers.leave(3, START_MS);
        let party = brokers.place(SIEGE, 5, &[4], START_MS).unwrap();
        assert_eq!(party.key, second.key);
        // Fills past it: 4's copy is full after 6, so 7 in the party goes to the one with room.
        brokers.place(SIEGE, 6, &[4], START_MS).unwrap();
        let overflow = brokers.place(SIEGE, 7, &[4], START_MS).unwrap();
        assert_eq!(overflow.key, first.key);
    }

    #[test]
    fn asking_again_returns_the_same_copy_and_categories_do_not_mix() {
        let mut brokers = Brokers::new();
        let a = brokers.place(SIEGE, 1, &[], START_MS).unwrap();
        assert_eq!(
            brokers.place(SIEGE, 1, &[], START_MS).unwrap(),
            Placement { opened: false, ..a }
        );
        let b = brokers.place(OTHER, 1, &[], START_MS).unwrap();
        assert_ne!(b.key, a.key, "another category is another copy");
        assert_eq!(brokers.copy_of(1), Some(b.key));
        assert!(
            brokers.present(&a.key).is_empty(),
            "moving to another category left the first"
        );
    }

    #[test]
    fn an_empty_copy_closes_after_the_grace_and_its_id_returns_after_the_acknowledgement() {
        let mut brokers = Brokers::new();
        let a = brokers.place(SIEGE, 1, &[], START_MS).unwrap();
        brokers.leave(1, START_MS);
        assert!(brokers.sweep(START_MS + IDLE_GRACE_MS - 1).is_empty());
        assert_eq!(brokers.sweep(START_MS + IDLE_GRACE_MS), vec![(a.key, a.id)]);
        // Not reusable until acknowledged.
        let b = brokers
            .place(SIEGE, 2, &[], START_MS + IDLE_GRACE_MS)
            .unwrap();
        assert_ne!(b.id, a.id);
        assert!(brokers.acknowledge(a.id));
        brokers.leave(2, START_MS + IDLE_GRACE_MS);
        let c = brokers
            .place(OTHER, 3, &[], START_MS + IDLE_GRACE_MS)
            .unwrap();
        assert_eq!(c.id, a.id);
    }

    #[test]
    fn a_copy_nobody_entered_still_idles_out() {
        let mut brokers = Brokers::new();
        let a = brokers.place(SIEGE, 1, &[], START_MS).unwrap();
        brokers.leave(1, START_MS);
        assert_eq!(brokers.sweep(START_MS + IDLE_GRACE_MS).len(), 1);
        assert_eq!(brokers.open_count(), 0);
        assert!(brokers.present(&a.key).is_empty());
    }
}

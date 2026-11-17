//! Ids for the hangar instances a DGS has open.
//!
//! A player's home hangar keeps the id `location::hangar_id(player_index)`. Any other instance (a second location, a
//! larger size, a guest's view of someone else's hangar) takes an id from this pool, above the home range and below the
//! carriages. Each id owns a stride of `HANGAR_ID_STRIDE` for the hangar's content, so two instances never share ids.
//! An id comes back into use only after the unbind of its instance was acknowledged, since a client that still holds
//! the old entities would otherwise see them change class.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The first id of the pool: above the home hangars, which take `FIRST_HANGAR_ID + player_index * stride` (76,000 players).
pub const POOL_FIRST_ID: u64 = 225_000_000_000;
/// The first id after the pool, where the carriages begin.
pub const POOL_END_ID: u64 = 230_000_000_000;
/// The room of one instance, as for a home hangar.
pub const POOL_STRIDE: u64 = 0x1_0000;
/// Instances one DGS keeps open at most; a request past it is refused so the panel can say why.
pub const MAX_OPEN_INSTANCES: usize = 256;

/// Whose hangar, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceKey {
    /// The account that owns the hangar.
    pub owner: u64,
    pub location_id: u32,
}

/// Why an instance was not opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// `MAX_OPEN_INSTANCES` are open.
    TooManyOpen,
    /// Every id of the pool is in use or waiting for its unbind.
    PoolExhausted,
}

/// The ids of the instances open, and of those closed and not yet acknowledged.
#[derive(Debug, Default)]
pub struct HangarSlots {
    open: BTreeMap<InstanceKey, u64>,
    /// Closed, with the unbind sent and no answer yet.
    closing: Vec<u64>,
    /// Free for reuse, oldest first.
    free: VecDeque<u64>,
    /// The next id never used.
    next: Option<u64>,
}

impl HangarSlots {
    pub fn new() -> Self {
        Self::default()
    }

    /// The id of an instance: its own when open, else a new one.
    pub fn open(&mut self, key: InstanceKey) -> Result<u64, Refused> {
        if let Some(id) = self.open.get(&key) {
            return Ok(*id);
        }
        if self.open.len() >= MAX_OPEN_INSTANCES {
            return Err(Refused::TooManyOpen);
        }
        let id = self.take()?;
        self.open.insert(key, id);
        Ok(id)
    }

    /// The id of an instance that is open.
    pub fn id_of(&self, key: &InstanceKey) -> Option<u64> {
        self.open.get(key).copied()
    }

    /// Closes an instance. Its id waits for `acknowledged`. `None` when it was not open.
    pub fn close(&mut self, key: &InstanceKey) -> Option<u64> {
        let id = self.open.remove(key)?;
        self.closing.push(id);
        Some(id)
    }

    /// The unbind of a closed instance reached every client; its id can be used again.
    pub fn acknowledged(&mut self, id: u64) -> bool {
        match self.closing.iter().position(|closing| *closing == id) {
            Some(index) => {
                self.closing.swap_remove(index);
                self.free.push_back(id);
                true
            }
            None => false,
        }
    }

    /// How many instances are open.
    pub fn open_count(&self) -> usize {
        self.open.len()
    }

    /// The instances open, with their ids.
    pub fn instances(&self) -> impl Iterator<Item = (&InstanceKey, u64)> {
        self.open.iter().map(|(key, id)| (key, *id))
    }

    fn take(&mut self) -> Result<u64, Refused> {
        if let Some(id) = self.free.pop_front() {
            return Ok(id);
        }
        let id = self.next.unwrap_or(POOL_FIRST_ID);
        if id + POOL_STRIDE > POOL_END_ID {
            return Err(Refused::PoolExhausted);
        }
        self.next = Some(id + POOL_STRIDE);
        Ok(id)
    }
}

/// How long an instance stays open with nobody in it or on the way, so a quick return finds it as it was.
pub const IDLE_GRACE_MS: u64 = 60_000;

/// Who is in each instance, or riding to it, and since when it has been empty.
#[derive(Debug, Default)]
pub struct Occupancy {
    people: BTreeMap<InstanceKey, BTreeSet<u64>>,
    /// When the last person left; absent while someone is there.
    empty_since: BTreeMap<InstanceKey, u64>,
}

impl Occupancy {
    pub fn new() -> Self {
        Self::default()
    }

    /// A player is inside, or heading to, the instance.
    pub fn enter(&mut self, key: InstanceKey, player: u64) {
        self.people.entry(key).or_default().insert(player);
        self.empty_since.remove(&key);
    }

    /// A player left the instance; it starts idling when the last one goes.
    pub fn leave(&mut self, key: InstanceKey, player: u64, now_ms: u64) {
        let Some(people) = self.people.get_mut(&key) else {
            return;
        };
        people.remove(&player);
        if people.is_empty() {
            self.empty_since.entry(key).or_insert(now_ms);
        }
    }

    /// An instance that was opened with nobody in it yet starts idling too.
    pub fn opened(&mut self, key: InstanceKey, now_ms: u64) {
        if self.people.get(&key).map_or(true, BTreeSet::is_empty) {
            self.empty_since.entry(key).or_insert(now_ms);
        }
    }

    /// Who is there.
    pub fn present(&self, key: &InstanceKey) -> Vec<u64> {
        self.people
            .get(key)
            .map(|people| people.iter().copied().collect())
            .unwrap_or_default()
    }

    /// The instances idle for the whole grace period; they are forgotten here, the caller closes them.
    pub fn due(&mut self, now_ms: u64) -> Vec<InstanceKey> {
        let due: Vec<InstanceKey> = self
            .empty_since
            .iter()
            .filter(|(_, since)| now_ms.saturating_sub(**since) >= IDLE_GRACE_MS)
            .map(|(key, _)| *key)
            .collect();
        for key in &due {
            self.empty_since.remove(key);
            self.people.remove(key);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn key(owner: u64, location_id: u32) -> InstanceKey {
        InstanceKey { owner, location_id }
    }

    #[test]
    fn an_instance_keeps_its_id_while_open() {
        let mut slots = HangarSlots::new();
        let first = slots.open(key(1, 10)).unwrap();
        assert_eq!(first, POOL_FIRST_ID);
        assert_eq!(slots.open(key(1, 10)), Ok(first));
        assert_eq!(slots.id_of(&key(1, 10)), Some(first));
        assert_eq!(slots.open_count(), 1);
    }

    #[test]
    fn instances_of_other_owners_and_places_do_not_overlap() {
        let mut slots = HangarSlots::new();
        let ids: Vec<u64> = [key(1, 10), key(1, 11), key(2, 10), key(2, 11)]
            .into_iter()
            .map(|key| slots.open(key).unwrap())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert!(
            sorted
                .windows(2)
                .all(|pair| pair[1] - pair[0] >= POOL_STRIDE),
            "{ids:?}"
        );
        assert!(ids
            .iter()
            .all(|id| (POOL_FIRST_ID..POOL_END_ID).contains(id)));
    }

    #[test]
    fn an_id_is_reused_only_after_the_unbind_was_acknowledged() {
        let mut slots = HangarSlots::new();
        let id = slots.open(key(1, 10)).unwrap();
        assert_eq!(slots.close(&key(1, 10)), Some(id));
        assert_eq!(slots.close(&key(1, 10)), None);
        let other = slots.open(key(2, 10)).unwrap();
        assert_ne!(other, id, "a closed instance's id is still held by clients");
        assert!(slots.acknowledged(id));
        assert!(!slots.acknowledged(id), "acknowledged once");
        assert_eq!(slots.open(key(3, 10)), Ok(id));
    }

    #[test]
    fn too_many_open_instances_are_refused() {
        let mut slots = HangarSlots::new();
        for owner in 0..MAX_OPEN_INSTANCES as u64 {
            slots.open(key(owner, 1)).unwrap();
        }
        assert_eq!(slots.open(key(u64::MAX, 1)), Err(Refused::TooManyOpen));
        assert_eq!(
            slots.open(key(0, 1)).map(|_| ()),
            Ok(()),
            "an instance already open is not refused"
        );
    }

    #[test]
    fn the_pool_ends_below_the_carriages() {
        let mut slots = HangarSlots::new();
        let room = ((POOL_END_ID - POOL_FIRST_ID) / POOL_STRIDE) as usize;
        assert!(room > MAX_OPEN_INSTANCES);
        slots.next = Some(POOL_END_ID - POOL_STRIDE);
        assert!(slots.open(key(1, 1)).is_ok());
        assert_eq!(slots.open(key(2, 1)), Err(Refused::PoolExhausted));
    }

    #[test]
    fn an_instance_closes_after_the_grace_when_the_last_one_leaves() {
        let mut occupancy = Occupancy::new();
        let home = key(1, 10);
        occupancy.enter(home, 1);
        occupancy.enter(home, 2);
        occupancy.leave(home, 1, 1_000);
        assert!(
            occupancy.due(1_000 + IDLE_GRACE_MS).is_empty(),
            "one is still there"
        );
        occupancy.leave(home, 2, 5_000);
        assert!(occupancy.due(5_000 + IDLE_GRACE_MS - 1).is_empty());
        assert_eq!(occupancy.due(5_000 + IDLE_GRACE_MS), vec![home]);
        assert!(occupancy.due(u64::MAX).is_empty(), "reported once");
    }

    #[test]
    fn coming_back_within_the_grace_keeps_the_instance() {
        let mut occupancy = Occupancy::new();
        let home = key(1, 10);
        occupancy.enter(home, 1);
        occupancy.leave(home, 1, 0);
        occupancy.enter(home, 1);
        assert!(occupancy.due(IDLE_GRACE_MS * 10).is_empty());
        assert_eq!(occupancy.present(&home), vec![1]);
    }

    #[test]
    fn an_instance_opened_empty_idles_from_its_opening() {
        let mut occupancy = Occupancy::new();
        let home = key(1, 10);
        occupancy.opened(home, 100);
        assert_eq!(occupancy.due(100 + IDLE_GRACE_MS), vec![home]);
    }
}

//! The bind batches of one connection: the ids the shard gives them, what each was sent for, and which the client has
//! spawned.
//!
//! The client answers every batch with `BindBatchAsyncSpawnsCompleted` (29), which carries the batch id and nothing
//! else, once every entity of the batch exists, has run its init and is physicalized (not when its meshes have streamed
//! in, and also when an entity failed to spawn). An id must be unique on its connection while its batch is open: the
//! client refuses a second `BindBatchBegin` with an id in use, and treats it as fatal. So ids come from a counter, and
//! each is kept with the key its caller gave it (a join stage, a location), which says when everything sent for a key
//! has spawned.

use std::collections::HashMap;

#[derive(Debug)]
struct Batch {
    key: String,
    completed: bool,
}

#[derive(Debug)]
pub struct Batches {
    /// The id the next batch gets, unless it is still held.
    next: u32,
    /// Every batch sent and not forgotten, by id.
    sent: HashMap<u32, Batch>,
}

/// What a client's 29 did.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    /// The first report of a batch the shard sent.
    New,
    /// A batch already reported, as the 29 of a resent packet is.
    Repeat,
    /// A batch the shard did not send, or has forgotten.
    Unknown,
}

impl Batches {
    pub fn new() -> Self {
        Self { next: 1, sent: HashMap::new() }
    }

    /// A new batch for `key`, with an id no batch of this connection holds. 0 is never given.
    pub fn open(&mut self, key: impl Into<String>) -> u32 {
        while self.next == 0 || self.sent.contains_key(&self.next) {
            self.next = self.next.wrapping_add(1);
        }
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        self.sent.insert(id, Batch { key: key.into(), completed: false });
        id
    }

    /// The client's report that batch `id` has spawned. Reports repeat (a resent packet is read again), and a repeat
    /// changes nothing.
    pub fn complete(&mut self, id: u32) -> Completion {
        match self.sent.get_mut(&id) {
            Some(batch) if batch.completed => Completion::Repeat,
            Some(batch) => {
                batch.completed = true;
                Completion::New
            }
            None => Completion::Unknown,
        }
    }

    pub fn is_completed(&self, id: u32) -> bool {
        self.sent.get(&id).is_some_and(|batch| batch.completed)
    }

    /// What batch `id` was sent for.
    pub fn key(&self, id: u32) -> Option<&str> {
        self.sent.get(&id).map(|batch| batch.key.as_str())
    }

    /// Whether the client has spawned everything sent for `key`: at least one batch, and every batch of it reported.
    /// False for a key nothing was sent for.
    // For the transit stops, which are not written yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn key_completed(&self, key: &str) -> bool {
        let mut batches = self.sent.values().filter(|batch| batch.key == key).peekable();
        batches.peek().is_some() && batches.all(|batch| batch.completed)
    }

    /// Drops what is kept of `key`'s batches, once nobody asks about it any more (its entities unbound). Their ids may
    /// then be given again, so only once the batches have ended.
    // For the transit stops, which are not written yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn forget(&mut self, key: &str) {
        self.sent.retain(|_, batch| batch.key != key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_count_up_from_one_whatever_the_key() {
        let mut batches = Batches::new();
        assert_eq!([batches.open("a"), batches.open("b"), batches.open("a")], [1, 2, 3]);
        assert_eq!(batches.key(2), Some("b"));
    }

    #[test]
    fn a_repeated_report_changes_nothing() {
        let mut batches = Batches::new();
        let id = batches.open("hangar");
        assert_eq!(batches.complete(id), Completion::New);
        assert_eq!(batches.complete(id), Completion::Repeat);
        assert!(batches.is_completed(id));
        assert_eq!(batches.complete(id + 1), Completion::Unknown);
        assert!(!batches.is_completed(id + 1), "a report of a batch never sent is not kept");
        assert_eq!(batches.open("next"), id + 1, "and does not take an id");
    }

    #[test]
    fn a_key_is_completed_once_every_batch_sent_for_it_is() {
        let mut batches = Batches::new();
        assert!(!batches.key_completed("station"), "nothing sent");
        let first = batches.open("station");
        let other = batches.open("hangar");
        let second = batches.open("station");
        batches.complete(first);
        assert!(!batches.key_completed("station"));
        batches.complete(other);
        assert!(!batches.key_completed("station"));
        batches.complete(second);
        assert!(batches.key_completed("station"));
        assert!(batches.key_completed("hangar"));
        let third = batches.open("station");
        assert!(!batches.key_completed("station"), "a later batch for it");
        batches.complete(third);
        assert!(batches.key_completed("station"));
    }

    #[test]
    fn an_id_still_held_is_never_given_again_when_the_counter_wraps() {
        let mut batches = Batches::new();
        let held = batches.open("held");
        batches.next = u32::MAX;
        assert_eq!(batches.open("last"), u32::MAX);
        assert_eq!(batches.open("wrapped"), held + 1, "0 and the held id are skipped");
        batches.forget("held");
        batches.next = held;
        assert_eq!(batches.open("again"), held, "free once forgotten");
        assert_eq!(batches.key(held), Some("again"));
    }
}

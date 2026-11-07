//! Which entities a DGS holds for which player, in memory. A restart forgets this, and the replicant, seeing a new
//! `boot_id`, joins its players again.

use std::collections::HashMap;

struct Held {
    session: u64,
    entities: Vec<u64>,
}

#[derive(Default)]
pub struct Holdings {
    /// The replicant these sessions are of.
    replicant: u64,
    /// By account, as a client that reconnects gets the same entities.
    players: HashMap<u64, Held>,
}

impl Holdings {
    /// A new `replicant` means the old one is gone, and nobody will leave for its players.
    pub fn join(&mut self, replicant: u64, session: u64, account: u64, entities: Vec<u64>) {
        if replicant != self.replicant {
            self.players.clear();
            self.replicant = replicant;
        }
        self.players.insert(account, Held { session, entities });
    }

    /// Frees what `session` holds and says how much. A session that has been taken over, or that this DGS has lost, holds
    /// nothing, so its late leave costs the player who took over nothing.
    pub fn leave(&mut self, replicant: u64, session: u64) -> u32 {
        if replicant != self.replicant {
            return 0;
        }
        let account = self.players.iter().find(|(_, held)| held.session == session).map(|(account, _)| *account);
        let held = account.and_then(|account| self.players.remove(&account));
        held.map_or(0, |held| held.entities.len() as u32)
    }

    pub fn players(&self) -> u32 {
        self.players.len() as u32
    }

    pub fn entities(&self) -> u64 {
        self.players.values().map(|held| held.entities.len() as u64).sum()
    }

    /// Whether any player's entities include `entity`.
    pub fn holds(&self, entity: u64) -> bool {
        self.players.values().any(|held| held.entities.contains(&entity))
    }

    /// The entities held for an account.
    pub fn of(&self, account: u64) -> Option<&[u64]> {
        self.players.get(&account).map(|held| held.entities.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_is_recorded_and_a_leave_frees_it() {
        let mut holdings = Holdings::default();
        holdings.join(1, 10, 1_000_001, vec![5, 6]);
        holdings.join(1, 11, 1_000_002, vec![7]);
        assert_eq!((holdings.players(), holdings.entities()), (2, 3));
        assert_eq!(holdings.of(1_000_001), Some(&[5, 6][..]));
        assert_eq!(holdings.leave(1, 10), 2);
        assert_eq!((holdings.players(), holdings.entities()), (1, 1));
        assert_eq!(holdings.of(1_000_001), None);
    }

    #[test]
    fn leaving_twice_or_for_a_stranger_frees_nothing() {
        let mut holdings = Holdings::default();
        holdings.join(1, 10, 1_000_001, vec![5]);
        assert_eq!(holdings.leave(1, 99), 0);
        assert_eq!(holdings.leave(1, 10), 1);
        assert_eq!(holdings.leave(1, 10), 0);
    }

    #[test]
    fn the_leave_of_a_replaced_connection_keeps_the_new_one() {
        let mut holdings = Holdings::default();
        holdings.join(1, 10, 1_000_001, vec![5]);
        holdings.join(1, 11, 1_000_001, vec![5]);
        assert_eq!((holdings.players(), holdings.leave(1, 10)), (1, 0));
        assert_eq!(holdings.players(), 1);
    }

    #[test]
    fn a_new_replicant_frees_what_the_old_one_left_behind() {
        let mut holdings = Holdings::default();
        holdings.join(1, 10, 1_000_001, vec![5]);
        holdings.join(2, 10, 1_000_002, vec![6]);
        assert_eq!((holdings.players(), holdings.of(1_000_001)), (1, None));
        assert_eq!(holdings.leave(1, 10), 0, "the old replicant has no say");
    }
}

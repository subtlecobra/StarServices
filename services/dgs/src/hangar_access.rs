//! Who may enter whose hangar (see docs/DGS_TRANSIT.md, "Instanced stops"): the owner always, party members and friends
//! when the owner lets them, a stranger never. The owner can deny a person whatever their relation. Guests have no
//! authority over the owner's ships or items.

use std::collections::BTreeSet;

/// How a visitor is related to the owner, as sc-social and sc-squad report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Owner,
    Party,
    Friend,
    Stranger,
}

/// The owner's choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    pub party: bool,
    pub friends: bool,
    /// Accounts refused whatever their relation.
    pub denied: BTreeSet<u64>,
}

impl Default for Access {
    /// Party members and friends may come in; nobody is denied.
    fn default() -> Self {
        Self {
            party: true,
            friends: true,
            denied: BTreeSet::new(),
        }
    }
}

impl Access {
    /// Whether `visitor` may enter.
    pub fn may_enter(&self, visitor: u64, relation: Relation) -> bool {
        match relation {
            Relation::Owner => true,
            _ if self.denied.contains(&visitor) => false,
            Relation::Party => self.party,
            Relation::Friend => self.friends,
            Relation::Stranger => false,
        }
    }

    /// Whether `visitor` may act on the owner's ships and items there: only the owner.
    pub fn may_manage(relation: Relation) -> bool {
        relation == Relation::Owner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST: u64 = 7;

    #[test]
    fn the_owner_always_enters_and_a_stranger_never() {
        let mut access = Access {
            party: false,
            friends: false,
            ..Access::default()
        };
        access.denied.insert(GUEST);
        assert!(access.may_enter(GUEST, Relation::Owner));
        assert!(!Access::default().may_enter(GUEST, Relation::Stranger));
    }

    #[test]
    fn party_and_friends_follow_the_owners_choices() {
        let access = Access {
            party: true,
            friends: false,
            ..Access::default()
        };
        assert!(access.may_enter(GUEST, Relation::Party));
        assert!(!access.may_enter(GUEST, Relation::Friend));
    }

    #[test]
    fn a_denied_person_is_refused_whatever_the_relation() {
        let mut access = Access::default();
        access.denied.insert(GUEST);
        assert!(!access.may_enter(GUEST, Relation::Party));
        assert!(!access.may_enter(GUEST, Relation::Friend));
        assert!(access.may_enter(GUEST + 1, Relation::Friend));
    }

    #[test]
    fn only_the_owner_manages() {
        assert!(Access::may_manage(Relation::Owner));
        assert!(![Relation::Party, Relation::Friend, Relation::Stranger]
            .into_iter()
            .any(Access::may_manage));
    }
}

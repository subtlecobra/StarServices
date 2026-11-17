//! The hangar instances other than a player's home hangar a DGS has open (see docs/HANGARS.md).
//!
//! An instance is keyed by (owner, location). It opens on demand when the first player enters it, is bound to the session of
//! every player inside it or riding to it, and unbound from a session when that player leaves. With nobody in it or on the
//! way for `IDLE_GRACE_MS` it closes: its elevator stop goes, and its id returns to the pool once the unbind is
//! acknowledged (`acknowledge`), as `HangarSlots` has it. Time is passed in (`now_ms`), so nothing here sleeps.
//!
//! The default join path does not use this: the home hangar is bound by the join response and stays as it is.

use std::collections::BTreeMap;

use sc_api::internal::{session_update::Update, EntityBind, EntityBinds, EntityUnbinds};
use tracing::{info, warn};

use crate::{
    hangar_slots::{HangarSlots, InstanceKey, Occupancy, Refused},
    location::HomeHangar,
    outbox::{Outbox, SessionKey},
    transit::elevators::Elevators,
};

/// The prefix of the key of an instance's `EntityBinds` batch, followed by the instance's id.
const BIND_KEY_PREFIX: &str = "hangar:";

/// Why a player could not enter an instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    /// No instance could be opened.
    Refused(Refused),
    /// The location has no hangar in the home locations table.
    UnknownLocation,
}

/// An instance open: its hangar, and who has it bound.
struct Instance {
    hangar: HomeHangar,
    /// The sessions it is bound to, by player.
    sessions: BTreeMap<u64, SessionKey>,
    /// The stop of its elevator, while the owner is in it.
    stop: Option<u64>,
}

impl Instance {
    /// The entities the instance binds: the hangar and its content.
    fn binds(&self) -> Vec<EntityBind> {
        let mut binds = vec![EntityBind::from(self.hangar.bind())];
        binds.extend(
            self.hangar
                .content_binds()
                .into_iter()
                .map(EntityBind::from),
        );
        binds.extend(
            crate::atc::binds(&self.hangar)
                .into_iter()
                .map(EntityBind::from),
        );
        binds
    }
}

pub struct HangarInstances {
    outbox: Outbox,
    planet_zone: bool,
    slots: HangarSlots,
    occupancy: Occupancy,
    open: BTreeMap<InstanceKey, Instance>,
}

impl HangarInstances {
    pub fn new(outbox: Outbox, planet_zone: bool) -> Self {
        Self {
            outbox,
            planet_zone,
            slots: HangarSlots::new(),
            occupancy: Occupancy::new(),
            open: BTreeMap::new(),
        }
    }

    /// The id of an instance that is open.
    pub fn id_of(&self, key: &InstanceKey) -> Option<u64> {
        self.slots.id_of(key)
    }

    pub fn open_count(&self) -> usize {
        self.slots.open_count()
    }

    /// Who is in the instance or on the way to it.
    pub fn present(&self, key: &InstanceKey) -> Vec<u64> {
        self.occupancy.present(key)
    }

    /// The elevator stop of an open instance, once its owner is in it.
    pub fn stop_of(&self, key: &InstanceKey) -> Option<u64> {
        self.open.get(key).and_then(|instance| instance.stop)
    }

    fn open(&mut self, key: InstanceKey, now_ms: u64) -> Result<u64, Denied> {
        if let Some(id) = self.slots.id_of(&key) {
            return Ok(id);
        }
        let id = self.slots.open(key).map_err(Denied::Refused)?;
        let Some(hangar) = HomeHangar::with_id(key.location_id, id, self.planet_zone) else {
            // Nothing was bound with this id: it is free again at once.
            self.slots.close(&key);
            self.slots.acknowledged(id);
            return Err(Denied::UnknownLocation);
        };
        self.occupancy.opened(key, now_ms);
        self.open.insert(
            key,
            Instance {
                hangar,
                sessions: BTreeMap::new(),
                stop: None,
            },
        );
        info!(
            owner = key.owner,
            location = key.location_id,
            id,
            "hangar instance opened"
        );
        Ok(id)
    }

    /// A player is inside the instance, or heading to it: it opens when it is not, and is bound to their session. The
    /// owner's entry also opens the instance's elevator stop. Entering again binds to the new session.
    pub fn enter(
        &mut self,
        key: InstanceKey,
        player: u64,
        session: SessionKey,
        now_ms: u64,
        elevators: &mut Elevators,
    ) -> Result<u64, Denied> {
        let id = self.open(key, now_ms)?;
        self.occupancy.enter(key, player);
        let instance = self.open.get_mut(&key).expect("opened");
        let rebound = instance.sessions.insert(player, session).is_some();
        let binds = instance.binds();
        if rebound {
            self.outbox.send(
                session,
                Update::Unbinds(EntityUnbinds {
                    entity_ids: binds.iter().map(|bind| bind.entity_id).collect(),
                }),
            );
        }
        self.outbox.send(
            session,
            Update::Binds(EntityBinds {
                key: format!("{BIND_KEY_PREFIX}{id}"),
                binds,
            }),
        );
        if player == key.owner {
            instance.stop = elevators.open_instance(key.owner, &instance.hangar, session);
        }
        Ok(id)
    }

    /// A player left the instance (or stopped riding to it): it is unbound from their session. It idles when the last one goes.
    pub fn leave(&mut self, key: InstanceKey, player: u64, now_ms: u64, elevators: &mut Elevators) {
        let Some(instance) = self.open.get_mut(&key) else {
            return;
        };
        if let Some(session) = instance.sessions.remove(&player) {
            self.outbox.send(
                session,
                Update::Unbinds(EntityUnbinds {
                    entity_ids: Self::bound_ids(&instance.binds()),
                }),
            );
        }
        if player == key.owner {
            if let Some(stop) = instance.stop.take() {
                elevators.close_hangar(stop);
            }
        }
        self.occupancy.leave(key, player, now_ms);
    }

    /// A session ended: its player leaves every instance they were in.
    pub fn session_ended(&mut self, session: SessionKey, now_ms: u64, elevators: &mut Elevators) {
        let held: Vec<(InstanceKey, u64)> = self
            .open
            .iter()
            .flat_map(|(key, instance)| {
                instance
                    .sessions
                    .iter()
                    .filter(|(_, held)| **held == session)
                    .map(|(player, _)| (*key, *player))
            })
            .collect();
        for (key, player) in held {
            // The session is gone: nothing is sent to it.
            if let Some(instance) = self.open.get_mut(&key) {
                instance.sessions.remove(&player);
            }
            self.leave(key, player, now_ms, elevators);
        }
    }

    /// Closes the instances idle for the grace period: the elevator stop goes and anyone still bound is unbound. Their ids
    /// are returned, and come back to the pool with `acknowledge`.
    pub fn sweep(&mut self, now_ms: u64, elevators: &mut Elevators) -> Vec<u64> {
        let mut closed = Vec::new();
        for key in self.occupancy.due(now_ms) {
            let Some(instance) = self.open.remove(&key) else {
                continue;
            };
            let ids = Self::bound_ids(&instance.binds());
            for session in instance.sessions.values() {
                self.outbox.send(
                    *session,
                    Update::Unbinds(EntityUnbinds {
                        entity_ids: ids.clone(),
                    }),
                );
            }
            if let Some(stop) = instance.stop {
                elevators.close_hangar(stop);
            }
            match self.slots.close(&key) {
                Some(id) => {
                    info!(
                        owner = key.owner,
                        location = key.location_id,
                        id,
                        "hangar instance closed, its unbind waits for the acknowledgement"
                    );
                    closed.push(id);
                }
                None => warn!(
                    owner = key.owner,
                    location = key.location_id,
                    "an instance idle that had no id"
                ),
            }
        }
        closed
    }

    /// The unbind of a closed instance reached its clients: its id can be used again.
    pub fn acknowledge(&mut self, id: u64) -> bool {
        self.slots.acknowledged(id)
    }

    fn bound_ids(binds: &[EntityBind]) -> Vec<u64> {
        binds.iter().map(|bind| bind.entity_id).collect()
    }
}

#[cfg(test)]
mod tests {
    use sc_api::internal::SessionUpdate;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::{
        hangar_slots::{IDLE_GRACE_MS, POOL_FIRST_ID, POOL_STRIDE},
        outbox::Sent,
    };

    /// Orison's spaceport, whose hangar has a Transit elevator.
    const ORISON: u32 = 1_105_811_199;
    const OWNER: u64 = 300_000_000_001;
    const GUEST: u64 = 300_000_000_002;
    const OWNER_SESSION: SessionKey = (1, 7);
    const GUEST_SESSION: SessionKey = (1, 8);
    const KEY: InstanceKey = InstanceKey {
        owner: OWNER,
        location_id: ORISON,
    };
    const START_MS: u64 = 1_000;

    struct Rig {
        instances: HangarInstances,
        elevators: Elevators,
        owner: UnboundedReceiver<Sent>,
        guest: UnboundedReceiver<Sent>,
    }

    fn rig() -> Rig {
        let outbox = Outbox::default();
        let owner = outbox.subscribe(OWNER_SESSION);
        let guest = outbox.subscribe(GUEST_SESSION);
        let elevators = Elevators::new(outbox.clone())
            .with_cab_statics(crate::transit::wire::CabStatics::Server);
        Rig {
            instances: HangarInstances::new(outbox, false),
            elevators,
            owner,
            guest,
        }
    }

    fn drain(updates: &mut UnboundedReceiver<Sent>) -> Vec<Update> {
        let mut out = Vec::new();
        while let Ok(Ok(SessionUpdate {
            update: Some(update),
        })) = updates.try_recv()
        {
            out.push(update);
        }
        out
    }

    fn bound(updates: &[Update]) -> Vec<u64> {
        updates
            .iter()
            .flat_map(|update| match update {
                Update::Binds(binds) => binds.binds.iter().map(|bind| bind.entity_id).collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn unbound(updates: &[Update]) -> Vec<u64> {
        updates
            .iter()
            .flat_map(|update| match update {
                Update::Unbinds(unbinds) => unbinds.entity_ids.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn the_first_entry_opens_the_instance_and_binds_it_to_the_session() {
        let mut rig = rig();
        let id = rig
            .instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        assert_eq!(id, POOL_FIRST_ID);
        assert_eq!(rig.instances.id_of(&KEY), Some(id));
        let sent = drain(&mut rig.owner);
        assert_eq!(bound(&sent).first(), Some(&id), "the hangar entity first");
        assert!(
            bound(&sent)
                .iter()
                .all(|bound| (id..id + POOL_STRIDE).contains(bound)),
            "inside the instance's stride"
        );
        assert!(
            rig.instances.stop_of(&KEY).is_some(),
            "the owner's entry opens the elevator stop"
        );
        assert_eq!(
            rig.elevators.stop_of(OWNER, ORISON),
            rig.instances.stop_of(&KEY)
        );
    }

    #[test]
    fn a_guest_is_bound_the_same_instance_without_an_elevator_stop_of_their_own() {
        let mut rig = rig();
        let id = rig
            .instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        assert_eq!(
            rig.instances
                .enter(KEY, GUEST, GUEST_SESSION, START_MS, &mut rig.elevators),
            Ok(id)
        );
        assert_eq!(bound(&drain(&mut rig.guest)), bound(&drain(&mut rig.owner)));
        assert_eq!(rig.instances.present(&KEY), [OWNER, GUEST]);
        assert_eq!(rig.elevators.stops_of(GUEST_SESSION), Vec::<u64>::new());
    }

    #[test]
    fn leaving_unbinds_that_session_only() {
        let mut rig = rig();
        rig.instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .enter(KEY, GUEST, GUEST_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        let bound_ids = bound(&drain(&mut rig.guest));
        drain(&mut rig.owner);
        rig.instances
            .leave(KEY, GUEST, START_MS + 10, &mut rig.elevators);
        assert_eq!(unbound(&drain(&mut rig.guest)), bound_ids);
        assert!(drain(&mut rig.owner).is_empty());
        assert!(
            rig.instances.stop_of(&KEY).is_some(),
            "the owner is still in"
        );
    }

    #[test]
    fn the_owner_leaving_closes_the_elevator_stop_not_the_instance() {
        let mut rig = rig();
        rig.instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .leave(KEY, OWNER, START_MS + 10, &mut rig.elevators);
        assert_eq!(rig.instances.stop_of(&KEY), None);
        assert_eq!(rig.elevators.stop_of(OWNER, ORISON), None);
        assert!(rig.instances.id_of(&KEY).is_some(), "it idles first");
    }

    #[test]
    fn an_empty_instance_closes_after_the_grace_period_on_the_injected_clock() {
        let mut rig = rig();
        let id = rig
            .instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .leave(KEY, OWNER, START_MS, &mut rig.elevators);
        assert!(rig
            .instances
            .sweep(START_MS + IDLE_GRACE_MS - 1, &mut rig.elevators)
            .is_empty());
        assert_eq!(
            rig.instances
                .sweep(START_MS + IDLE_GRACE_MS, &mut rig.elevators),
            [id]
        );
        assert_eq!(rig.instances.id_of(&KEY), None);
        assert_eq!(rig.instances.open_count(), 0);
    }

    #[test]
    fn coming_back_within_the_grace_period_keeps_the_instance() {
        let mut rig = rig();
        let id = rig
            .instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .leave(KEY, OWNER, START_MS, &mut rig.elevators);
        assert_eq!(
            rig.instances.enter(
                KEY,
                OWNER,
                OWNER_SESSION,
                START_MS + IDLE_GRACE_MS - 1,
                &mut rig.elevators
            ),
            Ok(id)
        );
        assert!(
            rig.instances
                .sweep(START_MS + 10 * IDLE_GRACE_MS, &mut rig.elevators)
                .is_empty(),
            "somebody is in"
        );
    }

    #[test]
    fn a_closed_instances_id_is_reused_only_after_the_acknowledgement() {
        let mut rig = rig();
        let other = InstanceKey {
            owner: GUEST,
            location_id: ORISON,
        };
        let id = rig
            .instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .leave(KEY, OWNER, START_MS, &mut rig.elevators);
        assert_eq!(
            rig.instances
                .sweep(START_MS + IDLE_GRACE_MS, &mut rig.elevators),
            [id]
        );
        let next = rig
            .instances
            .enter(
                other,
                GUEST,
                GUEST_SESSION,
                START_MS + IDLE_GRACE_MS,
                &mut rig.elevators,
            )
            .unwrap();
        assert_ne!(next, id, "its unbind is not acknowledged yet");
        assert!(rig.instances.acknowledge(id));
        assert!(!rig.instances.acknowledge(id), "once");
        rig.instances
            .leave(other, GUEST, START_MS + IDLE_GRACE_MS, &mut rig.elevators);
        let third = InstanceKey {
            owner: 3,
            location_id: ORISON,
        };
        assert_eq!(
            rig.instances.enter(
                third,
                3,
                GUEST_SESSION,
                START_MS + IDLE_GRACE_MS,
                &mut rig.elevators
            ),
            Ok(id)
        );
    }

    #[test]
    fn closing_unbinds_whoever_is_still_bound() {
        let mut rig = rig();
        let id = rig
            .instances
            .enter(KEY, GUEST, GUEST_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        let bound_ids = bound(&drain(&mut rig.guest));
        // A guest whose session dropped without a leave: the occupancy still lists them until `session_ended`.
        rig.instances.occupancy.leave(KEY, GUEST, START_MS);
        assert_eq!(
            rig.instances
                .sweep(START_MS + IDLE_GRACE_MS, &mut rig.elevators),
            [id]
        );
        assert_eq!(unbound(&drain(&mut rig.guest)), bound_ids);
    }

    #[test]
    fn an_ended_session_leaves_every_instance_without_sending_to_it() {
        let mut rig = rig();
        rig.instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        drain(&mut rig.owner);
        rig.instances
            .session_ended(OWNER_SESSION, START_MS + 5, &mut rig.elevators);
        assert!(drain(&mut rig.owner).is_empty());
        assert!(rig.instances.present(&KEY).is_empty());
        assert_eq!(rig.elevators.stop_of(OWNER, ORISON), None);
    }

    #[test]
    fn an_unknown_location_is_refused_and_leaves_no_id_taken() {
        let mut rig = rig();
        let unknown = InstanceKey {
            owner: OWNER,
            location_id: 1,
        };
        assert_eq!(
            rig.instances
                .enter(unknown, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators),
            Err(Denied::UnknownLocation)
        );
        assert_eq!(rig.instances.open_count(), 0);
        assert_eq!(
            rig.instances
                .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators),
            Ok(POOL_FIRST_ID)
        );
    }

    #[test]
    fn instances_of_one_owner_at_two_locations_are_two_elevator_stops() {
        let mut rig = rig();
        let other_location = crate::location::ORBITUARY;
        let second = InstanceKey {
            owner: OWNER,
            location_id: other_location,
        };
        rig.instances
            .enter(KEY, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        rig.instances
            .enter(second, OWNER, OWNER_SESSION, START_MS, &mut rig.elevators)
            .unwrap();
        assert_ne!(rig.instances.id_of(&KEY), rig.instances.id_of(&second));
        let stops = rig.elevators.stops_of(OWNER_SESSION);
        assert!(
            stops.contains(&rig.instances.stop_of(&KEY).unwrap()),
            "{stops:?}"
        );
    }
}

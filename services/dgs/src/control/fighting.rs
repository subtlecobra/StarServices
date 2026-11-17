//! The part of `Control` that shoots and heals (see `combat`): the trigger of the player's drawn weapon takes rounds from
//! its magazine and replicates the count as `ac.ammoCount`, and a consumable used on a player restores its health and is
//! used up. Hits and the replication of health to the client are not wired yet: see `docs/COMBAT.md`.

use super::*;
use crate::{
    combat::{
        self, take_round, Condition, Refusal, WeaponStats, CHANGE_MODE_PRESS, CONSUME_PRESS,
        FIRE_METHOD_PRIMARY,
    },
    loadout::port_attach_id,
    reload::{ammo_update, MAGAZINE_PORT},
};

impl Control {
    /// One input frame of a player: the fire mode key, the trigger, the shots that are due, and a consumable used.
    pub(super) fn combat_frame(
        &self,
        player: u64,
        replicant: u64,
        session_id: u64,
        frame: &InputFrame,
    ) {
        let session = (replicant, session_id);
        let mut rounds = None;
        let mut class_of_magazine = 0;
        {
            let mut all = self.worn.lock().unwrap();
            let Some(held) = all
                .get_mut(&player)
                .filter(|held| (held.replicant, held.session) == session)
            else {
                return;
            };
            if frame.reload_ammo_capacity > 0 && frame.reload_magazine != 0 {
                held.combat
                    .capacities
                    .set(frame.reload_magazine, frame.reload_ammo_capacity as u32);
            }
            let weapon = held.weapon_in_hand();
            let able = held.combat.health.condition() == Condition::Alive;
            if frame.change_mode == Some(CHANGE_MODE_PRESS) {
                held.combat.trigger.change_mode();
                info!(player = player, mode = ?held.combat.trigger.mode, "the fire mode changed");
            }
            if let (Some(intention), FIRE_METHOD_PRIMARY) = (frame.fire, frame.fire_method) {
                match (weapon, able) {
                    (None, _) => {
                        info!(player = player, refusal = ?Refusal::Holstered, "a shot that was not fired")
                    }
                    (_, false) => {
                        info!(player = player, refusal = ?Refusal::Incapacitated, "a shot that was not fired")
                    }
                    _ => held.combat.trigger.input(intention, frame.time),
                }
            }
            let Some(weapon) = weapon.filter(|_| able) else {
                held.combat.trigger.release();
                return;
            };
            let class = held.worn.get(weapon).map_or(0, |item| item.class_crc);
            let shots = held
                .combat
                .trigger
                .due(frame.time, WeaponStats::of(class).interval());
            for _ in 0..shots {
                let Some(magazine) = held
                    .worn
                    .child_at(weapon, port_attach_id(MAGAZINE_PORT))
                    .map(|item| item.geid)
                else {
                    info!(player = player, weapon, refusal = ?Refusal::NoMagazine, "a shot that was not fired");
                    held.combat.trigger.release();
                    break;
                };
                match take_round(
                    held.ammo.get(magazine),
                    held.combat.capacities.get(magazine),
                ) {
                    Ok(left) => {
                        held.ammo.set(magazine, left);
                        rounds = Some((magazine, left));
                        class_of_magazine =
                            held.worn.get(magazine).map_or(0, |item| item.class_crc);
                    }
                    Err(refusal) => {
                        info!(
                            player = player,
                            weapon,
                            magazine,
                            ?refusal,
                            "a shot that was not fired"
                        );
                        held.combat.trigger.release();
                        break;
                    }
                }
            }
            if let Some((magazine, left)) = rounds {
                info!(player = player, weapon, magazine, left, "a weapon fired");
            }
        }
        // The count the shots left goes to the client as the replicated `ac.ammoCount` of the magazine, for the classes whose
        // variable index is verified (none yet: a wrong index killed the client); the count is kept either way.
        if let Some(update) =
            rounds.and_then(|(magazine, left)| ammo_update(magazine, class_of_magazine, left))
        {
            self.outbox.send(session, Update::Variables(update));
        }
        if let (Some(CONSUME_PRESS), true) = (frame.consume_state, frame.consume_item != 0) {
            self.consume(player, session, frame.consume_item, frame.consume_target);
        }
    }

    /// `player` uses a consumable on `target` (itself when 0): the target's health goes up by what the item's class restores
    /// and the item is used up, unless the target had nothing to restore or the item is not the player's.
    fn consume(&self, player: u64, session: (u64, u64), item: u64, target: u64) {
        let target = if target == 0 { player } else { target };
        let unbinds = {
            let mut all = self.worn.lock().unwrap();
            let Some(user) = all
                .get(&player)
                .filter(|held| (held.replicant, held.session) == session)
            else {
                return;
            };
            let Some(class) = user.worn.get(item).map(|worn| worn.class_crc) else {
                info!(
                    player = player,
                    item, "a consumable the player does not wear"
                );
                return;
            };
            if user.combat.health.condition() != Condition::Alive {
                info!(player = player, item, refusal = ?Refusal::Incapacitated, "a consumable not used");
                return;
            }
            let Some(patient) = all.get_mut(&target) else {
                info!(
                    player = player,
                    item, target, "a consumable on a player this DGS does not hold"
                );
                return;
            };
            let was = patient.combat.health;
            if patient.combat.health.heal(combat::heal_of(class)) == 0 {
                info!(
                    player = player,
                    item, target, "a consumable not used, the target is at full health"
                );
                return;
            }
            info!(
                player = player,
                item,
                target,
                from = was.current,
                to = patient.combat.health.current,
                "health restored"
            );
            let held = all.get_mut(&player).expect("looked up above");
            let mut unbinds = held.worn.detach(item);
            unbinds.reverse();
            unbinds
        };
        self.outbox.send(
            session,
            Update::Unbinds(EntityUnbinds {
                entity_ids: unbinds,
            }),
        );
    }

    /// A hit of the weapon of `weapon_class` on `target`: its health goes down by the weapon's damage. Returns whether it
    /// brought the player to 0. The client's hit reports are not read yet, so nothing calls this.
    pub fn hit(&self, target: u64, weapon_class: u32) -> bool {
        let mut all = self.worn.lock().unwrap();
        let Some(held) = all.get_mut(&target) else {
            return false;
        };
        let down = held
            .combat
            .health
            .damage(WeaponStats::of(weapon_class).damage);
        info!(
            target = target,
            health = held.combat.health.current,
            down,
            "a hit"
        );
        down
    }
}

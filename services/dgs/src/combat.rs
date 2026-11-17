//! Shooting and healing: the rules of the DGS, which has the authority over the entities in a fight. The client sends its
//! trigger as `SFire` (`EInputIntention` start, stop or toggle, on press and on release only), the fire mode key as
//! `SChangeMode` and the use of a consumable as `SConsume`; the DGS decides whether a shot happens (a weapon in the hand, a
//! magazine in it with a round), takes the round from the magazine (the `Ammo` of `reload`, replicated as `ac.ammoCount`),
//! and keeps the health of the player. See `docs/COMBAT.md` for what is verified on the wire and what is not.

use std::collections::HashMap;

/// `EInputIntention` of `SFire`.
pub const INTENTION_START: u32 = 0;
pub const INTENTION_STOP: u32 = 1;
pub const INTENTION_TOGGLE: u32 = 2;
/// `EFireMethod`: the primary fire. The secondary fire (a grenade launcher, a second barrel) is not handled.
pub const FIRE_METHOD_PRIMARY: u32 = 0;
/// `EConsumeActivationState` of a press of the consume key.
pub const CONSUME_PRESS: u32 = 1;
/// The activation mode of `SChangeMode` for a press (CryEngine's `EActionActivationMode`, unconfirmed on the wire).
pub const CHANGE_MODE_PRESS: u32 = 1;

/// The client's clock runs in 100 ns ticks.
pub const TICKS_PER_SECOND: u64 = 10_000_000;
pub const SECONDS_PER_MINUTE: u64 = 60;
/// The most shots a frame can owe: a frame never fires over more than this, whatever its time says.
pub const MAX_SHOTS_PER_FRAME: u32 = 8;
/// The rounds of a burst.
pub const BURST_ROUNDS: u32 = 3;

/// What the DGS assumes of a weapon whose class gamedata gives no figures for (the seed carries no weapon stats yet; these
/// are placeholders to replace with the weapon's data, not game values).
pub const DEFAULT_FIRE_RATE_RPM: u32 = 600;
pub const DEFAULT_SHOT_DAMAGE: u32 = 20;
/// The rounds of a magazine the session has no count for: the capacity the client reported with a reload, else this.
pub const DEFAULT_MAGAZINE_ROUNDS: u32 = 30;
/// The health a player starts with and the most it has (the game's figure is in the health component's params, not known).
pub const DEFAULT_MAX_HEALTH: u32 = 100;
/// What a consumable of a class without figures restores.
pub const DEFAULT_HEAL: u32 = 40;

/// How the trigger turns into shots. The modes a weapon has are not known (`SCItemWeaponComponent` has its selectable
/// fire actions in data), so the key cycles through all three.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FireMode {
    #[default]
    Auto,
    Single,
    Burst,
}

impl FireMode {
    fn next(self) -> Self {
        match self {
            Self::Auto => Self::Single,
            Self::Single => Self::Burst,
            Self::Burst => Self::Auto,
        }
    }
}

/// The figures of a weapon class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeaponStats {
    pub rounds_per_minute: u32,
    pub damage: u32,
}

impl WeaponStats {
    /// The figures of the weapon of `class_crc`. No class has its own yet.
    pub fn of(_class_crc: u32) -> Self {
        Self {
            rounds_per_minute: DEFAULT_FIRE_RATE_RPM,
            damage: DEFAULT_SHOT_DAMAGE,
        }
    }

    /// The ticks between two shots.
    pub fn interval(&self) -> u64 {
        TICKS_PER_SECOND * SECONDS_PER_MINUTE / u64::from(self.rounds_per_minute.max(1))
    }
}

/// What a consumable of `class_crc` restores. No class has its own yet.
pub fn heal_of(_class_crc: u32) -> u32 {
    DEFAULT_HEAL
}

/// Whether the trigger is held and when the next shot is due.
#[derive(Clone, Debug, Default)]
pub struct Trigger {
    held: bool,
    next_shot: u64,
    burst_left: u32,
    pub mode: FireMode,
}

impl Trigger {
    pub fn held(&self) -> bool {
        self.held
    }

    /// The client's `SFire` at its `time`.
    pub fn input(&mut self, intention: u32, time: u64) {
        match intention {
            INTENTION_START => self.press(time),
            INTENTION_STOP => self.release(),
            INTENTION_TOGGLE if self.held => self.release(),
            INTENTION_TOGGLE => self.press(time),
            _ => {}
        }
    }

    fn press(&mut self, time: u64) {
        self.held = true;
        self.next_shot = time;
        self.burst_left = BURST_ROUNDS;
    }

    pub fn release(&mut self) {
        self.held = false;
        self.burst_left = 0;
    }

    /// The fire mode key.
    pub fn change_mode(&mut self) {
        self.mode = self.mode.next();
    }

    /// The shots owed by the frame ending at `time`: the first at the press, the others each `interval` after. A single
    /// shot or a finished burst lets go of the trigger.
    pub fn due(&mut self, time: u64, interval: u64) -> u32 {
        if !self.held || time < self.next_shot {
            return 0;
        }
        let owed = 1
            + ((time - self.next_shot) / interval.max(1)).min(u64::from(MAX_SHOTS_PER_FRAME - 1))
                as u32;
        let shots = match self.mode {
            FireMode::Auto => owed,
            FireMode::Single => 1,
            FireMode::Burst => owed.min(self.burst_left),
        };
        self.next_shot = if owed >= MAX_SHOTS_PER_FRAME {
            time + interval
        } else {
            self.next_shot + u64::from(shots) * interval
        };
        match self.mode {
            FireMode::Auto => {}
            FireMode::Single => self.release(),
            FireMode::Burst => {
                self.burst_left -= shots;
                if self.burst_left == 0 {
                    self.release();
                }
            }
        }
        shots
    }
}

/// Why a shot did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No weapon in the hand.
    Holstered,
    /// The player cannot act.
    Incapacitated,
    /// The weapon has no magazine.
    NoMagazine,
    /// The magazine has no round.
    Empty,
}

/// The rounds of the magazines the session knows the capacity of, for a magazine `Ammo` has no count for (it is full).
#[derive(Clone, Debug, Default)]
pub struct Capacities(HashMap<u64, u32>);

impl Capacities {
    pub fn set(&mut self, magazine: u64, capacity: u32) {
        self.0.insert(magazine, capacity);
    }

    pub fn get(&self, magazine: u64) -> u32 {
        self.0
            .get(&magazine)
            .copied()
            .unwrap_or(DEFAULT_MAGAZINE_ROUNDS)
    }
}

/// A player's state in a fight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Alive,
    /// At 0 health. The game has the player recoverable for `g_removeIncapacitatedPlayerSessionDelay` seconds (120); what
    /// ends it (bleeding out, death) is not modelled.
    Incapacitated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Health {
    pub current: u32,
    pub max: u32,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            current: DEFAULT_MAX_HEALTH,
            max: DEFAULT_MAX_HEALTH,
        }
    }
}

impl Health {
    pub fn condition(&self) -> Condition {
        if self.current == 0 {
            Condition::Incapacitated
        } else {
            Condition::Alive
        }
    }

    /// Takes `amount` of damage; returns whether it brought the player to 0 just now. An incapacitated player takes no more.
    pub fn damage(&mut self, amount: u32) -> bool {
        if self.current == 0 {
            return false;
        }
        self.current = self.current.saturating_sub(amount);
        self.current == 0
    }

    /// Restores up to `amount`; returns what was restored. Healing a player at 0 is a revive; a player at the maximum takes
    /// nothing.
    pub fn heal(&mut self, amount: u32) -> u32 {
        let healed = amount.min(self.max - self.current);
        self.current += healed;
        healed
    }
}

/// What the DGS keeps of a player's fight, for the session.
#[derive(Clone, Debug, Default)]
pub struct Combat {
    pub trigger: Trigger,
    pub health: Health,
    pub capacities: Capacities,
}

/// A shot taken from `magazine`: the rounds left in it.
pub fn take_round(rounds: Option<u32>, capacity: u32) -> Result<u32, Refusal> {
    match rounds.unwrap_or(capacity) {
        0 => Err(Refusal::Empty),
        left => Ok(left - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: u64 = 1_000_000;
    const START: u64 = 5_000_000;

    fn held() -> Trigger {
        let mut trigger = Trigger::default();
        trigger.input(INTENTION_START, START);
        trigger
    }

    #[test]
    fn a_press_fires_at_once_and_a_held_trigger_at_the_rate() {
        let mut trigger = held();
        assert_eq!(trigger.due(START, INTERVAL), 1);
        assert_eq!(
            trigger.due(START + INTERVAL / 2, INTERVAL),
            0,
            "not due yet"
        );
        assert_eq!(
            trigger.due(START + 3 * INTERVAL + 5, INTERVAL),
            3,
            "a long frame owes the shots it spanned"
        );
        trigger.input(INTENTION_STOP, 0);
        assert_eq!(trigger.due(START + 9 * INTERVAL, INTERVAL), 0);
    }

    #[test]
    fn a_frame_never_owes_more_than_the_cap() {
        let mut trigger = held();
        assert_eq!(
            trigger.due(START + 1000 * INTERVAL, INTERVAL),
            MAX_SHOTS_PER_FRAME
        );
        assert_eq!(
            trigger.due(START + 1000 * INTERVAL, INTERVAL),
            0,
            "the clock resumes from the frame"
        );
    }

    #[test]
    fn single_and_burst_let_go_of_the_trigger() {
        let mut single = Trigger {
            mode: FireMode::Single,
            ..Trigger::default()
        };
        single.input(INTENTION_START, START);
        assert_eq!(
            (single.due(START + 5 * INTERVAL, INTERVAL), single.held()),
            (1, false)
        );
        let mut burst = Trigger {
            mode: FireMode::Burst,
            ..Trigger::default()
        };
        burst.input(INTENTION_START, START);
        assert_eq!(burst.due(START + INTERVAL, INTERVAL), 2);
        assert_eq!(
            burst.due(START + 5 * INTERVAL, INTERVAL),
            1,
            "the third round ends the burst"
        );
        assert!(!burst.held());
    }

    #[test]
    fn toggle_flips_and_the_mode_key_cycles() {
        let mut trigger = Trigger::default();
        trigger.input(INTENTION_TOGGLE, START);
        assert!(trigger.held());
        trigger.input(INTENTION_TOGGLE, START);
        assert!(!trigger.held());
        trigger.change_mode();
        assert_eq!(trigger.mode, FireMode::Single);
        trigger.change_mode();
        trigger.change_mode();
        assert_eq!(trigger.mode, FireMode::Auto);
    }

    #[test]
    fn the_interval_is_the_rate() {
        assert_eq!(
            WeaponStats {
                rounds_per_minute: 600,
                damage: 1
            }
            .interval(),
            TICKS_PER_SECOND / 10
        );
    }

    #[test]
    fn a_round_comes_off_the_count_or_the_full_capacity() {
        assert_eq!(take_round(None, 30), Ok(29));
        assert_eq!(take_round(Some(1), 30), Ok(0));
        assert_eq!(take_round(Some(0), 30), Err(Refusal::Empty));
        let mut capacities = Capacities::default();
        assert_eq!(capacities.get(7), DEFAULT_MAGAZINE_ROUNDS);
        capacities.set(7, 12);
        assert_eq!(capacities.get(7), 12);
    }

    #[test]
    fn damage_incapacitates_at_zero_and_healing_revives() {
        let mut health = Health::default();
        assert!(!health.damage(60));
        assert_eq!((health.current, health.condition()), (40, Condition::Alive));
        assert!(health.damage(60), "this one brought it to 0");
        assert_eq!(health.condition(), Condition::Incapacitated);
        assert!(!health.damage(10), "no more damage at 0");
        assert_eq!(health.heal(DEFAULT_HEAL), DEFAULT_HEAL);
        assert_eq!(health.condition(), Condition::Alive);
        assert_eq!(
            health.heal(500),
            DEFAULT_MAX_HEALTH - DEFAULT_HEAL,
            "no more than the maximum"
        );
        assert_eq!(health.heal(1), 0);
    }
}

//! The weapon command buffer (`Weapon::Network::CWeaponCommandBuffer`) of a weapon the player holds. A client that does not own
//! the weapon (it is the DGS's entity) does not start a shot at the trigger: it queues the command and asks the authority with
//! `RmAuthorityRequestCommand`, and the shot begins when the authority multicasts `RmMulticastReceiveFirePress` back (and ends
//! with `RmMulticastReceiveFireRelease`). Nothing answered, so the weapon stood still (see docs/COMBAT.md).
//!
//! Wire (4.10.2): the five remote
//! methods are registered one after the other, so their indices on the weapon entity are consecutive from the
//! request's, which clients send as method 3. A request is `SBufferedCommand` (20 bytes without a payload): tick u32 LE,
//! `ECommand` u8, `ETrigger` u8, then the payload variant. The fire press and release answers carry the tick alone (u32 LE).

use sc_api::internal::EntityMethodCall;

/// The method index of `RmAuthorityRequestCommand` on a weapon entity, as clients send it.
pub const REQUEST_COMMAND_METHOD: u32 = 3;
/// `RmMulticastReceiveFirePress`: the fifth after the request's (request, receive, rejected, fire press, fire release).
pub const FIRE_PRESS_METHOD: u32 = REQUEST_COMMAND_METHOD + 3;
/// `RmMulticastReceiveFireRelease`.
pub const FIRE_RELEASE_METHOD: u32 = REQUEST_COMMAND_METHOD + 4;
/// `RmMulticastReceiveCommand`, the answer that carries any command (the fire press and release ones are shorthands of it).
pub const RECEIVE_COMMAND_METHOD: u32 = REQUEST_COMMAND_METHOD + 1;
/// The no-argument call the client makes on a weapon it has just been given the state of nothing for (draw, streaming in): the
/// fifth method after the request's. Method 8 on a pistol (base 3), 23 on the cutter (base 18), 25 on ship
/// guns (base 20); the official server answers within about 150 ms with the state reply (`state_reply`).
pub const STATE_REQUEST_METHOD: u32 = REQUEST_COMMAND_METHOD + 5;
/// `Weapon::ECommand::kSetAmmoContainerAuthoritative`: tells the weapon which magazine it fires from, and how many rounds it has.
pub const COMMAND_SET_AMMO_CONTAINER: u8 = 0x14;
/// The payload variant kind of `SSetAmmoContainerData` (entity id u64, ammo count u32, container type varint).
const PAYLOAD_KIND_AMMO_CONTAINER: u8 = 1;
/// `EAmmoContainerType::kPrimary`, as a varint of one byte.
pub const AMMO_CONTAINER_PRIMARY: u8 = 0;
/// How many ticks (30 per second) ahead of the request that prompted it the container command is set. A command at or behind
/// the client's tick is refused as out of date, so it needs the round trip and a margin.
pub const CONTAINER_TICK_LEAD: u32 = 10;
/// `Weapon::ECommand::kFire`.
pub const COMMAND_FIRE: u8 = 0;
/// `Weapon::ECommand::kAim`: run by the client locally (client prediction), no variable feeds it; the request still
/// waits in the client's command queue until the authority acknowledges it.
pub const COMMAND_AIM: u8 = 1;
/// `Weapon::ETrigger::kPress` and `kRelease`.
pub const TRIGGER_PRESS: u8 = 0;
pub const TRIGGER_RELEASE: u8 = 1;
/// The bytes of a request before its payload: the tick, the command and the trigger.
const REQUEST_HEAD_LEN: usize = 6;
/// Where the command and the trigger are in a request.
const COMMAND_OFFSET: usize = 4;
const TRIGGER_OFFSET: usize = 5;

/// A request of a weapon command, as far as it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandRequest {
    pub tick: u32,
    pub command: u8,
    pub trigger: u8,
}

impl CommandRequest {
    pub fn parse(args: &[u8]) -> Option<Self> {
        let head = args.get(..REQUEST_HEAD_LEN)?;
        Some(Self {
            tick: u32::from_le_bytes(head[..COMMAND_OFFSET].try_into().ok()?),
            command: head[COMMAND_OFFSET],
            trigger: head[TRIGGER_OFFSET],
        })
    }
}

/// What the authority sends back for `request` of `weapon`: the fire press or release echoed with the request's tick, nothing
/// for the other commands (aim, zeroing, fire mode...), which stay as they were.
pub fn answer(weapon: u64, request: &CommandRequest) -> Option<EntityMethodCall> {
    let method = match (request.command, request.trigger) {
        (COMMAND_FIRE, TRIGGER_PRESS) => FIRE_PRESS_METHOD,
        (COMMAND_FIRE, TRIGGER_RELEASE) => FIRE_RELEASE_METHOD,
        _ => return None,
    };
    Some(EntityMethodCall {
        entity_id: weapon,
        method,
        args: request.tick.to_le_bytes().to_vec(),
    })
}

/// The acknowledgement of an aim request: `RmMulticastReceiveCommand` with the request's bytes as they came (tick, command,
/// trigger, payload variant), which the client matches by tick, command and trigger. `None` for any other command.
/// Not verified on a client.
pub fn aim_answer(weapon: u64, request: &CommandRequest, args: &[u8]) -> Option<EntityMethodCall> {
    (request.command == COMMAND_AIM).then(|| EntityMethodCall {
        entity_id: weapon,
        method: RECEIVE_COMMAND_METHOD,
        args: args.to_vec(),
    })
}

/// `RmMulticastReceiveCommand` carrying `kSetAmmoContainerAuthoritative` (pressed) at `tick`: the weapon fires from `magazine`,
/// which then holds `rounds`. The client's weapon looks its ammo up in that container; without it a shot finds none.
pub fn set_ammo_container(weapon: u64, tick: u32, magazine: u64, rounds: u32) -> EntityMethodCall {
    let mut args = Vec::with_capacity(20);
    args.extend_from_slice(&tick.to_le_bytes());
    args.extend_from_slice(&[
        COMMAND_SET_AMMO_CONTAINER,
        TRIGGER_PRESS,
        PAYLOAD_KIND_AMMO_CONTAINER,
    ]);
    args.extend_from_slice(&magazine.to_le_bytes());
    args.extend_from_slice(&rounds.to_le_bytes());
    args.push(AMMO_CONTAINER_PRIMARY);
    EntityMethodCall {
        entity_id: weapon,
        method: RECEIVE_COMMAND_METHOD,
        args,
    }
}

/// The state reply's methods on the weapon: `RmMulticastReceiveState` (no arguments, the end marker), the sight state, the
/// high-frequency state and the full state.
const STATE_END_METHOD: u32 = REQUEST_COMMAND_METHOD + 6;
const STATE_SIGHT_METHOD: u32 = REQUEST_COMMAND_METHOD + 7;
const STATE_HIGH_FREQUENCY_METHOD: u32 = REQUEST_COMMAND_METHOD + 9;
const STATE_FULL_METHOD: u32 = REQUEST_COMMAND_METHOD + 10;
/// `SAmmoState` on the magazine entity (the ammo component's third method: method 2 of the pistol's `_mag`).
pub const AMMO_STATE_METHOD: u32 = 2;
/// How many ticks (30 per second) after the client's last command the state is stamped: the official reply is stamped
/// 5 after the draw's commands, and a tick must not be behind the client's.
pub const STATE_TICK_LEAD: u32 = 5;
/// `SC_DGS_WEAPON_STATE_REPLY=off|0|false` turns the state reply off (it is on by default: the bytes and their order are the official ones).
pub const WEAPON_STATE_REPLY_SETTING: &str = "SC_DGS_WEAPON_STATE_REPLY";
/// `SWeaponState`'s first byte: bit 0 is set in every official reply (it is 1, 5, 0x15, 0x31, 0x65, 0xd5 or 0xfd);
/// the other bits are not identified, so the minimum is sent.
pub const WEAPON_STATE_FLAGS: u8 = 0x01;
/// The sight state: two f32 of -1.0 and a u32 0 (38 of the 38 pistol replies).
const SIGHT_UNSET: f32 = -1.0;
/// The high-frequency state of a handheld: fire mode 0, flag 0, energy multiplier and a second multiplier 1.0, two zero floats
/// (the second is the temperature), the `modifierGimbalMode` varint 0x15 of handhelds and two zero u32 (most replies; the
/// others carry fire mode 0xff, a third 1.0 or a trailing value of an energy weapon).
const FIRE_MODE_FIRST: u8 = 0;
const MODIFIER_GIMBAL_MODE_HANDHELD: u8 = 0x15;
/// `SAmmoState` hides the count: `count = encrypted ^ key`. The key of the official replies looks arbitrary (it is close to the
/// encrypted value), so one is derived from the tick.
const AMMO_KEY_MASK: u32 = 0x3a2b_c373;

/// The ticks per second of the client's clock (a client's commands come 22 ticks in 0.745 s).
const TICKS_PER_SECOND: f64 = 30.0;

/// The client's tick now, from its last command's tick and the time since it arrived.
pub fn estimate_tick(last: u32, elapsed: std::time::Duration) -> u32 {
    last.wrapping_add((elapsed.as_secs_f64() * TICKS_PER_SECOND) as u32)
}

/// Whether the state reply is on (`SC_DGS_WEAPON_STATE_REPLY=off|0|false` turns it off).
pub fn state_reply_enabled() -> bool {
    !std::env::var(WEAPON_STATE_REPLY_SETTING).is_ok_and(|value| ["off", "0", "false"].contains(&value.to_ascii_lowercase().as_str()))
}

/// `SC_DGS_WEAPON_ECHO_COMMANDS=on|1|true` acknowledges the commands the DGS does not know with their own bytes
/// (`RmMulticastReceiveCommand`, as the aim is). Not verified: off by default.
pub const WEAPON_ECHO_COMMANDS_SETTING: &str = "SC_DGS_WEAPON_ECHO_COMMANDS";

pub fn echo_unknown_commands() -> bool {
    std::env::var(WEAPON_ECHO_COMMANDS_SETTING).is_ok_and(|value| ["on", "1", "true"].contains(&value.to_ascii_lowercase().as_str()))
}

/// Whether the DGS answers `request` by itself (fire press and release, aim, the ammo container): any other command (such as
/// 28, sent while the player holds a button) gets no answer.
pub fn is_known_command(request: &CommandRequest) -> bool {
    [COMMAND_FIRE, COMMAND_AIM, COMMAND_SET_AMMO_CONTAINER].contains(&request.command)
}

/// The acknowledgement of any command: `RmMulticastReceiveCommand` with the request's bytes as they came.
pub fn echo_answer(weapon: u64, args: &[u8]) -> Option<EntityMethodCall> {
    Some(EntityMethodCall { entity_id: weapon, method: RECEIVE_COMMAND_METHOD, args: args.to_vec() })
}

fn put_f32(out: &mut Vec<u8>, value: f32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// `SAmmoState`: tick, the magazine's capacity, its count hidden with `key` (`encrypted = count ^ key`) and the key.
pub fn ammo_state_args(tick: u32, capacity: u32, rounds: u32, key: u32) -> Vec<u8> {
    let mut args = tick.to_le_bytes().to_vec();
    args.extend_from_slice(&capacity.to_le_bytes());
    args.extend_from_slice(&(rounds ^ key).to_le_bytes());
    args.extend_from_slice(&key.to_le_bytes());
    args
}

/// The state reply of a handheld weapon to `RmAuthorityRequestState`, in the official server's order (pistol entity
/// 927482317757: methods 12, 13, 10, then the magazine entity's 2, then 9, all stamped with one tick): the high-frequency state
/// (+9, 31 B), the full state (+10, 54 B) with the loaded magazine's entity id, the sight state (+7, 16 B), the magazine's
/// `SAmmoState` (method 2 on its own entity, 16 B: capacity, count and key) and the end marker (+6, no arguments).
///
/// The full state is: tick, flags u8, f32 1.0, u32 0, u8 0, f32 0, f32 0, u64 target 0 (offset 22), then three u64 ammo container
/// ids from offset 30: primary (the loaded magazine, verified on four pistols), medical and salvage (zero). The target can be counted
/// as the first of four u64, hence "the magazine in the second slot": the same bytes.
/// Not verified: the meaning of each field and of the flags' other bits; only the bytes and the order are the official ones.
pub fn state_reply(weapon: u64, tick: u32, magazine: u64, rounds: u32, capacity: u32, flags: u8) -> Vec<EntityMethodCall> {
    let head = tick.to_le_bytes().to_vec();
    let mut high = head.clone();
    high.extend_from_slice(&[FIRE_MODE_FIRST, 0]);
    put_f32(&mut high, 1.0);
    put_f32(&mut high, 1.0);
    put_f32(&mut high, 0.0);
    put_f32(&mut high, 0.0);
    high.push(MODIFIER_GIMBAL_MODE_HANDHELD);
    high.extend_from_slice(&[0; 8]);
    let mut full = head.clone();
    full.push(flags);
    put_f32(&mut full, 1.0);
    full.extend_from_slice(&0u32.to_le_bytes());
    full.push(0);
    put_f32(&mut full, 0.0);
    put_f32(&mut full, 0.0);
    full.extend_from_slice(&0u64.to_le_bytes());
    full.extend_from_slice(&magazine.to_le_bytes());
    full.extend_from_slice(&[0; 16]);
    let mut sight = head.clone();
    put_f32(&mut sight, SIGHT_UNSET);
    put_f32(&mut sight, SIGHT_UNSET);
    sight.extend_from_slice(&0u32.to_le_bytes());
    let ammo = ammo_state_args(tick, capacity, rounds, tick ^ AMMO_KEY_MASK);
    let call = |entity_id, method, args| EntityMethodCall { entity_id, method, args };
    vec![
        call(weapon, STATE_HIGH_FREQUENCY_METHOD, high),
        call(weapon, STATE_FULL_METHOD, full),
        call(weapon, STATE_SIGHT_METHOD, sight),
        call(magazine, AMMO_STATE_METHOD, ammo),
        call(weapon, STATE_END_METHOD, Vec::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEAPON: u64 = 400_000_000_148;
    /// Requests of a client: an aim press, a fire press and a fire release.
    const AIM_PRESS: &str = "0140a08801010000000000000000000000000000";
    const FIRE_PRESS: &str = "0140a08800000000000000000000000000000000";
    const FIRE_RELEASE: &str = "0140a08800010000000000000000000000000000";

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_request_reads_the_tick_the_command_and_the_trigger() {
        let request = CommandRequest::parse(&bytes(FIRE_PRESS)).unwrap();
        assert_eq!(
            request,
            CommandRequest {
                tick: 0x88a0_4001,
                command: COMMAND_FIRE,
                trigger: TRIGGER_PRESS
            }
        );
        assert_eq!(
            CommandRequest::parse(&bytes(FIRE_RELEASE)).unwrap().trigger,
            TRIGGER_RELEASE
        );
        assert_eq!(CommandRequest::parse(&[1, 2, 3]), None);
    }

    #[test]
    fn the_fire_press_and_release_are_answered_with_the_requests_tick() {
        let press = answer(WEAPON, &CommandRequest::parse(&bytes(FIRE_PRESS)).unwrap()).unwrap();
        assert_eq!(
            (press.entity_id, press.method, press.args),
            (WEAPON, 6, vec![0x01, 0x40, 0xa0, 0x88])
        );
        let release = answer(
            WEAPON,
            &CommandRequest::parse(&bytes(FIRE_RELEASE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (release.method, release.args),
            (7, vec![0x01, 0x40, 0xa0, 0x88])
        );
    }

    #[test]
    fn the_ammo_container_command_is_a_receive_command_with_the_container_payload() {
        let call = set_ammo_container(WEAPON, 0x88a0_400c, 400_000_000_214, 30);
        assert_eq!((call.entity_id, call.method), (WEAPON, 4));
        let mut expected = vec![0x0c, 0x40, 0xa0, 0x88, 0x14, 0x00, 0x01];
        expected.extend_from_slice(&400_000_000_214u64.to_le_bytes());
        expected.extend_from_slice(&[30, 0, 0, 0, 0]);
        assert_eq!(call.args, expected);
        assert_eq!(call.args.len(), 20);
    }

    /// The exchange of a klwe_pistol_energy_01: every aim answer of the official server is byte-for-byte the request it answers,
    /// and a fire press and release are answered with the request's tick alone.
    const PISTOL_AIM_PRESS: &str = "23410c0001000000000000000000000000000000";
    const PISTOL_AIM_RELEASE: &str = "35410c0001010000000000000000000000000000";
    const PISTOL_FIRE_PRESS: &str = "89410c0000000000000000000000000000000000";
    const PISTOL_FIRE_RELEASE: &str = "8b410c0000010000000000000000000000000000";
    const OFFICIAL_AMMO_CONTAINER: &str = "6b410c00140001352674a3d70000001e00000000";
    const PISTOL: u64 = 926_160_266_802;
    const PISTOL_MAGAZINE: u64 = 926_160_266_805;

    #[test]
    fn the_official_answers_to_the_pistol_requests_are_ours() {
        for official in [PISTOL_AIM_PRESS, PISTOL_AIM_RELEASE] {
            let args = bytes(official);
            let request = CommandRequest::parse(&args).unwrap();
            let call = aim_answer(PISTOL, &request, &args).unwrap();
            assert_eq!((call.method, call.args), (RECEIVE_COMMAND_METHOD, args));
        }
        let press = CommandRequest::parse(&bytes(PISTOL_FIRE_PRESS)).unwrap();
        let call = answer(PISTOL, &press).unwrap();
        assert_eq!((call.method, call.args), (FIRE_PRESS_METHOD, bytes("89410c00")));
        let release = CommandRequest::parse(&bytes(PISTOL_FIRE_RELEASE)).unwrap();
        let call = answer(PISTOL, &release).unwrap();
        assert_eq!((call.method, call.args), (FIRE_RELEASE_METHOD, bytes("8b410c00")));
    }

    #[test]
    fn the_ammo_container_command_is_the_official_bytes() {
        let call = set_ammo_container(PISTOL, 0x000c_416b, PISTOL_MAGAZINE, 30);
        assert_eq!(call.args, bytes(OFFICIAL_AMMO_CONTAINER));
    }

    #[test]
    fn the_state_reply_is_the_pistols_in_the_official_order() {
        // behr_pistol_ballistic_01: the reply is stamped with
        // tick 0x0004aa60: methods 12, 13, 10 on the weapon, 2 on its magazine (the `_mag` child), then 9.
        // Everything but the ids and the flags byte (the official one is 5) is built from fields.
        let weapon = 927_482_317_757;
        let magazine = 927_482_317_854;
        let calls = state_reply(weapon, 0x0004_aa60, magazine, 20, 20, 5);
        let official = [
            (weapon, 12, "60aa040000000000803f0000803f0000000000000000150000000000000000"),
            (weapon, 13, "60aa0400050000803f0000000000000000000000000000000000000000001e0c41f2d700000000000000000000000000000000000000"),
            (weapon, 10, "60aa0400000080bf000080bf00000000"),
        ];
        assert_eq!(calls.len(), official.len() + 2);
        for (call, (entity, method, hex)) in calls.iter().zip(official) {
            assert_eq!((call.entity_id, call.method, call.args.clone()), (entity, method, bytes(hex)));
        }
        // The magazine's `SAmmoState`: the official one (tick, capacity 20, 0x3a2bc367 ^ key 0x3a2bc373 = 20 rounds) is rebuilt with the
        // official key; ours derives a key from the tick and hides the same count.
        assert_eq!(
            ammo_state_args(0x0004_aa60, 20, 20, 0x3a2b_c373),
            bytes("60aa04001400000067c32b3a73c32b3a")
        );
        let ammo = &calls[3];
        assert_eq!((ammo.entity_id, ammo.method, ammo.args.len()), (magazine, AMMO_STATE_METHOD, 16));
        let word = |at: usize| u32::from_le_bytes(ammo.args[at..at + 4].try_into().unwrap());
        assert_eq!((word(4), word(8) ^ word(12)), (20, 20), "capacity and the count the key hides");
        // The end marker comes last, with no arguments.
        let end = &calls[4];
        assert_eq!((end.entity_id, end.method, end.args.len()), (weapon, 9, 0));
        // The sent flags byte is the minimum, with every official reply's bit 0.
        let sent = state_reply(weapon, 0x0004_aa60, magazine, 20, 20, WEAPON_STATE_FLAGS);
        assert_eq!(sent[1].args[4], 1);
        assert_eq!(STATE_REQUEST_METHOD, 8);
    }

    #[test]
    fn the_tick_is_estimated_from_the_last_command_and_the_time_since() {
        // Ticks 2304061442 and 2304061464 come 0.745 s apart (22 ticks).
        assert_eq!(estimate_tick(2_304_061_442, std::time::Duration::from_millis(745)), 2_304_061_442 + 22);
        assert_eq!(estimate_tick(u32::MAX, std::time::Duration::from_secs(1)), 29);
    }

    #[test]
    fn only_fire_aim_and_the_container_are_known_commands() {
        let command = |command| CommandRequest { tick: 1, command, trigger: TRIGGER_PRESS };
        assert!([COMMAND_FIRE, COMMAND_AIM, COMMAND_SET_AMMO_CONTAINER].iter().all(|c| is_known_command(&command(*c))));
        assert!(!is_known_command(&command(28)));
        let call = echo_answer(WEAPON, &[1, 2]).unwrap();
        assert_eq!((call.method, call.args), (RECEIVE_COMMAND_METHOD, vec![1, 2]));
    }

    #[test]
    fn an_aim_request_is_acknowledged_with_its_own_bytes() {
        let args = bytes(AIM_PRESS);
        let request = CommandRequest::parse(&args).unwrap();
        assert_eq!(answer(WEAPON, &request), None);
        let call = aim_answer(WEAPON, &request, &args).unwrap();
        assert_eq!((call.entity_id, call.method, call.args), (WEAPON, 4, args));
        let fire = CommandRequest::parse(&bytes(FIRE_PRESS)).unwrap();
        assert_eq!(aim_answer(WEAPON, &fire, &bytes(FIRE_PRESS)), None);
    }
}

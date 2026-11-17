# Combat: shooting and healing

The DGS has the authority over the entities in a fight. The client sends its intentions as actions of the input frame
(method 7 of the player); the replicant (`services/pu-frontend/src/input.rs`) decodes them into `InputFrame` fields, the DGS
(`services/dgs/src/combat.rs`, glue in `services/dgs/src/control/fighting.rs`) decides.

## Actions read

| Action | Type | Data | Frame field | Status |
| --- | --- | --- | --- | --- |
| `SFire` | `0x45` | `EInputIntention` byte (0 start, 1 stop, 2 toggle), `EFireMethod` varint (0 primary, 1 secondary) | `fire`, `fire_method` | Layout from `VisitPayload`; `45 01 00` is stop, primary. Sent on press and release only |
| `SChangeMode` | `0x31` | activation mode, one byte | `change_mode` | The value 1 for a press is CryEngine's `EActionActivationMode` and not verified |
| `SConsume` | `0x7b` | consumable `EntityId`, target `EntityId`, `EConsumeActivationState` varint (1 press, 2 release), a flag byte: 18 bytes | `consume_item`, `consume_target`, `consume_state` | Field list from `VisitPayload`; the 18 bytes are not verified on the wire (a wrong length fails the chain and the frame falls back to the scan) |

`SEatAndDrink` (`0x7c`, food) has the same layout; it is only skipped over, it heals nothing. The syringe stab on another
player (`OnActionMeleeAttackSyringeStab`, an `SBodyDrag` first) is not read.

## Shooting (DGS)

- A start of the primary fire needs a drawn weapon (`draw`) and a player with health above 0; otherwise it is refused (logged).
- The trigger (`combat::Trigger`) fires at the press, then every interval of the weapon's rate while held, from the client's
  frame time (100 ns ticks); a frame owes at most 8 shots. Fire modes: auto, single, burst (3), cycled by `SChangeMode`.
  Which modes a weapon really has is data of the weapon component: all three are offered.
- Each shot takes one round from the magazine in the weapon's `magazine_attach` port. The count is `Ammo` (`reload`) or,
  while the magazine is full, the capacity the client reported with a reload, else `DEFAULT_MAGAZINE_ROUNDS` (30). The new
  count is kept server-side. It is NOT sent to the client: the variable index of `ac.ammoCount` is not verified for any
  magazine class (`reload::VERIFIED_AMMO_COUNT_INDICES` is empty, `reload::ammo_update` returns `None`). The former index 55
  crashed the client on the first shot: `Fatal Error Thrown: CSerializedComponent::ReceiveDirtyValuesFromNetwork:
  Serialized variable deserializing stream size mismatch varName="CAlignmentSlot::m_lockedByEntityId.0", streamSize=64,
  replicaSize=32` (index 55 of the bound magazine is another component's variable). The HUD count therefore stays at the
  client's own value until a class's index is verified (add it to the list then).
- An empty or missing magazine refuses the shot and lets go of the trigger (the reload then continues from that count).
- Rate and damage per weapon class are placeholders (`combat::WeaponStats`: 600 rpm, 20): the seed and gamedata carry no
  weapon figures yet.

### Why a drawn weapon did not fire (2026-11-11)

The server counted the shots (`a weapon fired`) but the client showed no shot or flash. A drawn weapon asks the arbitrated token 0
(flags `0xce`) of its entity at each draw; that is the `ControlToken` of its `ItemControlComponent`, which `SetControlContext`
requests for the holder. The DGS left it unanswered, so the item never became controlled and the fire input never started the
action. The DGS now grants an arbitrated token of an item the player wears (`token::decide`). Not verified on a client: if
shots still do not show, the next suspects are the weapon command buffer (`CWeaponCommandBuffer`, ECommand kFire) and its
authority path.

Result (usa-030): the grant reached the client and the weapon still did not shoot. The next gate: the
client fires from `m_ammoContainers[type]`, which only the authority's weapon command 0x14 (`kSetAmmoContainerAuthoritative`,
multicast as `CWeaponCommandBuffer::RmMulticastReceiveCommand`) sets when a magazine attaches. The DGS sends no such call yet;
it needs the RM index on the weapon entity and the `SBufferedCommand` layout.

### The weapon command buffer

The client does request the shot: each trigger edge arrives as a call on the weapon entity, method 3, 20 bytes
(`RmAuthorityRequestCommand`: tick u32, `ECommand` u8, `ETrigger` u8, payload variant; fire press `...00 00`, release `...00 01`,
aim `...01 xx`). The five remote methods of `CWeaponCommandBuffer` are registered in a row (request, receive command, rejected,
fire press, fire release), so on the weapon entity they are 3..7. `weapon_command.rs` answers a fire press with method 6 and a
release with method 7, each carrying the request's tick (u32), to the caller; other commands get no answer. Only for the weapon the
player has drawn. The fire press/release answers are
acknowledgements only: a client in prediction mode (2) queues its own command and runs it at its tick (`UpdateCommands`), the multicast
just marks it acknowledged (the press/release handlers are shorthands of `RmMulticastReceiveCommand`).
The shot itself is gated by the ammo: `HasEnoughAmmo` reads the weapon's `m_ammoContainers[type]`, which only weapon command 0x14
(`kSetAmmoContainerAuthoritative`) sets. The DGS then sends it (`weapon_command::set_ammo_container`, method 4, 20 bytes: tick u32, 0x14,
press, payload kind 1, magazine id u64, rounds u32, container type varint 0) once for each weapon and magazine, answering the first
request after a draw or a reload, at the request's tick plus 10 (the client refuses a command at or behind its own tick). Its
rounds are the session's count for the magazine, else the reported capacity, else 30; the client sets the magazine's ammo to it.
The first shot after a draw or reload may still go before the command is executed.

## Health and healing (DGS)

- `combat::Health` per player held: 100 of 100 at join. Damage down to 0 means `Incapacitated` (the game has
  `g_removeIncapacitatedPlayerSessionDelay` = 120 s for such a player; bleeding out or death is not modelled).
  An incapacitated player cannot fire or use items on itself.
- `SConsume` press of an item the player wears restores `heal_of(class)` (placeholder 40) to the target (the player itself
  when the target is 0 or itself, another held player otherwise, which revives it from 0), and the item is unbound from the
  client (used up in the session). Refused when the target is at full health. Range checks are not done.
- Not persisted: the entity graph still has the item; a rejoin gives it back.

## Not done / unconfirmed

- Hits: the client's hit reports are not read (no message of them is known). `Control::hit(target, weapon_class)` applies a
  weapon's damage to a held player and is called by nothing yet. Hit resolution against other entities (NPCs, ships) waits for
  that and for the DGS's physics queries.
- Health on the wire: the player's health is not a health component but the actor status stats, one replicated
  variable per stat named `SCActorStateDataSynced::SStatus::<stat>` (`m_healthPool` key 0x68f77c2e, head, torso, limbs,
  `m_gasSaturationO2` for oxygen), struct a stat, type id 0x5add6b8f. Its value
  layout is 26 bytes with no recent hits (the maximum is not on the wire but comes from the
  client's config, 100 for the health pool); it is not verified against a real stream, so the DGS sends nothing: the client
  creates every stat at its full default and decays it itself.
- The consumed item's wire form (unbinding the entity) and the animation timing (the effect is applied on the press).
- Secondary fire, melee, grenades and tractor beams.

## Weapon calls of the official service

How the official service answers weapon calls (`weapon_command.rs` tests use these bytes):

- Aim (`ECommand` 1, press and release): the official server answers every aim request of the player's pistol with `RmMulticastReceiveCommand`
  (request base + 1) carrying the request's bytes unchanged, 70 to 160 ms later. Fire press and release: the request's tick as
  `RmMulticastReceiveFirePress` / `FireRelease` (+3 / +4). The ammo container command 0x14 is 20 bytes, sent at about the current tick
  (+2), as `set_ammo_container` builds it.
- The client's no-argument call at base + 5 (`RmAuthorityRequestState`: pistol 8, cutter 23, ship guns 25) asks for the weapon's state.
  The official server answers with one batch stamped with one tick (5 after the draw's commands), in this order
  +9 high-frequency state (31 B), +10 full state (54 B), +7 sight
  state (16 B), the magazine entity's own `SAmmoState` (method 2 on `_mag`, 16 B: tick, capacity u32, count ^ key u32, key u32; e.g. 20
  rounds with key 0x3a2bc373) and +6, the end marker, with no arguments. Byte map after the tick: **sight** = f32 -1, f32 -1, u32 0;
  **full** = flags u8 (offset 4; bit 0 always set), f32 1.0, u32 0, u8 0,
  f32 0, f32 0, u64 target 0 (offset 22), then three u64 ammo container ids from offset 30: primary (the loaded magazine), medical, salvage; **high-frequency**
  = fire mode u8 (0 on the pistol; 0xff at times), flag u8, f32 1.0, f32 1.0, f32 0, f32 0 (temperature), varint 0x15
  (`modifierGimbalMode` of handhelds; here it sits before the two u32), u32 0, u32 0.
  `weapon_command::state_reply` builds it (the test rebuilds the reply bytes, flags aside) and `control.rs::weapon_state_reply` sends
  it for the drawn handheld (base 3 only), stamped last command tick + 5 and with flags 0x01 (the other bits are unidentified).
  `SC_DGS_WEAPON_STATE_REPLY=off` turns it off. Cutters and ship guns are not wired (their bases are assumed).
- At a draw the official server sends `ReceiveCommand` 0x0e (float), 0x0f trigger 0 (u8 0) and 4 trigger 1 with the same tick; at a holster
  4 trigger 0 and 0x0f trigger 1 (u8 0xff). Their meaning (enum names) is not known.
- The command-buffer base of a class other than the handhelds (cutter 18, ship guns 20) is assumed from payload sizes only; requests on those classes are not decoded.

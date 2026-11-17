# Testing what only the real client decides

The server side of the player's actions is covered without the game by `sc-bot` (docs/BOT.md): the bot sends the same
action frames as the client and checks the variables the replicant returns. What stays open is what the **client** does
with them: it closes its mobiGlas when the replicated actor state disagrees, it holsters a weapon the server did not
confirm. Those need the real client. Three layers cover them, cheapest first.

## 1. Protocol scenarios (no game; in `sc-bot`)

`mobiglas` and `weapon-pose` send `SMobiGlas` (0x56), `SUnholster` (0x76) and `SHolster` (0x4d) frames and assert on the
variables 411 (`m_additiveState`), 471 (`SMobiGlas::m_state`) and 164 (`m_poseState`) with the frame time. They catch every
server-side regression of the fix. They cannot say that the client accepts the values.

## 2. The real client, driven and read (the bot client, `scbot` account)

What the client reports, with no human watching:

- **Verdicts from `Game.log`**: `Mobiglas state mismatch` means the client closed its mobiGlas against the server; `Exiting
  unholstering of '<item>' successfully` / `unsuccessfully ... Currently holding` is the draw's result. A scenario passes
  when the first is absent and the second says successfully for the interval it waits.
- **Console commands** over the pipe bridge (`127.0.0.1:47310`, `docs/BOT.md`): `disconnect` / `connect <host> <port>`
  join a chosen shard, so a test picks the shard with the build under test.

## 3. Input the pipe cannot reach (mobiGlas key, drawing a weapon)

The test pipe has no adapter for the keyboard. Options, by how safe they are:

1. **A virtual keyboard through `/dev/uinput`** (writable by this user; `ydotool` and `xdotool` exist). Keys go to the
   focused window, so the bot's window has to be focused and nothing else may take the keys: only on a session nobody is
   using, never beside the player's own game. Keys: the mobiGlas key (F1 by default) and the weapon slots; the binding is
   read from the client's `actionmaps.xml` (the profile of the `scbot` state directory), not assumed.
2. **A Windows-side helper in the client's own Wine prefix** (as the pipe helper is: `bridge/winpy`) that calls `SendInput`
   or posts messages to the game window. It does not need the desktop focus if the helper and the game share one Wine
   desktop, which has to be tried.
3. **Replaying a saved frame**: the protocol bot can send the exact frame a real draw produced (recorded by the
   replicant's `unholster action` log line) while a real client is connected as the same player. It tests the server, not
   the client, so it belongs to layer 1.

Recommended order: layer 1 now (an agent builds it), the log verdicts and the state read of layer 2 next (they only observe),
then option 1 of layer 3 on a dedicated session once the keys are known.

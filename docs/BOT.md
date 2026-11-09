# sc-bot: headless test bot

`tools/sc-bot` (a tool crate, not a service: no image) joins the stack as a client at the protocol level and runs scenarios
against it, so features can be checked without playing. It has two parts that ship separately:

- `sc-bot`: speaks the CryNub UDP protocol itself (Hub login over TLS, shard handshake, join state walk, reliable layer).
- `sc-bot-headless`: launches the real 4.10.2 client in its headless mode and drives it through its test-command pipes.
  Everything about the live client there is UNVERIFIED (see the end).

## Running against a cluster

Endpoints and credentials come from arguments or `SC_BOT_*` variables; nothing is built in.

```
sc-bot --login-data <LoginData.json> --hub <host:port> --ca <client.crt> --shard <host:port> --dgs-url <url> --report report.json
```

- Login: `--login-data` (token, username, Hub address) or `--token`/`--hub`. `--ca` is the CA that signed the Hub certificate.
- Direct mode: `--session <32 hex digits>` skips the Hub and puts a session id in the shard connect request.
- `--location` picks the location to go to; `--shard` overrides the shard address the Hub returns.
- `--dgs-url` enables the player-count check in `disconnect`.
- Fixtures the scenarios need: `--bank-location`, `--helmet-class`/`--helmet-port`, `--stack-class`,
  `--stack-equip-class`/`--stack-equip-port`, `--foreign-item` (an item the player does not own, for the denied token),
  `--expect-atc` (the home hangar has the kiosk container, so ATC, kiosks and call panels must be bound).
- `--process-method` is the player's method index of `RmAuthorityProcessInventoryManagementOperation` (default 185); the
  completion is taken at +10.
- `--scenario a,b` selects scenarios, `--list` lists them, `--strict` makes skips a failure.
- Timeouts: `--join-timeout`, `--scenario-timeout`, `--reply-timeout`, `--elevator-wait`.

Scenarios: `join`, `access-token`, `inventory-equip`, `inventory-move`, `inventory-stack`, `inventory-pocket`,
`inventory-equip-stack`, `elevator-call`, `disconnect`. The inventory scenarios restore what they move.

Exit code: 0 all passed, 1 a check failed (or a skip under `--strict`), 2 a scenario errored (could not run). The JSON report
(stdout, or `--report`) lists each scenario with its checks, facts, and the binds/events seen.

## Adding a scenario

Write `async fn` in `tools/sc-bot/src/scenarios/<name>.rs` taking the `Ctx` (see `join.rs`): join with `Session::join`,
act with `session.call(...)`/`pump`/`settle`, assert with `ctx.check(name, ok, detail)`, record with `ctx.fact`, and return
`Stop::Skip` when a fixture is missing. Then add one `scenario!` line to `ALL` in `scenarios/mod.rs`. A new request layout
goes in `requests.rs` with a byte-exact test against a captured or server-built message.

## Tests

- `cargo test -p sc-bot`: wire, nub, request layouts, a loopback of the reliable layer, a TLS Hub login against a fake Hub,
  and the headless framing against a fake pipe peer.
- `cargo build -p sc-pu-frontend && cargo test -p sc-bot --test local_stack -- --ignored --nocapture`: all nine scenarios
  against the real replicant binary and an in-process real DGS, with a fake Hub and a fake in-memory entitygraph (no
  ArangoDB, no podman). `SC_BOT_REPLICANT` overrides the replicant binary path.

Proven by those tests: the framing, handshake, ack/resend, bind parsing, the byte layout of the real equip request and
call-panel press, and all nine scenarios against the fakes. NOT proven: the real entitygraph/ArangoDB rules, the real Hub
with its database, method index 185 on a live build, real elevator timing.

Not emitted (layout not decoded): the tail of inventory requests; the per-type fields of Move, Split, Stack, StackAll, Swap,
SwapOutfit, AmmoRepool, Drop, pickup and StoreAndHide (inferred from the DGS builder; only Interaction and Store were seen
live); the call-panel interaction point is the one captured value; panel method indices on other panels; token flags other
than 0xce/0xcf.

## Headless client (`sc-bot-headless`) - unverified

```
sc-bot-headless --client <launcher> [--client-arg <a>]... --pipe-dir <dir> --game-log <Game.log> \
  [--wine] [--arg-prefix -] [--connect map,home] [--goto x,y,z] [--console "<text>"]... [--expect-log <text>]...
```

It makes two FIFOs in `--pipe-dir`, starts the client with `--client-headless-enabled 1 --client-login-show-dialog 0`
(unless `--no-default-args`), your `--client-arg`s (put the endpoint override flags there) and
`<prefix>testing_pipe_in <path>` / `<prefix>testing_pipe_out <path>`. Then it waits for `{"type":"TestSystem","name":"connected"}`,
optionally sends `map connect` and `map goto_global` (retried every 2 s until the client's state accepts them), runs the
console commands, lets the client run `--settle` seconds, kills it and scans the part of Game.log written since launch.
It fails on `closing inventory with N pending moves` (N above 0), `[NO ATC]`, `Snapshot Update For Missing Entity`,
and a few guessed rollback/failure lines (`default_forbidden` in `headless.rs`), and on any `--expect-log` text not found.

Protocol: NUL-terminated compact JSON. Client to harness: connected, events, and
`{"type":"command_response","command_id":N,"result":R,"details":..}` (0 ok, 1 failed, 2 adapter failure, 4 adapter not
found, 5 not supported). Harness to client: `{"command_id":N,"adapter_name":..,"command_name":..,"command_args":{..}}` + NUL.
Adapters: `console` (`command` {value}), `map` (`connect` {map_name, home_name} in state 2 only; `load_level`;
`unload_level` state 4 only; `goto_global`/`goto_zone` {position, rotation, target} state 4 only), `unittest` (`run_test`).
Vehicles, seats, weapons, call panels and UI are not reachable this way.

Unverified, never run against a client: the dash prefix of the pipe arguments; that the client opens a FIFO named with a
`Z:\...` path under Wine/Proton (a real named pipe `\\.\pipe\...` may be required, which would need a bridge such as a
Wine-side helper or socat to a Unix socket); who creates the pipes and in which mode; the harness-to-client message form
(inferred from the response form); the endpoint flags; the state numbers; the guessed log lines. Only the framing and message
parsing are tested, against a fake peer.

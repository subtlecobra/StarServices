# DGS transit

Elevators, then trams and shuttles. The DGS owns every carriage: the replicant only forwards what the client sends
(calls, picks, door requests) and what the DGS decides. The research behind this, from the game data, the binary and
official sessions, is kept with the reverse-engineering work outside this repository (`docs/transit/`).

Three rules come before copying the official servers:

1. At any time, the fewest carriages active that serve the calls.
2. A carriage unused for a long time is removed. A carriage with anyone aboard is never removed, and it stays bound to
   its riders' clients.
3. A carriage's doors never open at a stop whose location is not loaded for every rider aboard. Anyone aboard may
   step out when the doors open, and the server can't know who will, so the stop must be loaded for all of them.

Official servers never remove an unused carriage, and open the doors on a fixed timer (about 16 s from the pick, while
a Siege instance loaded up to 26 s after the players stepped out). See the research for the numbers.

## Data

A game build holds the hangar elevators of every home location, every hangar size, as three tables
(`tables/transit_*.tsv`, columns in [DATA.md](DATA.md)) that `tools/gamedata-build` reads from the archive and the
DGS's `transit` module loads at start: per hangar its system and the manager serving it, per Transit manager its
fleet (carriage class, cab, count) and timings, and every stop's gateway with its door, call panel and limbo node. No
data names a hangar's manager, as the hangar is instanced at run time: the converter takes the location's manager that
is linked to its instanced interior manager, and checks it against the tag rule (`transit.rs` there). Both agree at
Orison, New Babbage, Lorville and Levski; at the Pyro rest stops only the link holds, so their manager is marked as not
proven. Area18's hangars are recorded as their Transport manager and access point.

## Two systems

The game has two elevator systems, and each location uses one of them for its hangar elevator (build 4.10.2):

| System | Hangar elevators | Carriages |
|---|---|---|
| Transit (`SCTransit*`) | Orison, New Babbage, Lorville, Levski, the rest stops | Entities the server creates when needed and binds to clients with their state (`tc*` variables). A client never ties a carriage to its manager itself, so a fleet can be partly present. Official creates them only near players. |
| Transport (`SCTransport*`) | Area18, GrimHEX | The state of every carriage is a variable of the manager (`carriage_0` .. `carriage_{N-1}`, N from the data), and the client expects all N. A carriage's entity is optional: a carriage without one is streamed out. |

Rule 1 counts carriage entities. In Transit a carriage that exists nowhere costs nothing; in Transport its state
variable stays, idle and streamed out, and only its entity is created and removed.

The Transport manager logged at Orison serves the Shuttle Station's lift into the Siege instance, not the hangars. The
first target is Orison's hangar elevator, a Transit one.

## Several riders, one destination

N players are in one carriage and one of them picks a destination. (Several destinations: see the next section.)

**Readiness.** A rider is ready for a stop when their client has reported that the stop's location is loaded: every
entity batch the server sent for it has spawned. The client answers each batch with `BindBatchAsyncSpawnsCompleted`,
which carries only the batch id. So the replicant numbers its batches per connection (an id is never reused while a
batch is open: the client treats a duplicate as fatal) and keeps which location each batch belongs to; a location is
loaded for a client once every batch sent for it is answered. The answer means the entities and their containers are
created and physicalized, not that textures have streamed in, and an entity that failed to spawn is answered too. The
DGS must also hold the location itself. The doors wait for:

- the minimum ride time, 16 s from the pick (the official median), so a ride never feels shorter than official;
- every rider still aboard being ready.

When all riders are ready the doors open for all of them, as one carriage, as official does.

**A slow rider.** One rider's load must not hold the others for long. When the minimum ride time has passed, at least
one rider is ready and another is not, a deadline starts (`transit.slow_rider_grace`, 20 s by default). When it expires
the carriage splits:

- The DGS creates a second carriage of the same type, still in limbo, and moves the riders who are not ready into it,
  each at the same place relative to the cab. Both cabs are identical and in limbo, so a rider sees at most a small
  jump.
- The first carriage opens for the ready riders.
- The second opens when its riders are ready, under the same rules. It is an ordinary carriage from then on: when it
  is empty and idle, it is removed.

This is the only time a trip uses two carriages, which keeps rule 1 while keeping rule 3 for everyone.

The move is a teleport: a player's own client drives its position relative to its zone host (the carriage), and only
a teleport changes that host (`SActorTeleportParams`, as the official server teleports passengers). Its parameters
can keep the same place relative to the cab, with no screen fade. That the rider sees at most a one-frame jump is
expected from the binary and has to be shown on our server (the test plan is in the research). If it shows a break,
the fallback is that the ready riders wait too, with the doors closed, until the slow rider is ready or leaves.

The server can hold a carriage in limbo with its doors closed for as long as needed: official does it for hangars
(Transit's `tcWaitInLimbo`), and no client timeout was found. For Transport, whether a client ends a ride on its own
timer (its default longest teleport is 15 s) is checked by holding one for a minute.

**A rider who never becomes ready** while still connected (a lost report, a client stuck loading) keeps the panel:
they can pick another stop, such as the one they came from, which is loaded for them. After
`transit.resend_after` (60 s by default) without the report, the DGS sends the stop's batches to that client again.

## Several riders, several destinations

N players are in one carriage and pick M stops between them. The server knows who pressed each stop, but not who will
get out where: anyone may step out at any stop the doors open at. So the doors at every stop wait for every rider
aboard, whoever picked it.

**Stops queue.** The carriage serves one stop at a time, in the order they were picked (a ride through limbo takes the
same time whatever the distance). Each rider has at most one pending pick: a new pick replaces their previous one, so
nobody can queue ten stops. A stop nobody is waiting for any more (its picker got out earlier, or left the game) is
dropped from the queue.

**Every queued stop is loaded by every rider, all at once.** As soon as a stop is queued, the server starts sending
its location to every rider aboard, not only when it is next. While the carriage waits at the first stop, the others
load in the background, so the later stops are usually ready when the carriage gets there. A player who boards at a
stop starts loading every stop still queued. A rider who gets out stops loading the rest, and the server unbinds what
they no longer need.

**At each stop** the doors open under the rules of the previous section: the minimum ride time to the first stop
(and the stop's dwell time to each later one), and every rider aboard ready for that stop. The doors then stay open
for the official wait (5 s) after the last rider moves, then the carriage goes on to the next stop.

**A slow rider at one stop** is split off as in the previous section. The second carriage takes the slow riders and a
copy of the queue still ahead, this stop included: they get to it once they've loaded it, and to the rest after. The
first carriage opens for the ready riders and goes on with the same queue. Each split adds at most one carriage, so a
trip never uses more carriages than it has riders.

**Instanced stops.** A hangar is one player's instance. Official servers already let others in this way, and we do
the same:

- Each rider's panel lists their own hangars at that location and those of related players (most likely their
  party), labelled with the owner's name. A stranger's hangar is never offered.
- Picking an entry sends the carriage to the instance of that entry's owner.
- The doors open for everyone aboard, and anyone who steps out joins that hangar's instance: entering its zone sets
  the player's instance, with no check of owner or party.

So if B picks B's hangar, A can get out there too, whatever A's own list says. No rider is split off for access.

**What this costs.** Each rider loads up to N locations instead of one. A hangar is a small location; a large one
such as Siege's instance (about 19,000 entities) is the case to measure, and `transit.preload_limit` caps how many
queued stops load ahead of the next one if it is too much.

## A rider who leaves the game

**Alt+F4 or a crash.** The rider stops counting for readiness as soon as their connection ends.

- Alt+F4, closing the window, quitting to the desktop or exiting to the menu: the client sends a disconnect packet
  (transport type 5) before it goes: five copies at once when it quits, or one repeated until answered (type 6) when
  it exits to the menu. The replicant answers it and ends the connection at once.
- A crash, a killed process or a lost network sends nothing, and the connection ends after the replicant's peer
  timeout (60 s). Until then a crashed rider looks like a slow rider, so the others are split from them after the
  grace period and get out at most about 20 s late, not 60 s.

**Their character.** It is never saved inside a carriage or in limbo. A player whose connection ends aboard is saved
at the trip's destination stop, outside its door. If that destination no longer exists for them (their hangar is freed
when they leave), they are saved at the manager's stop in the persistent location instead (the spaceport, for a hangar
elevator). Logging in again then loads that place as any login does.

**The carriage.** When its last rider has left, it has no riders and no calls, and it is removed after the idle time
like any other (rule 2).

## Simulation

The DGS's `transit::sim` runs the rides of one manager as a `Line`: pure and deterministic, with no network and no
clock. Each step takes the time it happens at and one event (or none, a tick), and returns the commands to apply, in
order; the same events at the same times give the same commands. Wiring it to the replicant is separate work.

**Stops and locations.** A stop is a destination of the manager (its gateways, and the location a client loads for
it) or one player's hangar instance (owner-based, open while the instance exists). A carriage stops at one gateway of
a stop. Readiness is per rider and location: "rider X has location L loaded", whatever reports it.

**Carriages.** A carriage is a slot of the fleet (`0 .. fleet`: Transit's assigned id, Transport's `carriage_N`), in
one state:

| State | Where | Meaning |
|---|---|---|
| Idle | at a gateway, or in limbo | no riders, no call, nothing queued, doors closed; removed after `idle_removal` |
| Present | at a gateway | doors open, closing or closed; riders board and get out |
| Travelling | in limbo, toward a target gateway | the ride time is not over |
| WaitingInLimbo | in limbo | the ride is over and it waits: for every rider to be ready, for a free gateway, or for a pick |

**Events:** a call at a gateway; a player boards or gets out (the server's zone enter and exit; boarding needs a
carriage at a gateway); a pick (a destination, or an owner's hangar); a request to open or hold the doors; a rider has
a location loaded; a player's connection ended; a hangar instance opened (with its gateway count) or closed.

**Commands:** create a carriage's entity (at a gateway, or in limbo for a split); remove it; its state (with the
gateway it is at or its target, and since when); open or close its doors; move a rider into another carriage at the
same place in the cab; start loading a location for a rider (or again, after `resend_after`); stop loading one; save a
player who left the game aboard at a stop, outside a gateway.

**Settings**, with their defaults (the doors' wait and closing are the manager's data where it has them):

| Setting | Default | |
|---|---|---|
| `min_ride` | 16 s | doors open no earlier than this after the stop was picked |
| `min_leg` | 8.5 s | nor earlier than this after the carriage left its last stop: 16 s less the doors' wait and closing |
| `summon` | 0.3 s | a free carriage called to a gateway arrives after this (official call to present) |
| `door_wait` | 5 s | the doors stay open this long after the last rider moved (`carriageWaitTime`) |
| `door_close` | 2.5 s | the doors take this long to close before it leaves (`carriageDoorTimeout`) |
| `hold_doors` | 5 s | a hold adds this to the wait (Transport's `holdDoorsDelay`) |
| `transit.slow_rider_grace` | 20 s | the wait for a slow rider before the split |
| `transit.resend_after` | 60 s | a location not reported loaded after this is sent again |
| `transit.idle_removal` | 120 s | an idle carriage is removed after this. The game has no such timer: official servers never remove an unused carriage, only its entity when no player is near |
| `transit.preload_limit` | none | queued stops loaded past the next one |
| `split` | on | off is the fallback: the ready riders wait with the doors closed |

**What "fewest" means (rule 1).** The carriages needed are those with riders, and one for each called gateway that no
carriage stands at or is bound for, within the fleet. Every other carriage is idle, and the line keeps at most those
idle ones until `idle_removal`: a carriage left without riders closes its doors after the wait and is idle, a carriage
in limbo with neither riders nor a call is idle at once, and a carriage is created (for a call or a split) only when no
idle one can be sent. A split adds a carriage only when the slow riders are fewer than the riders aboard, so the
carriages of a trip never outnumber its riders.

**Decisions the design left open:**

- The ride time to a stop counts from its pick (`min_ride`) and from the carriage leaving its last stop (`min_leg`),
  whichever ends later: a stop picked on boarding opens 16 s after the pick, and one picked long ago still takes a ride.
- A call is served by a carriage standing at its gateway (the doors open again), else by one already bound for it, else
  by an idle one sent there, else by a new one created there with its doors open; with the fleet all out it waits.
  A carriage with riders is never diverted to a call.
- A carriage aims at a free gateway of its next stop, but holds none while in limbo: the first carriage ready takes a
  gateway. An idle carriage standing in the way is removed; a carriage whose riders have nothing queued, doors closed,
  goes down into limbo to wait there, and its riders keep the panel. So a hangar with one gateway is never blocked.
- A split takes an idle carriage of the fleet before creating one, and does not happen while the fleet is all out (the
  ready riders wait as in the fallback until a carriage is free). Both carriages keep the stop for all of their own
  riders; later stops follow the riders who picked them.
- A rider's pick is replaced by a new one; picking the stop the carriage stands at opens the doors instead. A stop with
  no picker aboard is dropped; a carriage whose last rider left drops its queue.
- Each rider keeps the last stop they were at (where they boarded or where the doors last opened for them) loaded while
  aboard, so going back never waits. The line only stops loading what it asked for.
- A rider who leaves the game aboard is saved at their pick, else the stop the carriage stands at, else its next stop,
  else the stop it came from; a hangar (anyone's) gives way to the persistent stop. They are not sent any unload.
- A closed hangar is dropped from every queue and pick, and an idle carriage there is removed. A carriage with riders
  standing at it or bound for it takes them to the persistent stop (the lobby) first, a full ride from the closing,
  its doors gated like any other stop's: the owner's friends are never left in a hangar that is gone.
- The doors' timings come from the manager's data; the hold time from Transport's class defaults, as Transit's data has
  none.

## Still open

- Whether a rider moved between two carriages in limbo sees a break (a test on our server).
- Whether a Transport client ends a held ride on its own timer (a test on our server).
- How a newly created Transport carriage entity finds its `carriage_N` state.
- Which manager serves the Orbituary rest stop's hangars: `TransitManager_Hangar-to-Lobby` is the one linked to the
  station's instanced interior manager, but its tag filter does not accept the hangar's gateway.
- Which service lists the related players whose hangars a panel offers, and how Transit merges several riders' lists.
- Whether two carriages in limbo below the same gateway (a split, or carriages waiting for one gateway) see each other.

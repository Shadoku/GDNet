# 10. Physics bodies: who simulates them, what clients show, and what they cost

A `RigidBody2D` or `RigidBody3D` under a `NetManager` is an entity like any scene instance (page 2), and
by default it needs nothing new: the server simulates it, clients show it, a body at rest costs nothing,
and a byte budget decides which moving bodies a client hears about first. Two options change that per
body, with no script change: **`physics`**, who simulates it, and **`body_prediction`**, what a client
that does not simulate it shows. This page covers the default, the four physics modes and how to choose,
body prediction, how the two combine, sleeping bodies, bandwidth, the known limits, and the Godot facts
a game script should know (docs/PLAN_NETCODE.md 2.11, 12.4, WP14, WP18, WP19). The worked example is the
Ballpit demo, `demos/ballpit/README.md`, whose HUD switches both options on a running pit.

## A body on the network: the default and what it costs

A body's inferred profile (page 3) makes its transform and its `linear_velocity` and `angular_velocity`
**hot**: sent in snapshots whenever their quantised value changes, and interpolated on clients. A body
update is about 15.8 bytes in 2D and 17.8 in 3D ("What a body update costs" below), and a sleeping body
sends nothing ("Sleeping bodies cost nothing").

By default (`physics = server`, D19) the server simulates the body with Godot's physics. Every other
peer holds a **replica**: gdnet sets `freeze = true` when it creates it, and again whenever an owner or
mode change moves the simulation away, and turns off physics processing of its parts (page 4). A frozen
body in the default `FREEZE_MODE_STATIC` is a static collider: other bodies collide with it, it never
moves by itself, and the snapshots move it. Its scripts' `_process` still runs, so visual code keeps
working. On the peer that simulates a body gdnet leaves `freeze` as the game set it: a body the scene or
a script froze waits there, as in single-player, until the game sets `freeze = false` (a ball held in the
air until it is thrown); only a change of who simulates thaws it.

Godot's physics is never rolled back: the engine cannot step a space on demand, and its engines are not
deterministic across machines (D24). The modes below are what can be done without it.

## The physics modes

Set the mode per body with `NetEntity.physics` (the `PHYSICS_*` constants) or `Net.configure(body,
{"physics": "handoff"})`, per manager with `NetManager.physics`, and per project with
`gdnet/physics/mode` (default `server`); the first that is not `default` wins. It applies to entities with
a `RigidBody2D` / `RigidBody3D` part (subclasses such as `VehicleBody3D` included); an entity without one
is `server` whatever it says. Change it on the server at any time (the property, or
`Net.set_physics(node, "simulated")`) and every peer follows at the next tick; on a client the option only
changes the local value (the server decides). `Net.physics_of(node)` says what this peer holds, and the
overlay labels each body with its mode (page 8). Modes mix freely in one scene.

| mode | who simulates | what the other peers show | what it feels like | what it is for |
|---|---|---|---|---|
| `server` (default) | the server | a frozen replica, interpolated (or predicted, below) | a client's push shows after a round trip plus the jitter buffer; piles are stable and identical everywhere | scenery physics, piles, settled structures, anything nobody pushes often |
| `owner` | the entity's owner | a frozen replica, the server included | immediate for the owner; where two owners' bodies meet, each collides with the other's past | a player's own vehicle or contraption |
| `handoff` | whoever is pushing it: the client whose player or bodies push it, until it rests; the server otherwise | a frozen replica, the server included while a client holds it | immediate for the one pushing; the others see it a round trip later | loose shared objects: crates, balls, carried items |
| `simulated` | every peer; the server's result is the reference | nothing frozen: the local body runs and the snapshots correct it | contacts are immediate on every screen; where the simulations disagree the body slides to the server's answer | contact-heavy play: a predicted player riding or shoving moving bodies |

Measured, each over emulated links of 50 ms each way (the WP19 tests):

| mode | figures |
|---|---|
| `server` | a predicted paddle's push in the Ballpit: the owner sees the first ball move 5 to 6 ticks after the server moved it, zero rollbacks (the paddle does not collide with the balls) |
| `owner` | the owner's body moves on the tick of the input; the server shows it 7 ticks after the press, the other client 11 to 12; the same as `client_owned = true` |
| `handoff` | the pusher's crate moves on the first physics step after the contact; the server 9 to 10 ticks later in 2D (11 to 13 in 3D), the other client 13 to 15 (15 to 18); back to the server 0.73 to 0.77 s after the key is released; 0 to 6 rollbacks of the pusher over a 1.5 s push (2 to 7 in 3D). In the Ballpit a paddle touched a ball, the ball moved on the pusher's screen the next tick, on the server 11 ticks after the contact, with no rollback |
| `simulated` | a predicted player pushing a crate moves it on the frame after the contact; a predicted player on a swinging platform rolls back 7 times in 180 ticks (6 in 3D) against 54 to 57 on a `server` platform; a push only the server saw reaches the client late and the crate slides there (largest step 14 to 16 px, 3 corrections, settled 0.02 to 0.08 px apart; 6.7 mm in 3D); a resting pile of 40 crates the client joins: asleep, 0 corrections per second, worst gap 0.18 px |

### Choosing a mode

- Start with `server`. It needs no tuning and is the only mode in which every peer sees the same pile.
- A body one player drives (a vehicle, a contraption): `owner`. It is `client_owned` for bodies (page 7).
- Loose things players push or pick up: `handoff`.
- A predicted player that stands on, rides or shoves moving bodies all the time: `simulated` for those
  bodies, accepting visible corrections when the simulations disagree (and see "Known limits").
- A body that flies more than it collides (a thrown grenade, a puck) is often best left `server` with
  `body_prediction = dead_reckoning`: it is then shown where it is now, not where it was.

**A predicted player and frozen replicas.** A replica is a picture of the server's body half a round trip
plus the jitter buffer in the past, and a predicted player runs ahead of the server. A player whose
collision mask includes frozen replicas bumps into those pictures, stops where the server's player did
not, and is rolled back by the next snapshot. With `server` or `owner` bodies, keep the bodies' layer out
of the player's mask and the player's layer in the bodies' mask: the player's own motion is then exact,
the server's player pushes the server's bodies, and the owner sees them react a round trip late (the
Ballpit paddle with `server` balls). With `handoff` or `simulated` bodies the body a player touches is
live on its screen, so the player may collide with it; the Ballpit's Spawner adds the balls to the
paddle's mask in those modes. Body prediction also helps: a predicted player that walks into a
dead-reckoned puck meets it 4.8 px from where the server has it, against 30.6 px when the puck is
interpolated, because the predicted pose is where the frozen collider is.

### `owner`

`physics = owner` and `client_owned = true` on a body are the same thing (D21; a body with `client_owned =
true` is `owner` whatever its option says). The owner's body is live, wakes and sleeps as usual and moves
on the tick of its input; every other copy, the server's included, is frozen. The server applies the
owner's states as they arrive and relays them in its snapshots, so the other clients see the body through
the server. Changing the owner moves the simulation: the old owner's copy freezes, the new owner's thaws
with the velocities it was showing. When a seat drops (page 13) its bodies freeze in place on the server
until it rejoins or its seat is released. The overlay shows `owner-sim <peer>`.

With `body_prediction = dead_reckoning` on another client, an `owner` body is shown at the server's
present: a coasting vehicle within 1.4 ticks of the server's copy (9.5 ms on average), and behind its
owner's own present by half the owner's round trip plus a few ticks (68 to 92 ms; the bound is 50 ms plus
three ticks). Dead reckoning does not know the owner's input: while the owner thrusts, the other client
trails the server's copy by about 52 ms; a `custom` hook can model the thrust.

### `handoff`

A body is simulated by whoever is pushing it and by the server the rest of the time. It is `owner` with an
owner that changes by itself: the holder is the entity's owner (`owner_changed` fires on every peer at
every claim and return, `Net.get_owner_of` names it), its body is live, every other copy is frozen.

*What claims a body.* On a client, its predicted player or a body it simulates (its `owner` bodies, the
`handoff` bodies it holds) pushing a `handoff` body the server holds. Pushing means: a `CharacterBody`
stopped by the body (walking into it from the side or from below, or landing on it); a `RigidBody`
touching it while faster than `correction_rest_speed`, or about to hit it within two ticks of its motion
(a body hit while still frozen stops the one that hits it dead). A player standing still on a body, or a
body resting on another, does not claim it, and a push never takes a body another client holds. A
`CharacterBody` stops at a `RigidBody` and does not push it: the player's script pushes the bodies it
hits, as in single-player (the Ballpit paddle's loop after `move_and_slide()`). Game code claims a body
with `Net.claim(node)` (also `NetSession.claim`) for a grab or a carried item; that may take it from
another client, and the body stays held until `Net.release_owner(node)`. To read contacts gdnet raises
`max_contacts_reported` to 4 on a body that has 0 and no `contact_monitor`.

*The claim is optimistic.* The claiming client thaws the body in the same tick, so it moves on the next
physics step. The claim carries the body's ownership sequence number plus one, and every change of owner
raises the number. The server accepts the first claim for a number; claims for the same number in the
same server tick go to the lowest peer id (a release counts as the server's, peer 1), and a claim for an
older number is refused. The loser is told the winner and the state; its copy freezes where it was and
blends to the winner's like a correction (measured after two players pushed one crate from both sides:
the loser's largest step beyond the body's own motion 0.86 px between two clients, 2.1 to 2.2 px for a
host against a client, 2.8 cm in 3D). On a listen server the host's player holds what it touches: a
client's claim for a body the host's player touched in the last 6 ticks (100 ms at 60 Hz) is refused. A
client whose claim was refused tries again after the rest time at the earliest. Overlay rows show the
holder (`simulated_by`) and the number (`handoff_seq`).

*The holder's player and the server agree.* The holder stamps each state of the body with the tick whose
inputs met it, and the server applies it at that tick, so the server's copy of the holder's player meets
the body where the holder's player met it. At the start of each tick the holder also puts the body at its
quantised position and rotation (what the server receives): a tenth of a pixel decides whether a player
touches a crate. During a rollback replay a held body is posed where it was at each replayed tick. Without
the stamping and the quantised pose a 1.5 s push cost 48 rollbacks. States from anyone but the holder, or
from before its claim (late packets of a previous holder), are dropped (`handoff_stale_states`); a
claimant's states that arrive with its claim wait for the server's decision.

*Returning.* The holder gives the body back when it has been at rest (asleep, or slower than
`correction_rest_speed`) for `gdnet/physics/handoff_rest_s` (default 0.5 s, about when Godot puts a body to
sleep), or when nothing of the holder's has touched it and none of its players has been within the
manager's `interaction_radius` of it for that long. The server takes a body back at once when its holder
leaves (whether or not its seat is held) and when the body leaves the holder's scope; it goes on from the
holder's state, still moving if it was. A body the server holds is the server's to simulate, so a pile of
`handoff` crates nobody touches is a `server` pile: asleep, no bytes, no claims.

The other clients show a held body frozen, interpolated or per `body_prediction`. With `dead_reckoning`
they show the holder's relayed present, which trails the holder by the holder's prediction lead (measured:
143 to 159 ms with a lead of 9 ticks; the bound is the lead plus three ticks).

When two players push one body from both sides, the one that does not hold it collides with the holder's
body as it was (frozen, a little in the past); bodies that are always in shared contact are what
`simulated` is for.

### `simulated`

Every peer runs the body. A client's body runs in the client's *predicted* timeline, the moment its own
predicted player is at, so a player and the bodies it touches meet where they are: a crate a player
pushes moves in the tick of contact, and a player riding a platform rides the live platform. The client
records each body's pose per tick, and a snapshot of tick T is compared with the pose recorded for T (both
carried to the present at their velocities), so motion both sides share is never corrected, whatever the
latency. Then:

- a difference below the correction thresholds is left alone on a resting body (a pile stays asleep) and
  refined quietly on a moving one;
- above them, on a moving body, it becomes a decaying offset on position and rotation, and the velocities
  are set to the server's; a resting body settles instead ("Resting replicas settle" below);
- beyond the property's `teleport_distance` (page 5) the body snaps.

Corrections move the body through the physics server (`body_set_state`), because setting `position` on an
awake `RigidBody` does not reach it ("Godot facts" below). During a rollback replay the bodies are not
stepped: each replayed tick poses them where they were recorded at that tick, and the present comes back
afterwards, so a replayed push does not count twice. The server never corrects its own bodies, and
snapshots cost the same in every mode. A `simulated` replica is not interpolated, for any field, and
`body_prediction` does not apply to it: the local physics is the prediction.

### Correction settings

One set of settings, read at each use, serves `simulated` corrections, the blend of a lost `handoff`
claim and body prediction's corrections:

| setting | default | meaning |
|---|---|---|
| `gdnet/physics/correction_position` | 0.02 (metres) | a position difference above this is a correction; 2 px in 2D at 100 px per metre, 2 cm in 3D |
| `gdnet/physics/correction_angle` | 3 (degrees) | the same for rotation |
| `gdnet/physics/correction_decay` | 0.85 | the share of a correction's offset left after one 60 Hz tick |
| `gdnet/physics/correction_rest_speed` | 0.15 (metres per second) | a body slower than this counts as at rest (`simulated`, `handoff`); keep it above Godot's sleep threshold |

`NetSession.stats()` on a client: `sim_bodies` (its live `simulated` bodies), `sim_correcting` (those
with an offset left), `sim_corrections` and `sim_corrections_per_s`, `sim_snaps`, `sim_settles`.

*Resting replicas settle.* A resting body is not corrected: a nudge would wake it, its neighbours would
push it back, and a stacked pile would be corrected for ever. Instead, once the server's body has reported
rest with nothing newer for 0.15 s and the client's body has been still for 0.15 s, the replica is frozen,
carried to the server's pose in 1 to 15 ticks (0.25 s at most at 60 Hz) and released exactly there, still
and asleep. The manager's other resting bodies join the round and are released together, so a pile ends
in the server's arrangement. A body one of this client's predicted entities touched in the last 30 ticks
is left alone, and a body knocked off again settles after 0.5 s at the earliest. Measured: a replica
0.021 m off settles in 0.35 s; a pile that settles while a client watches is asleep there 0.8 s after the
server's; the Ballpit's two-row pile of 60 balls ends 0.06 px from the server's, every ball asleep.

*Pushes and the physics space.* Whether a `CharacterBody`'s `move_and_slide()` meets the floor or a body
first depends on the physics space's history, and Godot cancels travel under 2 cm toward a wall met first.
A predicted pusher and the server could therefore part by a fraction of a millimetre and never meet
again. `gdnet/prediction/quantize_state` (default true) starts every tick of a predicted entity, on the
server and on its owner, from its positions and velocities as the snapshots carry them (1 mm and 1 mm/s
in 3D, 0.1 px and 0.1 px/s in 2D; rotations are untouched, and an entity touching a `simulated` body is
skipped). A 3D `handoff` push in a much-used space went from 23 rollbacks to 3 or 4. Side effect: a
velocity a game damps by multiplying stops changing once a tick's decrease is under half a step (about
5 mm/s in 3D); a game that needs it to reach exactly zero turns the setting off.

## Body prediction: showing a body where it is now

A frozen replica normally shows the server's past. `body_prediction` chooses what a client shows instead
(D26, D27):

| `body_prediction` | what the replica shows |
|---|---|
| `interpolate` (default) | the server's past, between two snapshots: exact, late by half a round trip plus the jitter buffer |
| `dead_reckoning` | the newest snapshot carried forward to the present by its velocities, gravity and damping |
| `custom` | the same starting point, advanced by the body script's `_net_predict_body(delta)` |

It is the client's display choice: nothing changes on the wire or on the server, so each client may
choose its own (the Ballpit's prediction selector is per client). Set it per body (`NetEntity.body_prediction`,
or `Net.configure(body, {"body_prediction": "dead_reckoning"})`, which wins over the `NetEntity`), per
manager (`NetManager.body_prediction`) or per project (`gdnet/physics/body_prediction`). A body whose script
defines `_net_predict_body` and has no setting of its own uses `custom`, even when its manager or the
project says otherwise; `custom` on a body without the hook means `dead_reckoning`. Changes apply at the
next frame. `Net.body_prediction_of(node)` returns the effective choice (`NetEntity.BODY_PREDICTION_INTERPOLATE`,
`_DEAD_RECKONING`, `_CUSTOM`, or `_NONE`, below), and the overlay labels a predicted body with its mode.
Only an entity's root body is predicted.

```gdscript
# A thrown grenade: shown where it is, not where it was.
Net.configure($Grenade, {"body_prediction": "dead_reckoning"})
```

**What dead reckoning computes.** It takes the newest snapshot's position, rotation, `linear_velocity`
and `angular_velocity` and steps them one physics tick at a time, as Godot's engines do for a body in free
flight: `v *= max(0, 1 - linear_damp * dt)`, then `v += gravity * gravity_scale * dt`, then `x += v * dt`
(Jolt adds gravity before damping; rotation is advanced by `angular_velocity * dt` the same way). The time
it steps over (the *horizon*) is the client's estimate of the server's present tick minus the snapshot's
tick: about half the round trip plus the snapshot's age, capped at `gdnet/physics/predict_max_ms` (250).
The estimate runs on the client's physics clock: the leading edge of snapshot arrivals plus half the
smallest recent round trip. Gravity and damping are the space's defaults (the project's
`physics/2d|3d/default_*`) and the replica's own `gravity_scale` and `linear_damp` / `angular_damp`,
which the default profiles do not replicate: keep them in the scene, or add them to the profile if a
script changes them. `Area2D/3D` gravity and damping overrides are not applied; use `custom` where they
matter. A body resting or sliding on something (its velocity stopped following gravity) is predicted
without gravity, so a sliding crate does not sink into the floor; a body at rest or asleep is not
predicted at all, so a sleeping pile costs nothing.

Gate G13: over a 100 ms link a thrown ball shown with `dead_reckoning` stays within one tick's travel of
the server's present position on every frame (measured worst 2.8 to 3.7 px against 4.8 to 4.9 px in 2D,
2.8 cm against 5.1 cm in 3D), where the interpolated one trails by about 31 px (1.8 times its velocity
times the one-way delay). In the Ballpit a thrown ball was on average 2.8 px from the server's present
dead-reckoned and 16.5 px interpolated.

**What it cannot know.** Dead reckoning knows nothing of a collision the server has not reported yet: a
ball predicted towards a wall passes into it until the snapshot of the bounce arrives, then bends back.
Each new snapshot is compared with what the prediction showed; the difference becomes an offset that
decays by `correction_decay` per 60 Hz frame, so the body bends back instead of jumping, and it snaps
beyond `teleport_distance`. Differences above the correction thresholds count in `stats()`. Measured: a
ball hitting the floor at 775 px/s passed it by about 64 px (less than speed times the 250 ms cap, 194 px),
its largest step in a frame was 6 px, and it ended within half a quantiser step of the server's. A body
that starts moving jumps ahead once by its velocity times the horizon (the interpolated pose was only
late).

**`custom`.** Before each call the body's position, rotation, `linear_velocity` and `angular_velocity`
hold the state to advance (the newest snapshot at first); the hook changes them as its own movement code
would over `delta` seconds, and gdnet shows the result. `Net.dead_reckon(body, delta)` is the built-in
step, if the hook wants it:

```gdscript
func _net_predict_body(delta: float) -> void:
	Net.dead_reckon(self, delta)                       # the default step; optional
	linear_velocity += -transform.basis.z * throttle * THRUST * delta
	position += -transform.basis.z * throttle * THRUST * delta * delta * 0.5
```

The calls are incremental: one per tick of the horizon (`delta` is one tick) and one per frame with the
fraction of a tick that is left; a new snapshot starts again from the snapshot. That is one or two calls
on most frames and a few more on a frame a snapshot arrives (measured: 3.5 calls a frame at 145 frames per
second over a 100 ms link). The hook should change only those four properties (it may read anything): the
fraction's call is not kept.

**Choosing.** Keep `interpolate` for piles and anything that collides all the time: dead reckoning
overshoots at every contact (D27). Use `dead_reckoning` for thrown and flying things and for bodies other
players drive, and `custom` for bodies with their own propulsion. `stats()` on a client:
`predicted_bodies` (moving bodies predicted in the last frame), `prediction_replicas` (frozen body
replicas body prediction may show), `prediction_corrections` and `prediction_corrections_per_s`,
`prediction_horizon_ms`.

**Moving a frozen body every frame.** Replicas use Godot's default `FREEZE_MODE_STATIC`: setting the
position teleports the collider (it is not swept), and the physics server wakes the bodies it touches,
as with interpolation, which moves replicas every frame too. A fast predicted body can therefore pass
through a thin collider between two frames; it is a picture of the server's body, which collides for real.

## How modes and prediction combine

Body prediction applies only where a peer shows the body as a frozen replica, which the mode decides:

| mode | where `body_prediction` applies |
|---|---|
| `server` | every client |
| `owner` | every client but the owner (the server applies the owner's states as they arrive and predicts nothing) |
| `handoff` | every client but the holder (the server predicts nothing) |
| `simulated` | nowhere: every peer runs the body |

Where the peer simulates the body itself, `Net.body_prediction_of` returns `NetEntity.BODY_PREDICTION_NONE`
(-1) and the overlay shows no mode. A mode or owner change moves a body into or out of prediction at once.
`SetOwner` and `SetPhysics` carry the tick and the entity's full state, so the new role starts from the
server's values even for fields that do not change (a body at constant velocity keeps it across a
handover), and a moving body does not jump: at 150 px/s over a 100 ms link no frame moved by more than its
travel plus 1.6 px, except the entry into `simulated`, where the body slides to the client's predicted
present, first by 15 % of that gap (5 px).

## Sleeping bodies cost nothing

The server records a hot field only when its *quantised* encoding changes. A body Godot put to sleep does
not move: its position, rotation and velocities keep exactly the values they had, so nothing is recorded,
nothing is pending, and no snapshot names it. Hash telemetry (page 8) resends an unchanged entity's hash
only every `gdnet/debug/hash_refresh_s` (10 s). Measured with the Ballpit demo: 300 sleeping balls, zero
recorded changes and zero snapshot bytes over a second; everything a client receives is the pings
(56 B/s). No dead band is applied: Godot does not jitter a sleeping body.

Sleeping bodies cost no CPU either: a sleeping body is off the server's hot list, so its position,
rotation and velocities are not read until its `sleeping_state_changed` signal reports it awake, and it
is sent in the tick it wakes (page 3, "Polling cost"). Its cold properties (and whether it still sleeps
and is not frozen, which Godot does not signal when a script sets them) are read every
`gdnet/sync/resting_poll_ticks` (24 ticks, 0.4 s at 60 Hz; `Net.touch(body)` reads them at once). The
meter's `poll` line counts the hot, idle and sleeping entities.

Two practical notes. A body that rolls very slowly on a flat floor can doze off and wake again for
seconds, costing a few bytes each time it moves; a little `angular_damp` (the Ballpit balls use 3) brings
it to rest. And a kinematic body (a paddle) resting inside a pile keeps nudging the bodies it overlaps, so
they never sleep.

The same holds in 3D: a pile of 40 `RigidBody3D` boxes on a `StaticBody3D` floor sleeps about 1.3 s after
it is dropped, the client's copies are within half a `mm` step (0.5 mm per axis) and 1.5e-4 rad of the
server's, and asleep the pile costs zero snapshot bytes (`tests/integration/test_net_3d_physics.gd`).

### The 3D physics engine

This project does not set `physics/3d/physics_engine`, so it runs on `DEFAULT`, which in Godot 4.6 is
GodotPhysics3D (a game picks Jolt with `physics/3d/physics_engine = "Jolt Physics"`). gdnet does not
depend on the engine: the WP18 3D tests pass on both. Two differences show in them. Jolt brings the test's
pile to sleep sooner (about 1.0 s against 1.3 s) and its body updates are a little larger while they fall
(18.5 against 17.8 bytes), which is the engines' different motion, not gdnet. And a `CharacterBody3D`
standing still on a body reports that body as a slide collision under GodotPhysics3D and nothing under
Jolt, so under Jolt the body a still player stands on interacts through the radius only; once the player
moves, both report what it touches.

## Priority: who goes first

Each tick the server builds one snapshot per client, filled up to the client's budget and never beyond
one 1,200-byte MTU. Every entity with a pending field adds a weight to its accumulator for that client:

| entity | weight per tick |
|---|---|
| owned by the client (its player) | 1,000,000 |
| interacting with the client | 100 |
| anything else | 1 |

times the entity's `priority` (`Net.configure(node, {"priority": p})`, `NetEntity.priority`). Entities go
into the snapshot in accumulator order; a sent entity drops back to zero, one that did not fit keeps its
sum and rises. An interacting body therefore goes out nearly every tick, and a distant body that has been
waiting k ticks overtakes it only after k reaches 100. Nothing is lost: a field left out stays pending and
the next snapshot that carries it carries its current value (most-recent-state, page 3).

## Interaction: near the player, or touching it

Before the snapshots are built, each server tick, gdnet marks for every client with a player (the first
entity `Net.spawn_for`'d to it, or `Net.set_player`) the entities that interact with it:

* entities whose root is within the manager's `interaction_radius` of the player's root (2D: pixels,
  3D: metres; `0` = the project setting `gdnet/budget/interaction_radius`, which is in pixels: 300 px,
  read as 3 m in 3D at 100 px per metre; negative = off for that manager), and
* bodies in contact with the player's body: a `RigidBody2D/3D` player with `contact_monitor` on reports
  `get_colliding_bodies()`; a `CharacterBody2D/3D` player reports its last `move_and_slide()` collisions.

The distance test runs on a coarse grid of cells as large as the largest radius, so it stays near linear
in the number of entities: about 0.4 ms for 300 bodies per tick in the demo's test (`stats()` keys
`interaction_ms`, the last pass, and `interaction_ms_avg`, the mean per tick over the last second). Most
of it is engine calls (each root's global position). The server's own player (a host) is not scored: it
receives no snapshots, and a peer without a player costs nothing. To trade reaction for time,
`gdnet/budget/interaction_every_ticks` (default 1) runs the pass every N server ticks and keeps the last
sets in between: at 4 the demo's test measures 0.11 ms per tick instead of 0.41.

Choose the radius by the budget: every interacting body that moves costs a full update each tick. In the
Ballpit demo the halved 16 KB/s budget carries about 17 body updates a tick; a 50 px radius around the
paddle keeps the interacting set below that, and those balls update on every tick they move.

```gdscript
NetSession.interacting(peer)          # the ids marked this tick (server)
NetSession.entity_sends(node, peer)   # how many snapshots carried that entity to that peer (server)
```

## The budget and degradation

`Net.budget_bytes_per_s` (or `NetSession.budget_bytes_per_s`; both apply to a running session at once;
the project setting `gdnet/budget/bytes_per_second`, 32,768) is a token bucket per client: it fills by
budget / tick rate each tick and holds at most two MTUs of slack, so the average stays within the budget
while a full MTU is still possible now and then. When more is pending than fits, the low-weight entities
wait: they **degrade** (update less often) instead of the budget being exceeded.

The degradation counter says how much: every tick, the entities with pending fields that a snapshot left
out entirely are counted as *deferred entities*, and every pending field a snapshot did not carry
(left-out entities and the rest of partly sent ones) as *deferred fields*. Measured with 300 shaken balls
at 16 KB/s: 16,387 B/s of snapshots, about 17,200 deferred entities per second, interacting balls sent on
every tick they moved and the others on 3 to 5 % of theirs.

## The meter

The meter's `budget` line (page 8):

```
budget 100% of 16384 B/s per peer  deferred: 17205 ent/s  interacting 4
```

* the share of the budget the snapshots used over the last second (server side, all peers),
* `deferred`: entities left out of snapshots per second; 0 while everything fits, it rises as soon as the
  budget (or the MTU) is the limit,
* `interacting`: entities marked interacting, summed over the clients.

`stats()` keys: `deferred_entities`, `deferred_fields` (totals), `deferred_entities_per_s`,
`deferred_fields_per_s`, `deferred_entities_tick`, `deferred_fields_tick` (the last tick's, all peers),
`interacting`, `interaction_ms`, `interaction_ms_avg`, `interaction_passes`, `hash_sample` (the effective
sample).

## What a body update costs

A ball's update carries its changed hot fields, quantised (position and linear velocity at `mm`, which in
2D is 0.1 px: 2 bytes per axis for values in the hundreds of pixels; rotation 2 bytes; angular velocity 2
bytes), plus an entry header: one bit, the net id as a varint (2 bytes above 127), the record length as a
varint (1 byte) and the dirty masks (9 bits for a ball). About 15.8 bytes in all; 300 moving bodies fill
MTU-sized snapshots to within 24 bytes. Two levers exist if that matters for a game: a coarser quantiser
(`cm` is a 1 px step in 2D and saves about a byte per axis for fast or far-flung bodies) through
`Net.configure` or a profile, and fewer hot properties (a body whose `scale` never changes costs nothing
for it: only changed fields travel).

A `RigidBody3D` update is about the same size: measured over the first quarter second of the 3D pile's
fall, 17.8 bytes per body update (position and both velocities at `mm`, a metre being 1,000 steps, so
1 to 2 bytes per axis for values under 8 m and 8 m/s; the quaternion's 47 bits; the same entry header).
Physics modes and body prediction change none of this: snapshots cost the same in every mode.

## Hash telemetry with many bodies

Hash telemetry samples `gdnet/debug/hash_sample` entities a tick (16). With hundreds of moving bodies
that can be a noticeable share of a small budget, so a busy scene lowers it: `Net.hash_sample = 2` (every
session that has no value of its own, at its next tick), or per session
`Net.session_for(self).hash_sample = 2`. The Ballpit demo does the latter; its channel 2 (hashes and
pings) stays at about 88 B/s with 300 shaken balls, 0.5 % of a 16 KB/s budget.

## Known limits

- **Pushing a `simulated` body costs rollbacks.** Each side simulates the crate itself, and the two can
  part (one tumbles, one slides): about 12 rollbacks of the pusher over a push in 2D, against 0 to 4 with
  `handoff`. Use `handoff` for things players push.
- **A settling `simulated` replica is frozen** for up to 15 ticks (see "Resting replicas settle" above).
  The Ballpit with 120 balls, or with balls spawned in `simulated`, has not been measured.
- **Client-driven bodies on a listen server** (`owner`, held `handoff`) look snapped on the host: a
  server applies a client's states as they arrive and does not interpolate them.
- **A mode changed at run time is not saved** in level state (page 9); the scene's or manager's value comes
  back.
- **Dead reckoning** ignores `Area` gravity and damping overrides, and contacts the server has not reported
  (bounded by the 250 ms cap); only an entity's root body is predicted; frozen colliders are teleported,
  not swept.
- **`handoff`**: a push claims only bodies the server holds (a listen server's host never takes a client's
  body by touching it); `owner_changed` fires on every claim and return; the holder quantises its own pose
  every tick; a body jammed among frozen ones (the middle of a pile) barely moves when claimed, because
  its neighbours are still frozen until they are claimed in turn.
- **One entity at a time.** A mode applies to an entity's bodies, a claim takes one entity; jointed
  assemblies are WP20.

## Godot facts game scripts should know

- **Setting `position` (or `transform`) on an awake `RigidBody` does not reach the physics server** in
  Godot 4.6: the next physics sync puts the body back. Move a simulating body with
  `PhysicsServer2D/3D.body_set_state(rid, BODY_STATE_TRANSFORM, ...)` (as gdnet's corrections do), or
  freeze it first; call `Net.touch(node)` when it was asleep.
- **A `CharacterBody` does not push `RigidBody`s.** `move_and_slide()` stops at them; apply an impulse to
  what `get_slide_collision(i)` reports, as the Ballpit paddle does.
- **Freezing a `RigidBody2D` that was simulating can leave its collider where it froze** in Godot 4.6, so
  later position writes do not move the collider; gdnet reassigns `global_transform` after it freezes one,
  and a script that freezes a body and then moves it should do the same.
- **Godot emits no `sleeping_state_changed`** when a script sets `sleeping` or freezes a sleeping body:
  gdnet notices within `resting_poll_ticks`, or at once after `Net.touch(node)`.
- **Godot wakes a body when it enters the tree**: a spawned body is awake at first.
- **A physics space's history changes its results.** An emptied space keeps the broadphase layout its
  earlier bodies left, so the same scene can end a few 1e-6 apart depending on what ran before. gdnet's
  test runner gives every test new physics spaces; a game that compares runs should do the same.

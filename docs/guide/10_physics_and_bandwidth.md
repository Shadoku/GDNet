# 10. Physics bodies and bandwidth: many bodies, a budget, and what degrades

A scene with hundreds of physics bodies needs nothing new from the model (docs/PLAN_NETCODE.md 2.11): the
server simulates them, clients receive their state and show it interpolated, a body at rest costs
nothing, and a byte budget decides which moving bodies a client hears about first. This page explains
each of those, and what the numbers on the meter mean. The worked example is the Ballpit demo,
`demos/ballpit/README.md`.

## RigidBodies on the network

A `RigidBody2D` or `RigidBody3D` under a `NetManager` is an entity like any scene instance (page 2). Its
inferred profile (page 3) adds `linear_velocity` and `angular_velocity` to the transform as **hot** state:
sent in snapshots whenever their quantised value changes, and interpolated on clients. The server owns
the body and simulates it with Godot's physics; there is no client-side simulation and no rollback of
bodies (the plan rules it out: Godot physics is not deterministic).

On every peer that does not simulate the body (with the default `physics = server`: every client) the
body is a **replica**: gdnet sets `freeze = true` when the replica is created, and again whenever an owner
or mode change moves the simulation away (see "Physics modes" below), and turns off physics processing of
its parts (page 4). A frozen body in the default `FREEZE_MODE_STATIC` is a static collider: other bodies
collide with it, it never moves by itself, and the interpolated snapshots move it. Its scripts' `_process`
still runs (visual code keeps working). On the peer that simulates the body gdnet leaves its `freeze` as
the game set it: a body the scene or a script froze waits there, as in single-player, until the game sets
`freeze = false` (a ball held in the air until it is thrown); only a change of who simulates thaws it.

### What the local player can and cannot push

A replica is a picture of the server's body a little in the past (half a round trip plus the jitter
buffer, page 5). A predicted player (page 4) runs ahead of the server. If the player's own collision mask
includes the bodies, it bumps into those pictures: it stops where the server's copy did not, and the next
snapshot rolls it back. Two honest options:

* **The player does not collide with the bodies; the bodies collide with the player.** Leave the bodies'
  layer out of the player's `collision_mask` and keep the player's layer in the bodies' mask. The player's
  movement is then exact on the owner (no rollback), the server's player pushes the server's bodies, and
  the owner sees them react one round trip plus the jitter buffer later: contact on the owner is visual
  only. The Ballpit paddle does this (5 to 6 ticks from the server's push to the owner's screen over a
  50 ms link, zero rollbacks).
* **`client_owned` bodies or players** (page 7): the owner simulates, the server trusts. No latency for the
  owner's own entity, but a modified client can put it anywhere.

Handing a touched body to the pushing client for a while, so the push is immediate and still arbitrated by
the server, is WP15 (cooperative physics ownership, after v1).

## Body prediction: showing a body where it is now

A replica normally shows the server's past (`interpolate`, the default). `body_prediction` chooses what a
client shows instead (docs/PLAN_NETCODE.md 12.4, D26, D27):

| `body_prediction` | what the replica shows |
|---|---|
| `interpolate` (default) | the server's past, between two snapshots: exact, late by half a round trip plus the jitter buffer |
| `dead_reckoning` | the newest snapshot carried forward to the present by its velocities, gravity and damping |
| `custom` | the same starting point, advanced by the body script's `_net_predict_body(delta)` |

It is the client's display choice: nothing changes on the wire or on the server, and no script needs
changing except for `custom`. Set it per body (`NetEntity.body_prediction`, or `Net.configure(body,
{"body_prediction": "dead_reckoning"})`, which wins over the `NetEntity`), per manager
(`NetManager.body_prediction`) or per project (`gdnet/physics/body_prediction`). A body whose script defines
`_net_predict_body` and has no setting of its own uses `custom`, even when its manager or the project says
otherwise; `custom` on a body without the hook means `dead_reckoning`. Changes apply at the next frame.
`Net.body_prediction_of(node)` returns the effective choice (`NetEntity.BODY_PREDICTION_INTERPOLATE`,
`_DEAD_RECKONING`, `_CUSTOM`), and the overlay labels a predicted body with its mode.

It applies only where the body is a frozen replica, which depends on its physics mode (below): with
`server`, on every client; with `owner`, on every client but the owner (the server applies the owner's
states as they arrive and predicts nothing); with `simulated`, nowhere (every peer runs the body, and the
snapshots correct it instead). Where the peer simulates the body itself, `Net.body_prediction_of` returns
`NetEntity.BODY_PREDICTION_NONE` and the overlay shows no mode. A mode or owner change moves a body into
or out of prediction at once: the change carries the entity's full state, so the new role starts from the
server's values even for fields that do not change (a body at constant velocity), and a moving body
does not jump (measured at 150 px/s over a 100 ms link: no frame moved by more than its travel plus
1.6 px, except the entry into `simulated`, where the body slides to the client's predicted present,
first by 15% of that gap: 5 px).

```gdscript
# A thrown grenade: shown where it is, not where it was.
Net.configure($Grenade, {"body_prediction": "dead_reckoning"})
```

**What dead reckoning computes.** It takes the newest snapshot's `position`, rotation, `linear_velocity`
and `angular_velocity` and steps them one physics tick at a time, as Godot's engines do for a body in free
flight: `v *= max(0, 1 - linear_damp * dt)`, then `v += gravity * gravity_scale * dt`, then `x += v * dt`
(Jolt adds gravity before damping; the rotation is advanced by `angular_velocity * dt` the same way). The
time it steps over (the *horizon*) is the client's estimate of the server's present tick minus the
snapshot's tick: about half the round trip plus the snapshot's age, capped at
`gdnet/physics/predict_max_ms` (250). Gravity and damping are the space's defaults (the project's
`physics/2d|3d/default_*`) and the replica's own `gravity_scale` and `linear_damp` / `angular_damp`
values, which the default profiles do not replicate: keep them in the scene, or add them to the profile
if a script changes them. `Area2D/3D` gravity and damping overrides are not applied; use `custom` where
they matter. A body resting or sliding on something (its velocity stopped following gravity) is predicted
without gravity, so a sliding crate does not sink into the floor, and a body at rest or asleep is not
predicted at all: a sleeping pile costs nothing.

Over a 100 ms link a thrown ball shown with `dead_reckoning` stays within one tick's travel of the server's
present position on every frame (gate G13; measured worst 2.7 px against 4.9 px in 2D, 2.8 cm against 5.1
cm in 3D), where the interpolated one trails by about 30 px (1.8 times its velocity times the one-way
delay). A predicted player that walks into a dead-reckoned puck meets it 4 to 5 px from where the server
has it, against 30 px when the puck is interpolated: the predicted pose is also where the frozen body's
collider is (`tests/integration/test_net_body_prediction*.gd`).

**What it cannot know.** Dead reckoning knows nothing of a collision the server has not reported yet: a
ball predicted towards a wall passes into it until the snapshot of the bounce arrives, then bends back.
Each new snapshot is compared with what the prediction showed; the difference becomes an offset that
decays by `gdnet/physics/correction_decay` per 60 Hz frame, so the body bends back instead of
jumping, and snaps beyond the property's `teleport_distance` (page 5). Differences above
`gdnet/physics/correction_position` or `correction_angle` count as corrections in `stats()`. These three
settings are shared with `simulated` corrections and read the same way: see "Correction settings" at the
end of "Physics modes". Measured: a ball hitting the floor at 775 px/s passed it by about 64 px (less
than speed times the 250 ms cap, 194 px) and its largest step in a frame was 6 px. A body that starts
moving jumps ahead once by its velocity times the horizon (the interpolated pose was only late); one
whose mode changes while it moves blends over.

**`custom`.** Before each call the body's `position`, rotation, `linear_velocity` and `angular_velocity`
hold the state to advance (the newest snapshot at first); the hook changes them as its own movement code
would over `delta` seconds, and gdnet shows the result. `Net.dead_reckon(body, delta)` is the built-in
step, if the hook wants it:

```gdscript
func _net_predict_body(delta: float) -> void:
	Net.dead_reckon(self, delta)                       # the default step; optional
	linear_velocity += -transform.basis.z * throttle * THRUST * delta
	position += -transform.basis.z * throttle * THRUST * delta * delta * 0.5
```

The hook is called for each tick of the horizon once (`delta` is one tick) and once per frame with the
fraction of a tick that is left; a new snapshot starts it again from the snapshot. That is one or two calls
on most frames and a few more on a frame a snapshot arrives (measured: 3.5 calls a frame at 145 frames
per second over a 100 ms link). The hook should change only those four properties (it may read anything):
the fraction's call is not kept.

**Choosing.** Keep `interpolate` for piles and anything that collides all the time: dead reckoning
overshoots at every contact (D27). Use `dead_reckoning` for thrown and flying things, and `custom` for
bodies with their own propulsion. A `simulated` physics mode (WP19) runs real local physics instead, and
ignores `body_prediction`. `stats()` reports `predicted_bodies` (moving bodies predicted in the last
frame), `prediction_replicas` (the frozen body replicas body prediction may show),
`prediction_corrections(_per_s)` and `prediction_horizon_ms`.

**Moving a frozen body every frame.** Replicas use Godot's default `FREEZE_MODE_STATIC`: setting the
position teleports the collider (it is not swept), and the physics server wakes the bodies it touches,
exactly as with interpolation, which moves replicas every frame too. A fast predicted body can therefore
pass through a thin collider between two frames; it is a picture of the server's body, which collides
for real.

## Physics modes

Who simulates a body is a choice per entity (WP19, docs/PLAN_NETCODE.md 12.4), with no script change:
`NetEntity.physics`, the `configure()` key `"physics"`, `NetManager.physics` for a manager's default,
and the project setting `gdnet/physics/mode` (default `server`). The first that is not `default` wins. It
applies to entities with a `RigidBody2D` / `RigidBody3D` part (subclasses such as `VehicleBody3D`
included); an entity without one ignores it. Set it on the server at any time (the property,
`Net.set_physics(node, "simulated")`) and every peer follows at the next tick; `Net.physics_of(node)`
says what a peer holds.

| mode | who simulates | the other peers |
|---|---|---|
| `server` | the server | the body is frozen and interpolated (everything above) |
| `owner` | the entity's owner (a client, or the server when it owns it) | frozen and interpolated, the server included; the owner's body is live, wakes and sleeps as usual, and moves on the tick of its input |
| `handoff` | not built yet: behaves as `server`, with a one-time warning | |
| `simulated` | every peer, the server's result is the reference | the client's body is live; snapshots correct it |

`owner` is `client_owned` for bodies (page 7): `physics = owner` and `client_owned = true` on a body are
the same thing (a body with `client_owned = true` is `owner` whatever its option says). Changing the owner
moves the simulation: the old owner's copy freezes, the new owner's thaws with the velocities it was
showing. When a seat drops (page 13), its bodies freeze in place on the server until it rejoins or its
seat is released. The overlay names the peer simulating an `owner` body (`owner-sim <peer>`).

The other clients see an `owner` body through the server: the server applies the owner's states as they
arrive, and relays them in its snapshots. With `body_prediction = dead_reckoning` on another client the
body is shown at the server's present, like a `server` body: measured over 100 ms links, a coasting
vehicle within 1.4 ticks of the server's copy (9.5 ms on average), and behind its owner's own present by
half the owner's round trip plus a few ticks (68 to 92 ms; the bound is 50 ms plus three ticks). Dead
reckoning does not know the owner's input: while the owner drives, the other client trails the server's
copy by about 50 ms (a `custom` hook can model the thrust, as in the `custom` example above).

**`simulated`.** The client's body runs in the client's *predicted* timeline, the moment its own
predicted player is at, so a player and the bodies it touches meet where they are: a player pushing a
crate moves it in the tick of contact, and a player riding a platform rides the live platform. Each
snapshot is compared with where the local body was at that snapshot's tick (both carried to the present
at their velocities), so motion both sides share is never corrected, whatever the latency. A difference
below the thresholds is left alone on a resting body (a pile stays asleep) and refined quietly on a
moving one; above them it becomes a decaying offset on position and rotation and the velocities are set
to the server's; beyond `gdnet/interpolation/teleport_distance` the body snaps. During a rollback replay
the bodies are not stepped (Godot cannot step physics on demand): each replayed tick puts them where they
were recorded at that tick, and the present comes back afterwards, so a replayed push does not count
twice. The server never corrects its own bodies, and snapshots cost the same in every mode.

**Correction settings**, shared by `simulated` corrections and body prediction (one reader, so both read
them the same way), read at each use: `gdnet/physics/correction_position` (metres, default 0.02; 2 px in 2D
at 100 px per metre, 2 cm in 3D), `gdnet/physics/correction_angle` (degrees, default 3),
`gdnet/physics/correction_decay` (the share of a correction left after one 60 Hz tick, default 0.85) and, for
`simulated` only,
`gdnet/physics/correction_rest_speed` (metres per second, default 0.15: a server sample slower than this
counts as at rest; keep it above Godot's sleep threshold). `NetSession.stats()` reports
`sim_corrections` (and `_per_s`), `sim_snaps`, `sim_correcting` and `sim_bodies` on a client.

What it costs and where it shows: a push only the server saw (its player, another client) reaches the
client late and the body slides to the server's answer (measured: a crate pushed to 3 m/s slides 0.4 m;
the client's largest step is 3x the server's own, it settles within 0.1 px). Bodies that settle while a
client watches take longer to fall asleep there (a 40-crate pile: about 10 s on the client against 1 s
on the server); a pile the client joins at rest sleeps at once. Setting a simulating `RigidBody`'s
`position` in a script does not reach the physics server in Godot 4.6 (the next physics sync puts it
back): move such a body with `PhysicsServer2D/3D.body_set_state(rid, BODY_STATE_TRANSFORM, ...)`, as
gdnet's corrections do, and call `Net.touch(node)` when it was asleep.

## Sleeping bodies cost nothing

The server records a hot field only when its *quantised* encoding changes. A body Godot put to sleep does
not move: its position, rotation and velocities keep exactly the values they had, so nothing is recorded,
nothing is pending, and no snapshot names it. Hash telemetry (page 8) resends an unchanged entity's hash
only every `gdnet/debug/hash_refresh_s` (10 s). Measured with the Ballpit demo: 300 sleeping balls, zero
recorded changes and zero snapshot bytes over a second; everything a client receives is the pings
(56 B/s).

Sleeping bodies cost no CPU either: a sleeping body is off the server's hot list, so its position,
rotation and velocities are not even read until its `sleeping_state_changed` signal reports it awake,
and it is sent in the tick it wakes (page 3, "Polling cost"). Its cold properties (and whether it still
sleeps and is not frozen, which Godot does not signal when a script sets them) are still read every
`gdnet/sync/resting_poll_ticks` (24 ticks, 0.4 s at 60; `Net.touch(body)` reads them at once). The meter's `poll` line counts the hot, idle and sleeping entities.

Two practical notes. A body that rolls very slowly on a flat floor can doze off and wake again for
seconds, costing a few bytes each time it moves; a little `angular_damp` (the Ballpit balls use 3) brings
it to rest. And a kinematic body (a paddle) resting inside a pile keeps nudging the bodies it overlaps,
so they never sleep. No dead band is applied: Godot does not jitter a sleeping body, so none is needed.

The same holds in 3D: a pile of 40 `RigidBody3D` boxes on a `StaticBody3D` floor sleeps about 1.3 s after
it is dropped, the client's copies are within half a `mm` step (0.5 mm per axis) and 1.5e-4 rad of the
server's, and asleep the pile costs zero snapshot bytes (`tests/integration/test_net_3d_physics.gd`).

### The 3D physics engine

This project does not set `physics/3d/physics_engine`, so it runs on `DEFAULT`, which in Godot 4.6 is
GodotPhysics3D (a game picks Jolt with `physics/3d/physics_engine = "Jolt Physics"`). gdnet does not
depend on the engine: every 3D test passes on both. Two differences show in them. Jolt brings the
test's pile to sleep sooner (about 1.0 s against 1.3 s) and its body updates are a little larger while
they fall (18.5 against 17.8 bytes), which is the engines' different motion, not gdnet. And a
`CharacterBody3D` standing still on a body reports that body as a slide collision under GodotPhysics3D
and nothing under Jolt, so under Jolt the body a still player stands on interacts through the radius
only (it is asleep and costs nothing either way); once the player moves, both report what it touches.

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
receives no snapshots, and a peer without a player costs nothing. A manager with a negative radius has its
positions skipped. To trade reaction for time, `gdnet/budget/interaction_every_ticks` (default 1) runs the
pass every N server ticks and keeps the last sets in between: at 4 the demo's test measures 0.11 ms per
tick instead of 0.41.

Choose the radius by the budget: every interacting body that moves costs a full update each tick. In the
Ballpit demo a body update is about 15.8 bytes, so the halved 16 KB/s budget carries about 17 updates a
tick; a 50 px radius around the paddle keeps the interacting set below that, and those balls update on
every tick they move.

```gdscript
NetSession.interacting(peer)          # the ids marked this tick (server)
NetSession.entity_sends(node, peer)   # how many snapshots carried that entity to that peer (server)
```

## The budget and degradation

`Net.budget_bytes_per_s` (or `NetSession.budget_bytes_per_s`; both apply to a running session at once;
the project setting
`gdnet/budget/bytes_per_second`, 32,768) is a token bucket per client: it fills by budget / tick rate
each tick and holds at most two MTUs of slack, so the average stays within the budget while a full MTU is
still possible now and then. When more is pending than fits, the low-weight entities wait: they
**degrade** (update less often) instead of the budget being exceeded.

The degradation counter says how much: every tick, the entities with pending fields that a snapshot left
out entirely are counted as *deferred entities*, and every pending field a snapshot did not carry
(left-out entities and the rest of partly sent ones) as *deferred fields*. Measured with 300 shaken balls
at 16 KB/s: 16,387 B/s of snapshots, about 17,200 deferred entities per second, interacting balls sent on
every tick they moved and the others on 3 % of theirs.

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
varint (1 byte) and the dirty masks (9 bits for a ball). About 15.8 bytes in all (20.6 before decision D17
made 2D `mm` a 0.1 px step instead of 0.001 px); 300 moving bodies fill MTU-sized snapshots to within 24
bytes. Two levers exist if that matters for a game: a coarser quantiser (`cm` is a 1 px step in 2D and
saves about a byte per axis for fast or far-flung bodies) through `Net.configure` or a profile, and fewer
hot properties (a body whose `scale` never changes costs nothing for it: only changed fields travel).

A `RigidBody3D` update is about the same size: measured over the first quarter second of the 3D pile's
fall, 17.8 bytes per body update (position and both velocities at `mm`, a metre being 1,000 steps, so
1 to 2 bytes per axis for values under 8 m and 8 m/s; the quaternion's 47 bits; the same entry header).

## Hash telemetry with many bodies

Hash telemetry samples `gdnet/debug/hash_sample` entities a tick (16). With hundreds of moving bodies
that can be a noticeable share of a small budget, so a busy scene lowers it: `Net.hash_sample = 2` (every
session that has no value of its own, at its next tick), or per session
`Net.session_for(self).hash_sample = 2`. The Ballpit demo does the latter; its channel 2 (hashes and
pings) stays at about 88 B/s with 300 shaken balls, 0.5 % of a 16 KB/s budget.

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

On every peer that is neither the server nor the owner the body is a **replica**: gdnet sets `freeze =
true` once, when the replica is created, and turns off physics processing of its parts (page 4). A frozen
body in the default `FREEZE_MODE_STATIC` is a static collider: other bodies collide with it, it never
moves by itself, and the interpolated snapshots move it. Its scripts' `_process` still runs (visual code
keeps working).

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

Tuning, read at each use: `gdnet/physics/correction_position` (metres, default 0.02; 2 px in 2D at 100
px per metre), `gdnet/physics/correction_angle` (degrees, default 3), `gdnet/physics/correction_decay`
(the share of a correction left after one 60 Hz tick, default 0.85) and
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
and it is sent in the tick it wakes (page 3, "Polling cost"). Its cold properties are still read every
`cold_poll_ticks`. The meter's `poll` line counts the hot, idle and sleeping entities.

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

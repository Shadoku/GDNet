# 4. A predicted player: `Net.input(self)`

A player entity becomes client-predicted with two changes: give it an owner, and read its input through
`Net.input(self)` instead of `Input`. Its ordinary `_physics_process` stays the place for movement code:
it is the tick on the server and on the owner, it is replayed after a correction, and it does not run on
the other clients at all. This page is the rung 1 tutorial of `docs/PLAN_NETCODE.md`: the before and
after, what `Net.input` returns where, who simulates, `Net.is_fresh()`, rollback, the engine-state
caveat, input delay and the settings.

What is not here yet: behaviour calls (`Net.sync`) are WP6; changing who may take an entity (`transfer`)
and trusting a client's own state (`client_owned`) are WP7. How the other clients show the player
(interpolation) and how the owner blends a correction is `05_interpolation.md`.

## Before and after

Single player:

```gdscript
extends CharacterBody2D
const SPEED := 200.0

func _physics_process(delta: float) -> void:
	velocity = Input.get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if velocity.length() > 0.0 and not $Steps.playing:
		$Steps.play()
```

Multiplayer:

```gdscript
extends CharacterBody2D
const SPEED := 200.0

func _physics_process(delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if Net.is_fresh() and velocity.length() > 0.0 and not $Steps.playing:
		$Steps.play()
```

and, on the server, wherever the player is spawned:

```gdscript
Net.spawn_for(peer_id, player_scene, $NetManager)   # or add_child(player) then Net.set_owner(player, peer_id)
```

That is the whole change (`tests/fixtures/net/predicted_player.gd` is this script, plus counters the
tests read; `tests/integration/test_net_prediction.gd` runs it). The script still works offline and as the
host's own player, unchanged.

### In 3D

The same two changes make a `CharacterBody3D` predicted. With gravity, a jump and facing the way it
walks:

```gdscript
extends CharacterBody3D
const SPEED := 4.0
const JUMP_SPEED := 4.5
const GRAVITY := 9.8

func _physics_process(delta: float) -> void:
	var input := Net.input(self)
	var dir := input.get_vector("left", "right", "up", "down")
	velocity.x = dir.x * SPEED
	velocity.z = dir.y * SPEED
	if dir != Vector2.ZERO:
		rotation.y = atan2(-dir.x, -dir.y)
	if is_on_floor():
		if input.is_action_just_pressed("jump"):
			velocity.y = JUMP_SPEED
	else:
		velocity.y -= GRAVITY * delta
	move_and_slide()
```

(`tests/fixtures/net3d/player_3d.gd`, run by `tests/integration/test_net_3d.gd` in both host modes.) The
owner moves on the tick after a press, the server converges within one `mm` step (0.001 m per component,
the 3D tolerance) about 9 ticks after the release over a 100 ms round trip, a server impulse causes
exactly one rollback, and walking, turning and jumping cause none. `rotation.y` is fine to write: gdnet
syncs and compares the `quaternion` it produces, not the Euler angles. A player away from its seat
(page 13) gets neutral input, so it stops walking and still falls.

## What happens each tick

1. The owner's `NetManager` samples the actions of the project `InputMap` from the real `Input` (a bitset
   of pressed actions plus a byte for each analog strength, typically 2 to 6 bytes), runs
   `_net_input()` if a part defines it, stores the frame and sends the last three frames to the server
   every tick (two consecutive losses cost nothing).
2. The owner runs `_physics_process` at once with that frame: the player moves on the very next tick
   after a key press, without waiting for the server (prediction). It records the player's hot state
   (position, rotation, scale, velocity) for that tick.
3. The server runs the same `_physics_process` with the frame the owner sent for that tick. When a frame
   is lost or late it repeats the last one (`stats()["inputs_repeated"]` counts those ticks).
4. The server's snapshots come back. The owner does not snap to them (they are the past: a round trip
   old); it compares the server's state for tick T with what it recorded for T. When they agree (within
   each property's tolerance) nothing happens. When they differ, it **rolls back**: restores the server's
   state at T and runs `_physics_process` again for every tick since T, with the frames it used then.
5. The other clients run nothing: their copy of the player shows the server's state.

The owner predicts the *server's* tick: it runs ahead of the server by half the round trip plus a margin
(`stats()["prediction_lead"]`), and stamps each frame with the server tick it is meant for, so the frame
arrives before the server simulates that tick. With a 100 ms round trip at 60 Hz the lead is about
8 ticks, and the server reaches the owner's position about 8 ticks after the owner stops.

## `Net.input(node)`

It returns the `NetInput` of the entity `node` belongs to (any part works; gdnet walks up to the entity
root). It mirrors `Input`'s query API:

| method | |
|---|---|
| `is_action_pressed(name)` | pressed in this tick's frame |
| `is_action_just_pressed(name)`, `is_action_just_released(name)` | against the frame the tick before used; identical on the owner, on the server and in a replay |
| `get_action_strength(name)` | 0..1, the action's deadzone applied |
| `get_axis(negative, positive)`, `get_vector(neg_x, pos_x, neg_y, pos_y, deadzone = -1.0)` | as `Input` |
| `get(name)`, `set(name, value)` | custom values (an aim position, a selected slot); numbers, vectors, colours, strings; objects cannot travel |
| `actions()`, `tick` | the actions it carries; the tick it is for |
| `is_live()` | true while it reads the real `Input` |

What it reads depends on where it runs:

| where | `Net.input(self)` reads |
|---|---|
| the owner (a client) | the frame it sampled this tick (with `input_delay_ticks`, the frame sampled that many ticks ago; during a replay, the frame of the replayed tick) |
| the server, for an entity a client owns | the frame that client sent for this tick, or its last one repeated |
| an entity the server owns (on any peer), offline, an unmanaged node | the real `Input`: single-player code is unchanged |
| another client's copy | an empty frame (its `_physics_process` does not run anyway) |

Custom values are written in `_net_input()`, which runs on the owner right after sampling:

```gdscript
func _net_input() -> void:
	Net.input(self).set("aim", get_global_mouse_position())

func _physics_process(delta: float) -> void:
	var aim: Vector2 = Net.input(self).get("aim")    # the same value on the owner and the server
```

`set()` only does something on the owner in a fresh tick; elsewhere it is ignored. A `NetEntity`'s
`input_properties` (`prop` or `Part:prop`) travel the same way without code: read on the owner after
`_net_input()`, written on the server before the tick.

Which actions travel: every action of the project `InputMap` except `ui_*` (D15), or the project setting
`gdnet/input/actions`, or a manager's `input_actions`; a `NetEntity`'s `input_actions` narrows the list
for one entity. The list is part of the scene's property table, so peers with different input maps are
told so at the first spawn.

`Input.` is still legal, but inside an owned entity's tick it reads the local keyboard, which on the
server is the server's: the copies diverge. In a debug build gdnet scans the scripts of every entity that
gets a client as its owner and warns once per script when `Input.` appears inside `_physics_process` or
`_net_tick` (`gdnet/debug/warn_on_global_input`, default on; `Net.global_input_warnings()` lists them).

## Who simulates

| peer | the entity's `_physics_process` |
|---|---|
| the server | runs (it is authoritative) |
| the owner | runs (prediction), and is replayed after a correction |
| any other client | disabled with `set_physics_process(false)`: the copy shows the server's state |

`_process` is never touched, so visual code keeps running everywhere. gdnet re-applies the rule whenever
ownership changes (`Net.set_owner` on the server): the old owner stops simulating, the new one starts,
and its prediction history starts empty. Opt-outs on a `NetEntity`: `simulate = everywhere` keeps
`_physics_process` running on every peer (a cosmetic local simulation); `predict = disabled` makes the
owner not predict at all (it sends its input and shows the server's state, like the other clients).

`_net_tick(delta, tick, is_fresh)` on a part replaces `_physics_process` as the step: where the entity is
simulated, gdnet disables that part's `_physics_process` and calls `_net_tick` from its own step instead
(before the other nodes' `_physics_process`, in tree order), with the tick it simulates and whether this
is a fresh tick. Use it when the code needs to know the tick or must behave differently in a replay.

## `Net.is_fresh()` and one-shot effects

During a replay the same `_physics_process` runs again for ticks the player has already seen.
Movement must run again (that is the point); a footstep sound, a muzzle flash or a particle burst must
not. `Net.is_fresh()` is false during a replay and true otherwise, so guard one-shots with it, as the
after script does for `$Steps.play()`. Just-pressed edges are recomputed from the stored frames, so
`is_action_just_pressed("jump")` is true again in the replayed tick of the press: count jumps with
`Net.is_fresh() and Net.input(self).is_action_just_pressed("jump")` if the count matters.

## Rollback

A rollback happens when the server's state for a tick differs from the owner's prediction beyond the
property's tolerance: 1 mm for positions and vectors (0.001 in 3D, 0.1 px in 2D: one `mm` step, decision
D17), 1e-4 for floats, exact
for everything else (change it with `Net.configure(node, {"tolerance": ...})` or a profile entry). Typical
causes: the server moved the player (a teleport, a push), the server saw something the owner did not, or
input arrived too late. The owner restores the confirmed state, replays to the present and emits
`rolled_back(from_tick, to_tick)` on the entity's `NetEntity` (and `rolled_back(node, from, to)` on the
session and `Net`). `NetEntity.predict`: `check` (default), `always` (roll back at every confirmation; for
debugging) or `disabled` (no prediction). `stats()` counts `rollbacks`, `rollbacks_per_s` and
`replayed_ticks`.

Every predicted entity of the owner is restored and replayed together, so two entities a player owns
stay consistent with each other.

**The engine-state caveat.** gdnet restores the properties it replicates and calls `_physics_process`
again; it cannot rewind the physics server. Queries such as `is_on_floor()`, `get_slide_collision()` or
an `Area2D`'s overlaps reflect the last real step until the replayed `move_and_slide()` updates them, so
during a replay they can be stale (netfox has the same limit). Derive such values from state (a
`grounded` variable you set after `move_and_slide()`, added as hot state with `Net.configure(node,
{"state": ["+grounded"], "send": {"grounded": "hot"}})`: only hot properties are recorded and restored),
or use `_net_tick` and treat `is_fresh == false` specially.

## Input delay

`Net.input_delay_ticks` (default 0) makes the owner apply its input that many ticks later and stamp it
accordingly: the player reacts later on screen, but the server receives every frame well before it is
needed, so fewer predictions are wrong. `Net.adaptive_delay` (experimental, D10, default off) ramps an
extra delay in only while an interaction is flagged (the hook later packages drive) and back out
gradually afterwards.

## Settings

| setting | default | |
|---|---|---|
| `gdnet/prediction/max_rollback_ticks` | 20 (a third of a second above 60 Hz) | ticks of history per predicted entity; a confirmation older than that is not compared, except one from before the owner's first predicted tick (a player the server moved right after spawning it), which is compared at that tick |
| `gdnet/prediction/quantize_state` | true | predicted entities start each tick from their positions and velocities as the wire carries them, on the server and the owner (page 10, "Pushes and the physics space") |
| `gdnet/input/actions` | empty (every action but `ui_*`) | the actions owned entities send |
| `gdnet/debug/warn_on_global_input` | true | the `Input.` warning (debug builds) |
| `Net.input_delay_ticks` | 0 | see above |
| `Net.adaptive_delay` | false | see above |

## Testing a predicted entity

Headless tests drive the real `Input` (`Input.action_press("right")`): the owning client branch samples
it, the server branch uses the frames it receives. `test_net_prediction.gd` measures the convergence at
30, 60 and 120 Hz (gate G5), a server impulse (exactly one rollback), `is_fresh` across a replay, replicas
not simulating, `_net_tick`, input delay, `predict = disabled`, the warning and two lost inputs out of
three. Give test players `collision_layer = 0` and `collision_mask = 0`: the branches share one physics
space, and the copies of a player would otherwise collide with each other. A 3D player that walks on a
floor keeps `collision_layer = 0` and masks only the floor's layer; one floor outside the branches serves
every copy (`test_net_3d.gd`).

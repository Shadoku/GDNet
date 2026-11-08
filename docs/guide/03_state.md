# 3. State: what syncs, and how to change it

An entity's properties replicate from the server to every client with no code: gdnet knows what a
`CharacterBody2D`, an `AnimationPlayer` or an `@export var open: bool` is, reads those properties on the
server every tick, and sends what changed. This page covers what syncs by default, hot and cold
properties, quantisation, the bandwidth budget, and the two ways to correct the inference
(`Net.configure()` and a `NetEntity` node).

Replicas show hot transforms interpolated (WP5, [05_interpolation.md](05_interpolation.md)); every other
property snaps to the latest value. Client input and
prediction (`Net.input(self)`) are in [04_prediction.md](04_prediction.md). Client-owned state (a client moving its own entity and the server
accepting it) is WP7: for now the server is the only source of state, and a client's own spawn
(`spawn_policy = owner_allowed`) keeps its local state.

## What syncs by default

Each part of an entity (the instance root and every node of its scene) gets a property list from the
**profiles**: gdnet walks the part's class hierarchy, most-derived class first, and unites the entries it
finds; a property the node does not have is skipped. Then it adds **every `@export` variable** of the
part's script, as a cold property (exports are the node's public API; plain `var`s are not synced unless
you add them). The shipped table (`addons/gdnet/default_profiles.tres`):

| class | properties (hot = every tick; cold = every `cold_poll_ticks`) |
|---|---|
| `Node2D` | hot: `position` (mm), `rotation` (deg), `scale` (f16); cold: `visible`, `modulate`, `z_index` |
| `Node3D` | hot: `position` (mm), `quaternion` (smallest3), `scale` (f16); cold: `visible` |
| `CharacterBody2D/3D` | + hot: `velocity` (mm, not interpolated: state for prediction) |
| `RigidBody2D/3D` | + hot: `linear_velocity`, `angular_velocity`; replicas get `freeze = true` |
| `AnimatedSprite2D/3D` | cold: `animation`, `frame`, `flip_h`, `flip_v`, `speed_scale`, `playing` (via `is_playing()`, applied with `play()` / `pause()`) |
| `AnimationPlayer` | cold: `current_animation`, `speed_scale`, and `current_animation_position` resent when the animation changes and once a second (applied with `seek()`) |
| `Sprite2D/3D`, `TextureRect` | cold: `frame`, `flip_h`, `flip_v`, `texture` (as its resource path) |
| `Label`, `RichTextLabel` | cold: `text` |
| `CanvasItem`, `Control` | cold: `visible`, `modulate` |
| `CollisionShape2D/3D`, `CollisionPolygon2D/3D` | cold: `disabled` |
| `Light2D` / `Light3D` | cold: `energy`, `color`, `enabled` / `light_energy`, `light_color`, `visible` |
| `GPUParticles*`, `CPUParticles*` | cold: `emitting` |
| `AudioStreamPlayer*`, `Camera2D/3D`, `Timer`, `Tween` | nothing (one-shots go through `Net.sync`, WP6) |
| any script | cold: every `@export` variable (`gdnet/sync/export_vars`, default on) |

A class mapped to an empty list syncs nothing and stops the walk: a `Camera2D` does not inherit
`Node2D`'s position. So a plain level, dropped under a `NetManager` with no script edits, replicates its
player's position and walk animation, its platform's `AnimationPlayer`, its label and its exported
counters (gate G7, `tests/integration/test_net_state.gd`, scene `tests/fixtures/zero_change/`).

Supported types: `bool`, `int`, `float`, `Vector2/3/4`, `Vector2i/3i`, `Quaternion`, `Transform2D/3D`,
`Color`, `String`, `StringName`, `NodePath`, resources (by path; an embedded resource has no path and
syncs as none) and, for cold properties only, anything else through `var_to_bytes` (arrays,
dictionaries). Node and object references never travel.

## Hot and cold

* **Hot** properties are read every physics tick on the server and sent whenever their quantised value
  changed (transforms, velocities).
* **Cold** properties are read every `cold_poll_ticks` (default 6, 10 Hz at 60 ticks), staggered across
  entities, and sent when they changed.

Either way only changes travel: a value is compared by its quantised encoding, so an entity at rest
costs nothing, and 100 idle entities cost only the session's pings (gate G4: about 50 B/s measured).
Delivery is most-recent-state: snapshots are unreliable, the client acknowledges what it received, a lost
snapshot's fields are sent again with their *current* values, and a client that stops acknowledging for
half a second gets full state. Values too large for a snapshot (over 1,000 bytes) and properties marked
`reliable` go on the reliable channel.

## Quantisation

| `quantize` | encoding | precision |
|---|---|---|
| `raw` | f32 per vector component, f64 for a float | exact |
| `mm` | fixed point, 0.001 units, zigzag varint | 0.0005 |
| `cm` | fixed point, 0.01 units | 0.005 |
| `deg` | an angle wrapped to one turn, 16 bits | 0.0055 degrees (replicas get the angle in [-PI, PI)) |
| `smallest3` | a unit quaternion's three smallest components, 47 bits | about 2e-5 |
| `f16` | half precision per component | about 1/2048 relative |

Defaults: hot vectors and transforms `mm`, quaternions `smallest3`, hot colours `f16`, everything else
`raw`. For a 2D game in pixels `cm` is plenty: `Net.configure(node, {"quantize": {"position": "cm"}})`.

## Bandwidth

Each client has a snapshot budget (`gdnet/budget/bytes_per_second`, default 32,768; `Net.budget_bytes_per_s`,
or `budget_bytes_per_s` on a `NetSession`, live). A snapshot never exceeds 1,200 bytes. When more changed
than fits, a priority accumulator decides: every tick each changed entity adds its weight (1,000,000 for
the client's own entity, 100 for one interacting with it, 1 otherwise, times its `priority`); the
snapshot takes the highest sums and the entities it sent drop to zero. An entity of priority 10 is sent
about twice as often as one of priority 5, the client's own entity every time, and nothing exceeds the
budget: with 100 moving entities and the budget halved, the ten of priority 10 updated every tick and the
other ninety about every seventh (`test_g4_a_halved_budget_updates_far_entities_less_often`). Distance and
rooms (interest management) come with WP7 and WP13.

`NetSession.stats()` reports bytes sent and received per channel per second, entity count, the polling
cost per tick (`poll_ms`, `poll_ms_avg`) and the snapshot counters; WP8's meter shows them. Measured
polling cost: 1,000 entities with 8 hot properties each take about 4 ms per tick (release build, the
benchmark test prints it).

## Correcting the inference

### `Net.configure(node, options)`

```gdscript
func _ready() -> void:            # in the level, on every peer
	Net.configure($Boss, {
		"state": ["+phase", "-scale", "+Sprite:region_enabled"],   # add a plain var, drop an inferred one
		"quantize": {"position": "cm"},
		"send": {"phase": "hot"},          # promote a cold property
		"tolerance": 0.01,                 # prediction tolerance (WP4), all properties or {prop: x}
		"interpolate": {"position": true}, # (WP5)
		"priority": 10,                    # this entity's share of the budget, any time
	})
```

Paths are `prop` or `Part:prop`, relative to the node you pass. Everything but `priority` changes the
property table of the entity's **scene** (all its instances), and **the table is fixed when the scene's
first entity spawns** on that peer: call `configure()` before then (in the level's `_ready`, or on the
instance before `add_child`), the same way on every peer. A later table change is refused with an error.

### The same-table rule

Both peers derive each scene's property table themselves, from the same scene, the same profiles and the
same `configure()` calls, and compare: the table travels with the scene's first spawn, and a difference
disconnects the client with a reason naming the scene, the part and the property
(`protocol mismatch: the property table of scene res://boss.tscn differs at part ., property position
(...)`). The effective global profiles are also part of the handshake. So keep profiles, `export_vars` and
`configure()` calls identical on server and clients, and do not attach scripts at run time to nodes that
are parts of a scene.

### A `NetEntity` node

Add a `NetEntity` child to the scene's root and set, in the inspector:

* `state_properties` (`Array[NodePath]`): `position`, `Sprite:frame`, ... relative to the entity root;
* `state_options` (Dictionary): per path, `{schedule: "hot" | "cold", quantize, interpolate, tolerance,
  reliable, poll_seconds}`;
* `inherit_profile`: **false** (default) means the list **replaces** inference (only the listed
  properties sync, exports included); **true** adds the list to the inferred properties;
* `priority`, and `input_properties` (used from WP4).

A `NetEntity` added only for `owner_peer` or to split a nested instance therefore syncs nothing unless you
list properties or turn `inherit_profile` on.

### Profiles as data

`NetProfiles` is a resource: `classes` maps a class name to an array of entries
`{name, schedule?, quantize?, interpolate?, tolerance?, reliable?, poll_seconds?, remove?}`. Layers, each
`merge()`d over the previous: the shipped `default_profiles.tres`, the resource at the project setting
`gdnet/sync/profiles`, `Net.profiles`, then a `NetManager.profiles` override for that manager. An entry
replaces the entry of the same name, `remove: true` drops it, an empty array empties a class.

## Settings

| setting | default | |
|---|---|---|
| `gdnet/sync/profiles` | `""` | a `NetProfiles` `.tres` laid over the defaults |
| `gdnet/sync/cold_poll_ticks` | 6 | also `Net.cold_poll_ticks`, `NetSession.cold_poll_ticks` |
| `gdnet/sync/export_vars` | true | sync every `@export` variable (D12) |
| `gdnet/budget/bytes_per_second` | 32768 | per client; also `Net.budget_bytes_per_s`, `NetSession.budget_bytes_per_s` |

The Rust derive's `mark_dirty()` (rung 5), which would let Rust classes skip polling, is not built yet.

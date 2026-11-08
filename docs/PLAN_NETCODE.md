# Plan: a Rust GDExtension for rapid multiplayer development in Godot 4

2026-11-08, revised the same day after the owner's review (the managed layer added, then the two demo end
goals). Approved 2026-11-08, with every recommendation of section 7 taken.

**What.** A Godot 4 extension, written in Rust with gdext, that makes a single-player scene multiplayer by
placing it under one node. A `NetManager` enrols everything beneath it: scene instances become networked
entities, their transforms, physics velocities, animation state, visibility and exported variables replicate
from inferred per-class profiles, remote copies interpolate, and any public method of a managed node can be
called across the network without declaring it. A player entity becomes predicted by giving it an owner and
reading input through `Net.input(self)` instead of `Input`; its ordinary `_physics_process` is the tick.
Multiplayer-specific code is optional and small: a few verbs (`Net.sync`, `Net.server`), a few hooks
(`_net_tick`, `_net_spawned`, `_net_allow`), and an explicit `NetEntity` node or Rust derive for the cases
inference gets wrong. Underneath: server authority, client prediction with rollback of the owner's kinematic
bodies, most-recent-state deltas with a bandwidth budget, typed RPCs with return values, and the diagnostics
(meter, emulation, hashes, ownership overlay) that Godot lacks.

**Why.** The research report (`docs/reports/Rust GDExtension multiplayer design.md`, notes under
`docs/research_notes/`) found that what developers migrate toward is a library that owns the tick (FishNet,
Fusion, Netcode for Entities, netfox), and that libraries which leave the tick to the engine (Mirror, NGO,
Godot's stock `MultiplayerSynchronizer`) collect the same issue tracker: spawn-order races, jitter, host
asymmetry, untyped RPCs. Godot ships the seams to fix this from an extension (`MultiplayerAPIExtension`,
`MultiplayerPeerExtension`), but no tick loop, prediction or interpolation. Roblox is the existence proof
that "everything replicates by default, override the rare exception" is what makes multiplayer feel like
single player; nothing in the Godot ecosystem offers it. The Rust replication crates are fused with Bevy and
cannot be reused; their designs can.

**What it requires.** Godot 4.3+ (`compatibility_minimum = 4.3`), gdext 0.5.x, stable Rust for native
targets, nightly Rust and emsdk only for the web export. No new runtime service: the extension composes with
the peers Godot already has (ENet, WebRTC, WebSocket, GodotSteam, Nakama).

**What it will not do.** No deterministic lockstep or rollback over Godot physics or arbitrary GDScript; no
RigidBody rollback; no interception of arbitrary property writes or method calls (the engine has no hook,
so polling and explicit verbs it is); no CRDTs; no learned prediction; no MMO-scale interest management; no
bundled hosting, matchmaking or relay service; no persistence database; no host migration in v1; no
Rust-side Steam binding.

---

## 0. Goals, in priority order

1. **Enrol, don't declare.** A scene dropped under `NetManager` replicates with no script changes: spawn,
   transform, velocity, animation, visibility, exported variables. Declaration is the exception
   (`NetEntity`), not the entry point.
2. **The single-player path is the multiplayer path.** The same scripts run offline, as host, as client and
   as dedicated server; the host is a client of itself (loopback peer); user code never branches on
   `is_server()`; `_physics_process` stays the place for movement code.
3. **Behaviour sync without ceremony.** Any public method of a managed node is network-callable through one
   verb, with sender info and an awaitable return, with no registration on either side.
4. **One line to predict.** Giving an entity an owner and reading `Net.input(self)` instead of `Input` makes
   it client-predicted with rollback; the input map is what travels, not a hand-written input struct.
5. **No ordering traps.** Spawn, parent, initial state and the first call arrive as one message. There is no
   NodePath on the wire.
6. **Quality and diagnosability by default.** Prediction, interpolation, quantisation, budgeting and lag
   compensation have sensible values; a meter, a network emulator, per-tick hashes and an ownership overlay
   ship in the first release.
7. **Novel where the evidence supports it.** Input-map replication, adaptive local lag (FDG 2026),
   notification-based most-recent-state with acked-baseline deltas (Tribes / Fiedler), awaitable RPC returns,
   one-message atomic entity actions (lightyear).
8. **Two instructional demos are the end goals** (section 2.11, WP13 and WP14): players travelling between
   scenes, and a physics scene with hundreds of colliding bodies. Each is a worked example a reader can copy,
   and each runs headless in the test suite, so the library is not done until they are.

## 1. What the developer writes: the escalation ladder

Each rung adds one idea. Most games stop at rung 2.

### Rung 0: enrol a scene, change nothing

```
Level (Node2D)
└── NetManager            # everything beneath is managed
    ├── Player.tscn       # instance: an entity
    ├── MovingPlatform.tscn
    ├── Door.tscn
    └── Enemies (Node2D)
        └── Slime.tscn ×3
```

Every scene instance under the manager is an *entity*. The server (or the single-player loopback host)
owns them all. Their inferred properties replicate to clients, which interpolate them. Spawning is
`instantiate()` + `add_child()` on the server, despawning is `queue_free()`, and the replica appears under
the same parent with the same name and its initial state in one message. Enemy AI, the platform's
`AnimationPlayer`, the door's `open` export, the slime's `AnimatedSprite2D` frames: all of it syncs with
zero script changes, because the profiles in section 2.1 know what a `CharacterBody2D`, an
`AnimationPlayer` and an `@export var open: bool` are.

### Rung 1: a player, predicted

Before (single player):

```gdscript
extends CharacterBody2D
const SPEED := 200.0

func _physics_process(delta: float) -> void:
	velocity = Input.get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if velocity.length() > 0.0 and not $Steps.playing:
		$Steps.play()
```

After:

```gdscript
extends CharacterBody2D
const SPEED := 200.0

func _physics_process(delta: float) -> void:
	velocity = Net.input(self).get_vector("left", "right", "up", "down") * SPEED
	move_and_slide()
	if Net.is_fresh() and velocity.length() > 0.0 and not $Steps.playing:
		$Steps.play()
```

plus, wherever the player is spawned: `Net.spawn_for(peer_id, player_scene, $NetManager)` (or
`add_child()` then `Net.set_owner(player, peer_id)`). That is the whole change. The owner samples its input
map each tick and sends it; the server runs the same `_physics_process` with the owner's input; the owner
runs it immediately (prediction) and again on rollback (replay), where `Net.is_fresh()` is false; other
clients do not run it at all, they interpolate. `Input.` is still legal: it just reads the local keyboard,
which is wrong on the server, so the manager warns once when a managed script with an owner calls
`Input.get_vector` and friends inside `_physics_process` (detected by a debug-build hook, decision D14).

### Rung 2: behaviour, called across the network

Single player: `open_door(7)`. Multiplayer: `Net.sync(open_door, 7)`.

```gdscript
func open_door(id: int) -> void:          # a plain method; nothing declared
	doors[id].open = true                  # (open is an @export: it would also sync as state)
	$Sfx.play()

# From anywhere, including a client that owns the player who touched the switch:
Net.sync(open_door, 7)                     # runs on the authority, then on everyone in scope

# A request with an answer:
var granted: bool = await Net.server(try_buy, item_id)

# Validation hook, optional:
func _net_allow(method: StringName, sender: int) -> bool:
	return method != &"give_gold" or sender == 1
```

Every method of a managed node (script methods and built-in ones, so `Net.sync($Anim.play, "attack")`
works) is addressable by entity id, part path and method name hash; nothing is declared on either side.
Routing verbs: `Net.sync` (authority runs it and broadcasts; a non-authority caller forwards to the
authority, allowed by default only for the entity's owner), `Net.server` (server only, awaitable),
`Net.owner`, `Net.others`, `Net.peer(id, ...)`. Reliable and ordered after the entity's spawn, never
re-sent during rollback replay. Calls carry a trailing `NetCallInfo` (sender, tick) when the method
declares a parameter of that type. (WP6: up to eight arguments after the Callable, bound arguments
included; `Net.server` returns its `NetCall`'s `completed` signal, so `await` yields the value; the other
verbs return the `NetCall`. See `docs/guide/06_calls.md`.)

### Rung 3: correct the inference

```gdscript
# In code, once, e.g. in _ready of the level:
Net.configure($NetManager/Boss, {
	"state": ["+phase", "-scale"],          # add a non-exported var, drop an inferred property
	"quantize": {"position": "cm"},
	"priority": 10,
	"radius": 1200.0,
	"lag_compensated": true,
})
```

or add a `NetEntity` child to the scene and set the same things in the inspector (`state_properties`,
`input_properties`, options, `owner_peer`, `transfer`, `client_owned`, `simulate`, `priority`, `radius`,
`visibility_filter`, `lag_compensated`, `predict`). An entity with a `NetEntity` child uses its lists
*instead of* inference unless `inherit_profile = true`.

### Rung 4: multiplayer-specific code

Optional virtuals on any managed node, all with defaults: `_net_tick(delta, tick, is_fresh)` (replaces
`_physics_process` as the simulated step when present), `_net_input()` (write custom input values:
`Net.input(self).set("aim", get_global_mouse_position())`), `_net_spawned(info)` (after initial state,
before the first tick), `_net_despawning()`, `_net_owner_changed(old, new)`, `_net_allow(method, sender)`.
Queries: `Net.is_owner(node)`, `Net.is_authority(node)`, `Net.is_fresh()`, `Net.tick()`, `Net.rtt(peer)`.

### Rung 5: Rust

```rust
#[derive(GodotClass, NetSync)]
#[class(base = CharacterBody2D)]
struct Player {
    base: Base<CharacterBody2D>,
    #[net(state, quantize = "mm", interpolate)] position: Vector2,
    #[net(state)] velocity: Vector2,
    #[net(input)] aim: Vector2,
}
#[godot_api]
impl INetEntity for Player {
    fn net_tick(&mut self, delta: f32, tick: u32, fresh: bool) { ... }
}
```

The derive fills the same property table the manager infers or the node declares, adds `mark_dirty()` so
the Rust side needs no polling, and lets gameplay logic live in Rust where GDScript is too slow.

### What the developer sees

- **Meter** panel (toggle key): RTT, jitter, tick offset, bytes per second per channel and per entity class,
  rollbacks per second, entities in scope, budget use, cold-poll cost.
- **Network emulation**: latency, variance, loss, reorder, duplication, per direction, Unreal's presets, in
  Project Settings and at runtime.
- **Ownership overlay** (editor debugger plugin and in-game): entities tinted by owner, Roblox-style; shows
  which entities are managed and from which profile.
- **Hash telemetry**: a cheap per-tick hash per entity compared between peers, shown as a divergence count.

## 2. Architecture

Three crates in a Cargo workspace, one addon folder.

```
rust/
  netcore/        sans-IO protocol engine. No godot, no sockets, no threads, no_std-capable (alloc).
                  Tick clock, entity/part table, property tables, dirty masks, acked-baseline deltas,
                  priority accumulator, input frames, prediction histories, interpolation buffers,
                  call framing, keyed state, hashes, emulator. Compiles for wasm32-unknown-emscripten.
  gdnet/          the GDExtension (gdext): NetManager, Net singleton, NetEntity, NetInput, profiles,
                  inference, the MultiplayerAPIExtension wrapper, polling, _physics_process gating and
                  replay, interpolation on _process, the editor plugin. Depends on netcore.
  gdnet-peer/     phase 2, native only, behind a Cargo feature: a MultiplayerPeerExtension over
                  renet/renetcode (optionally quinn) with a Rust I/O thread exchanging bytes with the
                  main thread through channels, never touching Gd<T>.
addons/gdnet/     gdnet.gdextension, compiled libraries, default_profiles.tres, the meter scene, icons.
```

Working name `gdnet` (decision D1).

### 2.1 The managed layer

**Enrolment.** `NetManager` is a plain `Node`. It connects to `SceneTree.node_added` / `node_removed` and
keeps every descendant that is a scene instance root (`scene_file_path != ""`) as an *entity*; the nodes
inside an instance are its *parts*, addressed by their path relative to the instance root, which is
identical on every peer because they come from the same `PackedScene`. Nested instances are parts of the
outer entity unless they have their own `NetEntity` child or are listed in the manager's
`split_scenes`. A node under the manager that is not a scene instance and not inside one (a bare `Sprite2D`
added by code) is replicated as class name + inferred properties with a one-time warning: scenes are the
unit of composition and the recommended path. `Net.manage(node)` enrols a node outside any manager;
`Net.ignore(node)` or the group `net_local` excludes a subtree.

**Identity.** Entities get `u32` net ids allocated from per-peer ranges; a part is (entity id, part index)
where part indices come from a depth-first walk of the scene file, computed identically on all peers and
checked by hash at spawn. No NodePath travels.

**Spawn.** On the authority, an entity entering the tree under the manager produces one reliable
`EntityAction::Create { scene hash, parent entity + part, name, owner, initial state of every part }`;
clients `load().instantiate()`, set the name, add under the same parent, apply the initial state, then call
`_net_spawned`. `queue_free` produces `Destroy`; reparenting within managed space produces `Reparent`.
Late joiners receive the entity table as a burst of `Create`s and one full snapshot. A client adding an
entity under the manager is refused with a readable error unless the manager's `spawn_policy` is
`owner_allowed` (then it becomes `client_owned` by that client).

**Profiles (what syncs).** `NetProfiles` is a `Resource` (`addons/gdnet/default_profiles.tres`, overridable
in Project Settings and per manager) mapping class names to property lists with options. Inference walks a
part's class hierarchy most-derived first and unions the profiles it finds, then adds **every `@export`
variable of the part's script** (exports are Godot's public API; private vars are not synced). Defaults:

| class | properties (hot = every tick, quantised, interpolated; cold = polled every `cold_poll_ticks`, reliable on change) |
|---|---|
| `Node2D` | hot: `position`, `rotation`, `scale`; cold: `visible`, `modulate`, `z_index` |
| `Node3D` | hot: `position`, `quaternion`, `scale`; cold: `visible` |
| `CharacterBody2D/3D` | + hot: `velocity` (state for prediction) |
| `RigidBody2D/3D` | + hot: `linear_velocity`, `angular_velocity`; replicas get `freeze = true`; never predicted |
| `AnimatedSprite2D/3D` | cold: `animation`, `frame`, `flip_h`, `flip_v`, `speed_scale`, playing (via `is_playing`) |
| `AnimationPlayer` | cold: `current_animation`, `speed_scale`; playback position resent on animation change and every second |
| `Sprite2D/3D`, `TextureRect` | cold: `frame`, `flip_h`, `flip_v`, `texture` (as resource path) |
| `Label`, `RichTextLabel` | cold: `text` |
| `CanvasItem`, `Control` | cold: `visible`, `modulate` |
| `CollisionShape2D/3D`, `CollisionPolygon*` | cold: `disabled` |
| `Light2D/3D` | cold: `energy`, `color`, `enabled`/`visible` |
| `GPUParticles*`, `CPUParticles*` | cold: `emitting` |
| `AudioStreamPlayer*` | nothing (one-shots go through `Net.sync($Sfx.play)`) |
| `Camera*`, `Timer`, `Tween` | nothing |
| any script | cold: every `@export` var; `@export` vars of `Vector2/3`, `float`, `int` named like transforms are still cold unless promoted by `configure()` |

Options per property: `quantize` (`mm`, `cm` (metres in 3D, 100 px per metre in 2D: D17), `deg`, `smallest3`, `f16`, `raw`), `interpolate`
(default on for hot transforms, off for cold), `tolerance` (prediction mismatch), `send` (`hot` or `cold`),
`reliable`. Profiles are data, so a project adds its own classes or removes defaults without code.

**Dirty tracking.** Polling (the engine offers no write hook). Hot properties are read every tick; cold
properties every `cold_poll_ticks` (default 6, i.e. 10 Hz at 60) and immediately when a `Net.sync` call
touches the entity; the Rust derive's `mark_dirty()` bypasses polling. Property reads use cached
`StringName`s and `Object.get_indexed()`. WP3 begins with a benchmark (1,000 entities × 8 properties) and
the meter shows polling cost, so the cold schedule can be tuned with numbers.

**Who simulates.** Each entity has one `owner_peer` (1 = server by default). The manager calls
`set_physics_process(false)` on every part of an entity on peers that are neither the server nor the owner,
so replicas only interpolate; `_process` is left alone everywhere (visual code keeps running). Entities with
`simulate = everywhere` (cosmetic local simulations) opt out. On the server and the owner,
`_physics_process` runs normally and is the tick; when a part defines `_net_tick`, that is called instead
and `_physics_process` is disabled for it. On rollback the manager calls `_physics_process(delta)` (or
`_net_tick`) directly on the owned entity's parts in tree order for each replayed tick with `Net.is_fresh()`
false. Known limit, inherited from the engine and shared with netfox: `is_on_floor()` and other
physics-server queries are stale during replay; the tutorial says to derive such values from state or
declare them as state.

**Input (`NetInput`).** `Net.input(node)` returns the per-entity `NetInput` for the entity that owns
`node`. It mirrors `Input`'s query API: `is_action_pressed`, `is_action_just_pressed`,
`is_action_just_released`, `get_action_strength`, `get_axis`, `get_vector`, plus `get(name)` / `set(name,
value)` for custom values (aim position, selected slot). On the owner, each tick the manager samples the
actions in the entity's `input_actions` (default: every action in the project `InputMap`, excluding
`ui_*`; a bitset of pressed plus a byte per analog strength, typically 2 to 6 bytes) and the custom values,
stores them as that tick's input frame, and sends the last 3 frames per packet (redundancy). The server
uses the frame for the matching tick and repeats the last one on loss. The just-pressed / just-released
edges are computed from consecutive frames on both sides, so they are identical during replay. For an
entity owned by the server (or when offline), `Net.input(node)` reads the real `Input`, so single-player
code is unchanged. This is the design's main novelty: the **input map replicates**, so prediction needs no
input struct.

**Auto-exposed methods.** At enrolment the manager records, per part, the method names of its script
(`Script.get_script_method_list()`) and class (`ClassDB`); a call names (entity, part, method hash, args).
The receiver checks the method exists and, for calls from a non-authority, that the sender is the entity's
owner (default) or that `_net_allow(method, sender)` returns true; `Net.server` calls are accepted from any
peer and always carry the sender. Methods beginning with `_` are never exposed. Arguments are Variants
encoded with postcard (Objects, Callables and Signals refused with an error at the call site; `Resource`
arguments travel by resource path when they have one). Return values travel as `CallReturn` matched by a
request id; `await Net.server(fn, ...)` resolves to it.

### 2.2 Data flow per frame

```
_physics_process of NetManager (runs first: process priority -1000), main thread, physics tick:
  1. drain       SceneMultiplayer.peer_packet -> netcore.receive(peer, bytes)
  2. inputs      owned entities: sample the input map and custom values -> netcore.local_input(tick)
  3. reconcile   netcore reports mismatches -> restore hot state at T, replay _physics_process/_net_tick
                 for T+1..now with is_fresh = false
  4. (engine)    the parts' own _physics_process / _net_tick run as the fresh tick (server: all;
                 owner: own; replicas: disabled)
NetManager's post-tick hook (a Node at process priority +1000):
  5. record      poll hot props (and due cold props) -> netcore.record(tick) (dirty masks)
  6. send        netcore.outgoing(budget) -> SceneMultiplayer.send_bytes(peer, bytes, mode, channel)
_process (every render frame):
  7. interpolate netcore.sample(render_time) -> write hot props on replicas with decaying error offsets
```

Only step 7 runs more than once per tick; everything else is tick-locked to physics, which Godot cannot
step manually (netfox's `physics_factor` workaround is the evidence that any other cadence fights
`move_and_slide()`).

### 2.3 Transport: wrap, do not replace

`Net` installs a Rust `MultiplayerAPIExtension` around the tree's `SceneMultiplayer` with
`SceneTree.set_multiplayer()`. Every stock call (`_rpc`, `_object_configuration_add` for
`MultiplayerSpawner` / `MultiplayerSynchronizer`, `_poll`) is forwarded unchanged, so existing scenes keep
working and a project migrates one scene at a time. gdnet's protocol travels as raw bytes through
`SceneMultiplayer.send_bytes()` and the `peer_packet` signal on its own channels, over whatever
`MultiplayerPeer` the project configured: ENet, WebRTC (browser), WebSocket, GodotSteam, Nakama, without
binding any SDK. It is also the only path to the web export, where no Rust transport has been shown to run
under Emscripten.

Channels: `0` reliable ordered (entity actions, calls, keyed state), `1` unreliable (inputs, snapshots), `2`
unreliable (hashes, pings). Snapshots are filled by priority up to a 1,200-byte MTU, never fragmented.

### 2.4 Protocol (netcore)

Messages, 1-byte kind prefix, `postcard` except snapshots:

| kind | channel | content |
|---|---|---|
| `Hello` / `Welcome` | reliable | protocol hash (scene hashes, profiles, part tables), tick rate, id ranges, current tick |
| `EntityAction` | reliable | create (net id, scene hash, parent entity+part, name, owner, initial state of all parts) / destroy / reparent (also across managers, carrying the destination room) / set owner / travel (to the owning client: load this level scene, then acknowledge). Applied atomically before any later message can target the id |
| `Input` | unreliable | tick, the last 3 input frames per owned entity, bit-packed |
| `Snapshot` | unreliable | server tick, baseline tick, per entity: id, part, dirty mask, changed fields bit-packed and quantised. Most-recent-state: never retransmitted; lost fields are dirty again (Tribes) |
| `Ack` | piggybacked | highest received tick + 32-bit history; drives baselines and the priority accumulator |
| `Call` / `CallReturn` | reliable or unreliable | entity, part, method hash, verb, args, optional request id |
| `KeyedState` | reliable | path, value; full map on join |
| `Hash` | unreliable | tick, sampled per-entity 32-bit hashes |
| `Time` | unreliable | ping/pong for RTT, jitter, tick offset |

Baselines come from acks; without an ack within ~0.5 s an entity falls back to a full state.

### 2.5 Prediction and reconciliation

The server runs each entity's tick with the inputs it holds for that tick (last known input repeated on
loss). The owner runs its own entities immediately and keeps a `PredictionHistory` of hot state per tick for
`max_rollback_ticks` (20). When a snapshot confirms tick T, netcore compares confirmed and predicted values
with per-property tolerance (position 1 mm default); on mismatch (`predict = check`, the default) gdnet
restores the hot state at T and replays T+1..now with `is_fresh` false. `always` and `disabled` exist.
Replicas never simulate; they interpolate on a render timeline one adaptive jitter buffer (2 ticks, 1..6)
behind the newest snapshot, with decaying error offsets (0.95 per frame small, 0.85 large, teleport above a
threshold). RigidBodies are state-only and frozen on replicas. `Net.input_delay_ticks` (default 0) and the
experimental `adaptive_delay` (ramps delay in only during interactions, the FDG 2026 result) are properties
of `Net`.

### 2.6 Authority

One `owner_peer` per entity, server by default, replicated on change, inherited by nested entities unless
overridden, with `transfer` rules `fixed` (default), `request`, `take`. Input is accepted only from the
owner; state only from the server, except `client_owned` entities, whose owner's state is accepted and
relayed (the Fusion Shared / Roblox mode, off by default, shown in the overlay and labelled as trusting
clients). A rejected write is rolled back on the writer (Normcore).

### 2.7 Interest management and budget

Scope per client = entities whose manager's `room` is the client's room (the room of the manager holding
its player, set by travel; `Net.set_room(peer, name)` overrides it for clients without a player, e.g. spectators; `room = "*"` managers are always in scope), within
`radius` of the client's focus entity (default unlimited), passing an optional `visibility_filter`. A
per-client byte budget (default 32 KB/s) is spent by a priority accumulator: owned entity 1,000,000,
interacting 100, others 1, scaled by `priority`; sent entities reset to zero.

### 2.8 Lag compensation

`Net.rewind_query(peer, callable)` runs `callable` with every `lag_compensated` entity's collision shapes
moved to the pose `peer` saw (RTT/2 + its jitter buffer, capped at 250 ms), then restores them. Used from
the server's tick for hitscan and melee.

### 2.9 Threading and web

No `experimental-threads`; every `Gd<T>` access is on the main thread. `gdnet-peer` runs renet on a Rust
thread and passes bytes through `std::sync::mpsc`, native only. `netcore` and `gdnet` build for
`wasm32-unknown-emscripten` with gdext's nothreads recipe; browser networking is Godot's own
`WebRTCMultiplayerPeer` / `WebSocketMultiplayerPeer`.

### 2.10 Determinism (opt-in, later)

A `netcore` feature `deterministic` exposes GGRS behind `Net.deterministic = true` for simulations written in
Rust with `fixed` / `libm` math, never for GDScript or Godot physics. Ships after v1 (WP12) as an explicit
project-start decision.

### 2.11 Scenes as rooms, and travel between them

A `NetManager` has a `room` (default: the path of the scene it sits in). On the server every level that has
someone in it is loaded under one `World` node, each with its own manager; a headless server may hold
several levels at once. A client loads only the level its player is in. Interest is by room: a client
receives entities whose manager's room equals its player's room (plus entities in managers marked
`room = "*"`, for global objects), and a late joiner's burst covers its room only. The keyed state is
global, so chat and scores cross rooms.

`Net.travel(player, "res://levels/cave.tscn", spawn_point := "Entrance")` on the server: loads the target
level if it is not loaded (the manager inside it registers its room), emits a `Travel` action to the owning
client naming the scene path, then a `Reparent` of the player entity into the target manager at the spawn
point's transform. The client, on `Travel`, instantiates the level under its `World`, acknowledges, applies
the `Reparent`, and frees its previous level once none of its entities remain there. The server unloads a
level `empty_level_timeout` (default 30 s) after its last player leaves. `Net.travelled(player, from, to)`
and `_net_travelled(from, to)` fire on both sides after the reparent, where the game resets the camera.

Physics bodies in a busy scene need nothing new in the model, only the defaults to hold: server-owned
`RigidBody2D/3D` entities replicate `position`, `rotation`, `linear_velocity` and `angular_velocity` as hot
state; replicas are frozen and interpolated; a sleeping body produces no dirty fields, so a settled pile
costs nothing; the priority accumulator sends bodies near the client's player (interacting, 100) far more
often than distant ones (1) and the budget degrades distant bodies gracefully instead of exceeding it.
Fiedler-style ownership handoff for bodies a client-owned paddle touches is a follow-up (WP15), not v1.

## 3. Contracts

### 3.1 Nodes, singletons, resources (gdnet)

| class | role |
|---|---|
| `Net` (singleton) | `host(port)`, `join(address, port)`, `offline()`, `use_peer(MultiplayerPeer)`, `leave()`; `manage(node)`, `ignore(node)`, `configure(node, Dictionary)`, `set_owner(node, peer)`, `spawn_for(peer, PackedScene, parent, name = "")`; `input(node) -> NetInput`; verbs `sync(Callable, ...)`, `server(Callable, ...) -> awaitable`, `owner(...)`, `others(...)`, `peer(id, Callable, ...)`; queries `is_owner(node)`, `is_authority(node)`, `is_fresh()`, `tick()`, `rtt(peer)`, `peers()`; `rewind_query(peer, Callable)` (WP9, not built in v1); `set_room(peer, name)`; `travel(player, scene_path, spawn_point = "")`, `load_level(scene_path)`, `unload_level(scene_path)`, `levels()`, signal `travelled(player, from, to)`; property `empty_level_timeout`; properties `send_rate` (not built in v1: snapshots go out every tick), `budget_bytes_per_s`, `cold_poll_ticks`, `input_delay_ticks`, `adaptive_delay`, `emulation: NetEmulation`, `profiles: NetProfiles`; signals `peer_joined(id)`, `peer_left(id)`, `connected`, `disconnected(reason)`, `ticked(tick)` (renamed from `tick` in WP2: the method `tick()` shadowed it), `entity_spawned(node)`, `entity_despawning(node)`, `spawn_refused(node)` (WP2), `call_refused(node, method, sender)`; WP2 queries `get_owner_of(node)`, `net_id(node)`; `state: NetKeyedState`. WP3: `cold_poll_ticks` and `budget_bytes_per_s` read as the effective values (0 written = back to the project settings); WP6: the verbs take the Callable then up to eight optional arguments (`sync`, `owner`, `others`, `peer(id, ...)` return a `NetCall`; `server` returns the `NetCall`'s `completed` signal, so `await` yields the server's return value), plus `sync_unreliable`; the same verbs and `call_refused` exist on `NetSession`, whose `stats()` gains the `calls_*` counters, and the test hook `_debug_extra_method` (`"Part:method"`); `NetSession` gains `stats()`, `budget_bytes_per_s`, `cold_poll_ticks` (0 = `Net`'s) and the test hook `_debug_perturb_property`; an internal `NetPoller` child runs the +1000 post-tick hook; WP4: `input(node)`, `is_fresh()`, `input_delay_ticks`, `adaptive_delay` work as described in 2.1/2.5; signal `rolled_back(node, from_tick, to_tick)` (also on `NetSession`); `global_input_warnings()` (the scripts D14 warned about); `NetSession.stats()` gains `rollbacks`, `rollbacks_per_s`, `replayed_ticks`, `inputs_repeated`, `prediction_lead`, `input_delay`, `predicted_tick` and the prediction counters; WP5: read-only `jitter_buffer_ticks` (also on `NetSession`), the `configure()` key `teleport_distance` (`float | {prop: float}`), `NetSession.stats()` gains `jitter_buffer_ticks`, `render_lag_ticks`, `late_snapshots`, `late_snapshots_per_s`, `offset_corrections`, `offset_corrections_per_s`, `interp_teleports`; WP7: `request_owner(node) -> NetCall` (`completed(granted: bool)`), `release_owner(node) -> bool`, signals `owner_changed(node, old, new)` and `write_rejected(node, sender)` (one signal on every side: on the writer `sender` is its own id), `state` as above; the same signals and `state` on `NetSession`, whose `stats()` gains `owner_changes`, `writes_rejected`, `writes_sent`, `owner_states_sent`, `owner_states_received`, `ownership_requests_granted`, `ownership_requests_refused`, `keyed_bytes_sent`, `keyed_bytes_received`, `keyed_writes_rejected`, `keyed_entries`; WP8: `show_meter(bool)`, `meter_visible()`, `show_overlay(bool)`, `overlay_visible()`, `overlay_rows() -> Array[Dictionary]` (`net_id`, `owner`, `role` = `server`/`own`/`other`/`client_owned`/`rejected`, `color`, `client_owned`, `transfer`, `scene`, `parts`, `hashes_diverged`, `node`), `stats()` (the default session's; `{}` without one), `divergences(node) -> int`, `emulation_preset(name) -> bool`; `NetSession` gains `overlay_rows()`, `divergences(node)`, the test hook `_debug_corrupt_held(node, prop, value)`, and `stats()` gains `hash_telemetry`, `hashes_sent`, `hashes_received`, `hashes_compared`, `hashes_skipped`, `divergences`, `divergences_per_s`, `rtt_ms`, `jitter_ms`, `tick`, `server_tick_offset`, `snapshot_bytes_per_s`, `keyed_bytes_per_s`, `entities_in_scope`, `budget_use`; `NetPipePeer` gains `bytes_sent()`, `packets_sent()`, `bytes_received()`, `packets_received()`; WP13: `travel` is queued and applied at the session's next tick (safe inside `body_entered`) and works in any branch (the player's session); `load_level`, `unload_level`, `levels`, `set_room` act on `Net`'s session; also `set_player(peer, node)`, `set_world(node)`, `session_for(node) -> NetSession` (the node's branch session, else `Net`'s); `empty_level_timeout` (0 = `gdnet/levels/empty_timeout_s`); `NetSession` gains `travel`, `load_level(scene_path) -> Node`, `unload_level`, `levels()`, `set_room`, `set_player`, `room_of(peer)`, `get_world()`, `empty_level_timeout`, signal `travelled(player, from, to)`, and `stats()` gains `entities_in_scope_by_peer`, `room`, `rooms`, `levels`, `travelling`, `travels`, `scope_entered`, `scope_left` (`entities_in_scope` = what a client holds, every entity on the server); `overlay_rows()` rows gain `room`; WP14: `NetSession` gains `interacting(peer) -> PackedInt64Array` (the net ids marked interacting with that peer this tick, server), `entity_sends(node, peer)` (snapshots that carried the entity to the peer, server), `hash_sample` (entities hash telemetry samples per tick on this server, -1 = `gdnet/debug/hash_sample`; applies at once), and `stats()` gains `deferred_entities`, `deferred_fields`, `deferred_entities_per_s`, `deferred_fields_per_s`, `deferred_entities_tick`, `deferred_fields_tick`, `interacting`, `interaction_ms`, `hash_sample` (review 2026-11-08: also `interaction_ms_avg`, `interaction_passes`); the meter's `budget` line shows `deferred: N ent/s` and `interacting N`. Review 2026-11-08: `Net.hash_sample` (-1 = the setting; `NetSession.hash_sample` -1 now means `Net`'s); **when `Net`'s settings apply**: at once (running sessions re-read them each tick) `budget_bytes_per_s`, `hash_sample`, `cold_poll_ticks`, `input_delay_ticks`, `adaptive_delay`, `emulation` (and `emulation_preset()`), `empty_level_timeout`; start-only: `profiles` (scenes already registered keep their tables; the `Hello` hash is taken at session start), `_debug_protocol_salt` (the next session); read-only: `tick_rate`, `jitter_buffer_ticks`, `state`; there is no `Net.interaction_radius` (the radius is `NetManager.interaction_radius`, else the setting, read every server tick) |
| `NetManager` (Node) | `room: String` (default the owning scene's path; `"*"` = visible from every room; WP13: empty = the manager's owner's `scene_file_path`, else its path relative to the session's branch), `profiles: NetProfiles` override, `spawn_policy: server_only | owner_allowed`, `split_scenes: Array[String]`, `input_actions: PackedStringArray` (default all non-`ui_` actions), read-only `entities: int`; WP14: `interaction_radius: float` (entities within it of a peer's player count as interacting with that peer; in the entities' units, pixels in 2D and metres in 3D; 0 = `gdnet/budget/interaction_radius`; negative = no proximity scoring) |
| `NetEntity` (Node child of an instance root) | `inherit_profile: bool` (default false), `state_properties: Array[NodePath]`, `state_options: Dictionary` (per-property options), `input_properties`, `owner_peer`, `transfer`, `client_owned`, `simulate: owner_and_server | everywhere`, `priority`, `radius`, `visibility_filter`, `lag_compensated` (these three not built in v1: interest is rooms (WP13) and interaction scoring (WP14), lag compensation is WP9; `Net.configure` keeps the keys in the root's `_net_config` meta), `predict`; read-only `net_id`; signals `owner_changed`, `rolled_back(from, to)`; WP4: `simulate` (`owner_and_server` = 0, `everywhere` = 1), `predict` (`check` = 0, `always` = 1, `disabled` = 2: no prediction, the owner shows the server's state), `input_actions: PackedStringArray` (narrows the manager's list), `input_properties` (`prop` / `Part:prop`, travel in the input frame), signal `rolled_back(from_tick, to_tick)` (from = the predicted tick, to = the confirmed tick restored); WP7: `transfer` (`fixed` = 0, `request` = 1, `take` = 2), `client_owned: bool`, `inherit_owner: bool` (default true: a nested entity takes its enclosing entity's owner at creation unless this is off or `owner_peer` is not 1), signal `owner_changed(old, new)` (every peer, once per change), virtual `_net_owner_changed(old, new)` |
| `NetInput` (RefCounted) | the `Input` query mirror plus `get(name)`, `set(name, value)`, `actions()`, `tick`; WP4: also `is_live()` (reads the real `Input`); `get`/`set` are registered under those names (they shadow `Object.get/set` on a `NetInput`) |
| `NetProfiles` (Resource) | `classes: Dictionary[String, Array]` of property entries with options (`name`, `schedule`/`send`, `quantize`, `interpolate`, `tolerance`, `reliable`, `poll_seconds`, `remove`; an empty array syncs nothing); `merge(other)`; WP3: static `builtin()`, `effective()`, `profile_hash()`; WP5: the entry option `teleport_distance` (also a `NetEntity` `state_options` key), and the shipped velocities (`velocity`, `linear_velocity`, `angular_velocity`) interpolate |
| `NetKeyedState` | `set`, `get`, `erase`, `view(path, Callable)`, signal `changed(path, value)`; WP7: a `RefCounted` (`Net.state` is the default session's, a local map while no session runs; `NetSession.state` in a branch), also `get(path, default = null)`, `has(path)`, `keys(prefix = "")`, `allow(prefix, who)` (`who` = a peer id, `"all"` or `"owner"`: the path segment after the prefix is the writer's id), `view(prefix, filter)` with `filter(peer_id) -> bool` re-evaluated every server tick, signals `erased(path)`, `write_rejected(path)`; `set`/`erase` return false (and print) when refused locally |
| `NetEmulation` (Resource) | latency, variance, loss, reorder, duplicate, per direction; presets (WP8: `off`, `lan` 5 ms, `average`, `bad`, `mobile` 120 ms + 20 ms variance + 2 % loss, `custom` = the `gdnet/debug/emulation/*` settings) |
| `NetCallInfo` (RefCounted) | `sender`, `tick`, `is_local` (WP6: true when the call was made on this peer, i.e. `sender` is this peer) |
| `NetCall` (RefCounted, WP6) | the result of a verb: read-only `ok`, `error`, `result`, `suppressed`, `is_done()`, signal `completed(value)` (emitted deferred, once) |
| `NetMeter` (CanvasLayer) | the panel (WP8: `addons/gdnet/meter.tscn` with the GDScript controller `addons/gdnet/meter.gd`, `class_name NetMeter`; `session` (null = `Net`'s, else the first session in the tree), `interval` (0.25 s), read-only `last_stats`, `text()`, `refresh()`); WP8 also `NetOverlay` (CanvasLayer, the in-game ownership overlay) and the editor classes `GdNetEditorPlugin` / `GdNetDebuggerPlugin` |
| virtuals on managed scripts | `_net_tick`, `_net_input`, `_net_spawned`, `_net_despawning`, `_net_owner_changed`, `_net_travelled(from, to)`, `_net_allow`, registered with `#[func(virtual)]` so GDScript overrides them like `_ready`; WP7: `_net_owner_changed(old, new)` runs on every part that defines it, and `_net_grant_owner(requester) -> bool` (optional, on the current owner's copy, for `transfer = request`: every defining part must say yes; without it yes for a server-owned entity, no for a client-owned one) |

### 3.2 Rust API (gdnet)

`INetEntity` with the same virtuals; `#[derive(NetSync)]` with `#[net(state|input, ...)]` producing the
property table and `mark_dirty()`; `Net::singleton()`; `NetProfiles` extendable from Rust.

### 3.3 Protocol versioning

`Hello` carries a 64-bit hash of the netcore version, every managed scene's part table, the effective
profiles and option sets. Mismatch disconnects naming the first differing item.

### 3.4 Project settings

`gdnet/tick/send_rate` (not built in v1), `gdnet/budget/bytes_per_second`, `gdnet/sync/cold_poll_ticks`,
`gdnet/sync/profiles` (a `.tres` path laid over `addons/gdnet/default_profiles.tres`, default `""`), `gdnet/sync/export_vars` (bool, D12, default true), (WP3: `gdnet/budget/bytes_per_second` default 32,768 and `gdnet/sync/cold_poll_ticks` default 6; review 2026-11-08: both re-read every tick, so they and `Net.budget_bytes_per_s` / `Net.cold_poll_ticks` reach running sessions), `gdnet/input/actions` (WP4: PackedStringArray, empty = every InputMap action but `ui_*`, sorted),
`gdnet/prediction/max_rollback_ticks` (WP4: unset = 20, and a third of a second above 60 Hz, e.g. 40 at 120 Hz), `gdnet/interpolation/jitter_buffer_ticks` (WP5: default 2, the starting buffer, 1..6), `gdnet/interpolation/adaptive` (WP5: default true), `gdnet/interpolation/teleport_distance` (WP5: default 0 = 2 m, 200 px for `Vector2` properties; in the property's units), all three read when a session starts,
`gdnet/lag_compensation/max_ms` (WP9, not built), `gdnet/debug/meter` (WP8: default false; shows the meter when `Net` starts a session), `gdnet/debug/overlay` (WP8: default false, the same for the overlay), `gdnet/debug/emulation_preset` (WP8: `off` default, `lan`, `average`, `bad`, `mobile`, `custom`; applied when `Net` starts a session without `Net.emulation`), `gdnet/debug/emulation/<field>` (WP8: the `NetEmulation` fields for `custom`),
`gdnet/debug/hash_telemetry` (WP8: default true), `gdnet/debug/hash_sample` (WP8: entities per tick, default 16; review 2026-11-08: re-read every tick, overridden by `Net.hash_sample`), `gdnet/debug/hash_refresh_s` (WP8: an unchanged hash is resent after this long, default 10), the first and last read when any session starts, `gdnet/debug/warn_on_global_input` (WP4: default true, debug builds only).
`gdnet/levels/empty_timeout_s` (WP13: default 30; a server's level is unloaded this long after its last player left).
`gdnet/budget/interaction_radius` (WP14: default 300, in pixels; 3D reads it at 100 px per metre, 3 m; read every server tick), `gdnet/budget/interaction_every_ticks` (review 2026-11-08: the interaction pass runs every N server ticks, default 1, the sets kept in between; read every tick).

## 4. Work packages

One commit series per package, ending with `python3 tools/check.py` green (which from WP0 includes
`cargo test`). Rust unit tests cover everything in `netcore`; GDScript `TestCase` tests cover everything
user-visible, headless.

**Headless two-peer testing.** Godot supports one `MultiplayerAPI` per subtree
(`SceneTree.set_multiplayer(api, root_path)`), so one headless process holds a `/root/Server` branch and
one or more `/root/ClientN` branches connected by `NetPipePeer`, an in-memory `MultiplayerPeerExtension`
pair with the emulator between them (WP1). `tests/framework/net_fixture.gd` builds and tears down the
branches and runs each scenario twice: host-as-player and dedicated server (gate G2). The same pipe is the
production `offline()` peer.

| WP | scope | files | tests | docs |
|---|---|---|---|---|
| **WP0 scaffolding** | Cargo workspace, gdext build, `addons/gdnet/gdnet.gdextension`, a `NetVersion` class; `tools/check.py` gains `build` (`cargo build --release -p gdnet` into `addons/gdnet/bin/<platform>`, skipped by `--no-build` when up to date) and `rust-test` (`cargo test --workspace`) stages; CI installs Rust, caches `target/`, adds a `cargo check --target wasm32-unknown-emscripten` job on nightly (gate G3 from day one) | `rust/Cargo.toml`, `rust/netcore`, `rust/gdnet`, `addons/gdnet/`, `tools/check.py`, `.github/workflows/ci.yml`, `.gitignore` | `tests/unit/test_gdnet_loads.gd`; an empty `cargo test` | CLAUDE.md commands, this file's status |
| **WP1 tick, session, pipe peer** | `netcore`: clock, peers, `Hello`/`Welcome`, `Time`, emulator; `gdnet`: `Net`, the `MultiplayerAPIExtension` wrapper with full forwarding, `NetPipePeer`, `host`/`join`/`offline`, `net_fixture.gd` | `rust/netcore/src/{clock,peers,emul,wire}.rs`, `rust/gdnet/src/{net,api_ext,pipe_peer}.rs`, `tests/framework/net_fixture.gd` | Rust: drift, emulator, handshake, hash mismatch; GDScript: `offline()` ticks at the physics rate; two branches connect and see `peer_joined`; stock `@rpc` and `MultiplayerSynchronizer` under the wrapper still work (G1); emulation delays by the configured latency | starting a session |
| **WP2 manager, entities, atomic spawn** | `netcore`: entity/part table, id ranges, `EntityAction`; `gdnet`: `NetManager` enrolment via `node_added/removed`, scene hashes and part tables, create + parent + initial state as one message, destroy, reparent, late-join burst, `spawn_policy`, `Net.manage/ignore`, `_net_spawned/_net_despawning`, the bare-node fallback with its warning | `rust/netcore/src/entity.rs`, `rust/gdnet/src/{manager,entity,parts}.rs` | Rust: ordered application, a create whose parent is unknown is held, ranges never collide, part tables are deterministic; GDScript: a server `add_child` under the manager appears on the client under the same parent and name with initial state before `_net_spawned`; nested instances are parts; a late joiner gets all 50 spawns; `queue_free` frees the replica; a client-side `add_child` is refused readably under `server_only` and becomes `client_owned` under `owner_allowed`; a bare `Sprite2D` replicates with the warning | enrolment and spawning |
| **WP3 profiles, inference, state replication** | `netcore`: property tables, quantisers, hot/cold schedules, dirty masks, acked-baseline deltas, MTU packing, priority accumulator, budget; `gdnet`: `NetProfiles` + defaults `.tres`, inference over the class hierarchy and `@export` vars, polling with cached `StringName`s, snapshot application, `configure()`, `NetEntity` with `inherit_profile`; the polling benchmark | `rust/netcore/src/{props,quant,delta,snapshot,priority}.rs`, `rust/gdnet/src/{profiles,infer,poll,apply,configure}.rs`, `addons/gdnet/default_profiles.tres` | Rust: quantiser round trips, lost packets re-dirty, baseline fallback, accumulator ratios, MTU never exceeded; GDScript: **gate G7**: a stock test scene (CharacterBody2D with AnimatedSprite2D, a platform with AnimationPlayer, a Label, a script with three `@export`s) under `NetManager` with no script edits replicates to the client within tolerance (positions, animation names and frames, text, exports); `visible = false` propagates; `configure()` adding and removing properties takes effect; 100 idle entities cost under a stated ceiling and 100 moving entities stay within the budget (G4); the benchmark prints polling cost per 1,000 entities and the meter counter matches it | profiles reference, the "what syncs" table |
| **WP4 input map, `_physics_process` as tick, prediction** | `netcore`: input frames with redundancy, repeat-last-input, `PredictionHistory`, mismatch, rollback requests, `predict` policy; `gdnet`: `NetInput` sampling of the InputMap and custom values, `Net.input()`, `set_physics_process(false)` on replicas, `_net_tick` precedence, replay by direct `_physics_process` calls, `Net.is_fresh()`, `set_owner`/`spawn_for`, the global-`Input` warning | `rust/netcore/src/{input,predict}.rs`, `rust/gdnet/src/{input,tick,replay}.rs` | Rust: frames land on the matching tick, 2-of-3 loss covered, mismatch at T rolls back to T, `disabled` never rolls back, edges identical during replay; GDScript: rung 1's player over a 100 ms emulated link moves immediately on the owner and converges with the server within N ticks; just-pressed fires once across a replay; a server impulse causes exactly one rollback and the `is_fresh` one-shot runs once; the replica's `_physics_process` does not run and its `_process` does; the host's own player never rolls back; passes at physics 30, 60 and 120 Hz (G5); `_net_tick` takes precedence when defined; the warning fires for `Input.get_vector` in an owned entity | the rung 1 tutorial |
| **WP5 interpolation and smoothing** | `netcore`: render timeline, adaptive jitter buffer, sampling, error offsets, teleport threshold; `gdnet`: `_process` application on replicas, blending on owned entities after rollback | `rust/netcore/src/interp.rs`, `rust/gdnet/src/interp.rs` | Rust: linear sampling, decay rates, snap threshold; GDScript: a remote entity at constant speed renders with per-frame displacement variance below a bound at 60 and 144 Hz render rates and never moves backwards | interpolation |
| **WP6 auto-exposed methods and verbs** | `netcore`: `Call`/`CallReturn`, request ids, verbs, reliable default, suppression during replay; `gdnet`: method tables from `get_script_method_list()` and `ClassDB`, `Net.sync/server/owner/others/peer`, `NetCallInfo` injection, awaitable returns, Variant argument rules, `_net_allow`, `call_refused` | `rust/netcore/src/call.rs`, `rust/gdnet/src/{methods,call}.rs` | Rust: routing matrix, unknown hash is an error to the caller, returns matched; GDScript: rung 2 end to end (a client's `Net.sync(open_door, 7)` runs on the server then on both clients once); `await Net.server(try_buy, 3)` returns the server's value; a built-in method (`$Anim.play`) syncs; a non-owner's `sync` is refused and `_net_allow` admits it; a call to an entity the receiver has not spawned waits behind its `Create` (never "node not found"); calls made inside a replayed tick are not re-sent; an Object argument errors at the call site; **gate G6**: randomized spawn/reparent/call/destroy under loss and reorder never errors | the rung 2 guide, migration from `@rpc` |
| **WP7 authority and keyed state** | `netcore`: owner actions, acceptance rules, transfer, `KeyedState` with views; `gdnet`: `owner_peer`, `transfer`, `client_owned`, inheritance to nested entities, `_net_owner_changed`, `NetKeyedState`, rejected-write rollback | `rust/netcore/src/{authority,keyed}.rs`, `rust/gdnet/src/{authority,keyed}.rs` | Rust: non-owner input dropped, `take`/`request` semantics, keyed state deltas and full map on join, views; GDScript: a nested entity inherits its owner; changing the owner mid-game moves prediction; a `client_owned` box moved by its client is seen by another; a late joiner receives the score | authority guide with the trust warning |
| **WP8 diagnostics** | `NetMeter`, hash telemetry, ownership and profile overlay (editor debugger plugin + in-game tint), emulation presets in Project Settings | `rust/gdnet/src/{meter,hash,overlay,editor}.rs`, `addons/gdnet/meter.tscn` | Rust: hash equality and sensitivity; GDScript: meter counters equal the pipe's byte counts; a deliberate client-side state edit raises the divergence counter; the overlay node exists and tints by owner | diagnostics |
| **WP9 lag compensation** | pose history, `rewind_query` moving and restoring shapes, the 250 ms cap | `rust/netcore/src/rewind.rs`, `rust/gdnet/src/rewind.rs` | Rust: cap; GDScript: a server raycast inside `rewind_query` hits where the client saw the target over 200 ms and misses without it | lag compensation |
| **WP10 native peer (phase 2)** | `gdnet-peer`: `MultiplayerPeerExtension` over renet/renetcode, connect tokens, I/O thread + channels; optional quinn | `rust/gdnet-peer/` | Rust: the 16 virtuals over loopback UDP; GDScript: two headless processes driven by a Python test connect over 127.0.0.1 | transports |
| **WP11 web build** | Emscripten CI job, an exported web demo over `WebRTCMultiplayerPeer` | `.github/workflows/web.yml`, `export_presets.cfg` | the build succeeds (G3); browser smoke recorded as manual | export |
| **WP12 deterministic mode (optional)** | `netcore` feature `deterministic`: GGRS over the pipe/peers, `SyncTest`, input recording and replay | `rust/netcore/src/det.rs` | Rust: sync-test on a fixed-point sample passes and a scripted divergence is caught | determinism, with the project-start warning |

| **WP13 demo: Doors (players travel between scenes)** | `demos/doors/`: a hub and two levels, each a scene with a `NetManager`; doors are `Area2D`s whose server-side `body_entered` calls `Net.travel()`; a chat box on the keyed state; a camera reset in `_net_travelled`; the `World` node pattern for server and client; headless server launch (`--headless --server`). Library work it drives: `room` on managers, server-side level load/unload with `empty_level_timeout`, the `Travel` action and client acknowledgement, cross-manager `Reparent`, room-scoped interest and late-join bursts, `Net.travelled` | `demos/doors/**`, `rust/netcore/src/rooms.rs`, `rust/gdnet/src/{world,travel}.rs` | Rust: room filtering of snapshots and bursts, a reparent across rooms moves scope, travel ordering (level loaded before the reparent applies); GDScript (two client branches + server): client A walks through a door and is in the cave on both the server and A; B stops receiving A's updates and A stops receiving B's; A and B meet again in the cave and see each other; chat written in one room is read in another; a third client joining late into the cave gets only the cave's entities; the hub unloads on the server after the timeout and reloads on the next travel; the whole demo runs under `net_fixture.gd` in both host modes (G2) | the Doors tutorial (walkthrough of every file, in the demo's README) |
| **WP14 demo: Ballpit (many colliding bodies)** | `demos/ballpit/`: a box with N `RigidBody2D` balls (N from a slider, default 300), two player paddles (`CharacterBody2D`, predicted), a meter overlay, a body-count and budget slider; a `client_owned` paddle toggle to show the trust trade-off. Library work it drives: the RigidBody profile and freeze-on-replica rule, sleeping-body zero cost, proximity priority (bodies touching or near the player's paddle count as interacting), MTU packing of many small entities, graceful degradation under budget | `demos/ballpit/**`, `rust/netcore/src/priority.rs` (interaction scoring), `rust/gdnet/src/physics.rs` | Rust: 300 entities pack into MTU-sized snapshots, the accumulator sends near bodies at least 10× as often as far ones under a 32 KB/s budget, a body with no dirty fields costs 0 bytes; GDScript: 300 balls settle on the server and the client's interpolated positions converge within tolerance; while settled, bytes per second to a client are under a stated ceiling (near zero plus heartbeats); a paddle push on the owner is predicted and the pushed balls converge on the client within N ticks without double simulation (replica `_physics_process` off, `freeze` on); with the budget halved the demo stays within budget and the meter's degradation counter rises; G4 and G5 scenarios reuse this scene | the Ballpit tutorial (README), the bandwidth chapter |
| **WP15 follow-up: cooperative physics ownership** | Fiedler-style per-body authority with ownership sequence numbers for bodies a `client_owned` paddle touches; a Ballpit toggle | `rust/netcore/src/handoff.rs` | Rust: sequence-number arbitration; GDScript: a client-pushed ball is smooth on the pusher and converges on others | the Ballpit tutorial's last section |

WP0-WP2 and WP6-WP8 are each a few days; WP3-WP5 are the core and largest; WP9-WP12 and WP15 are
independent; WP13 and WP14 are the end goals of v1 and should be started as soon as WP7 lands so their
requirements pull on the earlier packages (the demo scenes are written first, as failing tests, and grow
with each package).

## 5. Order and gates

Order: WP0 → WP1 → WP2 → WP3 → WP4 → WP5 → WP6 → WP7 → WP8 → WP13 → WP14, then WP9-WP12 and WP15 in any
order. WP6 may be pulled forward after WP2 for lobby code. v1 = WP0-WP8 plus the two demos (decision D11).

- **G0 Single player untouched.** The extension installed, no `Net` call, no manager: no packets, no polling,
  no measurable frame cost. The repo's smoke test plus a frame-time assertion.
- **G1 Stock compatibility.** `@rpc`, `MultiplayerSpawner`, `MultiplayerSynchronizer` work under the wrapper
  (WP1).
- **G2 Host parity.** Every integration scenario passes with the host as a player and as a dedicated server
  (WP2 onward, through `net_fixture.gd`).
- **G3 Web build.** `netcore` and `gdnet` compile for Emscripten; a forbidden dependency fails CI in the
  package that adds it (WP0).
- **G4 Bandwidth bound.** 100 moving entities within budget; 100 idle entities under a stated ceiling (WP3).
- **G5 Rate independence.** Prediction tests at physics 30/60/120 Hz; interpolation at render 60/144 Hz
  (WP4, WP5).
- **G6 No ordering race.** Randomized spawn, reparent, call and destroy under emulated loss and reorder never
  produce a missing-node error or a dangling replica (WP6).
- **G7 Zero-change scene.** A stock single-player scene under `NetManager`, with no script edits, replicates
  transforms, animation, text and exports to a client within tolerance (WP3). This is the gate that stands
  for the owner's requirement.
- **G8 Demos run headless.** The Doors and Ballpit scenes pass their scenario tests under `net_fixture.gd`
  in both host modes, and their READMEs' code listings are the files in the repo (a test diffs them) (WP13,
  WP14).

## 6. Testing procedure additions for this repo

- `tools/check.py` stages: `build → import → validate → rust-test → test → smoke`; `--no-build` skips the
  build; the failure table in `CLAUDE.md` gains `cargo` rows.
- The pipe peer plus emulator makes every network test deterministic and headless: no sockets, no sleeps,
  `await wait_frames(n)` only.
- `tests/framework/net_fixture.gd` parametrises scenarios over host-as-player and dedicated server (G2).
- Gate G7's fixture scene lives in `tests/fixtures/zero_change/` and is deliberately written as a plain
  single-player scene; a test asserts its scripts contain no `Net.` reference.
- The skill template is not changed; if the kit grows a native-extension option, the `build` stage is the
  piece to port.

## 7. Decisions for the owner

Each with a recommendation. The owner took every recommendation on 2026-11-08; they stand as decided.

- **D1 Name and home.** `gdnet`, in this repository (`rust/` + `addons/gdnet/`) with this project as sample
  and test bed; extract later. *Recommended.*
- **D2 Transport.** Wrap `SceneMultiplayer` first; own peer over renet as WP10. *Recommended.*
- **D3 Default model.** State sync with prediction of the owner's kinematic entities, interpolation for the
  rest; deterministic rollback opt-in, Rust only. *Recommended.*
- **D4 Behaviour-sync verbs.** `Net.sync(callable, args...)` and friends taking a `Callable`, versus a method
  on the node (`net_call("open_door", 7)`) versus naming conventions (`server_*`). *Recommended: Callable
  verbs*; they are typo-checked by the editor, need no registration, and work for built-in methods.
- **D5 Identity.** Own `u32` net ids with (entity, part index) addressing from the scene file; not NodePath,
  not 4.6 unique node ids. *Recommended.*
- **D6 Minimum Godot.** `compatibility_minimum = 4.3`. *Recommended.*
- **D7 Built artefacts.** Not committed; `check.py` builds locally, CI builds per platform. *Recommended.*
- **D8 Default rates.** 60 Hz tick (physics), send rate = tick, 32 KB/s per client, `cold_poll_ticks` 6,
  20-tick rollback window, 2-tick jitter buffer, 250 ms lag-compensation cap. *Recommended* as starting
  values.
- **D9 Shared authority exposure.** Ship `client_owned`, `transfer = take` and `spawn_policy =
  owner_allowed` in WP7 with the trust warning and the overlay. *Recommended: ship.*
- **D10 Adaptive delay.** Experimental, off by default, in WP4. *Recommended.*
- **D11 v1 scope.** WP0-WP8 plus the two demos WP13 and WP14; WP9 first follow-up; WP10-WP12 and WP15 by
  demand. *Recommended; the owner added the demos as end goals.*
- **D12 Exported variables sync by default.** Yes, as cold properties, because `@export` is the node's
  public API and the inspector already shows exactly that list; a project can turn it off
  (`gdnet/sync/export_vars`) or exclude per node. *Recommended: yes.* Alternative: only properties from
  class profiles.
- **D13 Replicas stop simulating.** `set_physics_process(false)` on every part of a non-owned, non-server
  entity, with `simulate = everywhere` as the opt-out. *Recommended: yes*; it is what makes `_physics_process`
  usable as the tick and prevents the double-simulation jitter that fills the FishNet and Godot trackers.
  Alternative: leave `_physics_process` running and only overwrite state afterwards (jittery).
- **D14 Auto-exposure scope and trust.** Every public method of every managed part is callable; a
  non-authority may `sync` only entities it owns unless `_net_allow` says otherwise; `server` calls are
  open but carry the sender. *Recommended.* Alternative: expose only methods listed on the manager (safer,
  more ceremony). The global-`Input` warning is a debug-build check that compares `Input` action state
  with the sampled `NetInput` inside an owned entity's tick; it is advisory only.
- **D15 Input map replication default.** All non-`ui_` actions of the project InputMap, filterable per
  manager and per entity. *Recommended*; typical cost is a few bytes per tick and the meter shows it.
  Alternative: declare actions per entity.
- **D16 Non-scene dynamic nodes.** Replicate as class + inferred properties with a one-time warning,
  recommending scenes. *Recommended.* Alternative: refuse.
- **D17 Units of the fixed-point quantisers (decided in the review of 2026-11-08).** 3D lengths are
  metres; 2D lengths are pixels at 100 px per metre, so in 2D `mm` is a 0.1 px step and `cm` a 1 px step
  (`Vector2` values and a `Transform2D`'s origin; `Vector2i` is integers and untouched; 2D `rotation` is
  `deg` and has no length; a bare `float` has no known dimension and keeps 0.001 / 0.01 of its unit). The
  default for 2D positions and velocities stays `mm`, now 0.1 px. The 2D prediction tolerance follows (one
  `mm` step, 0.1 px; 3D keeps 0.001). *Taken.* Reasoning: WP3 had defined `mm`/`cm` as 1/1000 and 1/100 of
  the type's unit, which for pixels meant 0.001 px, finer than any display or physics engine resolves, at
  about a byte more per axis for positions in the hundreds of pixels; WP5's thresholds already read 2D at
  100 px per metre, so the quantisers now agree with them. It changes the wire format (`wire::WIRE_FORMAT`
  2, a `Protocol` item, so a WP14 peer is refused naming it). Effect: a Ballpit body update 20.6 -> 15.8
  bytes; the halved-budget G4 test 17.4 -> 23.1 updates of a far entity in 2 s. Alternative: keep 1/1000 of
  the unit and let 2D projects pick `cm` (the WP14 deviation 7 stance), rejected because the default is
  what nearly every project ships.

## 8. Risks and how the plan handles them

- **Polling cost.** Inference lists more properties than hand declaration would. Hot/cold schedules, cached
  `StringName`s, `mark_dirty()` for Rust, the WP3 benchmark and the meter's polling counter keep it measured;
  the stock synchronizer polls too, so parity is the floor.
- **Scene part tables drifting.** A client with a different version of a scene file gets a different part
  table; the protocol hash catches it at connect and names the scene.
- **`_physics_process` replay and engine state.** `is_on_floor()` and similar are stale during replay
  (netfox's limit). Documented in rung 1; WP4 pins the behaviour with a test; `_net_tick` is the escape for
  code that must know whether it is replaying.
- **Input-map replication and `Input` singletons.** Scripts that read `Input` directly inside an owned
  entity will diverge on the server; the warning (D14) and the tutorial address it.
- **gdext API churn.** Pin the minor; keep `godot` behind `rust/gdnet/src/engine.rs`.
- **Emscripten unknowns.** No Rust transport in the web build; G3 from WP0.
- **Two MultiplayerAPIs in one process for tests.** Supported but little used; WP1 proves it first, with the
  two-process driver as the fallback.
- **Scope creep toward services.** The non-goals in the head of this file.

## 9. CLAUDE.md after this plan

Commands table: `build`, `rust-test`, `--no-build`. A "Rust" conventions paragraph (`cargo fmt`, `cargo
clippy -D warnings`, no `experimental-threads`, all `Gd<T>` on the main thread, `netcore` has no `godot` and
no `std::net` dependency, enforced by a test). Layout rows for `rust/` and `addons/gdnet/`. A "Writing
networked scenes" section with the ladder of section 1 in five lines. Pointers to this plan and the report.

## 10. Status

Research done 2026-11-08 (five notes, one report). Plan revised the same day to put the managed layer first
and to add the Doors and Ballpit demos as end goals. Approved 2026-11-08 with every recommendation.
Package status (updated as each lands):

| WP | status |
|---|---|
| WP0 scaffolding | built 2026-11-08: workspace `rust/` (netcore, gdnet on gdext 0.5.5 `api-4-3`), `addons/gdnet/gdnet.gdextension`, `NetVersion`, `check.py` `build` + `rust-test` stages and `--no-build`, CI with Rust and the `wasm-check` job, `tests/unit/test_gdnet_loads.gd`; `CHECK: PASS` (TESTS: 9 passed) |
| WP1 tick, session, pipe peer | built 2026-11-08: `netcore` `clock` (tick clock, server-tick offset, RTT/jitter), `peers`, `wire` (1-byte kind + postcard; `Hello`/`Welcome`/`Reject`, `Time` ping/pong; `Protocol` items and hash naming the first differing item), `emul` (seeded PCG32 emulator, Average/Bad presets), `session` (sans-IO handshake and `Time` state machine); `gdnet` `Net` singleton, `NetSession` (the per-tick node), `NetMultiplayerApi` (forwarding wrapper, G1), `NetPipePeer`, `NetEmulation`; `tests/framework/net_fixture.gd` (`NetFixture`, both host modes); tests `tests/integration/test_net_session.gd`, `tests/unit/test_net_emulation.gd`; guide `docs/guide/01_sessions.md`; `CHECK: PASS` (TESTS: 25 passed, 0 failed, 0 errors, 0 skipped (25 total); netcore 31 Rust tests). Deviations: (1) the method `tick()` and the signal `tick` share a name as specified, but GDScript resolves `Net.tick` to the method (`Net.tick.connect` is a parse error), so the signal is connected with `Net.connect("tick", f)` / `Signal(Net, "tick")`; documented in the guide. (2) gdext requires `#[class(tool)]` on `MultiplayerPeerExtension` / `MultiplayerApiExtension` subclasses. (3) Steps 1 and 6 of 2.2 run in one `_physics_process` of `NetSession` at priority -1000 (nothing is recorded between them yet; WP3 splits step 6 into the +1000 post-tick hook). (4) netcore uses `std::` paths: the purity test forbids `extern crate`, so it is not literally `no_std` (it uses only `alloc`-level types); postcard (`alloc` feature) brings cobs, embedded-io and thiserror transitively, all building for emscripten. (5) Emulation: reliable packets are never lost but each simulated loss costs a resend delay; at loss 1.0 nothing arrives. On `NetPipePeer` it applies to all traffic and counts time in physics frames (deterministic); for other peers the session applies it to gdnet's own messages only. (6) A GDScript lambda connected to a `Net` signal that captures no object is never disconnected by Godot and crashed the process when freed with the singleton after the script language shut down; `Net` hooks the root's `tree_exiting` (on its first session or emulation call) to disconnect everything at shutdown, because the 4.5 main-loop callbacks are not available with the `api-4-3` build. (7) Contract choices the plan left open: `peers()` lists remote peers only; `rtt()` is milliseconds (-1 unknown); `connected` also fires on a host at its first tick; `leave()` emits nothing; a `NetSession` ends when its node leaves the tree; ENet sessions use 3 channels. |
| WP2 manager, entities, atomic spawn | built 2026-11-08: `netcore` `entity` (`EntityTable` with cascade destroy and a parents-first burst, `IdAllocator` over the per-peer ranges, `HeldQueue` keeping per-entity order, `PartTable` + FNV-1a hashes, `EntityAction` = `Scene` / `Create` / `Destroy` / `Reparent` / `SetOwner` as message kind 6 on channel 0), `session` (id ranges handed out in `Welcome`, broadcast and relay, late-join burst after `Welcome`, held actions reported after 5 s and capped at 65,536, proposals for `owner_allowed` creates, per-scene part checks), `wire::Protocol.scenes` + `scene_mismatch`; `gdnet` `NetManager` (`manager.rs`: enrolment via `node_added`/`node_removed`, static adoption, spawn/destroy/reparent/owner announcements, `spawn_policy`, `split_scenes`), `NetEntity` (`entity_node.rs`: `owner_peer`, read-only `net_id`, virtual `_net_spawned`/`_net_despawning`), `parts.rs` (part tables from a throwaway instance, cached), `Net.manage/ignore/set_owner/spawn_for/is_owner/is_authority/get_owner_of/net_id`, signals `entity_spawned`/`entity_despawning`/`spawn_refused` on `Net`, `NetSession` and `NetManager`, `NetSession.packets_sent_total()` (G0 probe), `NetFixture.start_server/start_clients/add_managers`; tests `tests/integration/test_net_entities.gd` (14, spawning scenarios in both host modes) with fixtures `tests/fixtures/net/{crate,nested,split_crate}.tscn` + `spawn_probe.gd`; guide `docs/guide/02_entities.md`; `CHECK: PASS` (TESTS: 39 passed, 0 failed, 0 errors, 0 skipped (39 total); netcore 49 Rust tests). Contract change: the per-tick signal is `ticked(tick)` on `Net` and `NetSession` (3.1 updated). Deviations: (1) part tables are not in `Hello`: they are computed when a scene is first spawned, so each side's (path, part hash) goes in a fifth `EntityAction`, `Scene`, sent to a peer before the first `Create` that uses the scene; a mismatch disconnects the client (or makes the server refuse the client) with a reason naming the scene; the `Hello` hash is unchanged. (2) `ParentRef` carries a manager key (FNV-1a of the manager root's path relative to the session's branch root) beside entity + part, since several managers may share a branch; every peer needs its managers at the same paths. (3) The gdnet files are `manager.rs`, `entity_node.rs` (not `entity.rs`, to keep it apart from `netcore::entity`) and `parts.rs`. (4) Announcements and remote actions are applied in the session's `_physics_process` (priority -1000), so a spawn travels at the next tick, after the node's `_ready`; a client-side refusal also happens there (the refused node lives until that tick). (5) Static content: nodes present when a manager becomes ready (and nested split instances inside a replica, and nodes a client adds before its session is connected) are level content; a client binds its own node to the server's `Create` when parent, name and scene match, instead of instantiating; content the server never announces stays local. (6) `Net.manage(node)` adds a hidden internal `NetManager` child whose managed root is `node`: the descendants of `node` are enrolled, not `node` itself, and it must be called on every peer. (7) Contract choices: `is_owner`/`is_authority` are true for unmanaged nodes and without a session; `is_authority` is "this peer is the server" for every entity; a client may destroy and move entities it spawned under `owner_allowed` (the server checks the id range and owner), never others' and never `SetOwner`; entities of a client that leaves stay; replicas stay when a session ends (a new session treats them as level content); cross-manager `Reparent` is refused (WP13); a client's replicas carry the meta `_net_replica`, entity roots `_net_manager`, and `spawn_for` sets `_net_owner`. |
| WP3 profiles, inference, state replication | built 2026-11-08: `netcore` `props` (property descriptors: type tag, quantiser, hot/cold, interpolate, tolerance, reliable, slow ticks; per-scene tables with a hash and a diff naming part and property), `quant` (bit packer, zigzag varints; quantisers `raw`, `mm`, `cm`, `deg`, `smallest3`, `f16` (f16 by hand, no new dependency)), `snapshot` (the bit-packed `Snapshot`: server tick, baseline tick, per entity id + record length + per-part changed bit + dirty mask + values), `priority` (weights 1,000,000 / 100 / 1 times `priority`, the per-peer token bucket, 1,200-byte MTU), `delta` (`Replication`: change detection by quantised encoding, per-peer acked/sent versions, `Ack` = highest tick + 32-bit history, lost snapshots re-dirty their fields, full-state fallback after 0.5 s without acks, MTU packing by accumulator within the budget, reliable `Field` messages for `reliable` properties and values over 1,000 bytes), `session` (`Create` and the late-join burst carry full state, `EntityAction::Scene` carries the property table, message kinds 7 `Snapshot`, 8 `Ack`, 9 `Field`, the `profiles` item in `Hello`, `send_snapshots`, `record_part`, `decode_state`, budget); `gdnet` `NetProfiles` (`profiles.rs`; `merge`, static `builtin()` / `effective()`, `profile_hash()`; `addons/gdnet/default_profiles.tres` = the table of 2.1, a test keeps it equal to `builtin()`), `infer.rs` (class hierarchy most-derived first, `@export` vars as cold, `NetEntity` lists, `configure()` ops, cached per scene), `configure.rs` (`Net.configure`), `apply.rs` (Variant conversion, pseudo-properties `playing` and `current_animation_position`, resources by path), `poll.rs` (`NetPoller`, the +1000 post-tick hook: record, then send; WP1's send moved there), `NetManager.record/apply_states` (initial state applied after `add_child`, before `_net_spawned`; RigidBody replicas frozen once), `NetManager.profiles`, `NetEntity.inherit_profile/state_properties/state_options/input_properties/priority`, `Net.profiles/cold_poll_ticks/budget_bytes_per_s`, `NetSession.stats()/budget_bytes_per_s/cold_poll_ticks/_debug_perturb_property`, project settings `gdnet/sync/profiles`, `gdnet/sync/cold_poll_ticks`, `gdnet/sync/export_vars`, `gdnet/budget/bytes_per_second`; tests `tests/integration/test_net_state.gd` (14: G7 and its no-`Net.` check over `tests/fixtures/zero_change/`, initial state before `_net_spawned`, `configure()`, `NetEntity` lists, the property-table mismatch, late join, G4 idle / moving / halved budget, loss, the benchmark, G0 with a manager and no session) with fixtures `tests/fixtures/net/{configured,listed,inherited,bench}.tscn` + `state_probe.gd`; guide `docs/guide/03_state.md`; `CHECK: PASS` (TESTS: 53 passed, 0 failed, 0 errors, 0 skipped (53 total); netcore 86 Rust tests). Measured: polling 1,000 entities x 8 hot properties 3.8-4.3 ms per tick (release); 100 idle entities 48 B/s in total (pings only, 0 snapshot bytes); 100 moving entities 32,769 B/s of snapshots against a 32,768 budget (a one-second window); with the budget halved 16,380 B/s, the ten priority-10 entities updated on all 120 of 120 ticks and the ninety others 17.4 times [review 2026-11-08, D17's 0.1 px `mm`: 100 moving entities 32,764 B/s, the far ones 23.1 times in 2 s on `crate_snap.tscn`]. Deviations: (1) property tables are compared per scene through the `Scene` action, which now carries the sender's table (the receiver diffs it against its own and names scene, part path and property); the `Hello` hash gains only the effective global profiles (item `profiles`); tables are derived from the first live instance (after its `_ready`, so `configure()` in `_ready` counts) and cached per process, not from the throwaway instance. (2) Cold properties travel in snapshots too (most-recent-state, acknowledged, so the latest value is guaranteed) instead of a reliable message per change; the `reliable` option and values over 1,000 bytes use reliable `Field` messages. (3) Fields carry absolute values; the baseline tick in a snapshot is the newest tick the peer acknowledged (0 after a fallback) and per-field acked versions, not baseline values, select what is sent. (4) Late join: netcore keeps each entity's latest encoded values, so the burst's `Create`s carry current state and the peer counts as holding it (no extra full snapshot). (5) A client's own spawn (`owner_allowed`) is never sent back to it and keeps its local state; the server relays only its initial state until WP7 decides authority. (6) Replicas snap and still run their `_physics_process` (D13 is WP4), so between updates a replica's own scripts move it; G7 waits for convergence within tolerances. (7) "Far" in G4 is a lower `priority` (distance is WP7/WP13); the interacting weight is a hook (`Session::set_interacting`) WP14 scores. (8) The single-process fixture shares one physics space, so the G7 test moves the client copy's bodies to collision layer 2 (the scene is untouched); it also shares inference caches, so a test configures a scene once for both branches. (9) `Net` is now `#[class(tool)]`: the singleton is created in the editor at import, and gdext's v0.6 editor-placeholder migration warned because a non-tool class was instantiated there; the notice is gone. (10) The test runner prints a passing test's output with `--verbose` (benchmarks and measurements), ported to the skill template. Contract choices: a class mapped to an empty profile list syncs nothing and stops the hierarchy walk; profile entries take `schedule` (or `send`), `quantize`, `interpolate`, `tolerance`, `reliable`, `poll_seconds`, `remove`; `merge()` replaces by name; a `NetEntity` with `inherit_profile = false` and no list syncs nothing; `configure()` table changes after the scene's first spawn on that peer are refused with an error, `priority` applies any time, other keys are stored in the root's `_net_config` meta; `NetSession.stats()` per-second figures are the last full second; `deg` replicas see angles in [-PI, PI); the Rust derive's `mark_dirty()` is not built (it needs the derive, rung 5). |
| WP4 input map, `_physics_process` as tick, prediction | built 2026-11-08: `netcore` `input` (`InputFrame`: pressed bitset + a byte per analog strength + custom values hashed to u32 with a 5-bit type tag over the raw quantiser; the bit-packed `Input` message, kind 10, channel 1, the last 3 frames per owned entity; `InputBuffer`: frames by tick, repeat-last on a gap, `repeated` counter; `OwnerInputs` (frames by applied tick, gaps filled with the previous frame); edges derived from consecutive frames; `AdaptiveDelay`), `predict` (`Prediction`: per-entity ring of quantised hot state, a confirmed mirror fed by the snapshots, `evaluate()` at the newest confirmed tick with per-property tolerance, `Policy` check/always/disabled, one `Rollback` for every predicted entity; `lead_target`/`next_lead`), `session` (snapshot hot fields of predicted entities diverted to the mirror, `StateUpdate.tick`, `on_snapshot` returns the applied tick, the server takes each client-owned entity's frame once per tick, owner input refused from non-owners, `predicted_tick()`, `local_input()`, `owner_frame()`, `server_input()`, `track_prediction()`, `record_prediction()`, `take_rollback()`, `set_input_delay()`, `set_adaptive_delay()`, `set_interaction()`, `InputStats`), `ScenePropsTable.inputs` (the action list, hashed and diffed); `gdnet` `input.rs` (`NetInput`: live / frame modes, the `Input` query mirror, `get`/`set`, `actions()`, `tick`, `is_live()`; sampling; the action list from `NetManager.input_actions`, `gdnet/input/actions`, the `InputMap` minus `ui_*`, narrowed by `NetEntity.input_actions`), `tick.rs` (`Net.is_fresh()`, the who-simulates `Gate`, settings, the global-`Input` source scan), `replay.rs` (restore, replay with `is_fresh` false, catch-up), `NetManager` WP4 block (`gate_and_track`, `prepare_inputs` with `_net_input()` and `input_properties`, `net_ticks`, `restore`/`replay_tick`, `record_owned`), `NetSession.input_step()` (after the entity actions, -1000) and the owner's record + send in `post_tick()` (+1000), `NetEntity.simulate/predict/input_actions/input_properties` + `rolled_back`, `Net.input/is_fresh/input_delay_ticks/adaptive_delay/global_input_warnings` + `rolled_back`, project input actions `left/right/up/down/jump`; tests `tests/integration/test_net_prediction.gd` (14: rung 1's convergence in both host modes, G5 at 30/60/120 Hz with an impulse each, one rollback per server impulse, `is_fresh` and just-pressed across a replay, replicas not simulating and `simulate = everywhere`, ownership moving simulation, the host's player never rolling back, `_net_tick`, `input_delay_ticks = 2`, `predict = disabled`, the warning, 2-of-3 input loss, custom values and `input_properties`, `Net.input` offline) with fixtures `tests/fixtures/net/{predicted_player,predicted_player_everywhere,predicted_player_unpredicted,net_tick_player,global_input_player,aim_player}`; guide `docs/guide/04_prediction.md`; `CHECK: PASS` (TESTS: 67 passed, 0 failed, 0 errors, 0 skipped (67 total); netcore 114 Rust tests, gdnet 2). Measured (50 ms each way, a 100 ms round trip): the owner moves on the tick after the press; the server reaches the owner's final position 7-9 ticks after the release at 60 Hz (lead 8-9 ticks), 6-8 at 30 Hz (lead 7-8), 13-14 at 120 Hz (lead 10-14); a 50 px server impulse causes exactly one rollback (12-13 replayed ticks at 60 Hz, 20 at 120 Hz) and the owner converges 4-8 ticks later; no rollback in steady motion; with 66 % input loss the server's copy never stalled in 60 ticks. Deviations: (1) `Session::set_interacting` is already WP3's snapshot-priority hook, so the adaptive-delay flag is `Session::set_interaction(bool)`. (2) Mismatch detection compares the full confirmed mirror (the server's latest value of every hot field) with the history at the newest confirmed tick of the step, for every predicted entity, instead of per entity at each entity's own T; a snapshot that names no predicted entity still confirms its tick (a server that did not move the player is caught); several snapshots in one step = the newest tick wins; a field whose update was lost may cause one spurious rollback, corrected by the next snapshot. (3) `predict = disabled` means no prediction: the owner does not simulate (`_physics_process` off, like a replica), applies snapshots and still sends its input; netcore's `Disabled` policy never rolls back. (4) `gdnet/prediction/max_rollback_ticks` unset = 20 ticks, raised to a third of a second above 60 Hz (40 at 120 Hz): at 120 Hz with a 100 ms round trip a 20-tick window made confirmations stale. (5) The tick mapping: the owner simulates predicted tick `local tick + lead`, lead = round(offset) + ceil(rtt/2 + min(jitter, rtt/4) + 2), raised at once and lowered only on a drop of 4 or more; when it grows, gdnet simulates the skipped predicted ticks at once as fresh catch-up ticks (the frames for them are the previous frame, as the server's repeat), so a lead change costs no correction. (6) The global-`Input` warning (D14) is the source scan (the comparison heuristic cannot work on the owner, whose sampled frame is the real `Input`); it runs when an entity gets a client owner, on the server and the owner, once per script. (7) `rolled_back` is also on `NetSession` / `Net` (with the node), since rung 1's player has no `NetEntity`. (8) Float properties default to a prediction tolerance of 1e-4 (others keep 0.001; ints, bools, strings compare exactly whatever the number). Contract choices: `Net.input()` of an entity the server owns is live on every peer (custom values kept on the object); offline / unmanaged nodes get a fresh live `NetInput` per call; `set()` only acts on the owner in a fresh tick; `_net_input()` runs for every entity the local peer owns (the server's and offline too); `_net_tick` runs from the session's step (-1000), before the other nodes' `_physics_process`, tick = the server tick (server) or predicted tick (owner); replays call `_physics_process` only on parts whose physics processing is on; `from_tick` of `rolled_back` is the last predicted tick simulated, `to_tick` the restored confirmed tick; cold properties of an owned entity still snap to the server's values; client-spawned (`owner_allowed`) entities are not predicted and read the real `Input` on their owner; the default action list is sorted. |
| WP5 interpolation and smoothing | built 2026-11-08: `netcore` `interp` (`Timeline`: the render tick one jitter buffer behind the leading edge of the snapshot arrivals, advanced by real time at 0.9x..1.1x, the adaptive buffer 1..6 ticks growing by one when more than 2 % of the last second's snapshots were late (at most every 250 ms) and shrinking by one after 4 s of headroom; `Track`: per-field ring of (server tick, value) samples with linear / shorter-arc angle / slerp / step blending, extrapolation capped at one tick, hold samples, corrections against the stream's heading turned into decaying offsets (0.95 / 0.85 per 60 Hz frame, scaled by frame time), teleports; `Interp`: the tracks, owned offsets, `blend_owned`), `Session` routing (a client's `interpolate` fields of entities it does not predict go to the sample buffers, not `drain_state_updates`; `interp_frame`, `interp_seed`, `blend_owned`, `apply_owned_offset`, `owned_offset_keys`, `interp_reset`, `set_interpolation`, `is_interpolated`), `PropDesc.teleport_distance`; `gdnet` `interp.rs` (settings), `NetSession._process` (priority -1000, client sessions only) writing sampled replica fields (`NetManager::apply_interp`) and owned offsets (`show_owned`, write/restore), the restore at the tree's `physics_frame` (and in `step()`), `replay::rollback` blending (`interp_hot` before and after), seeds from a replica's `Create`, `Net.jitter_buffer_ticks` and `NetSession.jitter_buffer_ticks`, the `configure()` / profile / `state_options` option `teleport_distance`, `stats()` keys `jitter_buffer_ticks`, `render_lag_ticks`, `late_snapshots(_per_s)`, `offset_corrections(_per_s)`, `interp_teleports`, settings `gdnet/interpolation/{jitter_buffer_ticks,adaptive,teleport_distance}`; the shipped velocities interpolate (`default_profiles.tres` updated); tests `tests/integration/test_net_interpolation.gd` (9: G5 render 60/144 Hz smoothness, no backwards motion and the lag, a 20 px correction and a teleport, the owner's blend with exact `_physics_process` values, cold snaps, `interpolate = false` and `teleport_distance` via `configure()`, the `deg` seam, the buffer under 40 ms jitter, settings and G0) with fixtures `tests/fixtures/net/{mover,mover_snap,mover_teleport,blend_player,crate_snap}`; guide `docs/guide/05_interpolation.md`; `CHECK: PASS` (TESTS: 76 passed, 0 failed, 0 errors, 0 skipped (76 total); netcore 127 Rust tests, gdnet 2). Measured (physics 60 Hz, a 120 px/s replica): render 60 Hz per-frame displacement 2.000 px, sd 0.014 px (variance 0.0002 px²); render 144 Hz 0.833 px, sd 0.025 px (variance 0.0006 px²); never backwards; `render_lag_ticks` 2.00, lag behind the server 2.99 ticks (60 Hz) and 2.55 (144 Hz) with the default 2-tick buffer; a 20 px correction spread over 172 frames, largest step 0.43 px; after a 50 px impulse the owner's largest rendered step 3.6 px with all 198 `_physics_process` calls exact; 40 ms jitter: the buffer grew 2 -> 3 and late snapshots fell to 0/s; G5 prediction unchanged (30/60/120 Hz: 8/9/13 ticks after the release, one rollback each). Deviations: (1) the render timeline is anchored to the snapshot arrivals (a leading-edge estimate of `snapshot tick - local clock`), not to `server_tick_estimate()`: that estimate includes the one-way delay, so `estimate - buffer` would sit ahead of the newest snapshot; 2.5's "behind the newest snapshot" is what is built. (2) The decay factors are per 60 Hz frame scaled by the frame time (`f ^ (dt * 60)`), so smoothing is rate independent (G5). (3) Thresholds for `Vector2` are in pixels at 100 px per metre (large 25 px, teleport 200 px); a correction needs two earlier samples and an error above 5 cm / 5 px / 5 degrees; a replica's correction is measured against the extrapolation of its last two samples (the start of a fast motion, above 5 px per tick, also counts: it blends in a tick early). (4) Snapshots only carry changes, so a hold sample is inserted when the entity appeared without the field or the gap exceeds max(6 ticks, 0.1 s), replicas are seeded from their `Create`, and extrapolation stops when a newer snapshot arrived without the field; with no snapshot at all for two more ticks (an idle world) it settles back onto the newest sample over one tick (a stopped entity may step back by up to one tick's motion once). (5) Owned offsets come off at the tree's `physics_frame` signal (the session connects first), not only in the -1000 step: code awaiting `physics_frame` resumes before the -1000 step and WP4's tests read positions there. (6) Stale (overtaken) snapshots count as arrivals for the jitter measurement. (7) `teleport_distance` is a `PropDesc` field (part of the table, hashed only when set). (8) Two WP3 tests changed: G7's velocity tolerance is 6 instead of 2 (velocity now interpolates, a jitter buffer plus transit behind), and the halved-budget G4 test counts updates on `crate_snap.tscn` with `interpolate = false` (interpolated replicas change every frame); G5's prediction test is untouched. Contract choices: only client sessions interpolate or run `_process`; the owner of a `predict = disabled` entity interpolates it like a replica; `render_lag_ticks` is the leading-edge estimate now minus the render tick; offsets under 1 % of the epsilon are dropped (the field lands exactly); `offset_corrections` counts replica corrections and owner blends. |
| WP6 auto-exposed methods and verbs | built 2026-11-08: `netcore` `call` (`CallMsg` kind 11 and `CallReturn` kind 12; method ids = FNV-1a 32 of the name; `Arg`, the postcard argument encoding; `Verb` = `Sync` / `Server` / `Owner` / `Others` / `Peer`; `OutCall`, `CallPlan`, `CallIn`, `CallResult`, `CallStats`, the 10 s request timeout), `session` (`call()` routes by role and verb, `finish_call()` / `refuse_call()` after gdnet's verdict, `register_methods()` checks received hashes, `poll_call_result()`, request ids and timeouts, an `in_scope()` hook that is "every joined peer" until WP7/WP13), received calls go through the held queue as an internal `EntityAction::Call` so they wait behind a pending `Create` and keep the order per entity, `EntityEvent::Call`; `ScenePropsTable.methods` (per-part method table hashes, hashed only when present; the diff names the part); `gdnet` `methods.rs` (per part: the script's methods at every inheritance level plus `ClassDB` methods with inheritance, minus `_`-prefixed names and gdnet's own nodes; sorted, hashed; trailing-`NetCallInfo` detection; cached per scene), `call.rs` (`NetCall`, `NetCallInfo`, the argument rules, the call-site checks, `NetArg`), the verbs `sync` / `sync_unreliable` / `server` / `owner` / `others` / `peer` on `Net` and `NetSession`, `NetManager` "Calls" block (`call_out`, calls queued until the entity has a net id, `dispatch`, `on_call` with the D14 trust check and `_net_allow`, `callv` with the info appended, `mark_cold_dirty` after every run), signal `call_refused(node, method, sender)` on `NetSession` and `Net`, `NetSession.stats()` `calls_*`, test hook `_debug_extra_method`; tests `tests/integration/test_net_calls.gd` (13: rung 2 in both host modes with two clients, `await Net.server`, the single-player path and G0, built-in methods and bound arguments, owner/others/peer, refusal and `_net_allow`, `NetCallInfo`, a call on a same-tick spawn, a call in a replayed tick, call-site errors, a resource by path, the method-table mismatch, G6) with fixtures `tests/fixtures/net/{door,call_probe,calling_player}`; guide `docs/guide/06_calls.md`; `CHECK: PASS` (TESTS: 89 passed, 0 failed, 0 errors, 0 skipped (89 total); netcore 140 Rust tests, gdnet 2). G6 (seed 20261108, 30 ms each way, 10 % loss, 10 % reorder, 200 server steps): 48 spawns (22 with a call in the same tick), 51 reparents, 20 destroys, 157 calls, 5 survivors; the client matched the server's entities and ran every survivor's calls once, in order, with no engine error, in 3.5 s. Deviations: (1) gdext 0.5.5 has no varargs `#[func]` and refuses `Variant` as an `#[opt]` default, so each verb takes eight optional arguments of a transparent `Variant` newtype (`NetArg`, unsafe `GodotImmutable` impl) whose default is a private RID sentinel: `Net.sync(open_door, 7)` reads as specified, with at most eight arguments after the Callable. (2) `await Net.server(f, x)` works as specified because `server` returns a `Signal` (its `NetCall`'s `completed`); the other verbs return the `NetCall` (`ok`, `error`, `result`, `suppressed`). (3) The method table hash joins the scene's property table (`ScenePropsTable.methods`, carried by the `Scene` action), not the part table hash, so a mismatch names scene and part. (4) A call on an entity that has no net id yet (spawned this tick; a client's static content before its `Create`) is queued and dispatched after the spawn is announced, on the server too, so every peer applies the `Create`'s state and then the call. (5) The trust rule covers every verb but `server`: a client's `owner`, `others` and `peer` calls also need ownership or `_net_allow`. (6) A client's reliable calls always carry a request id (the server answers every one), so refusals and errors reach the caller for every verb; the server's own calls carry none, and an error on a target client (an unknown hash) is only counted. (7) `sync_unreliable` is the `unreliable = true` option; such calls have no request id, and one that arrives before its entity is dropped. Contract choices: `NetCallInfo.is_local` = the call was made on this peer (`sender` is this peer); `call_refused`'s node is the part; without a session in the node's branch a verb calls the method directly (single player), while an unmanaged node in a branch with a session is an error; a client's call resolves when the server routed it (`ok`) or refused it, a server's at once; `completed` is emitted deferred; a call's arguments are the given ones then the bound ones; calls to an entity destroyed before arrival are counted in `calls_dropped`. |
| WP7 authority and keyed state | built 2026-11-08: `netcore` `authority` (`Transfer` fixed/request/take and `decide()`, `StateMsg` for `OWNER_STATE` kind 13 (unreliable) and `WRITE` kind 14 (reliable), `WriteReject` kind 15, `write_checked()` with `LOCALLY_DRIVEN`, `AuthorityStats`), `keyed` (`KeyedState`: the map, `Who` All/Owner/Peer rules, views with per-peer decisions, `KeyMsg` kind 16 = Set/Erase/Full/Rules/Rejected, `KeyEvent`, `KeyedStats`), `entity` (`CreateAction`/`EntityRecord` carry `transfer` and `client_owned`; `EntityAction` gains `RequestOwner`, `ReleaseOwner`, `OwnerQuery`, `OwnerAnswer`, `OwnerReply`), `session` (`request_owner`, `release_owner`, `answer_owner_query`, queries refused after 5 s, `set_owner` emits a local `SetOwner` event on the server too, departed clients' entities go back to the server, `Spawn::with_authority` / `inheriting_owner`, owner states accepted from the owner of a `client_owned` entity, recorded and relayed, never echoed, `owns_state`, `remember_received`, `check_writes`, rejected writes answered with `WriteReject` and restored from the last server values, at most 3 rejects in a row per field, `keyed_set/erase/allow/view/set_visible`, the full map in the join burst, `EntityEvent::OwnerQuery/OwnerReply/WriteRejected`, `EntityEvent::manager()`), `delta` (`encode_values`, `last_change`); `gdnet` `authority.rs` (`_net_owner_changed` / `owner_changed` notification, `_net_grant_owner`, `NetCall` helpers), `keyed.rs` (`NetKeyedState`), `NetEntity.transfer/client_owned/inherit_owner` + `owner_changed` + virtual `_net_owner_changed`, `NetManager` WP7 parts (owner callbacks once per change on every peer, ownership queries, rejected-write restore, `request_owner`/`release_owner`, `state_is_local`, the who-simulates rule for `client_owned` entities (the owner only; never predicted; live input), `record_client_owned`, `poll_writes`, the server skips recording a client's `client_owned` entity, `apply_states` skips what this peer is the source of), `NetSession.state`, `owner_changed`/`write_rejected` signals, `stats()` WP7 keys, view filters re-evaluated per server tick, `Net.request_owner/release_owner/state` + relayed signals; tests `tests/integration/test_net_authority.gd` (11: nested inheritance in both host modes, `set_owner` moving prediction with `owner_changed` and `_net_owner_changed` once on every peer, `take`, `request` (server-owned granted, client-owned refused by default, granted by `_net_grant_owner`), `fixed`, release, a `client_owned` box in both host modes (owner never oscillates: every step exactly 2 px), an `owner_allowed` spawn's movement relayed, a rejected write snapping back with the signal on both sides, keyed state to a client and a late joiner, permissions, a view, G0 with `Net.state` offline) with fixtures `tests/fixtures/net/{owned_player,owned_box,owned_nested,split_keep}`; guide `docs/guide/07_authority.md`; `CHECK: PASS` (TESTS: 100 passed, 0 failed, 0 errors, 0 skipped (100 total); netcore 156 Rust tests, gdnet 2). Deviations: (1) rejected writes are detected by polling: every `cold_poll_ticks` a client reads the non-interpolated fields of the replicas it neither owns nor simulates and netcore compares them (canonical encodings) with the last values the server sent; interpolated fields, slow fields and the engine-driven `frame`, `frame_progress`, `playing`, `current_animation`, `current_animation_position`, `emitting` never count; a field rejected 3 times in a row is not checked again until the server sends a new value for it (code rewriting it every frame, or a value that does not survive a write and read back). (2) One signal `write_rejected(node, sender)` on `Net`/`NetSession` for both sides (on the writer `sender` is its own id) instead of `write_rejected(node)` there; the reject message is `WriteReject` (`Reject` is the handshake's). (3) Owner state is not acked: the owner sends the full recorded state of each `client_owned` entity it owns every tick it changed and for 3 ticks after, plus a keyframe each second, packed under 1,100 bytes; the server drops a state older than the newest it applied, records it in its `Replication` and relays it in its snapshots (the snapshot skip rule is now `client_owned && owner == peer`); a host-as-player server shows it snapped. (4) A client's own spawn (`owner_allowed`) is always `client_owned` (netcore forces it), whatever its `NetEntity` says. (5) The "registered policy" for `request` is gdnet's `_net_grant_owner` hook on the current owner's copy (the server for a server-owned entity, the owning client otherwise; `OwnerQuery`/`OwnerAnswer`); an owner that does not answer within 5 s refuses. (6) Inheritance is resolved by netcore at spawn from the parent entity's current owner (`Spawn::inheriting_owner()`); gdnet asks for it unless the entity has the `_net_owner` meta (`spawn_for`, `set_owner`), an `owner_peer` other than 1, or `inherit_owner = false`. (7) A client that leaves gives its entities back to the server (they still stay, WP2). (8) On the server `owner_changed` fires at the tick after `Net.set_owner` (the change goes through netcore's event queue like a remote one), while `get_owner_of` changes at once. (9) Tests were written after a first draft of the code and then run against the WP6 library, where they failed on the missing API (`request_owner`, `NetKeyedState`, `Net.state`), before the WP7 build. Contract choices: `request_owner` returns a `NetCall` whose `completed` carries the grant (true at once for the owner, without a session or for an unmanaged node); `release_owner` on the server is `set_owner(node, 1)`; a client's allowed keyed write applies at once on the writer and the server's refusal rolls it back (`write_rejected(path)`); a refused-locally keyed write prints an error and returns false; peers without a decision for a view do not see its paths (a joining peer gets them the tick after its join); `NetKeyedState` values follow the WP6 argument rules; without a session `Net.state` is a local map with signals and sends nothing; `client_owned` entities owned by a client are simulated by the owner only (also on a host-as-player server), never predicted, and read the real `Input` there; `simulate = everywhere` still runs everywhere; `transfer` and `client_owned` are mirrored on a replica's `NetEntity`. Hand-off: WP8's meter reads the `stats()` keys above; per-entity hashes of hot state are cheapest where `Replication::record_part` already holds each field's quantised encoding (`delta.rs`, `FieldState.enc`); the ownership overlay reads each entry's owner and `client_owned` (`EntityRecord`), and `Session::owns_state()`. |
| WP8 diagnostics | built 2026-11-08: `netcore` `hash` (FNV-1a 32 per entity over the hot fields' quantised encodings, `HashMsg` kind 17 on channel 2 with `settled` and `carried` entries, `HashTx` round-robin sampling, `HashRx` (the client's held encodings re-encoded from the decoded values, a 32-entry `(tick, hash)` history per entity, the applied-tick ring, a stash for hashes that arrive before their snapshot), `HashStats`, `REFRESH_S`), `delta` (`Replication::entity_hash`, `hash_eligibility`, `hashed`, `entity_ids`), `session` (`set_hash_telemetry`, `set_hash_refresh`, `hash_stats`, `divergences_of`, `state_hash(tick)` for WP12, the test hook `debug_corrupt_held`, `send_hashes` after the snapshots, client notes in `route_updates` and `remember_received`), `emul` presets `lan` and `mobile` (+ `PRESETS`); `gdnet` `hash.rs` (settings), `meter.rs` (show/hide, the toggle key), `overlay.rs` (`NetOverlay`, roles and colours), `editor.rs` (`GdNetEditorPlugin` + `GdNetDebuggerPlugin`, a "gdnet" debugger tab fed by `gdnet:stats` / `gdnet:entities`), `NetSession.overlay_rows()` / `divergences(node)` / `_debug_corrupt_held`, the new `stats()` keys and per-second windows, the editor feed from `Net`'s session, `Net.show_meter` / `meter_visible` / `show_overlay` / `overlay_visible` / `overlay_rows` / `stats` / `divergences` / `emulation_preset`, `gdnet/debug/emulation_preset` applied at `Net`'s session start, `NetEmulation.preset("custom")`, `NetPipePeer` byte and packet counters, the manager's `overlay_entries` / `part_address` and the rejected-write time; `addons/gdnet/meter.tscn` + `meter.gd` (`class_name NetMeter`); tests `tests/integration/test_net_diagnostics.gd` (7) and netcore hash tests (hash equality, a 1 mm change, sampling, settled/carried comparison at equal ticks only, lost snapshots skipped, off sends nothing, idle not resent, the quantisers re-encode identically); guide `docs/guide/08_diagnostics.md`; `CHECK: PASS` (TESTS: 107 passed, 0 failed, 0 errors, 0 skipped (107 total); netcore 166 Rust tests, gdnet 2). Measured: the G4 idle scenario stays at 41 B/s (hashes of idle entities are not resent within 10 s); the meter's byte counters equal the pipe's (minus `SceneMultiplayer`'s byte per raw packet) within 64 bytes over a second. Deviations: (1) the server folds the hash at send time over the encodings recorded this tick (only for the sampled entities), not at record time for every entity: the same bytes, cheaper. (2) Hashes are sent per peer and only when that peer can check them at that tick: *settled* (every hashed field acknowledged at its current version) or *carried* (the rest acknowledged, the changed fields in this tick's snapshot to that peer, compared only if the client applied that snapshot); without this a moving replica could never be compared at an equal tick and a lost snapshot would count as a divergence. (3) An entity whose hash did not change since a peer last got it is skipped until `gdnet/debug/hash_refresh_s` (10 s) passed, a setting the plan did not name: the brief's 16 entities per tick cost ~6 KB/s on an idle world and broke gate G4's 400 B/s ceiling. (4) The client hashes what netcore holds (the values the server sent, re-encoded), not what the nodes show: interpolation and prediction act on nodes, and a node write is the rejected-write poll's job (WP7); so the integration test's deliberate edit uses the hook `NetSession._debug_corrupt_held(node, prop, value)` (on a hot field the server does not resend while the entity keeps moving). (5) Only hot, non-reliable fields of at most 1,000 encoded bytes are hashed (cold ones arrive on their own schedule; reliable ones travel outside snapshots). (6) `NetMeter` is a GDScript controller (`class_name NetMeter`) in `meter.tscn`, the data side (`stats()`) Rust; layers are added deferred (they work from any `_ready`), so they appear in the tree on the next frame. (7) The editor debugger plugin is built with gdext's `EditorDebuggerPlugin` virtuals (`setup_session`, `has_capture`, `capture`), which 0.5.5 exposes; it loads under `--headless --import` with no error, but the tab is verified by hand only. Contract choices: overlay role precedence `rejected` > `client_owned` > `server` (owner 1, also the host's own entities on the host) > `own` > `other`; a row is red for 1 s after a `write_rejected` on that peer (writer and server); `overlay_rows()` adds `color` and `node` to the listed keys; the overlay shows every session in the tree; `entities_in_scope` = every entity until WP13; `budget_use` is server-side snapshot bytes per second over budget x peers (0 on clients); `rtt_ms` / `jitter_ms` are the mean over joined peers; `server_tick_offset` = a client's server tick estimate minus its tick; `gdnet/debug/emulation_preset` applies only to `Net`'s sessions and only when `Net.emulation` is unset (an explicit resource wins); `Net.stats()` is `{}` without a session; the toggle key is read only by `Net`'s session (fixture sessions never toggle). |
| WP13 demo: Doors (players travel between scenes) | built 2026-11-08: `netcore` `rooms` (`Rooms`: manager key -> room hash with a name table and the level scene, players, `set_room` overrides, `strict`, the rule `sees()`; `room_hash`, `GLOBAL` = `"*"`), `entity` (`EntityAction::Travel { scene_path, room }` server -> client, `TravelAck { room }`, `Scope { leave }`; `EntityTable::reparent` moves the descendants to the new manager; `subtree`, `root_of`), `delta` (`forget_peer_entity`), `session` (per-peer scope on the server: the late-join burst, `spawn`, `accept_create`, destroys, reparents, owner changes, snapshots' skip closure, `send_hashes`, `broadcast_call` and the owner-state relay all filtered by it; `refresh_scope` sends `Travel` when a peer's room becomes a level, then `Scope` leaves, then `Create`s parents first with full state; a cross-manager reparent refreshes every scope and sends `Reparent` to the peers that saw it before and still do; a client frees left entities like a destroy (`HashRx::forget` through `forget_input`); `set_rooms`, `set_rooms_strict`, `set_player`, `set_room`, `peer_room`, `manager_room`, `players`, `scope_len`, `travelling`, `travel_ack`, `ScopeStats`, `Spawn::as_player(PlayerMark)`, `EntityEvent::Travel`); `gdnet` `NetManager.room` + `room_setting`, `take_entries`/`put_entries`/`set_entry_parent`/`destroy_all`, the `_net_player` meta (`spawn_for` = first entity, `set_player` = always), `world.rs` (the World group, level names, instantiation, spawn markers, `_net_travelled`, the timeout setting), `travel.rs` (queued travel, entry and node moves between managers, client `Travel` loading + `TravelAck`, held cross-manager reparents, freeing stale levels, the empty-level timer, `register_rooms` each server tick), the `NetSession` / `Net` surface of 3.1, the `rooms` meter line, overlay `room`, `NetKeyedState` is `#[class(tool)]` (the import is silent); demo `demos/doors/` (`main.tscn/.gd` with `--server`, `world.gd`, `hub/cave/garden.tscn`, `door.gd`, `player.tscn/.gd`, `chat.tscn/.gd`, `torch.tscn`) and its README tutorial; tests `tests/integration/test_doors_demo.gd` (4: travel, scope both ways, chat across rooms, meeting again, `entities_in_scope` per peer and overlay rooms; a late joiner into the cave; the hub unloading after 0.5 s and reloading; G8's README listings) in both host modes, netcore `rooms_scope_creates_snapshots_and_a_move_between_rooms`, `a_late_joiners_burst_covers_its_room_only` and the `rooms` unit tests; guide `docs/guide/09_rooms_and_travel.md`; `CHECK: PASS` (TESTS: 111 passed, 0 failed, 0 errors, 0 skipped (111 total); netcore 171 Rust tests, gdnet 2). Deviations: (1) the destination room is not a field of `Reparent`: the `ParentRef`'s manager key names the destination manager, and the room travels in the `Travel` action (scene path and room name) that precedes it; the player's position travels in the next snapshot (no transform in the action): the owner re-tracks prediction and rolls back to it. (2) `Travel` is sent whenever a peer's room becomes a room that is a level (a manager inside a level loaded with `load_level`), not only on `travel`: a new player's client is told to load its start level the same way. (3) Rooms are strict only once the server uses levels (`load_level` / `travel`): before that a peer without a room sees every entity, so single-level games and every earlier test are unchanged; strict, it sees only `"*"` managers (a late joiner's burst is then empty until its player is spawned). (4) An entity whose topmost entity a peer owns is always in that peer's scope. (5) `Net.travel` is queued and applied at the session's next step (a door's `body_entered` runs inside the physics flush, where moving a body is refused). (6) The World fallback is the session's branch root, not "the manager's parent". (7) A client drops a `Destroy` for a manager it no longer has (its level was freed) unless earlier events for that manager are still waiting. (8) Moving an entity between managers by hand is still destroy + create; only `travel` moves it. (9) The demo's scripts reach their branch's session with `Net.session_for(self)` (added) so the same files run under `NetFixture`; `door.gd` checks the body belongs to its own level (a server's levels share one physics space). Contract choices: room names default to the manager owner's `scene_file_path`, else the branch-relative path; level nodes are named after the scene file's stem; `levels()` lists scene paths in load order; `travelled` fires for every cross-manager move a peer applies (the server, and clients that saw the entity before and after: the owner); on the server `entities_in_scope` is every entity and `entities_in_scope_by_peer` each peer's; a client's `travelling` is 1 from a `Travel` until one of its own entities is in that level; the empty timer counts players (`Rooms::players`), so a level holding only server-owned non-player entities counts as empty; unloading destroys the level's entities first. Hand-off (WP14): `Session::set_interacting(peer, id, on)` is still the only interacting hook (no caller yet; score bodies near a client's player each tick before `send_snapshots`); budget degradation is visible in `repl_stats().entities_sent` per second and the halved-budget G4 test, there is no dedicated degradation counter (add one in `Replication::build` where candidates are left out); hash sampling stays 16 entities per tick with unchanged hashes skipped for 10 s, so 300 settled bodies cost almost nothing and moving ones cost ~16 x 8 bytes per tick per peer; a single-level physics demo needs no rooms: one manager means one room, no `load_level` means non-strict, so every peer sees every body and no `Travel` is sent. |
| WP14 demo: Ballpit (many colliding bodies) | built 2026-11-08: `netcore` `priority` (`near()`: the proximity half of interaction scoring over a coarse grid of cells as large as the largest radius, `Body`, `cmp()`), `delta` (`ReplStats` gains `deferred_entities`, `deferred_fields` and their `_tick` forms, counted in `Replication::build` where candidates with pending fields are left out of a snapshot (budget at zero, MTU or allowance full) and where an entity's remaining fields did not fit; `begin_tick`, `interacting(peer)`, `sends(peer, id)` (a per-peer, per-entity count of snapshots that carried it)), `session` (`interacting`, `entity_sends`, `begin_tick` in `send_snapshots`); `gdnet` `physics.rs` (the interaction pass: entity roots' global positions (`Node2D`/`Node3D`), contacts of the player's body (`get_colliding_bodies()` with `contact_monitor`, a CharacterBody's slide collisions), the setting `gdnet/budget/interaction_radius`), `NetManager.interaction_radius` and `roots()`, `NetSession` (the pass each server tick after recording and before `send_snapshots`, scoring every peer but the server's own player and re-asserting the set every tick; `interacting(peer)`, `entity_sends(node, peer)`, `hash_sample`, the `stats()` keys of 3.1 with per-second windows); the meter's `budget` line gains `deferred: N ent/s  interacting N`; demo `demos/ballpit/` (`main.tscn/.gd` with `--server`, `pit.tscn` + `spawner.gd` (the Spawner: balls in loose rows, a paddle per peer, `set_ball_count`, `set_trusted_paddles`, `shake`, `layer_shift`), `ball.tscn` (a script-less `RigidBody2D`), `paddle.tscn/.gd` (rung 1), `trusted_paddle.tscn` (the same scene with a `client_owned` `NetEntity`), `hud.tscn/.gd`) and its README tutorial with a bandwidth chapter; tests `tests/integration/test_ballpit_demo.gd` (4: 300 balls settle, converge and cost nothing asleep in both host modes, with the trusted-paddle toggle and its overlay role; a predicted paddle push over a 50 ms link with the interaction pass checked against the radius, no double simulation and no rollback; the halved budget with near and far send rates, the deferred counter and hash bytes; G8's README listings) and netcore `three_hundred_bodies_pack_into_mtu_sized_snapshots`, `near_bodies_are_sent_ten_times_as_often_as_far_ones`, `deferred_counters_count_what_the_budget_left_out`, `near_lists_bodies_within_their_radius`; guide `docs/guide/10_physics_and_bandwidth.md`; `CHECK: PASS` (TESTS: 115 passed, 0 failed, 0 errors, 0 skipped (115 total); netcore 175 Rust tests, gdnet 2). Measured (headless, physics 60 Hz, release build): 300 balls fall asleep 2.2-2.4 s after spawning; asleep, the server sends a client 56 B/s in total (0 snapshot bytes, 0 recorded changes over a second; pings only), so a sleeping `RigidBody2D` produces no dirty field and needs no dead band (Godot does not move a sleeping body); a body update is 20.6 bytes (4 hot fields ~15.5 bytes at `mm` / 16-bit angles plus a ~5-byte entry header: more bit, id varint, record-length varint, 9 mask bits) [review 2026-11-08, D17: 15.8 bytes, positions and velocities 2 bytes per axis at 0.1 px], and 300 moving bodies fill every snapshot to within 24 bytes of the 1,200-byte MTU; in netcore at 32 KB/s 20 interacting bodies went out on 600 of 600 ticks and 280 others 15 times each (40x); the push test (50 ms each way): the owner's paddle moves on the tick after the press, the owner sees the first ball move 5-6 ticks after the server moved it (67 ticks after its press, most of it the paddle's travel to the pile), 17-18 balls interacting at the deepest point, zero rollbacks, the server's pile asleep again ~110 ticks after the push and the client within 0.5 px 32 ticks and within 0.05 px 77 ticks after that (WP5's decaying correction offsets) [D17: a replica now sits up to half a 0.1 px step off, so the test's bar is 0.075 px; measured 6 and 41 ticks]; with the budget halved to 16,384 B/s and 300 shaken balls: 16,387 B/s of snapshots in a one-second window, 273 bytes per snapshot (about 13 ball updates a tick), ~17,200 deferred entities/s and ~68,800 deferred fields/s, interacting balls sent on 100 % of the ticks they moved (354 slots), the far moving balls on 3 % (35,646 slots) [D17: about 17 updates a tick, far balls 5 %, hashes and pings 88 B/s]; hash telemetry at `hash_sample = 2` plus pings 66 B/s on channel 2 (0.4 % of that budget); the interaction pass 0.36-0.56 ms per tick for 300 bodies and one client (a linear Rust pass over engine getters, the grid query itself negligible). Deviations: (1) the budget slider sets the host session's `budget_bytes_per_s` (`Net.session_for(pit)`, which is `Net`'s session in a game) instead of `Net.budget_bytes_per_s`, which is read when a session starts (3.4) and so does not reach a running session. (2) `hash_sample` is a `NetSession` property (no `Net.hash_sample`): the demo sets it on its session in `start()`. (3) The walls sit beside the `NetManager`, not under it (level geometry, identical everywhere; under the manager they would be bare-node entities with a warning). (4) The paddle's collision mask holds only the walls (the balls' mask holds the paddle): its motion never depends on replicas, so prediction never fights the server, and a push is seen one trip from the server plus the jitter buffer late; a player that collides with frozen replicas would be stopped by the past and roll back (documented, no library change). (5) The demo's `interaction_radius` is 50 px, not the 300 px default: at the halved budget a snapshot carries ~13 ball updates, and a larger radius marks more moving balls than fit (100 px measured 75 % of near balls sent per tick). (6) The balls have `angular_damp = 3` (a ball rolling on the floor otherwise dozes off and wakes for many seconds) and the push test lifts the paddle out of the pile afterwards (a kinematic paddle resting inside the pile keeps nudging balls awake). (7) An entity-id delta encoding within a snapshot was considered and not built: it would save at most ~1 byte of 20.6 per update while making the MTU accounting depend on the write order; the larger lever is the quantiser (a `Vector2` at `mm` is 1/1000 px, `cm` saves a byte per axis), left as a project choice. (8) Tests set `timeout_sec` (20-40 s): a 300-body pile needs seconds of simulated time. (9) The G4 and G5 tests stay on their own fixtures, untouched (the row said they would reuse this scene); the demo's halved-budget test is the bodies' bandwidth gate. Contract choices: interacting = within the radius of the entity's own manager, measured between entity roots' global positions, plus the player's contacts; `interaction_radius` 0 = the setting, negative = proximity off for that manager, 3D reads the setting at 100 px per metre; the set is re-asserted every tick (an entity that left a peer's scope and came back gets a fresh per-peer record); `deferred_entities` counts candidates that carried nothing this tick, `deferred_fields` every pending field not carried (partly sent entities included), both summed over peers; `stats()` per-second figures are the last full second; `entity_sends` and `interacting` answer 0 / empty on a client. Hand-off: WP15 hands touched bodies to the pusher; the hooks it needs are the interaction pass's contact list (physics.rs `contacts_of`) and `client_owned` (WP7). |
| WP9-WP12, WP15 | not started |

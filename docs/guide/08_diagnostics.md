# 8. Diagnostics: the meter, the overlay, hash telemetry, emulation

Godot ships no network meter, no emulator and no way to see who owns what. gdnet does: a meter panel,
an ownership overlay in game, a "gdnet" tab in the editor's debugger, per-entity state hashes compared
between the server and each client, and network emulation presets (`docs/PLAN_NETCODE.md` 1 "What the
developer sees", 2.4 `Hash`, 3.1, 3.4, WP8). None of it costs anything until a session runs (gate G0),
and none of it changes what replicates.

## The meter

```gdscript
Net.show_meter(true)        # adds addons/gdnet/meter.tscn under the root as __NetMeter
Net.show_meter(false)       # frees it
Net.meter_visible()
```

While `Net` has a session, the input action `gdnet_toggle_meter` toggles it when the project defines
that action, else **F9**. The setting `gdnet/debug/meter = true` shows it whenever a session starts.

The meter is a `CanvasLayer` (`NetMeter`, a GDScript controller in `addons/gdnet/meter.gd`) with plain
labels, refreshed four times a second from a session's `stats()`: `Net`'s default session, else the
first session in the tree (set `meter.session` to choose one, e.g. a test branch). What it shows:

| line | from `stats()` |
|---|---|
| RTT, jitter | `rtt_ms`, `jitter_ms` (to the server on a client; the mean over clients on the server) |
| tick, server offset | `tick`, `server_tick_offset` (a client's estimate of the server tick minus its own) |
| jitter buffer, render lag | `jitter_buffer_ticks`, `render_lag_ticks` (page 5) |
| in / out per channel | `bytes_received_per_s`, `bytes_sent_per_s` (0 reliable, 1 unreliable, 2 diagnostics) |
| entities, in scope | `entities`, `entities_in_scope` (a client: what it holds; the server: everything, with `entities_in_scope_by_peer`; page 9) |
| snapshots, keyed | `snapshot_bytes_per_s`, `snapshot_bytes`, `keyed_bytes_per_s` |
| budget | `budget_use` (server: snapshot bytes per second over the budget times the peers), `budget_bytes_per_s` |
| calls | `calls_sent_per_s`, `calls_received_per_s`, `calls_refused` |
| prediction | `rollbacks_per_s`, `replayed_ticks`, `inputs_repeated` (page 4) |
| poll | `poll_ms`, `poll_ms_avg`: what reading the hot and cold properties cost (page 3, "Dirty tracking") |
| hashes | `hashes_sent`, `hashes_received`, `divergences`, `divergences_per_s` |
| authority | `owner_changes`, `writes_rejected` (page 7) |
| rooms | `room` (a client's), `rooms` (entities per room), `travelling` (page 9) |

Byte counts are what gdnet handed to the transport: `SceneMultiplayer` adds one byte per raw packet,
which a `NetPipePeer`'s `bytes_sent()` / `bytes_received()` include (`packets_sent()` /
`packets_received()` count them). `Net.stats()` is the default session's `stats()` (empty without one);
`NetSession.stats()` serves any branch.

## The ownership overlay

```gdscript
Net.show_overlay(true)      # adds a NetOverlay CanvasLayer as __NetOverlay
Net.overlay_rows()          # the same data: one Dictionary per managed entity
```

Each frame the overlay puts a small label at every managed entity whose root is a `Node2D` (its canvas
position) or a `Node3D` (projected through the current `Camera3D`; skipped without one), reading
`#<net id> owner <peer>`, coloured by role, Roblox-style:

| colour | `role` | meaning |
|---|---|---|
| white | `server` | server-owned (also on the server itself, and the host's own entities) |
| green | `own` | owned by this peer |
| grey | `other` | owned by another client |
| amber | `client_owned` | `client_owned = true`: the owner's state is accepted as is (the trust label, D9; page 7) |
| red | `rejected` | a write to it was rejected in the last second (on the writer and on the server) |

`overlay_rows()` rows: `net_id`, `owner`, `role`, `color`, `client_owned`, `transfer` (0 fixed, 1
request, 2 take), `scene`, `parts`, `hashes_diverged` and `node`. `NetSession.overlay_rows()` gives a
branch's rows; the overlay shows every session in the tree. `gdnet/debug/overlay = true` shows it when a
session starts.

## Hash telemetry

The server folds a 32-bit FNV-1a hash over each entity's **hot** fields, exactly the quantised bytes a
snapshot carries, and sends a sample of them on channel 2 (`Hash`, unreliable): 16 entities per tick
round-robin (`gdnet/debug/hash_sample`), plus every entity a client owns when it owns at most eight. A
client hashes the values it holds (what snapshots, reliable fields and the `Create` delivered, decoded
and encoded again; for a predicted entity that is the confirmed state, not the prediction) and counts a
**divergence** when the two differ at the same tick.

Replicas legitimately lag, so only equal ticks are compared: the server sends an entity's hash to a
client only when it knows what that client holds at that tick (every hot field acknowledged at its
current value, or the changed fields in this very tick's snapshot), and a client compares the second kind
only if it applied that tick's snapshot. A lost snapshot or an unknown tick is skipped
(`hashes_skipped`), never counted. Nothing halts and nothing is corrected: it is telemetry.

```gdscript
Net.divergences(node)       # this client's count for the entity node belongs to
NetSession.stats()["divergences"], ["divergences_per_s"], ["hashes_sent"], ["hashes_received"]
```

What a divergence means: the client holds a value of a hot property that is not what the server sent
for that tick. Causes: code (or an engine system) on the client wrote state it does not own in a way the
rejected-write check (page 7) does not see, a quantiser that does not survive a decode and encode, or a
bug in gdnet. Interpolation and prediction do not cause one (they act on the node, not on the held
values). Cold properties are not hashed (they arrive on their own schedule). `gdnet/debug/hash_telemetry
= false` sends and compares nothing. For deterministic sync tests (WP12) netcore exposes
`Session::state_hash(tick)`, the hash of every entity's hot state.

The test hook `NetSession._debug_corrupt_held(node, "prop" | "Part:prop", value)` replaces a client's held
copy of a property, so a test can watch the counter rise.

## Network emulation presets

```gdscript
Net.emulation_preset("bad")   # a new NetEmulation, applied to the running session at once
```

| preset | latency | variance | loss |
|---|---|---|---|
| `off` | 0 | 0 | 0 |
| `lan` | 5 ms | 0 | 0 |
| `average` | 30 ms | 30 ms | 1 % |
| `bad` | 100 ms | 100 ms | 5 % |
| `mobile` | 120 ms | 20 ms | 2 % |
| `custom` | the `gdnet/debug/emulation/<field>` settings (`send_latency_ms`, `receive_loss`, ..., `seed`) | | |

Each applies to both directions. `gdnet/debug/emulation_preset` (default `off`) gives `Net.emulation` a
preset when `Net` starts a session and no emulation was set; writing `Net.emulation` replaces it at once.
`NetEmulation.preset(name)` sets an existing resource. Over a `NetPipePeer` (offline and tests) every
packet is emulated; over another peer gdnet's own messages are (page 1).

## The editor's debugger tab

`GdNetEditorPlugin` (registered by the extension) adds a **gdnet** tab to each session of the editor's
debugger. When the game runs from the editor, `Net`'s session sends `gdnet:stats` (its `stats()`) and
`gdnet:entities` (its overlay rows) twice a second through `EngineDebugger`, and the tab lists them. This
part needs the editor and is checked by hand only.

## Settings

| setting | default | read |
|---|---|---|
| `gdnet/debug/meter` | false | when `Net` starts a session: show the meter |
| `gdnet/debug/overlay` | false | when `Net` starts a session: show the overlay |
| `gdnet/debug/hash_telemetry` | true | when any session starts |
| `gdnet/debug/hash_sample` | 16 | every tick (entities per tick, 0..1024); `Net.hash_sample` overrides it for every session (-1 = the setting), `NetSession.hash_sample` per session (-1 = `Net`'s), both at once |
| `gdnet/debug/emulation_preset` | `off` | when `Net` starts a session without `Net.emulation` |
| `gdnet/debug/emulation/<field>` | 0 | for the `custom` preset |

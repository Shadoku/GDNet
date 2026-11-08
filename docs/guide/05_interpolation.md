# 5. Interpolation and smoothing

Other clients do not simulate an entity they do not own (D13); they show the server's state. Shown raw,
that state arrives a tick at a time, late by the network and unevenly, so motion would stutter at 60 Hz and
jump at 144 Hz. gdnet therefore shows replicas on a *render timeline* a little behind the newest
snapshot, interpolating between the snapshots around it, and blends corrections out over a few frames
instead of jumping. The owner of a predicted entity gets the same treatment for the corrections a rollback
makes. None of this needs code: the profiles mark what interpolates, and the rest snaps as before.

This page covers the render timeline and its jitter buffer, what interpolates and what snaps, error
offsets and teleports, the owner's blending after a rollback, the settings, and what this means for game
code (`docs/PLAN_NETCODE.md` 2.2 step 7, 2.5, WP5).

## The render timeline

Every snapshot carries the server tick it was taken at. A client measures when each one arrives against
its own clock and keeps an estimate of the *leading edge* of the stream: the newest tick it can expect to
have at any moment. Replicas are drawn at the **render tick**, a continuous tick value one *jitter buffer*
behind that edge:

```
render tick = (newest tick expected now) - jitter_buffer_ticks
```

The render tick advances with real time every render frame (`_process`), between 0.9x and 1.1x speed while
it catches up with its target, so it never runs backwards and never jumps unless it is more than 4 ticks
off. Between two snapshots a property is interpolated; when a snapshot is late and the render tick passes
the newest sample, the property extrapolates from the last two samples, at most one tick.

With a 60 Hz tick, the default 2-tick buffer and a pipe that delivers the next frame, a replica shows the
server's state about 3 ticks (50 ms) after the server had it: 2 ticks of buffer plus the transit
(measured: `render_lag_ticks` 2.00, lag behind the server 2.5 to 3.0 ticks, at render 60 and 144 Hz).

### The jitter buffer

The buffer absorbs uneven arrival. It starts at `gdnet/interpolation/jitter_buffer_ticks` (default 2, D8)
and, while `gdnet/interpolation/adaptive` is on (the default), adapts between 1 and 6 ticks:

- a snapshot is **late** when it arrives after the render tick passed its tick (the timeline had to
  extrapolate);
- when more than 2 % of the last second's snapshots were late, the buffer grows by one tick at once (at
  most every quarter second);
- when for 4 seconds every arrival stayed more than half a tick inside what a buffer one tick smaller
  would allow, it shrinks by one tick.

So it grows quickly and shrinks slowly. Under 40 ms of emulated jitter it grew from 2 to 3 ticks after two
late snapshots, and late snapshots then stayed at 0 per second.

## What interpolates and what snaps

A property interpolates when its profile entry says `interpolate` (2.1). The shipped defaults mark the
**hot transforms and velocities**: `Node2D` `position`, `rotation`, `scale`; `Node3D` `position`,
`quaternion`, `scale`; `CharacterBody2D/3D` `velocity`; `RigidBody2D/3D` `linear_velocity` and
`angular_velocity`. Velocities are interpolated as values: a replica never integrates them. A hot property
inferred from a script export or added by `configure()` interpolates when it is numeric (a float, a
vector, a quaternion, a colour).

How each type blends:

| type | between two samples |
|---|---|
| `float`, `Vector2/3/4`, `Color` | linearly |
| `rotation`, or a float quantised `deg` | linearly along the shorter arc, wrapped to [-PI, PI) (3.1 to -3.1 goes through PI, not through 0) |
| `Quaternion` | slerp along the shorter arc, always unit length |
| `int`, `bool`, strings, transforms, anything else | steps when the render tick reaches the sample |

**Cold properties snap** (visibility, modulate, animation names, text, exports): they change rarely and
blending them would be wrong. Change it per property with `interpolate` in a profile, in a `NetEntity`'s
`state_options`, or with `configure()`:

```gdscript
Net.configure($NetManager/Door, {"interpolate": {"position": false}})   # snaps
Net.configure($NetManager/Boss, {"interpolate": false})                 # nothing interpolates
```

(`configure()` table changes, as always, before the scene's first spawn, the same way on every peer.)

A snapshot only carries what changed. A field with no sample for a while was unchanged, so when an entity
that stood still starts moving, it starts at the right tick instead of crawling across the gap; when
snapshots arrive without a field (or none arrive at all), it stays where its newest sample put it.

## Error offsets and teleports

When a new sample disagrees with where the stream was heading (the extrapolation of the last two samples)
by more than a small epsilon (5 cm, 5 px for `Vector2`, 5 degrees), it is a **correction**: a knock-back,
a server-side reposition. The older samples move with it, so the motion stays continuous, and the
difference becomes a visual **offset** that cancels the jump now and decays to zero:

- by **0.95 per frame** for small errors,
- by **0.85 per frame** while the error is above 25 cm (25 px for `Vector2`) or 10 degrees.

The factors are per 60 Hz frame and scaled by the real frame time (`factor ^ (dt * 60)`), so the residual
after a given time is the same at 60 and 144 Hz. The displayed value approaches the target monotonically
and never overshoots: a 20 px correction travelled over 172 frames with its largest step 0.43 px.

Above the **teleport distance** the property snaps instead, with no offset: by default 2 m (200 px for a
`Vector2`, whose unit is a pixel at 100 px per metre) and 90 degrees. Set it per project with
`gdnet/interpolation/teleport_distance` (in the property's own units; 0 = the defaults above), or per
property with the `teleport_distance` option (profile entry, `state_options`, or `configure()`):

```gdscript
Net.configure($NetManager/Portal, {"teleport_distance": {"position": 10.0}})
```

Offsets are visual only: nothing records them into a history, and the server never sees them.

## The owner after a rollback

A predicted owner moves at once and is corrected by rollback (04_prediction.md). The rollback itself
stays exact: the hot properties hold the replayed values, so the prediction history, physics and the next
`_physics_process` see exactly what was simulated. What changes is what is **drawn**: before the
rollback gdnet reads what was shown (the simulated value plus any offset still decaying), after the replay
it keeps the difference to the replayed value as an offset per interpolated hot field, decaying like a
replica's (teleports snap here too).

The classes gdnet replicates have no render-only transform (a `Node2D`'s `position` is both what the
physics uses and what is drawn), so the offset is applied by write and restore:

- in `_process` (the session's runs at priority -1000, before the game's), the property is written as
  `simulated + offset`, and the simulated value is remembered;
- at the start of the next physics frame (the tree's `physics_frame` signal, which a client session
  connects to when it starts: before any `_physics_process`, and before code awaiting that signal), the
  simulated value is written back.

So the owner's `_physics_process` never sees the offset, and its `_process` and the renderer do. Measured:
after a 50 px server impulse the owner rolled back once, its largest frame-to-frame step on screen was
3.6 px, and every one of its 198 `_physics_process` calls saw an exact simulated position.

## Settings and surface

| setting | default | |
|---|---|---|
| `gdnet/interpolation/jitter_buffer_ticks` | 2 | the starting buffer (1..6), read when a session starts |
| `gdnet/interpolation/adaptive` | true | the buffer adapts; false keeps it fixed |
| `gdnet/interpolation/teleport_distance` | 0 | 0 = 2 m / 200 px; else in the property's units |

- `Net.jitter_buffer_ticks` (read only): the default session's current buffer while it is a client, else
  the setting. `NetSession.jitter_buffer_ticks` likewise per session.
- `NetSession.stats()` gains `jitter_buffer_ticks`, `render_lag_ticks` (newest snapshot tick expected now
  minus the render tick), `late_snapshots` and `late_snapshots_per_s`, `offset_corrections` and
  `offset_corrections_per_s` (corrections turned into offsets, replicas and owners), `interp_teleports`.
- Without a client session nothing runs per render frame (gate G0): a `NetSession` only turns its
  `_process` on when it starts as a client.

## Notes for game code

- **Do not read a replica's visual position in `_physics_process`.** A replica's `_physics_process` does
  not run anyway (D13), but other nodes' may: what they read there is whatever the last render frame wrote,
  already a jitter buffer behind the server. Read replicas in `_process`, or ask the server.
- On the owner, `_process` sees the blended position right after a correction; `_physics_process` sees the
  simulated one. A camera that follows the player belongs in `_process` (or uses its smoothing) so it
  follows what is drawn.
- In a world where nothing changes the server sends no snapshots. An entity that stops in such a world may
  be drawn up to one tick ahead (extrapolated) for two ticks and then settle back onto its last position;
  in a world where something still moves, it stops exactly.

## Testing

`tests/integration/test_net_interpolation.gd` shows the patterns: a fixture that records `position` in its
own `_process` (`tests/fixtures/net/mover.gd`), `Engine.max_fps` for the render rate (60 and 144 with
physics at 60; the test skips its variance bound with a printed notice when the headless runner cannot
reach the rate), `NetEmulation.receive_variance_ms` for jitter, and `stats()` for the timeline. Measured
constant-speed motion (120 px/s): per-frame displacement 2.000 px at 60 Hz with a standard deviation of
0.014 px, 0.833 px at 144 Hz with 0.025 px, never backwards.

//! Interpolation and smoothing, the gdnet side (docs/PLAN_NETCODE.md 2.2 step 7, 2.5, WP5; the
//! algorithms are netcore's `interp` module).
//!
//! * **Replicas.** On a client, netcore keeps the snapshot values of every `interpolate` field of an
//!   entity it does not predict in a sample buffer instead of handing them to `NetManager::apply_states`
//!   (which still snaps every other field). Each render frame `NetSession::_process` (process priority
//!   -1000, so before the game's `_process`) calls `Session::interp_frame()`, which advances the render
//!   timeline and returns the fields whose displayed value changed (sample + decaying error offset);
//!   `NetManager::apply_interp` writes them with `apply::write`. A replica's `_physics_process` does not
//!   run (D13), so nothing else moves it; game code that reads a replica's transform should do it in
//!   `_process` (the value `_physics_process` would see is whatever the last render frame wrote).
//! * **Owned entities.** After a rollback (`replay::rollback`) each interpolated hot field of a restored
//!   entity compares what was shown before the rollback (`simulated + owned offset`) with the replayed
//!   value; the difference becomes an owned offset in netcore, decaying per render frame. The offset is
//!   visual only and the classes gdnet replicates have no render-only transform to put it on (a
//!   `Node2D`'s `position` is both), so gdnet uses write/restore: in `_process` it writes `property =
//!   simulated + offset` (remembering `simulated`), and at the start of the next physics frame (the
//!   tree's `physics_frame` signal, which a client session connects to when it starts, so it runs before
//!   any `_physics_process` and before code awaiting that signal resumes; again in `NetSession::step()`
//!   at priority -1000 as a fallback) it writes `property = simulated` back
//!   (`NetManager::restore_owned`). The owner's `_physics_process`, the
//!   prediction history and physics only ever see simulated values.
//! * **Settings.** `gdnet/interpolation/jitter_buffer_ticks` (default 2, D8; the starting value, 1..6),
//!   `gdnet/interpolation/adaptive` (default true), `gdnet/interpolation/teleport_distance` (default 0:
//!   automatic, 2 m for 3D and 200 px for `Vector2` properties; a property's `teleport_distance` option
//!   wins), read when a session starts.

use godot::classes::ProjectSettings;
use godot::prelude::*;

pub const SETTING_JITTER_BUFFER: &str = "gdnet/interpolation/jitter_buffer_ticks";
pub const SETTING_ADAPTIVE: &str = "gdnet/interpolation/adaptive";
pub const SETTING_TELEPORT: &str = "gdnet/interpolation/teleport_distance";

fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name).then(|| ps.get_setting(name))
}

/// `gdnet/interpolation/jitter_buffer_ticks` (default 2), clamped to 1..=6.
pub fn jitter_buffer_ticks() -> u32 {
    setting(SETTING_JITTER_BUFFER)
        .and_then(|v| crate::profiles::number(&v))
        .map_or(netcore::interp::DEFAULT_JITTER_BUFFER, |v| {
            (v.round() as i64).clamp(
                i64::from(netcore::interp::MIN_JITTER_BUFFER),
                i64::from(netcore::interp::MAX_JITTER_BUFFER),
            ) as u32
        })
}

/// `gdnet/interpolation/adaptive` (default true).
pub fn adaptive() -> bool {
    setting(SETTING_ADAPTIVE)
        .and_then(|v| v.try_to::<bool>().ok())
        .unwrap_or(true)
}

/// `gdnet/interpolation/teleport_distance` (default 0 = automatic).
pub fn teleport_distance() -> f64 {
    setting(SETTING_TELEPORT)
        .and_then(|v| crate::profiles::number(&v))
        .unwrap_or(0.0)
        .max(0.0)
}

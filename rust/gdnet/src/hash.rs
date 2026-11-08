//! WP8 hash telemetry, the gdnet side (docs/PLAN_NETCODE.md 2.4 `Hash`, 3.4): the project settings
//! `gdnet/debug/hash_telemetry` (default on) and `gdnet/debug/hash_sample` (entities sampled per tick,
//! default 16) and `gdnet/debug/hash_refresh_s` (an unchanged entity's hash is resent after this many
//! seconds, default 10). The sample size is re-read every tick (`Net.hash_sample` wins over the
//! setting); telemetry on/off and the refresh are read when a session starts. The hashing and comparison live in netcore (`hash.rs`); a
//! session's `stats()` carries the counters and `divergences(node)` the per-entity count.

use godot::classes::ProjectSettings;
use godot::prelude::*;

pub const SETTING_TELEMETRY: &str = "gdnet/debug/hash_telemetry";
pub const SETTING_SAMPLE: &str = "gdnet/debug/hash_sample";
pub const SETTING_REFRESH: &str = "gdnet/debug/hash_refresh_s";

/// A project setting's value when it is set (and not null).
pub fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name)
        .then(|| ps.get_setting(name))
        .filter(|v| !v.is_nil())
}

/// A boolean project setting, `default` when unset or not a bool.
pub fn bool_setting(name: &str, default: bool) -> bool {
    setting(name)
        .and_then(|v| v.try_to::<bool>().ok())
        .unwrap_or(default)
}

/// `gdnet/debug/hash_telemetry` (default true).
pub fn telemetry() -> bool {
    bool_setting(SETTING_TELEMETRY, true)
}

thread_local! {
    /// `Net.hash_sample` when set (review 2026-11-08): wins over the project setting.
    static NET_SAMPLE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Sets (or, with `None`, clears) `Net.hash_sample`.
pub fn set_net_sample(v: Option<u32>) {
    NET_SAMPLE.with(|c| c.set(v));
}

/// `Net.hash_sample`, else `gdnet/debug/hash_sample` (default 16, 0..=1024). Sessions read it every
/// tick, so a change applies at once.
pub fn sample() -> u32 {
    if let Some(n) = NET_SAMPLE.with(|c| c.get()) {
        return n;
    }
    setting(SETTING_SAMPLE)
        .and_then(|v| crate::profiles::number(&v))
        .map_or(netcore::hash::DEFAULT_SAMPLE, |n| {
            (n.round() as i64).clamp(0, 1024) as u32
        })
}

/// `gdnet/debug/hash_refresh_s` (default 10 s).
pub fn refresh_s() -> f64 {
    setting(SETTING_REFRESH)
        .and_then(|v| crate::profiles::number(&v))
        .map_or(netcore::hash::REFRESH_S as f64, |n| n.max(0.0))
}

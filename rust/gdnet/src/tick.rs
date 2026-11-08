//! `_physics_process` as the tick (docs/PLAN_NETCODE.md 2.1 "Who simulates", 2.2 steps 2-4, WP4).
//!
//! * `Net.is_fresh()` lives here: a per-thread flag, true outside a replay. The replay (replay.rs) sets
//!   it false while it calls `_physics_process` / `_net_tick` again for confirmed-and-replayed ticks.
//!   Anything that must happen once per real tick (a sound, a particle burst, WP6's `Net.sync`) checks
//!   it; WP6 suppresses outgoing calls while it is false.
//! * Who simulates (D13): on a peer that is neither the server nor the entity's owner, every part of the
//!   entity gets `set_physics_process(false)` (the replica only shows the server's state); on the server
//!   and the owner `_physics_process` runs as usual and is the tick, except for parts that define
//!   `_net_tick(delta, tick, is_fresh)`: those have `_physics_process` disabled and get `_net_tick` from
//!   the session's step instead. `NetEntity.simulate = everywhere` opts out of the replica rule; an owner
//!   with `NetEntity.predict = disabled` does not simulate (it shows the server's state, like a replica).
//!   `_process` is never touched. [`Gate`] remembers what each part had so it can be restored.
//! * The settings `Net.input_delay_ticks`, `Net.adaptive_delay`, `gdnet/prediction/max_rollback_ticks`,
//!   `gdnet/input/actions`, `gdnet/debug/warn_on_global_input`.
//! * The global-`Input` warning (D14): the simplest reliable check is a scan of the script source. When
//!   an entity gets an owner other than the server, on the server and on the owner, in a debug build and
//!   with `gdnet/debug/warn_on_global_input` on, each part's script is read once (`Script.source_code`):
//!   an `Input.` reference inside a `_physics_process` or `_net_tick` body raises one `push_warning` per
//!   script. (The plan's alternative, comparing the real `Input` with the sampled frame after the tick,
//!   cannot work on the owner: there the sampled frame *is* the real `Input`.)

use godot::classes::{Node, Os, ProjectSettings, Script};
use godot::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

pub const SETTING_MAX_ROLLBACK: &str = "gdnet/prediction/max_rollback_ticks";
pub const SETTING_INPUT_ACTIONS: &str = "gdnet/input/actions";
pub const SETTING_WARN_GLOBAL_INPUT: &str = "gdnet/debug/warn_on_global_input";

thread_local! {
    static FRESH: Cell<bool> = const { Cell::new(true) };
    static INPUT_DELAY: Cell<u32> = const { Cell::new(0) };
    static ADAPTIVE: Cell<bool> = const { Cell::new(false) };
    static WARNED: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
}

/// `Net.is_fresh()`: false while a replay re-runs a tick.
pub fn is_fresh() -> bool {
    FRESH.with(Cell::get)
}

/// Marks the code inside a replay (`is_fresh()` false) until dropped.
pub struct Replaying {
    before: bool,
}

impl Replaying {
    pub fn start() -> Self {
        let before = FRESH.with(|f| f.replace(false));
        Self { before }
    }
}

impl Drop for Replaying {
    fn drop(&mut self) {
        let b = self.before;
        FRESH.with(|f| f.set(b));
    }
}

pub fn input_delay() -> u32 {
    INPUT_DELAY.with(Cell::get)
}

pub fn set_input_delay(v: u32) {
    INPUT_DELAY.with(|c| c.set(v));
}

pub fn adaptive_delay() -> bool {
    ADAPTIVE.with(Cell::get)
}

pub fn set_adaptive_delay(v: bool) {
    ADAPTIVE.with(|c| c.set(v));
}

fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name).then(|| ps.get_setting(name))
}

/// `gdnet/prediction/max_rollback_ticks`. Unset: 20 ticks (D8), and at tick rates above 60 a third of a
/// second (40 at 120 Hz), so a fast tick rate does not shrink the round trip prediction can cover.
pub fn max_rollback_ticks(tick_rate: u32) -> u32 {
    setting(SETTING_MAX_ROLLBACK)
        .and_then(|v| v.try_to::<i64>().ok())
        .map_or_else(
            || netcore::predict::DEFAULT_MAX_ROLLBACK.max(tick_rate.div_ceil(3)),
            |v| v.clamp(1, 600) as u32,
        )
}

/// `gdnet/input/actions` (default empty: every InputMap action but `ui_*`).
pub fn input_actions_setting() -> Vec<String> {
    setting(SETTING_INPUT_ACTIONS)
        .and_then(|v| v.try_to::<PackedStringArray>().ok())
        .map(|a| a.as_slice().iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// `gdnet/debug/warn_on_global_input` (default true), only in debug builds.
pub fn warn_on_global_input() -> bool {
    Os::singleton().is_debug_build()
        && setting(SETTING_WARN_GLOBAL_INPUT)
            .and_then(|v| v.try_to::<bool>().ok())
            .unwrap_or(true)
}

/// What the gate did to one entity's parts.
#[derive(Clone, Debug, Default)]
pub struct Gate {
    /// The last decision: simulate (true) or not; None before the first.
    pub simulating: Option<bool>,
    /// Per part: physics processing before gdnet turned it off.
    pub saved: Vec<bool>,
    /// Per part: the part defines `_net_tick`.
    pub net_tick: Vec<bool>,
}

impl Gate {
    pub fn new(nodes: &[Option<Gd<Node>>]) -> Self {
        Self {
            simulating: None,
            saved: nodes
                .iter()
                .map(|n| n.as_ref().is_some_and(|n| n.is_physics_processing()))
                .collect(),
            net_tick: nodes
                .iter()
                .map(|n| n.as_ref().is_some_and(|n| n.has_method("_net_tick")))
                .collect(),
        }
    }

    /// Applies `simulate` to the parts when it differs from the last decision. Calls into the engine:
    /// the caller holds its base guard.
    pub fn apply(&mut self, nodes: &[Option<Gd<Node>>], simulate: bool) {
        if self.simulating == Some(simulate) {
            return;
        }
        for (i, n) in nodes.iter().enumerate() {
            let Some(n) = n else { continue };
            if !n.is_instance_valid() {
                continue;
            }
            let mut n = n.clone();
            let net_tick = self.net_tick.get(i).copied().unwrap_or(false);
            if simulate {
                let was = self.saved.get(i).copied().unwrap_or(false);
                n.set_physics_process(was && !net_tick);
            } else {
                if self.simulating != Some(false) {
                    // Remember what the part had (a part with _net_tick keeps its own setting).
                    if let Some(s) = self.saved.get_mut(i) {
                        *s = *s || n.is_physics_processing();
                    }
                }
                n.set_physics_process(false);
            }
        }
        self.simulating = Some(simulate);
    }
}

/// The scripts already warned about (once per script per process).
pub fn warned_scripts() -> Vec<String> {
    WARNED.with(|w| w.borrow().iter().cloned().collect())
}

/// Scans the scripts of `parts` for `Input.` inside `_physics_process` / `_net_tick` and warns once per
/// script (D14).
pub fn check_global_input(parts: &[Option<Gd<Node>>]) {
    if !warn_on_global_input() {
        return;
    }
    for n in parts.iter().flatten() {
        let Some(script) = n.get_script().and_then(|s| s.try_cast::<Script>().ok()) else {
            continue;
        };
        let path = script.get_path().to_string();
        let key = if path.is_empty() {
            std::format!("{}", script.instance_id())
        } else {
            path.clone()
        };
        if WARNED.with(|w| w.borrow().contains(&key)) {
            continue;
        }
        let src = script.get_source_code().to_string();
        if let Some((func, line)) = global_input_use(&src) {
            WARNED.with(|w| w.borrow_mut().insert(key.clone()));
            godot_warn!(
                "gdnet: {key}:{line} reads Input inside {func}() of an entity with an owner. On the server that is the server's own keyboard, so the server and the owner diverge. Read Net.input(self) instead (docs/guide/04_prediction.md); set gdnet/debug/warn_on_global_input to false to silence this"
            );
        }
    }
}

/// The first `Input.` reference inside a `_physics_process` or `_net_tick` body of GDScript source:
/// (function name, 1-based line). Bodies are found by indentation; comments and strings are not parsed
/// beyond a trailing `#`.
pub fn global_input_use(src: &str) -> Option<(&'static str, usize)> {
    let mut body: Option<&'static str> = None;
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        let code = trimmed.split('#').next().unwrap_or("");
        let indented = line.len() != trimmed.len();
        let decl = code.strip_prefix("static ").unwrap_or(code);
        if let Some(rest) = decl.strip_prefix("func ") {
            let name = rest.split('(').next().unwrap_or("").trim();
            body = match name {
                "_physics_process" => Some("_physics_process"),
                "_net_tick" => Some("_net_tick"),
                _ => None,
            };
            continue;
        }
        if code.trim().is_empty() {
            continue;
        }
        if !indented {
            body = None;
            continue;
        }
        if let Some(func) = body {
            if mentions_input(code) {
                return Some((func, i + 1));
            }
        }
    }
    None
}

/// `Input.` as a whole identifier (not `NetInput.`, `my_Input.`, `x.Input.`).
fn mentions_input(code: &str) -> bool {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(pos) = code[from..].find("Input.") {
        let at = from + pos;
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let ok = before.is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'.'));
        if ok {
            return true;
        }
        from = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_input_in_the_tick_only() {
        let src = "extends CharacterBody2D\n\nfunc _ready() -> void:\n\tprint(Input.is_action_pressed(\"x\"))\n\nfunc _physics_process(delta: float) -> void:\n\tvar v := Input.get_vector(\"left\", \"right\", \"up\", \"down\")\n\tmove_and_slide()\n";
        assert_eq!(global_input_use(src), Some(("_physics_process", 7)));
        let ok = "func _physics_process(delta: float) -> void:\n\tvelocity = Net.input(self).get_vector(\"a\", \"b\", \"c\", \"d\")\n\t# Input.get_vector would be wrong here\n\tvar n: NetInput = Net.input(self)\n";
        assert_eq!(global_input_use(ok), None);
        let ready_only =
            "func _ready():\n\tInput.action_press(\"x\")\nfunc _physics_process(d):\n\tpass\n";
        assert_eq!(global_input_use(ready_only), None);
        let tick = "var a := 1\nfunc _net_tick(delta, tick, fresh):\n\n\tif Input.is_action_pressed(\"fire\"):\n\t\tpass\n";
        assert_eq!(global_input_use(tick), Some(("_net_tick", 4)));
        let after = "func _physics_process(d):\n\tpass\nvar x = Input.get_axis(\"a\", \"b\")\n";
        assert_eq!(
            global_input_use(after),
            None,
            "a top-level line ends the body"
        );
    }
}

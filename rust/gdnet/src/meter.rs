//! WP8: the meter and the overlay's show/hide (docs/PLAN_NETCODE.md 1 "What the developer sees", 3.1
//! `NetMeter`, 3.4 `gdnet/debug/meter`, `gdnet/debug/overlay`).
//!
//! The meter is `addons/gdnet/meter.tscn`, a `CanvasLayer` with the GDScript controller
//! `addons/gdnet/meter.gd` (`class_name NetMeter`): plain `Label`s refreshed four times a second from a
//! session's `stats()` (the data side is Rust: `NetSession.stats()`). `Net.show_meter(true)` adds it
//! under the tree's root as `__NetMeter` (deferred, so it works from any `_ready`), `false` frees it.
//! The overlay (`NetOverlay`, overlay.rs) is added the same way as `__NetOverlay`.
//!
//! The toggle: `Net`'s session reads `_unhandled_input` and toggles the meter on the input action
//! `gdnet_toggle_meter` when the project defines it, else on F9. Nothing of this runs without a session
//! (gate G0) unless a script calls `Net.show_meter(true)` / `Net.show_overlay(true)`.

use godot::classes::{
    Engine, InputEvent, InputEventKey, InputMap, Node, PackedScene, ResourceLoader,
};
use godot::global::Key;
use godot::prelude::*;

pub const SETTING_METER: &str = "gdnet/debug/meter";
pub const SETTING_OVERLAY: &str = "gdnet/debug/overlay";
pub const METER_SCENE: &str = "res://addons/gdnet/meter.tscn";
pub const METER_NODE: &str = "__NetMeter";
pub const OVERLAY_NODE: &str = "__NetOverlay";
pub const TOGGLE_ACTION: &str = "gdnet_toggle_meter";

/// True when `event` is the meter's toggle: the action `gdnet_toggle_meter` if the project defines it,
/// else a press of F9 (no echo).
pub fn is_toggle(event: &Gd<InputEvent>) -> bool {
    if InputMap::singleton().has_action(TOGGLE_ACTION) {
        return event.is_action_pressed_ex(TOGGLE_ACTION).done();
    }
    match event.clone().try_cast::<InputEventKey>() {
        Ok(k) => k.is_pressed() && !k.is_echo() && k.get_keycode() == Key::F9,
        Err(_) => false,
    }
}

/// Toggles the meter through the `Net` singleton (called by `Net`'s session on the toggle key).
pub fn toggle_from_session() {
    let Some(obj) = Engine::singleton().get_singleton(crate::NET_SINGLETON) else {
        return;
    };
    let Ok(mut net) = obj.try_cast::<crate::net::Net>() else {
        return;
    };
    let shown = net.bind().meter_visible();
    net.bind_mut().show_meter(!shown);
}

fn root() -> Option<Gd<Node>> {
    let tree = Engine::singleton()
        .get_main_loop()?
        .try_cast::<godot::classes::SceneTree>()
        .ok()?;
    tree.get_root().map(|r| r.upcast())
}

/// Adds `node` under the root as `name` (deferred: the root may be busy setting up children).
pub fn add_layer(mut node: Gd<Node>, name: &str) -> Option<Gd<Node>> {
    let Some(mut root) = root() else {
        node.free();
        return None;
    };
    if let Some(mut stale) = root.get_node_or_null(name) {
        stale.set_name(&format!("{name}_old"));
        stale.queue_free();
    }
    node.set_name(name);
    root.call_deferred("add_child", &[node.to_variant()]);
    Some(node)
}

/// A new meter from `addons/gdnet/meter.tscn`; None (with an error) when it is missing.
pub fn new_meter() -> Option<Gd<Node>> {
    let mut loader = ResourceLoader::singleton();
    if !loader.exists(METER_SCENE) {
        godot_error!("gdnet: {METER_SCENE} is missing; the meter cannot be shown");
        return None;
    }
    let scene = loader.load(METER_SCENE)?.try_cast::<PackedScene>().ok()?;
    scene.instantiate()
}

/// Frees a layer added by [`add_layer`] (queued, so a pending deferred add still runs first).
pub fn remove_layer(node: Option<Gd<Node>>) {
    if let Some(mut n) = node {
        if n.is_instance_valid() {
            n.queue_free();
        }
    }
}

/// True while `node` is a live layer not queued for deletion.
pub fn is_live(node: &Option<Gd<Node>>) -> bool {
    node.as_ref()
        .is_some_and(|n| n.is_instance_valid() && !n.is_queued_for_deletion())
}

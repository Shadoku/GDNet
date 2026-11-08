//! The `World` pattern and levels (docs/PLAN_NETCODE.md 2.11, WP13).
//!
//! A *level* is a scene with a `NetManager` inside, loaded under the game's *World* node: the node the
//! game registers with `Net.set_world(node)` (it joins the group [`WORLD_GROUP`]; each session uses the
//! World inside its own branch), else the session's branch root. The server keeps every level someone
//! is in (`NetSession.load_level`, `travel`), unloading one `empty_level_timeout` seconds after its
//! last player left; a client loads only the level the server's `Travel` names and frees the others
//! once none of its entities remain there. A level node is named after its scene file (`hub.tscn` ->
//! `hub`), so its managers sit at the same path on every peer (WP2's manager keys).
//!
//! Settings: `gdnet/levels/empty_timeout_s` (default 30), overridden by `Net.empty_level_timeout` and
//! `NetSession.empty_level_timeout` (0 = the next one down).

use godot::classes::{Node, Node2D, Node3D, PackedScene};
use godot::prelude::*;
use std::cell::Cell;

/// The group `Net.set_world(node)` puts the World node in.
pub const WORLD_GROUP: &str = "__gdnet_world";
/// The project setting for the empty-level timeout.
pub const SETTING_EMPTY_TIMEOUT: &str = "gdnet/levels/empty_timeout_s";
/// The empty-level timeout without a setting (seconds).
pub const DEFAULT_EMPTY_TIMEOUT_S: f64 = 30.0;

thread_local! {
    /// `Net.empty_level_timeout` (0 = the project setting).
    static NET_TIMEOUT: Cell<f64> = const { Cell::new(0.0) };
}

pub fn set_net_timeout(v: f64) {
    NET_TIMEOUT.with(|c| c.set(v.max(0.0)));
}

pub fn net_timeout() -> f64 {
    NET_TIMEOUT.with(Cell::get)
}

/// `Net.empty_level_timeout`, else `gdnet/levels/empty_timeout_s`, else 30 s.
pub fn empty_timeout_s() -> f64 {
    let v = net_timeout();
    if v > 0.0 {
        return v;
    }
    crate::hash::setting(SETTING_EMPTY_TIMEOUT)
        .and_then(|v| crate::profiles::number(&v))
        .filter(|n| *n > 0.0)
        .unwrap_or(DEFAULT_EMPTY_TIMEOUT_S)
}

/// The World node of a branch: the first node of [`WORLD_GROUP`] inside `branch`, else `branch`.
pub fn world_of(branch: &Gd<Node>) -> Gd<Node> {
    if let Some(tree) = branch.get_tree_or_null() {
        for n in tree.get_nodes_in_group(WORLD_GROUP).iter_shared() {
            if n.is_inside_tree() && (n == *branch || branch.is_ancestor_of(&n)) {
                return n;
            }
        }
    }
    branch.clone()
}

/// The node name of a level: its scene file's name without the extension, made a valid node name.
pub fn level_name(scene_path: &str) -> String {
    let file = scene_path.rsplit('/').next().unwrap_or(scene_path);
    let stem = file.split('.').next().unwrap_or(file);
    let clean: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        String::from("Level")
    } else {
        clean
    }
}

/// Instantiates a level scene, named after its file. None (with an error) when it cannot be loaded.
pub fn instantiate_level(scene_path: &str) -> Option<Gd<Node>> {
    let Some(scene) = try_load::<PackedScene>(scene_path).ok() else {
        godot_error!("gdnet: the level {scene_path} could not be loaded");
        return None;
    };
    let Some(mut node) = scene.instantiate() else {
        godot_error!("gdnet: the level {scene_path} could not be instantiated");
        return None;
    };
    node.set_name(level_name(scene_path).as_str());
    Some(node)
}

/// Puts `root` (a `Node2D` or `Node3D`) at the marker named `spawn_point` inside `level` (searched
/// recursively), else at the level root's origin.
pub fn place_at_spawn(root: &Gd<Node>, level: &Gd<Node>, spawn_point: &str) {
    let marker = if spawn_point.is_empty() {
        None
    } else {
        level
            .find_child_ex(spawn_point)
            .recursive(true)
            .owned(false)
            .done()
    };
    if marker.is_none() && !spawn_point.is_empty() {
        godot_warn!(
            "gdnet: travel: the level {} has no node named {spawn_point}; the player arrives at its origin",
            level.get_scene_file_path()
        );
    }
    let at = marker.unwrap_or_else(|| level.clone());
    if let Ok(mut r) = root.clone().try_cast::<Node2D>() {
        let pos = at
            .try_cast::<Node2D>()
            .map_or(Vector2::ZERO, |m| m.get_global_position());
        r.set_global_position(pos);
    } else if let Ok(mut r) = root.clone().try_cast::<Node3D>() {
        let pos = at
            .try_cast::<Node3D>()
            .map_or(Vector3::ZERO, |m| m.get_global_position());
        r.set_global_position(pos);
    }
}

/// Calls `_net_travelled(from, to)` on every part that defines it (2.11). The caller holds a base guard.
pub fn notify_travelled(parts: &[Gd<Node>], from: &str, to: &str) {
    let (f, t) = (GString::from(from), GString::from(to));
    for p in parts {
        let mut p = p.clone();
        if p.has_method("_net_travelled") {
            p.call("_net_travelled", &[f.to_variant(), t.to_variant()]);
        }
    }
}

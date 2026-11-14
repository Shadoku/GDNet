//! The `World` pattern and levels (docs/PLAN_NETCODE.md 2.11, 12.2; WP13, WP16).
//!
//! A *level* is a scene with a `NetManager` inside, loaded under the game's *World* node: the node the
//! game registers with `Net.set_world(node)` (it joins the group [`WORLD_GROUP`]; each session uses the
//! World inside its own branch), else the session's branch root. The server keeps every level a peer
//! has a room in (`NetSession.load_level`, `travel`, neighbours, `add_room`), unloading one
//! `empty_level_timeout` seconds after nobody wants it; a client loads the levels the server's
//! `Travel`s name and frees each on its `RoomLeft`. A level node is named after its scene file
//! (`hub.tscn` -> `hub`), so its managers sit at the same path on every peer (WP2's manager keys).
//!
//! Placement (WP16): a level's root can be put at an *origin* ([`place_level`]), the same on every
//! peer. A `NetManager` is a plain `Node`, so the `Node2D` / `Node3D` children of a manager do not
//! follow their level root's transform; placement moves them by the same offset, so the whole level
//! moves as the editor shows it. With `gdnet/levels/spacing` > 0 the server gives a level loaded
//! without an origin the next free cell of a grid (cell n at x = n * spacing, n >= 1; cell 0 is left to
//! the authored world), unless one of its managers names `neighbours` (a chunk keeps the position it
//! was authored at).
//!
//! Settings: `gdnet/levels/empty_timeout_s` (default 30), overridden by `Net.empty_level_timeout` and
//! `NetSession.empty_level_timeout` (0 = the next one down); `gdnet/levels/spacing` (default 0 = off).

use godot::classes::{CanvasItem, CanvasLayer, Node, Node2D, Node3D, PackedScene, Viewport};
use godot::prelude::*;
use netcore::Origin;
use std::cell::Cell;

/// The group `Net.set_world(node)` puts the World node in.
pub const WORLD_GROUP: &str = "__gdnet_world";
/// The project setting for the empty-level timeout.
pub const SETTING_EMPTY_TIMEOUT: &str = "gdnet/levels/empty_timeout_s";
/// The empty-level timeout without a setting (seconds).
pub const DEFAULT_EMPTY_TIMEOUT_S: f64 = 30.0;
/// WP16: the project setting for the placement grid's spacing (0 = levels keep their authored place).
pub const SETTING_SPACING: &str = "gdnet/levels/spacing";

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

/// `gdnet/levels/spacing` (0 when unset or not positive).
pub fn spacing() -> f64 {
    crate::hash::setting(SETTING_SPACING)
        .and_then(|v| crate::profiles::number(&v))
        .filter(|n| *n > 0.0)
        .unwrap_or(0.0)
}

/// A `Vector2` / `Vector3` argument as an origin; `Ok(None)` for null; `Err` for anything else.
pub fn origin_from_variant(v: &Variant) -> Result<Option<Origin>, String> {
    if v.is_nil() {
        return Ok(None);
    }
    if let Ok(o) = v.try_to::<Vector2>() {
        return Ok(Some(Origin::Flat([o.x, o.y])));
    }
    if let Ok(o) = v.try_to::<Vector3>() {
        return Ok(Some(Origin::Deep([o.x, o.y, o.z])));
    }
    Err(format!(
        "an origin is a Vector2 or a Vector3, not {}",
        v.stringify()
    ))
}

/// An origin as a `Vector2` / `Vector3` (null for None).
pub fn origin_to_variant(o: Option<Origin>) -> Variant {
    match o {
        Some(Origin::Flat([x, y])) => Vector2::new(x, y).to_variant(),
        Some(Origin::Deep([x, y, z])) => Vector3::new(x, y, z).to_variant(),
        None => Variant::nil(),
    }
}

/// Whether a level root is placed in 3D (`Node3D`), 2D (`Node2D`), or cannot be placed (None).
pub fn level_is_deep(node: &Gd<Node>) -> Option<bool> {
    if node.clone().try_cast::<Node3D>().is_ok() {
        Some(true)
    } else if node.clone().try_cast::<Node2D>().is_ok() {
        Some(false)
    } else {
        None
    }
}

/// Puts a level's root at `origin` and moves the spatial nodes that do not inherit the root's
/// transform (the children of its `NetManager`s, or of any other plain `Node`) by the same offset.
/// False (with a warning) when the root is neither a `Node2D` nor a `Node3D`, or the origin's kind
/// does not match it. Works before the level enters the tree and on a loaded level.
pub fn place_level(level: &Gd<Node>, origin: Origin) -> bool {
    let path = level.get_scene_file_path();
    match (origin, level_is_deep(level)) {
        (Origin::Flat([x, y]), Some(false)) => {
            let mut root = level.clone().cast::<Node2D>();
            let shift = Vector2::new(x, y) - root.get_position();
            root.set_position(Vector2::new(x, y));
            shift_detached(level, Vector3::new(shift.x, shift.y, 0.0));
            true
        }
        (Origin::Deep([x, y, z]), Some(true)) => {
            let mut root = level.clone().cast::<Node3D>();
            let shift = Vector3::new(x, y, z) - root.get_position();
            root.set_position(Vector3::new(x, y, z));
            shift_detached(level, shift);
            true
        }
        (_, None) => {
            godot_warn!(
                "gdnet: the level {path} has no Node2D or Node3D root, so it cannot be placed at an origin"
            );
            false
        }
        (Origin::Flat(_), Some(true)) => {
            godot_warn!("gdnet: the level {path} is 3D: its origin must be a Vector3");
            false
        }
        (Origin::Deep(_), Some(false)) => {
            godot_warn!("gdnet: the level {path} is 2D: its origin must be a Vector2");
            false
        }
    }
}

/// Moves by `shift` every `Node2D` / `Node3D` below `node` whose transform is not its parent's (its
/// parent is not spatial, or it is `top_level`); separate canvases and viewports are left alone.
fn shift_detached(node: &Gd<Node>, shift: Vector3) {
    for child in node.get_children().iter_shared() {
        if child.clone().try_cast::<CanvasLayer>().is_ok()
            || child.clone().try_cast::<Viewport>().is_ok()
        {
            continue;
        }
        if let Ok(mut n) = child.clone().try_cast::<Node2D>() {
            let inherits =
                node.clone().try_cast::<CanvasItem>().is_ok() && !n.is_set_as_top_level();
            if !inherits {
                let p = n.get_position();
                n.set_position(p + Vector2::new(shift.x, shift.y));
            }
        } else if let Ok(mut n) = child.clone().try_cast::<Node3D>() {
            let inherits = node.clone().try_cast::<Node3D>().is_ok() && !n.is_set_as_top_level();
            if !inherits {
                let p = n.get_position();
                n.set_position(p + shift);
            }
        }
        shift_detached(&child, shift);
    }
}

/// `p` (global) in the coordinates of the nearest `Node2D` ancestor of `manager` (its level's root,
/// which moves with the level's origin); `p` itself when there is none.
pub fn to_level_local_2d(manager: &Gd<Node>, p: Vector2) -> Vector2 {
    let mut at = manager.get_parent();
    while let Some(n) = at {
        if let Ok(n2) = n.clone().try_cast::<Node2D>() {
            return n2.get_global_transform().affine_inverse() * p;
        }
        at = n.get_parent();
    }
    p
}

/// The 3D form of [`to_level_local_2d`].
pub fn to_level_local_3d(manager: &Gd<Node>, p: Vector3) -> Vector3 {
    let mut at = manager.get_parent();
    while let Some(n) = at {
        if let Ok(n3) = n.clone().try_cast::<Node3D>() {
            return n3.get_global_transform().affine_inverse() * p;
        }
        at = n.get_parent();
    }
    p
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
/// recursively; its global position, so the level's origin applies) and turns it the marker's way: a
/// `Node2D` takes the marker's global rotation, a `Node3D` the rotation of the marker's global basis
/// (without the marker's scale), keeping its own scale (the owner's decision, 2026-11-14). An empty
/// `spawn_point` leaves `root` where and as it is (WP16: travel without a marker keeps the global
/// transform); a missing marker warns and does the same.
pub fn place_at_spawn(root: &Gd<Node>, level: &Gd<Node>, spawn_point: &str) {
    if spawn_point.is_empty() {
        return;
    }
    let Some(at) = level
        .find_child_ex(spawn_point)
        .recursive(true)
        .owned(false)
        .done()
    else {
        godot_warn!(
            "gdnet: travel: the level {} has no node named {spawn_point}; the player keeps its position",
            level.get_scene_file_path()
        );
        return;
    };
    if let Ok(mut r) = root.clone().try_cast::<Node2D>() {
        if let Ok(m) = at.try_cast::<Node2D>() {
            r.set_global_position(m.get_global_position());
            r.set_global_rotation(m.get_global_rotation());
        }
    } else if let Ok(mut r) = root.clone().try_cast::<Node3D>() {
        if let Ok(m) = at.try_cast::<Node3D>() {
            let marker = m.get_global_transform();
            let mut t = r.get_global_transform();
            t.basis = turned(marker.basis, t.basis);
            t.origin = marker.origin;
            r.set_global_transform(t);
        }
    }
}

/// The rotation of `marker` (orthonormalised, a mirrored marker's reflection dropped, as Godot's
/// `Basis.get_rotation_quaternion` does) with the scale of `own` along its axes; `own` itself when the
/// marker's basis is degenerate (a zero scale has no rotation to take).
fn turned(marker: Basis, own: Basis) -> Basis {
    if marker.determinant().abs() < 1e-12 {
        return own;
    }
    let scale = own.get_scale();
    let mut rot = marker.orthonormalized();
    if rot.determinant() < 0.0 {
        rot = rot.scaled(Vector3::new(-1.0, -1.0, -1.0));
    }
    Basis::from_cols(
        rot.col_a() * scale.x,
        rot.col_b() * scale.y,
        rot.col_c() * scale.z,
    )
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

//! WP14: proximity interaction scoring (docs/PLAN_NETCODE.md 2.7, 2.11): every server tick, before the
//! snapshots are built, each peer with a player gets its interacting set: the entities whose root is
//! within `interaction_radius` of the player's root, plus the bodies in contact with the player's body
//! (a RigidBody with `contact_monitor` on: `get_colliding_bodies()`; a CharacterBody: the last
//! `move_and_slide()` collisions). Interacting entities weigh 100 in the priority accumulator instead
//! of 1 (owned ones keep 1,000,000), so under a tight budget the bodies near a player keep updating
//! every tick and the far ones are deferred.
//!
//! The radius: `NetManager.interaction_radius` for the manager's entities (in their units: pixels in
//! 2D, metres in 3D), 0 = the project setting `gdnet/budget/interaction_radius` (default 300, in
//! pixels; 3D reads it at 100 px per metre, i.e. 3 m). A negative radius turns proximity off for that
//! manager. The distance test is netcore's `priority::near` (a coarse grid); this module reads the
//! engine's positions and contacts, main thread only.

use crate::manager::{entity_root_of, NetManager};
use godot::classes::{
    CharacterBody2D, CharacterBody3D, Node2D, Node3D, ProjectSettings, RigidBody2D, RigidBody3D,
};
use godot::prelude::*;
use netcore::priority::{self, Body};
use netcore::NetId;
use std::collections::{BTreeSet, HashMap};

pub const SETTING_RADIUS: &str = "gdnet/budget/interaction_radius";
/// The default radius in pixels (2D); 3D divides by [`PX_PER_M`].
pub const DEFAULT_RADIUS: f64 = 300.0;
/// Pixels per metre, the convention the interpolation thresholds use too (WP5).
pub const PX_PER_M: f64 = 100.0;

/// `gdnet/budget/interaction_radius` (pixels), default 300.
pub fn setting_radius() -> f64 {
    let ps = ProjectSettings::singleton();
    if !ps.has_setting(SETTING_RADIUS) {
        return DEFAULT_RADIUS;
    }
    crate::profiles::number(&ps.get_setting(SETTING_RADIUS)).unwrap_or(DEFAULT_RADIUS)
}

/// The position of an entity root and whether it is 3D; None for a root that is neither a `Node2D`
/// nor a `Node3D`.
fn position_of(root: &Gd<Node>) -> Option<([f64; 3], bool)> {
    if let Ok(n) = root.clone().try_cast::<Node2D>() {
        let p = n.get_global_position();
        return Some(([f64::from(p.x), f64::from(p.y), 0.0], false));
    }
    if let Ok(n) = root.clone().try_cast::<Node3D>() {
        let p = n.get_global_position();
        return Some(([f64::from(p.x), f64::from(p.y), f64::from(p.z)], true));
    }
    None
}

/// The nodes the player's body touches.
fn contacts_of(root: &Gd<Node>) -> Vec<Gd<Node>> {
    let mut out = Vec::new();
    if let Ok(b) = root.clone().try_cast::<RigidBody2D>() {
        if b.is_contact_monitor_enabled() {
            out.extend(b.get_colliding_bodies().iter_shared().map(|n| n.upcast()));
        }
    } else if let Ok(b) = root.clone().try_cast::<RigidBody3D>() {
        if b.is_contact_monitor_enabled() {
            out.extend(b.get_colliding_bodies().iter_shared().map(|n| n.upcast()));
        }
    } else if let Ok(b) = root.clone().try_cast::<CharacterBody2D>() {
        for i in 0..b.get_slide_collision_count() {
            if let Some(c) = b.get_slide_collision(i).and_then(|c| c.get_collider()) {
                if let Ok(n) = c.try_cast::<Node>() {
                    out.push(n);
                }
            }
        }
    } else if let Ok(b) = root.clone().try_cast::<CharacterBody3D>() {
        for i in 0..b.get_slide_collision_count() {
            if let Some(c) = b.get_slide_collision(i).and_then(|c| c.get_collider()) {
                if let Ok(n) = c.try_cast::<Node>() {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Each player's interacting set: `players` = (peer, player entity) from netcore, `managers` = the
/// session's managers. Peers whose player is not found get an empty set.
pub fn interacting_sets(
    managers: &[(u64, Gd<NetManager>)],
    players: &[(u32, NetId)],
) -> Vec<(u32, BTreeSet<NetId>)> {
    if players.is_empty() {
        return Vec::new();
    }
    let default = setting_radius();
    let mut bodies: Vec<Body> = Vec::new();
    let mut roots: HashMap<NetId, Gd<Node>> = HashMap::new();
    for (_, m) in managers {
        let (radius, list) = {
            let b = m.bind();
            (b.interaction_radius_setting(), b.roots())
        };
        for (id, root) in list {
            let Some((pos, is_3d)) = position_of(&root) else {
                roots.insert(id, root);
                continue;
            };
            let r = if radius > 0.0 {
                radius
            } else if radius < 0.0 {
                0.0
            } else if is_3d {
                default / PX_PER_M
            } else {
                default
            };
            bodies.push(Body { id, pos, radius: r });
            roots.insert(id, root);
        }
    }
    let mut placed = Vec::new();
    for &(peer, player) in players {
        if let Some(pos) = roots.get(&player).and_then(position_of) {
            placed.push((peer, player, pos.0));
        }
    }
    let near = priority::near(&placed, &bodies);
    let mut out: Vec<(u32, BTreeSet<NetId>)> = Vec::with_capacity(players.len());
    for &(peer, player) in players {
        let mut set: BTreeSet<NetId> = near
            .iter()
            .find(|(p, _)| *p == peer)
            .map(|(_, ids)| ids.iter().copied().collect())
            .unwrap_or_default();
        if let Some(root) = roots.get(&player) {
            for n in contacts_of(root) {
                if let Some((r, m)) = entity_root_of(&n) {
                    let id = m.bind().net_id_of(&r);
                    if id != 0 && id != player {
                        set.insert(id);
                    }
                }
            }
        }
        out.push((peer, set));
    }
    out
}

//! Travel between levels (docs/PLAN_NETCODE.md 2.11, WP13; netcore's side is netcore/src/rooms.rs).
//!
//! Server, `Net.travel(player, scene_path, spawn_point)`: the request is queued and applied at the
//! session's next step (never inside a physics callback such as a door's `body_entered`, where moving a
//! body is refused by the engine): the target level is loaded under the World if it is not, its
//! managers register their rooms, the player's entry (and the entries of the entities below it) move
//! from its manager to the level's manager, the node is reparented under that manager at the
//! `spawn_point` marker, `_net_travelled(from, to)` runs on its parts and `travelled(player, from, to)`
//! fires; then netcore's cross-manager `Reparent` changes the player's room, which sends its owner a
//! `Travel` (load that level), the scope change (`Scope` leaves, `Create`s), then the `Reparent`.
//!
//! Client: a `Travel` event loads the level at once (before the events after it are routed, so they
//! find its managers) and is acknowledged with `TravelAck`; a cross-manager `Reparent` waits until both
//! managers exist, then moves the entry and the node the same way (prediction and interpolation of
//! the moved entities restart) and fires the same callbacks; afterwards every level other than the
//! `Travel`'s that holds no entity this client owns is freed.
//!
//! Server levels empty for `empty_level_timeout` seconds (no player's entity in any of their managers)
//! are unloaded: their entities are destroyed first, so the peers that saw them free them.

use crate::manager::{entity_root_of, NetManager};
use crate::session::NetSession;
use crate::world::{self, instantiate_level, notify_travelled, place_at_spawn, world_of};
use godot::classes::Node;
use godot::prelude::*;
use netcore::session::EntityEvent;
use netcore::{NetId, ParentRef, MANAGER_ROOT};
use std::collections::HashSet;

/// A level this session loaded.
pub struct Level {
    pub path: String,
    pub node: Gd<Node>,
    /// Server: ticks in a row without a player in it.
    pub empty_ticks: u64,
}

/// A queued `Net.travel`.
pub struct TravelReq {
    pub player: Gd<Node>,
    pub scene_path: String,
    pub spawn_point: String,
}

impl NetSession {
    /// The World node levels go under (see world.rs).
    pub(crate) fn world(&self) -> Option<Gd<Node>> {
        self.branch().map(|b| world_of(&b))
    }

    fn live_levels(&mut self) {
        self.levels.retain(|l| {
            l.node.is_instance_valid()
                && l.node.is_inside_tree()
                && !l.node.is_queued_for_deletion()
        });
    }

    /// The scene paths of the loaded levels, in load order.
    pub(crate) fn level_paths(&mut self) -> Vec<String> {
        self.live_levels();
        self.levels.iter().map(|l| l.path.clone()).collect()
    }

    /// Loads `scene_path` under the World (or returns the loaded level). On the server the session
    /// becomes room-strict (2.11) and the level's managers register their rooms at once.
    pub(crate) fn do_load_level(&mut self, scene_path: &str) -> Option<Gd<Node>> {
        self.live_levels();
        if let Some(l) = self.levels.iter().find(|l| l.path == scene_path) {
            return Some(l.node.clone());
        }
        let Some(mut w) = self.world() else {
            godot_error!(
                "gdnet: load_level({scene_path}): the session has no branch to load it in"
            );
            return None;
        };
        let node = instantiate_level(scene_path)?;
        {
            let _guard = self.base_mut();
            w.add_child(&node);
        }
        self.levels.push(Level {
            path: scene_path.to_string(),
            node: node.clone(),
            empty_ticks: 0,
        });
        if self.is_server() {
            self.with_core(|c, now| c.set_rooms_strict(true, now));
            self.register_rooms();
        }
        Some(node)
    }

    /// Frees a loaded level (the server destroys its entities first). False when it is not loaded.
    pub(crate) fn do_unload_level(&mut self, scene_path: &str) -> bool {
        self.live_levels();
        let Some(i) = self.levels.iter().position(|l| l.path == scene_path) else {
            return false;
        };
        let mut node = self.levels.remove(i).node;
        if self.is_server() {
            let me = self.to_gd();
            let inside: Vec<Gd<NetManager>> = self
                .managers()
                .into_iter()
                .map(|(_, m)| m)
                .filter(|m| node.is_ancestor_of(m))
                .collect();
            let _guard = self.base_mut();
            for mut m in inside {
                m.bind_mut().destroy_all(&me);
            }
        }
        {
            let _guard = self.base_mut();
            if let Some(mut p) = node.get_parent() {
                p.remove_child(&node);
            }
            node.queue_free();
        }
        if self.is_server() {
            self.register_rooms();
        }
        true
    }

    /// The scene path of the loaded level `m` sits in.
    fn level_of_manager(&self, m: &Gd<NetManager>) -> Option<String> {
        self.levels
            .iter()
            .find(|l| l.node.is_instance_valid() && l.node.is_ancestor_of(m))
            .map(|l| l.path.clone())
    }

    /// A manager's room name: its `room`, else its scene, else its branch-relative path.
    pub(crate) fn room_name(&self, m: &Gd<NetManager>) -> String {
        m.bind().room_setting().unwrap_or_else(|| {
            let root = m.bind().get_managed_root();
            self.branch()
                .map(|b| b.get_path_to(&root).to_string())
                .unwrap_or_else(|| root.get_path().to_string())
        })
    }

    /// Server: tells netcore which manager is in which room (cheap when nothing changed).
    pub(crate) fn register_rooms(&mut self) {
        let table: Vec<(u64, String, Option<String>)> = self
            .managers()
            .iter()
            .map(|(k, m)| (*k, self.room_name(m), self.level_of_manager(m)))
            .collect();
        self.with_core(|c, now| c.set_rooms(&table, now));
    }

    /// Server: applies the queued `Net.travel` requests.
    pub(crate) fn process_travels(&mut self) {
        for req in std::mem::take(&mut self.travels) {
            if !req.player.is_instance_valid() {
                continue;
            }
            let Some((root, src)) = entity_root_of(&req.player) else {
                godot_error!(
                    "gdnet: Net.travel: {} is not a managed entity",
                    req.player.get_path()
                );
                continue;
            };
            let id = src.bind().net_id_of(&root);
            if id == 0 {
                godot_error!(
                    "gdnet: Net.travel: {} is not spawned yet; travel after it was spawned",
                    root.get_path()
                );
                continue;
            }
            let Some(level) = self.do_load_level(&req.scene_path) else {
                continue;
            };
            let Some((dkey, dest)) = self
                .managers()
                .into_iter()
                .find(|(_, m)| level.is_ancestor_of(m))
            else {
                godot_error!(
                    "gdnet: Net.travel: the level {} has no NetManager",
                    req.scene_path
                );
                continue;
            };
            let skey = self.manager_key(&src.bind().get_managed_root());
            if skey == dkey {
                let _guard = self.base_mut();
                place_at_spawn(&root, &level, &req.spawn_point);
                continue;
            }
            let parent = ParentRef {
                manager: dkey,
                entity: MANAGER_ROOT,
                part: 0,
            };
            let (from, to) = (self.room_name(&src), self.room_name(&dest));
            self.move_entity(
                src,
                dest,
                &root,
                parent,
                (&from, &to),
                Some((&level, &req.spawn_point)),
            );
            self.with_core(|c, now| c.reparent(id, parent, now));
        }
    }

    /// Moves the entity rooted at `root` from manager `src` to `dest` under `parent` (optionally at a
    /// spawn marker of `place.0`), then runs the travel callbacks.
    fn move_entity(
        &mut self,
        mut src: Gd<NetManager>,
        mut dest: Gd<NetManager>,
        root: &Gd<Node>,
        parent: ParentRef,
        rooms: (&str, &str),
        place: Option<(&Gd<Node>, &str)>,
    ) {
        let moved = {
            let _guard = self.base_mut();
            src.bind_mut().take_entries(root)
        };
        let Some(moved) = moved else {
            return;
        };
        let generation = self.generation();
        let ids = {
            let _guard = self.base_mut();
            dest.bind_mut().put_entries(moved, generation)
        };
        let Some(mut new_parent) = dest.bind().resolve_parent(&parent) else {
            godot_error!(
                "gdnet: travel: the destination of {} was not found",
                root.get_path()
            );
            return;
        };
        {
            let _guard = self.base_mut();
            let node = root.clone();
            if let Some(mut old) = node.get_parent() {
                old.remove_child(&node);
            }
            new_parent.add_child(&node);
            if let Some((level, spawn)) = place {
                place_at_spawn(&node, level, spawn);
            }
            dest.bind_mut().set_entry_parent(root, parent);
        }
        if !self.is_server() {
            // WP4 / WP5 reset points: the owner re-tracks prediction, replicas start new buffers.
            self.with_core(|c, _| {
                for id in &ids {
                    c.untrack_prediction(*id);
                    c.interp_reset(*id);
                }
            });
        }
        let parts = dest.bind().parts_of(root);
        {
            let _guard = self.base_mut();
            notify_travelled(&parts, rooms.0, rooms.1);
        }
        self.base_mut().emit_signal(
            "travelled",
            &[
                root.to_variant(),
                GString::from(rooms.0).to_variant(),
                GString::from(rooms.1).to_variant(),
            ],
        );
    }

    /// Client: a cross-manager `Reparent` whose two managers exist.
    pub(crate) fn apply_move(
        &mut self,
        managers: &[(u64, Gd<NetManager>)],
        id: NetId,
        from_manager: u64,
        parent: ParentRef,
    ) {
        let find = |k: u64| {
            managers
                .iter()
                .find(|(mk, _)| *mk == k)
                .map(|(_, m)| m.clone())
        };
        let (Some(src), Some(dest)) = (find(from_manager), find(parent.manager)) else {
            return;
        };
        let Some(root) = src.bind().root_of_id(id) else {
            return;
        };
        let (from, to) = (self.room_name(&src), self.room_name(&dest));
        self.move_entity(src, dest, &root, parent, (&from, &to), None);
    }

    /// Client: the server moved this client to `room` (a level): load it now and acknowledge.
    pub(crate) fn on_travel_event(&mut self, scene_path: &str, room: &str) {
        self.travel_target = Some(scene_path.to_string());
        self.local_room = room.to_string();
        self.travelling_client = true;
        self.do_load_level(scene_path);
        self.with_core(|c, now| c.travel_ack(room, now));
    }

    /// Client: frees every loaded level but the `Travel`'s that holds no entity this client owns.
    pub(crate) fn free_stale_levels(&mut self) {
        if self.is_server() {
            return;
        }
        self.live_levels();
        let local = self.unique_id() as u32;
        let managers = self.managers();
        let mut stale = Vec::new();
        for l in &self.levels {
            let holds = managers
                .iter()
                .filter(|(_, m)| l.node.is_ancestor_of(m))
                .any(|(_, m)| m.bind().holds_owned_by(local));
            if self.travel_target.as_deref() == Some(l.path.as_str()) {
                if holds {
                    self.travelling_client = false;
                }
            } else if !holds {
                stale.push(l.path.clone());
            }
        }
        for p in stale {
            self.do_unload_level(&p);
        }
    }

    /// Server: counts the ticks each level has been without a player and unloads the ones empty for
    /// `empty_level_timeout` seconds.
    pub(crate) fn check_empty_levels(&mut self) {
        self.live_levels();
        if self.levels.is_empty() {
            return;
        }
        let occupied: HashSet<u64> = self
            .with_core(|c, _| {
                c.players()
                    .iter()
                    .filter_map(|(_, id)| c.entities().get(*id).map(|r| r.parent.manager))
                    .collect()
            })
            .unwrap_or_default();
        let managers = self.managers();
        let secs = self.effective_empty_timeout();
        let limit = (secs * f64::from(crate::session::tick_rate()))
            .ceil()
            .max(1.0) as u64;
        let mut unload = Vec::new();
        for l in &mut self.levels {
            let busy = managers
                .iter()
                .any(|(k, m)| occupied.contains(k) && l.node.is_ancestor_of(m));
            if busy {
                l.empty_ticks = 0;
            } else {
                l.empty_ticks += 1;
                if l.empty_ticks >= limit {
                    unload.push(l.path.clone());
                }
            }
        }
        for p in unload {
            self.do_unload_level(&p);
        }
    }

    pub(crate) fn effective_empty_timeout(&self) -> f64 {
        if self.empty_level_timeout > 0.0 {
            self.empty_level_timeout
        } else {
            world::empty_timeout_s()
        }
    }
}

/// Client routing: whether a cross-manager `Reparent` can be applied now (both managers exist).
pub fn is_cross_move(e: &EntityEvent) -> Option<(NetId, u64, ParentRef)> {
    match e {
        EntityEvent::Reparent {
            id,
            from_manager,
            parent,
        } if parent.manager != *from_manager => Some((*id, *from_manager, *parent)),
        _ => None,
    }
}

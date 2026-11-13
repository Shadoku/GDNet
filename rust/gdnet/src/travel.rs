//! Travel between levels (docs/PLAN_NETCODE.md 2.11, 12.2; WP13, WP16; netcore's side is
//! netcore/src/rooms.rs).
//!
//! Server, `Net.travel(player, scene_path, spawn_point)`: the request is queued and applied at the
//! session's next step (never inside a physics callback such as a door's `body_entered`, where moving a
//! body is refused by the engine): the target level is loaded under the World if it is not, its
//! managers register their rooms, the player's entry (and the entries of the entities below it) move
//! from its manager to the level's manager, the node is reparented under that manager (at the
//! `spawn_point` marker, else keeping its global position), `_net_travelled(from, to)` runs on its
//! parts and `travelled(player, from, to)` fires; then netcore's cross-manager `Reparent` changes the
//! player's rooms, which sends its owner a `Travel` for each level room it gains, the scope change
//! (`Scope` leaves, `RoomLeft`s, `Create`s), then the `Reparent`. A peer that has both rooms gets only
//! the `Reparent`. `NetManager.auto_travel` queues the same request when a player walks into the
//! manager's `bounds`.
//!
//! Every server tick the levels of rooms that some peer wants (neighbours, added rooms) and no manager
//! registered are loaded; levels nobody wants for `empty_level_timeout` seconds are unloaded (their
//! entities are destroyed first, so the peers that saw them free them).
//!
//! Client: a `Travel` event loads the level at once (before the events after it are routed, so they
//! find its managers), at the origin it names, and is acknowledged with `TravelAck`; a cross-manager
//! `Reparent` waits until both managers exist, then moves the entry and the node the same way and
//! fires the same callbacks (prediction and interpolation restart only when the move changes the
//! entity's local frame); a `RoomLeft` frees the room's level once no room of the client names it and
//! nothing the client owns is in it.

use crate::manager::{entity_root_of, NetManager};
use crate::session::NetSession;
use crate::world::{self, instantiate_level, notify_travelled, place_at_spawn, world_of};
use godot::classes::{Node, Node2D, Node3D, ResourceLoader};
use godot::prelude::*;
use netcore::session::EntityEvent;
use netcore::{room_hash, NetId, Origin, ParentRef, RoomEntry, MANAGER_ROOT};
use std::collections::HashSet;

/// A level this session loaded.
pub struct Level {
    pub path: String,
    pub node: Gd<Node>,
    /// Server: ticks in a row nobody wanted it.
    pub empty_ticks: u64,
    /// WP16: where its root was put (None: where it was authored).
    pub origin: Option<Origin>,
    /// WP16, server: the grid cell it took (`gdnet/levels/spacing`).
    pub cell: Option<u32>,
}

/// Where a level goes when it is loaded (WP16).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    /// The server's default: the next free grid cell with `gdnet/levels/spacing` > 0 (unless a manager
    /// of the level names neighbours), else where it was authored.
    Auto,
    /// At this origin.
    At(Origin),
    /// Where it was authored.
    Authored,
}

/// Where a queued travel goes.
pub enum TravelTarget {
    /// The first manager of this level (loaded if needed).
    Scene(String),
    /// This manager (auto-travel).
    Manager(Gd<NetManager>),
}

/// A queued `Net.travel`.
pub struct TravelReq {
    pub player: Gd<Node>,
    pub target: TravelTarget,
    /// The marker to arrive at; empty: keep the global position.
    pub spawn_point: String,
}

/// Whether two parents give their children the same global frame (both non-spatial, or spatial
/// with the same global transform): a child moved between them keeps its local values.
fn same_frame(a: Option<Gd<Node>>, b: &Gd<Node>) -> bool {
    let Some(a) = a else {
        return true;
    };
    match (
        a.clone().try_cast::<Node2D>(),
        b.clone().try_cast::<Node2D>(),
    ) {
        (Ok(x), Ok(y)) => return x.get_global_transform() == y.get_global_transform(),
        (Err(_), Err(_)) => {}
        _ => return false,
    }
    match (
        a.clone().try_cast::<Node3D>(),
        b.clone().try_cast::<Node3D>(),
    ) {
        (Ok(x), Ok(y)) => x.get_global_transform() == y.get_global_transform(),
        (Err(_), Err(_)) => true,
        _ => false,
    }
}

/// The global position of a spatial node (`Vector2` / `Vector3`), null otherwise.
pub(crate) fn global_position_of(node: &Gd<Node>) -> Variant {
    if let Ok(n) = node.clone().try_cast::<Node2D>() {
        n.get_global_position().to_variant()
    } else if let Ok(n) = node.clone().try_cast::<Node3D>() {
        n.get_global_position().to_variant()
    } else {
        Variant::nil()
    }
}

/// Saves a spatial node's global transform, to put it back after a reparent.
enum Pose {
    Flat(Transform2D),
    Deep(Transform3D),
    None,
}

fn pose_of(node: &Gd<Node>) -> Pose {
    if let Ok(n) = node.clone().try_cast::<Node2D>() {
        Pose::Flat(n.get_global_transform())
    } else if let Ok(n) = node.clone().try_cast::<Node3D>() {
        Pose::Deep(n.get_global_transform())
    } else {
        Pose::None
    }
}

fn restore_pose(node: &Gd<Node>, pose: Pose) {
    match pose {
        Pose::Flat(t) => {
            if let Ok(mut n) = node.clone().try_cast::<Node2D>() {
                n.set_global_transform(t);
            }
        }
        Pose::Deep(t) => {
            if let Ok(mut n) = node.clone().try_cast::<Node3D>() {
                n.set_global_transform(t);
            }
        }
        Pose::None => {}
    }
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

    /// WP16: the origin a loaded level was put at (None: authored position, or not loaded).
    pub(crate) fn origin_of_level(&mut self, scene_path: &str) -> Option<Origin> {
        self.live_levels();
        self.levels
            .iter()
            .find(|l| l.path == scene_path)
            .and_then(|l| l.origin)
    }

    /// The next free grid cell (n >= 1).
    fn free_cell(&self) -> u32 {
        let used: HashSet<u32> = self.levels.iter().filter_map(|l| l.cell).collect();
        (1..).find(|n| !used.contains(n)).unwrap_or(1)
    }

    /// Whether a manager inside `level` names neighbours (a chunk keeps its authored position).
    fn names_neighbours(level: &Gd<Node>) -> bool {
        fn walk(n: &Gd<Node>) -> bool {
            if let Ok(m) = n.clone().try_cast::<NetManager>() {
                if !m.bind().neighbours_setting().is_empty() {
                    return true;
                }
            }
            n.get_children().iter_shared().any(|c| walk(&c))
        }
        walk(level)
    }

    /// Loads `scene_path` under the World (or returns the loaded level), placed as `place` says. On
    /// the server the session becomes room-strict (2.11) and the level's managers register their rooms
    /// at once.
    pub(crate) fn do_load_level(&mut self, scene_path: &str, place: Place) -> Option<Gd<Node>> {
        self.live_levels();
        if let Some(i) = self.levels.iter().position(|l| l.path == scene_path) {
            // A client told to put a loaded level elsewhere (the server reloaded it) moves it.
            if let Place::At(o) = place {
                if !self.is_server() && self.levels[i].origin != Some(o) {
                    let node = self.levels[i].node.clone();
                    let _guard = self.base_mut();
                    world::place_level(&node, o);
                    drop(_guard);
                    self.levels[i].origin = Some(o);
                }
            }
            return Some(self.levels[i].node.clone());
        }
        let Some(mut w) = self.world() else {
            godot_error!(
                "gdnet: load_level({scene_path}): the session has no branch to load it in"
            );
            return None;
        };
        let node = instantiate_level(scene_path)?;
        let (origin, cell) = match place {
            Place::At(o) => (world::place_level(&node, o).then_some(o), None),
            Place::Authored => (None, None),
            Place::Auto => {
                let spacing = world::spacing();
                let deep = world::level_is_deep(&node);
                match deep {
                    Some(deep)
                        if self.is_server() && spacing > 0.0 && !Self::names_neighbours(&node) =>
                    {
                        let cell = self.free_cell();
                        let o = Origin::cell(cell, spacing as f32, deep);
                        world::place_level(&node, o);
                        (Some(o), Some(cell))
                    }
                    _ => (None, None),
                }
            }
        };
        {
            let _guard = self.base_mut();
            w.add_child(&node);
        }
        self.levels.push(Level {
            path: scene_path.to_string(),
            node: node.clone(),
            empty_ticks: 0,
            origin,
            cell,
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

    /// The loaded level `m` sits in.
    fn level_of_manager(&self, m: &Gd<NetManager>) -> Option<&Level> {
        self.levels
            .iter()
            .find(|l| l.node.is_instance_valid() && l.node.is_ancestor_of(m))
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

    /// Server: tells netcore which manager is in which room, in which level at which origin, with
    /// which neighbours (cheap when nothing changed).
    pub(crate) fn register_rooms(&mut self) {
        let table: Vec<RoomEntry> = self
            .managers()
            .iter()
            .map(|(k, m)| {
                let level = self.level_of_manager(m);
                RoomEntry {
                    manager: *k,
                    room: self.room_name(m),
                    level: level.map(|l| l.path.clone()),
                    origin: level.and_then(|l| l.origin),
                    neighbours: m.bind().neighbours_setting(),
                }
            })
            .collect();
        self.with_core(|c, now| c.set_rooms(&table, now));
    }

    /// Server (WP16): loads the levels of rooms some peer wants (a neighbour, an added room) that no
    /// manager registered yet; a room is a level when its name is a scene that exists.
    pub(crate) fn load_wanted_levels(&mut self) {
        let missing: Vec<String> = self
            .with_core(|c, _| {
                c.wanted_rooms()
                    .into_iter()
                    .filter(|r| !c.room_registered(r))
                    .collect()
            })
            .unwrap_or_default();
        for name in missing {
            let scene = name.ends_with(".tscn") || name.ends_with(".scn");
            if !scene || self.not_levels.contains(&name) || self.level_paths().contains(&name) {
                continue;
            }
            if ResourceLoader::singleton().exists(name.as_str()) {
                self.do_load_level(&name, Place::Auto);
            } else {
                godot_warn!(
                    "gdnet: a peer has the room {name} (a neighbour or an added room), but there is no such scene to load"
                );
                self.not_levels.insert(name);
            }
        }
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
            let (level, dest) = match req.target {
                TravelTarget::Scene(path) => {
                    let Some(level) = self.do_load_level(&path, Place::Auto) else {
                        continue;
                    };
                    let found = self
                        .managers()
                        .into_iter()
                        .map(|(_, m)| m)
                        .find(|m| level.is_ancestor_of(m));
                    let Some(dest) = found else {
                        godot_error!("gdnet: Net.travel: the level {path} has no NetManager");
                        continue;
                    };
                    (level, dest)
                }
                TravelTarget::Manager(m) => {
                    if !m.is_instance_valid() {
                        continue;
                    }
                    let level = self
                        .level_of_manager(&m)
                        .map(|l| l.node.clone())
                        .or_else(|| m.get_parent())
                        .unwrap_or_else(|| m.clone().upcast());
                    (level, m)
                }
            };
            let skey = self.manager_key(&src.bind().get_managed_root());
            let dkey = self.manager_key(&dest.bind().get_managed_root());
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

    /// Server (WP16): each player entity standing inside the `bounds` of another manager with
    /// `auto_travel` (and outside its own manager's bounds) travels there, keeping its position.
    pub(crate) fn check_auto_travel(&mut self) {
        let managers = self.managers();
        let targets: Vec<&(u64, Gd<NetManager>)> = managers
            .iter()
            .filter(|(_, m)| m.bind().auto_travel_setting())
            .collect();
        if targets.is_empty() {
            return;
        }
        let players: Vec<(NetId, u64)> = self
            .with_core(|c, _| {
                c.players()
                    .iter()
                    .filter_map(|(_, id)| c.entities().get(*id).map(|r| (*id, r.parent.manager)))
                    .collect()
            })
            .unwrap_or_default();
        for (id, key) in players {
            let Some((_, src)) = managers.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            let Some(root) = src.bind().root_of_id(id) else {
                continue;
            };
            if self.travels.iter().any(|r| r.player == root) {
                continue;
            }
            let at = global_position_of(&root);
            if at.is_nil() || src.bind().bounds_contain(&at) == Some(true) {
                continue;
            }
            let dest = targets
                .iter()
                .find(|(k, m)| *k != key && m.bind().bounds_contain(&at) == Some(true));
            if let Some((_, m)) = dest {
                self.travels.push(TravelReq {
                    player: root,
                    target: TravelTarget::Manager(m.clone()),
                    spawn_point: String::new(),
                });
            }
        }
    }

    /// Moves the entity rooted at `root` from manager `src` to `dest` under `parent` (at a spawn marker
    /// of `place.0` when `place.1` names one, else keeping its global transform), then runs the travel
    /// callbacks. Prediction and interpolation of the moved entities restart only on a client and
    /// only when the move changes the entity's local frame (the two managed roots differ in transform).
    fn move_entity(
        &mut self,
        mut src: Gd<NetManager>,
        mut dest: Gd<NetManager>,
        root: &Gd<Node>,
        parent: ParentRef,
        rooms: (&str, &str),
        place: Option<(&Gd<Node>, &str)>,
    ) {
        let Some(mut new_parent) = dest.bind().resolve_parent(&parent) else {
            godot_error!(
                "gdnet: travel: the destination of {} was not found",
                root.get_path()
            );
            return;
        };
        let continuous = same_frame(root.get_parent(), &new_parent);
        let restart = !self.is_server() && !continuous;
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
            dest.bind_mut().put_entries(moved, generation, restart)
        };
        {
            let _guard = self.base_mut();
            let node = root.clone();
            let pose = pose_of(&node);
            if let Some(mut old) = node.get_parent() {
                old.remove_child(&node);
            }
            new_parent.add_child(&node);
            if !continuous {
                restore_pose(&node, pose);
            }
            if let Some((level, spawn)) = place {
                place_at_spawn(&node, level, spawn);
            }
            dest.bind_mut().set_entry_parent(root, parent);
        }
        if restart {
            // WP4 / WP5 reset points: the owner re-tracks prediction, replicas start new buffers.
            self.with_core(|c, _| {
                for id in &ids {
                    c.untrack_prediction(*id);
                    c.interp_reset(*id);
                }
            });
        }
        // The engine skips a node removed during the physics pass for the rest of that pass, and the
        // session moves it from its own `_physics_process`: its tick is run after the pass instead.
        self.missed_physics.push(root.clone());
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

    /// Runs this tick's physics callbacks of the nodes moved between managers during the session's
    /// step (Godot's physics pass skips a node removed while it runs, so the moved subtree missed its
    /// tick; without this a moving player would fall a tick behind and its owner roll back). Called
    /// from `post_tick`, after every other node's `_physics_process` and before recording.
    pub(crate) fn catch_up_moved(&mut self) {
        let moved = std::mem::take(&mut self.missed_physics);
        if moved.is_empty() {
            return;
        }
        fn walk(n: &Gd<Node>, out: &mut Vec<Gd<Node>>) {
            out.push(n.clone());
            for c in n.get_children().iter_shared() {
                walk(&c, out);
            }
        }
        let mut nodes = Vec::new();
        for root in moved {
            if root.is_instance_valid() && root.is_inside_tree() && !nodes.contains(&root) {
                walk(&root, &mut nodes);
            }
        }
        let _guard = self.base_mut();
        for mut n in nodes {
            if !n.is_instance_valid() || !n.is_inside_tree() || !n.can_process() {
                continue;
            }
            if n.is_physics_processing_internal() {
                n.notify(godot::classes::notify::NodeNotification::INTERNAL_PHYSICS_PROCESS);
            }
            if n.is_physics_processing() {
                n.notify(godot::classes::notify::NodeNotification::PHYSICS_PROCESS);
            }
        }
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

    /// Client: `room`, a level, joined this client's rooms: load it now (at `origin`) and acknowledge.
    pub(crate) fn on_travel_event(&mut self, scene_path: &str, room: &str, origin: Option<Origin>) {
        self.room_levels
            .insert(room.to_string(), scene_path.to_string());
        let place = origin.map_or(Place::Authored, Place::At);
        self.do_load_level(scene_path, place);
        self.with_core(|c, now| c.travel_ack(room, now));
    }

    /// Client: `room` left this client's rooms; its level is freed by `free_left_levels`.
    pub(crate) fn on_room_left(&mut self, room: &str) {
        self.room_levels.remove(room);
    }

    /// Client: frees every loaded level that no room of this client names any more, unless an entity
    /// this client owns is still in it (the server moves it out, or destroys it, next).
    pub(crate) fn free_left_levels(&mut self) {
        if self.is_server() {
            return;
        }
        self.live_levels();
        let wanted: HashSet<&String> = self.room_levels.values().collect();
        let left: Vec<&Level> = self
            .levels
            .iter()
            .filter(|l| !wanted.contains(&l.path))
            .collect();
        if left.is_empty() {
            return;
        }
        let local = self.unique_id() as u32;
        let managers = self.managers();
        let mut stale = Vec::new();
        for l in left {
            let holds = managers
                .iter()
                .filter(|(_, m)| l.node.is_ancestor_of(m))
                .any(|(_, m)| m.bind().holds_owned_by(local));
            if !holds {
                stale.push(l.path.clone());
            }
        }
        for p in stale {
            self.do_unload_level(&p);
        }
    }

    /// Client: its primary room, the room of the manager holding its first entity (the player, as
    /// on the server: the first entity `spawn_for`'d to a peer); else the room of its first level.
    pub(crate) fn client_room(&self) -> String {
        let Some(core) = self.core.as_ref() else {
            return String::new();
        };
        let local = core.local_id();
        let first = core
            .entities()
            .iter()
            .filter(|r| r.owner == local && r.parent.entity == MANAGER_ROOT)
            .min_by_key(|r| r.seq)
            .map(|r| r.parent.manager);
        if let Some(key) = first {
            if let Some((_, m)) = self.managers().into_iter().find(|(k, _)| *k == key) {
                return self.room_name(&m);
            }
        }
        self.room_levels.keys().next().cloned().unwrap_or_default()
    }

    /// Client: the rooms whose levels it holds (from the server's `Travel`s).
    pub(crate) fn client_rooms(&self) -> Vec<String> {
        self.room_levels.keys().cloned().collect()
    }

    /// Server: counts the ticks each level has been unwanted (no player in it, and its room in no
    /// peer's set) and unloads the ones unwanted for `empty_level_timeout` seconds.
    pub(crate) fn check_empty_levels(&mut self) {
        self.live_levels();
        if self.levels.is_empty() {
            return;
        }
        let (occupied, wanted): (HashSet<u64>, HashSet<u64>) = self
            .with_core(|c, _| {
                let occupied = c
                    .players()
                    .iter()
                    .filter_map(|(_, id)| c.entities().get(*id).map(|r| r.parent.manager))
                    .collect();
                let wanted = c.wanted_rooms().iter().map(|r| room_hash(r)).collect();
                (occupied, wanted)
            })
            .unwrap_or_default();
        let managers = self.managers();
        let rooms: Vec<(u64, u64)> = managers
            .iter()
            .map(|(k, m)| (*k, room_hash(&self.room_name(m))))
            .collect();
        let secs = self.effective_empty_timeout();
        let limit = (secs * f64::from(crate::session::tick_rate()))
            .ceil()
            .max(1.0) as u64;
        let mut unload = Vec::new();
        for l in &mut self.levels {
            let busy = managers.iter().zip(&rooms).any(|((k, m), (_, room))| {
                l.node.is_ancestor_of(m) && (occupied.contains(k) || wanted.contains(room))
            });
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

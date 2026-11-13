//! Level state, gdnet's side (docs/PLAN_NETCODE.md 12.2 "Level state", decision D20; WP16). The format
//! and its versioning are netcore's (netcore/src/levelstate.rs). This module is a child of `manager`
//! (declared there), so the manager's half reads its entries directly.
//!
//! Server only (an offline session is a server). When the server loads a level, its managers record
//! the entities enrolled at that moment as the level's own content (`mark_homes`): their addresses
//! (paths relative to the managed root) are how a later state finds them again. When it unloads a level
//! (`unload_level`, the empty-level timeout), the managers whose `keep_state` is on are captured
//! (`capture_level`): every entity under them except players and what hangs below a player, with
//! its replicated values read from the nodes (the values the next `Create` would carry), the level
//! content that is gone, and whether each entity was spawned at run time. The bytes are kept per scene
//! path in `NetSession::kept_levels` until the next load of that level, where `restore_level` frees the
//! gone content, instantiates the spawned entities (parents first) and writes the values, after the
//! level entered the tree (its nodes' `_ready` ran) and before the next sync announces anything: the
//! peers' `Create`s carry the kept values, and `_net_spawned` sees them on every peer. Positions that
//! were captured at another level origin are moved by the difference.
//!
//! Every `Travel` to a level names its content that is gone on the server (`Session::set_level_gone`),
//! so a client loading the level frees its copies, which no `Create` would ever claim (also without
//! `keep_state`: a coin picked up before the client arrived).
//!
//! A blob is refused (with an error; the level loads fresh) when it does not decode, has an unknown
//! version, belongs to another scene, or names a manager or a level entity the scene does not have (or
//! one of another scene). Unknown property names are skipped, so a blob outlives a property added to a
//! scene. Restored entities belong to the server (or to the owner their scene gives them); WP17: the
//! blob records each entity's owner as a seat, and a restored entity whose seat is connected (or
//! connects later in the session: a rejoin, or a session started from a saved state) goes to that
//! seat's peer (`NetSession::process_seat_claims`).

use super::{part_node_list, KeepState, NetManager, PartNodes};
use crate::apply;
use crate::infer::RuntimeProps;
use crate::parts::{self, SceneInfo};
use crate::session::NetSession;
use godot::classes::{CanvasItem, Node, Node2D, Node3D};
use godot::prelude::*;
use netcore::levelstate::{EntityState, LevelState, ManagerState, PartValues};
use netcore::{NetId, Origin as Placed};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The project setting for `keep_state`'s default.
pub const SETTING_KEEP_STATE: &str = "gdnet/levels/keep_state";

/// `gdnet/levels/keep_state` (default true, D20).
pub fn keep_state_default() -> bool {
    crate::hash::bool_setting(SETTING_KEEP_STATE, true)
}

impl NetManager {
    /// Whether this manager's entities are kept when its level unloads.
    pub(crate) fn keeps_state(&self) -> bool {
        match self.keep_state {
            KeepState::On => true,
            KeepState::Off => false,
            KeepState::Default => keep_state_default(),
        }
    }

    /// Server, right after its level was loaded: every entity enrolled now is level content.
    pub(crate) fn mark_homes(&mut self) {
        let root = self.get_managed_root();
        self.homes.clear();
        for iid in self.order.clone() {
            let Ok(n) = Gd::<Node>::try_from_instance_id(iid) else {
                continue;
            };
            let Some(e) = self.entries.get_mut(&iid) else {
                continue;
            };
            let address: Rc<str> = Rc::from(root.get_path_to(&n).to_string().as_str());
            e.home = Some(address.clone());
            self.homes.push(address);
        }
        self.homes_changed = false;
    }

    /// The addresses of this level's own entities that are no longer here (destroyed, or moved out).
    pub(crate) fn gone_homes(&self) -> Vec<String> {
        if self.homes.is_empty() {
            return Vec::new();
        }
        let root = self.get_managed_root();
        let present: HashSet<&str> = self
            .entries
            .iter()
            .filter(|(iid, _)| {
                Gd::<Node>::try_from_instance_id(**iid)
                    .is_ok_and(|n| root.is_ancestor_of(&n) && !n.is_queued_for_deletion())
            })
            .filter_map(|(_, e)| e.home.as_deref())
            .collect();
        self.homes
            .iter()
            .filter(|h| !present.contains(&***h))
            .map(|h| h.to_string())
            .collect()
    }

    /// Whether level content went away since the last call.
    pub(crate) fn take_homes_changed(&mut self) -> bool {
        std::mem::take(&mut self.homes_changed)
    }

    /// The kept state of this manager (its `path` is the caller's to fill): every entity but the
    /// players and what hangs below them, in enrolment order (parents first), and the gone content.
    /// The caller holds the session's base guard (property tables register with the core).
    pub(crate) fn capture(
        &mut self,
        session: &Gd<NetSession>,
        players: &[Gd<Node>],
        seat_of: &HashMap<u32, u32>,
    ) -> ManagerState {
        let root = self.get_managed_root();
        let mut out = ManagerState {
            removed: self.gone_homes(),
            ..ManagerState::default()
        };
        for iid in self.order.clone() {
            let Ok(node) = Gd::<Node>::try_from_instance_id(iid) else {
                continue;
            };
            if !root.is_ancestor_of(&node) || node.is_queued_for_deletion() {
                continue;
            }
            if players
                .iter()
                .any(|p| *p == node || p.is_ancestor_of(&node))
            {
                continue;
            }
            let Some(e) = self.entries.get(&iid) else {
                continue;
            };
            let (scene, home, owner, props) =
                (e.scene.clone(), e.home.clone(), e.owner, e.props.clone());
            let props = match props {
                Some(p) => Some(p),
                None => self.props_for(&node, &scene, session),
            };
            let parts = props
                .map(|rt| {
                    let nodes = part_node_list(&scene, &node);
                    let _guard = self.base_mut();
                    read_named(&rt, &scene, &nodes)
                })
                .unwrap_or_default();
            let address = match &home {
                Some(h) => h.to_string(),
                None => root.get_path_to(&node).to_string(),
            };
            out.entities.push(EntityState {
                address,
                scene: scene.path.clone(),
                spawned: home.is_none(),
                owner_seat: seat_of.get(&owner).copied().unwrap_or(0),
                parts,
            });
        }
        out
    }

    /// The scene path of the entity rooted at `node` (None: not an entity of this manager).
    fn scene_of(&self, node: &Gd<Node>) -> Option<String> {
        self.entries
            .get(&node.instance_id())
            .map(|e| e.scene.path.clone())
    }

    /// Writes kept values to the entity rooted at `node` (unknown parts and properties are skipped).
    /// Returns whether its root part's `position` was among them. The caller holds the session's base
    /// guard.
    pub(crate) fn write_kept(
        &mut self,
        session: &Gd<NetSession>,
        node: &Gd<Node>,
        parts: &[PartValues],
    ) -> bool {
        let Some(e) = self.entries.get(&node.instance_id()) else {
            return false;
        };
        let (scene, props) = (e.scene.clone(), e.props.clone());
        let rt = match props {
            Some(p) => Some(p),
            None => self.props_for(node, &scene, session),
        };
        let Some(rt) = rt else {
            return false;
        };
        let nodes = part_node_list(&scene, node);
        let mut writes = Vec::new();
        let mut moved = false;
        for pv in parts {
            let Some(p) = scene.table.index_of(&pv.part).map(usize::from) else {
                continue;
            };
            let (Some(names), Some(fields), Some(Some(target))) =
                (rt.names.get(p), rt.fields.get(p), nodes.get(p))
            else {
                continue;
            };
            for (name, value) in &pv.fields {
                if let Some(f) = names.iter().position(|n| n == name) {
                    writes.push((target.clone(), fields[f].clone(), value.clone()));
                    moved |= p == 0 && name == "position";
                }
            }
        }
        let _guard = self.base_mut();
        for (n, f, v) in writes {
            apply::write(&n, &f, &v);
        }
        moved
    }
}

/// Every part's fields by name, as the nodes hold them now.
fn read_named(rt: &RuntimeProps, scene: &SceneInfo, nodes: &PartNodes) -> Vec<PartValues> {
    rt.fields
        .iter()
        .enumerate()
        .filter(|(_, fields)| !fields.is_empty())
        .filter_map(|(p, fields)| {
            let node = nodes.get(p)?.as_ref()?;
            let part = scene.table.entries.get(p)?.0.clone();
            let names = rt.names.get(p)?;
            Some(PartValues {
                part,
                fields: fields
                    .iter()
                    .zip(names)
                    .map(|(f, n)| (n.clone(), apply::read(node, f)))
                    .collect(),
            })
        })
        .collect()
}

/// A level root's position (None when it is neither a `Node2D` nor a `Node3D`).
fn root_position(level: &Gd<Node>) -> Option<Placed> {
    if let Ok(n) = level.clone().try_cast::<Node2D>() {
        let p = n.get_position();
        Some(Placed::Flat([p.x, p.y]))
    } else if let Ok(n) = level.clone().try_cast::<Node3D>() {
        let p = n.get_position();
        Some(Placed::Deep([p.x, p.y, p.z]))
    } else {
        None
    }
}

/// Moves `node` by `delta` when its transform is not its parent's (it hangs under a plain `Node` such
/// as a `NetManager`, or is `top_level`): the nodes `world::place_level` moves with their level.
fn shift_detached(node: &Gd<Node>, delta: Vector3) {
    let parent = node.get_parent();
    if let Ok(mut n) = node.clone().try_cast::<Node2D>() {
        let inherits =
            parent.is_some_and(|p| p.try_cast::<CanvasItem>().is_ok()) && !n.is_set_as_top_level();
        if !inherits {
            let p = n.get_position();
            n.set_position(p + Vector2::new(delta.x, delta.y));
        }
    } else if let Ok(mut n) = node.clone().try_cast::<Node3D>() {
        let inherits =
            parent.is_some_and(|p| p.try_cast::<Node3D>().is_ok()) && !n.is_set_as_top_level();
        if !inherits {
            let p = n.get_position();
            n.set_position(p + delta);
        }
    }
}

/// The offset from a captured root position to the current one (None when there is nothing to move).
fn offset(from: Option<Placed>, to: Option<Placed>) -> Option<Vector3> {
    let d = match (from, to) {
        (Some(Placed::Flat([a, b])), Some(Placed::Flat([x, y]))) => Vector3::new(x - a, y - b, 0.0),
        (Some(Placed::Deep([a, b, c])), Some(Placed::Deep([x, y, z]))) => {
            Vector3::new(x - a, y - b, z - c)
        }
        _ => return None,
    };
    (d != Vector3::ZERO).then_some(d)
}

/// A manager of the level being restored, checked.
struct Target {
    manager: Gd<NetManager>,
    root: Gd<Node>,
    split: PackedStringArray,
}

impl NetSession {
    /// The root nodes of every peer's player (they and what hangs below them are never kept).
    fn player_roots(&self) -> Vec<Gd<Node>> {
        let ids: Vec<NetId> = self
            .core
            .as_ref()
            .map(|c| c.players().iter().map(|(_, id)| *id).collect())
            .unwrap_or_default();
        if ids.is_empty() {
            return Vec::new();
        }
        let managers = self.managers();
        ids.iter()
            .filter_map(|id| managers.iter().find_map(|(_, m)| m.bind().root_of_id(*id)))
            .collect()
    }

    /// The managers inside a loaded level.
    fn managers_in(&self, level: &Gd<Node>) -> Vec<Gd<NetManager>> {
        self.managers()
            .into_iter()
            .map(|(_, m)| m)
            .filter(|m| level.is_ancestor_of(m))
            .collect()
    }

    /// Server: the state of a loaded level's managers whose `keep_state` is on; None when none is.
    pub(crate) fn capture_level(
        &mut self,
        level: &Gd<Node>,
        scene_path: &str,
    ) -> Option<LevelState> {
        let players = self.player_roots();
        // WP17: owners are recorded as seats (a peer id means nothing in a later session); the server's
        // own entities are the server's (0) whatever its seat.
        let seat_of: HashMap<u32, u32> = self
            .core
            .as_ref()
            .map(|c| {
                c.seat_table()
                    .into_iter()
                    .filter(|r| r.peer != netcore::SERVER && r.peer != 0)
                    .map(|r| (r.peer, r.seat))
                    .collect()
            })
            .unwrap_or_default();
        let me = self.to_gd();
        let mut st = LevelState::new(scene_path);
        st.origin = root_position(level);
        let mut any = false;
        for mut m in self.managers_in(level) {
            if !m.bind().keeps_state() {
                continue;
            }
            any = true;
            let mut ms = {
                let _guard = self.base_mut();
                m.bind_mut().capture(&me, &players, &seat_of)
            };
            ms.path = level.get_path_to(&m).to_string();
            st.managers.push(ms);
        }
        any.then_some(st)
    }

    /// Server: a level was just loaded under the World: its content is marked, then the state to start
    /// from is applied (`given`: the `state` argument of `load_level`, empty for a fresh start; None:
    /// the kept state, if any).
    pub(crate) fn level_loaded(
        &mut self,
        level: &Gd<Node>,
        scene_path: &str,
        given: Option<Vec<u8>>,
    ) {
        let managers = self.managers_in(level);
        for mut m in managers {
            m.bind_mut().mark_homes();
        }
        let kept = self.kept_levels.remove(scene_path);
        if let Some(bytes) = given.or(kept).filter(|b| !b.is_empty()) {
            if let Err(e) = self.restore_level(level, scene_path, &bytes) {
                godot_error!(
                    "gdnet: the level state for {scene_path} was refused: {e}; the level loads fresh"
                );
            }
        }
        self.update_level_gone(level, scene_path);
    }

    /// Server: a level is about to be unloaded: keeps its state (unless `reset_level` asked for a fresh
    /// next load, or no manager of it keeps state) and clears what its `Travel`s said was gone.
    pub(crate) fn level_unloading(&mut self, level: &Gd<Node>, scene_path: &str, reset: bool) {
        let st = if reset {
            None
        } else {
            self.capture_level(level, scene_path)
        };
        match st {
            Some(st) => {
                self.kept_levels.insert(scene_path.to_string(), st.encode());
            }
            None => {
                self.kept_levels.remove(scene_path);
            }
        }
        self.with_core(|c, _| c.set_level_gone(scene_path, Vec::new()));
    }

    /// Server: tells netcore which of the level's own entities are gone (paths from the level root).
    fn update_level_gone(&mut self, level: &Gd<Node>, scene_path: &str) {
        let mut gone = Vec::new();
        for m in self.managers_in(level) {
            let b = m.bind();
            let prefix = level.get_path_to(&b.get_managed_root()).to_string();
            for g in b.gone_homes() {
                gone.push(format!("{prefix}/{g}"));
            }
        }
        self.with_core(|c, _| c.set_level_gone(scene_path, gone));
    }

    /// Server, each step while levels are loaded: refreshes the gone lists of levels whose content
    /// changed (event-driven: a flag per manager).
    pub(crate) fn sync_level_gone(&mut self) {
        if self.levels.is_empty() {
            return;
        }
        let levels: Vec<(Gd<Node>, String)> = self
            .levels
            .iter()
            .filter(|l| l.node.is_instance_valid())
            .map(|l| (l.node.clone(), l.path.clone()))
            .collect();
        let managers = self.managers();
        for (node, path) in levels {
            let mut changed = false;
            for (_, m) in managers.iter().filter(|(_, m)| node.is_ancestor_of(m)) {
                changed |= m.clone().bind_mut().take_homes_changed();
            }
            if changed {
                self.update_level_gone(&node, &path);
            }
        }
    }

    /// Server: applies a blob to a level that just entered the tree. Checks everything before changing
    /// anything; Ok holds the number of entities written.
    fn restore_level(
        &mut self,
        level: &Gd<Node>,
        scene_path: &str,
        bytes: &[u8],
    ) -> Result<usize, String> {
        let st = LevelState::decode(bytes).map_err(|e| e.to_string())?;
        if st.scene != scene_path {
            return Err(format!("it is the state of {}", st.scene));
        }
        let mut targets = Vec::with_capacity(st.managers.len());
        for ms in &st.managers {
            let manager = level
                .get_node_or_null(ms.path.as_str())
                .and_then(|n| n.try_cast::<NetManager>().ok())
                .ok_or_else(|| format!("the level has no NetManager at {}", ms.path))?;
            let (root, split) = {
                let b = manager.bind();
                (b.get_managed_root(), b.split_scenes.clone())
            };
            for e in &ms.entities {
                if e.spawned {
                    if parts::scene_info(&e.scene, &split).is_none() {
                        return Err(format!(
                            "the scene {} of {}/{} cannot be loaded",
                            e.scene, ms.path, e.address
                        ));
                    }
                    continue;
                }
                let node = root
                    .get_node_or_null(e.address.as_str())
                    .ok_or_else(|| format!("{}/{} is not in the scene", ms.path, e.address))?;
                match manager.bind().scene_of(&node) {
                    Some(s) if s == e.scene => {}
                    Some(s) => {
                        return Err(format!(
                            "{}/{} is a {s} in the scene, not a {}",
                            ms.path, e.address, e.scene
                        ))
                    }
                    None => {
                        return Err(format!(
                            "{}/{} is not an entity in the scene",
                            ms.path, e.address
                        ))
                    }
                }
            }
            targets.push(Target {
                manager,
                root,
                split,
            });
        }
        let me = self.to_gd();
        let delta = offset(st.origin, root_position(level));
        let mut written = 0;
        let mut claims = Vec::new();
        for (t, ms) in targets.iter_mut().zip(&st.managers) {
            // The level content that was destroyed is freed again (it was never announced).
            for address in &ms.removed {
                if let Some(mut n) = t.root.get_node_or_null(address.as_str()) {
                    let _guard = self.base_mut();
                    n.queue_free();
                    if let Some(mut p) = n.get_parent() {
                        p.remove_child(&n);
                    }
                }
            }
            let order = ms.in_restore_order();
            // Spawned entities, parents first.
            for e in order.iter().filter(|e| e.spawned) {
                let parent = if e.parent_address().is_empty() {
                    Some(t.root.clone())
                } else {
                    t.root.get_node_or_null(e.parent_address())
                };
                let Some(mut parent) = parent else {
                    godot_warn!(
                        "gdnet: the kept entity {}/{} of {scene_path} has no parent to go under; left out",
                        ms.path,
                        e.address
                    );
                    continue;
                };
                let Some(mut n) =
                    parts::scene_info(&e.scene, &t.split).and_then(|i| parts::instantiate(&i))
                else {
                    continue;
                };
                n.set_name(e.name());
                let _guard = self.base_mut();
                parent.add_child(&n);
            }
            // Then every value.
            for e in &order {
                let Some(n) = t.root.get_node_or_null(e.address.as_str()) else {
                    continue;
                };
                let _guard = self.base_mut();
                let moved = t.manager.bind_mut().write_kept(&me, &n, &e.parts);
                if let (true, Some(d)) = (moved, delta) {
                    shift_detached(&n, d);
                }
                if e.owner_seat != 0 {
                    claims.push((e.owner_seat, n.clone()));
                }
                written += 1;
            }
        }
        // WP17: entities a seat owned go back to it once it is connected.
        self.seat_claims.extend(claims);
        Ok(written)
    }

    /// Client: frees the level content a `Travel` named as gone, unless a `Create` already claimed it.
    pub(crate) fn free_gone(&mut self, level: &Gd<Node>, gone: &[String]) {
        for path in gone {
            let Some(mut n) = level.get_node_or_null(path.as_str()) else {
                continue;
            };
            let unclaimed = crate::manager::entity_root_of(&n)
                .is_some_and(|(root, m)| root == n && m.bind().net_id_of(&n) == 0);
            if !unclaimed {
                continue;
            }
            let _guard = self.base_mut();
            n.queue_free();
            if let Some(mut p) = n.get_parent() {
                p.remove_child(&n);
            }
        }
    }
}

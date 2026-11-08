//! `NetManager` (docs/PLAN_NETCODE.md 2.1 "Enrolment" and "Spawn", 3.1, WP2): a plain `Node`; every
//! scene instance below it is an entity, the nodes inside an instance are its parts.
//!
//! Enrolment: the manager connects to `SceneTree.node_added` / `node_removed` and keeps the nodes of its
//! own subtree (its "root": itself, or the node `Net.manage()` was given). At `_ready` it enrols what is
//! already there ("static" entities, part of the level on every peer). Classification of a node:
//!
//! * inside a nested `NetManager`, in the group `net_local`, or below such a node: ignored;
//! * below an entity (not bare): a part when its path is in the entity's part table; else an entity when
//!   it is a scene instance (a split nested instance, or one added at run time); else ignored (a helper
//!   node a script added inside an entity);
//! * otherwise: a scene instance is an entity; a bare node is an entity of its class (a one-time
//!   warning recommends scenes).
//!
//! Spawning is driven by the session (`NetSession` calls `sync()` once per tick, in its `_physics_process`
//! at priority -1000, with a `base_mut()` guard held so user code may call back into gdnet): on the server
//! new entities are announced (`Create`), removed ones destroyed, moved ones reparented; on a client the
//! remote actions are applied (instantiate + name + `add_child` under the resolved parent, then
//! `_net_spawned`), a client-side spawn is refused under `server_only` and announced under
//! `owner_allowed`. A `Create` whose parent already holds a not-yet-bound static node of the same name
//! and scene adopts that node instead of instantiating (the level's own instances, and nested split
//! instances, exist on both peers).
//!
//! Re-entrancy: every call out into the engine that may run user code (instantiate, `add_child`,
//! `queue_free`, the `_net_*` calls, signal emits) holds `self.base_mut()`; core access goes through short
//! `session.bind_mut().with_core(...)` borrows.

use crate::entity_node::NetEntity;
use crate::parts::{self, SceneInfo};
use crate::session::NetSession;
use godot::classes::{Engine, INode, Node, SceneTree};
use godot::prelude::*;
use netcore::session::EntityEvent;
use netcore::{NetId, ParentRef, Spawn, MANAGER_ROOT};
use std::collections::HashMap;
use std::rc::Rc;

/// The group every manager joins, so a session can find the managers of its branch.
pub const MANAGERS_GROUP: &str = "__gdnet_managers";
/// Nodes in this group (and below them) are never enrolled (`Net.ignore()`).
pub const LOCAL_GROUP: &str = "net_local";
/// Meta set on a node gdnet itself added as a replica (so its own `node_added` is not re-announced).
pub const META_REPLICA: &str = "_net_replica";
/// Meta on an entity root: the instance id of the manager it belongs to.
pub const META_MANAGER: &str = "_net_manager";
/// Meta read at enrolment for the owner when the entity has no `NetEntity` (`Net.spawn_for`).
pub const META_OWNER: &str = "_net_owner";

/// Who may spawn entities under a manager.
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum SpawnPolicy {
    /// Only the server spawns; a client-side `add_child` of an entity is refused.
    #[default]
    ServerOnly = 0,
    /// A client may spawn; the entity is owned by that client and relayed by the server.
    OwnerAllowed = 1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// Present when the manager became ready, or created inside a remote entity's instance: expected on
    /// every peer. The server announces it; a client waits for its `Create` and adopts it.
    Static,
    /// Added at run time by local code.
    Local,
    /// Created by gdnet from a remote `Create`.
    Remote,
}

struct Entry {
    id: Option<NetId>,
    scene: Rc<SceneInfo>,
    owner: u32,
    origin: Origin,
    /// The parent last announced or applied.
    parent: Option<ParentRef>,
    /// Moved or removed since the last sync: check for a reparent or a destroy.
    dirty: bool,
    owner_dirty: bool,
    /// A parent that cannot be announced was reported once.
    reported: bool,
}

#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct NetManager {
    base: Base<Node>,
    /// Who may spawn entities here: `server_only` (default) or `owner_allowed`.
    #[export]
    spawn_policy: SpawnPolicy,
    /// Scene paths whose nested instances are entities of their own instead of parts of the instance
    /// they sit in (a `NetEntity` child does the same per instance).
    #[export]
    split_scenes: PackedStringArray,
    /// Spawned entities under this manager (read only).
    #[var(get = get_entities, no_set)]
    #[allow(dead_code)]
    entities: i64,
    /// The managed root: this node, or the node `Net.manage()` was given.
    root_override: Option<Gd<Node>>,
    entries: HashMap<InstanceId, Entry>,
    /// Enrolment order (parents before children); stale ids are skipped.
    order: Vec<InstanceId>,
    by_id: HashMap<NetId, InstanceId>,
    /// Ids destroyed by local code since the last sync (announced at the next one).
    destroyed: Vec<NetId>,
    generation: u64,
    online: bool,
    is_server: bool,
    local_id: u32,
    warned_bare: bool,
    connected_signals: bool,
}

#[godot_api]
impl INode for NetManager {
    fn ready(&mut self) {
        self.base_mut().add_to_group(MANAGERS_GROUP);
        {
            let mut tree = self.base().get_tree();
            let me = self.to_gd();
            for (signal, method) in [
                ("node_added", "_on_node_added"),
                ("node_removed", "_on_node_removed"),
            ] {
                let cb = Callable::from_object_method(&me, method);
                if !tree.is_connected(signal, &cb) {
                    tree.connect(signal, &cb);
                }
            }
            self.connected_signals = true;
        }
        self.scan_existing();
    }

    fn exit_tree(&mut self) {
        if self.connected_signals {
            if let Some(mut tree) = scene_tree() {
                let me = self.to_gd();
                for (signal, method) in [
                    ("node_added", "_on_node_added"),
                    ("node_removed", "_on_node_removed"),
                ] {
                    let cb = Callable::from_object_method(&me, method);
                    if tree.is_connected(signal, &cb) {
                        tree.disconnect(signal, &cb);
                    }
                }
            }
            self.connected_signals = false;
        }
        for (iid, _) in std::mem::take(&mut self.entries) {
            if let Ok(mut n) = Gd::<Node>::try_from_instance_id(iid) {
                if n.has_meta(META_MANAGER) {
                    n.remove_meta(META_MANAGER);
                }
            }
        }
        self.order.clear();
        self.by_id.clear();
        self.destroyed.clear();
        // Enrol again (as static content) if this manager re-enters the tree.
        self.base_mut().request_ready();
    }
}

#[godot_api]
impl NetManager {
    #[signal]
    fn entity_spawned(node: Gd<Node>);
    #[signal]
    fn entity_despawning(node: Gd<Node>);
    #[signal]
    fn spawn_refused(node: Gd<Node>);

    #[func]
    fn get_entities(&self) -> i64 {
        self.entries.values().filter(|e| e.id.is_some()).count() as i64
    }

    /// The node whose subtree this manager manages (itself unless created by `Net.manage()`).
    #[func]
    pub fn get_managed_root(&self) -> Gd<Node> {
        self.root_override
            .clone()
            .unwrap_or_else(|| self.to_gd().upcast())
    }

    #[func]
    fn _on_node_added(&mut self, node: Gd<Node>) {
        let root = self.get_managed_root();
        if !root.is_ancestor_of(&node) {
            return;
        }
        let iid = node.instance_id();
        if let Some(e) = self.entries.get_mut(&iid) {
            e.dirty = true;
            return;
        }
        if node.has_meta(META_REPLICA) {
            return;
        }
        // Added while no session is connected: level content every peer builds the same way (a
        // client adopts it when the server's create arrives), not a client-side spawn.
        let online = self.session().is_some_and(|s| s.bind().is_connected());
        let origin = if online {
            Origin::Local
        } else {
            Origin::Static
        };
        self.enrol(node, origin);
    }

    #[func]
    fn _on_node_removed(&mut self, node: Gd<Node>) {
        let iid = node.instance_id();
        let Some(entry) = self.entries.get(&iid) else {
            return;
        };
        if !queued_for_deletion(&node) {
            if let Some(e) = self.entries.get_mut(&iid) {
                e.dirty = true;
            }
            return;
        }
        // Being freed: announce now (the node is still valid for the callbacks).
        let id = entry.id;
        let scene = entry.scene.clone();
        let authoritative = id.is_some() && self.may_change(entry);
        self.forget(iid);
        if let (Some(id), true) = (id, authoritative) {
            self.despawn_callbacks(&node, &scene);
            self.destroyed.push(id);
        }
    }
}

impl NetManager {
    /// Makes `root` the managed root (`Net.manage`). Must be called before the manager enters the tree.
    pub fn set_managed_root(&mut self, root: Gd<Node>) {
        self.root_override = Some(root);
    }

    fn may_change(&self, e: &Entry) -> bool {
        self.online && (self.is_server || (e.origin == Origin::Local && e.owner == self.local_id))
    }

    /// Enrols what is below the root when the manager becomes ready.
    fn scan_existing(&mut self) {
        let root = self.get_managed_root();
        let mut stack: Vec<Gd<Node>> = root.get_children().iter_shared().collect();
        stack.reverse();
        while let Some(node) = stack.pop() {
            self.enrol(node.clone(), Origin::Static);
            let mut kids: Vec<Gd<Node>> = node.get_children().iter_shared().collect();
            kids.reverse();
            stack.extend(kids);
        }
    }

    /// The entry of the entity nearest above `node` (not `node` itself), or None at the root. Also
    /// reports whether a nested manager or an ignored node lies on the way.
    fn ancestor_entity(&self, node: &Gd<Node>) -> (Option<InstanceId>, bool) {
        let root = self.get_managed_root();
        let mut cur = node.get_parent();
        while let Some(n) = cur {
            if n == root {
                return (None, false);
            }
            if n.clone().try_cast::<NetManager>().is_ok() || n.is_in_group(LOCAL_GROUP) {
                return (None, true);
            }
            if self.entries.contains_key(&n.instance_id()) {
                return (Some(n.instance_id()), false);
            }
            cur = n.get_parent();
        }
        (None, true)
    }

    fn enrol(&mut self, node: Gd<Node>, origin: Origin) {
        if node.clone().try_cast::<NetManager>().is_ok()
            || node.clone().try_cast::<NetSession>().is_ok()
            || node.is_in_group(LOCAL_GROUP)
        {
            return;
        }
        let (ancestor, ignored) = self.ancestor_entity(&node);
        if ignored {
            return;
        }
        let scene_path = node.get_scene_file_path().to_string();
        let mut origin = origin;
        if let Some(a) = ancestor {
            let parent = &self.entries[&a];
            if !parent.scene.bare {
                let anc_node = Gd::<Node>::try_from_instance_id(a).ok();
                let rel = anc_node
                    .map(|an| an.get_path_to(&node).to_string())
                    .unwrap_or_default();
                if parent.scene.table.index_of(&rel).is_some() || scene_path.is_empty() {
                    return; // a part, or a helper node inside an entity
                }
            }
            // Inside a remote or static instance: the same node exists on the server.
            if parent.origin != Origin::Local {
                origin = Origin::Static;
            }
        }
        let split = self.split_scenes.clone();
        let scene = if scene_path.is_empty() {
            let class = node.get_class().to_string();
            if !self.warned_bare {
                self.warned_bare = true;
                godot_warn!(
                    "gdnet: {} is a bare {class} under a NetManager; it replicates as its class only. Make it a scene (.tscn) to replicate its children and script",
                    node.get_path()
                );
            }
            parts::scene_info(&class, &split)
        } else {
            parts::scene_info(&scene_path, &split)
        };
        let Some(scene) = scene else {
            godot_error!(
                "gdnet: cannot load the scene of {} ({scene_path}); it is not managed",
                node.get_path()
            );
            return;
        };
        let owner = owner_of(&node);
        let iid = node.instance_id();
        self.entries.insert(
            iid,
            Entry {
                id: None,
                scene,
                owner,
                origin,
                parent: None,
                dirty: false,
                owner_dirty: false,
                reported: false,
            },
        );
        self.order.push(iid);
        let mut n = node;
        n.set_meta(
            META_MANAGER,
            &(self.to_gd().instance_id().to_i64()).to_variant(),
        );
    }

    fn forget(&mut self, iid: InstanceId) {
        if let Some(e) = self.entries.remove(&iid) {
            if let Some(id) = e.id {
                self.by_id.remove(&id);
            }
            if let Ok(mut n) = Gd::<Node>::try_from_instance_id(iid) {
                if n.has_meta(META_MANAGER) {
                    n.remove_meta(META_MANAGER);
                }
            }
        }
    }

    fn entry_of_id(&self, id: NetId) -> Option<(&Entry, Gd<Node>)> {
        let iid = self.by_id.get(&id)?;
        let e = self.entries.get(iid)?;
        let n = Gd::<Node>::try_from_instance_id(*iid).ok()?;
        Some((e, n))
    }

    /// The parent reference of `node` as it hangs now, or None when its parent is not addressable (a
    /// helper node inside an entity, or an entity without an id yet).
    fn parent_ref(&self, node: &Gd<Node>, key: u64) -> Option<ParentRef> {
        let root = self.get_managed_root();
        let direct = node.get_parent()?;
        let mut cur = Some(direct.clone());
        while let Some(n) = cur {
            if n == root {
                return Some(ParentRef {
                    manager: key,
                    entity: MANAGER_ROOT,
                    part: 0,
                });
            }
            if let Some(e) = self.entries.get(&n.instance_id()) {
                let id = e.id?;
                let rel = n.get_path_to(&direct).to_string();
                let part = e.scene.table.index_of(&rel)?;
                return Some(ParentRef {
                    manager: key,
                    entity: id,
                    part,
                });
            }
            cur = n.get_parent();
        }
        None
    }

    /// The node a parent reference names.
    fn resolve_parent(&self, parent: &ParentRef) -> Option<Gd<Node>> {
        if parent.entity == MANAGER_ROOT {
            return Some(self.get_managed_root());
        }
        let (e, n) = self.entry_of_id(parent.entity)?;
        e.scene.part_node(&n, parent.part)
    }

    fn info(&self, id: NetId, owner: u32) -> VarDictionary {
        let mut d = VarDictionary::new();
        d.set("net_id", i64::from(id));
        d.set("owner", i64::from(owner));
        d.set("is_owner", owner == self.local_id);
        d.set("is_authority", self.is_server);
        d
    }

    /// Calls `_net_spawned(info)` on every part that defines it, then emits `entity_spawned` on the
    /// manager and the session. Holds the base guard.
    fn spawned_callbacks(
        &mut self,
        node: &Gd<Node>,
        scene: &SceneInfo,
        info: VarDictionary,
        session: &Gd<NetSession>,
    ) {
        let parts = scene.part_nodes(node);
        let mut session_obj = session.clone().upcast::<Node>();
        let _guard = self.base_mut();
        for mut p in parts {
            if p.has_method("_net_spawned") {
                p.call("_net_spawned", &[info.to_variant()]);
            }
        }
        drop(_guard);
        let v = node.to_variant();
        self.base_mut()
            .emit_signal("entity_spawned", std::slice::from_ref(&v));
        let _guard = self.base_mut();
        session_obj.emit_signal("entity_spawned", &[v]);
    }

    /// Calls `_net_despawning()` on every part that defines it and emits `entity_despawning`.
    fn despawn_callbacks(&mut self, node: &Gd<Node>, scene: &SceneInfo) {
        let parts = scene.part_nodes(node);
        let session = self.session();
        let _guard = self.base_mut();
        for mut p in parts {
            if p.has_method("_net_despawning") {
                p.call("_net_despawning", &[]);
            }
        }
        drop(_guard);
        let v = node.to_variant();
        self.base_mut()
            .emit_signal("entity_despawning", std::slice::from_ref(&v));
        if let Some(s) = session {
            let mut s = s.upcast::<Node>();
            let _guard = self.base_mut();
            s.emit_signal("entity_despawning", &[v]);
        }
    }

    /// The session serving this manager's branch, if one runs.
    fn session(&self) -> Option<Gd<NetSession>> {
        let api = self.base().get_multiplayer()?;
        if !api.has_meta(crate::session::META_SESSION) {
            return None;
        }
        let iid = api
            .get_meta(crate::session::META_SESSION)
            .try_to::<i64>()
            .ok()?;
        Gd::<NetSession>::try_from_instance_id(InstanceId::from_i64(iid)).ok()
    }

    /// Sets an entity's owner (server: announced at the next sync).
    pub fn set_entity_owner(&mut self, root: &Gd<Node>, peer: u32) -> bool {
        let Some(e) = self.entries.get_mut(&root.instance_id()) else {
            return false;
        };
        e.owner = peer;
        e.owner_dirty = e.id.is_some();
        set_net_entity(root, Some(peer), None);
        true
    }

    /// (owner, is_owner, is_authority) of the entity rooted at `root`.
    pub fn query(&self, root: &Gd<Node>) -> Option<(u32, bool, bool)> {
        let e = self.entries.get(&root.instance_id())?;
        if !self.online {
            return Some((e.owner, true, true));
        }
        Some((e.owner, e.owner == self.local_id, self.is_server))
    }

    /// The net id of the entity rooted at `root` (0 before it is spawned).
    pub fn net_id_of(&self, root: &Gd<Node>) -> u32 {
        self.entries
            .get(&root.instance_id())
            .and_then(|e| e.id)
            .unwrap_or(0)
    }

    /// One tick, driven by the session: apply remote actions, then announce local changes.
    pub fn sync(&mut self, session: Gd<NetSession>, key: u64, events: Vec<EntityEvent>) {
        let (generation, online, is_server, local_id, salt) = {
            let s = session.bind();
            (
                s.generation(),
                s.is_connected(),
                s.is_server(),
                s.unique_id() as u32,
                s.debug_part_table_salt() as u64,
            )
        };
        if generation != self.generation {
            // A new session: earlier ids mean nothing; remote replicas become static content.
            self.generation = generation;
            self.by_id.clear();
            for e in self.entries.values_mut() {
                e.id = None;
                e.parent = None;
                if e.origin == Origin::Remote {
                    e.origin = Origin::Static;
                }
            }
        }
        self.online = online;
        self.is_server = is_server;
        self.local_id = local_id;
        if !online {
            return;
        }
        for ev in events {
            if session.bind().is_closed() {
                return;
            }
            self.apply_event(&session, key, salt, ev);
        }
        self.announce(&session, key, salt);
    }

    fn apply_event(&mut self, session: &Gd<NetSession>, key: u64, salt: u64, ev: EntityEvent) {
        match ev {
            EntityEvent::Create {
                create,
                scene_path,
                from,
                proposal,
            } => {
                let refuse = |session: &Gd<NetSession>, id: NetId| {
                    let mut s = session.clone();
                    s.bind_mut().with_core(|c, now| c.refuse_create(id, now));
                };
                if proposal && self.spawn_policy != SpawnPolicy::OwnerAllowed {
                    godot_warn!(
                        "gdnet: peer {from} tried to spawn {scene_path} under {}, whose spawn_policy is server_only; refused",
                        self.base().get_path()
                    );
                    refuse(session, create.id);
                    return;
                }
                let Some(scene) = parts::scene_info(&scene_path, &self.split_scenes.clone()) else {
                    godot_error!(
                        "gdnet: cannot load {scene_path:?} to spawn entity {} ({}); every peer needs the scene at the same path",
                        create.id,
                        create.name
                    );
                    if proposal {
                        refuse(session, create.id);
                    }
                    return;
                };
                {
                    let mut s = session.clone();
                    s.bind_mut().with_core(|c, _| {
                        c.register_scene(&scene.path, scene.parts_hash.wrapping_add(salt))
                    });
                    if !s.bind().is_connected() {
                        return;
                    }
                }
                let Some(mut parent) = self.resolve_parent(&create.parent) else {
                    godot_error!(
                        "gdnet: the parent of entity {} ({}) was not found (entity {}, part {})",
                        create.id,
                        create.name,
                        create.parent.entity,
                        create.parent.part
                    );
                    if proposal {
                        refuse(session, create.id);
                    }
                    return;
                };
                // Adopt a static node of the same name and scene, else instantiate.
                let existing = parent.get_node_or_null(create.name.as_str()).filter(|n| {
                    self.entries.get(&n.instance_id()).is_some_and(|e| {
                        e.id.is_none() && e.origin == Origin::Static && e.scene.path == scene.path
                    })
                });
                let (node, adopted) = match existing {
                    Some(n) => (n, true),
                    None => {
                        let Some(mut n) = parts::instantiate(&scene) else {
                            godot_error!("gdnet: could not instantiate {}", scene.path);
                            if proposal {
                                refuse(session, create.id);
                            }
                            return;
                        };
                        n.set_name(create.name.as_str());
                        n.set_meta(META_REPLICA, &true.to_variant());
                        (n, false)
                    }
                };
                let iid = node.instance_id();
                if adopted {
                    if let Some(e) = self.entries.get_mut(&iid) {
                        e.id = Some(create.id);
                        e.owner = create.owner;
                        e.origin = Origin::Remote;
                        e.parent = Some(create.parent);
                    }
                } else {
                    self.entries.insert(
                        iid,
                        Entry {
                            id: Some(create.id),
                            scene: scene.clone(),
                            owner: create.owner,
                            origin: Origin::Remote,
                            parent: Some(create.parent),
                            dirty: false,
                            owner_dirty: false,
                            reported: false,
                        },
                    );
                    self.order.push(iid);
                    let mut n = node.clone();
                    n.set_meta(
                        META_MANAGER,
                        &(self.to_gd().instance_id().to_i64()).to_variant(),
                    );
                }
                self.by_id.insert(create.id, iid);
                set_net_entity(&node, Some(create.owner), Some(create.id));
                if !adopted {
                    let _guard = self.base_mut();
                    parent.add_child(&node);
                }
                if node.get_name().to_string() != create.name {
                    godot_error!(
                        "gdnet: entity {} arrived as {:?} but its parent already has a child named that; it was renamed {}",
                        create.id,
                        create.name,
                        node.get_name()
                    );
                }
                if proposal {
                    let mut s = session.clone();
                    s.bind_mut()
                        .with_core(|c, now| c.accept_create(create.id, now));
                }
                let info = self.info(create.id, create.owner);
                self.spawned_callbacks(&node, &scene, info, session);
                let _ = key;
            }
            EntityEvent::Destroy { record, .. } => {
                let Some(iid) = self.by_id.get(&record.id).copied() else {
                    return;
                };
                let scene = self.entries[&iid].scene.clone();
                self.forget(iid);
                if let Ok(node) = Gd::<Node>::try_from_instance_id(iid) {
                    self.despawn_callbacks(&node, &scene);
                    if node.is_queued_for_deletion() {
                        return;
                    }
                    let mut node = node;
                    let _guard = self.base_mut();
                    if let Some(mut p) = node.get_parent() {
                        p.remove_child(&node);
                    }
                    node.queue_free();
                }
            }
            EntityEvent::Reparent { id, parent, .. } => {
                let Some(iid) = self.by_id.get(&id).copied() else {
                    return;
                };
                let Some(mut new_parent) = self.resolve_parent(&parent) else {
                    godot_error!(
                        "gdnet: entity {id} was moved under entity {} part {}, which was not found",
                        parent.entity,
                        parent.part
                    );
                    return;
                };
                if let Some(e) = self.entries.get_mut(&iid) {
                    e.parent = Some(parent);
                }
                if let Ok(node) = Gd::<Node>::try_from_instance_id(iid) {
                    if node.get_parent().as_ref() == Some(&new_parent) {
                        return;
                    }
                    let mut marked = node.clone();
                    marked.set_meta(META_REPLICA, &true.to_variant());
                    let _guard = self.base_mut();
                    if let Some(mut old) = node.get_parent() {
                        old.remove_child(&node);
                    }
                    new_parent.add_child(&node);
                }
            }
            EntityEvent::SetOwner { id, owner, .. } => {
                let Some(iid) = self.by_id.get(&id).copied() else {
                    return;
                };
                if let Some(e) = self.entries.get_mut(&iid) {
                    e.owner = owner;
                }
                if let Ok(node) = Gd::<Node>::try_from_instance_id(iid) {
                    set_net_entity(&node, Some(owner), None);
                }
            }
        }
    }

    /// Announces local changes: destroys, moves, owner changes, then new entities in enrolment order.
    fn announce(&mut self, session: &Gd<NetSession>, key: u64, salt: u64) {
        let mut s = session.clone();
        for id in std::mem::take(&mut self.destroyed) {
            s.bind_mut().with_core(|c, now| c.destroy(id, now));
        }
        // Moved or removed entities.
        let dirty: Vec<InstanceId> = self
            .order
            .iter()
            .copied()
            .filter(|i| self.entries.get(i).is_some_and(|e| e.dirty))
            .collect();
        let root = self.get_managed_root();
        for iid in dirty {
            let node = Gd::<Node>::try_from_instance_id(iid).ok();
            let inside = node
                .as_ref()
                .is_some_and(|n| n.is_inside_tree() && root.is_ancestor_of(n));
            let Some(e) = self.entries.get_mut(&iid) else {
                continue;
            };
            e.dirty = false;
            let (id, may) = (
                e.id,
                self.online
                    && (self.is_server || (e.origin == Origin::Local && e.owner == self.local_id)),
            );
            if !inside {
                let scene = self.entries[&iid].scene.clone();
                self.forget(iid);
                if let (Some(id), true) = (id, may) {
                    if let Some(n) = &node {
                        self.despawn_callbacks(n, &scene);
                    }
                    s.bind_mut().with_core(|c, now| c.destroy(id, now));
                }
                continue;
            }
            if let (Some(id), true, Some(n)) = (id, may, node.as_ref()) {
                let now_parent = self.parent_ref(n, key);
                let before = self.entries.get(&iid).and_then(|e| e.parent);
                if let Some(p) = now_parent {
                    if Some(p) != before {
                        let ok = s
                            .bind_mut()
                            .with_core(|c, now| c.reparent(id, p, now))
                            .unwrap_or(false);
                        if ok {
                            if let Some(e) = self.entries.get_mut(&iid) {
                                e.parent = Some(p);
                            }
                        }
                    }
                }
            }
        }
        // Owner changes (server).
        if self.is_server {
            let changes: Vec<(InstanceId, NetId, u32)> = self
                .entries
                .iter()
                .filter(|(_, e)| e.owner_dirty)
                .filter_map(|(i, e)| e.id.map(|id| (*i, id, e.owner)))
                .collect();
            for (iid, id, owner) in changes {
                s.bind_mut().with_core(|c, now| c.set_owner(id, owner, now));
                if let Some(e) = self.entries.get_mut(&iid) {
                    e.owner_dirty = false;
                }
            }
        }
        // New entities, in enrolment order (parents first).
        self.order.retain(|i| self.entries.contains_key(i));
        let fresh: Vec<InstanceId> = self
            .order
            .iter()
            .copied()
            .filter(|i| self.entries.get(i).is_some_and(|e| e.id.is_none()))
            .collect();
        for iid in fresh {
            let Ok(node) = Gd::<Node>::try_from_instance_id(iid) else {
                self.forget(iid);
                continue;
            };
            let (origin, scene) = {
                let e = &self.entries[&iid];
                (e.origin, e.scene.clone())
            };
            if !self.is_server {
                match (origin, self.spawn_policy) {
                    (Origin::Local, SpawnPolicy::ServerOnly) => {
                        self.refuse(&node, session);
                        continue;
                    }
                    (Origin::Local, SpawnPolicy::OwnerAllowed) => {
                        if let Some(e) = self.entries.get_mut(&iid) {
                            e.owner = self.local_id;
                        }
                    }
                    // Static content waits for the server's create.
                    _ => continue,
                }
            }
            let Some(parent) = self.parent_ref(&node, key) else {
                if let Some(e) = self.entries.get_mut(&iid) {
                    if !e.reported {
                        e.reported = true;
                        godot_error!(
                            "gdnet: {} cannot be spawned: its parent is neither the manager, an entity, nor a part of one",
                            node.get_path()
                        );
                    }
                }
                continue;
            };
            let owner = self.entries[&iid].owner;
            let name = node.get_name().to_string();
            let result = s.bind_mut().with_core(|c, now| {
                c.register_scene(&scene.path, scene.parts_hash.wrapping_add(salt));
                c.spawn(
                    &scene.path,
                    Spawn::new(parent, &name, owner, scene.part_count(), Vec::new()),
                    now,
                )
            });
            match result {
                Some(Ok(id)) => {
                    if let Some(e) = self.entries.get_mut(&iid) {
                        e.id = Some(id);
                        e.parent = Some(parent);
                    }
                    self.by_id.insert(id, iid);
                    set_net_entity(&node, Some(owner), Some(id));
                    let info = self.info(id, owner);
                    self.spawned_callbacks(&node, &scene, info, session);
                }
                Some(Err(reason)) => {
                    godot_error!("gdnet: could not spawn {}: {reason}", node.get_path());
                    self.forget(iid);
                }
                None => {}
            }
        }
    }

    fn refuse(&mut self, node: &Gd<Node>, session: &Gd<NetSession>) {
        self.forget(node.instance_id());
        godot_error!(
            "gdnet: {} was refused: the NetManager {} has spawn_policy server_only, so only the server spawns entities. Spawn it on the server (or with Net.spawn_for), or set spawn_policy to owner_allowed",
            node.get_path(),
            self.base().get_path()
        );
        let v = node.to_variant();
        self.base_mut()
            .emit_signal("spawn_refused", std::slice::from_ref(&v));
        let mut session_obj = session.clone().upcast::<Node>();
        let mut n = node.clone();
        let _guard = self.base_mut();
        session_obj.emit_signal("spawn_refused", &[v]);
        if is_instance_valid(&n) && !n.is_queued_for_deletion() {
            if let Some(mut p) = n.get_parent() {
                p.remove_child(&n);
            }
            n.queue_free();
        }
    }
}

fn is_instance_valid(n: &Gd<Node>) -> bool {
    n.is_instance_valid()
}

fn scene_tree() -> Option<Gd<SceneTree>> {
    Engine::singleton()
        .get_main_loop()
        .and_then(|m| m.try_cast::<SceneTree>().ok())
}

/// True when `node` or one of its ancestors was queued for deletion.
fn queued_for_deletion(node: &Gd<Node>) -> bool {
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if n.is_queued_for_deletion() {
            return true;
        }
        cur = n.get_parent();
    }
    false
}

/// The owner an entity starts with: its `NetEntity` child's `owner_peer`, else the `_net_owner` meta, else
/// the server.
pub fn owner_of(node: &Gd<Node>) -> u32 {
    if let Some(ne) = net_entity_of(node) {
        return ne.bind().get_owner_peer().max(0) as u32;
    }
    if node.has_meta(META_OWNER) {
        return node
            .get_meta(META_OWNER)
            .try_to::<i64>()
            .unwrap_or(1)
            .max(0) as u32;
    }
    1
}

pub fn net_entity_of(node: &Gd<Node>) -> Option<Gd<NetEntity>> {
    node.get_children()
        .iter_shared()
        .find_map(|c| c.try_cast::<NetEntity>().ok())
}

fn set_net_entity(node: &Gd<Node>, owner: Option<u32>, id: Option<NetId>) {
    if let Some(mut ne) = net_entity_of(node) {
        let mut b = ne.bind_mut();
        if let Some(o) = owner {
            b.set_owner_peer(i64::from(o));
        }
        if let Some(i) = id {
            b.set_net_id(i64::from(i));
        }
    }
}

/// The entity root `node` belongs to (itself or an ancestor carrying the manager meta), and its manager.
pub fn entity_root_of(node: &Gd<Node>) -> Option<(Gd<Node>, Gd<NetManager>)> {
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if n.has_meta(META_MANAGER) {
            let iid = n.get_meta(META_MANAGER).try_to::<i64>().ok()?;
            let m = Gd::<NetManager>::try_from_instance_id(InstanceId::from_i64(iid)).ok()?;
            return Some((n, m));
        }
        cur = n.get_parent();
    }
    None
}

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
//! `queue_free`, the `_net_*` calls, signal emits, property reads and writes) holds `self.base_mut()`;
//! core access goes through short `session.bind_mut().with_core(...)` borrows.
//!
//! State (WP3): each entity gets its scene's property table (infer.rs) when it is spawned or applied;
//! the server reads its parts' properties for the `Create` and then every tick in `record()` (called by
//! the session's post-tick hook); a client applies a `Create`'s initial state after `add_child` and
//! before `_net_spawned`, and snapshot values in `apply_states()`. RigidBody parts of a client's replica
//! are frozen once.

use crate::apply;
use crate::call::{self as net_call, NetCall, NetCallInfo};
use crate::entity_node::{NetEntity, Simulate};
use crate::infer::{self, RuntimeProps};
use crate::input::{self as net_input, ActionTable, NetInput};
use crate::methods::{self, MethodTable};
use crate::parts::{self, SceneInfo};
use crate::profiles::{self, NetProfiles, Profiles};
use crate::session::NetSession;
use crate::tick::{self, Gate};
use godot::classes::{Engine, INode, Node, SceneTree};
use godot::prelude::*;
use netcore::call::{method_hash, CallIn, OutCall, Verb};
use netcore::input::{name_hash, InputFrame};
use netcore::session::EntityEvent;
use netcore::{NetId, ParentRef, Policy, Spawn, StateUpdate, Value, MANAGER_ROOT};
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
/// Meta on an entity root: its priority set by `Net.configure(node, {"priority": p})`.
pub const META_PRIORITY: &str = "_net_priority";

/// One recorded part: (entity, part index, (field index, value)...).
pub type Recorded = (NetId, u16, Vec<(u16, Value)>);

type PartNodes = Rc<Vec<Option<Gd<Node>>>>;

/// WP5: a field showing an owned offset: its node, its accessor, the simulated value to restore.
type Shown = (Gd<Node>, crate::infer::RtField, Value);

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
    /// WP3: the scene's property table with cached accessors, and the part nodes.
    props: Option<Rc<RuntimeProps>>,
    nodes: PartNodes,
    /// Poll the cold properties at the next record (`mark_cold_dirty`).
    cold_force: bool,
    /// WP4: the entity's input actions, its `NetInput`, who simulates, prediction.
    actions: Rc<ActionTable>,
    input: Option<Gd<NetInput>>,
    gate: Option<Gate>,
    /// Read from the `NetEntity` when the gate is created.
    everywhere: bool,
    policy: Policy,
    /// `NetEntity.input_properties`: (part node, property, wire name hash).
    input_props: Vec<(Gd<Node>, StringName, u32)>,
    /// Tracked by netcore's prediction (this peer owns it as a client and predicts it).
    predicted: Option<Policy>,
    /// The scripts were checked for global `Input` use (once, when a client owns it).
    input_checked: bool,
    /// WP6: the scene's method tables (methods.rs), once derived.
    methods: Option<Rc<MethodTable>>,
}

impl Entry {
    fn new(scene: Rc<SceneInfo>, owner: u32, origin: Origin) -> Self {
        Self {
            id: None,
            scene,
            owner,
            origin,
            parent: None,
            dirty: false,
            owner_dirty: false,
            reported: false,
            props: None,
            nodes: Rc::new(Vec::new()),
            cold_force: false,
            actions: Rc::new(ActionTable::default()),
            input: None,
            gate: None,
            everywhere: false,
            policy: Policy::Check,
            input_props: Vec::new(),
            predicted: None,
            input_checked: false,
            methods: None,
        }
    }
}

/// What the session tells its managers each tick (WP4).
#[derive(Clone, Copy, Debug)]
pub struct TickCtx {
    pub is_server: bool,
    pub local_id: u32,
    /// The server tick (server) or the predicted tick (client).
    pub tick: u64,
    /// No input delay: the frame read this tick is the one sampled now.
    pub delay0: bool,
    pub delta: f64,
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
    /// Profiles laid over the global ones for this manager's entities (3.1).
    #[export]
    #[var(get = get_profiles, set = set_profiles)]
    profiles: Option<Gd<NetProfiles>>,
    /// The input actions owned entities sample and send (WP4, D15). Empty: the project setting
    /// `gdnet/input/actions`, else every InputMap action except `ui_*`. A `NetEntity`'s
    /// `input_actions` narrows it per entity.
    #[export]
    input_actions: PackedStringArray,
    actions_cache: HashMap<String, Rc<ActionTable>>,
    profiles_cache: Option<(usize, Rc<Profiles>)>,
    runtime: HashMap<usize, Rc<RuntimeProps>>,
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
    /// WP5: owned fields showing an offset this render frame: (node, field, the simulated value to put
    /// back before the next tick).
    owned_shown: HashMap<(NetId, u16, u16), Shown>,
    /// WP6: calls on entities that have no net id yet (spawned this tick, or a client's static content
    /// before the server's create), dispatched once they have one.
    queued_calls: Vec<Queued>,
}

/// WP6: a call checked at the call site, on its way out.
pub struct CallReq {
    pub verb: Verb,
    pub target: u32,
    pub method: StringName,
    pub args: Vec<Variant>,
    pub bytes: Vec<u8>,
    pub unreliable: bool,
    pub call: Gd<NetCall>,
}

/// WP6: a call waiting for its entity's net id.
struct Queued {
    iid: InstanceId,
    part: u16,
    hash: u32,
    wants_info: bool,
    req: CallReq,
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
    fn get_profiles(&self) -> Option<Gd<NetProfiles>> {
        self.profiles.clone()
    }

    #[func]
    fn set_profiles(&mut self, p: Option<Gd<NetProfiles>>) {
        self.profiles = p;
        self.profiles_cache = None;
    }

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
        self.entries.insert(iid, Entry::new(scene, owner, origin));
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
                self.owned_shown.retain(|k, _| k.0 != id);
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
        self.flush_queued(&session);
    }

    fn apply_event(&mut self, session: &Gd<NetSession>, key: u64, salt: u64, ev: EntityEvent) {
        match ev {
            EntityEvent::Call(ci) => self.on_call(session, ci),
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
                    let mut entry = Entry::new(scene.clone(), create.owner, Origin::Remote);
                    entry.id = Some(create.id);
                    entry.parent = Some(create.parent);
                    self.entries.insert(iid, entry);
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
                // WP3: the scene's property table, then the initial state, before _net_spawned.
                let rt = self.props_for(&node, &scene, session);
                if !session.bind().is_connected() {
                    return; // the property tables differ: the session ended naming them
                }
                let nodes = part_node_list(&scene, &node);
                if let Some(e) = self.entries.get_mut(&iid) {
                    e.props = rt.clone();
                    e.nodes = nodes.clone();
                }
                if let Some(rt) = &rt {
                    let mut s = session.clone();
                    let values = s
                        .bind_mut()
                        .with_core(|c, _| c.decode_state(create.scene_hash, &create.initial_state))
                        .flatten()
                        .unwrap_or_default();
                    // WP5: a replica's interpolated fields start their sample buffers from it.
                    if !self.is_server {
                        let id = create.id;
                        s.bind_mut().with_core(|c, _| {
                            for (p, f, v) in &values {
                                c.interp_seed(id, *p, *f, v.clone());
                            }
                        });
                    }
                    let writes = state_writes(rt, &nodes, values);
                    let _guard = self.base_mut();
                    for (n, f, v) in writes {
                        apply::write(&n, &f, &v);
                    }
                }
                if !self.is_server {
                    freeze_bodies(&nodes);
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
                // WP5 (and WP7's hook): a new authority starts with fresh interpolation buffers.
                {
                    let mut s = session.clone();
                    s.bind_mut().with_core(|c, _| c.interp_reset(id));
                }
                self.owned_shown.retain(|k, _| k.0 != id);
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
            // WP3: the property table and every part's values travel with the create.
            s.bind_mut().with_core(|c, _| {
                c.register_scene(&scene.path, scene.parts_hash.wrapping_add(salt));
            });
            let rt = self.props_for(&node, &scene, session);
            if !s.bind().is_connected() {
                return;
            }
            let nodes = part_node_list(&scene, &node);
            let values = rt.as_ref().map(|rt| {
                let _guard = self.base_mut();
                read_all(rt, &nodes)
            });
            let priority = priority_of(&node);
            let result = s.bind_mut().with_core(|c, now| {
                let spec = Spawn::new(parent, &name, owner, scene.part_count(), Vec::new());
                let spec = match values {
                    Some(v) => spec.with_values(v),
                    None => spec,
                };
                let r = c.spawn(&scene.path, spec, now);
                if let Ok(id) = r {
                    c.set_entity_priority(id, priority);
                }
                r
            });
            match result {
                Some(Ok(id)) => {
                    if let Some(e) = self.entries.get_mut(&iid) {
                        e.id = Some(id);
                        e.parent = Some(parent);
                        e.props = rt.clone();
                        e.nodes = nodes.clone();
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

    /// The global profiles with this manager's override on top.
    fn effective_profiles(&mut self) -> Rc<Profiles> {
        let global = profiles::global_profiles();
        let key = Rc::as_ptr(&global) as usize;
        if let Some((k, p)) = &self.profiles_cache {
            if *k == key {
                return p.clone();
            }
        }
        let p = match &self.profiles {
            Some(over) => {
                let mut merged = (*global).clone();
                merged.merge(&over.bind().parsed());
                Rc::new(merged)
            }
            None => global,
        };
        self.profiles_cache = Some((key, p.clone()));
        p
    }

    /// The property table of `node`'s scene (derived once per scene), registered with the session so
    /// netcore can pack it and compare it with the peers'.
    fn props_for(
        &mut self,
        node: &Gd<Node>,
        scene: &SceneInfo,
        session: &Gd<NetSession>,
    ) -> Option<Rc<RuntimeProps>> {
        let profiles = self.effective_profiles();
        let perturb = session.bind().debug_perturb_property();
        let inferred = infer::derive(
            node,
            scene,
            &profiles,
            &perturb,
            crate::session::tick_rate(),
        );
        let key = Rc::as_ptr(&inferred) as usize;
        let rt = self
            .runtime
            .entry(key)
            .or_insert_with(|| Rc::new(RuntimeProps::new(inferred.clone())))
            .clone();
        // WP4: the input actions are part of the table, so both peers agree on their indices.
        let actions = self.actions_for(node, &scene.path);
        let mut table = inferred.table.clone();
        table.inputs = actions.strings.clone();
        // WP6: the per-part method table hashes join the table (a difference names the part).
        let mt = {
            let nodes = part_node_list(scene, node);
            methods::table_for(scene, &nodes)
        };
        let extra = session.bind().debug_extra_method();
        table.methods = mt.hashes(&inferred.part_names, &extra);
        let lists = mt.lists();
        let mut s = session.clone();
        s.bind_mut().with_core(|c, _| {
            let hash = netcore::entity::scene_hash(&scene.path);
            if c.props(hash) != Some(&table) {
                c.register_props(&scene.path, table, inferred.part_names.clone());
            }
            c.register_methods(hash, lists);
        });
        if let Some(e) = self.entries.get_mut(&node.instance_id()) {
            e.actions = actions;
            e.methods = Some(mt);
        }
        Some(rt)
    }

    /// The input actions of an entity of `scene` (cached per scene).
    fn actions_for(&mut self, node: &Gd<Node>, scene: &str) -> Rc<ActionTable> {
        if let Some(a) = self.actions_cache.get(scene) {
            return a.clone();
        }
        let manager: Vec<String> = self
            .input_actions
            .as_slice()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let entity: Vec<String> = net_entity_of(node)
            .map(|ne| {
                ne.bind()
                    .get_input_actions()
                    .as_slice()
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let a = Rc::new(ActionTable::new(net_input::action_list(&manager, &entity)));
        self.actions_cache.insert(scene.to_string(), a.clone());
        a
    }

    /// 2.2 step 5 on the server: reads the hot properties of every spawned entity, and the cold ones
    /// when due (every `cold` ticks, staggered by id, or right after `mark_cold_dirty`).
    pub fn record(&mut self, tick: u64, cold: u32, out: &mut Vec<Recorded>) {
        if !self.online || !self.is_server {
            return;
        }
        let cold = u64::from(cold.max(1));
        let mut work: Vec<(NetId, Rc<RuntimeProps>, PartNodes, bool)> = Vec::new();
        for iid in &self.order {
            let Some(e) = self.entries.get_mut(iid) else {
                continue;
            };
            let (Some(id), Some(rt)) = (e.id, e.props.clone()) else {
                continue;
            };
            let due = e.cold_force || (tick + u64::from(id)).is_multiple_of(cold);
            e.cold_force = false;
            work.push((id, rt, e.nodes.clone(), due));
        }
        let _guard = self.base_mut();
        for (id, rt, nodes, cold_due) in work {
            for (p, node) in nodes.iter().enumerate() {
                let Some(node) = node else {
                    continue;
                };
                if !node.is_instance_valid() {
                    continue;
                }
                let hot = &rt.hot[p];
                let colds = if cold_due { rt.cold[p].as_slice() } else { &[] };
                if hot.is_empty() && colds.is_empty() {
                    continue;
                }
                let mut vals = Vec::with_capacity(hot.len() + colds.len());
                for &f in hot.iter().chain(colds) {
                    vals.push((f, apply::read(node, &rt.fields[p][usize::from(f)])));
                }
                out.push((id, p as u16, vals));
            }
        }
    }

    /// Applies snapshot values to replicas, snapping (WP5: netcore keeps a replica's `interpolate`
    /// fields for `apply_interp` instead). Entities this peer spawned itself keep their own state.
    pub fn apply_states(&mut self, updates: Vec<StateUpdate>) {
        let mut writes = Vec::new();
        for u in updates {
            let Some(e) = self.by_id.get(&u.id).and_then(|i| self.entries.get(i)) else {
                continue;
            };
            if e.origin == Origin::Local {
                continue;
            }
            let Some(rt) = e.props.as_ref() else {
                continue;
            };
            let Some(Some(node)) = e.nodes.get(usize::from(u.part)) else {
                continue;
            };
            let Some(f) = rt
                .fields
                .get(usize::from(u.part))
                .and_then(|p| p.get(usize::from(u.field)))
            else {
                continue;
            };
            writes.push((node.clone(), f.clone(), u.value));
        }
        let _guard = self.base_mut();
        for (n, f, v) in writes {
            if n.is_instance_valid() {
                apply::write(&n, &f, &v);
            }
        }
    }

    /// Polls the entity's cold properties at the next record (WP6: after a call ran on it, so a changed
    /// `@export` reaches the replicas on the next tick).
    pub fn mark_cold_dirty(&mut self, root: &Gd<Node>) {
        if let Some(e) = self.entries.get_mut(&root.instance_id()) {
            e.cold_force = true;
        }
    }

    /// `Net.configure(node, {"priority": p})` on an enrolled entity: applied at once on the server.
    pub fn set_priority(&mut self, root: &Gd<Node>, priority: f32) {
        let id = self.entries.get(&root.instance_id()).and_then(|e| e.id);
        if let (Some(id), Some(mut s)) = (id, self.session()) {
            s.bind_mut()
                .with_core(|c, _| c.set_entity_priority(id, priority));
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

/// WP4: who simulates, input, prediction and replay (docs/PLAN_NETCODE.md 2.1, 2.2 steps 2-4, 2.5).
/// The session calls these once per tick in its step (priority -1000), after the entity actions and
/// received state were applied, in this order: `gate_and_track`, (a rollback: `restore` then
/// `replay_tick` per replayed tick), `prepare_inputs`, `net_ticks`; and `record_owned` from its
/// post-tick hook. Calls out to the engine hold the base guard; core access goes through short
/// `with_core` borrows.
impl NetManager {
    /// Applies the who-simulates rule (D13) to every spawned entity, starts or stops netcore's
    /// prediction for entities this client owns, creates their `NetInput`s and runs the global-`Input`
    /// check once per client-owned entity.
    pub fn gate_and_track(&mut self, session: &Gd<NetSession>) {
        if !self.online {
            return;
        }
        let (is_server, local) = (self.is_server, self.local_id);
        let order = self.order.clone();
        for iid in order {
            let Some(e) = self.entries.get_mut(&iid) else {
                continue;
            };
            let (Some(id), Some(_)) = (e.id, e.props.as_ref()) else {
                continue;
            };
            let nodes = e.nodes.clone();
            if e.gate.is_none() {
                let root = Gd::<Node>::try_from_instance_id(iid).ok();
                let ne = root.as_ref().and_then(net_entity_of);
                e.everywhere = ne
                    .as_ref()
                    .is_some_and(|n| n.bind().get_simulate() == Simulate::Everywhere);
                e.policy = ne
                    .as_ref()
                    .map_or(Policy::Check, |n| n.bind().get_predict().policy());
                e.input_props = match (&root, &ne) {
                    (Some(r), Some(n)) => input_props_of(r, &n.bind().get_input_properties()),
                    _ => Vec::new(),
                };
                e.gate = Some(Gate::new(&nodes));
            }
            let owned = e.owner == local;
            let simulate = if is_server || e.origin == Origin::Local {
                true
            } else if owned {
                e.policy != Policy::Disabled
            } else {
                e.everywhere
            };
            let want =
                (!is_server && owned && e.origin != Origin::Local && e.policy != Policy::Disabled)
                    .then_some(e.policy);
            let track = (want != e.predicted).then_some(want);
            e.predicted = want;
            let check = !e.input_checked && e.owner != netcore::SERVER && (is_server || owned);
            if check {
                e.input_checked = true;
            }
            if e.input.is_none() {
                let mut inp = NetInput::new_live();
                inp.bind_mut().set_actions(e.actions.clone());
                e.input = Some(inp);
            }
            let rt = e.props.clone();
            let gate = e.gate.as_mut().map(|g| (g.simulating != Some(simulate), g));
            if let Some((true, g)) = gate {
                let mut g2 = std::mem::take(g);
                {
                    let _guard = self.base_mut();
                    g2.apply(&nodes, simulate);
                }
                if let Some(e) = self.entries.get_mut(&iid) {
                    e.gate = Some(g2);
                }
            }
            if let Some(want) = track {
                let mut s = session.clone();
                // WP5: replica buffers or owned offsets of the old role do not carry over.
                s.bind_mut().with_core(|c, _| c.interp_reset(id));
                match want {
                    Some(policy) => {
                        let initial = {
                            let _guard = self.base_mut();
                            rt.as_ref()
                                .map(|rt| read_hot_indexed(rt, &nodes))
                                .unwrap_or_default()
                        };
                        s.bind_mut()
                            .with_core(|c, _| c.track_prediction(id, policy, &initial));
                    }
                    None => {
                        s.bind_mut().with_core(|c, _| c.untrack_prediction(id));
                    }
                }
            }
            if check {
                let _guard = self.base_mut();
                tick::check_global_input(&nodes);
            }
        }
    }

    /// 2.2 step 2: sets each entity's `NetInput` for this tick. The server uses the received frame of a
    /// client-owned entity (and writes its `input_properties`); a client samples the real `Input` for
    /// the entities it owns and runs `_net_input()`; entities the server owns read the real `Input`.
    pub fn prepare_inputs(&mut self, session: &Gd<NetSession>, ctx: &TickCtx) {
        if !self.online {
            return;
        }
        let order = self.order.clone();
        for iid in order {
            let Some(e) = self.entries.get(&iid) else {
                continue;
            };
            let (Some(id), Some(mut inp)) = (e.id, e.input.clone()) else {
                continue;
            };
            let owned = e.owner == ctx.local_id;
            let tick = ctx.tick as i64;
            let nodes = e.nodes.clone();
            let props = e.input_props.clone();
            if e.owner == netcore::SERVER || (owned && e.origin == Origin::Local) {
                inp.bind_mut().set_live(tick);
                if owned {
                    self.call_net_input(&nodes);
                }
            } else if ctx.is_server {
                let mut s = session.clone();
                let taken = s
                    .bind_mut()
                    .with_core(|c, _| c.server_input(id).cloned())
                    .flatten()
                    .unwrap_or_default();
                {
                    let _guard = self.base_mut();
                    write_input_props(&props, &taken.frame);
                }
                inp.bind_mut()
                    .set_frames(tick, taken.frame, taken.prev, None, false);
            } else if owned {
                let actions = e.actions.clone();
                let sampled = net_input::sample(&actions);
                let mut s = session.clone();
                let (stored, prev) = s
                    .bind_mut()
                    .with_core(|c, _| {
                        (
                            c.owner_frame(id, ctx.tick).cloned(),
                            ctx.tick
                                .checked_sub(1)
                                .and_then(|t| c.owner_frame(id, t).cloned()),
                        )
                    })
                    .unwrap_or_default();
                let current = if ctx.delay0 {
                    sampled.clone()
                } else {
                    stored.unwrap_or_default()
                };
                inp.bind_mut().set_frames(
                    tick,
                    current,
                    prev.unwrap_or_default(),
                    Some(sampled),
                    ctx.delay0,
                );
                self.call_net_input(&nodes);
                if !props.is_empty() {
                    let values: Vec<(u32, Value)> = {
                        let _guard = self.base_mut();
                        props
                            .iter()
                            .filter(|(n, _, _)| n.is_instance_valid())
                            .filter_map(|(n, p, h)| {
                                net_input::to_input_value(&n.get(p)).map(|v| (*h, v))
                            })
                            .collect()
                    };
                    let mut b = inp.bind_mut();
                    for (h, v) in values {
                        b.write_custom(h, v);
                    }
                }
                if !ctx.delay0 {
                    // input_properties of the applied frame (sampled `input_delay` ticks ago).
                    let current = inp.bind().current().clone();
                    let _guard = self.base_mut();
                    write_input_props(&props, &current);
                }
            } else {
                inp.bind_mut().set_frames(
                    tick,
                    InputFrame::default(),
                    InputFrame::default(),
                    None,
                    false,
                );
            }
        }
    }

    fn call_net_input(&mut self, nodes: &PartNodes) {
        let _guard = self.base_mut();
        for n in nodes.iter().flatten() {
            if n.is_instance_valid() && n.has_method("_net_input") {
                let mut n = n.clone();
                n.call("_net_input", &[]);
            }
        }
    }

    /// 2.2 step 4 for parts with `_net_tick`: called instead of their `_physics_process` on the peers
    /// that simulate the entity, in tree order.
    pub fn net_ticks(&mut self, ctx: &TickCtx) {
        if !self.online {
            return;
        }
        let mut work: Vec<Gd<Node>> = Vec::new();
        for iid in &self.order {
            let Some(e) = self.entries.get(iid) else {
                continue;
            };
            let Some(g) = e.gate.as_ref() else { continue };
            if g.simulating != Some(true) {
                continue;
            }
            for (i, n) in e.nodes.iter().enumerate() {
                if g.net_tick.get(i).copied().unwrap_or(false) {
                    if let Some(n) = n {
                        work.push(n.clone());
                    }
                }
            }
        }
        if work.is_empty() {
            return;
        }
        let args = [
            ctx.delta.to_variant(),
            (ctx.tick as i64).to_variant(),
            true.to_variant(),
        ];
        let _guard = self.base_mut();
        for mut n in work {
            if n.is_instance_valid() {
                n.call("_net_tick", &args);
            }
        }
    }

    /// Restores the hot state of the entities of `states` that this manager holds; returns their ids.
    pub fn restore(&mut self, states: &[(NetId, netcore::predict::EntityState)]) -> Vec<NetId> {
        let mut ids = Vec::new();
        let mut writes = Vec::new();
        for (id, state) in states {
            let Some(e) = self.by_id.get(id).and_then(|i| self.entries.get(i)) else {
                continue;
            };
            let Some(rt) = e.props.as_ref() else {
                continue;
            };
            writes.extend(state_writes(rt, &e.nodes, state.clone()));
            ids.push(*id);
        }
        let _guard = self.base_mut();
        for (n, f, v) in writes {
            if n.is_instance_valid() {
                apply::write(&n, &f, &v);
            }
        }
        ids
    }

    /// One replayed tick for `ids`: sets each entity's input to its frame for `tick`, calls
    /// `_net_tick(delta, tick, false)` or `_physics_process(delta)` on its simulating parts in tree
    /// order, and returns the hot state after it (for the history). `Net.is_fresh()` is false around it.
    pub fn replay_tick(
        &mut self,
        session: &Gd<NetSession>,
        ids: &[NetId],
        tick: u64,
        delta: f64,
    ) -> Vec<(NetId, Vec<Value>)> {
        let mut out = Vec::new();
        for id in ids {
            let Some(e) = self.by_id.get(id).and_then(|i| self.entries.get(i)) else {
                continue;
            };
            let (Some(rt), Some(mut inp), Some(g)) =
                (e.props.clone(), e.input.clone(), e.gate.clone())
            else {
                continue;
            };
            let nodes = e.nodes.clone();
            let props = e.input_props.clone();
            let mut s = session.clone();
            let (cur, prev) = s
                .bind_mut()
                .with_core(|c, _| {
                    (
                        c.owner_frame(*id, tick).cloned(),
                        tick.checked_sub(1)
                            .and_then(|t| c.owner_frame(*id, t).cloned()),
                    )
                })
                .unwrap_or_default();
            let cur = cur.unwrap_or_default();
            inp.bind_mut().set_frames(
                tick as i64,
                cur.clone(),
                prev.unwrap_or_default(),
                None,
                false,
            );
            let _guard = self.base_mut();
            write_input_props(&props, &cur);
            for (i, n) in nodes.iter().enumerate() {
                let Some(n) = n else { continue };
                if !n.is_instance_valid() {
                    continue;
                }
                let mut n = n.clone();
                if g.net_tick.get(i).copied().unwrap_or(false) {
                    n.call(
                        "_net_tick",
                        &[
                            delta.to_variant(),
                            (tick as i64).to_variant(),
                            false.to_variant(),
                        ],
                    );
                } else if n.is_physics_processing() && n.has_method("_physics_process") {
                    n.call("_physics_process", &[delta.to_variant()]);
                }
            }
            out.push((*id, read_hot(&rt, &nodes)));
        }
        out
    }

    /// Emits `rolled_back(from_tick, to_tick)` on the entities' `NetEntity` children and the session.
    pub fn emit_rolled_back(
        &mut self,
        session: &Gd<NetSession>,
        ids: &[NetId],
        from_tick: u64,
        to_tick: u64,
    ) {
        let nodes: Vec<Gd<Node>> = ids
            .iter()
            .filter_map(|id| self.by_id.get(id))
            .filter_map(|i| Gd::<Node>::try_from_instance_id(*i).ok())
            .collect();
        let (from, to) = (
            (from_tick as i64).to_variant(),
            (to_tick as i64).to_variant(),
        );
        let mut sobj = session.clone().upcast::<Node>();
        let _guard = self.base_mut();
        for n in nodes {
            if let Some(mut ne) = net_entity_of(&n) {
                ne.emit_signal("rolled_back", &[from.clone(), to.clone()]);
            }
            sobj.emit_signal("rolled_back", &[n.to_variant(), from.clone(), to.clone()]);
        }
    }

    /// 2.2 step 5 on an owner (a client): the hot state of every entity it predicts, and the frame it
    /// sampled for every entity it owns (to send).
    pub fn record_owned(
        &mut self,
        hist: &mut Vec<(NetId, Vec<Value>)>,
        frames: &mut Vec<(NetId, InputFrame)>,
    ) {
        if !self.online || self.is_server {
            return;
        }
        let mut work = Vec::new();
        for iid in &self.order {
            let Some(e) = self.entries.get(iid) else {
                continue;
            };
            let Some(id) = e.id else { continue };
            if e.owner != self.local_id || e.origin == Origin::Local {
                continue;
            }
            work.push((
                id,
                e.predicted.is_some(),
                e.props.clone(),
                e.nodes.clone(),
                e.input.clone(),
            ));
        }
        let _guard = self.base_mut();
        for (id, predicted, rt, nodes, inp) in work {
            if predicted {
                if let Some(rt) = rt {
                    hist.push((id, read_hot(&rt, &nodes)));
                }
            }
            if let Some(mut inp) = inp {
                if let Some(f) = inp.bind_mut().take_sample() {
                    frames.push((id, f.canonical()));
                }
            }
        }
    }

    /// The entities this peer predicts (netcore tracks them).
    pub fn predicted_ids(&self) -> Vec<NetId> {
        self.order
            .iter()
            .filter_map(|i| self.entries.get(i))
            .filter(|e| e.predicted.is_some())
            .filter_map(|e| e.id)
            .collect()
    }

    /// The `NetInput` of the entity rooted at `root` (None before it is spawned).
    pub fn input_of(&self, root: &Gd<Node>) -> Option<Gd<NetInput>> {
        self.entries.get(&root.instance_id())?.input.clone()
    }
}

/// WP5: interpolation and owned offsets (docs/PLAN_NETCODE.md 2.2 step 7, 2.5; see interp.rs). The
/// session calls `apply_interp` and `show_owned` from its `_process` and `restore_owned` at the start of
/// its physics step; `replay::rollback` calls `interp_hot` before and after a replay.
impl NetManager {
    /// The node and field of (entity, part, field) for a remote entity.
    fn remote_field(
        &self,
        id: NetId,
        part: u16,
        field: u16,
    ) -> Option<(Gd<Node>, crate::infer::RtField)> {
        let e = self.by_id.get(&id).and_then(|i| self.entries.get(i))?;
        if e.origin == Origin::Local {
            return None;
        }
        let rt = e.props.as_ref()?;
        let node = e.nodes.get(usize::from(part))?.clone()?;
        let f = rt
            .fields
            .get(usize::from(part))?
            .get(usize::from(field))?
            .clone();
        Some((node, f))
    }

    /// Writes the values netcore sampled for this manager's replicas this render frame.
    pub fn apply_interp(&mut self, values: Vec<((NetId, u16, u16), Value)>) {
        let writes: Vec<_> = values
            .into_iter()
            .filter_map(|((id, p, f), v)| self.remote_field(id, p, f).map(|(n, rf)| (n, rf, v)))
            .collect();
        let _guard = self.base_mut();
        for (n, f, v) in writes {
            if n.is_instance_valid() {
                apply::write(&n, &f, &v);
            }
        }
    }

    /// Shows the owned offsets of `keys` (this manager's entities): `property = simulated + offset`,
    /// remembering the simulated value; fields whose offset ended get their simulated value back.
    pub fn show_owned(&mut self, session: &Gd<NetSession>, keys: &[(NetId, u16, u16)]) {
        let gone: Vec<_> = self
            .owned_shown
            .keys()
            .filter(|k| !keys.contains(k))
            .copied()
            .collect();
        let restores: Vec<_> = gone
            .into_iter()
            .filter_map(|k| self.owned_shown.remove(&k))
            .collect();
        let mut known = Vec::new();
        let mut unread = Vec::new();
        for k in keys {
            match self.owned_shown.get(k) {
                Some((n, f, sim)) => known.push((*k, n.clone(), f.clone(), sim.clone())),
                None => {
                    if let Some((n, f)) = self.remote_field(k.0, k.1, k.2) {
                        unread.push((*k, n, f));
                    }
                }
            }
        }
        {
            let _guard = self.base_mut();
            for (n, f, sim) in &restores {
                if n.is_instance_valid() {
                    apply::write(n, f, sim);
                }
            }
            for (k, n, f) in unread {
                if n.is_instance_valid() {
                    let sim = apply::read(&n, &f);
                    known.push((k, n, f, sim));
                }
            }
        }
        let mut s = session.clone();
        let shown: Vec<Option<Value>> = s
            .bind_mut()
            .with_core(|c, _| {
                known
                    .iter()
                    .map(|(k, _, _, sim)| c.apply_owned_offset(k.0, k.1, k.2, sim))
                    .collect()
            })
            .unwrap_or_default();
        let mut kept = Vec::new();
        {
            let _guard = self.base_mut();
            for ((k, n, f, sim), v) in known.into_iter().zip(shown) {
                let Some(v) = v else { continue };
                if n.is_instance_valid() {
                    apply::write(&n, &f, &v);
                    kept.push((k, (n, f, sim)));
                }
            }
        }
        self.owned_shown.extend(kept);
    }

    /// Puts the simulated values back (before the next physics tick).
    pub fn restore_owned(&mut self) {
        let shown: Vec<_> = self.owned_shown.drain().map(|(_, v)| v).collect();
        let _guard = self.base_mut();
        for (n, f, sim) in shown {
            if n.is_instance_valid() {
                apply::write(&n, &f, &sim);
            }
        }
    }

    /// The current values of the interpolated hot fields of `ids` (for the rollback blend).
    pub fn interp_hot(&mut self, ids: &[NetId]) -> Vec<((NetId, u16, u16), Value)> {
        let mut work = Vec::new();
        for id in ids {
            let Some(e) = self.by_id.get(id).and_then(|i| self.entries.get(i)) else {
                continue;
            };
            let Some(rt) = e.props.clone() else { continue };
            work.push((*id, rt, e.nodes.clone()));
        }
        let mut out = Vec::new();
        let _guard = self.base_mut();
        for (id, rt, nodes) in work {
            for (p, hot) in rt.hot.iter().enumerate() {
                let Some(Some(n)) = nodes.get(p) else {
                    continue;
                };
                if !n.is_instance_valid() {
                    continue;
                }
                for &f in hot {
                    let field = &rt.fields[p][usize::from(f)];
                    if field.interp {
                        out.push(((id, p as u16, f), apply::read(n, field)));
                    }
                }
            }
        }
        out
    }
}

/// Every hot field of every part, in (part, field) order (netcore's `Prediction::hot_fields`).
fn read_hot(rt: &RuntimeProps, nodes: &[Option<Gd<Node>>]) -> Vec<Value> {
    let mut out = Vec::new();
    for (p, hot) in rt.hot.iter().enumerate() {
        for &f in hot {
            out.push(match nodes.get(p) {
                Some(Some(n)) if n.is_instance_valid() => {
                    apply::read(n, &rt.fields[p][usize::from(f)])
                }
                _ => Value::Nil,
            });
        }
    }
    out
}

fn read_hot_indexed(rt: &RuntimeProps, nodes: &[Option<Gd<Node>>]) -> Vec<(u16, u16, Value)> {
    let mut out = Vec::new();
    for (p, hot) in rt.hot.iter().enumerate() {
        for &f in hot {
            if let Some(Some(n)) = nodes.get(p) {
                out.push((p as u16, f, apply::read(n, &rt.fields[p][usize::from(f)])));
            }
        }
    }
    out
}

/// `NetEntity.input_properties` resolved against `root`: (node, property, wire hash of the path).
fn input_props_of(root: &Gd<Node>, paths: &Array<NodePath>) -> Vec<(Gd<Node>, StringName, u32)> {
    let mut out = Vec::new();
    for p in paths.iter_shared() {
        let text = p.to_string();
        let (node_path, prop) = match text.rsplit_once(':') {
            Some((n, prop)) => (if n.is_empty() { "." } else { n }, prop),
            None => (".", text.as_str()),
        };
        let Some(node) = root.get_node_or_null(node_path) else {
            godot_error!(
                "gdnet: input property {text} of {} names no node",
                root.get_path()
            );
            continue;
        };
        out.push((node, StringName::from(prop), name_hash(&text)));
    }
    out
}

/// Writes the frame's values of `input_properties` to their nodes (the server, a replay, a delayed
/// owner).
fn write_input_props(props: &[(Gd<Node>, StringName, u32)], frame: &InputFrame) {
    for (n, p, h) in props {
        if let Some(v) = frame.get(*h) {
            if n.is_instance_valid() {
                let mut n = n.clone();
                n.set(p, &net_input::from_input_value(v));
            }
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

/// Every part node of an entity, by part index (None for a part that is missing).
fn part_node_list(scene: &SceneInfo, root: &Gd<Node>) -> PartNodes {
    Rc::new(
        (0..scene.part_count())
            .map(|i| scene.part_node(root, i))
            .collect(),
    )
}

/// Every field of every part, in table order (for a `Create`).
fn read_all(rt: &RuntimeProps, nodes: &[Option<Gd<Node>>]) -> Vec<Vec<Value>> {
    rt.fields
        .iter()
        .enumerate()
        .map(|(p, fields)| match nodes.get(p) {
            Some(Some(n)) => fields.iter().map(|f| apply::read(n, f)).collect(),
            _ => fields.iter().map(|_| Value::Nil).collect(),
        })
        .collect()
}

/// The writes a decoded state asks for.
fn state_writes(
    rt: &RuntimeProps,
    nodes: &[Option<Gd<Node>>],
    values: Vec<(u16, u16, Value)>,
) -> Vec<(Gd<Node>, crate::infer::RtField, Value)> {
    values
        .into_iter()
        .filter_map(|(p, f, v)| {
            let node = nodes.get(usize::from(p))?.clone()?;
            let field = rt.fields.get(usize::from(p))?.get(usize::from(f))?.clone();
            Some((node, field, v))
        })
        .collect()
}

/// Replicas never simulate bodies (2.5): RigidBody parts are frozen once.
fn freeze_bodies(nodes: &[Option<Gd<Node>>]) {
    for n in nodes.iter().flatten() {
        let is_body = n.is_class("RigidBody2D") || n.is_class("RigidBody3D");
        if is_body {
            let mut n = n.clone();
            n.set("freeze", &true.to_variant());
        }
    }
}

/// An entity's priority: `Net.configure(node, {"priority": p})`, else its `NetEntity`'s, else 1.
pub fn priority_of(root: &Gd<Node>) -> f32 {
    if root.has_meta(META_PRIORITY) {
        if let Some(p) = profiles::number(&root.get_meta(META_PRIORITY)) {
            return p as f32;
        }
    }
    net_entity_of(root).map_or(1.0, |ne| ne.bind().get_priority() as f32)
}

/// WP6: calls (docs/PLAN_NETCODE.md 2.1 "Auto-exposed methods"; the verbs are in call.rs, the routing in
/// netcore/src/call.rs). Outgoing: `call_out` resolves the part and the method, then runs the call
/// locally (no session: single player), queues it (the entity has no net id yet), or asks netcore for
/// the route and runs it here when the route says so. Incoming: `on_call` (an `EntityEvent::Call`, in
/// order with the entity events) checks trust on the server (D14: the sender owns the entity, or the
/// part's `_net_allow(method, sender)` says yes), runs the method with `callv` (a trailing
/// `NetCallInfo` appended when the method declares one), marks the entity's cold properties dirty and
/// reports back to netcore. User code runs under the base guard; core access in short `with_core`
/// borrows.
impl NetManager {
    /// The method tables of the entity rooted at `iid` (derived now when the entity was not spawned yet).
    fn methods_of(&mut self, iid: InstanceId) -> Option<Rc<MethodTable>> {
        let e = self.entries.get(&iid)?;
        if let Some(m) = &e.methods {
            return Some(m.clone());
        }
        let scene = e.scene.clone();
        let root = Gd::<Node>::try_from_instance_id(iid).ok()?;
        let nodes = part_node_list(&scene, &root);
        let mt = methods::table_for(&scene, &nodes);
        if let Some(e) = self.entries.get_mut(&iid) {
            e.methods = Some(mt.clone());
        }
        Some(mt)
    }

    /// A call on `node`, a part of the entity rooted at `root` (call.rs checked the Callable and
    /// encoded the arguments). Errors are the call site's.
    pub fn call_out(
        &mut self,
        root: &Gd<Node>,
        node: &Gd<Node>,
        req: CallReq,
    ) -> Result<(), String> {
        let iid = root.instance_id();
        let Some(e) = self.entries.get(&iid) else {
            return Err(std::format!("{} is not enrolled", root.get_path()));
        };
        let rel = root.get_path_to(node).to_string();
        let Some(part) = e.scene.table.index_of(&rel) else {
            return Err(std::format!(
                "{} is inside the entity {} but is not one of its parts (a node added at run time); call a method of a part of the scene",
                node.get_path(),
                root.get_path()
            ));
        };
        let id = e.id;
        let Some(mt) = self.methods_of(iid) else {
            return Err(std::format!("{} has no method table", root.get_path()));
        };
        let hash = method_hash(&req.method.to_string());
        let Some(mi) = mt.lookup(part, hash).cloned() else {
            return Err(std::format!(
                "{} has no network-callable method {} (its script and class methods are, except those beginning with _)",
                node.get_path(),
                req.method
            ));
        };
        // Read the session now: a manager added this tick has not synced yet.
        let online = self.session().is_some_and(|s| s.bind().is_connected());
        if !online {
            // Single player (or no session yet in this branch): it just runs.
            let info = mi.wants_info.then(|| NetCallInfo::make(1, 0, true));
            let v = {
                let _guard = self.base_mut();
                invoke(node, &StringName::from(mi.name.as_str()), &req.args, info)
            };
            NetCall::resolve(&req.call, Ok(v));
            return Ok(());
        }
        let queued = Queued {
            iid,
            part,
            hash,
            wants_info: mi.wants_info,
            req,
        };
        match id {
            Some(id) => self.dispatch(id, queued),
            None => self.queued_calls.push(queued),
        }
        Ok(())
    }

    /// Hands a call on a spawned entity to netcore and runs it here when the route says so.
    fn dispatch(&mut self, id: NetId, q: Queued) {
        let Some(mut session) = self.session() else {
            NetCall::resolve(&q.req.call, Err(String::from("no session")));
            return;
        };
        let out = OutCall {
            entity: id,
            part: q.part,
            method: q.hash,
            verb: q.req.verb,
            target: q.req.target,
            args: q.req.bytes.clone(),
            unreliable: q.req.unreliable,
        };
        let plan = session
            .bind_mut()
            .with_core(|c, now| c.call(out, now).map(|p| (p, c.current_tick())));
        let (plan, tick) = match plan {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                NetCall::resolve(&q.req.call, Err(e));
                return;
            }
            None => {
                NetCall::resolve(&q.req.call, Err(String::from("no session")));
                return;
            }
        };
        let mut value = Variant::nil();
        if plan.run_local {
            let node = Gd::<Node>::try_from_instance_id(q.iid)
                .ok()
                .and_then(|root| {
                    self.entries
                        .get(&q.iid)
                        .and_then(|e| e.scene.part_node(&root, q.part))
                });
            if let Some(node) = node {
                let local = session.bind().unique_id() as u32;
                let info = q.wants_info.then(|| NetCallInfo::make(local, tick, true));
                let name = StringName::from(q.req.method.to_string().as_str());
                value = {
                    let _guard = self.base_mut();
                    invoke(&node, &name, &q.req.args, info)
                };
                self.cold_dirty(q.iid);
            }
        }
        match plan.request {
            Some(r) => session.bind_mut().register_call(r, q.req.call.clone()),
            None => NetCall::resolve(&q.req.call, Ok(value)),
        }
    }

    /// Dispatches queued calls whose entity got its net id; drops those whose entity is gone.
    fn flush_queued(&mut self, _session: &Gd<NetSession>) {
        if self.queued_calls.is_empty() {
            return;
        }
        let queued = std::mem::take(&mut self.queued_calls);
        for q in queued {
            match self.entries.get(&q.iid).map(|e| e.id) {
                Some(Some(id)) => self.dispatch(id, q),
                Some(None) => self.queued_calls.push(q),
                None => NetCall::resolve(
                    &q.req.call,
                    Err(String::from(
                        "the entity was removed before it was spawned; the call was dropped",
                    )),
                ),
            }
        }
    }

    fn cold_dirty(&mut self, iid: InstanceId) {
        if let Ok(root) = Gd::<Node>::try_from_instance_id(iid) {
            self.mark_cold_dirty(&root);
        }
    }

    /// A received call (in order with the entity events).
    fn on_call(&mut self, session: &Gd<NetSession>, ci: CallIn) {
        let mut s = session.clone();
        let respond = |s: &mut Gd<NetSession>, result: Result<Vec<u8>, String>| {
            if ci.handle != 0 {
                s.bind_mut()
                    .with_core(|c, now| c.finish_call(ci.handle, result, now));
            }
        };
        let Some(iid) = self.by_id.get(&ci.entity).copied() else {
            respond(&mut s, Err(std::format!("entity {} is gone", ci.entity)));
            return;
        };
        let node = Gd::<Node>::try_from_instance_id(iid).ok().and_then(|root| {
            self.entries
                .get(&iid)
                .and_then(|e| e.scene.part_node(&root, ci.part))
        });
        let (Some(node), Some(mt)) = (node, self.methods_of(iid)) else {
            respond(
                &mut s,
                Err(std::format!(
                    "part {} of entity {} is missing",
                    ci.part,
                    ci.entity
                )),
            );
            return;
        };
        let Some(mi) = mt.lookup(ci.part, ci.method).cloned() else {
            respond(
                &mut s,
                Err(std::format!(
                    "unknown method (hash {:08x}) on {}",
                    ci.method,
                    node.get_path()
                )),
            );
            return;
        };
        if ci.check {
            let allowed = {
                let mut n = node.clone();
                let _guard = self.base_mut();
                n.has_method("_net_allow")
                    && n.call(
                        "_net_allow",
                        &[
                            StringName::from(mi.name.as_str()).to_variant(),
                            i64::from(ci.sender).to_variant(),
                        ],
                    )
                    .try_to::<bool>()
                    .unwrap_or(false)
            };
            if !allowed {
                let reason = std::format!(
                    "refused: peer {} may not call {} on {}: it does not own the entity and _net_allow did not admit it",
                    ci.sender,
                    mi.name,
                    node.get_path()
                );
                {
                    let mut obj = s.clone().upcast::<Node>();
                    let _guard = self.base_mut();
                    obj.emit_signal(
                        "call_refused",
                        &[
                            node.to_variant(),
                            StringName::from(mi.name.as_str()).to_variant(),
                            i64::from(ci.sender).to_variant(),
                        ],
                    );
                }
                s.bind_mut()
                    .with_core(|c, now| c.refuse_call(ci.handle, &reason, now));
                return;
            }
        }
        if !ci.run_local {
            respond(&mut s, Ok(Vec::new()));
            return;
        }
        let Some(args) = net_call::decode(&ci.args) else {
            respond(&mut s, Err(String::from("malformed arguments")));
            return;
        };
        let info = mi
            .wants_info
            .then(|| NetCallInfo::make(ci.sender, ci.tick, ci.sender == self.local_id));
        let value = {
            let _guard = self.base_mut();
            invoke(&node, &StringName::from(mi.name.as_str()), &args, info)
        };
        self.cold_dirty(iid);
        let bytes = if ci.verb == Verb::Server {
            net_call::encode_return(&value)
        } else {
            Vec::new()
        };
        respond(&mut s, Ok(bytes));
    }
}

/// Calls `method` on `node` with `args` (and a trailing `NetCallInfo` when given). The caller holds its
/// base guard (user code runs).
pub fn invoke(
    node: &Gd<Node>,
    method: &StringName,
    args: &[Variant],
    info: Option<Gd<NetCallInfo>>,
) -> Variant {
    let mut arr = VarArray::new();
    for a in args {
        arr.push(a);
    }
    if let Some(i) = info {
        arr.push(&i.to_variant());
    }
    let mut n = node.clone();
    n.callv(method, &arr)
}

/// The session serving `node`'s branch, if one runs there.
pub fn session_of(node: &Gd<Node>) -> Option<Gd<NetSession>> {
    if !node.is_inside_tree() {
        return None;
    }
    let api = node.get_multiplayer()?;
    if !api.has_meta(crate::session::META_SESSION) {
        return None;
    }
    let iid = api
        .get_meta(crate::session::META_SESSION)
        .try_to::<i64>()
        .ok()?;
    Gd::<NetSession>::try_from_instance_id(InstanceId::from_i64(iid)).ok()
}

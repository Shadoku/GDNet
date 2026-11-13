//! `Net`: the singleton GDScript sees as `Net` (docs/PLAN_NETCODE.md 3.1; WP1 surface: sessions,
//! ticks, peers, RTT, emulation).
//!
//! Design (see session.rs): `Net` is a plain `Object` registered with `Engine.register_singleton` at
//! the scene init stage. It owns one default `NetSession`, a hidden `Node` it adds to the root of the
//! `SceneTree` (deferred, so `host()` may be called from any `_ready`) when a session starts and frees on
//! `leave()` or a disconnection. That node's `_physics_process` is the per-tick loop; `Net` re-emits its
//! signals. Per-branch sessions for headless tests are separate `NetSession` nodes
//! (tests/framework/net_fixture.gd); `Net` itself always serves the default (whole-tree) multiplayer.
//!
//! The per-tick signal is `ticked(tick)` (WP2 renamed it from `tick`, which the method `tick()` shadowed).
//!
//! WP2 entity surface: `manage`, `ignore`, `set_owner`, `spawn_for`, `is_owner`, `is_authority`,
//! `net_id`, and the signals `entity_spawned`, `entity_despawning`, `spawn_refused` (relayed from the
//! default session). The node-taking calls work for any branch: they find the node's `NetManager`, so
//! they serve fixture branches as well as the default session.
//!
//! WP3: `configure(node, Dictionary)` (configure.rs), and the properties `profiles` (laid over the
//! shipped defaults and `gdnet/sync/profiles`), `cold_poll_ticks` and `budget_bytes_per_s` (0 = the
//! project settings `gdnet/sync/cold_poll_ticks`, `gdnet/budget/bytes_per_second`). Sessions read them
//! when they start. `Net` is a `tool` class: it is created in the editor too (the singleton is registered
//! at the scene init stage), which gdext's v0.6 placeholder migration requires to be explicit.
//!
//! WP4: `input(node)` (the entity's `NetInput`, input.rs), `is_fresh()` (tick.rs), the properties
//! `input_delay_ticks` and `adaptive_delay`, the signal `rolled_back(node, from_tick, to_tick)` (relayed
//! from the default session) and `global_input_warnings()`.
//!
//! WP5: the read-only `jitter_buffer_ticks` (interp.rs) and the `teleport_distance` key of `configure()`.
//!
//! WP6: the call verbs `sync`, `sync_unreliable`, `server`, `owner`, `others`, `peer` (call.rs) and the
//! signal `call_refused(node, method, sender)` (relayed from the default session). gdext has no
//! varargs `#[func]`, so each verb takes up to eight optional Variant arguments (the left-out slots hold
//! a private RID sentinel); they serve any branch, through the node's manager.
//!
//! WP7: `request_owner(node)` (a `NetCall` whose `completed` carries true when granted),
//! `release_owner(node)`, the signals `owner_changed(node, old, new)` and `write_rejected(node, sender)`
//! (relayed from the default session) and `state` (`NetKeyedState`: the default session's keyed state,
//! a local map while no session runs).

use crate::authority;
use crate::call::{self as net_call, NetArg, NetCall};
use crate::configure::{self, Op};
use crate::emulation::NetEmulation;
use crate::input::NetInput;
use crate::keyed::NetKeyedState;
use crate::manager::{
    entity_root_of, net_entity_of, session_of, NetManager, LOCAL_GROUP, META_OWNER, META_PRIORITY,
};
use crate::meter;
use crate::pipe_peer::NetPipePeer;
use crate::profiles::{self, NetProfiles};
use crate::session::{tick_rate, NetSession};
use crate::tick;
use godot::classes::node::InternalMode;
use godot::classes::{ENetMultiplayerPeer, Engine, MultiplayerPeer, Node, PackedScene, SceneTree};
use godot::global::Error;
use godot::prelude::*;
use netcore::call::Verb;

/// Name of the default session's node under the tree's root.
pub const SESSION_NODE: &str = "__NetSession";
/// Name of the manager `Net.manage(node)` adds under `node`.
pub const MANAGED_NODE: &str = "__NetManaged";
/// Meta on an entity root: `Net.configure()` options later packages read (radius, ...).
pub const META_CONFIG: &str = "_net_config";

/// The scene instance root `node` belongs to (itself or the nearest ancestor with a scene file), or
/// `node` itself for a bare node.
fn scene_root_of(node: &Gd<Node>) -> Gd<Node> {
    let mut cur = Some(node.clone());
    while let Some(n) = cur {
        if !n.get_scene_file_path().is_empty() {
            return n;
        }
        cur = n.get_parent();
    }
    node.clone()
}

const RELAYED: [(&str, &str); 13] = [
    ("peer_joined", "_on_peer_joined"),
    ("peer_left", "_on_peer_left"),
    ("connected", "_on_connected"),
    ("disconnected", "_on_disconnected"),
    ("ticked", "_on_ticked"),
    ("entity_spawned", "_on_entity_spawned"),
    ("entity_despawning", "_on_entity_despawning"),
    ("spawn_refused", "_on_spawn_refused"),
    ("rolled_back", "_on_rolled_back"),
    ("call_refused", "_on_call_refused"),
    ("owner_changed", "_on_owner_changed"),
    ("write_rejected", "_on_write_rejected"),
    ("travelled", "_on_travelled"),
];

#[derive(GodotClass)]
#[class(tool, init, base = Object)]
pub struct Net {
    base: Base<Object>,
    session: Option<Gd<NetSession>>,
    /// Network emulation for the sessions `Net` starts (also changed live).
    #[var(get = get_emulation, set = set_emulation)]
    emulation: Option<Gd<NetEmulation>>,
    /// Physics ticks per second (read only).
    #[var(get = get_tick_rate, no_set)]
    #[allow(dead_code)]
    tick_rate: i64,
    /// Test hook: copied to the next session's `_debug_protocol_salt` (a non-zero value forces a
    /// protocol mismatch).
    shutdown_hooked: bool,
    #[var(rename = _debug_protocol_salt, get = get_debug_protocol_salt, set = set_debug_protocol_salt)]
    debug_protocol_salt: i64,
    /// Profiles laid over the shipped defaults and the `gdnet/sync/profiles` resource (WP3).
    #[var(get = get_profiles, set = set_profiles)]
    profiles: Option<Gd<NetProfiles>>,
    /// Cold properties are polled every this many ticks (0 = `gdnet/sync/cold_poll_ticks`, default 6).
    #[var(get = get_cold_poll_ticks, set = set_cold_poll_ticks)]
    cold_poll_ticks: i64,
    /// Snapshot bytes per second per peer (0 = `gdnet/budget/bytes_per_second`, default 32,768) for
    /// sessions whose own `budget_bytes_per_s` is 0 (`Net`'s among them). Applies to running sessions
    /// at their next tick (review 2026-11-08; WP14 had it read only at session start).
    #[var(get = get_budget_bytes_per_s, set = set_budget_bytes_per_s)]
    budget_bytes_per_s: i64,
    /// Review 2026-11-08: entities hash telemetry samples per tick on sessions whose own `hash_sample`
    /// is -1 (`Net`'s among them). Reads as the effective value; -1 written = back to
    /// `gdnet/debug/hash_sample` (default 16). Applies to running sessions at their next tick.
    #[var(get = get_hash_sample, set = set_hash_sample)]
    #[allow(dead_code)]
    hash_sample: i64,
    /// WP4: owners apply the input they sample this many ticks later (and stamp it accordingly), which
    /// trades responsiveness for fewer rollbacks. Default 0; applies to every session at once.
    #[var(get = get_input_delay_ticks, set = set_input_delay_ticks)]
    input_delay_ticks: i64,
    /// WP4, experimental (D10): ramps extra input delay in while an interaction is flagged and back out
    /// afterwards. Default off.
    #[var(get = get_adaptive_delay, set = set_adaptive_delay)]
    adaptive_delay: bool,
    /// WP5: the effective jitter buffer in ticks (read only): the default session's current value
    /// while it is a client, else `gdnet/interpolation/jitter_buffer_ticks` (default 2).
    #[var(get = get_jitter_buffer_ticks, no_set)]
    #[allow(dead_code)]
    jitter_buffer_ticks: i64,
    /// WP7: the keyed state (`NetKeyedState`): the default session's while one runs, else a local map.
    #[var(get = get_state, no_set)]
    #[allow(dead_code)]
    state: Option<Gd<NetKeyedState>>,
    state_cell: std::cell::OnceCell<Gd<NetKeyedState>>,
    /// WP13: a server's level is unloaded this many seconds after its last player left (0 = the project
    /// setting `gdnet/levels/empty_timeout_s`, default 30). Applies to every session at once.
    #[var(get = get_empty_level_timeout, set = set_empty_level_timeout)]
    #[allow(dead_code)]
    empty_level_timeout: f64,
    /// WP8: the meter and the overlay layers while shown.
    meter: Option<Gd<Node>>,
    overlay: Option<Gd<Node>>,
}

#[godot_api]
impl Net {
    #[func]
    fn get_debug_protocol_salt(&self) -> i64 {
        self.debug_protocol_salt
    }

    #[func]
    fn set_debug_protocol_salt(&mut self, salt: i64) {
        self.debug_protocol_salt = salt;
    }

    #[func]
    fn get_profiles(&self) -> Option<Gd<NetProfiles>> {
        self.profiles.clone()
    }

    #[func]
    fn set_profiles(&mut self, p: Option<Gd<NetProfiles>>) {
        profiles::set_net_profiles(p.as_ref().map(|r| r.bind().parsed()));
        self.profiles = p;
    }

    #[func]
    fn get_cold_poll_ticks(&self) -> i64 {
        i64::from(profiles::cold_poll_ticks())
    }

    #[func]
    fn set_cold_poll_ticks(&mut self, v: i64) {
        self.cold_poll_ticks = v.max(0);
        profiles::set_cold_poll_ticks((v > 0).then_some(v as u32));
    }

    #[func]
    fn get_budget_bytes_per_s(&self) -> i64 {
        i64::from(profiles::budget_bytes_per_s())
    }

    #[func]
    fn set_budget_bytes_per_s(&mut self, v: i64) {
        self.budget_bytes_per_s = v.max(0);
        profiles::set_budget((v > 0).then_some(v as u32));
    }

    #[func]
    fn get_hash_sample(&self) -> i64 {
        i64::from(crate::hash::sample())
    }

    #[func]
    fn set_hash_sample(&mut self, v: i64) {
        crate::hash::set_net_sample((v >= 0).then(|| v.min(1024) as u32));
    }

    #[func]
    fn get_input_delay_ticks(&self) -> i64 {
        i64::from(tick::input_delay())
    }

    #[func]
    fn set_input_delay_ticks(&mut self, v: i64) {
        self.input_delay_ticks = v.clamp(0, 60);
        tick::set_input_delay(self.input_delay_ticks as u32);
    }

    #[func]
    fn get_jitter_buffer_ticks(&self) -> i64 {
        match self.session.as_ref() {
            Some(s) => s.bind().get_jitter_buffer_ticks(),
            None => i64::from(crate::interp::jitter_buffer_ticks()),
        }
    }

    #[func]
    fn get_adaptive_delay(&self) -> bool {
        tick::adaptive_delay()
    }

    #[func]
    fn set_adaptive_delay(&mut self, v: bool) {
        self.adaptive_delay = v;
        tick::set_adaptive_delay(v);
    }

    /// The input of the entity `node` belongs to (rung 1): on its owner the frame sampled for this tick,
    /// on the server the frame the owner sent for this tick, and for an entity the server owns (or
    /// offline, or an unmanaged node) the real `Input`. Use it instead of `Input` in `_physics_process`.
    #[func]
    fn input(&self, node: Gd<Node>) -> Gd<NetInput> {
        entity_root_of(&node)
            .and_then(|(root, m)| m.bind().input_of(&root))
            .unwrap_or_else(NetInput::new_live)
    }

    /// False while gdnet replays ticks after a rollback (the same `_physics_process` runs again for ticks
    /// already shown); true otherwise. Guard one-shot effects (sounds, particles) with it.
    #[func]
    fn is_fresh(&self) -> bool {
        tick::is_fresh()
    }

    /// The scripts the global-`Input` check warned about so far (D14), for tools and tests.
    #[func]
    fn global_input_warnings(&self) -> PackedStringArray {
        tick::warned_scripts()
            .iter()
            .map(|s| GString::from(s.as_str()))
            .collect()
    }

    /// Corrects what the entity `node` belongs to replicates (rung 3): `state: ["+prop", "-prop"]`,
    /// `quantize: {prop: "cm"}`, `send: {prop: "hot"}`, `tolerance`, `interpolate`, `teleport_distance`
    /// change the property table of the entity's scene and must be made before that scene's first spawn on this peer, the
    /// same way on every peer; `priority` applies to this entity at any time. Paths are `prop` or
    /// `Part:prop`, relative to `node`.
    #[func]
    fn configure(&mut self, node: Gd<Node>, options: VarDictionary) {
        let (root, manager) = match entity_root_of(&node) {
            Some((r, m)) => (r, Some(m)),
            None => (scene_root_of(&node), None),
        };
        let scene = root.get_scene_file_path().to_string();
        let key = if scene.is_empty() {
            root.get_class().to_string()
        } else {
            scene
        };
        let prefix = if root == node {
            String::new()
        } else {
            root.get_path_to(&node).to_string()
        };
        let path = |t: &str| configure::parse_path(t, &prefix);
        let mut ops = Vec::new();
        let mut extra = VarDictionary::new();
        for (k, v) in options.iter_shared() {
            let k = k.to_string();
            match k.as_str() {
                "state" => {
                    let Ok(list) = v.try_to::<VarArray>() else {
                        godot_error!(
                            "Net.configure: state must be an Array of \"+prop\" / \"-prop\""
                        );
                        continue;
                    };
                    for item in list.iter_shared() {
                        let t = item.to_string();
                        if let Some(rest) = t.strip_prefix('-') {
                            ops.push(Op::Remove(path(rest)));
                        } else {
                            ops.push(Op::Add(path(t.trim_start_matches('+'))));
                        }
                    }
                }
                "quantize" | "send" | "schedule" => {
                    let Ok(d) = v.try_to::<VarDictionary>() else {
                        godot_error!("Net.configure: {k} must be a Dictionary of property: value");
                        continue;
                    };
                    for (p, q) in d.iter_shared() {
                        let (p, q) = (p.to_string(), q.to_string());
                        let op = if k == "quantize" {
                            netcore::Quant::parse(&q).map(|q| Op::Quantize(path(&p), q))
                        } else {
                            netcore::Schedule::parse(&q).map(|s| Op::Send(path(&p), s))
                        };
                        match op {
                            Some(op) => ops.push(op),
                            None => godot_error!("Net.configure: {k} {q:?} for {p} is not known"),
                        }
                    }
                }
                "tolerance" => match profiles::number(&v) {
                    Some(t) => ops.push(Op::Tolerance(None, t as f32)),
                    None => {
                        if let Ok(d) = v.try_to::<VarDictionary>() {
                            for (p, t) in d.iter_shared() {
                                let t = profiles::number(&t).unwrap_or(0.0) as f32;
                                ops.push(Op::Tolerance(Some(path(&p.to_string())), t));
                            }
                        }
                    }
                },
                "teleport_distance" => match profiles::number(&v) {
                    Some(t) => ops.push(Op::Teleport(None, t.max(0.0) as f32)),
                    None => {
                        if let Ok(d) = v.try_to::<VarDictionary>() {
                            for (p, t) in d.iter_shared() {
                                let t = profiles::number(&t).unwrap_or(0.0).max(0.0) as f32;
                                ops.push(Op::Teleport(Some(path(&p.to_string())), t));
                            }
                        }
                    }
                },
                "interpolate" => match v.try_to::<bool>() {
                    Ok(b) => ops.push(Op::Interpolate(None, b)),
                    Err(_) => {
                        if let Ok(d) = v.try_to::<VarDictionary>() {
                            for (p, b) in d.iter_shared() {
                                let b = b.try_to::<bool>().unwrap_or(false);
                                ops.push(Op::Interpolate(Some(path(&p.to_string())), b));
                            }
                        }
                    }
                },
                "priority" => {
                    let p = profiles::number(&v).unwrap_or(1.0);
                    let mut r = root.clone();
                    r.set_meta(META_PRIORITY, &p.to_variant());
                    if let Some(mut m) = manager.clone() {
                        m.bind_mut().set_priority(&root, p as f32);
                    }
                }
                // WP16 (interest.rs): table changes per property, settings per entity.
                "audience" => {
                    let Ok(d) = v.try_to::<VarDictionary>() else {
                        godot_error!("Net.configure: audience must be a Dictionary of property: \"owner\" | \"all\"");
                        continue;
                    };
                    for (p, a) in d.iter_shared() {
                        let (p, a) = (p.to_string(), a.to_string());
                        match netcore::Audience::parse(&a) {
                            Some(a) => ops.push(Op::Audience(path(&p), a)),
                            None => {
                                godot_error!("Net.configure: audience {a:?} for {p} is not known")
                            }
                        }
                    }
                }
                "radius" => {
                    crate::interest::store_radius(&root, profiles::number(&v).unwrap_or(0.0));
                    crate::interest::apply_live(&root);
                }
                "visible_to" => match crate::interest::store_visible_to(&root, &v) {
                    Ok(()) => crate::interest::apply_live(&root),
                    Err(e) => godot_error!("Net.configure: {e}"),
                },
                _ => {
                    extra.set(k.as_str(), &v);
                }
            }
        }
        if let Err(e) = configure::add_ops(&key, ops) {
            godot_error!("{e}");
        }
        if !extra.is_empty() {
            let mut r = root.clone();
            let mut stored = if r.has_meta(META_CONFIG) {
                r.get_meta(META_CONFIG)
                    .try_to::<VarDictionary>()
                    .unwrap_or_default()
            } else {
                VarDictionary::new()
            };
            for (k, v) in extra.iter_shared() {
                stored.set(&k, &v);
            }
            r.set_meta(META_CONFIG, &stored.to_variant());
        }
    }

    #[signal]
    fn peer_joined(id: i64);
    #[signal]
    fn peer_left(id: i64);
    #[signal]
    fn connected();
    #[signal]
    fn disconnected(reason: GString);
    #[signal]
    fn ticked(tick: i64);
    #[signal]
    fn entity_spawned(node: Gd<Node>);
    #[signal]
    fn entity_despawning(node: Gd<Node>);
    #[signal]
    fn spawn_refused(node: Gd<Node>);
    /// WP4: an entity this peer predicts was restored to the server's state at `to_tick` and replayed
    /// up to `from_tick` (relayed from the default session).
    #[signal]
    fn rolled_back(node: Gd<Node>, from_tick: i64, to_tick: i64);
    /// WP6 (server): a call was refused (relayed from the default session).
    #[signal]
    fn call_refused(node: Gd<Node>, method: StringName, sender: i64);
    /// WP7: an entity's owner changed (relayed from the default session).
    #[signal]
    fn owner_changed(node: Gd<Node>, old: i64, new: i64);
    /// WP7: a write was rejected (relayed from the default session; on the writer `sender` is itself).
    #[signal]
    fn write_rejected(node: Gd<Node>, sender: i64);

    /// WP7: the keyed state.
    #[func]
    fn get_state(&self) -> Option<Gd<NetKeyedState>> {
        Some(self.state_cell.get_or_init(NetKeyedState::new_gd).clone())
    }

    /// WP7: asks for ownership of the entity `node` belongs to (its `NetEntity.transfer` decides:
    /// `fixed` refuses, `take` grants, `request` asks the current owner). Returns a `NetCall`:
    /// `await Net.request_owner(node).completed` is true when granted. Already the owner, or without a
    /// session: true at once.
    #[func]
    fn request_owner(&mut self, node: Gd<Node>) -> Gd<NetCall> {
        let Some((root, mut m)) = entity_root_of(&node) else {
            return authority::resolved(true);
        };
        let r = m.bind_mut().request_owner(&root);
        match r {
            Ok(Some(granted)) => authority::resolved(granted),
            Ok(None) => {
                let id = m.bind().net_id_of(&root);
                let call = NetCall::fresh();
                match session_of(&root) {
                    Some(mut s) => s.bind_mut().register_owner_request(id, call.clone()),
                    None => NetCall::resolve(&call, Err(String::from("no session"))),
                }
                call
            }
            Err(e) => authority::failed(std::format!("Net.request_owner: {e}")),
        }
    }

    /// WP7: gives the entity `node` belongs to back to the server (a client must own it; on the server
    /// it is `set_owner(node, 1)`). True when the release was made.
    #[func]
    fn release_owner(&mut self, node: Gd<Node>) -> bool {
        match entity_root_of(&node) {
            Some((root, mut m)) => m.bind_mut().release_owner(&root),
            None => false,
        }
    }

    #[func]
    fn _on_owner_changed(&mut self, node: Gd<Node>, old: i64, new: i64) {
        self.base_mut().emit_signal(
            "owner_changed",
            &[node.to_variant(), old.to_variant(), new.to_variant()],
        );
    }

    #[func]
    fn _on_write_rejected(&mut self, node: Gd<Node>, sender: i64) {
        self.base_mut()
            .emit_signal("write_rejected", &[node.to_variant(), sender.to_variant()]);
    }

    /// Enrols `node`'s subtree as if `node` were a `NetManager` (its scene instances become entities).
    /// For nodes outside any manager; call it on every peer, at the same path, before spawning under it.
    /// Returns the manager created (a hidden child of `node`), or the existing one.
    #[func]
    fn manage(&mut self, mut node: Gd<Node>) -> Gd<NetManager> {
        if let Some(existing) = node.get_node_or_null(MANAGED_NODE) {
            if let Ok(m) = existing.try_cast::<NetManager>() {
                return m;
            }
        }
        let mut m = NetManager::new_alloc();
        m.set_name(MANAGED_NODE);
        m.bind_mut().set_managed_root(node.clone());
        node.add_child_ex(&m).internal(InternalMode::FRONT).done();
        m
    }

    /// Excludes `node` and everything below it from enrolment (adds it to the group `net_local`). Call it
    /// before the node enters a managed subtree.
    #[func]
    fn ignore(&mut self, mut node: Gd<Node>) {
        node.add_to_group(LOCAL_GROUP);
    }

    /// Server: makes `peer` the owner of the entity `node` belongs to (announced at the next tick). On a
    /// node not enrolled yet, the owner is used when it is. Returns false on a node outside any manager
    /// that is not a scene instance.
    #[func]
    fn set_owner(&mut self, mut node: Gd<Node>, peer: i64) -> bool {
        let peer = peer.max(0) as u32;
        match entity_root_of(&node) {
            Some((root, mut m)) => m.bind_mut().set_entity_owner(&root, peer),
            None => {
                node.set_meta(META_OWNER, &i64::from(peer).to_variant());
                if let Some(mut ne) = net_entity_of(&node) {
                    ne.bind_mut().set_owner_peer(i64::from(peer));
                }
                true
            }
        }
    }

    /// Server: instantiates `scene`, names it `name` (when given), makes `peer` its owner and adds it under
    /// `parent` (a manager or a node below one). Returns the new node.
    #[func]
    fn spawn_for(
        &mut self,
        peer: i64,
        scene: Gd<PackedScene>,
        mut parent: Gd<Node>,
        name: GString,
    ) -> Option<Gd<Node>> {
        let Some(mut node) = scene.instantiate() else {
            godot_error!("Net.spawn_for: the scene could not be instantiated");
            return None;
        };
        if !name.is_empty() {
            node.set_name(&name);
        }
        node.set_meta(META_OWNER, &peer.max(0).to_variant());
        // WP13: the first entity spawned for a peer is its player (its room is the peer's room).
        if !node.has_meta(crate::manager::META_PLAYER) {
            node.set_meta(crate::manager::META_PLAYER, &1i64.to_variant());
        }
        if let Some(mut ne) = net_entity_of(&node) {
            ne.bind_mut().set_owner_peer(peer.max(0));
        }
        parent.add_child(&node);
        Some(node)
    }

    /// True when this peer owns the entity `node` belongs to (without a session, or for an unmanaged node:
    /// true).
    #[func]
    fn is_owner(&self, node: Gd<Node>) -> bool {
        entity_root_of(&node)
            .and_then(|(root, m)| m.bind().query(&root))
            .is_none_or(|(_, owner, _)| owner)
    }

    /// True when this peer is the authority for the entity `node` belongs to: the server (without a
    /// session, or for an unmanaged node: true).
    #[func]
    fn is_authority(&self, node: Gd<Node>) -> bool {
        entity_root_of(&node)
            .and_then(|(root, m)| m.bind().query(&root))
            .is_none_or(|(_, _, auth)| auth)
    }

    /// The owner peer of the entity `node` belongs to (1 = the server; 0 for an unmanaged node).
    #[func]
    fn get_owner_of(&self, node: Gd<Node>) -> i64 {
        entity_root_of(&node)
            .and_then(|(root, m)| m.bind().query(&root))
            .map_or(0, |(o, _, _)| i64::from(o))
    }

    /// The net id of the entity `node` belongs to (0 before it is spawned or when unmanaged).
    #[func]
    fn net_id(&self, node: Gd<Node>) -> i64 {
        entity_root_of(&node).map_or(0, |(root, m)| i64::from(m.bind().net_id_of(&root)))
    }

    /// Hosts over ENet on `port`. Returns `OK` or the error `ENetMultiplayerPeer.create_server` gave.
    #[func]
    fn host(&mut self, port: i64) -> Error {
        self.ensure_shutdown_hook();
        let mut peer = ENetMultiplayerPeer::new_gd();
        let err = peer.create_server_ex(port as i32).max_channels(3).done();
        if err != Error::OK {
            return err;
        }
        self.start(peer.upcast())
    }

    /// Joins a host over ENet. `connected` fires when the handshake completes, `disconnected(reason)` if
    /// it fails.
    #[func]
    fn join(&mut self, address: GString, port: i64) -> Error {
        self.ensure_shutdown_hook();
        let mut peer = ENetMultiplayerPeer::new_gd();
        let err = peer
            .create_client_ex(&address, port as i32)
            .channel_count(3)
            .done();
        if err != Error::OK {
            return err;
        }
        self.start(peer.upcast())
    }

    /// A session with no network: a `NetPipePeer` server end, so this process hosts and plays as peer 1
    /// (the host is a client of itself).
    #[func]
    fn offline(&mut self) -> Error {
        self.ensure_shutdown_hook();
        let peer = NetPipePeer::new_gd();
        self.start(peer.upcast())
    }

    /// A session over any `MultiplayerPeer` (WebRTC, WebSocket, GodotSteam, Nakama, a `NetPipePeer`
    /// end...). A peer with unique id 1 hosts; any other joins.
    #[func]
    fn use_peer(&mut self, peer: Gd<MultiplayerPeer>) -> Error {
        self.ensure_shutdown_hook();
        self.start(peer)
    }

    /// Ends the session and restores the tree's original `MultiplayerAPI`.
    #[func]
    fn leave(&mut self) {
        if let Some(session) = self.session.take() {
            self.drop_session(session, true);
        }
    }

    /// The current tick; 0 when offline.
    #[func]
    fn tick(&self) -> i64 {
        self.session.as_ref().map_or(0, |s| s.bind().current_tick())
    }

    /// Smoothed round-trip time to `peer` in milliseconds; -1 when unknown.
    #[func]
    fn rtt(&self, peer: i64) -> f64 {
        self.session.as_ref().map_or(-1.0, |s| s.bind().rtt(peer))
    }

    /// The remote peers in the session (that finished the handshake), ascending.
    #[func]
    fn peers(&self) -> PackedInt32Array {
        self.session
            .as_ref()
            .map_or_else(PackedInt32Array::new, |s| s.bind().peers())
    }

    /// True from `host()` / `join()` / `offline()` / `use_peer()` until `leave()` or a disconnection.
    #[func]
    fn is_online(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.bind().is_online())
    }

    /// The default session's node while online (for tools and tests).
    #[func]
    fn get_session(&self) -> Option<Gd<NetSession>> {
        self.session.clone()
    }

    #[func]
    fn get_emulation(&self) -> Option<Gd<NetEmulation>> {
        self.emulation.clone()
    }

    #[func]
    fn set_emulation(&mut self, emulation: Option<Gd<NetEmulation>>) {
        self.ensure_shutdown_hook();
        self.emulation = emulation.clone();
        if let Some(mut s) = self.session.clone() {
            s.bind_mut().set_emulation(emulation);
        }
    }

    #[func]
    fn get_tick_rate(&self) -> i64 {
        i64::from(tick_rate())
    }

    /// WP8: sets `Net.emulation` to a new `NetEmulation` with the preset `name` (`off`, `lan`,
    /// `average`, `bad`, `mobile`, or `custom` = the `gdnet/debug/emulation/*` settings), applied to the
    /// running session at once. False (nothing changed) for an unknown name.
    #[func]
    fn emulation_preset(&mut self, name: GString) -> bool {
        match crate::emulation::from_preset(&name.to_string()) {
            Some(e) => {
                self.set_emulation(Some(e));
                true
            }
            None => false,
        }
    }

    /// WP8: shows (adds under the root as `__NetMeter`) or hides (frees) the network meter,
    /// `addons/gdnet/meter.tscn`. It shows the default session, else the first session in the tree.
    /// Also toggled by the action `gdnet_toggle_meter` (or F9) while `Net` has a session.
    #[func]
    pub fn show_meter(&mut self, show: bool) {
        if show == self.meter_visible() {
            return;
        }
        if show {
            self.meter = meter::new_meter().and_then(|m| meter::add_layer(m, meter::METER_NODE));
        } else {
            meter::remove_layer(self.meter.take());
        }
    }

    #[func]
    pub fn meter_visible(&self) -> bool {
        meter::is_live(&self.meter)
    }

    /// WP8: shows (adds under the root as `__NetOverlay`) or hides the ownership overlay: a label per
    /// managed entity at its screen position, coloured by role (see `overlay_rows()`).
    #[func]
    fn show_overlay(&mut self, show: bool) {
        if show == self.overlay_visible() {
            return;
        }
        if show {
            let layer = crate::overlay::NetOverlay::new_alloc().upcast::<Node>();
            self.overlay = meter::add_layer(layer, meter::OVERLAY_NODE);
        } else {
            meter::remove_layer(self.overlay.take());
        }
    }

    #[func]
    fn overlay_visible(&self) -> bool {
        meter::is_live(&self.overlay)
    }

    /// WP8: the default session's `stats()` (empty without a session).
    #[func]
    fn stats(&self) -> VarDictionary {
        self.session
            .as_ref()
            .map_or_else(VarDictionary::new, |s| s.bind().stats())
    }

    /// WP8: the default session's `overlay_rows()` (empty without a session).
    #[func]
    fn overlay_rows(&self) -> VarArray {
        self.session
            .as_ref()
            .map_or_else(VarArray::new, |s| s.bind().overlay_rows())
    }

    /// WP8: the divergences hash telemetry counted for the entity `node` belongs to, in its branch's
    /// session (0 on the server, unmanaged, or without a session).
    #[func]
    fn divergences(&self, node: Gd<Node>) -> i64 {
        session_of(&node).map_or(0, |s| s.bind().divergences(node))
    }

    #[func]
    fn _on_peer_joined(&mut self, id: i64) {
        self.base_mut()
            .emit_signal("peer_joined", &[id.to_variant()]);
    }

    #[func]
    fn _on_peer_left(&mut self, id: i64) {
        self.base_mut().emit_signal("peer_left", &[id.to_variant()]);
    }

    #[func]
    fn _on_connected(&mut self) {
        self.base_mut().emit_signal("connected", &[]);
    }

    #[func]
    fn _on_disconnected(&mut self, reason: GString) {
        // The session already stopped itself; free its node.
        if let Some(session) = self.session.take() {
            self.drop_session(session, false);
        }
        self.base_mut()
            .emit_signal("disconnected", &[reason.to_variant()]);
    }

    #[func]
    fn _on_tree_exiting(&mut self) {
        self.leave();
        let mut obj = self.base_mut();
        for signal in RELAYED.iter().map(|(s, _)| *s) {
            for conn in obj.get_signal_connection_list(signal).iter_shared() {
                if let Some(cb) = conn
                    .get("callable")
                    .and_then(|v| v.try_to::<Callable>().ok())
                {
                    obj.disconnect(signal, &cb);
                }
            }
        }
    }

    #[func]
    fn _on_ticked(&mut self, tick: i64) {
        self.base_mut().emit_signal("ticked", &[tick.to_variant()]);
    }

    #[func]
    fn _on_entity_spawned(&mut self, node: Gd<Node>) {
        self.base_mut()
            .emit_signal("entity_spawned", &[node.to_variant()]);
    }

    #[func]
    fn _on_entity_despawning(&mut self, node: Gd<Node>) {
        self.base_mut()
            .emit_signal("entity_despawning", &[node.to_variant()]);
    }

    #[func]
    fn _on_rolled_back(&mut self, node: Gd<Node>, from_tick: i64, to_tick: i64) {
        self.base_mut().emit_signal(
            "rolled_back",
            &[
                node.to_variant(),
                from_tick.to_variant(),
                to_tick.to_variant(),
            ],
        );
    }

    /// WP6 (rung 2): runs `callable` (a method of a part of a managed entity) on the authority, then on
    /// every peer that has the entity in scope. From a client it goes to the server only, which checks
    /// that the caller owns the entity (or the part's `_net_allow(method, sender)` admits it), runs it
    /// and sends it to everyone, the caller included. Up to eight arguments; bound arguments count.
    /// Returns a `NetCall` (`ok` / `error` once `completed`). Without a session it just runs.
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn sync(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Sync,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: `sync` on the unreliable channel (no answer, may be lost; for cosmetic effects).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn sync_unreliable(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Sync,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            true,
        )
    }

    /// WP6: runs `callable` on the server (any peer may call it; the method may take a trailing
    /// `NetCallInfo` to learn the sender) and returns the `completed` signal of its `NetCall`, so
    /// `var v = await Net.server(try_buy, 3)` is the server's return value (null on an error; the
    /// signal's object is the `NetCall`).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn server(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Signal {
        let call = net_call::make_call(
            Verb::Server,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        );
        Signal::from_object_signal(&call, "completed")
    }

    /// WP6: runs `callable` on the entity's owner only.
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn owner(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Owner,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: runs `callable` on every peer but the caller (the server included when a client calls).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn others(
        &mut self,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Others,
            0,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    /// WP6: runs `callable` on peer `id` only (1 = the server; a client's call goes via the server).
    // One slot per optional argument: gdext 0.5.5 has no varargs #[func].
    #[allow(clippy::too_many_arguments)]
    #[func]
    fn peer(
        &mut self,
        id: i64,
        callable: Callable,
        #[opt(default = net_call::NetArg::absent())] a0: NetArg,
        #[opt(default = net_call::NetArg::absent())] a1: NetArg,
        #[opt(default = net_call::NetArg::absent())] a2: NetArg,
        #[opt(default = net_call::NetArg::absent())] a3: NetArg,
        #[opt(default = net_call::NetArg::absent())] a4: NetArg,
        #[opt(default = net_call::NetArg::absent())] a5: NetArg,
        #[opt(default = net_call::NetArg::absent())] a6: NetArg,
        #[opt(default = net_call::NetArg::absent())] a7: NetArg,
    ) -> Gd<NetCall> {
        net_call::make_call(
            Verb::Peer,
            id.max(0) as u32,
            &callable,
            net_call::collect_args([a0, a1, a2, a3, a4, a5, a6, a7]),
            false,
        )
    }

    #[func]
    fn _on_call_refused(&mut self, node: Gd<Node>, method: StringName, sender: i64) {
        self.base_mut().emit_signal(
            "call_refused",
            &[node.to_variant(), method.to_variant(), sender.to_variant()],
        );
    }

    #[signal]
    fn travelled(player: Gd<Node>, from: GString, to: GString);

    #[func]
    fn _on_travelled(&mut self, player: Gd<Node>, from: GString, to: GString) {
        self.base_mut().emit_signal(
            "travelled",
            &[player.to_variant(), from.to_variant(), to.to_variant()],
        );
    }

    #[func]
    fn get_empty_level_timeout(&self) -> f64 {
        crate::world::net_timeout()
    }

    #[func]
    fn set_empty_level_timeout(&mut self, v: f64) {
        crate::world::set_net_timeout(v);
    }

    /// WP13 (2.11), server: sends the entity `player` belongs to into the level `scene_path`, at the
    /// node named `spawn_point` inside it (else the level's origin). The level is loaded under the World
    /// if needed; the player's client loads it too and frees its previous level. Applied at the next
    /// tick, so it is safe from a physics callback (a door's `body_entered`). Works in any branch.
    #[func]
    fn travel(
        &mut self,
        player: Gd<Node>,
        scene_path: GString,
        #[opt(default = "")] spawn_point: GString,
    ) -> bool {
        match session_of(&player) {
            Some(mut s) => s.bind_mut().travel(player, scene_path, spawn_point),
            None => {
                godot_error!("gdnet: Net.travel needs a session (host, join or offline first)");
                false
            }
        }
    }

    /// WP13, server: loads a level under the World of `Net`'s session (see `NetSession.load_level`).
    #[func]
    fn load_level(&mut self, scene_path: GString) -> Option<Gd<Node>> {
        let Some(mut s) = self.session.clone() else {
            godot_error!("gdnet: Net.load_level needs a session (host, join or offline first)");
            return None;
        };
        let level = s.bind_mut().load_level(scene_path);
        level
    }

    /// WP13: frees a level `Net`'s session loaded.
    #[func]
    fn unload_level(&mut self, scene_path: GString) -> bool {
        let Some(mut s) = self.session.clone() else {
            return false;
        };
        let done = s.bind_mut().unload_level(scene_path);
        done
    }

    /// WP13: the levels `Net`'s session holds (scene paths, in load order).
    #[func]
    fn levels(&self) -> PackedStringArray {
        self.session
            .clone()
            .map_or_else(PackedStringArray::new, |mut s| s.bind_mut().levels())
    }

    /// WP13, server: puts `peer` in room `name` (for peers without a player, e.g. spectators; empty:
    /// back to its player's room).
    #[func]
    fn set_room(&mut self, peer: i64, name: GString) {
        if let Some(mut s) = self.session.clone() {
            s.bind_mut().set_room(peer, name);
        }
    }

    /// WP13, server: the entity `node` belongs to is `peer`'s player: the peer's room is that entity's
    /// manager's room (the first entity `spawn_for`'d to a peer is its player without this call).
    #[func]
    fn set_player(&mut self, peer: i64, mut node: Gd<Node>) -> bool {
        match session_of(&node) {
            Some(mut s) => s.bind_mut().set_player(peer, node),
            None => {
                node.set_meta(crate::manager::META_PLAYER, &2i64.to_variant());
                true
            }
        }
    }

    /// WP13: registers the game's World node, the node levels are loaded under (in its branch).
    #[func]
    fn set_world(&mut self, mut node: Gd<Node>) {
        node.add_to_group(crate::world::WORLD_GROUP);
    }

    /// WP13: the session serving `node`'s branch (`Net`'s own session in a game; a fixture branch's
    /// session in the headless tests), or null without one.
    #[func]
    fn session_for(&self, node: Gd<Node>) -> Option<Gd<NetSession>> {
        session_of(&node).or_else(|| self.session.clone())
    }

    #[func]
    fn _on_spawn_refused(&mut self, node: Gd<Node>) {
        self.base_mut()
            .emit_signal("spawn_refused", &[node.to_variant()]);
    }
}

impl Net {
    /// Hooks the root's `tree_exiting` once: at shutdown, while scripts still exist, the singleton ends
    /// its session and drops every connection to its signals. (A GDScript lambda connected to
    /// `Net.tick` that captures no object is never disconnected by Godot; freeing it with the singleton,
    /// after the script language shut down, corrupts the heap. Godot 4.5's main-loop callbacks would
    /// do this too, but the extension targets the 4.3 API.)
    fn ensure_shutdown_hook(&mut self) {
        if self.shutdown_hooked {
            return;
        }
        let Some(mut root) = Engine::singleton()
            .get_main_loop()
            .and_then(|m| m.try_cast::<SceneTree>().ok())
            .and_then(|t| t.get_root())
        else {
            return;
        };
        let cb = Callable::from_object_method(&self.to_gd(), "_on_tree_exiting");
        root.connect("tree_exiting", &cb);
        self.shutdown_hooked = true;
    }

    fn start(&mut self, peer: Gd<MultiplayerPeer>) -> Error {
        self.leave();
        let Some(tree) = Engine::singleton()
            .get_main_loop()
            .and_then(|m| m.try_cast::<SceneTree>().ok())
        else {
            godot_error!("Net: no SceneTree to start a session in");
            return Error::ERR_UNCONFIGURED;
        };
        let Some(mut root) = tree.get_root() else {
            return Error::ERR_UNCONFIGURED;
        };
        let mut session = NetSession::new_alloc();
        session.set_name(SESSION_NODE);
        {
            let mut b = session.bind_mut();
            b.debug_protocol_salt = self.debug_protocol_salt;
            b.net_default = true;
            // WP8: `gdnet/debug/emulation_preset` when no emulation was set.
            if self.emulation.is_none() {
                self.emulation = crate::emulation::from_settings();
            }
            b.set_emulation(self.emulation.clone());
        }
        let err = session.bind_mut().start(peer, NodePath::default());
        if err != Error::OK {
            session.free();
            return err;
        }
        let state = self.state_cell.get_or_init(NetKeyedState::new_gd).clone();
        session.bind_mut().set_state_object(state);
        let me = self.to_gd();
        for (signal, method) in RELAYED {
            session.connect(signal, &Callable::from_object_method(&me, method));
        }
        root.call_deferred("add_child", &[session.to_variant()]);
        self.session = Some(session);
        // WP8: `gdnet/debug/meter` and `gdnet/debug/overlay` show them when a session starts.
        if crate::hash::bool_setting(meter::SETTING_METER, false) {
            self.show_meter(true);
        }
        if crate::hash::bool_setting(meter::SETTING_OVERLAY, false) {
            self.show_overlay(true);
        }
        Error::OK
    }

    fn drop_session(&mut self, mut session: Gd<NetSession>, stop: bool) {
        if let Some(state) = self.state_cell.get() {
            state.clone().bind_mut().attach(None);
        }
        let me = self.to_gd();
        for (signal, method) in RELAYED {
            let cb = Callable::from_object_method(&me, method);
            if session.is_connected(signal, &cb) {
                session.disconnect(signal, &cb);
            }
        }
        if stop {
            session.bind_mut().leave();
        }
        if session.is_inside_tree() {
            session.queue_free();
        } else {
            // Added deferred and not in the tree yet: free it once the pending add_child has run.
            session.call_deferred("queue_free", &[]);
        }
    }
}

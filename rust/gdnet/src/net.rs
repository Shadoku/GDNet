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

use crate::configure::{self, Op};
use crate::emulation::NetEmulation;
use crate::input::NetInput;
use crate::manager::{
    entity_root_of, net_entity_of, NetManager, LOCAL_GROUP, META_OWNER, META_PRIORITY,
};
use crate::pipe_peer::NetPipePeer;
use crate::profiles::{self, NetProfiles};
use crate::session::{tick_rate, NetSession};
use crate::tick;
use godot::classes::node::InternalMode;
use godot::classes::{ENetMultiplayerPeer, Engine, MultiplayerPeer, Node, PackedScene, SceneTree};
use godot::global::Error;
use godot::prelude::*;

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

const RELAYED: [(&str, &str); 9] = [
    ("peer_joined", "_on_peer_joined"),
    ("peer_left", "_on_peer_left"),
    ("connected", "_on_connected"),
    ("disconnected", "_on_disconnected"),
    ("ticked", "_on_ticked"),
    ("entity_spawned", "_on_entity_spawned"),
    ("entity_despawning", "_on_entity_despawning"),
    ("spawn_refused", "_on_spawn_refused"),
    ("rolled_back", "_on_rolled_back"),
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
    /// Snapshot bytes per second per peer (0 = `gdnet/budget/bytes_per_second`, default 32,768).
    #[var(get = get_budget_bytes_per_s, set = set_budget_bytes_per_s)]
    budget_bytes_per_s: i64,
    /// WP4: owners apply the input they sample this many ticks later (and stamp it accordingly), which
    /// trades responsiveness for fewer rollbacks. Default 0; applies to every session at once.
    #[var(get = get_input_delay_ticks, set = set_input_delay_ticks)]
    input_delay_ticks: i64,
    /// WP4, experimental (D10): ramps extra input delay in while an interaction is flagged and back out
    /// afterwards. Default off.
    #[var(get = get_adaptive_delay, set = set_adaptive_delay)]
    adaptive_delay: bool,
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
    fn get_input_delay_ticks(&self) -> i64 {
        i64::from(tick::input_delay())
    }

    #[func]
    fn set_input_delay_ticks(&mut self, v: i64) {
        self.input_delay_ticks = v.clamp(0, 60);
        tick::set_input_delay(self.input_delay_ticks as u32);
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
    /// `quantize: {prop: "cm"}`, `send: {prop: "hot"}`, `tolerance`, `interpolate` change the property
    /// table of the entity's scene and must be made before that scene's first spawn on this peer, the
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
            b.set_emulation(self.emulation.clone());
        }
        let err = session.bind_mut().start(peer, NodePath::default());
        if err != Error::OK {
            session.free();
            return err;
        }
        let me = self.to_gd();
        for (signal, method) in RELAYED {
            session.connect(signal, &Callable::from_object_method(&me, method));
        }
        root.call_deferred("add_child", &[session.to_variant()]);
        self.session = Some(session);
        Error::OK
    }

    fn drop_session(&mut self, mut session: Gd<NetSession>, stop: bool) {
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

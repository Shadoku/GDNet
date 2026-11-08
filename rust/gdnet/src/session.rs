//! `NetSession`: one gdnet session bound to one `MultiplayerAPI` (docs/PLAN_NETCODE.md 2.2, 2.3, WP1).
//!
//! Design: a session is a `Node`. Its `_physics_process` (process priority -1000, so it runs before the
//! game's nodes) is the per-tick loop of 2.2: step 1 drains the packets `SceneMultiplayer.peer_packet`
//! delivered since the last tick into netcore, netcore ticks, and step 6 hands netcore's outgoing
//! packets to `SceneMultiplayer.send_bytes()`. Then the `tick` signal fires. The node must be inside
//! the tree for this to run.
//!
//! `start(peer, root_path)` wraps a `SceneMultiplayer` in a `NetMultiplayerApi` and installs it with
//! `SceneTree.set_multiplayer(api, root_path)`: with an empty path it wraps the tree's default
//! `SceneMultiplayer` (this is what `Net` does); with a path it uses a fresh `SceneMultiplayer` for that
//! branch, which is how one headless process holds a server branch and client branches
//! (`tests/framework/net_fixture.gd`). `leave()` closes the peer and puts back what was there.
//!
//! The `Net` singleton owns one default session; tests and tools create more directly
//! (`NetSession.new()`), one per branch.

use crate::api_ext::NetMultiplayerApi;
use crate::emulation::{settings_of, NetEmulation};
use crate::manager::{NetManager, MANAGERS_GROUP};
use crate::pipe_peer::NetPipePeer;
use crate::poll::{NetPoller, POLLER_NODE};
use crate::profiles;
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::node::InternalMode;
use godot::classes::{
    Engine, INode, MultiplayerApi, MultiplayerPeer, Node, OfflineMultiplayerPeer, SceneMultiplayer,
    SceneTree, Time,
};
use godot::global::Error;
use godot::prelude::*;
use netcore::session::EntityEvent;
use netcore::{EmulSettings, Event, Reliability, Session, SessionConfig, StateUpdate};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Meta on a session's `NetMultiplayerApi`: the session's instance id (how a `NetManager` finds the
/// session of its branch through `Node.get_multiplayer()`).
pub const META_SESSION: &str = "_net_session";

/// Packets every gdnet session of this process handed to a transport (gate G0's probe).
static PACKETS_SENT: AtomicU64 = AtomicU64::new(0);
/// Session generations: each `start()` takes a new one, so managers notice a new session.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// What to put back on `leave()`.
enum Restore {
    /// The default multiplayer of the tree, and the peer its `SceneMultiplayer` had.
    Default {
        api: Gd<MultiplayerApi>,
        peer: Option<Gd<MultiplayerPeer>>,
    },
    /// A branch: the custom multiplayer that was configured for the path, if any.
    Branch {
        path: NodePath,
        previous: Option<Gd<MultiplayerApi>>,
    },
}

/// The callbacks a session connects on the wrapped `SceneMultiplayer`.
const HOOKS: [(&str, &str); 6] = [
    ("peer_packet", "_on_peer_packet"),
    ("peer_connected", "_on_peer_connected"),
    ("peer_disconnected", "_on_peer_disconnected"),
    ("connected_to_server", "_on_connected_to_server"),
    ("connection_failed", "_on_connection_failed"),
    ("server_disconnected", "_on_server_disconnected"),
];

#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct NetSession {
    base: Base<Node>,
    core: Option<Session>,
    api: Option<Gd<NetMultiplayerApi>>,
    inner: Option<Gd<SceneMultiplayer>>,
    peer: Option<Gd<MultiplayerPeer>>,
    restore: Option<Restore>,
    inbox: Vec<(i32, Vec<u8>)>,
    emulation: Option<Gd<NetEmulation>>,
    applied: Option<(EmulSettings, EmulSettings)>,
    /// Test hook (docs/PLAN_NETCODE.md 3.3): any value but 0 changes this side's protocol hash, so the
    /// handshake fails with a reason naming the salt. Set it before `start()`.
    #[var(rename = _debug_protocol_salt, get = get_debug_protocol_salt, set = set_debug_protocol_salt)]
    pub(crate) debug_protocol_salt: i64,
    /// Test hook (WP2): added to every part table hash this side registers, so a spawn fails the scene
    /// check with a reason naming the scene. Set it before the first spawn.
    #[var(rename = _debug_part_table_salt, get = get_debug_part_table_salt, set = set_debug_part_table_salt)]
    pub(crate) debug_part_table_salt: i64,
    generation: u64,
    branch_root: Option<Gd<Node>>,
    /// Entity events for a manager not found yet, with the tick they arrived and whether reported.
    unrouted: Vec<(u64, bool, EntityEvent)>,
    /// WP3: snapshot bytes per second per peer; 0 = `Net.budget_bytes_per_s` / the project setting.
    /// Changes apply at once.
    #[var(get = get_budget_bytes_per_s, set = set_budget_bytes_per_s)]
    budget_bytes_per_s: i64,
    /// WP3: cold properties are polled every this many ticks; 0 = `Net.cold_poll_ticks` / the project
    /// setting (default 6).
    #[var(get = get_cold_poll_ticks, set = set_cold_poll_ticks)]
    cold_poll_ticks: i64,
    /// Test hook (WP3): this side's property tables change the quantiser of every property with this
    /// name, so a spawn fails the property check with a reason naming the scene and property. Set it
    /// before the first spawn.
    #[var(rename = _debug_perturb_property, get = get_debug_perturb_property, set = set_debug_perturb_property)]
    debug_perturb_property: GString,
    poller_added: bool,
    stats: Counters,
}

/// Byte and timing counters behind `stats()` (2.7, the WP8 meter's source).
#[derive(Default, Clone)]
struct Counters {
    sent: [u64; 3],
    received: [u64; 3],
    window_start_sent: [u64; 3],
    window_start_received: [u64; 3],
    sent_per_s: [u64; 3],
    received_per_s: [u64; 3],
    window_ticks: u32,
    poll_us_last: u64,
    poll_us_window: u64,
    poll_ms_avg: f64,
    record_calls: u64,
}

#[godot_api]
impl INode for NetSession {
    fn ready(&mut self) {
        self.base_mut().set_process_priority(-1000);
        self.base_mut().set_physics_process_priority(-1000);
        if !self.poller_added {
            self.poller_added = true;
            let mut poller = NetPoller::new_alloc();
            poller.set_name(POLLER_NODE);
            self.base_mut()
                .add_child_ex(&poller)
                .internal(InternalMode::BACK)
                .done();
        }
    }

    fn physics_process(&mut self, _delta: f64) {
        self.step();
    }

    /// A session whose node leaves the tree ends (as `leave()`): freeing a branch ends its session.
    /// Nodes added to the branch after the session node exit the tree before it, while gdnet's API is
    /// still installed, so they unregister from the API they registered with.
    fn exit_tree(&mut self) {
        self.stop();
    }
}

#[godot_api]
impl NetSession {
    #[func]
    fn get_debug_protocol_salt(&self) -> i64 {
        self.debug_protocol_salt
    }

    #[func]
    fn set_debug_protocol_salt(&mut self, salt: i64) {
        self.debug_protocol_salt = salt;
    }

    #[signal]
    fn peer_joined(id: i64);
    #[signal]
    fn peer_left(id: i64);
    #[signal]
    fn connected();
    #[signal]
    fn disconnected(reason: GString);
    /// Once per physics tick while online (renamed from `tick` in WP2: the method `tick()` shares the
    /// name, so `Net.tick.connect` did not parse).
    #[signal]
    fn ticked(tick: i64);
    /// An entity was spawned in this session's branch (on the authority when announced, on a client
    /// when its replica was added).
    #[signal]
    fn entity_spawned(node: Gd<Node>);
    /// An entity is about to be freed (or its removal was announced).
    #[signal]
    fn entity_despawning(node: Gd<Node>);
    /// A client-side spawn was refused (spawn policy `server_only`); the node is freed.
    #[signal]
    fn spawn_refused(node: Gd<Node>);

    #[func]
    fn get_budget_bytes_per_s(&self) -> i64 {
        self.budget_bytes_per_s
    }

    #[func]
    fn set_budget_bytes_per_s(&mut self, v: i64) {
        self.budget_bytes_per_s = v.max(0);
        let b = self.effective_budget();
        if let Some(c) = self.core.as_mut() {
            c.set_budget(b);
        }
    }

    #[func]
    fn get_cold_poll_ticks(&self) -> i64 {
        self.cold_poll_ticks
    }

    #[func]
    fn set_cold_poll_ticks(&mut self, v: i64) {
        self.cold_poll_ticks = v.max(0);
    }

    #[func]
    fn get_debug_perturb_property(&self) -> GString {
        self.debug_perturb_property.clone()
    }

    #[func]
    fn set_debug_perturb_property(&mut self, v: GString) {
        self.debug_perturb_property = v;
    }

    /// Counters for the meter (WP8) and tests: `bytes_sent_per_s` / `bytes_received_per_s` (per channel,
    /// the last full second), `bytes_sent_total` / `bytes_received_total` (per channel),
    /// `entities`, `poll_ms` (the last record step), `poll_ms_avg` (over the last second),
    /// `budget_bytes_per_s`, `cold_poll_ticks`, and netcore's snapshot counters (`snapshots_sent`,
    /// `snapshot_bytes`, `snapshot_entities_sent`, `snapshot_fields_sent`, `reliable_fields_sent`,
    /// `snapshots_received`, `snapshots_stale`, `snapshots_unacked`, `fields_received`, `fallbacks`,
    /// `changes`).
    #[func]
    pub fn stats(&self) -> VarDictionary {
        let c = &self.stats;
        let arr = |a: &[u64; 3]| PackedInt64Array::from(&a.map(|x| x as i64)[..]);
        let mut d = VarDictionary::new();
        d.set("bytes_sent_per_s", &arr(&c.sent_per_s));
        d.set("bytes_received_per_s", &arr(&c.received_per_s));
        d.set("bytes_sent_total", &arr(&c.sent));
        d.set("bytes_received_total", &arr(&c.received));
        d.set("entities", self.entity_count());
        d.set("poll_ms", c.poll_us_last as f64 / 1000.0);
        d.set("poll_ms_avg", c.poll_ms_avg);
        d.set("budget_bytes_per_s", i64::from(self.effective_budget()));
        d.set("cold_poll_ticks", i64::from(self.effective_cold_poll()));
        if let Some(core) = self.core.as_ref() {
            let r = core.repl_stats();
            d.set("snapshots_sent", r.snapshots_sent as i64);
            d.set("snapshot_bytes", r.snapshot_bytes as i64);
            d.set("snapshot_entities_sent", r.entities_sent as i64);
            d.set("snapshot_fields_sent", r.fields_sent as i64);
            d.set("reliable_fields_sent", r.reliable_fields_sent as i64);
            d.set("snapshots_received", r.snapshots_received as i64);
            d.set("snapshots_stale", r.snapshots_stale as i64);
            d.set("snapshots_unacked", r.snapshots_unacked as i64);
            d.set("fields_received", r.fields_received as i64);
            d.set("fallbacks", r.fallbacks as i64);
            d.set("changes", r.changes as i64);
        }
        d
    }

    #[func]
    fn get_debug_part_table_salt(&self) -> i64 {
        self.debug_part_table_salt
    }

    #[func]
    fn set_debug_part_table_salt(&mut self, salt: i64) {
        self.debug_part_table_salt = salt;
    }

    /// Packets all gdnet sessions of this process have sent (gate G0: 0 while nothing uses gdnet).
    #[func]
    pub fn packets_sent_total() -> i64 {
        PACKETS_SENT.load(Ordering::Relaxed) as i64
    }

    /// Entities in this session's table (every manager of the branch).
    #[func]
    pub fn entity_count(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| c.entities().len() as i64)
    }

    /// Starts a session over `peer`, installing gdnet's `MultiplayerAPI` for `root_path` (empty: the
    /// whole tree). A peer whose unique id is 1 hosts; any other joins. Returns `OK` or an error code.
    #[func]
    pub fn start(&mut self, peer: Gd<MultiplayerPeer>, root_path: NodePath) -> Error {
        if self.core.is_some() {
            self.leave();
        }
        let Some(mut tree) = scene_tree() else {
            godot_error!("NetSession.start: no SceneTree");
            return Error::ERR_UNCONFIGURED;
        };
        let default_api = tree.get_multiplayer();
        let (inner, restore) = if root_path.is_empty() {
            let Some(api) = default_api else {
                return Error::ERR_UNCONFIGURED;
            };
            match api.clone().try_cast::<SceneMultiplayer>() {
                Ok(sm) => {
                    let previous_peer = sm.get_multiplayer_peer();
                    (
                        sm,
                        Restore::Default {
                            api,
                            peer: previous_peer,
                        },
                    )
                }
                Err(_) => (
                    SceneMultiplayer::new_gd(),
                    Restore::Default { api, peer: None },
                ),
            }
        } else {
            let current = tree.get_multiplayer_ex().for_path(&root_path).done();
            let is_default = match (&current, &default_api) {
                (Some(a), Some(b)) => a.instance_id() == b.instance_id(),
                (None, _) => true,
                _ => false,
            };
            if is_default {
                (
                    SceneMultiplayer::new_gd(),
                    Restore::Branch {
                        path: root_path.clone(),
                        previous: None,
                    },
                )
            } else {
                let previous = current.clone();
                let sm = current
                    .and_then(|c| c.try_cast::<SceneMultiplayer>().ok())
                    .unwrap_or_else(SceneMultiplayer::new_gd);
                (
                    sm,
                    Restore::Branch {
                        path: root_path.clone(),
                        previous,
                    },
                )
            }
        };

        let me = self.to_gd();
        let mut inner_mut = inner.clone();
        for (signal, method) in HOOKS {
            let cb = Callable::from_object_method(&me, method);
            if !inner_mut.is_connected(signal, &cb) {
                inner_mut.connect(signal, &cb);
            }
        }
        inner_mut.set_multiplayer_peer(&peer);
        let mut api = NetMultiplayerApi::wrap(inner.clone());
        api.set_meta(META_SESSION, &me.instance_id().to_i64().to_variant());
        self.branch_root = if root_path.is_empty() {
            tree.get_root().map(|r| r.upcast())
        } else {
            tree.get_root().and_then(|r| r.get_node_or_null(&root_path))
        };
        self.generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
        self.unrouted.clear();
        if root_path.is_empty() {
            tree.set_multiplayer(&api.clone().upcast::<MultiplayerApi>());
        } else {
            tree.set_multiplayer_ex(&api.clone().upcast::<MultiplayerApi>())
                .root_path(&root_path)
                .done();
        }

        let mut cfg = SessionConfig::new(tick_rate());
        cfg.protocol_salt = self.debug_protocol_salt as u64;
        cfg.profiles_hash = profiles::global_profiles().hash();
        cfg.budget_bytes_per_s = self.effective_budget();
        let hosting = peer.get_unique_id() == 1;
        let mut core = if hosting {
            Session::server(cfg)
        } else {
            Session::client(cfg)
        };
        let now = now_us();
        if hosting {
            for id in inner.get_peers().as_slice() {
                core.peer_connected(*id as u32, now);
            }
        } else if peer.get_connection_status() == ConnectionStatus::CONNECTED {
            core.peer_connected(1, now);
        }
        self.core = Some(core);
        self.api = Some(api);
        self.inner = Some(inner);
        self.peer = Some(peer);
        self.restore = Some(restore);
        self.inbox.clear();
        self.applied = None;
        self.stats = Counters::default();
        self.apply_emulation();
        self.base_mut().set_physics_process(true);
        Error::OK
    }

    /// Ends the session: closes the peer and puts back the `MultiplayerAPI` (and peer) that was there.
    /// Emits nothing.
    #[func]
    pub fn leave(&mut self) {
        self.stop();
    }

    /// True from `start()` until `leave()` or a disconnection.
    #[func]
    pub fn is_online(&self) -> bool {
        self.core.as_ref().is_some_and(|c| !c.is_closed())
    }

    /// True while hosting (the peer's id is 1).
    #[func]
    pub fn is_server(&self) -> bool {
        self.core
            .as_ref()
            .is_some_and(|c| c.role() == netcore::Role::Server)
    }

    /// This side's peer id: 1 when hosting, the server-assigned id on a client after `connected`, 0
    /// otherwise.
    #[func]
    pub fn unique_id(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| i64::from(c.local_id()))
    }

    /// The current tick (0 when offline). Named `tick` in GDScript; the signal is `ticked`.
    #[func(rename = tick)]
    pub fn current_tick(&self) -> i64 {
        self.core.as_ref().map_or(0, |c| c.current_tick() as i64)
    }

    /// Smoothed round-trip time to `peer` in milliseconds; -1 when unknown.
    #[func]
    pub fn rtt(&self, peer: i64) -> f64 {
        self.core
            .as_ref()
            .and_then(|c| c.rtt_ms(peer as u32))
            .unwrap_or(-1.0)
    }

    /// RTT jitter (mean deviation) to `peer` in milliseconds; -1 when unknown.
    #[func]
    pub fn jitter(&self, peer: i64) -> f64 {
        self.core
            .as_ref()
            .and_then(|c| c.jitter_ms(peer as u32))
            .unwrap_or(-1.0)
    }

    /// The remote peers that finished the handshake, ascending (on a client: `[1]` once connected).
    #[func]
    pub fn peers(&self) -> PackedInt32Array {
        let ids: Vec<i32> = self.core.as_ref().map_or_else(Vec::new, |c| {
            c.joined_peers().into_iter().map(|p| p as i32).collect()
        });
        PackedInt32Array::from(ids.as_slice())
    }

    /// The estimated server tick (fractional); on the server, its own tick.
    #[func]
    pub fn server_tick(&self) -> f64 {
        self.core
            .as_ref()
            .map_or(0.0, |c| c.clock().server_tick_estimate())
    }

    /// Physics ticks per second: the session's tick rate.
    #[func]
    pub fn get_tick_rate(&self) -> i64 {
        self.core
            .as_ref()
            .map_or_else(|| i64::from(tick_rate()), |c| i64::from(c.tick_rate()))
    }

    /// gdnet's `MultiplayerAPI` while online (the one `SceneTree.get_multiplayer()` returns for the
    /// session's branch).
    #[func]
    pub fn get_api(&self) -> Option<Gd<NetMultiplayerApi>> {
        self.api.clone()
    }

    /// The transport peer while online.
    #[func]
    pub fn get_peer(&self) -> Option<Gd<MultiplayerPeer>> {
        self.peer.clone()
    }

    /// Emulation for this session: applied by the `NetPipePeer` when the peer is one, else by netcore
    /// to gdnet's own messages. Null for none.
    #[func]
    pub fn set_emulation(&mut self, emulation: Option<Gd<NetEmulation>>) {
        self.emulation = emulation;
        self.applied = None;
        self.apply_emulation();
    }

    #[func]
    pub fn get_emulation(&self) -> Option<Gd<NetEmulation>> {
        self.emulation.clone()
    }

    #[func]
    fn _on_peer_packet(&mut self, id: i64, packet: PackedByteArray) {
        if self.core.is_some() {
            if let Some(&k) = packet.as_slice().first() {
                let ch = usize::from(netcore::wire::channel_of_kind(k).index()).min(2);
                self.stats.received[ch] += packet.len() as u64;
            }
            self.inbox.push((id as i32, packet.to_vec()));
        }
    }

    #[func]
    fn _on_peer_connected(&mut self, id: i64) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_connected(id as u32, now);
        }
    }

    #[func]
    fn _on_peer_disconnected(&mut self, id: i64) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_disconnected(id as u32, now);
        }
    }

    #[func]
    fn _on_connected_to_server(&mut self) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_connected(1, now);
        }
    }

    #[func]
    fn _on_connection_failed(&mut self) {
        if let Some(c) = self.core.as_mut() {
            c.connection_failed("could not connect to the server");
        }
    }

    #[func]
    fn _on_server_disconnected(&mut self) {
        let now = now_us();
        if let Some(c) = self.core.as_mut() {
            c.peer_disconnected(1, now);
        }
    }
}

impl NetSession {
    /// One physics tick: 2.2 step 1 (drain), netcore's tick, entity actions and received state, then
    /// the signals. Steps 5 and 6 (record, send) run in `post_tick()`, from the `NetPoller` at +1000.
    fn step(&mut self) {
        if self.core.is_none() {
            return;
        }
        self.apply_emulation();
        let now = now_us();
        let inbox = std::mem::take(&mut self.inbox);
        {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            for (peer, bytes) in inbox {
                core.receive(peer as u32, &bytes, now);
            }
            core.tick(now);
        }
        self.sync_managers();
        let (events, tick) = {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            let mut events = Vec::new();
            while let Some(e) = core.poll_event() {
                events.push(e);
            }
            (events, core.current_tick() as i64)
        };
        let mut ended: Option<String> = None;
        for e in events {
            match e {
                Event::PeerJoined(id) => {
                    self.base_mut()
                        .emit_signal("peer_joined", &[i64::from(id).to_variant()]);
                }
                Event::PeerLeft(id) => {
                    self.base_mut()
                        .emit_signal("peer_left", &[i64::from(id).to_variant()]);
                }
                Event::Connected { .. } => {
                    self.base_mut().emit_signal("connected", &[]);
                }
                Event::DisconnectPeer { peer, .. } => {
                    if let Some(mut p) = self.peer.clone() {
                        let _guard = self.base_mut();
                        p.disconnect_peer(peer as i32);
                    }
                }
                Event::Disconnected(reason) => ended = Some(reason),
                Event::EntityError(text) => godot_error!("{text}"),
            }
        }
        if let Some(reason) = ended {
            self.stop();
            self.base_mut().emit_signal(
                "disconnected",
                &[GString::from(reason.as_str()).to_variant()],
            );
            return;
        }
        self.base_mut().emit_signal("ticked", &[tick.to_variant()]);
    }

    /// 2.2 steps 5 and 6, from the `NetPoller` (priority +1000): the server records its managers'
    /// entities (hot properties every tick, cold ones every `cold_poll_ticks`, staggered), then every
    /// side builds its snapshots / acks and hands netcore's packets to the transport.
    pub fn post_tick(&mut self) {
        if self.core.is_none() {
            return;
        }
        let now = now_us();
        if self.is_server() && self.is_connected() {
            let t0 = now_us();
            let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
            let cold = self.effective_cold_poll();
            let managers = self.managers();
            let mut batch = Vec::new();
            {
                let _guard = self.base_mut();
                for (_, mut m) in managers {
                    m.bind_mut().record(tick, cold, &mut batch);
                }
            }
            if let Some(core) = self.core.as_mut() {
                for (id, part, values) in &batch {
                    core.record_part(*id, *part, values);
                }
            }
            let us = now_us().saturating_sub(t0);
            self.stats.poll_us_last = us;
            self.stats.poll_us_window += us;
            self.stats.record_calls += 1;
        }
        let outgoing = match self.core.as_mut() {
            Some(core) => {
                core.send_snapshots(now);
                core.outgoing()
            }
            None => return,
        };
        self.flush(outgoing);
        self.roll_stats_window();
    }

    fn flush(&mut self, outgoing: Vec<netcore::Outgoing>) {
        if let Some(mut inner) = self.inner.clone() {
            let connected = inner
                .get_multiplayer_peer()
                .is_some_and(|p| p.get_connection_status() == ConnectionStatus::CONNECTED);
            if connected {
                let known = inner.get_peers();
                for o in outgoing {
                    let to = o.peer as i32;
                    if !known.as_slice().contains(&to) {
                        continue;
                    }
                    let mode = match o.reliability {
                        Reliability::Reliable => TransferMode::RELIABLE,
                        Reliability::Unreliable => TransferMode::UNRELIABLE,
                    };
                    let bytes = PackedByteArray::from(o.bytes.as_slice());
                    let ch = usize::from(o.channel).min(2);
                    self.stats.sent[ch] += o.bytes.len() as u64;
                    PACKETS_SENT.fetch_add(1, Ordering::Relaxed);
                    inner
                        .send_bytes_ex(&bytes)
                        .id(to)
                        .mode(mode)
                        .channel(i32::from(o.channel))
                        .done();
                }
            }
        }
    }

    fn roll_stats_window(&mut self) {
        let rate = tick_rate();
        let c = &mut self.stats;
        c.window_ticks += 1;
        if c.window_ticks >= rate {
            for i in 0..3 {
                c.sent_per_s[i] = c.sent[i] - c.window_start_sent[i];
                c.received_per_s[i] = c.received[i] - c.window_start_received[i];
            }
            c.window_start_sent = c.sent;
            c.window_start_received = c.received;
            c.poll_ms_avg = c.poll_us_window as f64 / 1000.0 / f64::from(c.window_ticks);
            c.poll_us_window = 0;
            c.window_ticks = 0;
        }
    }

    fn effective_budget(&self) -> u32 {
        if self.budget_bytes_per_s > 0 {
            self.budget_bytes_per_s as u32
        } else {
            profiles::budget_bytes_per_s()
        }
    }

    pub fn effective_cold_poll(&self) -> u32 {
        if self.cold_poll_ticks > 0 {
            self.cold_poll_ticks as u32
        } else {
            profiles::cold_poll_ticks()
        }
    }

    pub fn debug_perturb_property(&self) -> String {
        self.debug_perturb_property.to_string()
    }

    /// The session's generation (a new one per `start()`).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn debug_part_table_salt(&self) -> i64 {
        self.debug_part_table_salt
    }

    /// True while in the session (the server from its first tick, a client from `Welcome`).
    pub fn is_connected(&self) -> bool {
        self.core.as_ref().is_some_and(|c| c.is_connected())
    }

    /// True once the session ended or was never started.
    pub fn is_closed(&self) -> bool {
        self.core.as_ref().is_none_or(|c| c.is_closed())
    }

    /// Runs `f` on the netcore session with the current time; None when there is none. Keep it short:
    /// never call into the engine from `f`.
    pub fn with_core<R>(&mut self, f: impl FnOnce(&mut Session, u64) -> R) -> Option<R> {
        let now = now_us();
        self.core.as_mut().map(|c| f(c, now))
    }

    /// The key of a manager root: a hash of its path relative to this session's branch root, equal on
    /// every peer when the levels match.
    pub fn manager_key(&self, root: &Gd<Node>) -> u64 {
        let rel = self
            .branch_root
            .as_ref()
            .filter(|b| b.is_instance_valid())
            .map(|b| b.get_path_to(root).to_string())
            .unwrap_or_else(|| root.get_path().to_string());
        netcore::entity::fnv1a64(rel.as_bytes())
    }

    /// The managers serving this session's branch, with their keys.
    fn managers(&self) -> Vec<(u64, Gd<NetManager>)> {
        let (Some(tree), Some(api)) = (scene_tree(), self.api.as_ref()) else {
            return Vec::new();
        };
        let api_id = api.instance_id();
        tree.get_nodes_in_group(MANAGERS_GROUP)
            .iter_shared()
            .filter_map(|n| n.try_cast::<NetManager>().ok())
            .filter(|m| {
                m.is_inside_tree()
                    && m.get_multiplayer()
                        .is_some_and(|a| a.instance_id() == api_id)
            })
            .map(|m| {
                let root = m.bind().get_managed_root();
                (self.manager_key(&root), m)
            })
            .collect()
    }

    /// 2.2 between steps 1 and 6 for entities: routes netcore's entity events to the managers by key
    /// and lets every manager apply them and announce its own changes. User code runs inside (spawn
    /// callbacks, `_ready` of replicas), so the base guard is held and core access is re-entrant.
    fn sync_managers(&mut self) {
        let mut events: Vec<(u64, bool, EntityEvent)> = std::mem::take(&mut self.unrouted);
        if let Some(core) = self.core.as_mut() {
            let tick = core.current_tick();
            while let Some(e) = core.poll_entity_event() {
                events.push((tick, false, e));
            }
        }
        let managers = self.managers();
        let updates: Vec<StateUpdate> = self
            .core
            .as_mut()
            .map(|c| c.drain_state_updates())
            .unwrap_or_default();
        if managers.is_empty() && events.is_empty() {
            return;
        }
        let mut routed: HashMap<u64, Vec<EntityEvent>> = HashMap::new();
        let tick = self.core.as_ref().map_or(0, |c| c.current_tick());
        let rate = u64::from(tick_rate());
        let mut keep = Vec::new();
        for (since, reported, e) in events {
            let key = match &e {
                EntityEvent::Create { create, .. } => create.parent.manager,
                EntityEvent::Destroy { record, .. } => record.parent.manager,
                EntityEvent::Reparent { from_manager, .. } => *from_manager,
                EntityEvent::SetOwner { manager, .. } => *manager,
            };
            if managers.iter().any(|(k, _)| *k == key) {
                routed.entry(key).or_default().push(e);
            } else {
                let mut reported = reported;
                if !reported && tick.saturating_sub(since) >= 5 * rate {
                    reported = true;
                    godot_error!(
                        "gdnet: an entity action for a NetManager this peer does not have has waited 5 s (manager key {key:016x}); the managers must sit at the same path in every peer's branch"
                    );
                }
                keep.push((since, reported, e));
            }
        }
        self.unrouted = keep;
        let mut states: HashMap<u64, Vec<StateUpdate>> = HashMap::new();
        if let Some(core) = self.core.as_ref() {
            for u in updates {
                if let Some(r) = core.entities().get(u.id) {
                    states.entry(r.parent.manager).or_default().push(u);
                }
            }
        }
        let me = self.to_gd();
        let _guard = self.base_mut();
        for (key, mut m) in managers {
            let evs = routed.remove(&key).unwrap_or_default();
            let sts = states.remove(&key).unwrap_or_default();
            m.bind_mut().sync(me.clone(), key, evs);
            if !sts.is_empty() {
                m.bind_mut().apply_states(sts);
            }
        }
    }

    fn apply_emulation(&mut self) {
        let (send, receive, seed) = settings_of(&self.emulation);
        if self.applied == Some((send, receive)) {
            return;
        }
        let Some(peer) = self.peer.clone() else {
            return;
        };
        match peer.try_cast::<NetPipePeer>() {
            Ok(mut pipe) => {
                let emu = self.emulation.clone();
                pipe.bind_mut().set_emulation(emu);
                if let Some(c) = self.core.as_mut() {
                    c.set_emulation(EmulSettings::default(), EmulSettings::default(), seed);
                }
            }
            Err(_) => {
                if let Some(c) = self.core.as_mut() {
                    c.set_emulation(send, receive, seed);
                }
            }
        }
        self.applied = Some((send, receive));
    }

    fn stop(&mut self) {
        let Some(_core) = self.core.take() else {
            return;
        };
        let me = self.to_gd();
        if let Some(mut inner) = self.inner.take() {
            for (signal, method) in HOOKS {
                let cb = Callable::from_object_method(&me, method);
                if inner.is_connected(signal, &cb) {
                    inner.disconnect(signal, &cb);
                }
            }
            if let Some(mut tree) = scene_tree() {
                match self.restore.take() {
                    Some(Restore::Default { api, peer }) => {
                        tree.set_multiplayer(&api);
                        let peer =
                            peer.unwrap_or_else(|| OfflineMultiplayerPeer::new_gd().upcast());
                        inner.set_multiplayer_peer(&peer);
                    }
                    Some(Restore::Branch { path, previous }) => {
                        tree.set_multiplayer_ex(previous.as_ref())
                            .root_path(&path)
                            .done();
                    }
                    None => {}
                }
            }
        }
        if let Some(mut api) = self.api.take() {
            NetMultiplayerApi::unwrap_inner(&mut api);
        }
        if let Some(mut peer) = self.peer.take() {
            peer.close();
        }
        self.inbox.clear();
        self.applied = None;
        self.base_mut().set_physics_process(false);
    }
}

fn scene_tree() -> Option<Gd<SceneTree>> {
    Engine::singleton()
        .get_main_loop()
        .and_then(|m| m.try_cast::<SceneTree>().ok())
}

/// The session's tick rate: the engine's physics ticks per second.
pub fn tick_rate() -> u32 {
    Engine::singleton().get_physics_ticks_per_second().max(1) as u32
}

fn now_us() -> u64 {
    Time::singleton().get_ticks_usec()
}

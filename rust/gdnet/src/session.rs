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
use crate::pipe_peer::NetPipePeer;
use godot::classes::multiplayer_peer::{ConnectionStatus, TransferMode};
use godot::classes::{
    Engine, INode, MultiplayerApi, MultiplayerPeer, Node, OfflineMultiplayerPeer, SceneMultiplayer,
    SceneTree, Time,
};
use godot::global::Error;
use godot::prelude::*;
use netcore::{EmulSettings, Event, Reliability, Session, SessionConfig};

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
}

#[godot_api]
impl INode for NetSession {
    fn ready(&mut self) {
        self.base_mut().set_process_priority(-1000);
        self.base_mut().set_physics_process_priority(-1000);
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
    #[signal]
    fn tick(tick: i64);

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
        let api = NetMultiplayerApi::wrap(inner.clone());
        if root_path.is_empty() {
            tree.set_multiplayer(&api.clone().upcast::<MultiplayerApi>());
        } else {
            tree.set_multiplayer_ex(&api.clone().upcast::<MultiplayerApi>())
                .root_path(&root_path)
                .done();
        }

        let mut cfg = SessionConfig::new(tick_rate());
        cfg.protocol_salt = self.debug_protocol_salt as u64;
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

    /// The current tick (0 when offline). Named `tick` in GDScript; the `tick` signal shares the name,
    /// so connect to it with `connect("tick", callable)`.
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
    /// One physics tick: 2.2 steps 1 (drain) and 6 (send), then the signals.
    fn step(&mut self) {
        if self.core.is_none() {
            return;
        }
        self.apply_emulation();
        let now = now_us();
        let inbox = std::mem::take(&mut self.inbox);
        let (outgoing, events, tick) = {
            let Some(core) = self.core.as_mut() else {
                return;
            };
            for (peer, bytes) in inbox {
                core.receive(peer as u32, &bytes, now);
            }
            core.tick(now);
            let out = core.outgoing();
            let mut events = Vec::new();
            while let Some(e) = core.poll_event() {
                events.push(e);
            }
            (out, events, core.current_tick() as i64)
        };
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
                    inner
                        .send_bytes_ex(&bytes)
                        .id(to)
                        .mode(mode)
                        .channel(i32::from(o.channel))
                        .done();
                }
            }
        }
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
        self.base_mut().emit_signal("tick", &[tick.to_variant()]);
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

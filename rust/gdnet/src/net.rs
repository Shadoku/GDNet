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
//! GDScript note: the method `tick()` and the signal `tick` share a name, as the plan specifies; Godot
//! resolves `Net.tick` to the method, so connect to the signal with `Net.connect("tick", f)` or
//! `Signal(Net, "tick")`.

use crate::emulation::NetEmulation;
use crate::pipe_peer::NetPipePeer;
use crate::session::{tick_rate, NetSession};
use godot::classes::{ENetMultiplayerPeer, Engine, MultiplayerPeer, SceneTree};
use godot::global::Error;
use godot::prelude::*;

/// Name of the default session's node under the tree's root.
pub const SESSION_NODE: &str = "__NetSession";

const RELAYED: [(&str, &str); 5] = [
    ("peer_joined", "_on_peer_joined"),
    ("peer_left", "_on_peer_left"),
    ("connected", "_on_connected"),
    ("disconnected", "_on_disconnected"),
    ("tick", "_on_tick"),
];

#[derive(GodotClass)]
#[class(init, base = Object)]
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
    fn _on_tick(&mut self, tick: i64) {
        self.base_mut().emit_signal("tick", &[tick.to_variant()]);
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

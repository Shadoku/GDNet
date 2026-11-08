//! `NetMultiplayerApi`: the `MultiplayerAPIExtension` that gdnet installs around a `SceneMultiplayer`
//! (docs/PLAN_NETCODE.md 2.3, "wrap, do not replace"). Every virtual is forwarded unchanged, so stock
//! `@rpc`, `MultiplayerSpawner` and `MultiplayerSynchronizer` keep working (gate G1); the wrapped
//! object's signals are re-emitted, so `multiplayer.peer_connected` and friends work too. gdnet's own
//! bytes travel beside them through `SceneMultiplayer.send_bytes()` and `peer_packet` (see session.rs).
//!
//! Re-entrancy: forwarding can call back into this object (an RPC whose method reads
//! `multiplayer.get_remote_sender_id()`, a signal re-emitted during `poll()`), so every forward that
//! may run user code holds a `base_mut()` guard, which lets gdext admit the nested call.

use godot::classes::{
    IMultiplayerApiExtension, MultiplayerApiExtension, MultiplayerPeer, SceneMultiplayer,
};
use godot::global::Error;
use godot::prelude::*;

/// The signals of `MultiplayerAPI` that are re-emitted, and the method that re-emits each.
const FORWARDED: [(&str, &str); 5] = [
    ("peer_connected", "_fwd_peer_connected"),
    ("peer_disconnected", "_fwd_peer_disconnected"),
    ("connected_to_server", "_fwd_connected_to_server"),
    ("connection_failed", "_fwd_connection_failed"),
    ("server_disconnected", "_fwd_server_disconnected"),
];

#[derive(GodotClass)]
#[class(init, tool, base = MultiplayerApiExtension)]
pub struct NetMultiplayerApi {
    base: Base<MultiplayerApiExtension>,
    inner: Option<Gd<SceneMultiplayer>>,
}

#[godot_api]
impl NetMultiplayerApi {
    /// The wrapped `SceneMultiplayer`.
    #[func]
    pub fn get_inner(&self) -> Option<Gd<SceneMultiplayer>> {
        self.inner.clone()
    }

    #[func]
    fn _fwd_peer_connected(&mut self, id: i64) {
        self.base_mut()
            .emit_signal("peer_connected", &[id.to_variant()]);
    }

    #[func]
    fn _fwd_peer_disconnected(&mut self, id: i64) {
        self.base_mut()
            .emit_signal("peer_disconnected", &[id.to_variant()]);
    }

    #[func]
    fn _fwd_connected_to_server(&mut self) {
        self.base_mut().emit_signal("connected_to_server", &[]);
    }

    #[func]
    fn _fwd_connection_failed(&mut self) {
        self.base_mut().emit_signal("connection_failed", &[]);
    }

    #[func]
    fn _fwd_server_disconnected(&mut self) {
        self.base_mut().emit_signal("server_disconnected", &[]);
    }
}

impl NetMultiplayerApi {
    /// A wrapper around `inner`, with its signals forwarded.
    pub fn wrap(inner: Gd<SceneMultiplayer>) -> Gd<Self> {
        let mut api = NetMultiplayerApi::new_gd();
        api.bind_mut().inner = Some(inner.clone());
        let mut inner = inner;
        for (signal, method) in FORWARDED {
            let cb = Callable::from_object_method(&api, method);
            if !inner.is_connected(signal, &cb) {
                inner.connect(signal, &cb);
            }
        }
        api
    }

    /// Disconnects the forwarders and drops the wrapped object.
    pub fn unwrap_inner(api: &mut Gd<Self>) {
        let inner = api.bind_mut().inner.take();
        if let Some(mut inner) = inner {
            for (signal, method) in FORWARDED {
                let cb = Callable::from_object_method(api, method);
                if inner.is_connected(signal, &cb) {
                    inner.disconnect(signal, &cb);
                }
            }
        }
    }
}

#[godot_api]
impl IMultiplayerApiExtension for NetMultiplayerApi {
    fn poll(&mut self) -> Error {
        let Some(mut inner) = self.inner.clone() else {
            return Error::OK;
        };
        let _guard = self.base_mut();
        inner.poll()
    }

    fn set_multiplayer_peer(&mut self, peer: Option<Gd<MultiplayerPeer>>) {
        let Some(mut inner) = self.inner.clone() else {
            return;
        };
        let _guard = self.base_mut();
        inner.set_multiplayer_peer(peer.as_ref());
    }

    fn get_multiplayer_peer(&mut self) -> Option<Gd<MultiplayerPeer>> {
        self.inner.as_ref().and_then(|i| i.get_multiplayer_peer())
    }

    fn get_unique_id(&self) -> i32 {
        self.inner.as_ref().map_or(1, |i| i.get_unique_id())
    }

    fn get_peer_ids(&self) -> PackedInt32Array {
        self.inner
            .as_ref()
            .map_or_else(PackedInt32Array::new, |i| i.get_peers())
    }

    fn rpc(
        &mut self,
        peer: i32,
        object: Option<Gd<Object>>,
        method: StringName,
        args: VarArray,
    ) -> Error {
        let Some(mut inner) = self.inner.clone() else {
            return Error::ERR_UNCONFIGURED;
        };
        let any = args.upcast_any_array();
        let _guard = self.base_mut();
        inner
            .rpc_ex(peer, object.as_ref(), &method)
            .arguments(&any)
            .done()
    }

    fn get_remote_sender_id(&self) -> i32 {
        self.inner.as_ref().map_or(0, |i| i.get_remote_sender_id())
    }

    fn object_configuration_add(
        &mut self,
        object: Option<Gd<Object>>,
        configuration: Variant,
    ) -> Error {
        let Some(mut inner) = self.inner.clone() else {
            return Error::ERR_UNCONFIGURED;
        };
        let _guard = self.base_mut();
        inner.object_configuration_add(object.as_ref(), &configuration)
    }

    fn object_configuration_remove(
        &mut self,
        object: Option<Gd<Object>>,
        configuration: Variant,
    ) -> Error {
        let Some(mut inner) = self.inner.clone() else {
            return Error::ERR_UNCONFIGURED;
        };
        let _guard = self.base_mut();
        inner.object_configuration_remove(object.as_ref(), &configuration)
    }
}

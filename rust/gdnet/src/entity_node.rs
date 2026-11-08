//! `NetEntity` (docs/PLAN_NETCODE.md 1 rung 3, 3.1): a `Node` child of an instance root that declares
//! things about its entity. WP2 uses it for two things only: `owner_peer` (the entity's owner, carried by
//! its `Create`) and splitting (a nested instance with a `NetEntity` child is an entity of its own, not a
//! part of the outer one). `net_id` is filled in when the entity is spawned. WP3 adds the property lists
//! and options: `state_properties` (`"Part:prop"` or `"prop"` paths relative to the entity root) with
//! `state_options` (per path: `schedule`, `quantize`, `interpolate`, `tolerance`, `reliable`,
//! `poll_seconds`) replace the inferred profile unless `inherit_profile` is on, which adds them to it;
//! `priority` scales the entity's share of the snapshot budget; `input_properties` is kept for WP4.
//!
//! Scripts that extend `NetEntity` may override `_net_spawned(info)` and `_net_despawning()`; scripts on
//! any other part get the same calls when they define those methods.

use godot::prelude::*;

#[derive(GodotClass)]
#[class(init, base = Node)]
pub struct NetEntity {
    base: Base<Node>,
    /// The peer that owns this entity (1 = the server). Read when the entity is spawned; change it at run
    /// time with `Net.set_owner(node, peer)` on the server.
    #[export]
    #[var(pub)]
    #[init(val = 1)]
    owner_peer: i64,
    /// False (default): `state_properties` replace the inferred profile. True: they are added to it.
    #[export]
    #[var(pub)]
    inherit_profile: bool,
    /// The properties to replicate, as `Part:prop` or `prop` paths relative to the entity root.
    #[export]
    #[var(pub)]
    state_properties: Array<NodePath>,
    /// Options per `state_properties` path (or property name): `{schedule, quantize, interpolate,
    /// tolerance, reliable, poll_seconds}`.
    #[export]
    #[var(pub)]
    state_options: VarDictionary,
    /// Input properties (used from WP4).
    #[export]
    #[var(pub)]
    input_properties: Array<NodePath>,
    /// Scales this entity's share of each peer's snapshot budget (2.7).
    #[export]
    #[var(pub)]
    #[init(val = 1.0)]
    priority: f64,
    /// The entity's net id once spawned (0 before). Read only.
    #[var(get = get_net_id, no_set)]
    net_id: i64,
}

#[godot_api]
impl NetEntity {
    #[func]
    pub fn get_net_id(&self) -> i64 {
        self.net_id
    }

    /// Called on every part of a spawned entity after it entered the tree (on the authority when it is
    /// announced, on a client after the replica was added). `info`: `net_id`, `owner`, `is_owner`,
    /// `is_authority`.
    #[func(virtual)]
    fn net_spawned(&self, _info: VarDictionary) {}

    /// Called on every part of an entity just before it is freed by gdnet (on a client) or when its
    /// despawn is announced (on the authority).
    #[func(virtual)]
    fn net_despawning(&self) {}
}

impl NetEntity {
    pub fn set_net_id(&mut self, id: i64) {
        self.net_id = id;
    }
}

//! `NetEntity` (docs/PLAN_NETCODE.md 1 rung 3, 3.1): a `Node` child of an instance root that declares
//! things about its entity. WP2 uses it for two things only: `owner_peer` (the entity's owner, carried by
//! its `Create`) and splitting (a nested instance with a `NetEntity` child is an entity of its own, not a
//! part of the outer one). `net_id` is filled in when the entity is spawned. WP3 adds the property lists
//! and options.
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

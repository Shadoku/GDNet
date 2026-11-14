//! `NetEntity` (docs/PLAN_NETCODE.md 1 rung 3, 3.1): a `Node` child of an instance root that declares
//! things about its entity. WP2 uses it for two things only: `owner_peer` (the entity's owner, carried by
//! its `Create`) and splitting (a nested instance with a `NetEntity` child is an entity of its own, not a
//! part of the outer one). `net_id` is filled in when the entity is spawned. WP3 adds the property lists
//! and options: `state_properties` (`"Part:prop"` or `"prop"` paths relative to the entity root) with
//! `state_options` (per path: `schedule`, `quantize`, `interpolate`, `tolerance`, `reliable`,
//! `poll_seconds`, WP5 `teleport_distance`) replace the inferred profile unless `inherit_profile` is on, which adds them to it;
//! `priority` scales the entity's share of the snapshot budget; `input_properties` is kept for WP4.
//!
//! WP4: `simulate` (who runs the entity's `_physics_process`), `predict` (the owner's reconciliation
//! policy), `input_actions` (narrows the manager's action list), `input_properties` (properties that
//! travel in the input frame: read on the owner after `_net_input()`, written on the server before the
//! tick) and the signal `rolled_back(from_tick, to_tick)`.
//!
//! WP7: `transfer` (how ownership may move: `fixed`, `request`, `take`), `client_owned` (the owner's state
//! is accepted and relayed: shared authority, D9), `inherit_owner` (a nested entity takes its enclosing
//! entity's owner at creation unless it sets its own) and the signal `owner_changed(old, new)`, emitted on
//! every peer once per change (parts get `_net_owner_changed(old, new)`).
//!
//! WP16: `radius`, `visible_to` with `visible_peers`, and `visibility_filter` (interest.rs; changes on a
//! spawned entity reach the server's session at the end of the frame).
//!
//! Scripts that extend `NetEntity` may override `_net_spawned(info)` and `_net_despawning()`; scripts on
//! any other part get the same calls when they define those methods.

use godot::prelude::*;

/// `NetEntity.simulate`: who runs the entity's `_physics_process` (D13).
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum Simulate {
    /// The server and the owner; replicas only show the server's state.
    #[default]
    OwnerAndServer = 0,
    /// Every peer (cosmetic local simulations).
    Everywhere = 1,
}

/// `NetEntity.predict`: how the owner reconciles its prediction with the server (2.5).
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum Predict {
    /// Roll back and replay when a confirmed value differs beyond its tolerance.
    #[default]
    Check = 0,
    /// Roll back and replay at every confirmed tick (debugging).
    Always = 1,
    /// No prediction: the owner sends its input and shows the server's state.
    Disabled = 2,
}

impl Predict {
    pub fn policy(self) -> netcore::Policy {
        match self {
            Predict::Check => netcore::Policy::Check,
            Predict::Always => netcore::Policy::Always,
            Predict::Disabled => netcore::Policy::Disabled,
        }
    }
}

/// `NetEntity.transfer`: how ownership of the entity may move (2.6).
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum Transfer {
    /// Only the server moves it (`Net.set_owner`).
    #[default]
    Fixed = 0,
    /// A client may ask (`Net.request_owner`); the current owner decides (`_net_grant_owner`).
    Request = 1,
    /// A client may take it at any time.
    Take = 2,
}

impl Transfer {
    pub fn rule(self) -> netcore::Transfer {
        match self {
            Transfer::Fixed => netcore::Transfer::Fixed,
            Transfer::Request => netcore::Transfer::Request,
            Transfer::Take => netcore::Transfer::Take,
        }
    }

    pub fn from_rule(t: netcore::Transfer) -> Self {
        match t {
            netcore::Transfer::Fixed => Transfer::Fixed,
            netcore::Transfer::Request => Transfer::Request,
            netcore::Transfer::Take => Transfer::Take,
        }
    }
}

/// WP16 `NetEntity.visible_to`: which peers may have the entity at all.
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum VisibleTo {
    /// Every peer whose rooms (and radius, filter) include it.
    #[default]
    All = 0,
    /// Only its owner.
    Owner = 1,
    /// Only the peers in `visible_peers` (`Net.show_to`).
    Peers = 2,
}

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
    /// tolerance, reliable, poll_seconds, teleport_distance}`.
    #[export]
    #[var(pub)]
    state_options: VarDictionary,
    /// Properties that travel in the input frame (`prop` or `Part:prop`): read on the owner after
    /// `_net_input()`, written on the server (and in a replay) before the tick.
    #[export]
    #[var(pub)]
    input_properties: Array<NodePath>,
    /// Narrows the manager's `input_actions` for this entity (empty: all of them).
    #[export]
    #[var(pub)]
    input_actions: PackedStringArray,
    /// Who runs `_physics_process`: `owner_and_server` (default) or `everywhere`.
    #[export]
    #[var(pub)]
    simulate: Simulate,
    /// The owner's reconciliation: `check` (default), `always` or `disabled` (no prediction).
    #[export]
    #[var(pub)]
    predict: Predict,
    /// Scales this entity's share of each peer's snapshot budget (2.7).
    #[export]
    #[var(pub)]
    #[init(val = 1.0)]
    priority: f64,
    /// WP7: how ownership may move: `fixed` (default: only the server's `Net.set_owner`), `request`
    /// (`Net.request_owner`; the current owner decides) or `take` (any client may take it).
    #[export]
    #[var(pub)]
    transfer: Transfer,
    /// WP7: the owner simulates the entity and its state is accepted and relayed to everyone (shared
    /// authority: the server trusts that client for this entity; D9). Off by default.
    #[export]
    #[var(pub)]
    client_owned: bool,
    /// WP7: a nested entity takes its enclosing entity's owner at creation (default), unless this is
    /// off or `owner_peer` is set to another value than 1.
    #[export]
    #[var(pub)]
    #[init(val = true)]
    inherit_owner: bool,
    /// WP16: a peer has this entity only while it is within this distance of the peer's focus (its
    /// player, or `Net.set_focus`); it leaves beyond 1.2 times it. Pixels in 2D, metres in 3D. 0: the
    /// manager's `radius`; negative: unlimited. Topmost entities only.
    #[export]
    #[var(get = get_radius, set = set_radius)]
    radius: f64,
    /// WP16: who may have this entity: `all` (default), `owner`, or `peers` (`visible_peers`).
    #[export]
    #[var(get = get_visible_to, set = set_visible_to)]
    visible_to: VisibleTo,
    /// WP16: the peers that may have this entity when `visible_to` is `peers` (`Net.show_to`).
    #[export]
    #[var(get = get_visible_peers, set = set_visible_peers)]
    visible_peers: PackedInt32Array,
    /// WP16: `Callable(peer: int) -> bool`, evaluated on the server every
    /// `gdnet/interest/filter_every_ticks`: whether `peer` has this entity.
    #[var(get = get_visibility_filter, set = set_visibility_filter)]
    #[init(val = Callable::invalid())]
    visibility_filter: Callable,
    /// The entity's net id once spawned (0 before). Read only.
    #[var(get = get_net_id, no_set)]
    net_id: i64,
    /// WP19 (12.4): who simulates this entity's RigidBody parts: `default` (the manager's, else the
    /// project setting `gdnet/physics/mode`, default `server`), `server`, `owner` (the owner simulates,
    /// as `client_owned`), `handoff` (not built yet: behaves as `server`) or `simulated` (every peer
    /// simulates; the server's snapshots correct the clients). Ignored by an entity without a body. Set
    /// on the server at any time: every peer follows at the next tick. On a client it shows the mode
    /// the server chose.
    #[export]
    #[var(get = get_physics, set = set_physics)]
    physics: crate::manager::modes::PhysicsOption,
}

#[godot_api]
impl NetEntity {
    #[func]
    pub fn get_net_id(&self) -> i64 {
        self.net_id
    }

    #[func]
    pub fn get_radius(&self) -> f64 {
        self.radius
    }

    #[func]
    pub fn set_radius(&mut self, r: f64) {
        self.set_radius_quiet(r);
        self.interest_changed();
    }

    #[func]
    pub fn get_visible_to(&self) -> VisibleTo {
        self.visible_to
    }

    #[func]
    pub fn set_visible_to(&mut self, v: VisibleTo) {
        self.set_visible_to_quiet(v);
        self.interest_changed();
    }

    #[func]
    pub fn get_visible_peers(&self) -> PackedInt32Array {
        self.visible_peers.clone()
    }

    #[func]
    pub fn set_visible_peers(&mut self, peers: PackedInt32Array) {
        self.set_visible_peers_quiet(peers);
        self.interest_changed();
    }

    #[func]
    pub fn get_visibility_filter(&self) -> Callable {
        self.visibility_filter.clone()
    }

    #[func]
    pub fn set_visibility_filter(&mut self, f: Callable) {
        self.set_visibility_filter_quiet(f);
        self.interest_changed();
    }

    /// WP16: pushes this entity's interest settings to a running server (deferred from the setters).
    /// `physics`: the manager's, else the project's.
    #[constant]
    const PHYSICS_DEFAULT: i64 = 0;
    #[constant]
    const PHYSICS_SERVER: i64 = 1;
    #[constant]
    const PHYSICS_OWNER: i64 = 2;
    #[constant]
    const PHYSICS_HANDOFF: i64 = 3;
    #[constant]
    const PHYSICS_SIMULATED: i64 = 4;

    #[func]
    pub fn get_physics(&self) -> crate::manager::modes::PhysicsOption {
        self.physics
    }

    #[func]
    pub fn set_physics(&mut self, v: crate::manager::modes::PhysicsOption) {
        self.physics = v;
        if self.net_id != 0 {
            self.to_gd().call_deferred("_net_apply_physics", &[]);
        }
    }

    #[func]
    fn _net_apply_physics(&self) {
        if let Some(root) = self.base().get_parent() {
            crate::manager::modes::apply_live(&root);
        }
    }

    #[func]
    fn _net_apply_interest(&self) {
        if let Some(root) = self.base().get_parent() {
            crate::interest::apply_live(&root);
        }
    }

    /// Called on every part of a spawned entity after it entered the tree (on the authority when it is
    /// announced, on a client after the replica was added). `info`: `net_id`, `owner`, `is_owner`,
    /// `is_authority`.
    #[func(virtual)]
    fn net_spawned(&self, _info: VarDictionary) {}

    /// The owner restored this entity to the server's state at `to_tick` and replayed its ticks up to
    /// `from_tick` (the predicted tick it was at).
    #[signal]
    fn rolled_back(from_tick: i64, to_tick: i64);

    /// WP7: the entity's owner changed from `old` to `new` (on every peer, once per change).
    #[signal]
    fn owner_changed(old: i64, new: i64);

    /// WP7: called on every part (that defines it) when the owner changed from `old` to `new`.
    #[func(virtual)]
    fn net_owner_changed(&self, _old: i64, _new: i64) {}

    /// Called on every part of an entity just before it is freed by gdnet (on a client) or when its
    /// despawn is announced (on the authority).
    #[func(virtual)]
    fn net_despawning(&self) {}
}

impl NetEntity {
    pub fn set_net_id(&mut self, id: i64) {
        self.net_id = id;
    }

    /// WP16: the setters without the live push (the caller pushes).
    pub fn set_physics_quiet(&mut self, v: crate::manager::modes::PhysicsOption) {
        self.physics = v;
    }

    pub fn set_radius_quiet(&mut self, r: f64) {
        self.radius = r;
    }

    pub fn set_visible_to_quiet(&mut self, v: VisibleTo) {
        self.visible_to = v;
    }

    pub fn set_visible_peers_quiet(&mut self, peers: PackedInt32Array) {
        self.visible_peers = peers;
    }

    pub fn set_visibility_filter_quiet(&mut self, f: Callable) {
        self.visibility_filter = f;
    }

    /// A spawned entity's settings changed: push them once this call returns (the push reads this
    /// node again).
    fn interest_changed(&self) {
        if self.net_id != 0 {
            self.to_gd().call_deferred("_net_apply_interest", &[]);
        }
    }
}

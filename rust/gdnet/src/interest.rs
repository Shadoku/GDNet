//! WP16: radius interest and visibility on the Godot side (docs/PLAN_NETCODE.md 12.2 "Radius" and
//! "Visibility"; the rules are netcore's `interest`).
//!
//! What an entity declares, read when it spawns on the server and again whenever it changes:
//!
//! - `radius`: `NetEntity.radius`, the `configure()` key `radius` (stored in the `_net_radius` meta on
//!   an entity without a `NetEntity`), else the manager's `NetManager.radius`. 0 = take the manager's;
//!   a negative value = unlimited even when the manager sets one. Topmost entities only (a nested
//!   entity goes wherever the entity it sits in goes). Units: pixels in 2D, metres in 3D.
//! - `visible_to`: `NetEntity.visible_to` (`all`, `owner`, `peers` with `visible_peers`), the
//!   `configure()` key `visible_to` (`"all"`, `"owner"` or an Array of peer ids) and
//!   `Net.show_to(node, peers)`; without a `NetEntity` they go to the `_net_visible_to` meta.
//! - `visibility_filter`: `NetEntity.visibility_filter` or `Net.set_visibility_filter(node, callable)`
//!   (the `_net_visibility_filter` meta): a `Callable(peer: int) -> bool`.
//!
//! Each server tick, after recording and the interaction pass and before the snapshots,
//! [`pass`] runs two optional steps. The filters are evaluated every
//! `gdnet/interest/filter_every_ticks` ticks (default 6), and at once when a filter is new or the set
//! of peers changed; each result goes to netcore. When some entity has a radius, the roots of the
//! radius-limited entities and every peer's focus (`Net.set_focus` / `NetSession.set_focus`, else its
//! player) are read (`Node2D` / `Node3D` global positions, physics.rs) and handed to netcore, which
//! sends `Create` and `Scope` leaves for the peers whose radius set changed. With no radius and no
//! filter anywhere the pass reads nothing (gate G0).

use crate::entity_node::VisibleTo;
use crate::manager::{entity_root_of, net_entity_of, session_of, NetManager};
use crate::net::Net;
use crate::session::NetSession;
use godot::classes::{Node, Node2D, Node3D, ProjectSettings, Time};
use godot::prelude::*;
use netcore::interest::{Spec, Visible};
use netcore::{NetId, MANAGER_ROOT, SERVER};
use std::collections::{BTreeSet, HashMap};

/// Evaluate every `visibility_filter` this often (server ticks).
pub const SETTING_FILTER_EVERY: &str = "gdnet/interest/filter_every_ticks";
pub const DEFAULT_FILTER_EVERY: u64 = 6;
/// Meta on an entity root without a `NetEntity`: its radius (`configure()`).
pub const META_RADIUS: &str = "_net_radius";
/// Meta on an entity root without a `NetEntity`: `"all"`, `"owner"` or a `PackedInt32Array` of peers.
pub const META_VISIBLE_TO: &str = "_net_visible_to";
/// Meta on an entity root without a `NetEntity`: its `visibility_filter` Callable.
pub const META_FILTER: &str = "_net_visibility_filter";

/// `gdnet/interest/filter_every_ticks`, default 6, at least 1.
pub fn filter_every() -> u64 {
    let ps = ProjectSettings::singleton();
    if !ps.has_setting(SETTING_FILTER_EVERY) {
        return DEFAULT_FILTER_EVERY;
    }
    crate::profiles::number(&ps.get_setting(SETTING_FILTER_EVERY))
        .map_or(DEFAULT_FILTER_EVERY, |n| {
            (n.round() as i64).clamp(1, 3600) as u64
        })
}

/// An entity root or focus node, cast once (the pass reads hundreds of positions per tick).
#[derive(Clone)]
enum Placed {
    D2(Gd<Node2D>),
    D3(Gd<Node3D>),
    /// Neither: it has no position (never inside a radius).
    None(Gd<Node>),
}

impl Placed {
    fn of(node: &Gd<Node>) -> Placed {
        if let Ok(n) = node.clone().try_cast::<Node2D>() {
            return Placed::D2(n);
        }
        if let Ok(n) = node.clone().try_cast::<Node3D>() {
            return Placed::D3(n);
        }
        Placed::None(node.clone())
    }

    fn is_valid(&self) -> bool {
        match self {
            Placed::D2(n) => n.is_instance_valid(),
            Placed::D3(n) => n.is_instance_valid(),
            Placed::None(n) => n.is_instance_valid(),
        }
    }

    fn position(&self) -> Option<[f64; 3]> {
        match self {
            Placed::D2(n) => {
                let p = n.get_global_position();
                Some([f64::from(p.x), f64::from(p.y), 0.0])
            }
            Placed::D3(n) => {
                let p = n.get_global_position();
                Some([f64::from(p.x), f64::from(p.y), f64::from(p.z)])
            }
            Placed::None(_) => None,
        }
    }
}

/// A session's interest state on the Godot side (server).
#[derive(Default)]
pub struct State {
    /// The roots of spawned radius-limited entities.
    radius_roots: HashMap<NetId, Placed>,
    /// Spawned entities with a filter.
    filters: HashMap<NetId, Callable>,
    /// A filter was added since the last evaluation.
    fresh_filter: bool,
    /// `set_focus`: per peer, the node whose position is its focus.
    pub(crate) focus: HashMap<u32, Gd<Node>>,
    /// The joined peers at the last filter evaluation.
    last_peers: Vec<u32>,
    /// Radius passes and filter evaluations run, and the cost of the last pass and of the last
    /// second (window sum in µs, window ticks, last mean in ms).
    passes: u64,
    filter_evaluations: u64,
    pass_us: u64,
    window: (u64, u32, f64),
}

impl State {
    /// Keeps (or drops) what the per-tick pass needs of entity `id`.
    pub fn track(&mut self, id: NetId, root: &Gd<Node>, radius: bool, filter: Option<Callable>) {
        if radius {
            self.radius_roots.insert(id, Placed::of(root));
        } else {
            self.radius_roots.remove(&id);
        }
        match filter {
            Some(f) => {
                if self.filters.insert(id, f).is_none() {
                    self.fresh_filter = true;
                }
            }
            None => {
                self.filters.remove(&id);
            }
        }
    }
}

fn now_us() -> u64 {
    Time::singleton().get_ticks_usec()
}

/// The radius `root` declares: its `NetEntity`'s, else the `_net_radius` meta; 0 = none declared.
fn declared_radius(root: &Gd<Node>) -> f64 {
    if let Some(ne) = net_entity_of(root) {
        let r = ne.bind().get_radius();
        if r != 0.0 {
            return r;
        }
    }
    if root.has_meta(META_RADIUS) {
        return crate::profiles::number(&root.get_meta(META_RADIUS)).unwrap_or(0.0);
    }
    0.0
}

/// The effective radius of an entity: its own, else its manager's; 0 for nested entities and for a
/// negative value.
pub fn radius_of(root: &Gd<Node>, top: bool, manager_radius: f64) -> f64 {
    if !top {
        return 0.0;
    }
    let own = declared_radius(root);
    let r = if own != 0.0 { own } else { manager_radius };
    if r.is_finite() && r > 0.0 {
        r
    } else {
        0.0
    }
}

fn peers_of(list: &PackedInt32Array) -> BTreeSet<u32> {
    list.as_slice()
        .iter()
        .filter(|p| **p > 0)
        .map(|p| *p as u32)
        .collect()
}

/// `visible_to` of an entity root.
pub fn visible_of(root: &Gd<Node>) -> Visible {
    if let Some(ne) = net_entity_of(root) {
        let b = ne.bind();
        return match b.get_visible_to() {
            VisibleTo::All => Visible::All,
            VisibleTo::Owner => Visible::Owner,
            VisibleTo::Peers => Visible::Peers(peers_of(&b.get_visible_peers())),
        };
    }
    if root.has_meta(META_VISIBLE_TO) {
        let v = root.get_meta(META_VISIBLE_TO);
        if let Ok(list) = v.try_to::<PackedInt32Array>() {
            return Visible::Peers(peers_of(&list));
        }
        if v.to_string() == "owner" {
            return Visible::Owner;
        }
    }
    Visible::All
}

/// The `visibility_filter` of an entity root, when it has a valid one.
pub fn filter_of(root: &Gd<Node>) -> Option<Callable> {
    if let Some(ne) = net_entity_of(root) {
        let f = ne.bind().get_visibility_filter();
        if f.is_valid() {
            return Some(f);
        }
    }
    if root.has_meta(META_FILTER) {
        if let Ok(f) = root.get_meta(META_FILTER).try_to::<Callable>() {
            if f.is_valid() {
                return Some(f);
            }
        }
    }
    None
}

/// The spec an entity spawns with, and its filter.
pub fn spec_of(root: &Gd<Node>, top: bool, manager_radius: f64) -> (Spec, Option<Callable>) {
    let filter = filter_of(root);
    let spec = Spec {
        radius: radius_of(root, top, manager_radius),
        visible: visible_of(root),
        filtered: filter.is_some(),
    };
    (spec, filter)
}

/// Writes a `visible_to` value (`"all"`, `"owner"`, an Array / PackedInt32Array of peers) to the
/// entity root: its `NetEntity` or the meta. Err with a readable reason for anything else.
pub fn store_visible_to(root: &Gd<Node>, value: &Variant) -> Result<(), String> {
    let peers = value.try_to::<PackedInt32Array>().ok().or_else(|| {
        value.try_to::<VarArray>().ok().map(|a| {
            a.iter_shared()
                .filter_map(|v| v.try_to::<i64>().ok())
                .map(|p| p as i32)
                .collect()
        })
    });
    let text = value.to_string();
    let (mode, list) = match (peers, text.as_str()) {
        (Some(list), _) => (VisibleTo::Peers, list),
        (None, "all") => (VisibleTo::All, PackedInt32Array::new()),
        (None, "owner") => (VisibleTo::Owner, PackedInt32Array::new()),
        _ => {
            return Err(format!(
                "visible_to must be \"all\", \"owner\" or an Array of peer ids, not {text:?}"
            ))
        }
    };
    let mut root = root.clone();
    match net_entity_of(&root) {
        Some(mut ne) => {
            let mut b = ne.bind_mut();
            b.set_visible_peers_quiet(list);
            b.set_visible_to_quiet(mode);
        }
        None => match mode {
            VisibleTo::Peers => root.set_meta(META_VISIBLE_TO, &list.to_variant()),
            VisibleTo::Owner => root.set_meta(META_VISIBLE_TO, &"owner".to_variant()),
            VisibleTo::All => {
                if root.has_meta(META_VISIBLE_TO) {
                    root.remove_meta(META_VISIBLE_TO);
                }
            }
        },
    }
    Ok(())
}

/// Writes a radius to the entity root (its `NetEntity` or the meta).
pub fn store_radius(root: &Gd<Node>, radius: f64) {
    let mut root = root.clone();
    match net_entity_of(&root) {
        Some(mut ne) => ne.bind_mut().set_radius_quiet(radius),
        None => root.set_meta(META_RADIUS, &radius.to_variant()),
    }
}

/// Writes a filter to the entity root (its `NetEntity` or the meta); an invalid Callable clears it.
pub fn store_filter(root: &Gd<Node>, filter: Callable) {
    let mut root = root.clone();
    match net_entity_of(&root) {
        Some(mut ne) => ne.bind_mut().set_visibility_filter_quiet(filter),
        None => {
            if filter.is_valid() {
                root.set_meta(META_FILTER, &filter.to_variant());
            } else if root.has_meta(META_FILTER) {
                root.remove_meta(META_FILTER);
            }
        }
    }
}

/// The node an interest call is about: the entity root `node` belongs to, else `node` itself (an
/// entity not enrolled yet reads its settings when it spawns).
pub fn target_root(node: &Gd<Node>) -> Gd<Node> {
    entity_root_of(node).map_or_else(|| node.clone(), |(r, _)| r)
}

/// Pushes an entity's current interest settings to its server session (no-op on a client, for an
/// entity not spawned yet, or without a session).
pub fn apply_live(node: &Gd<Node>) {
    let Some((root, manager)) = entity_root_of(node) else {
        return;
    };
    let Some(mut s) = session_of(&root) else {
        return;
    };
    if !s.bind().is_server() {
        return;
    }
    let (id, manager_radius) = {
        let m = manager.bind();
        (m.net_id_of(&root), m.radius_setting())
    };
    if id == 0 {
        return;
    }
    let top = s
        .bind_mut()
        .with_core(|c, _| {
            c.entities()
                .get(id)
                .is_some_and(|r| r.parent.entity == MANAGER_ROOT)
        })
        .unwrap_or(false);
    let (spec, filter) = spec_of(&root, top, manager_radius);
    let mut b = s.bind_mut();
    b.with_core(|c, now| c.set_interest(id, &spec, now));
    b.interest.track(id, &root, spec.radius > 0.0, filter);
}

/// Server, at a spawn (manager.rs): the spec for netcore's `Spawn::with_interest`, and the filter.
pub fn spawn_spec(node: &Gd<Node>, top: bool, manager: &NetManager) -> (Spec, Option<Callable>) {
    spec_of(node, top, manager.radius_setting())
}

/// The per-tick pass (server, after recording and the interaction pass, before the snapshots).
pub fn pass(s: &mut NetSession) {
    if !s.is_server() || !s.is_connected() {
        return;
    }
    let active = s.core.as_ref().is_some_and(|c| c.interest().is_active());
    if s.interest.filters.is_empty() && !active {
        // Gate G0: nothing declared, nothing read.
        return;
    }
    let t0 = now_us();
    let tick = s.core.as_ref().map_or(0, |c| c.current_tick());
    let peers: Vec<u32> = s
        .core
        .as_ref()
        .map(|c| c.joined_peers())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| *p != SERVER)
        .collect();
    if !s.interest.filters.is_empty() {
        let due = s.interest.fresh_filter
            || peers != s.interest.last_peers
            || tick.is_multiple_of(filter_every());
        if due {
            evaluate_filters(s, &peers);
        }
    }
    if active {
        radius_pass(s, &peers);
    }
    let spent = now_us().saturating_sub(t0);
    let st = &mut s.interest;
    st.pass_us = spent;
    st.window.0 += spent;
    st.window.1 += 1;
    let rate = crate::session::tick_rate().max(1);
    if st.window.1 >= rate {
        st.window.2 = st.window.0 as f64 / f64::from(st.window.1) / 1000.0;
        st.window = (0, 0, st.window.2);
    }
}

/// Runs every filter for every client peer and hands the results to netcore. User code runs under
/// the session's base guard (it may call back into gdnet).
fn evaluate_filters(s: &mut NetSession, peers: &[u32]) {
    let live: Vec<(NetId, Callable)> = {
        let core = s.core.as_ref();
        s.interest
            .filters
            .iter()
            .filter(|(id, _)| core.is_some_and(|c| c.entities().contains(**id)))
            .map(|(id, f)| (*id, f.clone()))
            .collect()
    };
    s.interest
        .filters
        .retain(|id, _| live.iter().any(|(l, _)| l == id));
    s.interest.fresh_filter = false;
    s.interest.last_peers = peers.to_vec();
    s.interest.filter_evaluations += 1;
    let mut results: Vec<(NetId, BTreeSet<u32>)> = Vec::with_capacity(live.len());
    {
        let _guard = s.base_mut();
        for (id, f) in &live {
            let admitted: BTreeSet<u32> = peers
                .iter()
                .copied()
                .filter(|p| f.callv(&varray![i64::from(*p)]).booleanize())
                .collect();
            results.push((*id, admitted));
        }
    }
    s.with_core(|c, now| {
        for (id, set) in results {
            c.set_filter_result(id, Some(set), now);
        }
    });
}

/// Reads the radius-limited roots' and the peers' focus positions and hands them to netcore.
fn radius_pass(s: &mut NetSession, peers: &[u32]) {
    s.interest.passes += 1;
    let (ids, players) = match s.core.as_ref() {
        Some(c) => (c.interest().radius_ids(), c.players()),
        None => return,
    };
    // A root not tracked here (an owner_allowed spawn adopted elsewhere, a node replaced) is found
    // through the managers once and then cached.
    let mut managers: Option<Vec<(u64, Gd<NetManager>)>> = None;
    let mut find = |s: &NetSession, id: NetId| -> Option<Placed> {
        let list = managers.get_or_insert_with(|| s.managers());
        list.iter()
            .find_map(|(_, m)| m.bind().root_of_id(id))
            .map(|n| Placed::of(&n))
    };
    let mut positions = Vec::with_capacity(ids.len());
    for id in ids {
        let placed = match s.interest.radius_roots.get(&id) {
            Some(p) if p.is_valid() => Some(p.clone()),
            _ => find(s, id),
        };
        match placed {
            Some(p) => {
                if let Some(pos) = p.position() {
                    positions.push((id, pos));
                }
                s.interest.radius_roots.insert(id, p);
            }
            None => {
                s.interest.radius_roots.remove(&id);
            }
        }
    }
    s.interest.focus.retain(|_, n| n.is_instance_valid());
    let mut focus = Vec::with_capacity(peers.len());
    for &peer in peers {
        let placed = match s.interest.focus.get(&peer) {
            Some(n) => Some(Placed::of(n)),
            None => players
                .iter()
                .find(|(p, _)| *p == peer)
                .and_then(|(_, id)| {
                    s.interest
                        .radius_roots
                        .get(id)
                        .filter(|p| p.is_valid())
                        .cloned()
                        .or_else(|| find(s, *id))
                }),
        };
        if let Some(pos) = placed.and_then(|p| p.position()) {
            focus.push((peer, pos));
        }
    }
    s.with_core(|c, now| c.interest_update(&positions, &focus, now));
}

/// The `stats()` keys of WP16.
pub fn stats_into(s: &NetSession, d: &mut VarDictionary) {
    let st = &s.interest;
    d.set("interest_passes", st.passes as i64);
    d.set("interest_ms", st.pass_us as f64 / 1000.0);
    d.set("interest_ms_avg", st.window.2);
    d.set("filter_evaluations", st.filter_evaluations as i64);
    let (radius, inside) = s.core.as_ref().map_or((0, 0), |c| {
        let i = c.interest();
        (i.radius_ids().len(), i.stats.entered + i.stats.left)
    });
    d.set("radius_entities", radius as i64);
    d.set("radius_changes", inside as i64);
}

#[godot_api(secondary)]
impl NetSession {
    /// WP16, server: `node`'s position is `peer`'s focus for radius interest (a camera, a spectator's
    /// marker) instead of its player's; null goes back to the player.
    #[func]
    pub fn set_focus(&mut self, peer: i64, node: Option<Gd<Node>>) {
        let peer = peer.max(0) as u32;
        match node {
            Some(n) => {
                self.interest.focus.insert(peer, n);
            }
            None => {
                self.interest.focus.remove(&peer);
            }
        }
    }

    /// WP16, server: the node `set_focus` gave for `peer` (null: its player is its focus).
    #[func]
    pub fn get_focus(&self, peer: i64) -> Option<Gd<Node>> {
        self.interest
            .focus
            .get(&(peer.max(0) as u32))
            .filter(|n| n.is_instance_valid())
            .cloned()
    }

    /// WP16, server: the net ids inside `peer`'s radius (radius-limited entities only), ascending.
    #[func]
    pub fn in_radius(&self, peer: i64) -> PackedInt64Array {
        self.core
            .as_ref()
            .map(|c| c.interest().inside(peer.max(0) as u32))
            .unwrap_or_default()
            .into_iter()
            .map(i64::from)
            .collect()
    }
}

#[godot_api(secondary)]
impl Net {
    /// WP16, server: `node`'s position is `peer`'s focus for radius interest (in `node`'s branch;
    /// null clears it on `Net`'s own session).
    #[func]
    fn set_focus(&self, peer: i64, node: Option<Gd<Node>>) {
        let session = match &node {
            Some(n) => session_of(n),
            None => self
                .to_gd()
                .call("get_session", &[])
                .try_to::<Gd<NetSession>>()
                .ok(),
        };
        if let Some(mut s) = session {
            s.bind_mut().set_focus(peer, node);
        }
    }

    /// WP16: only `peers` (and the entity's owner) have the entity `node` belongs to: its
    /// `visible_to` becomes a peer list. Applies at once on a running server.
    #[func]
    fn show_to(&self, node: Gd<Node>, peers: PackedInt32Array) {
        let root = target_root(&node);
        if let Err(e) = store_visible_to(&root, &peers.to_variant()) {
            godot_error!("Net.show_to: {e}");
            return;
        }
        apply_live(&root);
    }

    /// WP16: `filter(peer) -> bool` decides, on the server every `gdnet/interest/filter_every_ticks`,
    /// whether `peer` has the entity `node` belongs to (an invalid Callable removes the filter).
    #[func]
    fn set_visibility_filter(&self, node: Gd<Node>, filter: Callable) {
        let root = target_root(&node);
        store_filter(&root, filter);
        apply_live(&root);
    }
}

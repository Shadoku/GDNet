//! Interest beyond rooms (docs/PLAN_NETCODE.md 2.7, 12.2 "Radius" and "Visibility", WP16): which of
//! the entities in a peer's rooms the peer also gets.
//!
//! Three conditions, all on the server, each one optional per entity:
//!
//! - **radius**: an entity root with a radius is in a peer's scope only while it is within that
//!   radius of the peer's *focus* (a position gdnet reads each server tick: the peer's player, or the
//!   node given with `Net.set_focus`). It enters at the radius and leaves beyond [`LEAVE_FACTOR`] times
//!   it, so an entity at the edge is not created and freed in turn. A peer without a focus position
//!   gets no radius-limited entity (as a peer without a room gets only the `"*"` managers in a strict
//!   session); a spectator is given a focus with `Net.set_focus`. The distance test is
//!   [`priority::near`], WP14's grid, run twice (the enter and the leave radius).
//! - **visible_to** ([`Visible`]): every peer (default), the entity's owner only, or a list of peers.
//! - **filter**: the peers for which the entity's `visibility_filter` last returned true (gdnet
//!   evaluates the Callable every `gdnet/interest/filter_every_ticks` and hands the result here). An
//!   entity with a filter and no result yet is visible to no one.
//!
//! `visible_to` and the filter apply to an entity and to every entity it sits in (a hidden parent
//! hides its children: a replica cannot exist without its parent); the radius applies to topmost
//! entities only. The session adds the rule kept from WP13: an entity whose topmost entity a peer owns
//! is always in that peer's scope, whatever this module says. With no radius, `visible_to` or filter
//! set anywhere, [`Interest::allows`] returns at once and [`Interest::is_active`] is false, so gdnet
//! skips its position pass (gate G0).

use crate::entity::{EntityTable, NetId, MANAGER_ROOT};
use crate::peers::PeerId;
use crate::priority::{self, Body};
use std::collections::{BTreeMap, BTreeSet};
use std::vec::Vec;

/// An entity inside a peer's radius stays in its scope until it is farther than this times the radius.
pub const LEAVE_FACTOR: f64 = 1.2;

/// `visible_to`: who may have the entity at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Visible {
    /// Every peer whose rooms (and radius, filter) include it.
    #[default]
    All,
    /// Only the entity's owner.
    Owner,
    /// Only these peers (`Net.show_to(node, peers)`).
    Peers(BTreeSet<PeerId>),
}

impl Visible {
    fn allows(&self, peer: PeerId, owner: PeerId) -> bool {
        match self {
            Visible::All => true,
            Visible::Owner => peer == owner,
            Visible::Peers(list) => list.contains(&peer),
        }
    }
}

/// An entity's interest settings, given at its spawn (`Spawn::with_interest`) or later.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spec {
    /// 0 or less: no radius (unlimited). Only read for topmost entities.
    pub radius: f64,
    pub visible: Visible,
    /// The entity has a `visibility_filter`: hidden from every peer until its first result.
    pub filtered: bool,
}

impl Spec {
    /// True when the spec changes nothing (the default for every entity).
    pub fn is_default(&self) -> bool {
        self.radius <= 0.0 && self.visible == Visible::All && !self.filtered
    }
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterestStats {
    /// Radius passes run ([`Interest::update`]).
    pub passes: u64,
    /// Times a radius-limited entity entered / left a peer's radius set.
    pub entered: u64,
    pub left: u64,
    /// Filter results that changed a peer's answer.
    pub filter_changes: u64,
}

/// The server's interest state beyond rooms.
#[derive(Clone, Debug, Default)]
pub struct Interest {
    /// Topmost entities with a radius.
    radius: BTreeMap<NetId, f64>,
    /// Their positions, as of the last pass.
    pos: BTreeMap<NetId, [f64; 3]>,
    /// Entities whose `visible_to` is not `All`.
    visible: BTreeMap<NetId, Visible>,
    /// Entities with a filter: the peers its last result admitted.
    filter: BTreeMap<NetId, BTreeSet<PeerId>>,
    /// Each peer's focus position, as of the last pass.
    focus: BTreeMap<PeerId, [f64; 3]>,
    /// Per peer, the radius-limited entities currently inside its radius (with hysteresis).
    inside: BTreeMap<PeerId, BTreeSet<NetId>>,
    pub stats: InterestStats,
}

impl Interest {
    /// True when some entity has a radius: gdnet then reads positions every server tick.
    pub fn is_active(&self) -> bool {
        !self.radius.is_empty()
    }

    /// True when nothing is configured: every entity passes.
    fn is_idle(&self) -> bool {
        self.radius.is_empty() && self.visible.is_empty() && self.filter.is_empty()
    }

    /// The entities with a radius, ascending (gdnet reads their roots' positions).
    pub fn radius_ids(&self) -> Vec<NetId> {
        self.radius.keys().copied().collect()
    }

    pub fn radius_of(&self, id: NetId) -> f64 {
        self.radius.get(&id).copied().unwrap_or(0.0)
    }

    pub fn visible_of(&self, id: NetId) -> Visible {
        self.visible.get(&id).cloned().unwrap_or_default()
    }

    /// The peers `id`'s filter admitted last (None without a filter).
    pub fn filter_of(&self, id: NetId) -> Option<&BTreeSet<PeerId>> {
        self.filter.get(&id)
    }

    /// The focus position of `peer` as of the last pass.
    pub fn focus_of(&self, peer: PeerId) -> Option<[f64; 3]> {
        self.focus.get(&peer).copied()
    }

    /// The radius-limited entities inside `peer`'s radius.
    pub fn inside(&self, peer: PeerId) -> Vec<NetId> {
        self.inside
            .get(&peer)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Applies `spec` to `id`. Returns true when anything changed. A new radius takes effect at the
    /// next [`Interest::update`] (until then the entity is in no peer's radius); a removed filter
    /// admits everyone again; a new filter admits no one until its first result.
    pub fn set(&mut self, id: NetId, spec: &Spec) -> bool {
        let mut changed = self.set_radius(id, spec.radius);
        changed |= self.set_visible(id, spec.visible.clone());
        if spec.filtered != self.filter.contains_key(&id) {
            if spec.filtered {
                self.filter.insert(id, BTreeSet::new());
            } else {
                self.filter.remove(&id);
            }
            changed = true;
        }
        changed
    }

    /// Sets `id`'s radius (0 or less: none). True when it changed.
    pub fn set_radius(&mut self, id: NetId, radius: f64) -> bool {
        let radius = if radius.is_finite() { radius } else { 0.0 };
        let old = self.radius_of(id);
        if radius <= 0.0 {
            if self.radius.remove(&id).is_none() {
                return false;
            }
            self.pos.remove(&id);
            for s in self.inside.values_mut() {
                s.remove(&id);
            }
            return true;
        }
        self.radius.insert(id, radius);
        old != radius
    }

    /// Sets `id`'s `visible_to`. True when it changed.
    pub fn set_visible(&mut self, id: NetId, v: Visible) -> bool {
        let changed = self.visible_of(id) != v;
        if v == Visible::All {
            self.visible.remove(&id);
        } else {
            self.visible.insert(id, v);
        }
        changed
    }

    /// A new result of `id`'s filter: the peers it admits (None removes the filter). Returns the
    /// peers whose answer changed; `all` lists the peers to consider when a filter is added or removed
    /// (everyone else's answer flips with it).
    pub fn set_filter(
        &mut self,
        id: NetId,
        admitted: Option<BTreeSet<PeerId>>,
        all: &[PeerId],
    ) -> Vec<PeerId> {
        let old = self.filter.get(&id).cloned();
        let changed: Vec<PeerId> = match (&old, &admitted) {
            (None, None) => Vec::new(),
            (Some(o), Some(n)) => o.symmetric_difference(n).copied().collect(),
            (Some(o), None) => all.iter().copied().filter(|p| !o.contains(p)).collect(),
            (None, Some(n)) => all.iter().copied().filter(|p| !n.contains(p)).collect(),
        };
        match admitted {
            Some(set) => {
                self.filter.insert(id, set);
            }
            None => {
                self.filter.remove(&id);
            }
        }
        self.stats.filter_changes += changed.len() as u64;
        changed
    }

    /// Forgets a destroyed entity.
    pub fn remove(&mut self, id: NetId) {
        self.radius.remove(&id);
        self.pos.remove(&id);
        self.visible.remove(&id);
        self.filter.remove(&id);
        for s in self.inside.values_mut() {
            s.remove(&id);
        }
    }

    /// Forgets a departed peer's focus and radius set (`visible_to` lists keep it: WP17 moves them to
    /// a rejoining peer).
    pub fn forget_peer(&mut self, peer: PeerId) {
        self.focus.remove(&peer);
        self.inside.remove(&peer);
        for s in self.filter.values_mut() {
            s.remove(&peer);
        }
    }

    /// The radius pass: `positions` are the current positions of radius-limited entity roots (others
    /// are ignored; an entity missing from the list keeps its last position), `focus` every peer's
    /// focus position (a peer missing from it has none). Recomputes each peer's radius set: an
    /// entity enters within its radius and stays until it is farther than [`LEAVE_FACTOR`] times it.
    /// Returns the peers whose set changed, ascending.
    pub fn update(
        &mut self,
        positions: &[(NetId, [f64; 3])],
        focus: &[(PeerId, [f64; 3])],
    ) -> Vec<PeerId> {
        self.stats.passes += 1;
        for (id, p) in positions {
            if self.radius.contains_key(id) && p.iter().all(|v| v.is_finite()) {
                self.pos.insert(*id, *p);
            }
        }
        self.focus = focus
            .iter()
            .filter(|(_, p)| p.iter().all(|v| v.is_finite()))
            .copied()
            .collect();
        // One grid query at the leave radius; an entity not inside yet must also be within the
        // enter radius.
        let bodies: Vec<Body> = self
            .radius
            .iter()
            .filter_map(|(id, r)| {
                self.pos.get(id).map(|p| Body {
                    id: *id,
                    pos: *p,
                    radius: r * LEAVE_FACTOR,
                })
            })
            .collect();
        let players: Vec<(PeerId, NetId, [f64; 3])> = self
            .focus
            .iter()
            .map(|(peer, p)| (*peer, MANAGER_ROOT, *p))
            .collect();
        let stay = priority::near(&players, &bodies);
        let mut next: BTreeMap<PeerId, BTreeSet<NetId>> = BTreeMap::new();
        let mut changed = Vec::new();
        let empty = BTreeSet::new();
        for (i, (peer, _, focus)) in players.iter().enumerate() {
            let old = self.inside.get(peer).unwrap_or(&empty);
            let within_enter = |id: &NetId| {
                let (Some(p), Some(r)) = (self.pos.get(id), self.radius.get(id)) else {
                    return false;
                };
                let d2: f64 = (0..3).map(|k| (p[k] - focus[k]).powi(2)).sum();
                d2 <= r * r
            };
            let set: BTreeSet<NetId> = stay[i]
                .1
                .iter()
                .copied()
                .filter(|id| old.contains(id) || within_enter(id))
                .collect();
            if &set != old {
                self.stats.entered += set.difference(old).count() as u64;
                self.stats.left += old.difference(&set).count() as u64;
                changed.push(*peer);
            }
            next.insert(*peer, set);
        }
        for (peer, old) in &self.inside {
            if !next.contains_key(peer) && !old.is_empty() {
                self.stats.left += old.len() as u64;
                changed.push(*peer);
            }
        }
        self.inside = next;
        changed.sort_unstable();
        changed.dedup();
        changed
    }

    /// Whether `peer` may have entity `id` as far as interest goes: `visible_to` and the filter of
    /// `id` and of every entity it sits in, and the radius of its topmost entity. (The session's own
    /// rules come first: rooms, and an owned topmost entity always being in scope.)
    pub fn allows(&self, peer: PeerId, id: NetId, entities: &EntityTable) -> bool {
        if self.is_idle() {
            return true;
        }
        let mut cur = id;
        let top = loop {
            let Some(r) = entities.get(cur) else {
                return false;
            };
            if self
                .visible
                .get(&cur)
                .is_some_and(|v| !v.allows(peer, r.owner))
            {
                return false;
            }
            if self.filter.get(&cur).is_some_and(|f| !f.contains(&peer)) {
                return false;
            }
            if r.parent.entity == MANAGER_ROOT {
                break cur;
            }
            cur = r.parent.entity;
        };
        !self.radius.contains_key(&top) || self.inside.get(&peer).is_some_and(|s| s.contains(&top))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{CreateAction, ParentRef};

    fn table(entries: &[(NetId, NetId, PeerId)]) -> EntityTable {
        let mut t = EntityTable::default();
        for &(id, parent, owner) in entries {
            t.insert(&CreateAction {
                id,
                parent: ParentRef {
                    manager: 1,
                    entity: parent,
                    part: 0,
                },
                owner,
                name: std::format!("e{id}"),
                parts: 1,
                ..CreateAction::default()
            });
        }
        t
    }

    fn radius(r: f64) -> Spec {
        Spec {
            radius: r,
            ..Spec::default()
        }
    }

    #[test]
    fn nothing_configured_allows_everything() {
        let i = Interest::default();
        let t = table(&[(5, 0, 1)]);
        assert!(!i.is_active());
        assert!(i.allows(2, 5, &t));
        assert!(i.allows(3, 5, &t));
    }

    #[test]
    fn a_radius_enters_at_r_and_leaves_beyond_one_point_two_r() {
        let mut i = Interest::default();
        let t = table(&[(5, 0, 1)]);
        assert!(i.set(5, &radius(100.0)));
        assert!(i.is_active());
        // Before any pass the entity is in no one's radius; without a focus it never is.
        assert!(!i.allows(2, 5, &t));
        assert!(i.update(&[(5, [150.0, 0.0, 0.0])], &[]).is_empty());
        assert!(!i.allows(2, 5, &t));
        // A focus 150 away: outside.
        assert!(i
            .update(&[(5, [150.0, 0.0, 0.0])], &[(2, [0.0, 0.0, 0.0])])
            .is_empty());
        assert!(!i.allows(2, 5, &t));
        // At exactly the radius it enters.
        assert_eq!(i.update(&[(5, [100.0, 0.0, 0.0])], &[(2, [0.0; 3])]), [2]);
        assert!(i.allows(2, 5, &t));
        // Between r and 1.2 r it stays.
        for x in [101.0, 110.0, 119.9, 120.0] {
            assert!(
                i.update(&[(5, [x, 0.0, 0.0])], &[(2, [0.0; 3])]).is_empty(),
                "{x}"
            );
            assert!(i.allows(2, 5, &t), "{x}");
        }
        // Beyond 1.2 r it leaves, and does not come back at 1.1 r.
        assert_eq!(i.update(&[(5, [120.5, 0.0, 0.0])], &[(2, [0.0; 3])]), [2]);
        assert!(!i.allows(2, 5, &t));
        assert!(i
            .update(&[(5, [110.0, 0.0, 0.0])], &[(2, [0.0; 3])])
            .is_empty());
        assert!(!i.allows(2, 5, &t));
        assert_eq!(i.stats.entered, 1);
        assert_eq!(i.stats.left, 1);
    }

    #[test]
    fn the_focus_decides_and_a_lost_focus_drops_the_set() {
        let mut i = Interest::default();
        let t = table(&[(5, 0, 1), (6, 0, 1)]);
        i.set(5, &radius(50.0));
        i.set(6, &radius(50.0));
        let pos = [(5, [0.0, 0.0, 0.0]), (6, [1000.0, 0.0, 0.0])];
        // Peer 2 near entity 5 (its player), peer 3 near entity 6 (a camera given as its focus).
        assert_eq!(
            i.update(&pos, &[(2, [10.0, 0.0, 0.0]), (3, [990.0, 0.0, 0.0])]),
            [2, 3]
        );
        assert!(i.allows(2, 5, &t) && !i.allows(2, 6, &t));
        assert!(i.allows(3, 6, &t) && !i.allows(3, 5, &t));
        // Peer 3's focus moves to entity 5: 6 leaves, 5 enters.
        assert_eq!(i.update(&pos, &[(2, [10.0, 0.0, 0.0]), (3, [0.0; 3])]), [3]);
        assert!(i.allows(3, 5, &t) && !i.allows(3, 6, &t));
        // Peer 2 loses its focus: it holds nothing radius-limited any more.
        assert_eq!(i.update(&pos, &[(3, [0.0; 3])]), [2]);
        assert!(!i.allows(2, 5, &t));
        assert_eq!(i.focus_of(2), None);
        // Entities without a radius are not affected; children follow their topmost entity.
        let t2 = table(&[(5, 0, 1), (7, 0, 1), (8, 5, 1)]);
        assert!(i.allows(2, 7, &t2));
        assert!(!i.allows(2, 8, &t2));
        assert!(i.allows(3, 8, &t2));
        // Removing the radius makes the entity visible again.
        assert!(i.set_radius(5, 0.0));
        assert!(i.allows(2, 5, &t2));
        assert_eq!(i.radius_ids(), [6]);
    }

    #[test]
    fn visible_to_owner_and_a_peer_list() {
        let mut i = Interest::default();
        let t = table(&[(5, 0, 2), (6, 0, 1), (9, 6, 3)]);
        assert!(i.set_visible(5, Visible::Owner));
        assert!(!i.set_visible(5, Visible::Owner));
        assert!(i.allows(2, 5, &t));
        assert!(!i.allows(3, 5, &t));
        i.set_visible(6, Visible::Peers([3, 4].into_iter().collect()));
        assert!(!i.allows(2, 6, &t));
        assert!(i.allows(3, 6, &t) && i.allows(4, 6, &t));
        // A child of a hidden parent is hidden too, whatever its own setting.
        assert!(!i.allows(2, 9, &t));
        assert!(i.allows(3, 9, &t));
        i.set_visible(9, Visible::Owner);
        assert!(!i.allows(4, 9, &t));
        assert!(i.allows(3, 9, &t));
        // Back to all.
        assert!(i.set_visible(6, Visible::All));
        assert!(i.allows(2, 6, &t));
        assert_eq!(i.visible_of(6), Visible::All);
    }

    #[test]
    fn filter_results_gate_and_report_changed_peers() {
        let mut i = Interest::default();
        let t = table(&[(5, 0, 1)]);
        let all = [2, 3, 4];
        // A new filter hides the entity until its first result.
        assert!(i.set(
            5,
            &Spec {
                filtered: true,
                ..Spec::default()
            }
        ));
        assert!(all.iter().all(|p| !i.allows(*p, 5, &t)));
        let changed = i.set_filter(5, Some([2, 3].into_iter().collect()), &all);
        assert_eq!(changed, [2, 3]);
        assert!(i.allows(2, 5, &t) && i.allows(3, 5, &t) && !i.allows(4, 5, &t));
        // The same result changes nothing; a new one names exactly the flipped peers.
        assert!(i
            .set_filter(5, Some([2, 3].into_iter().collect()), &all)
            .is_empty());
        assert_eq!(
            i.set_filter(5, Some([3, 4].into_iter().collect()), &all),
            [2, 4]
        );
        // Removing the filter admits the peers it kept out.
        assert_eq!(i.set_filter(5, None, &all), [2]);
        assert!(all.iter().all(|p| i.allows(*p, 5, &t)));
        // A destroyed entity and a departed peer are forgotten.
        i.set(5, &radius(10.0));
        i.update(&[(5, [0.0; 3])], &[(2, [0.0; 3])]);
        assert_eq!(i.inside(2), [5]);
        i.forget_peer(2);
        assert!(i.inside(2).is_empty());
        i.remove(5);
        assert!(!i.is_active());
    }
}

/// The interest rules through the session: scopes, `Create`s with current state, `Scope` leaves, and
/// `audience: owner` checked in the bytes each peer receives.
#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::entity::{EntityAction, ParentRef};
    use crate::peers::SERVER;
    use crate::props::{Audience, PropDesc, Quant, ScenePropsTable, TypeTag, Value};
    use crate::quant::{encode, BitReader, BitWriter};
    use crate::session::{EntityEvent, Session, SessionConfig, Spawn};
    use crate::wire::{self, Message};
    use std::string::String;

    const SCENE: &str = "res://tests/fixtures/net/card.tscn";
    const TICK_US: u64 = 16_667;
    const SECRET: f64 = 4_242.424_242;
    const CARD: &str = "ACE-OF-SPADES-0001";

    /// position (hot), secret (hot, owner only), card (reliable, owner only), visible.
    fn table() -> ScenePropsTable {
        let mut secret = PropDesc::new("secret", TypeTag::Float).hot(Quant::Raw);
        secret.audience = Audience::Owner;
        let mut card = PropDesc::new("card", TypeTag::String);
        card.reliable = true;
        card.audience = Audience::Owner;
        ScenePropsTable::new(std::vec![std::vec![
            PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
            secret,
            card,
            PropDesc::new("visible", TypeTag::Bool),
        ]])
    }

    fn values(x: f32, secret: f64, card: &str) -> Vec<Vec<Value>> {
        std::vec![std::vec![
            Value::Vector2([x, 0.0]),
            Value::Float(secret),
            Value::Str(String::from(card)),
            Value::Bool(true),
        ]]
    }

    /// A server and clients (by transport id) with an instant transport; every packet the server
    /// sends is kept per peer.
    struct World {
        server: Session,
        clients: Vec<(PeerId, Session)>,
        now: u64,
        sent: BTreeMap<PeerId, Vec<Vec<u8>>>,
    }

    impl World {
        fn new(ids: &[PeerId]) -> Self {
            let mut w = Self {
                server: Session::server(SessionConfig::new(60)),
                clients: ids
                    .iter()
                    .map(|id| (*id, Session::client(SessionConfig::new(60))))
                    .collect(),
                now: 1_000_000,
                sent: BTreeMap::new(),
            };
            for (id, c) in &mut w.clients {
                w.server.peer_connected(*id, w.now);
                c.peer_connected(SERVER, w.now);
            }
            w.steps(3);
            let names = std::vec![String::from(".")];
            w.server.register_scene(SCENE, 42);
            w.server.register_props(SCENE, table(), names.clone());
            for (_, c) in &mut w.clients {
                assert!(c.is_connected());
                c.register_scene(SCENE, 42);
                c.register_props(SCENE, table(), names.clone());
            }
            w
        }

        fn step(&mut self) {
            self.now += TICK_US;
            self.server.tick(self.now);
            self.server.send_snapshots(self.now);
            for (_, c) in &mut self.clients {
                c.tick(self.now);
                c.send_snapshots(self.now);
            }
            let now = self.now;
            for (id, c) in &mut self.clients {
                for o in c.outgoing() {
                    self.server.receive(*id, &o.bytes, now);
                }
            }
            for o in self.server.outgoing() {
                self.sent.entry(o.peer).or_default().push(o.bytes.clone());
                if let Some((_, c)) = self.clients.iter_mut().find(|(id, _)| *id == o.peer) {
                    c.receive(SERVER, &o.bytes, now);
                }
            }
        }

        fn steps(&mut self, n: usize) {
            for _ in 0..n {
                self.step();
            }
        }

        fn client(&mut self, id: PeerId) -> &mut Session {
            &mut self
                .clients
                .iter_mut()
                .find(|(i, _)| *i == id)
                .expect("client")
                .1
        }

        fn spawn(&mut self, owner: PeerId, x: f32, spec: Spec) -> NetId {
            let now = self.now;
            self.server
                .spawn(
                    SCENE,
                    Spawn::new(
                        ParentRef {
                            manager: 1,
                            entity: MANAGER_ROOT,
                            part: 0,
                        },
                        "Card",
                        owner,
                        1,
                        std::vec![],
                    )
                    .with_values(values(x, SECRET, CARD))
                    .with_interest(spec),
                    now,
                )
                .expect("spawn")
        }

        /// The entity events of client `id` since the last call: ("create" | "destroy", id).
        fn events(&mut self, id: PeerId) -> Vec<(&'static str, NetId)> {
            let c = self.client(id);
            let mut out = Vec::new();
            while let Some(e) = c.poll_entity_event() {
                match e {
                    EntityEvent::Create { create, .. } => out.push(("create", create.id)),
                    EntityEvent::Destroy { record, .. } => out.push(("destroy", record.id)),
                    _ => {}
                }
            }
            out
        }

        fn has(&mut self, client: PeerId, id: NetId) -> bool {
            self.client(client).entities().get(id).is_some()
        }

        /// Every (entity, part, field) the server sent `peer` a value of: `Create` initial states,
        /// snapshots and reliable fields.
        fn fields_sent_to(&self, peer: PeerId) -> BTreeSet<(NetId, u16, u16)> {
            let t = table();
            let mut out = BTreeSet::new();
            for bytes in self.sent.get(&peer).into_iter().flatten() {
                match wire::decode(bytes) {
                    Ok(Message::Entity(EntityAction::Create(c))) => {
                        let decoded = self.server.decode_state(c.scene_hash, &c.initial_state);
                        for (p, f, _) in decoded.unwrap_or_default() {
                            out.insert((c.id, p, f));
                        }
                    }
                    Ok(Message::Snapshot(body)) => {
                        let d = crate::snapshot::decode(&body, |_| Some(&t)).expect("snapshot");
                        for e in d.entities {
                            for (p, f, _) in e.fields {
                                out.insert((e.id, p, f));
                            }
                        }
                    }
                    Ok(Message::Field(f)) => {
                        out.insert((f.id, f.part, f.field));
                    }
                    _ => {}
                }
            }
            out
        }

        /// True when the bits of `value` (encoded as field `field` of the table) appear at any bit
        /// offset of any packet the server sent `peer`.
        fn bits_sent_to(&self, peer: PeerId, field: u16, value: &Value) -> bool {
            let d = table().parts[0][usize::from(field)].clone();
            let mut w = BitWriter::new();
            encode(&mut w, d.ty, d.quant, value);
            let n = w.bit_len();
            let needle = w.as_bytes().to_vec();
            self.sent
                .get(&peer)
                .into_iter()
                .flatten()
                .any(|hay| contains_bits(hay, &needle, n))
        }
    }

    fn contains_bits(hay: &[u8], needle: &[u8], n: usize) -> bool {
        let total = hay.len() * 8;
        if n == 0 || n > total {
            return false;
        }
        let chunks = |buf: &[u8], skip: usize| -> Option<Vec<u64>> {
            let mut r = BitReader::new(buf);
            r.skip(skip)?;
            let mut left = n;
            let mut out = Vec::new();
            while left > 0 {
                let k = left.min(64);
                out.push(r.read_bits(k as u32)?);
                left -= k;
            }
            Some(out)
        };
        let want = chunks(needle, 0);
        (0..=total - n).any(|o| chunks(hay, o) == want)
    }

    fn radius(r: f64) -> Spec {
        Spec {
            radius: r,
            ..Spec::default()
        }
    }

    #[test]
    fn beyond_the_radius_an_entity_leaves_and_returns_with_current_state() {
        let mut w = World::new(&[2]);
        let r = w.spawn(SERVER, 0.0, radius(100.0));
        let plain = w.spawn(SERVER, 0.0, Spec::default());
        let mine = w.spawn(2, 0.0, radius(100.0));
        w.step();
        // No focus yet: the radius-limited entity is not sent; the plain one is, and the client's
        // own entity always is.
        assert_eq!(w.events(2), [("create", plain), ("create", mine)]);
        let far = [300.0, 0.0, 0.0];
        w.server.interest_update(
            &[(r, far), (mine, [5000.0, 0.0, 0.0])],
            &[(2, [0.0; 3])],
            w.now,
        );
        w.step();
        assert!(w.events(2).is_empty());
        // The entity moved on the server while away; it enters at the radius with that state.
        w.server
            .record_part(r, 0, &[(0, Value::Vector2([100.0, 0.0]))]);
        w.server
            .interest_update(&[(r, [100.0, 0.0, 0.0])], &[(2, [0.0; 3])], w.now);
        w.step();
        assert_eq!(w.events(2), [("create", r)]);
        // At the edge, between r and 1.2 r: no churn.
        for x in [115.0, 105.0, 119.0, 112.0] {
            w.server
                .interest_update(&[(r, [x, 0.0, 0.0])], &[(2, [0.0; 3])], w.now);
            w.step();
            assert!(w.events(2).is_empty(), "{x}");
        }
        assert_eq!(w.server.scope_len(2), 3);
        // Beyond 1.2 r it leaves; at 1.1 r it stays away; inside r it returns.
        w.server
            .interest_update(&[(r, [125.0, 0.0, 0.0])], &[(2, [0.0; 3])], w.now);
        w.step();
        assert_eq!(w.events(2), [("destroy", r)]);
        assert_eq!(w.server.scope_len(2), 2);
        w.server
            .interest_update(&[(r, [110.0, 0.0, 0.0])], &[(2, [0.0; 3])], w.now);
        w.step();
        assert!(w.events(2).is_empty());
        w.server
            .record_part(r, 0, &[(0, Value::Vector2([-90.0, 0.0]))]);
        w.server
            .interest_update(&[(r, [-90.0, 0.0, 0.0])], &[(2, [0.0; 3])], w.now);
        w.step();
        assert_eq!(w.events(2), [("create", r)]);
        // The second create carried the current position.
        let creates: Vec<Vec<(u16, u16, Value)>> = w.sent[&2]
            .iter()
            .filter_map(|b| match wire::decode(b) {
                Ok(Message::Entity(EntityAction::Create(c))) if c.id == r => {
                    w.server.decode_state(c.scene_hash, &c.initial_state)
                }
                _ => None,
            })
            .collect();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[0][0], (0, 0, Value::Vector2([100.0, 0.0])));
        assert_eq!(creates[1][0], (0, 0, Value::Vector2([-90.0, 0.0])));
        // The owned entity far away never left.
        assert!(w.has(2, mine));
        // A focus elsewhere (a camera): the entity leaves.
        w.server
            .interest_update(&[(r, [-90.0, 0.0, 0.0])], &[(2, [1000.0, 0.0, 0.0])], w.now);
        w.step();
        assert_eq!(w.events(2), [("destroy", r)]);
    }

    #[test]
    fn visible_to_owner_and_a_peer_list_decide_who_has_an_entity() {
        let mut w = World::new(&[2, 3]);
        let hand = w.spawn(
            2,
            0.0,
            Spec {
                visible: Visible::Owner,
                ..Spec::default()
            },
        );
        let shown = w.spawn(
            SERVER,
            0.0,
            Spec {
                visible: Visible::Peers([3].into_iter().collect()),
                ..Spec::default()
            },
        );
        w.steps(2);
        assert!(w.has(2, hand) && !w.has(3, hand));
        assert!(!w.has(2, shown) && w.has(3, shown));
        // show_to moves the server's entity to peer 2.
        w.server
            .set_visible(shown, Visible::Peers([2].into_iter().collect()), w.now);
        w.steps(2);
        assert!(w.has(2, shown) && !w.has(3, shown));
        // The hand changes owner: it moves with its owner.
        w.server.set_owner(hand, 3, w.now);
        w.steps(2);
        assert!(!w.has(2, hand) && w.has(3, hand));
        // Back to everyone.
        w.server.set_visible(hand, Visible::All, w.now);
        w.steps(2);
        assert!(w.has(2, hand) && w.has(3, hand));
    }

    #[test]
    fn an_owner_only_value_reaches_the_owner_and_no_one_else() {
        let mut w = World::new(&[2, 3]);
        let card = w.spawn(2, 0.0, Spec::default());
        w.steps(2);
        assert!(w.has(2, card) && w.has(3, card));
        let secret = (card, 0, 1);
        let name = (card, 0, 2);
        // The Create: the owner's carries both values, the other's neither.
        let to_owner = w.fields_sent_to(2);
        let to_other = w.fields_sent_to(3);
        assert!(
            to_owner.contains(&secret) && to_owner.contains(&name),
            "{to_owner:?}"
        );
        assert!(to_other.contains(&(card, 0, 0)), "{to_other:?}");
        assert!(
            !to_other.contains(&secret) && !to_other.contains(&name),
            "{to_other:?}"
        );
        // A change: a snapshot to the owner, a reliable field to the owner; nothing to the other.
        w.server.record_part(
            card,
            0,
            &[
                (0, Value::Vector2([5.0, 0.0])),
                (1, Value::Float(SECRET * 2.0)),
                (2, Value::Str(String::from("KING-OF-HEARTS-0002"))),
            ],
        );
        w.steps(3);
        let to_other = w.fields_sent_to(3);
        assert!(
            !to_other.contains(&secret) && !to_other.contains(&name),
            "{to_other:?}"
        );
        assert!(w
            .client(3)
            .drain_state_updates()
            .iter()
            .all(|u| u.field == 0 || u.field == 3));
        let ups = w.client(2).drain_state_updates();
        assert!(ups
            .iter()
            .any(|u| u.field == 1 && u.value == Value::Float(SECRET * 2.0)));
        assert!(ups.iter().any(|u| u.field == 2));
        // In the bytes: the owner's packets hold the encodings, the other's never do.
        for (field, v) in [
            (1, Value::Float(SECRET)),
            (1, Value::Float(SECRET * 2.0)),
            (2, Value::Str(String::from(CARD))),
        ] {
            assert!(w.bits_sent_to(2, field, &v), "{v:?} to the owner");
            assert!(!w.bits_sent_to(3, field, &v), "{v:?} to the other client");
        }
        // Hash telemetry ignores the field entirely.
        let h = w.server.replication().entity_hash(card);
        w.server.record_part(card, 0, &[(1, Value::Float(-1.0))]);
        assert_eq!(w.server.replication().entity_hash(card), h);
        // A new owner receives the current value; the old one hears no more of it.
        w.server.set_owner(card, 3, w.now);
        w.steps(3);
        assert!(w
            .client(3)
            .drain_state_updates()
            .iter()
            .any(|u| u.field == 1 && u.value == Value::Float(-1.0)));
        assert!(w.fields_sent_to(3).contains(&name));
        w.client(2).drain_state_updates();
        w.server.record_part(card, 0, &[(1, Value::Float(77.0))]);
        w.steps(3);
        assert!(w
            .client(2)
            .drain_state_updates()
            .iter()
            .all(|u| u.field != 1));
        assert!(!w.bits_sent_to(2, 1, &Value::Float(77.0)));
        assert!(w.bits_sent_to(3, 1, &Value::Float(77.0)));
    }

    #[test]
    fn a_late_joiners_burst_respects_the_rules() {
        let mut w = World::new(&[2]);
        let card = w.spawn(2, 0.0, Spec::default());
        let hand = w.spawn(
            2,
            0.0,
            Spec {
                visible: Visible::Owner,
                ..Spec::default()
            },
        );
        w.steps(2);
        // Peer 3 joins later.
        let mut late = Session::client(SessionConfig::new(60));
        w.server.peer_connected(3, w.now);
        late.peer_connected(SERVER, w.now);
        w.clients.push((3, late));
        w.steps(3);
        w.client(3).register_scene(SCENE, 42);
        w.client(3)
            .register_props(SCENE, table(), std::vec![String::from(".")]);
        w.steps(2);
        assert!(w.has(3, card) && !w.has(3, hand));
        let to_late = w.fields_sent_to(3);
        assert!(!to_late.contains(&(card, 0, 1)) && !to_late.contains(&(card, 0, 2)));
        assert!(!w.bits_sent_to(3, 1, &Value::Float(SECRET)));
    }

    #[test]
    fn a_filter_result_gates_the_scope() {
        let mut w = World::new(&[2, 3]);
        let fog = w.spawn(
            SERVER,
            0.0,
            Spec {
                filtered: true,
                ..Spec::default()
            },
        );
        w.steps(2);
        // No result yet: hidden from both.
        assert!(!w.has(2, fog) && !w.has(3, fog));
        w.server
            .set_filter_result(fog, Some([3].into_iter().collect()), w.now);
        w.steps(2);
        assert!(!w.has(2, fog) && w.has(3, fog));
        w.server
            .set_filter_result(fog, Some([2].into_iter().collect()), w.now);
        w.steps(2);
        assert!(w.has(2, fog) && !w.has(3, fog));
        assert_eq!(w.events(3), [("create", fog), ("destroy", fog)]);
        // The owner always has its entity, whatever the filter says.
        w.server.set_owner(fog, 3, w.now);
        w.steps(2);
        assert!(w.has(3, fog));
        // No filter: everyone.
        w.server.set_filter_result(fog, None, w.now);
        w.steps(2);
        assert!(w.has(2, fog) && w.has(3, fog));
    }
}

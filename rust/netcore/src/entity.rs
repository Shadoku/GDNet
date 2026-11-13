//! Entities (docs/PLAN_NETCODE.md 2.1 "Identity" and "Spawn", 2.4 `EntityAction`, WP2).
//!
//! An entity is a scene instance (or a bare node) under a `NetManager`. It has a `u32` net id taken from
//! the id range of the peer that created it, a scene (a 64-bit hash of its resource path, or of its class
//! name for a bare node), a parent (another entity's part, or the root of a manager, `parent_entity ==
//! 0`), a name, an owner peer and a part count. A part is (entity id, part index); part indices come from
//! a depth-first walk of the scene, which gdnet computes and hashes into a [`PartTable`] so peers can
//! compare. No node path travels.
//!
//! This module is the pure data: the [`EntityTable`], the [`IdAllocator`], the [`HeldQueue`] of actions
//! waiting for an entity they name, and the hashes. The session (session.rs) wires them to messages.

use crate::peers::PeerId;
use crate::wire::IdRange;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::string::String;
use std::vec::Vec;

/// A net id. 0 is never an entity: as a parent it means "the root of the manager".
pub type NetId = u32;

/// The parent id that means the manager's root node.
pub const MANAGER_ROOT: NetId = 0;

/// FNV-1a 64 of `bytes`. Stable across platforms and versions: it is part of the protocol.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// The scene hash of a resource path (`res://enemies/slime.tscn`) or, for a bare node, of its class name
/// (`Sprite2D`). Both peers must have the scene at the same path.
pub fn scene_hash(path: &str) -> u64 {
    fnv1a64(path.as_bytes())
}

/// The parts of a scene in depth-first order: each entry is (path relative to the instance root, class
/// name). Entry 0 is the root itself (`"."`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartTable {
    pub entries: Vec<(String, String)>,
}

impl PartTable {
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self { entries }
    }

    /// FNV-1a 64 over every path and class, each terminated by a zero byte, in order.
    pub fn hash(&self) -> u64 {
        let mut bytes = Vec::new();
        for (path, class) in &self.entries {
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(class.as_bytes());
            bytes.push(0);
        }
        fnv1a64(&bytes)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The index of the part at `path`.
    pub fn index_of(&self, path: &str) -> Option<u16> {
        self.entries
            .iter()
            .position(|(p, _)| p == path)
            .and_then(|i| u16::try_from(i).ok())
    }
}

/// Where an entity hangs: a manager (by key) and, inside it, the root or a part of another entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ParentRef {
    /// The manager's key: a hash of its path relative to the session's branch root (gdnet).
    pub manager: u64,
    /// The parent entity, or [`MANAGER_ROOT`].
    pub entity: NetId,
    /// The part of the parent entity (0 for the manager root).
    pub part: u16,
}

/// One entity as every peer knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityRecord {
    pub id: NetId,
    pub scene: u64,
    pub parent: ParentRef,
    pub name: String,
    pub owner: PeerId,
    pub parts: u16,
    /// Creation order on this peer (the burst is sent parents first, then by this).
    pub seq: u64,
    /// WP7: how ownership may move, and whether the owner's state is accepted (2.6).
    pub transfer: crate::authority::Transfer,
    pub client_owned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CreateAction {
    pub id: NetId,
    pub scene_hash: u64,
    pub parent: ParentRef,
    pub name: String,
    pub owner: PeerId,
    pub parts: u16,
    /// The initial state of every part, opaque to the entity layer (empty until WP3 fills it).
    pub initial_state: Vec<u8>,
    /// WP7: the entity's transfer rule.
    pub transfer: crate::authority::Transfer,
    /// WP7: the owner's state is accepted and relayed (shared authority, D9).
    pub client_owned: bool,
}

/// `EntityAction` (2.4): one reliable ordered message per action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityAction {
    /// The sender's scene `path` has part table hash `parts_hash`; sent once per peer before the first
    /// `Create` that uses the scene, so the receiver can resolve the hash and compare part tables.
    Scene {
        hash: u64,
        path: String,
        parts_hash: u64,
        /// WP3: the sender's property table for the scene (compared with the receiver's; a difference
        /// names the scene, part and property). Empty when the sender registered none.
        props: crate::props::ScenePropsTable,
    },
    Create(CreateAction),
    Destroy {
        id: NetId,
    },
    Reparent {
        id: NetId,
        parent: ParentRef,
    },
    SetOwner {
        id: NetId,
        owner: PeerId,
    },
    /// WP7, client to server: the sender asks to own `id` (its transfer rule decides).
    RequestOwner {
        id: NetId,
    },
    /// WP7, client to server: the owner gives `id` back to the server.
    ReleaseOwner {
        id: NetId,
    },
    /// WP7, server to the owner of a `request` entity: may `requester` have it?
    OwnerQuery {
        id: NetId,
        requester: PeerId,
    },
    /// WP7, the owner's answer to an `OwnerQuery`.
    OwnerAnswer {
        id: NetId,
        requester: PeerId,
        grant: bool,
    },
    /// WP7, server to a requester: the outcome of its `RequestOwner` (after the `SetOwner` of a grant).
    OwnerReply {
        id: NetId,
        granted: bool,
    },
    /// WP13/WP16, server to a client: `room`, a level, joined the client's set of rooms: load
    /// `scene_path` (under the game's World node, its root at `origin`, else where it was authored)
    /// before the actions that follow, then answer `TravelAck`.
    Travel {
        scene_path: String,
        room: String,
        origin: Option<crate::rooms::Origin>,
        /// WP16 (level state, wire format 4): paths relative to the level root of the level's own
        /// entities that are gone on the server; the client frees its copies.
        gone: Vec<String>,
    },
    /// WP16, server to a client: `room`, a level, left the client's set of rooms (after the `Scope`
    /// leave of its entities): the client frees that level once nothing it owns is in it.
    RoomLeft {
        room: String,
    },
    /// WP13, client to server: the level of `room` (its hash) is loaded.
    TravelAck {
        room: u64,
    },
    /// WP13, server to a client: these entities left the client's scope (its room changed, or they
    /// moved to another room); the client forgets them and frees their replicas.
    Scope {
        leave: Vec<NetId>,
    },
    /// WP6: a received call (it travels as its own message, kind 11, never as an `EntityAction`). It is
    /// kept here so a call waits behind its entity's pending `Create` in the same held queue, in order.
    #[serde(skip)]
    Call(crate::call::CallMsg),
}

impl EntityAction {
    /// The entity this action creates or targets.
    pub fn subject(&self) -> Option<NetId> {
        match self {
            EntityAction::Scene { .. }
            | EntityAction::Travel { .. }
            | EntityAction::TravelAck { .. }
            | EntityAction::RoomLeft { .. }
            | EntityAction::Scope { .. } => None,
            EntityAction::Create(c) => Some(c.id),
            EntityAction::Destroy { id }
            | EntityAction::SetOwner { id, .. }
            | EntityAction::Reparent { id, .. }
            | EntityAction::RequestOwner { id }
            | EntityAction::ReleaseOwner { id }
            | EntityAction::OwnerQuery { id, .. }
            | EntityAction::OwnerAnswer { id, .. }
            | EntityAction::OwnerReply { id, .. } => Some(*id),
            EntityAction::Call(c) => Some(c.entity),
        }
    }

    /// Entity ids this action needs to exist before it can apply (the target and/or the parent).
    pub fn needs(&self) -> (Option<NetId>, Option<NetId>) {
        let parent = |p: &ParentRef| (p.entity != MANAGER_ROOT).then_some(p.entity);
        match self {
            EntityAction::Scene { .. }
            | EntityAction::Travel { .. }
            | EntityAction::TravelAck { .. }
            | EntityAction::RoomLeft { .. }
            | EntityAction::Scope { .. } => (None, None),
            EntityAction::Create(c) => (None, parent(&c.parent)),
            EntityAction::Destroy { id }
            | EntityAction::SetOwner { id, .. }
            | EntityAction::RequestOwner { id }
            | EntityAction::ReleaseOwner { id }
            | EntityAction::OwnerQuery { id, .. }
            | EntityAction::OwnerAnswer { id, .. }
            | EntityAction::OwnerReply { id, .. } => (Some(*id), None),
            EntityAction::Call(c) => (Some(c.entity), None),
            EntityAction::Reparent { id, parent: p } => (Some(*id), parent(p)),
        }
    }
}

/// Every entity this peer knows.
#[derive(Clone, Debug, Default)]
pub struct EntityTable {
    records: BTreeMap<NetId, EntityRecord>,
    seq: u64,
}

impl EntityTable {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn get(&self, id: NetId) -> Option<&EntityRecord> {
        self.records.get(&id)
    }

    pub fn contains(&self, id: NetId) -> bool {
        self.records.contains_key(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &EntityRecord> {
        self.records.values()
    }

    /// Inserts a record from a `Create`. False when the id is taken, is 0, or the parent is unknown.
    pub fn insert(&mut self, c: &CreateAction) -> bool {
        if c.id == MANAGER_ROOT || self.records.contains_key(&c.id) {
            return false;
        }
        if c.parent.entity != MANAGER_ROOT && !self.records.contains_key(&c.parent.entity) {
            return false;
        }
        self.seq += 1;
        self.records.insert(
            c.id,
            EntityRecord {
                id: c.id,
                scene: c.scene_hash,
                parent: c.parent,
                name: c.name.clone(),
                owner: c.owner,
                parts: c.parts,
                seq: self.seq,
                transfer: c.transfer,
                client_owned: c.client_owned,
            },
        );
        true
    }

    /// Removes `id` and every entity below it. Returns the removed records, children before their
    /// parents, the destroyed entity last. Empty when `id` is unknown.
    pub fn remove(&mut self, id: NetId) -> Vec<EntityRecord> {
        if !self.records.contains_key(&id) {
            return Vec::new();
        }
        let mut order = Vec::new();
        self.post_order(id, &mut order);
        order
            .into_iter()
            .filter_map(|i| self.records.remove(&i))
            .collect()
    }

    fn children_of(&self, id: NetId) -> Vec<NetId> {
        let mut c: Vec<&EntityRecord> = self
            .records
            .values()
            .filter(|r| r.parent.entity == id && r.id != id)
            .collect();
        c.sort_by_key(|r| r.seq);
        c.into_iter().map(|r| r.id).collect()
    }

    fn post_order(&self, id: NetId, out: &mut Vec<NetId>) {
        for child in self.children_of(id) {
            self.post_order(child, out);
        }
        out.push(id);
    }

    /// True when `ancestor` is `id` or above it.
    pub fn is_ancestor(&self, ancestor: NetId, id: NetId) -> bool {
        let mut cur = Some(id);
        let mut steps = 0;
        while let Some(c) = cur {
            if c == ancestor {
                return true;
            }
            steps += 1;
            if steps > self.records.len() + 1 {
                return false;
            }
            cur = self
                .records
                .get(&c)
                .map(|r| r.parent.entity)
                .filter(|p| *p != MANAGER_ROOT);
        }
        false
    }

    /// Moves `id` under `parent`. False for an unknown entity or parent, or a move under itself. A
    /// move to another manager (WP13) moves every entity below `id` to that manager too.
    pub fn reparent(&mut self, id: NetId, parent: ParentRef) -> bool {
        let Some(old) = self.records.get(&id).map(|r| r.parent.manager) else {
            return false;
        };
        if parent.entity != MANAGER_ROOT
            && (!self.records.contains_key(&parent.entity) || self.is_ancestor(id, parent.entity))
        {
            return false;
        }
        if old != parent.manager {
            for d in self.subtree(id) {
                if let Some(r) = self.records.get_mut(&d) {
                    r.parent.manager = parent.manager;
                }
            }
        }
        if let Some(r) = self.records.get_mut(&id) {
            r.parent = parent;
        }
        true
    }

    /// `id` and every entity below it, children before their parents (`id` last); empty when unknown.
    pub fn subtree(&self, id: NetId) -> Vec<NetId> {
        if !self.records.contains_key(&id) {
            return Vec::new();
        }
        let mut order = Vec::new();
        self.post_order(id, &mut order);
        order
    }

    /// The topmost entity above `id` (itself when it hangs at a manager root).
    pub fn root_of(&self, id: NetId) -> Option<&EntityRecord> {
        let mut cur = self.records.get(&id)?;
        let mut steps = 0;
        while cur.parent.entity != MANAGER_ROOT {
            steps += 1;
            match self.records.get(&cur.parent.entity) {
                Some(p) if steps <= self.records.len() => cur = p,
                _ => break,
            }
        }
        Some(cur)
    }

    pub fn set_owner(&mut self, id: NetId, owner: PeerId) -> bool {
        match self.records.get_mut(&id) {
            Some(r) => {
                r.owner = owner;
                true
            }
            None => false,
        }
    }

    /// The table as a sequence of creates a late joiner can apply in order: every parent before its
    /// children, siblings in creation order.
    pub fn burst(&self) -> Vec<CreateAction> {
        let mut out = Vec::with_capacity(self.records.len());
        let mut roots: Vec<&EntityRecord> = self
            .records
            .values()
            .filter(|r| r.parent.entity == MANAGER_ROOT)
            .collect();
        roots.sort_by_key(|r| r.seq);
        let mut stack: Vec<NetId> = roots.into_iter().rev().map(|r| r.id).collect();
        while let Some(id) = stack.pop() {
            if let Some(r) = self.records.get(&id) {
                out.push(CreateAction {
                    id: r.id,
                    scene_hash: r.scene,
                    parent: r.parent,
                    name: r.name.clone(),
                    owner: r.owner,
                    parts: r.parts,
                    initial_state: Vec::new(),
                    transfer: r.transfer,
                    client_owned: r.client_owned,
                });
            }
            for child in self.children_of(id).into_iter().rev() {
                stack.push(child);
            }
        }
        out
    }
}

/// Hands out ids from one peer's range, never 0 and never reusing an id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdAllocator {
    range: IdRange,
    next: u32,
}

impl IdAllocator {
    pub fn new(range: IdRange) -> Self {
        Self {
            range,
            next: range.start.max(1),
        }
    }

    pub fn range(&self) -> IdRange {
        self.range
    }

    /// The next free id, or `None` when the range is used up.
    pub fn allocate(&mut self) -> Option<NetId> {
        let end = u64::from(self.range.start) + u64::from(self.range.len);
        if u64::from(self.next) >= end {
            return None;
        }
        let id = self.next;
        self.next += 1;
        Some(id)
    }

    /// True when `id` lies in `range`.
    pub fn in_range(range: IdRange, id: NetId) -> bool {
        id >= range.start && u64::from(id) < u64::from(range.start) + u64::from(range.len)
    }
}

/// An action waiting for an entity it names (a `Create` whose parent has not arrived, or anything that
/// targets such an entity). Kept in arrival order and applied once unblocked; never dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    pub from: PeerId,
    pub action: EntityAction,
    /// The local tick when it was held.
    pub since: u64,
    /// True once the "still waiting" error has been reported.
    pub reported: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HeldQueue {
    items: VecDeque<Held>,
}

impl HeldQueue {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push(&mut self, from: PeerId, action: EntityAction, since: u64) {
        self.items.push_back(Held {
            from,
            action,
            since,
            reported: false,
        });
    }

    /// Ids that held creates will bring (so later actions naming them wait too).
    pub fn expected(&self) -> BTreeSet<NetId> {
        self.items
            .iter()
            .filter_map(|h| match &h.action {
                EntityAction::Create(c) => Some(c.id),
                _ => None,
            })
            .collect()
    }

    /// True when a held action creates or targets `id` (so a later action naming `id` must wait behind
    /// it to keep the order per entity).
    pub fn names(&self, id: NetId) -> bool {
        self.items.iter().any(|h| h.action.subject() == Some(id))
    }

    /// Takes the first held action that `ready` admits, scanning in order. An action is only admitted
    /// when no earlier held action names the same entities (so the order per entity is kept).
    pub fn take_ready(&mut self, ready: impl Fn(&EntityAction) -> bool) -> Option<Held> {
        let mut blocked: BTreeSet<NetId> = BTreeSet::new();
        for i in 0..self.items.len() {
            let a = &self.items[i].action;
            let (_, parent) = a.needs();
            let own = a.subject();
            let names_blocked = [own, parent]
                .iter()
                .flatten()
                .any(|id| blocked.contains(id));
            if !names_blocked && ready(a) {
                return self.items.remove(i);
            }
            blocked.extend(own);
        }
        None
    }

    /// Held actions older than `ticks` that were not reported yet; marks them reported.
    pub fn overdue(&mut self, now: u64, ticks: u64) -> Vec<Held> {
        let mut out = Vec::new();
        for h in self.items.iter_mut() {
            if !h.reported && now.saturating_sub(h.since) >= ticks {
                h.reported = true;
                out.push(h.clone());
            }
        }
        out
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(id: NetId, parent: NetId) -> CreateAction {
        CreateAction {
            id,
            scene_hash: scene_hash("res://a.tscn"),
            parent: ParentRef {
                manager: 7,
                entity: parent,
                part: 0,
            },
            name: std::format!("E{id}"),
            owner: 1,
            parts: 3,
            initial_state: Vec::new(),
            ..CreateAction::default()
        }
    }

    #[test]
    fn hashes_are_stable() {
        // Pinned values: these are protocol constants.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(scene_hash("res://a.tscn"), scene_hash("res://a.tscn"));
        assert_ne!(scene_hash("res://a.tscn"), scene_hash("res://b.tscn"));
    }

    #[test]
    fn part_table_hash_is_deterministic_and_sensitive() {
        let t = |e: &[(&str, &str)]| {
            PartTable::new(e.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect())
        };
        let a = t(&[(".", "Node2D"), ("Sprite", "Sprite2D"), ("Area", "Area2D")]);
        let b = t(&[(".", "Node2D"), ("Sprite", "Sprite2D"), ("Area", "Area2D")]);
        assert_eq!(a.hash(), b.hash());
        assert_eq!(a.index_of("Area"), Some(2));
        assert_eq!(a.index_of("Nope"), None);
        // Order, names and classes all count; the terminators keep "ab"+"c" apart from "a"+"bc".
        let reordered = t(&[(".", "Node2D"), ("Area", "Area2D"), ("Sprite", "Sprite2D")]);
        let renamed = t(&[(".", "Node2D"), ("Sprite", "Sprite2D"), ("Zone", "Area2D")]);
        let retyped = t(&[(".", "Node2D"), ("Sprite", "Sprite3D"), ("Area", "Area2D")]);
        let split1 = t(&[("ab", "c")]);
        let split2 = t(&[("a", "bc")]);
        for other in [&reordered, &renamed, &retyped] {
            assert_ne!(a.hash(), other.hash());
        }
        assert_ne!(split1.hash(), split2.hash());
    }

    #[test]
    fn ranges_never_collide() {
        let ranges: Vec<IdRange> = (0..8u32)
            .map(|slot| IdRange {
                start: slot * 1000,
                len: 1000,
            })
            .collect();
        let mut seen = BTreeSet::new();
        for r in &ranges {
            let mut a = IdAllocator::new(*r);
            while let Some(id) = a.allocate() {
                assert_ne!(id, MANAGER_ROOT);
                assert!(IdAllocator::in_range(*r, id));
                assert!(seen.insert(id), "id {id} handed out twice");
                for other in ranges.iter().filter(|o| *o != r) {
                    assert!(!IdAllocator::in_range(*other, id));
                }
            }
        }
        // Slot 0 skips id 0: 999 ids; the other seven slots 1000 each.
        assert_eq!(seen.len(), 999 + 7 * 1000);
    }

    #[test]
    fn insert_needs_the_parent_and_a_fresh_id() {
        let mut t = EntityTable::default();
        assert!(!t.insert(&create(2, 1)), "unknown parent");
        assert!(t.insert(&create(1, MANAGER_ROOT)));
        assert!(t.insert(&create(2, 1)));
        assert!(!t.insert(&create(2, 1)), "id taken");
        assert!(!t.insert(&create(0, MANAGER_ROOT)), "0 is never an entity");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn destroying_a_parent_destroys_its_children() {
        let mut t = EntityTable::default();
        t.insert(&create(1, MANAGER_ROOT));
        t.insert(&create(2, 1));
        t.insert(&create(3, 2));
        t.insert(&create(4, 1));
        t.insert(&create(5, MANAGER_ROOT));
        let gone: Vec<NetId> = t.remove(1).iter().map(|r| r.id).collect();
        assert_eq!(
            gone,
            std::vec![3, 2, 4, 1],
            "children first, the target last"
        );
        assert_eq!(t.len(), 1);
        assert!(t.contains(5));
        assert!(t.remove(1).is_empty());
    }

    #[test]
    fn reparent_rules() {
        let mut t = EntityTable::default();
        t.insert(&create(1, MANAGER_ROOT));
        t.insert(&create(2, 1));
        t.insert(&create(3, MANAGER_ROOT));
        let under = |e: NetId| ParentRef {
            manager: 7,
            entity: e,
            part: 1,
        };
        assert!(!t.reparent(1, under(2)), "not under its own child");
        assert!(!t.reparent(1, under(1)), "not under itself");
        assert!(!t.reparent(1, under(9)), "unknown parent");
        assert!(t.reparent(2, under(3)));
        assert_eq!(t.get(2).unwrap().parent, under(3));
        assert!(t.reparent(2, ParentRef::default()));
        assert!(t.set_owner(2, 5));
        assert_eq!(t.get(2).unwrap().owner, 5);
        assert!(!t.set_owner(9, 5));
    }

    #[test]
    fn a_burst_reproduces_the_table() {
        let mut t = EntityTable::default();
        t.insert(&create(1, MANAGER_ROOT));
        t.insert(&create(2, MANAGER_ROOT));
        t.insert(&create(3, 2));
        // 4 is created under 1, then 1 is moved under 3: a parent created after its child.
        t.insert(&create(4, 1));
        t.reparent(
            1,
            ParentRef {
                manager: 7,
                entity: 3,
                part: 2,
            },
        );
        let burst = t.burst();
        assert_eq!(burst.len(), 4);
        let mut copy = EntityTable::default();
        for c in &burst {
            assert!(copy.insert(c), "{} applies in order", c.id);
        }
        for r in t.iter() {
            let c = copy.get(r.id).unwrap();
            assert_eq!(
                (c.scene, c.parent, &c.name, c.owner, c.parts),
                (r.scene, r.parent, &r.name, r.owner, r.parts)
            );
        }
    }

    #[test]
    fn held_queue_keeps_order_per_entity() {
        let mut q = HeldQueue::default();
        let table_has = |ids: &'static [NetId]| {
            move |a: &EntityAction| {
                let (t, p) = a.needs();
                [t, p].into_iter().flatten().all(|i| ids.contains(&i))
            }
        };
        q.push(1, EntityAction::Create(create(5, 4)), 0);
        q.push(1, EntityAction::Destroy { id: 5 }, 0);
        q.push(1, EntityAction::Create(create(9, MANAGER_ROOT)), 0);
        assert_eq!(q.expected(), [5, 9].into_iter().collect());
        // Parent 4 unknown: the create of 5 waits, the destroy of 5 behind it too; 9 is free to go.
        let h = q.take_ready(table_has(&[])).unwrap();
        assert_eq!(h.action, EntityAction::Create(create(9, MANAGER_ROOT)));
        assert!(q.take_ready(table_has(&[])).is_none());
        // The destroy alone would be ready once 5 exists, but the create of 5 is ahead of it.
        assert!(q.take_ready(table_has(&[5])).is_none());
        let h = q.take_ready(table_has(&[4])).unwrap();
        assert!(matches!(h.action, EntityAction::Create(ref c) if c.id == 5));
        let h = q.take_ready(table_has(&[4, 5])).unwrap();
        assert_eq!(h.action, EntityAction::Destroy { id: 5 });
        assert!(q.is_empty());
    }

    #[test]
    fn overdue_reports_once() {
        let mut q = HeldQueue::default();
        q.push(1, EntityAction::Create(create(5, 4)), 10);
        assert!(q.overdue(50, 60).is_empty());
        assert_eq!(q.overdue(70, 60).len(), 1);
        assert!(q.overdue(500, 60).is_empty());
    }
}

//! Rooms and interest by room (docs/PLAN_NETCODE.md 2.7, 2.11, 12.2; WP13, WP16).
//!
//! Every manager belongs to a *room* (gdnet's `NetManager.room`: by default the scene the manager sits
//! in); a room travels as the FNV-1a hash of its name ([`room_hash`]), and the table here keeps the
//! names so messages and errors can name it. The room named [`GLOBAL`] (`"*"`) is visible from every
//! room.
//!
//! A peer's *primary* room is the room of the manager holding its *player* (the entity gdnet flags
//! with `Net.set_player`, or the first entity `spawn_for`'d to it), unless `Net.set_room(peer, name)`
//! overrides it. Its *scope* is a set of rooms ([`Rooms::scope`]): the primary room, the rooms that
//! room's managers name as `neighbours`, and the rooms added with `Net.add_room(peer, name)`.
//! Interest ([`Rooms::sees`]): a peer sees an entity whose manager's room is in its set or is `"*"`,
//! or whose manager has no registered room. A peer with an empty set sees every entity, unless the
//! session is *strict* (the server uses levels: gdnet's `load_level` / `travel`), where it sees only
//! `"*"` managers until it has a room.
//!
//! The session (session.rs) applies the rule per peer: it keeps the set of entities each peer has been
//! sent (its scope), filters entity actions, snapshots, hashes and calls by it, and when a peer's rooms
//! or an entity's manager change it sends the difference: `Travel` for each room of the set that is a
//! level the peer did not have (its client loads it at the level's [`Origin`]), `Scope { leave }` for
//! entities leaving, `RoomLeft` for each level room it lost (its client frees that level), and a burst
//! of `Create`s for entities entering.

use crate::entity::{fnv1a64, NetId};
use crate::peers::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::string::String;
use std::vec::Vec;

/// The room visible from every room.
pub const GLOBAL: &str = "*";

/// The id of a room name.
pub fn room_hash(name: &str) -> u64 {
    fnv1a64(name.as_bytes())
}

/// A set of rooms (their hashes).
pub type RoomSet = BTreeSet<u64>;

/// Where a loaded level's root sits (WP16 placement): the same on the server and on every client.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Origin {
    /// A 2D level (`Node2D` root): x, y.
    Flat([f32; 2]),
    /// A 3D level (`Node3D` root): x, y, z.
    Deep([f32; 3]),
}

/// Origins are finite (gdnet never sends NaN), so equality is total.
impl Eq for Origin {}

impl Origin {
    /// The origin of cell `index` of a grid of `spacing` (cells along +x, cell 0 at the origin).
    pub fn cell(index: u32, spacing: f32, deep: bool) -> Origin {
        let x = index as f32 * spacing;
        if deep {
            Origin::Deep([x, 0.0, 0.0])
        } else {
            Origin::Flat([x, 0.0])
        }
    }
}

/// The level of a room: the scene a client loads for it, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Level {
    pub path: String,
    /// None: the level keeps the position it was authored at.
    pub origin: Option<Origin>,
}

/// One manager as gdnet registers it ([`Rooms::set_managers`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoomEntry {
    /// The manager's key.
    pub manager: u64,
    /// Its room's name.
    pub room: String,
    /// The level scene it sits in (None: not inside a level).
    pub level: Option<String>,
    /// That level's origin (None: authored position).
    pub origin: Option<Origin>,
    /// The rooms a peer whose primary room is this manager's also sees (`NetManager.neighbours`).
    pub neighbours: Vec<String>,
}

impl RoomEntry {
    pub fn new(manager: u64, room: &str) -> Self {
        Self {
            manager,
            room: String::from(room),
            ..Self::default()
        }
    }

    /// The manager sits in the level `path`, at `origin`.
    pub fn in_level(mut self, path: &str, origin: Option<Origin>) -> Self {
        self.level = Some(String::from(path));
        self.origin = origin;
        self
    }

    pub fn with_neighbours(mut self, names: &[&str]) -> Self {
        self.neighbours = names.iter().map(|n| String::from(*n)).collect();
        self
    }
}

/// One registered room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomInfo {
    pub name: String,
    /// The level to load for it (a client loads it while the room is in its peer's set).
    pub level: Option<Level>,
    /// The neighbour rooms its managers name.
    pub neighbours: RoomSet,
}

/// The room table: which manager is in which room, which peer has which rooms.
#[derive(Clone, Debug, Default)]
pub struct Rooms {
    /// Manager key -> room hash.
    managers: BTreeMap<u64, u64>,
    /// Room hash -> name, level and neighbours (registered rooms only).
    info: BTreeMap<u64, RoomInfo>,
    /// Names of rooms mentioned (neighbours, overrides, added rooms) whether registered or not.
    names: BTreeMap<u64, String>,
    /// Peer -> its player entity.
    players: BTreeMap<PeerId, NetId>,
    /// Peer -> the room `set_room` gave it (overrides the player's).
    overrides: BTreeMap<PeerId, u64>,
    /// Peer -> the rooms `add_room` gave it.
    added: BTreeMap<PeerId, RoomSet>,
    /// A peer without a room sees only `"*"` managers.
    pub strict: bool,
}

impl Rooms {
    /// Replaces the manager table. True when anything changed.
    pub fn set_managers(&mut self, entries: &[RoomEntry]) -> bool {
        let mut managers = BTreeMap::new();
        let mut info: BTreeMap<u64, RoomInfo> = BTreeMap::new();
        for e in entries {
            let h = room_hash(&e.room);
            managers.insert(e.manager, h);
            let entry = info.entry(h).or_insert_with(|| RoomInfo {
                name: e.room.clone(),
                level: None,
                neighbours: RoomSet::new(),
            });
            if entry.level.is_none() {
                entry.level = e.level.as_ref().map(|path| Level {
                    path: path.clone(),
                    origin: e.origin,
                });
            }
            for n in &e.neighbours {
                let nh = room_hash(n);
                if nh != h {
                    entry.neighbours.insert(nh);
                }
                self.names.entry(nh).or_insert_with(|| n.clone());
            }
        }
        for (h, i) in &info {
            self.names.entry(*h).or_insert_with(|| i.name.clone());
        }
        let changed = managers != self.managers || info != self.info;
        self.managers = managers;
        self.info = info;
        changed
    }

    /// The room of a manager key (None when unregistered).
    pub fn of_manager(&self, key: u64) -> Option<u64> {
        self.managers.get(&key).copied()
    }

    /// A registered room.
    pub fn info(&self, room: u64) -> Option<&RoomInfo> {
        self.info.get(&room)
    }

    /// The level of a registered room.
    pub fn level(&self, room: u64) -> Option<&Level> {
        self.info.get(&room).and_then(|i| i.level.as_ref())
    }

    /// Whether a manager registered `room`.
    pub fn is_registered(&self, room: u64) -> bool {
        self.info.contains_key(&room)
    }

    /// The name of a room (its hash in hex when unknown).
    pub fn name(&self, room: u64) -> String {
        self.names
            .get(&room)
            .cloned()
            .unwrap_or_else(|| std::format!("{room:016x}"))
    }

    pub fn set_player(&mut self, peer: PeerId, id: NetId) {
        self.players.insert(peer, id);
    }

    pub fn player(&self, peer: PeerId) -> Option<NetId> {
        self.players.get(&peer).copied()
    }

    pub fn players(&self) -> impl Iterator<Item = (PeerId, NetId)> + '_ {
        self.players.iter().map(|(p, i)| (*p, *i))
    }

    /// Every peer with a player, an override or an added room.
    pub fn peers(&self) -> BTreeSet<PeerId> {
        self.players
            .keys()
            .chain(self.overrides.keys())
            .chain(self.added.keys())
            .copied()
            .collect()
    }

    /// Forgets players whose entity is gone; returns their peers.
    pub fn drop_players(&mut self, gone: &dyn Fn(NetId) -> bool) -> Vec<PeerId> {
        let peers: Vec<PeerId> = self
            .players
            .iter()
            .filter(|(_, id)| gone(**id))
            .map(|(p, _)| *p)
            .collect();
        for p in &peers {
            self.players.remove(p);
        }
        peers
    }

    fn remember(&mut self, name: &str) -> u64 {
        let h = room_hash(name);
        self.names.entry(h).or_insert_with(|| String::from(name));
        h
    }

    /// `set_room(peer, Some(name))` overrides the player's room; `None` removes the override.
    pub fn set_override(&mut self, peer: PeerId, name: Option<&str>) {
        match name {
            Some(n) => {
                let h = self.remember(n);
                self.overrides.insert(peer, h);
            }
            None => {
                self.overrides.remove(&peer);
            }
        }
    }

    pub fn override_of(&self, peer: PeerId) -> Option<u64> {
        self.overrides.get(&peer).copied()
    }

    /// `Net.add_room(peer, name)`: the peer also sees room `name`. True when it was not added before.
    pub fn add_room(&mut self, peer: PeerId, name: &str) -> bool {
        let h = self.remember(name);
        self.added.entry(peer).or_default().insert(h)
    }

    /// `Net.remove_room(peer, name)`: undoes `add_room`. True when it was added.
    pub fn remove_room(&mut self, peer: PeerId, name: &str) -> bool {
        let h = room_hash(name);
        let Some(set) = self.added.get_mut(&peer) else {
            return false;
        };
        let removed = set.remove(&h);
        if set.is_empty() {
            self.added.remove(&peer);
        }
        removed
    }

    /// The rooms `add_room` gave a peer.
    pub fn added(&self, peer: PeerId) -> RoomSet {
        self.added.get(&peer).cloned().unwrap_or_default()
    }

    /// WP17: `old`'s player, primary room override and added rooms become `new`'s (a rejoin).
    pub fn rename_peer(&mut self, old: PeerId, new: PeerId) {
        if let Some(p) = self.players.remove(&old) {
            self.players.insert(new, p);
        }
        if let Some(o) = self.overrides.remove(&old) {
            self.overrides.insert(new, o);
        }
        if let Some(a) = self.added.remove(&old) {
            self.added.insert(new, a);
        }
    }

    pub fn forget_peer(&mut self, peer: PeerId) {
        self.players.remove(&peer);
        self.overrides.remove(&peer);
        self.added.remove(&peer);
    }

    /// A peer's set of rooms: its `primary` room (its player's, or its override), that room's
    /// neighbours, and the rooms added to it.
    pub fn scope(&self, peer: PeerId, primary: Option<u64>) -> RoomSet {
        let mut set = self.added(peer);
        if let Some(p) = primary {
            set.insert(p);
            if let Some(i) = self.info.get(&p) {
                set.extend(i.neighbours.iter().copied());
            }
        }
        set
    }

    /// The interest rule: does a peer with the rooms `rooms` see an entity of a manager in
    /// `entity_room`?
    pub fn sees(&self, rooms: &RoomSet, entity_room: Option<u64>) -> bool {
        let Some(er) = entity_room else {
            return true;
        };
        if er == room_hash(GLOBAL) {
            return true;
        }
        if rooms.is_empty() {
            return !self.strict;
        }
        rooms.contains(&er)
    }

    /// The levels of a set of rooms (rooms without a level are left out).
    pub fn levels_of(&self, rooms: &RoomSet) -> BTreeMap<u64, Level> {
        rooms
            .iter()
            .filter_map(|r| self.level(*r).map(|l| (*r, l.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUB: &str = "res://hub.tscn";
    const CAVE: &str = "res://cave.tscn";
    const A: &str = "res://chunk_a.tscn";
    const B: &str = "res://chunk_b.tscn";
    const C: &str = "res://chunk_c.tscn";

    fn rooms() -> Rooms {
        let mut r = Rooms::default();
        r.set_managers(&[
            RoomEntry::new(1, HUB).in_level(HUB, None),
            RoomEntry::new(2, CAVE).in_level(CAVE, Some(Origin::Flat([2000.0, 0.0]))),
            RoomEntry::new(3, GLOBAL),
            RoomEntry::new(4, A).in_level(A, None).with_neighbours(&[B]),
            RoomEntry::new(5, B)
                .in_level(B, None)
                .with_neighbours(&[A, C]),
        ]);
        r
    }

    fn set(r: &Rooms, peer: PeerId, primary: Option<u64>) -> RoomSet {
        r.scope(peer, primary)
    }

    #[test]
    fn a_peer_sees_its_room_and_the_global_one() {
        let r = rooms();
        let hub = r.of_manager(1);
        let cave = r.of_manager(2);
        let s = set(&r, 7, hub);
        assert!(r.sees(&s, hub));
        assert!(!r.sees(&s, cave));
        assert!(r.sees(&set(&r, 7, cave), r.of_manager(3)));
        assert!(
            r.sees(&s, r.of_manager(99)),
            "an unregistered manager is visible"
        );
        assert_eq!(r.name(cave.unwrap()), CAVE);
        assert_eq!(
            r.level(cave.unwrap()),
            Some(&Level {
                path: String::from(CAVE),
                origin: Some(Origin::Flat([2000.0, 0.0]))
            })
        );
    }

    #[test]
    fn a_roomless_peer_sees_everything_unless_strict() {
        let mut r = rooms();
        let none = set(&r, 7, None);
        assert!(r.sees(&none, r.of_manager(2)));
        r.strict = true;
        assert!(!r.sees(&none, r.of_manager(2)));
        assert!(r.sees(&none, r.of_manager(3)));
    }

    #[test]
    fn the_set_holds_the_primary_room_its_neighbours_and_added_rooms() {
        let mut r = rooms();
        r.strict = true;
        let (a, b, c) = (room_hash(A), room_hash(B), room_hash(C));
        // In chunk A: A and its neighbour B, not B's neighbour C.
        let s = set(&r, 7, Some(a));
        assert_eq!(s, RoomSet::from([a, b]));
        assert!(r.sees(&s, r.of_manager(4)));
        assert!(r.sees(&s, r.of_manager(5)));
        assert!(!r.sees(&s, r.of_manager(1)));
        assert!(r.sees(&s, r.of_manager(3)), "\"*\" is always seen");
        // In chunk B: A, B and C (C is named although no manager registered it yet).
        assert_eq!(set(&r, 7, Some(b)), RoomSet::from([a, b, c]));
        assert_eq!(r.name(c), C);
        assert!(!r.is_registered(c));
        // An added room joins the set; removing it takes it out again.
        assert!(r.add_room(7, CAVE));
        assert!(!r.add_room(7, CAVE), "added once");
        let s = set(&r, 7, Some(a));
        assert!(r.sees(&s, r.of_manager(2)));
        assert_eq!(r.levels_of(&s).len(), 3);
        assert_eq!(r.peers(), BTreeSet::from([7]));
        // Without a primary room the added rooms are the whole set (strict or not).
        let s = set(&r, 7, None);
        assert_eq!(s, RoomSet::from([room_hash(CAVE)]));
        assert!(!r.sees(&s, r.of_manager(1)));
        assert!(r.remove_room(7, CAVE));
        assert!(!r.remove_room(7, CAVE));
        assert!(set(&r, 7, None).is_empty());
        assert!(r.peers().is_empty());
    }

    #[test]
    fn set_managers_reports_changes_and_overrides_keep_their_names() {
        let mut r = Rooms::default();
        let table = [
            RoomEntry::new(1, HUB).in_level(HUB, None),
            RoomEntry::new(2, CAVE).in_level(CAVE, None),
        ];
        assert!(r.set_managers(&table));
        assert!(!r.set_managers(&table));
        // A new origin or new neighbours are a change.
        assert!(r.set_managers(&[
            RoomEntry::new(1, HUB).in_level(HUB, None),
            RoomEntry::new(2, CAVE).in_level(CAVE, Some(Origin::Flat([1.0, 0.0]))),
        ]));
        assert!(r.set_managers(&[
            RoomEntry::new(1, HUB)
                .in_level(HUB, None)
                .with_neighbours(&[CAVE]),
            RoomEntry::new(2, CAVE).in_level(CAVE, Some(Origin::Flat([1.0, 0.0]))),
        ]));
        r.set_override(7, Some("spectators"));
        assert!(r.set_managers(&[RoomEntry::new(1, HUB)]));
        assert_eq!(r.of_manager(2), None);
        assert_eq!(r.name(r.override_of(7).unwrap()), "spectators");
        r.set_player(5, 40);
        assert_eq!(r.drop_players(&|id| id == 40), std::vec![5]);
        assert_eq!(r.player(5), None);
    }

    #[test]
    fn grid_cells_lie_along_x() {
        assert_eq!(Origin::cell(0, 1000.0, false), Origin::Flat([0.0, 0.0]));
        assert_eq!(Origin::cell(3, 1000.0, false), Origin::Flat([3000.0, 0.0]));
        assert_eq!(Origin::cell(2, 50.0, true), Origin::Deep([100.0, 0.0, 0.0]));
    }
}

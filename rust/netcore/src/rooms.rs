//! Rooms and interest by room (docs/PLAN_NETCODE.md 2.7, 2.11, WP13).
//!
//! Every manager belongs to a *room* (gdnet's `NetManager.room`: by default the scene the manager sits
//! in); a room travels as the FNV-1a hash of its name ([`room_hash`]), and the table here keeps the
//! names so messages and errors can name it. The room named [`GLOBAL`] (`"*"`) is visible from every
//! room.
//!
//! A peer's room is the room of the manager holding its *player* (the entity gdnet flags with
//! `Net.set_player`, or the first entity `spawn_for`'d to it), unless `Net.set_room(peer, name)`
//! overrides it. Interest ([`Rooms::sees`]): a peer sees an entity whose manager's room is its own
//! room or `"*"`, or whose manager has no registered room. A peer without a room sees every entity,
//! unless the session is *strict* (the server uses levels: gdnet's `load_level` / `travel`), where it
//! sees only `"*"` managers until it has a room.
//!
//! The session (session.rs) applies the rule per peer: it keeps the set of entities each peer has been
//! sent (its scope), filters entity actions, snapshots, hashes and calls by it, and when a peer's room
//! or an entity's manager changes it sends the difference: `Scope { leave }` for entities leaving
//! (the client frees those replicas) and a burst of `Create`s for entities entering. A room that is a
//! level (it has a scene path) also makes the server send `Travel` to the peer whose room it becomes,
//! so its client loads that level first.

use crate::entity::{fnv1a64, NetId};
use crate::peers::PeerId;
use std::collections::BTreeMap;
use std::string::String;

/// The room visible from every room.
pub const GLOBAL: &str = "*";

/// The id of a room name.
pub fn room_hash(name: &str) -> u64 {
    fnv1a64(name.as_bytes())
}

/// One registered room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomInfo {
    pub name: String,
    /// The level scene to load for it (a client loads it when its peer's room becomes this one).
    pub level: Option<String>,
}

/// The room table: which manager is in which room, which peer is in which room.
#[derive(Clone, Debug, Default)]
pub struct Rooms {
    /// Manager key -> room hash.
    managers: BTreeMap<u64, u64>,
    /// Room hash -> name and level.
    info: BTreeMap<u64, RoomInfo>,
    /// Peer -> its player entity.
    players: BTreeMap<PeerId, NetId>,
    /// Peer -> the room `set_room` gave it (overrides the player's).
    overrides: BTreeMap<PeerId, u64>,
    /// A peer without a room sees only `"*"` managers.
    pub strict: bool,
}

impl Rooms {
    /// Replaces the manager table with `rooms` = (manager key, room name, level scene). True when
    /// anything changed.
    pub fn set_managers(&mut self, rooms: &[(u64, String, Option<String>)]) -> bool {
        let mut managers = BTreeMap::new();
        let mut info = BTreeMap::new();
        for (key, name, level) in rooms {
            let h = room_hash(name);
            managers.insert(*key, h);
            let entry = info.entry(h).or_insert_with(|| RoomInfo {
                name: name.clone(),
                level: None,
            });
            if entry.level.is_none() {
                entry.level = level.clone();
            }
        }
        // Keep the names of override rooms nobody registered.
        for h in self.overrides.values() {
            if let Some(i) = self.info.get(h) {
                info.entry(*h).or_insert_with(|| i.clone());
            }
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

    pub fn info(&self, room: u64) -> Option<&RoomInfo> {
        self.info.get(&room)
    }

    /// The name of a room (its hash in hex when unknown).
    pub fn name(&self, room: u64) -> String {
        self.info
            .get(&room)
            .map_or_else(|| std::format!("{room:016x}"), |i| i.name.clone())
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

    /// Forgets players whose entity is gone; returns their peers.
    pub fn drop_players(&mut self, gone: &dyn Fn(NetId) -> bool) -> std::vec::Vec<PeerId> {
        let peers: std::vec::Vec<PeerId> = self
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

    /// `set_room(peer, Some(name))` overrides the player's room; `None` removes the override.
    pub fn set_override(&mut self, peer: PeerId, name: Option<&str>) {
        match name {
            Some(n) => {
                let h = room_hash(n);
                self.info.entry(h).or_insert_with(|| RoomInfo {
                    name: String::from(n),
                    level: None,
                });
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

    pub fn forget_peer(&mut self, peer: PeerId) {
        self.players.remove(&peer);
        self.overrides.remove(&peer);
    }

    /// The interest rule: does a peer in `peer_room` see an entity of a manager in `entity_room`?
    pub fn sees(&self, peer_room: Option<u64>, entity_room: Option<u64>) -> bool {
        let Some(er) = entity_room else {
            return true;
        };
        if er == room_hash(GLOBAL) {
            return true;
        }
        match peer_room {
            Some(pr) => pr == er,
            None => !self.strict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rooms() -> Rooms {
        let mut r = Rooms::default();
        r.set_managers(&[
            (
                1,
                String::from("res://hub.tscn"),
                Some(String::from("res://hub.tscn")),
            ),
            (
                2,
                String::from("res://cave.tscn"),
                Some(String::from("res://cave.tscn")),
            ),
            (3, String::from(GLOBAL), None),
        ]);
        r
    }

    #[test]
    fn a_peer_sees_its_room_and_the_global_one() {
        let r = rooms();
        let hub = r.of_manager(1);
        let cave = r.of_manager(2);
        assert!(r.sees(hub, hub));
        assert!(!r.sees(hub, cave));
        assert!(r.sees(cave, r.of_manager(3)));
        assert!(
            r.sees(hub, r.of_manager(99)),
            "an unregistered manager is visible"
        );
        assert_eq!(r.name(cave.unwrap()), "res://cave.tscn");
        assert_eq!(
            r.info(cave.unwrap())
                .and_then(|i| i.level.clone())
                .as_deref(),
            Some("res://cave.tscn")
        );
    }

    #[test]
    fn a_roomless_peer_sees_everything_unless_strict() {
        let mut r = rooms();
        assert!(r.sees(None, r.of_manager(2)));
        r.strict = true;
        assert!(!r.sees(None, r.of_manager(2)));
        assert!(r.sees(None, r.of_manager(3)));
    }

    #[test]
    fn set_managers_reports_changes_and_overrides_keep_their_names() {
        let mut r = rooms();
        assert!(!r.set_managers(&[
            (
                1,
                String::from("res://hub.tscn"),
                Some(String::from("res://hub.tscn"))
            ),
            (
                2,
                String::from("res://cave.tscn"),
                Some(String::from("res://cave.tscn"))
            ),
            (3, String::from(GLOBAL), None),
        ]));
        r.set_override(7, Some("spectators"));
        assert!(r.set_managers(&[(1, String::from("res://hub.tscn"), None)]));
        assert_eq!(r.of_manager(2), None);
        assert_eq!(r.name(r.override_of(7).unwrap()), "spectators");
        r.set_player(5, 40);
        assert_eq!(r.drop_players(&|id| id == 40), std::vec![5]);
        assert_eq!(r.player(5), None);
    }
}

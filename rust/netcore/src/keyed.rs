//! Keyed state (docs/PLAN_NETCODE.md 1 rung 2, 2.4 `KeyedState`, WP7): a reliable map from paths
//! (`"score/5"`, `"chat/12"`) to values, replicated on channel 0.
//!
//! The server holds the map. Its `set` / `erase` go as deltas to every peer that can see the path; a
//! joining peer gets the whole map it can see in one [`KeyMsg::Full`]. Values are opaque bytes here
//! (gdnet stores postcard-encoded `Arg`s, the WP6 Variant encoding).
//!
//! Permissions: by default only the server writes. `allow(prefix, who)` lets clients write paths that
//! start with `prefix`: [`Who::All`] (every peer), [`Who::Peer`] (one peer) or [`Who::Owner`] (a peer
//! may write the paths whose segment after the prefix is its own id: `allow("score/", owner)` lets
//! peer 5 write `score/5` and `score/5/...`). The rules travel to the clients, so a client refuses a
//! write it may not make locally; a client's write the server refuses anyway (rules changed meanwhile)
//! comes back as [`KeyMsg::Rejected`] with the server's value, which the client restores. A client
//! applies its own allowed writes at once (optimistically); the server's echo confirms them.
//!
//! WP17 adds [`Who::Seat`]: the segment after the prefix is the writer's seat, so a path such as
//! `hand/2` stays writable by seat 2 after it reconnects under a new peer id. The session keeps the
//! peer-to-seat map here ([`KeyedState::set_seats`]) on the server and on every client.
//!
//! Views: `view(prefix)` hides the paths under `prefix` from every peer whose decision for the view is
//! not `true`. gdnet evaluates the view's filter (a Callable) per peer and feeds the decisions
//! ([`KeyedState::set_visible`]); a path under several views needs every one of them.

use crate::peers::{PeerId, SERVER};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::string::String;
use std::vec::Vec;

/// Who may write under a prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Who {
    /// Every peer.
    All,
    /// The peer whose id is the path's segment after the prefix.
    Owner,
    /// One peer.
    Peer(PeerId),
    /// WP17: the peer whose seat is the path's segment after the prefix (survives a rejoin).
    Seat,
}

/// The `KEYED` message (kind 16, reliable).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyMsg {
    Set {
        path: String,
        value: Vec<u8>,
    },
    Erase {
        path: String,
    },
    /// Server to a joining client: every entry it can see, and the write rules.
    Full {
        entries: Vec<(String, Vec<u8>)>,
        rules: Vec<(String, Who)>,
    },
    /// Server to clients: the write rules changed.
    Rules {
        rules: Vec<(String, Who)>,
    },
    /// Server to a writer: its write of `path` was refused; `value` is the server's (None: absent).
    Rejected {
        path: String,
        value: Option<Vec<u8>>,
    },
}

/// What happened to the map, for gdnet's signals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Changed(String, Vec<u8>),
    Erased(String),
    /// This peer's write of the path was refused (by the server, or locally).
    Rejected(String),
}

/// Counters for diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyedStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub messages_sent: u64,
    pub messages_received: u64,
    pub rejected: u64,
}

#[derive(Clone, Debug, Default)]
pub struct KeyedState {
    map: BTreeMap<String, Vec<u8>>,
    rules: Vec<(String, Who)>,
    /// View prefix -> per-peer decision (absent = not visible yet).
    views: BTreeMap<String, BTreeMap<PeerId, bool>>,
    /// WP17: each connected peer's seat (for [`Who::Seat`]).
    seats: BTreeMap<PeerId, u32>,
    events: VecDeque<KeyEvent>,
    pub stats: KeyedStats,
}

/// The segment of `path` after `prefix` (up to the next `/`).
fn segment_after<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    Some(rest.split('/').next().unwrap_or(""))
}

impl KeyedState {
    pub fn get(&self, path: &str) -> Option<&Vec<u8>> {
        self.map.get(path)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The paths starting with `prefix`, sorted.
    pub fn keys(&self, prefix: &str) -> Vec<String> {
        self.map
            .range(String::from(prefix)..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn rules(&self) -> &[(String, Who)] {
        &self.rules
    }

    /// Adds (or replaces) the write rule for `prefix`.
    pub fn allow(&mut self, prefix: &str, who: Who) {
        match self
            .rules
            .iter_mut()
            .find(|(p, w)| p == prefix && *w == who)
        {
            Some(_) => {}
            None => self.rules.push((String::from(prefix), who)),
        }
    }

    pub fn set_rules(&mut self, rules: Vec<(String, Who)>) {
        self.rules = rules;
    }

    /// Whether `peer` may write `path` (the server always may).
    pub fn may_write(&self, peer: PeerId, path: &str) -> bool {
        if peer == SERVER {
            return true;
        }
        self.rules.iter().any(|(prefix, who)| {
            path.starts_with(prefix.as_str())
                && match who {
                    Who::All => true,
                    Who::Peer(p) => *p == peer,
                    Who::Owner => {
                        segment_after(prefix, path).is_some_and(|s| s == std::format!("{peer}"))
                    }
                    Who::Seat => self.seats.get(&peer).is_some_and(|seat| {
                        segment_after(prefix, path).is_some_and(|s| s == std::format!("{seat}"))
                    }),
                }
        })
    }

    /// Registers a view over `prefix` (decisions start unknown, i.e. hidden).
    pub fn add_view(&mut self, prefix: &str) {
        self.views.entry(String::from(prefix)).or_default();
    }

    pub fn has_view(&self, prefix: &str) -> bool {
        self.views.contains_key(prefix)
    }

    /// The view prefixes.
    pub fn view_prefixes(&self) -> Vec<String> {
        self.views.keys().cloned().collect()
    }

    /// Records `peer`'s decision for the view `prefix`; returns true when it changed.
    pub fn set_visible(&mut self, prefix: &str, peer: PeerId, visible: bool) -> bool {
        let Some(d) = self.views.get_mut(prefix) else {
            return false;
        };
        d.insert(peer, visible) != Some(visible)
    }

    /// WP17: the seat of each connected peer.
    pub fn set_seats(&mut self, seats: BTreeMap<PeerId, u32>) {
        self.seats = seats;
    }

    /// WP17: `old`'s view decisions become `new`'s (a rejoin).
    pub fn rename_peer(&mut self, old: PeerId, new: PeerId) {
        for d in self.views.values_mut() {
            if let Some(v) = d.remove(&old) {
                d.insert(new, v);
            }
        }
    }

    /// Every entry (a saved session state).
    pub fn entries(&self) -> Vec<(String, Vec<u8>)> {
        self.map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn forget_peer(&mut self, peer: PeerId) {
        for d in self.views.values_mut() {
            d.remove(&peer);
        }
    }

    /// Whether `peer` sees `path` (the server sees everything).
    pub fn visible(&self, peer: PeerId, path: &str) -> bool {
        if peer == SERVER {
            return true;
        }
        self.views
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
            .all(|(_, d)| d.get(&peer).copied().unwrap_or(false))
    }

    /// The entries `peer` sees.
    pub fn entries_for(&self, peer: PeerId) -> Vec<(String, Vec<u8>)> {
        self.map
            .iter()
            .filter(|(k, _)| self.visible(peer, k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Every entry under `prefix`.
    pub fn entries_under(&self, prefix: &str) -> Vec<(String, Vec<u8>)> {
        self.map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Sets `path`; true when the value changed (a `Changed` event is queued).
    pub fn apply_set(&mut self, path: &str, value: Vec<u8>) -> bool {
        if self.map.get(path) == Some(&value) {
            return false;
        }
        self.map.insert(String::from(path), value.clone());
        self.events
            .push_back(KeyEvent::Changed(String::from(path), value));
        true
    }

    /// Erases `path`; true when it existed (an `Erased` event is queued).
    pub fn apply_erase(&mut self, path: &str) -> bool {
        if self.map.remove(path).is_none() {
            return false;
        }
        self.events.push_back(KeyEvent::Erased(String::from(path)));
        true
    }

    /// Replaces the whole map (a client's `Full`), queueing the differences as events.
    pub fn apply_full(&mut self, entries: Vec<(String, Vec<u8>)>) {
        let incoming: BTreeMap<String, Vec<u8>> = entries.into_iter().collect();
        let gone: Vec<String> = self
            .map
            .keys()
            .filter(|k| !incoming.contains_key(*k))
            .cloned()
            .collect();
        for k in gone {
            self.apply_erase(&k);
        }
        for (k, v) in incoming {
            self.apply_set(&k, v);
        }
    }

    pub fn push_event(&mut self, e: KeyEvent) {
        self.events.push_back(e);
    }

    pub fn poll_event(&mut self) -> Option<KeyEvent> {
        self.events.pop_front()
    }
}

/// Encodes a keyed message.
pub fn encode(msg: &KeyMsg) -> Vec<u8> {
    postcard::to_allocvec(msg).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_need_a_rule() {
        let mut k = KeyedState::default();
        assert!(k.may_write(SERVER, "score/5"), "the server always writes");
        assert!(!k.may_write(5, "score/5"), "server-only by default");
        k.allow("chat/", Who::All);
        k.allow("score/", Who::Owner);
        k.allow("admin/", Who::Peer(7));
        assert!(k.may_write(5, "chat/1"));
        assert!(k.may_write(5, "score/5"));
        assert!(k.may_write(5, "score/5/kills"));
        assert!(!k.may_write(5, "score/6"), "another peer's path");
        assert!(
            !k.may_write(5, "score/55"),
            "a whole segment, not a prefix of it"
        );
        assert!(k.may_write(7, "admin/x"));
        assert!(!k.may_write(5, "admin/x"));
        assert!(!k.may_write(5, "other"));
    }

    #[test]
    fn the_seat_rule_reads_the_writers_seat() {
        let mut k = KeyedState::default();
        k.allow("hand/", Who::Seat);
        assert!(!k.may_write(5, "hand/2"), "no seat known yet");
        k.set_seats([(5, 2), (6, 3)].into_iter().collect());
        assert!(k.may_write(5, "hand/2"));
        assert!(k.may_write(5, "hand/2/cards"));
        assert!(!k.may_write(5, "hand/3"));
        assert!(!k.may_write(5, "hand/22"));
        assert!(!k.may_write(5, "hand/5"), "the peer id is not the seat");
        // Peer 5 drops; seat 2 comes back as peer 9.
        k.set_seats([(9, 2), (6, 3)].into_iter().collect());
        assert!(k.may_write(9, "hand/2"));
        assert!(!k.may_write(5, "hand/2"));
    }

    #[test]
    fn sets_and_erases_queue_events_once() {
        let mut k = KeyedState::default();
        assert!(k.apply_set("a", std::vec![1]));
        assert!(!k.apply_set("a", std::vec![1]), "unchanged");
        assert!(k.apply_set("a", std::vec![2]));
        assert!(k.apply_erase("a"));
        assert!(!k.apply_erase("a"));
        let evs: Vec<KeyEvent> = core::iter::from_fn(|| k.poll_event()).collect();
        assert_eq!(
            evs,
            std::vec![
                KeyEvent::Changed("a".into(), std::vec![1]),
                KeyEvent::Changed("a".into(), std::vec![2]),
                KeyEvent::Erased("a".into()),
            ]
        );
    }

    #[test]
    fn views_hide_until_decided() {
        let mut k = KeyedState::default();
        k.apply_set("team/red/plan", std::vec![1]);
        k.apply_set("score/5", std::vec![2]);
        k.add_view("team/red/");
        assert!(!k.visible(5, "team/red/plan"), "undecided = hidden");
        assert!(k.visible(5, "score/5"));
        assert!(k.visible(SERVER, "team/red/plan"));
        assert!(k.set_visible("team/red/", 5, true));
        assert!(!k.set_visible("team/red/", 5, true), "unchanged");
        assert!(k.visible(5, "team/red/plan"));
        assert!(!k.visible(6, "team/red/plan"));
        assert_eq!(k.entries_for(6).len(), 1);
        assert_eq!(k.entries_for(5).len(), 2);
        assert_eq!(k.keys("team/"), std::vec![String::from("team/red/plan")]);
    }

    #[test]
    fn a_full_map_replaces_with_events() {
        let mut k = KeyedState::default();
        k.apply_set("a", std::vec![1]);
        k.apply_set("b", std::vec![1]);
        while k.poll_event().is_some() {}
        k.apply_full(std::vec![
            ("b".into(), std::vec![2]),
            ("c".into(), std::vec![3])
        ]);
        let evs: Vec<KeyEvent> = core::iter::from_fn(|| k.poll_event()).collect();
        assert_eq!(evs.len(), 3);
        assert!(evs.contains(&KeyEvent::Erased("a".into())));
        assert_eq!(k.keys(""), std::vec![String::from("b"), String::from("c")]);
    }
}

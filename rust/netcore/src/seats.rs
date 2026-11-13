//! Seats (docs/PLAN_NETCODE.md 12.3, WP17): a player's stable number for the session, separate from
//! the peer id, which changes with every connection.
//!
//! A client sends a *key* in `Hello` (a random string its process keeps, or one the game passes: a save
//! slot, an account name). The server maps key to seat: a new key takes the lowest free seat number
//! (from 1), a key whose seat is away takes it back (a *rejoin*), and a key whose seat is connected is
//! refused (two connections cannot hold one seat). Keys are identifiers, not credentials (12.1): they
//! are never verified, and they never leave the server (clients learn seats and peer ids only).
//!
//! When a seated peer disconnects its seat is *held* for the rejoin window ([`Hold`]): 0 releases it at
//! once (v1's behaviour), a number of ticks holds it that long, and [`Hold::Session`] for the whole
//! session. Time is the session's tick, passed in by the caller (tests pass any number). A *reserved*
//! seat comes from a saved session state: it is away from the start and never expires, since it owns
//! nothing live until its key joins.
//!
//! Nothing here sends anything: the session (session.rs) turns claims, drops and releases into
//! messages and events.

use crate::peers::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;

/// A seat number (1, 2, ...; 0 means none).
pub type Seat = u32;

/// How long a dropped peer's seat is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Released at once: the peer's entities go back to the server (v1's behaviour).
    Released,
    /// Held this many ticks.
    Ticks(u64),
    /// Held for the whole session.
    Session,
}

impl Hold {
    /// `gdnet/session/rejoin_timeout_s`: 0 = [`Hold::Released`], negative = [`Hold::Session`], else
    /// the seconds in ticks (at least one).
    pub fn from_seconds(seconds: f64, tick_rate: u32) -> Hold {
        if seconds.is_nan() || seconds < 0.0 {
            Hold::Session
        } else if seconds == 0.0 {
            Hold::Released
        } else {
            Hold::Ticks(((seconds * f64::from(tick_rate.max(1))).round() as u64).max(1))
        }
    }
}

/// One row of the seat table as every peer knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInfo {
    pub seat: Seat,
    /// The connected peer, or while the seat is away the peer that held it last (0: never connected in
    /// this session, a reserved seat).
    pub peer: PeerId,
    pub away: bool,
}

/// What a `Hello` with a key gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// A new seat (or a reserved one that never connected in this session).
    Seated(Seat),
    /// An away seat taken back; `old_peer` held it last (its entities, rooms and views move).
    Rejoined { seat: Seat, old_peer: PeerId },
    /// The key's seat is connected (`peer` holds it): the claim is refused.
    Taken { seat: Seat, peer: PeerId },
}

/// What a disconnection did to a seated peer's seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// The seat is held for the rejoin window.
    Held(Seat),
    /// The seat was released at once ([`Hold::Released`]).
    Released(Seat),
}

#[derive(Clone, Debug)]
struct Entry {
    key: String,
    /// The connected peer, 0 while away.
    peer: PeerId,
    /// The peer that held it last (0: never connected in this session).
    last_peer: PeerId,
    /// The tick it went away.
    away_since: u64,
    /// From a saved state and never connected since: never expires.
    reserved: bool,
    /// Claimed with an empty key: no later connection can take it back, so it is released at once.
    keyless: bool,
}

/// The server's seat table (a client keeps [`SeatInfo`] rows only).
#[derive(Clone, Debug)]
pub struct Seats {
    entries: BTreeMap<Seat, Entry>,
    by_key: BTreeMap<String, Seat>,
    hold: Hold,
}

impl Default for Seats {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            by_key: BTreeMap::new(),
            hold: Hold::Ticks(60 * 60),
        }
    }
}

impl Seats {
    pub fn hold(&self) -> Hold {
        self.hold
    }

    /// Changes the rejoin window; seats already away follow the new one at the next [`Seats::expire`].
    pub fn set_hold(&mut self, hold: Hold) {
        self.hold = hold;
    }

    fn lowest_free(&self) -> Seat {
        let mut s = 1;
        while self.entries.contains_key(&s) {
            s += 1;
        }
        s
    }

    /// `peer` connects with `key`. `prefer` (a restored host's seat) is used for a new key when it is
    /// free or reserved for no live key.
    /// An empty key gets a seat no later connection can take back (released when it drops).
    pub fn claim(&mut self, key: &str, peer: PeerId, prefer: Option<Seat>) -> Claim {
        let keyless = key.is_empty();
        let key = if keyless {
            std::format!("\u{0}peer {peer}")
        } else {
            String::from(key)
        };
        let key = key.as_str();
        if let Some(&seat) = self.by_key.get(key) {
            let Some(e) = self.entries.get_mut(&seat) else {
                return Claim::Seated(seat);
            };
            if e.peer != 0 {
                return Claim::Taken { seat, peer: e.peer };
            }
            let old = e.last_peer;
            e.peer = peer;
            e.last_peer = peer;
            e.reserved = false;
            return if old == 0 {
                Claim::Seated(seat)
            } else {
                Claim::Rejoined {
                    seat,
                    old_peer: old,
                }
            };
        }
        let seat = match prefer {
            Some(p) if p != 0 && !self.entries.contains_key(&p) => p,
            Some(p) if self.entries.get(&p).is_some_and(|e| e.reserved) => {
                // The saved host seat, re-keyed for this host.
                if let Some(old) = self.entries.remove(&p) {
                    self.by_key.remove(&old.key);
                }
                p
            }
            _ => self.lowest_free(),
        };
        self.entries.insert(
            seat,
            Entry {
                key: String::from(key),
                peer,
                last_peer: peer,
                away_since: 0,
                reserved: false,
                keyless,
            },
        );
        self.by_key.insert(String::from(key), seat);
        Claim::Seated(seat)
    }

    /// `peer` disconnected at tick `now`: its seat is held or released (None: it had no seat).
    pub fn drop_peer(&mut self, peer: PeerId, now: u64) -> Option<Dropped> {
        let seat = self.seat_of_connected(peer)?;
        if self.hold == Hold::Released || self.entries.get(&seat).is_some_and(|e| e.keyless) {
            self.remove(seat);
            return Some(Dropped::Released(seat));
        }
        if let Some(e) = self.entries.get_mut(&seat) {
            e.peer = 0;
            e.away_since = now;
        }
        Some(Dropped::Held(seat))
    }

    fn remove(&mut self, seat: Seat) -> Option<Entry> {
        let e = self.entries.remove(&seat)?;
        self.by_key.remove(&e.key);
        Some(e)
    }

    /// Releases the away seats whose window ended at tick `now`: (seat, the peer that held it last).
    pub fn expire(&mut self, now: u64) -> Vec<(Seat, PeerId)> {
        let hold = self.hold;
        let due: Vec<Seat> = self
            .entries
            .iter()
            .filter(|(_, e)| e.peer == 0 && !e.reserved)
            .filter(|(_, e)| match hold {
                Hold::Released => true,
                Hold::Ticks(t) => now >= e.away_since.saturating_add(t),
                Hold::Session => false,
            })
            .map(|(s, _)| *s)
            .collect();
        due.into_iter()
            .filter_map(|s| self.remove(s).map(|e| (s, e.last_peer)))
            .collect()
    }

    /// A saved seat: away and kept for `key` until it joins.
    pub fn reserve(&mut self, seat: Seat, key: &str) {
        if seat == 0 || self.by_key.contains_key(key) {
            return;
        }
        if let Some(old) = self.remove(seat) {
            self.by_key.remove(&old.key);
        }
        self.entries.insert(
            seat,
            Entry {
                key: String::from(key),
                peer: 0,
                last_peer: 0,
                away_since: 0,
                reserved: true,
                keyless: false,
            },
        );
        self.by_key.insert(String::from(key), seat);
    }

    fn seat_of_connected(&self, peer: PeerId) -> Option<Seat> {
        if peer == 0 {
            return None;
        }
        self.entries
            .iter()
            .find(|(_, e)| e.peer == peer)
            .map(|(s, _)| *s)
    }

    /// The seat `peer` holds, or held last while that seat is away.
    pub fn seat_of(&self, peer: PeerId) -> Option<Seat> {
        if peer == 0 {
            return None;
        }
        self.seat_of_connected(peer).or_else(|| {
            self.entries
                .iter()
                .find(|(_, e)| e.peer == 0 && e.last_peer == peer)
                .map(|(s, _)| *s)
        })
    }

    /// The peer connected on `seat` (0 while away or unknown).
    pub fn peer_of(&self, seat: Seat) -> PeerId {
        self.entries.get(&seat).map_or(0, |e| e.peer)
    }

    /// Whether `seat` exists and is away.
    pub fn is_away(&self, seat: Seat) -> bool {
        self.entries.get(&seat).is_some_and(|e| e.peer == 0)
    }

    /// The table every peer learns.
    pub fn table(&self) -> Vec<SeatInfo> {
        self.entries
            .iter()
            .map(|(s, e)| SeatInfo {
                seat: *s,
                peer: if e.peer != 0 { e.peer } else { e.last_peer },
                away: e.peer == 0,
            })
            .collect()
    }

    /// (seat, key) of every seat with a key, for a saved session state.
    pub fn keys(&self) -> Vec<(Seat, String)> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.keyless)
            .map(|(s, e)| (*s, e.key.clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Seats away (held, or reserved and not joined yet).
    pub fn away(&self) -> usize {
        self.entries.values().filter(|e| e.peer == 0).count()
    }
}

/// A client's view of the table: the seat of a peer id (connected, or the last holder of an away
/// seat), and the peer on a seat.
pub fn seat_in(table: &[SeatInfo], peer: PeerId) -> Option<Seat> {
    if peer == 0 {
        return None;
    }
    table
        .iter()
        .find(|r| r.peer == peer && !r.away)
        .or_else(|| table.iter().find(|r| r.peer == peer))
        .map(|r| r.seat)
}

/// The connected peer on `seat` in a table (0 while away or unknown).
pub fn peer_in(table: &[SeatInfo], seat: Seat) -> PeerId {
    table
        .iter()
        .find(|r| r.seat == seat && !r.away)
        .map_or(0, |r| r.peer)
}

/// What changed between two tables, as the events a client reports: (seat dropped), (seat rejoined
/// by peer), (seat released). New seats report nothing (`peer_joined` covers them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeatChange {
    Dropped(Seat),
    /// `old` held the seat before it went away.
    Rejoined {
        seat: Seat,
        peer: PeerId,
        old: PeerId,
    },
    Released(Seat),
}

pub fn changes(before: &[SeatInfo], after: &[SeatInfo]) -> Vec<SeatChange> {
    let mut out = Vec::new();
    for b in before {
        match after.iter().find(|a| a.seat == b.seat) {
            None => out.push(SeatChange::Released(b.seat)),
            Some(a) if !b.away && a.away => out.push(SeatChange::Dropped(b.seat)),
            Some(a) if b.away && !a.away => out.push(SeatChange::Rejoined {
                seat: a.seat,
                peer: a.peer,
                old: b.peer,
            }),
            Some(a) if !b.away && !a.away && a.peer != b.peer => {
                // Dropped and rejoined between two tables.
                out.push(SeatChange::Dropped(b.seat));
                out.push(SeatChange::Rejoined {
                    seat: a.seat,
                    peer: a.peer,
                    old: b.peer,
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seated(c: Claim) -> Seat {
        match c {
            Claim::Seated(s) => s,
            other => panic!("expected a new seat, got {other:?}"),
        }
    }

    #[test]
    fn keys_map_to_the_lowest_free_seats() {
        let mut s = Seats::default();
        assert_eq!(seated(s.claim("host", 1, None)), 1);
        assert_eq!(seated(s.claim("alice", 7, None)), 2);
        assert_eq!(seated(s.claim("bob", 9, None)), 3);
        assert_eq!(s.seat_of(7), Some(2));
        assert_eq!(s.peer_of(3), 9);
        assert_eq!(s.seat_of(4), None);
        assert_eq!(s.peer_of(4), 0);
        assert_eq!(s.len(), 3);
        assert_eq!(s.away(), 0);
    }

    #[test]
    fn the_same_key_returns_to_its_seat() {
        let mut s = Seats::default();
        s.set_hold(Hold::Session);
        s.claim("alice", 7, None);
        s.claim("bob", 9, None);
        assert_eq!(s.drop_peer(7, 100), Some(Dropped::Held(1)));
        assert_eq!(s.peer_of(1), 0, "away");
        assert!(s.is_away(1));
        assert_eq!(
            s.seat_of(7),
            Some(1),
            "the last holder still names the seat"
        );
        assert_eq!(s.away(), 1);
        // Someone new does not take the held seat.
        assert_eq!(seated(s.claim("carol", 11, None)), 3);
        assert_eq!(
            s.claim("alice", 12, None),
            Claim::Rejoined {
                seat: 1,
                old_peer: 7
            }
        );
        assert_eq!(s.peer_of(1), 12);
        assert_eq!(s.seat_of(12), Some(1));
        assert_eq!(s.seat_of(7), None, "the old id is forgotten once rejoined");
        assert_eq!(s.away(), 0);
    }

    #[test]
    fn a_key_whose_seat_is_connected_is_refused() {
        let mut s = Seats::default();
        s.claim("alice", 7, None);
        assert_eq!(s.claim("alice", 8, None), Claim::Taken { seat: 1, peer: 7 });
        assert_eq!(s.peer_of(1), 7, "the connected peer keeps it");
    }

    #[test]
    fn a_seat_without_a_key_is_released_at_once() {
        let mut s = Seats::default();
        s.set_hold(Hold::Session);
        assert_eq!(seated(s.claim("", 7, None)), 1);
        assert_eq!(seated(s.claim("", 8, None)), 2, "empty keys never match");
        assert!(s.keys().is_empty(), "nothing to save");
        assert_eq!(s.drop_peer(7, 0), Some(Dropped::Released(1)));
    }

    #[test]
    fn a_zero_window_releases_at_once() {
        let mut s = Seats::default();
        s.set_hold(Hold::from_seconds(0.0, 60));
        s.claim("alice", 7, None);
        assert_eq!(s.drop_peer(7, 5), Some(Dropped::Released(1)));
        assert!(s.is_empty());
        assert_eq!(s.drop_peer(7, 5), None, "no seat any more");
        assert_eq!(seated(s.claim("alice", 8, None)), 1, "a fresh seat");
    }

    #[test]
    fn a_positive_window_expires_on_the_fake_clock() {
        let mut s = Seats::default();
        s.set_hold(Hold::from_seconds(0.5, 60));
        assert_eq!(s.hold(), Hold::Ticks(30));
        s.claim("alice", 7, None);
        s.claim("bob", 9, None);
        s.drop_peer(7, 100);
        assert!(s.expire(129).is_empty(), "29 ticks: still held");
        assert_eq!(s.expire(130), std::vec![(1, 7)]);
        assert_eq!(s.len(), 1);
        assert_eq!(
            seated(s.claim("alice", 12, None)),
            1,
            "a new seat after the release"
        );
        // Changing the window applies to seats already away.
        s.drop_peer(9, 200);
        s.set_hold(Hold::Released);
        assert_eq!(s.expire(200), std::vec![(2, 9)]);
    }

    #[test]
    fn a_negative_window_holds_for_the_session() {
        let mut s = Seats::default();
        s.set_hold(Hold::from_seconds(-1.0, 60));
        assert_eq!(s.hold(), Hold::Session);
        s.claim("alice", 7, None);
        s.drop_peer(7, 0);
        assert!(s.expire(u64::MAX).is_empty());
        assert!(matches!(
            s.claim("alice", 8, None),
            Claim::Rejoined { seat: 1, .. }
        ));
    }

    #[test]
    fn reserved_seats_wait_for_their_key_and_never_expire() {
        let mut s = Seats::default();
        s.set_hold(Hold::Ticks(1));
        s.reserve(2, "bob");
        s.reserve(1, "alice");
        assert_eq!(s.away(), 2);
        assert!(s.expire(1_000).is_empty());
        assert_eq!(
            seated(s.claim("carol", 5, None)),
            3,
            "reserved numbers stay free"
        );
        assert_eq!(seated(s.claim("bob", 6, None)), 2, "the saved seat");
        assert_eq!(s.keys().len(), 3);
        // A restored host takes the saved host seat, re-keyed.
        assert_eq!(seated(s.claim("new host key", 1, Some(1))), 1);
        assert_eq!(s.keys()[0], (1, String::from("new host key")));
    }

    #[test]
    fn a_client_derives_drops_rejoins_and_releases_from_tables() {
        let row = |seat, peer, away| SeatInfo { seat, peer, away };
        let a = std::vec![row(1, 1, false), row(2, 7, false)];
        let b = std::vec![row(1, 1, false), row(2, 7, true)];
        let c = std::vec![row(1, 1, false), row(2, 12, false)];
        let d = std::vec![row(1, 1, false)];
        assert_eq!(changes(&a, &b), std::vec![SeatChange::Dropped(2)]);
        assert_eq!(
            changes(&b, &c),
            std::vec![SeatChange::Rejoined {
                seat: 2,
                peer: 12,
                old: 7
            }]
        );
        assert_eq!(changes(&c, &d), std::vec![SeatChange::Released(2)]);
        assert_eq!(seat_in(&b, 7), Some(2));
        assert_eq!(peer_in(&b, 2), 0);
        assert_eq!(peer_in(&c, 2), 12);
    }
}

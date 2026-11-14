//! The `handoff` physics mode (docs/PLAN_NETCODE.md 12.4, WP19; WP15's scope): a body is simulated by
//! whoever is interacting with it. This module is the sans-IO part: the server's arbitration of claims
//! by ownership sequence number, which holder states it accepts, the holder's return timers, and the
//! per-session bookkeeping `Session` keeps (session.rs applies it to the entity table and the wire).
//!
//! **The holder is the owner.** A `handoff` entity's holder is its owner (WP7), and the server sends
//! `client_owned = true` with the mode, so everything WP7 does for a `client_owned` entity follows: the
//! holder simulates it and sends its state, the server applies and relays it, nobody else simulates
//! it. The server holds a body nobody touches, so an untouched pile is a `server` pile.
//!
//! **Sequence numbers.** Every entity has an ownership sequence number (`EntityRecord::owner_seq`),
//! raised by every change of owner and carried by `SetOwner` and `Create`. A claim
//! (`EntityAction::Claim`) carries the number its sender saw plus one. The server takes one tick's
//! claims for an entity together ([`arbitrate`]): a claim is valid when its number is the current one
//! plus one (a lower number is stale: its sender had not seen the last change) and, for a release, when
//! it comes from the holder. Among the valid claims of one tick the lowest peer id wins (a release
//! counts as the server's, peer 1, so it beats a claim in the same tick); across ticks the first wins,
//! because the winner raised the number. While the server holds a body and its own player (a listen
//! server) touched it within [`SERVER_TOUCH_GUARD_TICKS`], a client's claim is refused: the server
//! is already the authority that touches it. A refused claimant gets a `SetOwner` naming the holder,
//! the number and the state.
//!
//! **States.** Owner states for a `handoff` body are accepted only from its holder, and only those its
//! holder simulated at or after the tick of its claim ([`accepts_state`]); the others (late packets
//! from a previous holder) are dropped without a reply. A claimant's states that arrive before the
//! server decided its claim wait beside it and join the queue if the claim wins. The holder stamps a state with its predicted tick, and the values it records in a tick are
//! the pose the body had while that tick's inputs ran (they are read before the tick's physics step);
//! the server applies the state at that same server tick, before its physics, so its copy of the
//! holder's player, running the same tick's inputs, meets the body where the holder's player met it
//! ([`StateQueue`]).
//!
//! **Returns.** The holder gives a body back ([`Holding`]) when it has rested for `handoff_rest_s`, or
//! when nothing of the holder's has touched it, nor its player been near it, for that long; a body
//! claimed by game code (`Net.claim`) is held until the game releases it. The server takes it back at
//! once when the holder leaves (or its seat is dropped) and when the body leaves the holder's scope.
//! Whenever the server takes a body back it goes on from the holder's state for the server's present
//! tick: the states the holder sent for later ticks (it runs ahead by its prediction lead) are
//! dropped, so a moving body does not jump.

use crate::peers::{PeerId, SERVER};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// Ticks (100 ms at 60 Hz) after its own player touched a body the server holds during which a client's
/// claim for it is refused.
pub const SERVER_TOUCH_GUARD_TICKS: u64 = 6;

/// One claim (or release) the server received for an entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub peer: PeerId,
    /// The sequence number the sender saw plus one.
    pub seq: u32,
    /// The sender's predicted tick when it claimed.
    pub tick: u64,
    /// A release (from the holder) instead of a claim.
    pub release: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Refuse,
}

/// The result of one tick's arbitration for one entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The holder after the tick (unchanged when every claim was refused).
    pub holder: PeerId,
    pub seq: u32,
    /// The index of the accepted claim, if any.
    pub winner: Option<usize>,
    /// Per claim, in order.
    pub verdicts: Vec<Verdict>,
}

/// The server's decision on one tick's `claims` for an entity held by `holder` at sequence number
/// `seq`; `touched` is the last tick the server's own player touched it, `now` the server tick (see the
/// module docs for the rules).
pub fn arbitrate(
    holder: PeerId,
    seq: u32,
    claims: &[Claim],
    touched: Option<u64>,
    now: u64,
) -> Outcome {
    let next = seq.wrapping_add(1);
    let guarded = holder == SERVER
        && touched.is_some_and(|t| now.saturating_sub(t) <= SERVER_TOUCH_GUARD_TICKS);
    let valid = |c: &Claim| {
        c.seq == next
            && if c.release {
                c.peer == holder && holder != SERVER
            } else {
                c.peer != holder && !(guarded && c.peer != SERVER)
            }
    };
    // A release counts as the server's: the lowest peer id.
    let rank = |c: &Claim| if c.release { SERVER } else { c.peer };
    let winner = claims
        .iter()
        .enumerate()
        .filter(|(_, c)| valid(c))
        .min_by_key(|(i, c)| (rank(c), *i))
        .map(|(i, _)| i);
    let verdicts = (0..claims.len())
        .map(|i| {
            if Some(i) == winner {
                Verdict::Accept
            } else {
                Verdict::Refuse
            }
        })
        .collect();
    match winner {
        Some(i) => Outcome {
            holder: if claims[i].release {
                SERVER
            } else {
                claims[i].peer
            },
            seq: next,
            winner,
            verdicts,
        },
        None => Outcome {
            holder,
            seq,
            winner: None,
            verdicts,
        },
    }
}

/// Whether the server takes an owner state of a `handoff` body from `from`, simulated at the sender's
/// tick `tick`: only from a client holder, from the tick of its claim on.
pub fn accepts_state(holder: PeerId, held_since: u64, from: PeerId, tick: u64) -> bool {
    holder != SERVER && from == holder && tick >= held_since
}

/// Server: the accepted holder states of one body, each applied at the server tick it is stamped with.
#[derive(Clone, Debug, Default)]
pub struct StateQueue {
    items: VecDeque<(u64, Vec<u8>)>,
    newest: Option<u64>,
}

impl StateQueue {
    /// Queues a state of tick `tick`; false (dropped) when it is not newer than the newest queued.
    pub fn push(&mut self, tick: u64, bytes: Vec<u8>) -> bool {
        if self.newest.is_some_and(|n| tick <= n) {
            return false;
        }
        self.newest = Some(tick);
        self.items.push_back((tick, bytes));
        true
    }

    /// The states due at server tick `now` (stamped `now` or earlier), oldest first.
    pub fn due(&mut self, now: u64) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.items.front().is_some_and(|(t, _)| *t <= now) {
            if let Some((_, b)) = self.items.pop_front() {
                out.push(b);
            }
        }
        out
    }

    /// Everything queued, oldest first (the holder is leaving: its last states apply now).
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        self.items.drain(..).map(|(_, b)| b).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Why a holder gives a body back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Return {
    /// It rested (asleep, or slower than the rest speed) for the rest time.
    Rest,
    /// Nothing of the holder's touched it, and its player was not near it, for the rest time.
    Away,
}

/// The holder's timers for one body it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Holding {
    /// Claimed by game code (`Net.claim`): held until the game releases it.
    pub sticky: bool,
    rest: f64,
    away: f64,
}

impl Holding {
    pub fn new(sticky: bool) -> Self {
        Self {
            sticky,
            rest: 0.0,
            away: 0.0,
        }
    }

    /// One tick of `dt` seconds: whether the body is at rest, and whether something of the holder's
    /// touched it (or its player is near it). Returns the reason to give it back once a timer reaches
    /// `rest_s`.
    pub fn step(&mut self, at_rest: bool, touched: bool, dt: f64, rest_s: f64) -> Option<Return> {
        self.rest = if at_rest { self.rest + dt } else { 0.0 };
        self.away = if touched { 0.0 } else { self.away + dt };
        if self.sticky {
            return None;
        }
        // Half a tick of slack so a rest time of N ticks returns on the Nth tick.
        let limit = rest_s.max(0.0) - dt * 0.5;
        if self.rest >= limit {
            Some(Return::Rest)
        } else if self.away >= limit {
            Some(Return::Away)
        } else {
            None
        }
    }
}

/// `stats()` counters. On the server: claims received (its own included), claims refused, bodies
/// returned to the server, holder states dropped, holder states that came late. On a client: claims
/// sent, claims lost, releases sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandoffStats {
    pub claims: u64,
    pub refused: u64,
    pub returns: u64,
    pub stale_states: u64,
    /// Server: holder states that arrived after the tick they were stamped with (applied at once).
    pub late_states: u64,
}

/// A holder state and the tick it was stamped with.
pub type Stamped = (u64, Vec<u8>);

/// What a `Session` keeps for `handoff` bodies.
#[derive(Clone, Debug, Default)]
pub struct Handoffs {
    /// Server: this tick's claims per entity, with the claimant's state.
    pub pending: BTreeMap<u32, Vec<(Claim, Vec<u8>)>>,
    /// Server: the last tick its own player touched each body.
    pub touched: BTreeMap<u32, u64>,
    /// Server: the claim tick of each body's current client holder.
    pub held_since: BTreeMap<u32, u64>,
    /// Server: the holder states waiting for their tick.
    pub queues: BTreeMap<u32, StateQueue>,
    /// Server: states from a claimant that arrived before its claim was decided, by (entity, peer).
    pub early: BTreeMap<(u32, PeerId), Vec<Stamped>>,
    /// Client: claims (and releases) this side sent and the server has not answered: the sequence
    /// number each named, and whether it was a release.
    pub claimed: BTreeMap<u32, (u32, bool)>,
    /// Client: claims and releases to send with the next packets: (entity, sequence number, release).
    pub out: Vec<(u32, u32, bool)>,
    pub stats: HandoffStats,
}

impl Handoffs {
    /// Server: forgets the holder bookkeeping of `id` (its holder changed or it is gone).
    pub fn forget_holder(&mut self, id: u32) {
        self.held_since.remove(&id);
        self.queues.remove(&id);
    }

    /// Forgets everything about `id`.
    pub fn forget(&mut self, id: u32) {
        self.forget_holder(id);
        self.early.retain(|k, _| k.0 != id);
        self.pending.remove(&id);
        self.touched.remove(&id);
        self.claimed.remove(&id);
        self.out.retain(|o| o.0 != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(peer: PeerId, seq: u32) -> Claim {
        Claim {
            peer,
            seq,
            tick: 100,
            release: false,
        }
    }

    fn release(peer: PeerId, seq: u32) -> Claim {
        Claim {
            release: true,
            ..claim(peer, seq)
        }
    }

    #[test]
    fn the_first_valid_claim_wins_and_raises_the_sequence() {
        let o = arbitrate(SERVER, 4, &[claim(3, 5)], None, 200);
        assert_eq!((o.holder, o.seq, o.winner), (3, 5, Some(0)));
        assert_eq!(o.verdicts, [Verdict::Accept]);
        // A claim in a later tick for the same number is stale now.
        let o = arbitrate(o.holder, o.seq, &[claim(2, 5)], None, 201);
        assert_eq!((o.holder, o.seq, o.winner), (3, 5, None));
        assert_eq!(o.verdicts, [Verdict::Refuse]);
    }

    #[test]
    fn equal_numbers_in_one_tick_go_to_the_lowest_peer_id_whatever_the_arrival_order() {
        for claims in [[claim(7, 1), claim(3, 1)], [claim(3, 1), claim(7, 1)]] {
            let o = arbitrate(SERVER, 0, &claims, None, 10);
            assert_eq!((o.holder, o.seq), (3, 1));
            let accepted: Vec<PeerId> = claims
                .iter()
                .zip(&o.verdicts)
                .filter(|(_, v)| **v == Verdict::Accept)
                .map(|(c, _)| c.peer)
                .collect();
            assert_eq!(accepted, [3]);
        }
    }

    #[test]
    fn stale_and_future_numbers_are_refused() {
        let o = arbitrate(
            SERVER,
            9,
            &[claim(2, 9), claim(3, 8), claim(4, 11)],
            None,
            50,
        );
        assert_eq!((o.holder, o.seq, o.winner), (SERVER, 9, None));
        assert!(o.verdicts.iter().all(|v| *v == Verdict::Refuse));
    }

    #[test]
    fn a_body_the_servers_player_touches_is_refused_to_clients_for_the_guard() {
        let o = arbitrate(SERVER, 0, &[claim(2, 1)], Some(95), 100);
        assert_eq!((o.holder, o.winner), (SERVER, None));
        let o = arbitrate(
            SERVER,
            0,
            &[claim(2, 1)],
            Some(100 - SERVER_TOUCH_GUARD_TICKS - 1),
            100,
        );
        assert_eq!(o.holder, 2, "the guard is over");
        // The guard only covers a body the server holds.
        let o = arbitrate(5, 3, &[claim(2, 4)], Some(100), 100);
        assert_eq!(o.holder, 2);
    }

    #[test]
    fn a_release_returns_the_body_to_the_server_and_beats_a_claim_in_the_same_tick() {
        let o = arbitrate(3, 6, &[release(3, 7)], None, 10);
        assert_eq!((o.holder, o.seq), (SERVER, 7));
        let o = arbitrate(3, 6, &[claim(2, 7), release(3, 7)], None, 10);
        assert_eq!((o.holder, o.seq, o.winner), (SERVER, 7, Some(1)));
        // Only the holder can release, and only with the next number.
        let o = arbitrate(3, 6, &[release(2, 7), release(3, 6)], None, 10);
        assert_eq!((o.holder, o.winner), (3, None));
        // A claim names another peer than the holder (game code may take a held body).
        let o = arbitrate(3, 6, &[claim(3, 7), claim(4, 7)], None, 10);
        assert_eq!((o.holder, o.winner), (4, Some(1)));
    }

    #[test]
    fn the_server_may_take_a_body_back_like_a_claimant() {
        let o = arbitrate(3, 6, &[claim(SERVER, 7), claim(2, 7)], None, 10);
        assert_eq!((o.holder, o.seq), (SERVER, 7));
    }

    #[test]
    fn states_come_only_from_the_holder_from_its_claim_on() {
        assert!(accepts_state(3, 40, 3, 40));
        assert!(
            !accepts_state(3, 40, 3, 39),
            "a late state of an earlier holding"
        );
        assert!(!accepts_state(3, 40, 2, 45), "a previous holder");
        assert!(!accepts_state(SERVER, 0, 2, 45), "the server holds it");
    }

    #[test]
    fn queued_states_apply_at_their_tick_in_order_and_late_ones_are_dropped() {
        let mut q = StateQueue::default();
        assert!(q.push(10, std::vec![1]));
        assert!(q.push(12, std::vec![2]));
        assert!(!q.push(11, std::vec![9]), "older than the newest");
        assert!(!q.push(12, std::vec![9]), "a duplicate");
        assert!(q.due(9).is_empty(), "tick 10's state waits for tick 10");
        assert_eq!(q.due(11), [std::vec![1u8]]);
        assert!(q.push(13, std::vec![3]));
        assert_eq!(q.drain(), [std::vec![2u8], std::vec![3u8]]);
        assert!(q.is_empty());
    }

    #[test]
    fn a_holder_returns_after_resting_or_being_away_for_the_rest_time() {
        let dt = 1.0 / 60.0;
        let mut h = Holding::new(false);
        for _ in 0..29 {
            assert_eq!(h.step(true, true, dt, 0.5), None);
        }
        assert_eq!(h.step(true, true, dt, 0.5), Some(Return::Rest), "30 ticks");
        // Motion restarts the rest timer; a touch restarts the away timer.
        let mut h = Holding::new(false);
        for i in 0..100 {
            let r = h.step(i % 20 == 0, i % 10 == 0, dt, 0.5);
            assert_eq!(r, None, "tick {i}");
        }
        let mut h = Holding::new(false);
        let n = (0..100).find(|_| h.step(false, false, dt, 0.5).is_some());
        assert_eq!(n, Some(29));
        // Game code's claims are held until released.
        let mut h = Holding::new(true);
        assert!((0..600).all(|_| h.step(true, false, dt, 0.5).is_none()));
    }

    #[test]
    fn forgetting_a_holder_drops_its_queue() {
        let mut h = Handoffs::default();
        h.held_since.insert(4, 10);
        h.queues.entry(4).or_default().push(10, std::vec![1]);
        h.claimed.insert(4, (2, false));
        h.forget_holder(4);
        assert!(h.queues.is_empty() && h.held_since.is_empty());
        assert_eq!(h.claimed.len(), 1);
        h.forget(4);
        assert!(h.claimed.is_empty());
    }
}

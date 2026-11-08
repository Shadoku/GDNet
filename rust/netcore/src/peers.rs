//! The peer table (docs/PLAN_NETCODE.md 2.4, WP1): peer ids (1 is the server), join and leave, and per
//! peer the handshake stage, the last tick it was heard, RTT and jitter.

use crate::clock::RttEstimator;
use crate::wire::IdRange;
use std::collections::BTreeMap;
use std::vec::Vec;

/// A transport peer id, as Godot's `MultiplayerPeer` numbers them.
pub type PeerId = u32;

/// The server's id.
pub const SERVER: PeerId = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The transport connected; the handshake has not finished.
    Connecting,
    /// `Hello` / `Welcome` done: the peer is in the session.
    Joined,
    /// The handshake failed; the peer is waiting to be disconnected. Holds the tick it was rejected at.
    Rejected(u64),
}

#[derive(Clone, Debug)]
pub struct PeerState {
    pub id: PeerId,
    pub stage: Stage,
    pub last_heard_tick: u64,
    pub rtt: RttEstimator,
    pub id_range: IdRange,
}

impl PeerState {
    pub fn new(id: PeerId, tick: u64) -> Self {
        Self {
            id,
            stage: Stage::Connecting,
            last_heard_tick: tick,
            rtt: RttEstimator::default(),
            id_range: IdRange::default(),
        }
    }
}

/// All known peers, ordered by id.
#[derive(Clone, Debug, Default)]
pub struct PeerTable {
    peers: BTreeMap<PeerId, PeerState>,
}

impl PeerTable {
    /// Adds a peer in the `Connecting` stage; returns false when it was already known.
    pub fn insert(&mut self, id: PeerId, tick: u64) -> bool {
        if self.peers.contains_key(&id) {
            return false;
        }
        self.peers.insert(id, PeerState::new(id, tick));
        true
    }

    pub fn remove(&mut self, id: PeerId) -> Option<PeerState> {
        self.peers.remove(&id)
    }

    pub fn get(&self, id: PeerId) -> Option<&PeerState> {
        self.peers.get(&id)
    }

    pub fn get_mut(&mut self, id: PeerId) -> Option<&mut PeerState> {
        self.peers.get_mut(&id)
    }

    pub fn is_joined(&self, id: PeerId) -> bool {
        self.get(id).is_some_and(|p| p.stage == Stage::Joined)
    }

    /// Ids of every peer that finished the handshake, ascending.
    pub fn joined(&self) -> Vec<PeerId> {
        self.peers
            .values()
            .filter(|p| p.stage == Stage::Joined)
            .map(|p| p.id)
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &PeerState> {
        self.peers.values()
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_leave() {
        let mut t = PeerTable::default();
        assert!(t.insert(5, 0));
        assert!(t.insert(2, 0));
        assert!(!t.insert(5, 3), "already known");
        assert_eq!(t.joined(), Vec::<PeerId>::new());
        t.get_mut(5).unwrap().stage = Stage::Joined;
        t.get_mut(2).unwrap().stage = Stage::Joined;
        assert_eq!(t.joined(), std::vec![2, 5]);
        assert!(t.is_joined(2));
        assert_eq!(t.remove(2).map(|p| p.id), Some(2));
        assert!(!t.is_joined(2));
        assert_eq!(t.len(), 1);
    }
}

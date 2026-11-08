//! Hash telemetry (docs/PLAN_NETCODE.md 1 "What the developer sees", 2.4 `Hash`, WP8).
//!
//! A cheap 32-bit FNV-1a hash per entity over its hot fields' quantised encodings (the bytes a
//! snapshot carries), compared between the server and each client as a divergence count. It is
//! telemetry: nothing halts, nothing is corrected.
//!
//! Server: after each tick's snapshots, [`Replication::hash_eligibility`](crate::delta::Replication)
//! says per peer which sampled entities that peer can check at this tick: *settled* ones (every hot
//! field acknowledged at its current version, so the peer holds exactly the current values whatever
//! it receives later) and *carried* ones (the rest acknowledged, the changed fields in this very
//! snapshot, so the peer holds the current values once it applied this tick's snapshot). Their hashes
//! go in a `Hash` message (channel 2, unreliable). The sample is `sample` entities per tick round-robin
//! plus every entity the peer owns when it owns at most [`OWNED_EVERY_TICK_MAX`]; a sampled entity whose
//! hash did not change since that peer last got it is skipped until [`REFRESH_S`] passed (so an idle
//! world sends almost nothing).
//!
//! Client: [`HashRx`] keeps the encodings of the hot fields the server sent (snapshots, reliable
//! fields, a `Create`'s initial state), re-encoded from the decoded values (so a quantiser that does
//! not round-trip shows up too), and per entity a short history of `(tick, hash)` after each applied
//! snapshot. A settled entry compares with the newest history entry at or before its tick; a carried
//! one only when this client applied that tick's snapshot (it waits in a small stash when the snapshot
//! has not arrived yet, and is skipped when it was lost). A replica legitimately lags: only equal ticks
//! are compared, and an unknown tick is skipped, never counted.

use crate::entity::NetId;
use crate::quant::Encoded;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// FNV-1a 32 offset basis.
pub const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Entities sampled per tick by default (`gdnet/debug/hash_sample`).
pub const DEFAULT_SAMPLE: u32 = 16;
/// An entity whose hash did not change since a peer last got it is sent again after this many seconds
/// (`gdnet/debug/hash_refresh_s`), so an idle world costs almost nothing (gate G4).
pub const REFRESH_S: u64 = 10;
/// A peer owning at most this many entities gets all of them checked every tick.
pub const OWNED_EVERY_TICK_MAX: usize = 8;
/// History entries kept per entity on a client.
pub const HISTORY: usize = 32;
/// Applied snapshot ticks remembered on a client.
pub const APPLIED: usize = 64;
/// `Hash` messages waiting for their snapshot on a client.
pub const STASH_MAX: usize = 16;

/// Folds `bytes` into an FNV-1a 32 hash.
pub fn fnv1a(mut h: u32, bytes: &[u8]) -> u32 {
    for b in bytes {
        h ^= u32::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Folds one field (its address and its encoding) into `h`.
pub fn fold_field(h: u32, part: u16, field: u16, enc: &Encoded) -> u32 {
    let h = fnv1a(h, &part.to_le_bytes());
    let h = fnv1a(h, &field.to_le_bytes());
    let h = fnv1a(h, &enc.bits.to_le_bytes());
    fnv1a(h, &enc.data)
}

/// The hash of an entity's fields, given in (part, field) order.
pub fn entity_hash<'a>(fields: impl IntoIterator<Item = (u16, u16, &'a Encoded)>) -> u32 {
    fields
        .into_iter()
        .fold(FNV_OFFSET, |h, (p, f, e)| fold_field(h, p, f, e))
}

/// Folds an entity hash into a whole-state hash (entities in id order).
pub fn fold_entity(h: u32, id: NetId, entity: u32) -> u32 {
    fnv1a(fnv1a(h, &id.to_le_bytes()), &entity.to_le_bytes())
}

/// The `Hash` message (kind 17, channel 2): sampled per-entity hashes of server tick `tick`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashMsg {
    pub tick: u64,
    /// Entities whose every hot field the peer acknowledged at its current version.
    pub settled: Vec<(NetId, u32)>,
    /// Entities whose changed hot fields are in this tick's snapshot (the rest acknowledged).
    pub carried: Vec<(NetId, u32)>,
}

impl HashMsg {
    pub fn is_empty(&self) -> bool {
        self.settled.is_empty() && self.carried.is_empty()
    }

    pub fn len(&self) -> usize {
        self.settled.len() + self.carried.len()
    }
}

/// Hash telemetry counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashStats {
    /// Entity hashes sent (server).
    pub sent: u64,
    /// Entity hashes received (client).
    pub received: u64,
    /// Received hashes compared at an equal tick.
    pub compared: u64,
    /// Compared hashes that differed.
    pub divergences: u64,
    /// Received hashes not compared (tick unknown, snapshot lost, entity unknown).
    pub skipped: u64,
}

/// The server side's sampling state.
#[derive(Clone, Debug, Default)]
pub struct HashTx {
    /// The next entity id the round-robin starts after.
    pub cursor: NetId,
}

impl HashTx {
    /// The next `n` ids of `ids` (ascending) after the cursor, wrapping; advances the cursor.
    pub fn sample(&mut self, ids: &[NetId], n: usize) -> Vec<NetId> {
        if ids.is_empty() || n == 0 {
            return Vec::new();
        }
        let start = ids.partition_point(|&id| id <= self.cursor);
        let take = n.min(ids.len());
        let out: Vec<NetId> = (0..take).map(|i| ids[(start + i) % ids.len()]).collect();
        if let Some(&last) = out.last() {
            self.cursor = last;
        }
        out
    }
}

/// The client side: held encodings, per-entity histories, the comparison.
#[derive(Clone, Debug, Default)]
pub struct HashRx {
    held: BTreeMap<NetId, BTreeMap<(u16, u16), Encoded>>,
    history: BTreeMap<NetId, VecDeque<(u64, u32)>>,
    dirty: BTreeMap<NetId, u64>,
    applied: VecDeque<u64>,
    last_applied: Option<u64>,
    stash: VecDeque<HashMsg>,
    divergences: BTreeMap<NetId, u64>,
    pub stats: HashStats,
}

impl HashRx {
    /// The server sent this encoding of a hot field of `id` with a value of server tick `tick` (0 for a
    /// `Create`'s initial state: held since the entity exists).
    pub fn note(&mut self, id: NetId, part: u16, field: u16, enc: Encoded, tick: u64) {
        self.held.entry(id).or_default().insert((part, field), enc);
        let t = self.dirty.entry(id).or_insert(0);
        *t = (*t).max(tick);
    }

    /// Records the hashes of the entities changed since the last commit.
    pub fn commit(&mut self) {
        for (id, tick) in std::mem::take(&mut self.dirty) {
            let Some(fields) = self.held.get(&id) else {
                continue;
            };
            let h = entity_hash(fields.iter().map(|((p, f), e)| (*p, *f, e)));
            let hist = self.history.entry(id).or_default();
            match hist.back_mut() {
                // Same tick again (a reliable field beside the snapshot): the newer state wins.
                Some(last) if last.0 >= tick => last.1 = h,
                Some(last) if last.1 == h => {}
                _ => hist.push_back((tick, h)),
            }
            while hist.len() > HISTORY {
                hist.pop_front();
            }
        }
    }

    /// This client applied the snapshot of server tick `tick`; stashed carried hashes it was waiting
    /// for are compared.
    pub fn applied(&mut self, tick: u64) {
        self.applied.push_back(tick);
        while self.applied.len() > APPLIED {
            self.applied.pop_front();
        }
        self.last_applied = Some(self.last_applied.map_or(tick, |t| t.max(tick)));
        let stash = std::mem::take(&mut self.stash);
        for m in stash {
            if m.tick <= tick {
                self.compare_carried(&m);
            } else {
                self.stash.push_back(m);
            }
        }
    }

    /// A `Hash` message from the server.
    pub fn on_msg(&mut self, m: HashMsg) {
        self.stats.received += m.len() as u64;
        for &(id, h) in &m.settled {
            self.compare_at(id, m.tick, h);
        }
        if m.carried.is_empty() {
            return;
        }
        if self.applied.contains(&m.tick) || self.last_applied.is_some_and(|t| t >= m.tick) {
            self.compare_carried(&m);
        } else {
            if self.stash.len() >= STASH_MAX {
                if let Some(old) = self.stash.pop_front() {
                    self.stats.skipped += old.carried.len() as u64;
                }
            }
            self.stash.push_back(HashMsg {
                tick: m.tick,
                settled: Vec::new(),
                carried: m.carried,
            });
        }
    }

    fn compare_carried(&mut self, m: &HashMsg) {
        if !self.applied.contains(&m.tick) {
            self.stats.skipped += m.carried.len() as u64;
            return;
        }
        for &(id, h) in &m.carried {
            self.compare_at(id, m.tick, h);
        }
    }

    fn compare_at(&mut self, id: NetId, tick: u64, h: u32) {
        match self.hash_at(id, tick) {
            None => self.stats.skipped += 1,
            Some(mine) => {
                self.stats.compared += 1;
                if mine != h {
                    self.stats.divergences += 1;
                    *self.divergences.entry(id).or_insert(0) += 1;
                }
            }
        }
    }

    /// This client's hash of `id` as of server tick `tick` (the newest history entry at or before it).
    pub fn hash_at(&self, id: NetId, tick: u64) -> Option<u32> {
        self.history
            .get(&id)?
            .iter()
            .rev()
            .find(|(t, _)| *t <= tick)
            .map(|(_, h)| *h)
    }

    /// The entities this client holds hashed state for.
    pub fn ids(&self) -> impl Iterator<Item = NetId> + '_ {
        self.history.keys().copied()
    }

    /// Divergences counted for `id`.
    pub fn divergences_of(&self, id: NetId) -> u64 {
        self.divergences.get(&id).copied().unwrap_or(0)
    }

    /// The newest applied snapshot tick.
    pub fn last_applied(&self) -> Option<u64> {
        self.last_applied
    }

    /// Drops what is held for `id` (destroyed, or now simulated here); its divergence count stays
    /// unless `counts` is set.
    pub fn forget(&mut self, id: NetId, counts: bool) {
        self.held.remove(&id);
        self.history.remove(&id);
        self.dirty.remove(&id);
        if counts {
            self.divergences.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(data: &[u8]) -> Encoded {
        Encoded {
            bits: (data.len() * 8) as u32,
            data: data.to_vec(),
        }
    }

    #[test]
    fn equal_fields_hash_equal_and_any_change_differs() {
        let a = [enc(&[1, 2, 3]), enc(&[9])];
        let b = [enc(&[1, 2, 3]), enc(&[9])];
        let ha = entity_hash(a.iter().enumerate().map(|(i, e)| (0, i as u16, e)));
        let hb = entity_hash(b.iter().enumerate().map(|(i, e)| (0, i as u16, e)));
        assert_eq!(ha, hb);
        let c = [enc(&[1, 2, 4]), enc(&[9])];
        let hc = entity_hash(c.iter().enumerate().map(|(i, e)| (0, i as u16, e)));
        assert_ne!(ha, hc);
        // The same bytes at another address differ too.
        let hd = entity_hash(a.iter().enumerate().map(|(i, e)| (1, i as u16, e)));
        assert_ne!(ha, hd);
        assert_eq!(fnv1a(FNV_OFFSET, b""), FNV_OFFSET);
        assert_eq!(fnv1a(FNV_OFFSET, b"a"), 0xe40c_292c);
    }

    #[test]
    fn sampling_round_robins_and_wraps() {
        let ids: Vec<NetId> = (1..=5).collect();
        let mut tx = HashTx::default();
        assert_eq!(tx.sample(&ids, 2), [1, 2]);
        assert_eq!(tx.sample(&ids, 2), [3, 4]);
        assert_eq!(tx.sample(&ids, 2), [5, 1]);
        assert_eq!(tx.sample(&ids, 9), [2, 3, 4, 5, 1]);
        assert!(tx.sample(&[], 3).is_empty());
        // A removed cursor id: continues after it.
        let mut tx = HashTx { cursor: 3 };
        assert_eq!(tx.sample(&[1, 2, 4, 6], 2), [4, 6]);
    }

    fn rx_with(id: NetId, data: &[u8], tick: u64) -> HashRx {
        let mut rx = HashRx::default();
        rx.note(id, 0, 0, enc(data), tick);
        rx.commit();
        rx
    }

    fn h(data: &[u8]) -> u32 {
        entity_hash([(0u16, 0u16, &enc(data))])
    }

    #[test]
    fn settled_hashes_compare_with_the_state_at_or_before_their_tick() {
        let mut rx = rx_with(7, &[1], 10);
        rx.applied(10);
        rx.note(7, 0, 0, enc(&[2]), 20);
        rx.commit();
        rx.applied(20);
        // The server's hash of tick 15 is the tick-10 state: equal, no divergence.
        rx.on_msg(HashMsg {
            tick: 15,
            settled: std::vec![(7, h(&[1]))],
            carried: std::vec![],
        });
        assert_eq!(rx.stats.compared, 1);
        assert_eq!(rx.stats.divergences, 0);
        // A different hash at an equal tick is a divergence for that entity.
        rx.on_msg(HashMsg {
            tick: 25,
            settled: std::vec![(7, h(&[3]))],
            carried: std::vec![],
        });
        assert_eq!(rx.stats.divergences, 1);
        assert_eq!(rx.divergences_of(7), 1);
        // Before anything was held, or an unknown entity: skipped.
        rx.on_msg(HashMsg {
            tick: 5,
            settled: std::vec![(7, 0), (8, 0)],
            carried: std::vec![],
        });
        assert_eq!(rx.stats.skipped, 2);
        assert_eq!(rx.stats.divergences, 1);
    }

    #[test]
    fn carried_hashes_need_their_snapshot() {
        let mut rx = rx_with(7, &[1], 10);
        rx.applied(10);
        // Tick 12's snapshot was lost (13 applied): the carried hash of 12 is skipped, not counted,
        // although the state differs from what the server hashed.
        rx.note(7, 0, 0, enc(&[3]), 13);
        rx.commit();
        rx.applied(13);
        rx.on_msg(HashMsg {
            tick: 12,
            settled: std::vec![],
            carried: std::vec![(7, h(&[2]))],
        });
        assert_eq!(rx.stats.divergences, 0);
        assert_eq!(rx.stats.skipped, 1);
        // A hash that arrives before its snapshot waits, then compares at the equal tick.
        rx.on_msg(HashMsg {
            tick: 14,
            settled: std::vec![],
            carried: std::vec![(7, h(&[4]))],
        });
        assert_eq!(rx.stats.compared, 0);
        rx.note(7, 0, 0, enc(&[4]), 14);
        rx.commit();
        rx.applied(14);
        assert_eq!(rx.stats.compared, 1);
        assert_eq!(rx.stats.divergences, 0);
        // A wrong one at an applied tick counts.
        rx.on_msg(HashMsg {
            tick: 14,
            settled: std::vec![],
            carried: std::vec![(7, h(&[5]))],
        });
        assert_eq!(rx.stats.divergences, 1);
    }

    #[test]
    fn history_is_bounded_and_forget_drops_it() {
        let mut rx = HashRx::default();
        for t in 0..100u64 {
            rx.note(1, 0, 0, enc(&[t as u8]), t);
            rx.commit();
        }
        assert_eq!(rx.history[&1].len(), HISTORY);
        assert_eq!(rx.hash_at(1, 99), Some(h(&[99])));
        assert_eq!(rx.hash_at(1, 10), None);
        rx.forget(1, false);
        assert_eq!(rx.hash_at(1, 99), None);
    }

    /// The client re-encodes decoded values: every quantiser must give back the encoding it decoded.
    #[test]
    fn re_encoding_a_decoded_value_gives_the_same_bytes() {
        use crate::props::{Quant, TypeTag, Value};
        use crate::quant::{decode, encode, BitReader, BitWriter};
        let cases: Vec<(TypeTag, Quant, Value)> = std::vec![
            (
                TypeTag::Vector2,
                Quant::Mm,
                Value::Vector2([1234.5678, -0.0004])
            ),
            (TypeTag::Vector2, Quant::Cm, Value::Vector2([-3.217, 9.999])),
            (
                TypeTag::Vector3,
                Quant::Mm,
                Value::Vector3([1e5, -2.5, 3.0001])
            ),
            (TypeTag::Float, Quant::Deg, Value::Float(-3.1)),
            (TypeTag::Float, Quant::Deg, Value::Float(100.0)),
            (
                TypeTag::Vector3,
                Quant::Deg,
                Value::Vector3([0.5, -0.5, 3.0])
            ),
            (
                TypeTag::Quaternion,
                Quant::Smallest3,
                Value::Quaternion([0.1, -0.7, 0.2, 0.676])
            ),
            (
                TypeTag::Quaternion,
                Quant::Smallest3,
                Value::Quaternion([0.5, 0.5, 0.5, 0.5])
            ),
            (TypeTag::Float, Quant::F16, Value::Float(0.333)),
            (TypeTag::Vector2, Quant::Raw, Value::Vector2([0.1, 0.2])),
            (TypeTag::Float, Quant::Raw, Value::Float(0.1)),
            (
                TypeTag::Transform2D,
                Quant::Raw,
                Value::Transform2D([1.0, 0.0, 0.0, 1.0, 5.5, 6.5])
            ),
            (TypeTag::Int, Quant::Raw, Value::Int(-77)),
            (TypeTag::Bool, Quant::Raw, Value::Bool(true)),
        ];
        for (ty, q, v) in cases {
            let mut w = BitWriter::new();
            encode(&mut w, ty, q, &v);
            let first = Encoded::of(&w);
            let back = decode(&mut BitReader::new(&first.data), ty, q).expect("decodes");
            let mut w2 = BitWriter::new();
            encode(&mut w2, ty, q, &back);
            assert_eq!(Encoded::of(&w2), first, "{ty:?} {q:?} {v:?} -> {back:?}");
        }
    }
}

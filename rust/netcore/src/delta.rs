//! State replication (docs/PLAN_NETCODE.md 2.1 "Dirty tracking", 2.4 `Snapshot` / `Ack`, 2.7, WP3).
//!
//! The authority records each entity's property values (gdnet polls them: hot ones every tick, cold
//! ones every `cold_poll_ticks`). [`Replication::record_part`] encodes each value with its quantiser and
//! compares the encoding with the last one: a different encoding is a change and bumps the field's
//! version. Per peer and entity it keeps, per field, the version the peer is known to hold (`acked`) and
//! the version last sent to it (`sent`). A field is pending for a peer when its version is neither: the
//! peer's value differs and nothing in flight would fix it.
//!
//! Most-recent-state (Tribes): nothing is retransmitted. A snapshot records which (field, version)
//! pairs it carried; the receiver acknowledges the highest snapshot tick it applied plus a 32-bit
//! history of the ticks before it. A received snapshot raises `acked`; a lost one puts `sent` back to
//! `acked`, so its fields are pending again and the next snapshot carries their *current* values. When
//! no ack arrives for half a second while snapshots are in flight, the peer falls back to full state
//! (every field pending, baseline tick 0).
//!
//! Full state (an entity's every recorded field) goes in `Create` (`initial_state`) and in the late-join
//! burst: netcore keeps each entity's latest values, so the burst carries current state and the peer is
//! marked as holding it. Values too large for a snapshot (over [`RELIABLE_BYTES`]) and fields marked
//! `reliable` go as reliable `Field` messages instead.

use crate::entity::NetId;
use crate::peers::PeerId;
use crate::priority::{self, Budget};
use crate::props::{ScenePropsTable, Value};
use crate::quant::{encode, BitReader, BitWriter, Encoded};
use crate::snapshot::{self, Header};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// Values whose encoding is larger than this go on the reliable channel.
pub const RELIABLE_BYTES: usize = 1000;
/// At most this many snapshots per peer wait for an ack; older ones count as lost.
pub const MAX_IN_FLIGHT: usize = 64;

/// A property value a remote authority sent, for gdnet to apply.
#[derive(Clone, Debug, PartialEq)]
pub struct StateUpdate {
    pub id: NetId,
    pub part: u16,
    pub field: u16,
    pub value: Value,
    /// WP4: the server tick of the snapshot (or reliable field) that carried the value.
    pub tick: u64,
}

/// A field sent reliably (2.4: `reliable` properties and values too large for a snapshot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldMsg {
    pub tick: u64,
    pub id: NetId,
    pub part: u16,
    pub field: u16,
    pub bits: u32,
    pub data: Vec<u8>,
}

/// The `Ack` message: the highest snapshot tick applied and which of the 32 ticks before it were.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub tick: u64,
    pub history: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldState {
    pub enc: Encoded,
    /// 0 = never recorded.
    pub version: u32,
    /// The tick of the last change.
    pub last_change: u64,
}

#[derive(Clone, Debug)]
pub struct EntityState {
    pub scene: u64,
    pub fields: Vec<FieldState>,
    pub priority: f32,
}

#[derive(Clone, Debug)]
struct PeerEntity {
    acked: Vec<u32>,
    sent: Vec<u32>,
    accum: f64,
    interacting: bool,
    /// WP14: snapshots that carried this entity to the peer (diagnostics).
    sends: u64,
}

impl PeerEntity {
    fn new(n: usize) -> Self {
        Self {
            acked: std::vec![0; n],
            sent: std::vec![0; n],
            accum: 0.0,
            interacting: false,
            sends: 0,
        }
    }

    fn fit(&mut self, n: usize) {
        if self.acked.len() != n {
            self.acked.resize(n, 0);
            self.sent.resize(n, 0);
        }
    }
}

#[derive(Clone, Debug)]
struct InFlight {
    tick: u64,
    sent_at: u64,
    fields: Vec<(NetId, u32, u32)>,
}

#[derive(Clone, Debug, Default)]
struct PeerRepl {
    entities: BTreeMap<NetId, PeerEntity>,
    in_flight: VecDeque<InFlight>,
    last_ack_at: u64,
    acked_tick: u64,
    budget: Budget,
}

/// The receiving side's bookkeeping (a client, from the server).
#[derive(Clone, Debug, Default)]
struct Rx {
    last_applied: Option<u64>,
    ack_any: bool,
    ack_tick: u64,
    ack_history: u32,
    ack_due: bool,
    reliable_ticks: BTreeMap<(NetId, u16, u16), u64>,
}

impl Rx {
    fn mark_received(&mut self, t: u64) {
        if !self.ack_any {
            self.ack_any = true;
            self.ack_tick = t;
            self.ack_history = 0;
        } else if t > self.ack_tick {
            let d = t - self.ack_tick;
            let h = u64::from(self.ack_history);
            let shifted = if d >= 64 {
                0
            } else {
                (h << d) | (1u64 << (d - 1))
            };
            self.ack_history = shifted as u32;
            self.ack_tick = t;
        } else {
            let d = self.ack_tick - t;
            if (1..=32).contains(&d) {
                self.ack_history |= 1 << (d - 1);
            }
        }
        self.ack_due = true;
    }
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplStats {
    pub snapshots_sent: u64,
    pub snapshot_bytes: u64,
    pub entities_sent: u64,
    pub fields_sent: u64,
    pub reliable_fields_sent: u64,
    pub snapshots_received: u64,
    pub snapshots_stale: u64,
    /// Received snapshots applied but not acknowledged (they named an entity not known yet).
    pub snapshots_unacked: u64,
    pub fields_received: u64,
    pub fallbacks: u64,
    pub decode_errors: u64,
    /// Fields recorded with a new value.
    pub changes: u64,
    /// WP14 (budget degradation): candidates with pending fields that a snapshot left out entirely
    /// (the budget or the MTU ran out first), summed over peers and ticks; `_tick` = this tick's.
    pub deferred_entities: u64,
    /// WP14: pending fields a snapshot did not carry (left-out entities and the rest of partly sent
    /// ones).
    pub deferred_fields: u64,
    pub deferred_entities_tick: u64,
    pub deferred_fields_tick: u64,
}

impl ReplStats {
    fn defer(&mut self, entities: usize, fields: usize) {
        self.deferred_entities += entities as u64;
        self.deferred_fields += fields as u64;
        self.deferred_entities_tick += entities as u64;
        self.deferred_fields_tick += fields as u64;
    }
}

/// What a snapshot pass produced for one peer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outbound {
    /// The snapshot body (after the kind byte), if any entity fitted.
    pub snapshot: Option<Vec<u8>>,
    pub fields: Vec<FieldMsg>,
}

#[derive(Clone, Debug, Default)]
pub struct Replication {
    tables: BTreeMap<u64, ScenePropsTable>,
    entities: BTreeMap<NetId, EntityState>,
    peers: BTreeMap<PeerId, PeerRepl>,
    rx: Rx,
    updates: VecDeque<StateUpdate>,
    scratch: BitWriter,
    pub stats: ReplStats,
}

impl Replication {
    pub fn register_table(&mut self, scene: u64, table: ScenePropsTable) {
        self.tables.insert(scene, table);
    }

    pub fn table(&self, scene: u64) -> Option<&ScenePropsTable> {
        self.tables.get(&scene)
    }

    pub fn entity(&self, id: NetId) -> Option<&EntityState> {
        self.entities.get(&id)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    fn new_state(&self, scene: u64) -> Option<EntityState> {
        let n = self.tables.get(&scene)?.field_count();
        Some(EntityState {
            scene,
            fields: std::vec![FieldState::default(); n],
            priority: 1.0,
        })
    }

    /// Starts tracking `id` (of a scene with a registered table) with every part's values; returns its
    /// full state for the `Create`. Empty bytes when the scene has no table.
    pub fn add_entity(
        &mut self,
        id: NetId,
        scene: u64,
        values: &[Vec<Value>],
        tick: u64,
    ) -> Vec<u8> {
        let Some(state) = self.new_state(scene) else {
            return Vec::new();
        };
        self.entities.insert(id, state);
        for (part, vals) in values.iter().enumerate() {
            let indexed: Vec<(u16, Value)> = vals
                .iter()
                .enumerate()
                .map(|(f, v)| (f as u16, v.clone()))
                .collect();
            self.record_part(id, scene, part as u16, &indexed, tick);
        }
        self.encode_full(id)
    }

    pub fn remove_entity(&mut self, id: NetId) {
        self.entities.remove(&id);
        for p in self.peers.values_mut() {
            p.entities.remove(&id);
        }
    }

    pub fn set_priority(&mut self, id: NetId, priority: f32) {
        if let Some(e) = self.entities.get_mut(&id) {
            e.priority = priority;
        }
    }

    /// Records the current values of fields of one part (`(field index, value)`). An entity not tracked
    /// yet starts with nothing known. Slow fields (`slow_ticks > 0`) only take a new value when another
    /// field of the part changed in this call or `slow_ticks` passed since their last change. Returns
    /// true when anything changed.
    pub fn record_part(
        &mut self,
        id: NetId,
        scene: u64,
        part: u16,
        values: &[(u16, Value)],
        tick: u64,
    ) -> bool {
        if !self.entities.contains_key(&id) {
            match self.new_state(scene) {
                Some(s) => {
                    self.entities.insert(id, s);
                }
                None => return false,
            }
        }
        let Some(table) = self.tables.get(&scene) else {
            return false;
        };
        let Some(state) = self.entities.get_mut(&id) else {
            return false;
        };
        if state.scene != scene {
            return false;
        }
        let Some(descs) = table.parts.get(usize::from(part)) else {
            return false;
        };
        let base: usize = table.parts[..usize::from(part)].iter().map(Vec::len).sum();
        let mut changed = false;
        for pass in 0..2 {
            for (f, v) in values {
                let Some(d) = descs.get(usize::from(*f)) else {
                    continue;
                };
                let slow = d.slow_ticks > 0;
                if (pass == 0) == slow {
                    continue;
                }
                let fs = &mut state.fields[base + usize::from(*f)];
                if slow
                    && !changed
                    && fs.version != 0
                    && tick.saturating_sub(fs.last_change) < u64::from(d.slow_ticks)
                {
                    continue;
                }
                self.scratch.clear();
                encode(&mut self.scratch, d.ty, d.quant, v);
                if fs.version != 0 && fs.enc.same_as(&self.scratch) {
                    continue;
                }
                fs.enc = Encoded::of(&self.scratch);
                fs.version += 1;
                fs.last_change = tick;
                self.stats.changes += 1;
                if !slow {
                    changed = true;
                }
            }
        }
        changed
    }

    /// The entity's every recorded field as a header-less entry (decoded by [`Replication::decode_state`]).
    pub fn encode_full(&self, id: NetId) -> Vec<u8> {
        self.encode_full_for(id, true)
    }

    /// WP16: [`Replication::encode_full`] for one receiver: without the `audience: owner` fields
    /// unless `owner` (the receiver owns the entity).
    pub fn encode_full_for(&self, id: NetId, owner: bool) -> Vec<u8> {
        let Some(state) = self.entities.get(&id) else {
            return Vec::new();
        };
        let Some(table) = self.tables.get(&state.scene) else {
            return Vec::new();
        };
        let mut fields = Vec::new();
        let mut flat = 0usize;
        for (p, part) in table.parts.iter().enumerate() {
            for (f, d) in part.iter().enumerate() {
                let fs = &state.fields[flat];
                if fs.version != 0 && (owner || !d.owner_only()) {
                    fields.push((p as u16, f as u16, &fs.enc));
                }
                flat += 1;
            }
        }
        let mut w = BitWriter::new();
        snapshot::write_entity(&mut w, id, table, &fields);
        snapshot::write_end(&mut w);
        w.into_bytes()
    }

    /// WP7: `values` (`(part, field, value)`) of entity `id` encoded with `scene`'s table as a
    /// header-less entry, like [`Replication::encode_full`] but for the given fields only (a client's
    /// write). Empty when the table is unknown.
    pub fn encode_values(&self, id: NetId, scene: u64, values: &[(u16, u16, Value)]) -> Vec<u8> {
        let Some(table) = self.tables.get(&scene) else {
            return Vec::new();
        };
        let mut sorted: Vec<&(u16, u16, Value)> = values
            .iter()
            .filter(|(p, f, _)| table.get(*p, *f).is_some())
            .collect();
        sorted.sort_by_key(|(p, f, _)| (*p, *f));
        sorted.dedup_by_key(|(p, f, _)| (*p, *f));
        let encs: Vec<(u16, u16, Encoded)> = sorted
            .iter()
            .filter_map(|(p, f, v)| {
                let d = table.get(*p, *f)?;
                let mut w = BitWriter::new();
                encode(&mut w, d.ty, d.quant, v);
                Some((*p, *f, Encoded::of(&w)))
            })
            .collect();
        let fields: Vec<(u16, u16, &Encoded)> = encs.iter().map(|(p, f, e)| (*p, *f, e)).collect();
        let mut w = BitWriter::new();
        snapshot::write_entity(&mut w, id, table, &fields);
        snapshot::write_end(&mut w);
        w.into_bytes()
    }

    /// WP7: the tick the entity's newest recorded change happened at (None when untracked or never
    /// recorded).
    pub fn last_change(&self, id: NetId) -> Option<u64> {
        self.entities
            .get(&id)?
            .fields
            .iter()
            .filter(|f| f.version != 0)
            .map(|f| f.last_change)
            .max()
    }

    /// WP8: true when a field takes part in hash telemetry: hot, not reliable, and small enough for a
    /// snapshot (what a client receives in snapshots every tick it changes). WP16: never an
    /// `audience: owner` field (only some peers hold its value).
    pub fn hashed(d: &crate::props::PropDesc, enc: &Encoded) -> bool {
        d.schedule == crate::props::Schedule::Hot
            && !d.reliable
            && !d.owner_only()
            && enc.byte_len() <= RELIABLE_BYTES
    }

    /// WP8: the hash of `id`'s recorded hot fields (their quantised encodings, see crate::hash); None
    /// when untracked or nothing hot was recorded.
    pub fn entity_hash(&self, id: NetId) -> Option<u32> {
        let state = self.entities.get(&id)?;
        let table = self.tables.get(&state.scene)?;
        let mut h = crate::hash::FNV_OFFSET;
        let mut any = false;
        let mut flat = 0usize;
        for (p, part) in table.parts.iter().enumerate() {
            for (f, d) in part.iter().enumerate() {
                let fs = &state.fields[flat];
                if fs.version != 0 && Self::hashed(d, &fs.enc) {
                    h = crate::hash::fold_field(h, p as u16, f as u16, &fs.enc);
                    any = true;
                }
                flat += 1;
            }
        }
        any.then_some(h)
    }

    /// WP8: whether `peer` can check `id`'s hash of `tick` (call after this tick's `build`): Some(false)
    /// when every hashed field is acknowledged at its current version (settled), Some(true) when the
    /// ones that are not were carried by this tick's snapshot to the peer, None otherwise.
    pub fn hash_eligibility(&self, peer: PeerId, id: NetId, tick: u64) -> Option<bool> {
        let state = self.entities.get(&id)?;
        let table = self.tables.get(&state.scene)?;
        let p = self.peers.get(&peer)?;
        let pe = p.entities.get(&id)?;
        let last = p.in_flight.back().filter(|f| f.tick == tick);
        let mut carried = false;
        let mut any = false;
        let mut flat = 0usize;
        for part in &table.parts {
            for d in part {
                let fs = &state.fields[flat];
                if fs.version != 0 && Self::hashed(d, &fs.enc) {
                    any = true;
                    if pe.acked.get(flat) != Some(&fs.version) {
                        let in_last =
                            last.is_some_and(|f| f.fields.contains(&(id, flat as u32, fs.version)));
                        if !in_last {
                            return None;
                        }
                        carried = true;
                    }
                }
                flat += 1;
            }
        }
        any.then_some(carried)
    }

    /// WP8: the ids of the tracked entities, ascending.
    pub fn entity_ids(&self) -> Vec<NetId> {
        self.entities.keys().copied().collect()
    }

    /// Decodes a full state ([`Replication::encode_full`]) with this side's table of `scene`.
    pub fn decode_state(&self, scene: u64, bytes: &[u8]) -> Option<Vec<(u16, u16, Value)>> {
        if bytes.is_empty() {
            return Some(Vec::new());
        }
        let table = self.tables.get(&scene)?;
        // Reuse the snapshot decoder: prepend an empty header.
        let mut w = BitWriter::new();
        snapshot::write_header(&mut w, Header::default());
        w.write_raw(bytes, bytes.len() * 8);
        let d = snapshot::decode(w.as_bytes(), |_| Some(table))?;
        Some(d.entities.into_iter().flat_map(|e| e.fields).collect())
    }

    pub fn add_peer(&mut self, peer: PeerId, now_tick: u64) {
        self.peers.insert(
            peer,
            PeerRepl {
                last_ack_at: now_tick,
                ..PeerRepl::default()
            },
        );
    }

    pub fn remove_peer(&mut self, peer: PeerId) {
        self.peers.remove(&peer);
    }

    /// The peer received `id`'s full state reliably (a `Create`): it holds every current version. WP16:
    /// unless `owner`, the `audience: owner` fields were left out ([`Replication::encode_full_for`]):
    /// the peer holds none of them, so they become pending the moment it owns the entity.
    pub fn mark_synced(&mut self, peer: PeerId, id: NetId, owner: bool) {
        let Some(state) = self.entities.get(&id) else {
            return;
        };
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        let masked = if owner {
            Vec::new()
        } else {
            self.tables
                .get(&state.scene)
                .map(|t| t.parts.iter().flatten().map(|d| d.owner_only()).collect())
                .unwrap_or_default()
        };
        let pe = p
            .entities
            .entry(id)
            .or_insert_with(|| PeerEntity::new(state.fields.len()));
        pe.fit(state.fields.len());
        for (i, fs) in state.fields.iter().enumerate() {
            let v = if masked.get(i).copied().unwrap_or(false) {
                0
            } else {
                fs.version
            };
            pe.acked[i] = v;
            pe.sent[i] = v;
        }
    }

    /// WP13: `id` left `peer`'s scope: forget what that peer held of it (a later `Create` brings it
    /// back with full state).
    pub fn forget_peer_entity(&mut self, peer: PeerId, id: NetId) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.entities.remove(&id);
        }
    }

    /// WP14: starts a server tick's snapshot pass (zeroes the per-tick deferred counters).
    pub fn begin_tick(&mut self) {
        self.stats.deferred_entities_tick = 0;
        self.stats.deferred_fields_tick = 0;
    }

    /// WP14: the entities marked as interacting with `peer`, ascending.
    pub fn interacting(&self, peer: PeerId) -> Vec<NetId> {
        self.peers.get(&peer).map_or_else(Vec::new, |p| {
            p.entities
                .iter()
                .filter(|(_, e)| e.interacting)
                .map(|(id, _)| *id)
                .collect()
        })
    }

    /// WP14: how many snapshots carried `id` to `peer` (diagnostics and tests).
    pub fn sends(&self, peer: PeerId, id: NetId) -> u64 {
        self.peers
            .get(&peer)
            .and_then(|p| p.entities.get(&id))
            .map_or(0, |e| e.sends)
    }

    /// The hook WP14 scores: marks `id` as interacting with `peer` (weight 100 instead of 1).
    pub fn set_interacting(&mut self, peer: PeerId, id: NetId, on: bool) {
        let n = self.entities.get(&id).map_or(0, |s| s.fields.len());
        if let Some(p) = self.peers.get_mut(&peer) {
            let pe = p.entities.entry(id).or_insert_with(|| PeerEntity::new(n));
            pe.interacting = on;
        }
    }

    /// Fields of `id` pending for `peer` (tests and diagnostics).
    pub fn pending_count(&self, peer: PeerId, id: NetId) -> usize {
        let Some(state) = self.entities.get(&id) else {
            return 0;
        };
        let pe = self.peers.get(&peer).and_then(|p| p.entities.get(&id));
        state
            .fields
            .iter()
            .enumerate()
            .filter(|(i, fs)| {
                fs.version != 0
                    && pe.is_none_or(|pe| {
                        pe.acked.get(*i) != Some(&fs.version)
                            && pe.sent.get(*i) != Some(&fs.version)
                    })
            })
            .count()
    }

    /// An `Ack` from `peer`: received snapshots raise what it holds; lost ones make their fields
    /// pending again.
    pub fn on_ack(&mut self, peer: PeerId, ack: Ack, now_tick: u64) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.last_ack_at = now_tick;
        p.acked_tick = p.acked_tick.max(ack.tick);
        while p.in_flight.front().is_some_and(|f| f.tick <= ack.tick) {
            let Some(f) = p.in_flight.pop_front() else {
                break;
            };
            let d = ack.tick - f.tick;
            let received = d == 0 || (d <= 32 && (ack.history >> (d - 1)) & 1 == 1);
            settle(&mut p.entities, &f.fields, received);
        }
    }

    /// Builds what goes to `peer` this tick: reliable field messages, and a snapshot filled by priority
    /// within the peer's budget and one MTU. `owner_of(id)` gives the entity's owner (for the owned
    /// weight); `skip(id)` excludes an entity (one the peer created itself).
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &mut self,
        peer: PeerId,
        server_tick: u64,
        now_tick: u64,
        tick_rate: u32,
        bytes_per_s: u32,
        owner_of: &dyn Fn(NetId) -> Option<PeerId>,
        skip: &dyn Fn(NetId) -> bool,
    ) -> Outbound {
        let mut out = Outbound::default();
        let Some(p) = self.peers.get_mut(&peer) else {
            return out;
        };
        p.budget.tick(bytes_per_s, tick_rate);
        // No ack for half a second while snapshots are in flight: fall back to full state.
        let timeout = u64::from(tick_rate / 2).max(1);
        if let Some(oldest) = p.in_flight.front() {
            if now_tick.saturating_sub(oldest.sent_at.max(p.last_ack_at)) > timeout {
                p.in_flight.clear();
                for pe in p.entities.values_mut() {
                    pe.acked.iter_mut().for_each(|v| *v = 0);
                    pe.sent.iter_mut().for_each(|v| *v = 0);
                }
                p.acked_tick = 0;
                p.last_ack_at = now_tick;
                self.stats.fallbacks += 1;
            }
        }
        while p.in_flight.len() > MAX_IN_FLIGHT {
            if let Some(f) = p.in_flight.pop_front() {
                settle(&mut p.entities, &f.fields, false);
            }
        }
        // Candidates (id, accumulated sum, pending fields) and reliable fields.
        let mut cands: Vec<(NetId, f64, usize)> = Vec::new();
        for (&id, state) in &self.entities {
            if skip(id) {
                continue;
            }
            let Some(table) = self.tables.get(&state.scene) else {
                continue;
            };
            let pe = p
                .entities
                .entry(id)
                .or_insert_with(|| PeerEntity::new(state.fields.len()));
            pe.fit(state.fields.len());
            let owned = owner_of(id) == Some(peer);
            let mut any = 0usize;
            let mut flat = 0usize;
            for (pi, part) in table.parts.iter().enumerate() {
                for (fi, d) in part.iter().enumerate() {
                    let fs = &state.fields[flat];
                    // WP16: an `audience: owner` field never goes to another peer.
                    let pending = fs.version != 0
                        && (owned || !d.owner_only())
                        && fs.version != pe.acked[flat]
                        && fs.version != pe.sent[flat];
                    if pending {
                        if d.reliable || fs.enc.byte_len() > RELIABLE_BYTES {
                            out.fields.push(FieldMsg {
                                tick: server_tick,
                                id,
                                part: pi as u16,
                                field: fi as u16,
                                bits: fs.enc.bits,
                                data: fs.enc.data.clone(),
                            });
                            pe.acked[flat] = fs.version;
                            pe.sent[flat] = fs.version;
                            self.stats.reliable_fields_sent += 1;
                        } else {
                            any += 1;
                        }
                    }
                    flat += 1;
                }
            }
            if any > 0 {
                pe.accum += priority::weight(owned, pe.interacting, state.priority);
                cands.push((id, pe.accum, any));
            }
        }
        let allowance = p.budget.allowance();
        if cands.is_empty() {
            return out;
        }
        if allowance == 0 {
            let fields: usize = cands.iter().map(|c| c.2).sum();
            self.stats.defer(cands.len(), fields);
            return out;
        }
        cands.sort_by(|a, b| priority::cmp(a.0, a.1, b.0, b.1));
        let header = Header {
            tick: server_tick,
            baseline: p.acked_tick,
        };
        let limit = allowance * 8;
        let mut used = snapshot::frame_bits(header);
        let mut w = BitWriter::new();
        snapshot::write_header(&mut w, header);
        let mut carried: Vec<(NetId, u32, u32)> = Vec::new();
        let mut entities_sent = 0u64;
        let (mut left_out, mut fields_left) = (0usize, 0usize);
        for (id, _, n_pending) in cands {
            if limit.saturating_sub(used) < 64 {
                left_out += 1;
                fields_left += n_pending;
                continue;
            }
            let Some(state) = self.entities.get(&id) else {
                continue;
            };
            let Some(table) = self.tables.get(&state.scene) else {
                continue;
            };
            let Some(pe) = p.entities.get_mut(&id) else {
                continue;
            };
            // The pending fields, in order, as many as fit.
            let owned = owner_of(id) == Some(peer);
            let mut chosen: Vec<(u16, u16, u32)> = Vec::new();
            let mut flats: Vec<usize> = Vec::new();
            let mut flat = 0usize;
            'parts: for (pi, part) in table.parts.iter().enumerate() {
                for (fi, d) in part.iter().enumerate() {
                    let fs = &state.fields[flat];
                    let pending = fs.version != 0
                        && (owned || !d.owner_only())
                        && fs.version != pe.acked[flat]
                        && fs.version != pe.sent[flat];
                    if pending {
                        chosen.push((pi as u16, fi as u16, fs.enc.bits));
                        let rec = snapshot::record_bits(table, &chosen);
                        if used + snapshot::entry_overhead(id, rec) + rec > limit {
                            chosen.pop();
                            break 'parts;
                        }
                        flats.push(flat);
                    }
                    flat += 1;
                }
            }
            if chosen.is_empty() {
                left_out += 1;
                fields_left += n_pending;
                continue;
            }
            fields_left += n_pending - chosen.len();
            let fields: Vec<(u16, u16, &Encoded)> = chosen
                .iter()
                .zip(&flats)
                .map(|((pi, fi, _), fl)| (*pi, *fi, &state.fields[*fl].enc))
                .collect();
            let before = w.bit_len();
            snapshot::write_entity(&mut w, id, table, &fields);
            used += w.bit_len() - before;
            for &fl in &flats {
                let v = state.fields[fl].version;
                pe.sent[fl] = v;
                carried.push((id, fl as u32, v));
            }
            pe.accum = 0.0;
            pe.sends += 1;
            entities_sent += 1;
            self.stats.fields_sent += flats.len() as u64;
        }
        self.stats.defer(left_out, fields_left);
        if carried.is_empty() {
            return out;
        }
        snapshot::write_end(&mut w);
        let body = w.into_bytes();
        let bytes = body.len() + 1;
        debug_assert!(bytes <= priority::MTU);
        p.budget.spend(bytes);
        p.in_flight.push_back(InFlight {
            tick: server_tick,
            sent_at: now_tick,
            fields: carried,
        });
        self.stats.snapshots_sent += 1;
        self.stats.snapshot_bytes += bytes as u64;
        self.stats.entities_sent += entities_sent;
        out.snapshot = Some(body);
        out
    }

    /// A snapshot body from the authority. `scene_of(id)` gives a known entity's scene; entities this
    /// side cannot decode yet are skipped and the snapshot is then not acknowledged. Returns the
    /// snapshot's server tick when it was applied (WP4: the tick prediction compares against).
    pub fn on_snapshot(
        &mut self,
        body: &[u8],
        scene_of: &dyn Fn(NetId) -> Option<u64>,
    ) -> Option<u64> {
        let tables = &self.tables;
        let Some(d) = snapshot::decode(body, |id| scene_of(id).and_then(|s| tables.get(&s))) else {
            self.stats.decode_errors += 1;
            return None;
        };
        let t = d.header.tick;
        if self.rx.last_applied.is_some_and(|last| t <= last) {
            self.stats.snapshots_stale += 1;
            return None;
        }
        self.rx.last_applied = Some(t);
        self.stats.snapshots_received += 1;
        for e in d.entities {
            for (part, field, value) in e.fields {
                if self
                    .rx
                    .reliable_ticks
                    .get(&(e.id, part, field))
                    .is_some_and(|rt| *rt > t)
                {
                    continue;
                }
                self.stats.fields_received += 1;
                self.updates.push_back(StateUpdate {
                    id: e.id,
                    part,
                    field,
                    value,
                    tick: t,
                });
            }
        }
        if d.skipped == 0 {
            self.rx.mark_received(t);
        } else {
            self.stats.snapshots_unacked += 1;
        }
        Some(t)
    }

    /// A reliable field from the authority.
    pub fn on_field(&mut self, msg: FieldMsg, scene_of: &dyn Fn(NetId) -> Option<u64>) {
        let Some(scene) = scene_of(msg.id) else {
            return;
        };
        let Some(d) = self
            .tables
            .get(&scene)
            .and_then(|t| t.get(msg.part, msg.field))
        else {
            return;
        };
        let key = (msg.id, msg.part, msg.field);
        if self
            .rx
            .reliable_ticks
            .get(&key)
            .is_some_and(|t| *t > msg.tick)
        {
            return;
        }
        let Some(value) = crate::quant::decode(&mut BitReader::new(&msg.data), d.ty, d.quant)
        else {
            self.stats.decode_errors += 1;
            return;
        };
        self.rx.reliable_ticks.insert(key, msg.tick);
        self.stats.fields_received += 1;
        self.updates.push_back(StateUpdate {
            id: msg.id,
            part: msg.part,
            field: msg.field,
            value,
            tick: msg.tick,
        });
    }

    /// The ack to send, when snapshots arrived since the last one.
    pub fn take_ack(&mut self) -> Option<Ack> {
        if !self.rx.ack_due {
            return None;
        }
        self.rx.ack_due = false;
        Some(Ack {
            tick: self.rx.ack_tick,
            history: self.rx.ack_history,
        })
    }

    /// Forgets what was received (a new session) and drops a destroyed entity's reliable ticks.
    pub fn forget_received(&mut self, id: NetId) {
        self.rx.reliable_ticks.retain(|(i, _, _), _| *i != id);
    }

    pub fn poll_update(&mut self) -> Option<StateUpdate> {
        self.updates.pop_front()
    }

    pub fn drain_updates(&mut self) -> Vec<StateUpdate> {
        self.updates.drain(..).collect()
    }
}

fn settle(
    entities: &mut BTreeMap<NetId, PeerEntity>,
    fields: &[(NetId, u32, u32)],
    received: bool,
) {
    for &(id, flat, v) in fields {
        let Some(pe) = entities.get_mut(&id) else {
            continue;
        };
        let i = flat as usize;
        if i >= pe.acked.len() {
            continue;
        }
        if received {
            if v > pe.acked[i] {
                pe.acked[i] = v;
            }
        } else if pe.sent[i] == v {
            pe.sent[i] = pe.acked[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::priority::MTU;
    use crate::props::{PropDesc, Quant, TypeTag};

    const SCENE: u64 = 42;
    const RATE: u32 = 60;

    fn table() -> ScenePropsTable {
        ScenePropsTable::new(std::vec![
            std::vec![
                PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("rotation", TypeTag::Float).hot(Quant::Deg),
                PropDesc::new("visible", TypeTag::Bool),
            ],
            std::vec![PropDesc::new("text", TypeTag::String)],
        ])
    }

    fn values(x: f32) -> Vec<Vec<Value>> {
        std::vec![
            std::vec![
                Value::Vector2([x, 0.0]),
                Value::Float(0.0),
                Value::Bool(true)
            ],
            std::vec![Value::Str("a".into())],
        ]
    }

    /// A server-side replication with one peer and `n` entities.
    fn server(n: u32) -> Replication {
        let mut r = Replication::default();
        r.register_table(SCENE, table());
        r.add_peer(2, 0);
        for id in 1..=n {
            r.add_entity(id, SCENE, &values(0.0), 0);
        }
        r
    }

    fn client() -> Replication {
        let mut c = Replication::default();
        c.register_table(SCENE, table());
        c
    }

    fn set_x(r: &mut Replication, id: NetId, x: f32, tick: u64) -> bool {
        r.record_part(id, SCENE, 0, &[(0, Value::Vector2([x, 0.0]))], tick)
    }

    fn build(r: &mut Replication, tick: u64, budget: u32) -> Outbound {
        r.build(2, tick, tick, RATE, budget, &|_| Some(1), &|_| false)
    }

    const BODY: u64 = 77;

    /// A RigidBody2D ball as gdnet infers it: hot position, rotation, both velocities (WP14); cold
    /// visible, modulate, z_index; a shape part with `disabled`.
    fn body_table() -> ScenePropsTable {
        ScenePropsTable::new(std::vec![
            std::vec![
                PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("rotation", TypeTag::Float).hot(Quant::Deg),
                PropDesc::new("scale", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("linear_velocity", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("angular_velocity", TypeTag::Float).hot(Quant::Mm),
                PropDesc::new("visible", TypeTag::Bool),
                PropDesc::new("z_index", TypeTag::Int),
            ],
            std::vec![PropDesc::new("disabled", TypeTag::Bool)],
        ])
    }

    fn move_body(r: &mut Replication, id: NetId, t: u64) {
        let f = t as f32 * 0.016 + id as f32;
        let vals = [
            (
                0,
                Value::Vector2([300.0 + 100.0 * f.sin(), 200.0 + 80.0 * f.cos()]),
            ),
            (1, Value::Float(f64::from(f % 3.0))),
            (3, Value::Vector2([100.0 * f.cos(), -80.0 * f.sin()])),
            (4, Value::Float(f64::from(2.0 * f.sin()))),
        ];
        r.record_part(id, BODY, 0, &vals, t);
    }

    /// A server with one peer and `n` bodies the peer already holds.
    fn pit(n: u32) -> Replication {
        let mut r = Replication::default();
        r.register_table(BODY, body_table());
        r.add_peer(2, 0);
        let init = std::vec![
            std::vec![
                Value::Vector2([0.0, 0.0]),
                Value::Float(0.0),
                Value::Vector2([1.0, 1.0]),
                Value::Vector2([0.0, 0.0]),
                Value::Float(0.0),
                Value::Bool(true),
                Value::Int(0),
            ],
            std::vec![Value::Bool(false)],
        ];
        for id in 1..=n {
            r.add_entity(id, BODY, &init, 0);
            r.mark_synced(2, id, false);
        }
        r
    }

    /// Ticks `ticks` of a pit where every body moves, acking every snapshot; returns the snapshot sizes.
    fn run_pit(r: &mut Replication, from: u64, ticks: u64, budget: u32) -> Vec<usize> {
        let mut c = Replication::default();
        c.register_table(BODY, body_table());
        let n = r.entity_count() as u32;
        let mut sizes = Vec::new();
        for t in from..from + ticks {
            for id in 1..=n {
                move_body(r, id, t);
            }
            let out = r.build(2, t, t, RATE, budget, &|_| Some(1), &|_| false);
            if let Some(body) = out.snapshot {
                sizes.push(body.len() + 1);
                c.on_snapshot(&body, &|_| Some(BODY));
                c.drain_updates();
                if let Some(ack) = c.take_ack() {
                    r.on_ack(2, ack, t);
                }
            }
        }
        sizes
    }

    #[test]
    fn three_hundred_bodies_pack_into_mtu_sized_snapshots() {
        let mut r = pit(300);
        // A budget far above what fits: every snapshot is limited by the MTU alone.
        let sizes = run_pit(&mut r, 1, 120, 10_000_000);
        assert_eq!(sizes.len(), 120);
        assert!(sizes.iter().all(|s| *s <= MTU), "{sizes:?}");
        let min = *sizes.iter().min().unwrap();
        assert!(
            min >= MTU - 24,
            "a full backlog leaves little room unused: {min}"
        );
        let per_body = r.stats.snapshot_bytes as f64 / r.stats.entities_sent as f64;
        std::println!("bytes per body update {per_body:.2}");
        // 4 hot fields (position and linear velocity 2 bytes per axis at `mm` = 0.1 px (D17), rotation
        // and angular velocity 2 bytes each), about 12 bytes of values, plus the entry header: the
        // "more" bit, the id varint (2 bytes above 127), the record length varint (1 byte) and the
        // masks (9 bits), about 4 bytes. Measured 2026-11-08: 20.6 bytes per body update at WP14's
        // 0.001 px step, 15.7 at D17's 0.1 px.
        assert!(per_body < 17.0, "bytes per body update {per_body}");
        assert!(
            r.stats.deferred_entities > 0,
            "300 moving bodies do not fit an MTU a tick"
        );
    }

    #[test]
    fn near_bodies_are_sent_ten_times_as_often_as_far_ones() {
        let mut r = pit(300);
        for id in 1..=20 {
            r.set_interacting(2, id, true);
        }
        assert_eq!(r.interacting(2), (1..=20).collect::<Vec<_>>());
        run_pit(&mut r, 1, 600, 32_768);
        let near: u64 = (1..=20).map(|id| r.sends(2, id)).sum::<u64>() / 20;
        let far: u64 = (21..=300).map(|id| r.sends(2, id)).sum::<u64>() / 280;
        std::println!("near {near} far {far}");
        assert!(
            near >= 500,
            "near bodies go out (nearly) every tick: {near} of 600"
        );
        assert!(near >= 10 * far.max(1), "near {near} vs far {far}");
        assert!(far >= 1, "far bodies still go out now and then: {far}");
        assert!(r.stats.snapshot_bytes <= 10 * 32_768 + 3 * MTU as u64);
    }

    #[test]
    fn deferred_counters_count_what_the_budget_left_out() {
        let mut r = pit(50);
        r.begin_tick();
        run_pit(&mut r, 1, 60, 10_000_000);
        assert_eq!(
            r.stats.deferred_entities, 0,
            "everything fits a generous budget"
        );
        assert_eq!(r.stats.deferred_fields, 0);
        run_pit(&mut r, 61, 60, 4_096);
        let s = r.stats;
        assert!(
            s.deferred_entities > 0 && s.deferred_fields >= s.deferred_entities,
            "{s:?}"
        );
        r.begin_tick();
        assert_eq!(r.stats.deferred_entities_tick, 0);
        run_pit(&mut r, 121, 1, 4_096);
        assert!(r.stats.deferred_entities_tick > 0);
        // A settled pit: nothing pending, nothing deferred, no bytes.
        let mut r = pit(300);
        let sizes = run_pit(&mut r, 0, 0, 32_768);
        assert!(sizes.is_empty());
        for t in 1..60 {
            assert!(build_body(&mut r, t).snapshot.is_none());
        }
        assert_eq!(r.stats.deferred_entities + r.stats.snapshot_bytes, 0);
    }

    fn build_body(r: &mut Replication, t: u64) -> Outbound {
        r.build(2, t, t, RATE, 32_768, &|_| Some(1), &|_| false)
    }

    #[test]
    fn records_detect_changes_by_their_quantised_value() {
        let mut r = server(1);
        assert!(!set_x(&mut r, 1, 0.0, 1), "same value");
        assert!(
            !set_x(&mut r, 1, 0.02, 2),
            "below one mm (0.1 px, D17): same encoding"
        );
        assert!(set_x(&mut r, 1, 0.2, 3));
        let v = r.entity(1).unwrap().fields[0].version;
        assert_eq!(v, 2);
    }

    #[test]
    fn a_new_peer_gets_full_state_then_only_dirty_fields() {
        let mut r = server(1);
        let out = build(&mut r, 1, 32_768);
        let body = out.snapshot.expect("full state goes out");
        let mut c = client();
        c.on_snapshot(&body, &|_| Some(SCENE));
        assert_eq!(c.drain_updates().len(), 4, "every field");
        // In flight: nothing pending, nothing sent.
        assert!(build(&mut r, 2, 32_768).snapshot.is_none());
        r.on_ack(2, c.take_ack().unwrap(), 2);
        assert_eq!(r.pending_count(2, 1), 0);
        // One field changes: only it travels.
        set_x(&mut r, 1, 5.0, 3);
        let body = build(&mut r, 3, 32_768).snapshot.unwrap();
        c.on_snapshot(&body, &|_| Some(SCENE));
        let ups = c.drain_updates();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].value, Value::Vector2([5.0, 0.0]));
    }

    #[test]
    fn a_zero_dirty_entity_costs_nothing() {
        let mut r = server(3);
        let mut c = client();
        let body = build(&mut r, 1, 1_000_000).snapshot.unwrap();
        c.on_snapshot(&body, &|_| Some(SCENE));
        r.on_ack(2, c.take_ack().unwrap(), 1);
        for t in 2..100 {
            let out = build(&mut r, t, 1_000_000);
            assert!(out.snapshot.is_none() && out.fields.is_empty(), "tick {t}");
        }
        assert_eq!(r.stats.snapshots_sent, 1);
        // One of three changes: the snapshot names it alone.
        set_x(&mut r, 2, 1.0, 100);
        let body = build(&mut r, 100, 1_000_000).snapshot.unwrap();
        let d = snapshot::decode(&body, |_| r.table(SCENE)).unwrap();
        assert_eq!(d.entities.len(), 1);
        assert_eq!(d.entities[0].id, 2);
    }

    #[test]
    fn a_lost_packet_re_marks_its_fields_dirty() {
        let mut r = server(1);
        let mut c = client();
        let first = build(&mut r, 1, 32_768).snapshot.unwrap();
        c.on_snapshot(&first, &|_| Some(SCENE));
        r.on_ack(2, c.take_ack().unwrap(), 1);
        set_x(&mut r, 1, 7.0, 2);
        let lost = build(&mut r, 2, 32_768).snapshot.unwrap();
        drop(lost);
        // Tick 3: nothing new to say (the change is in flight).
        assert!(build(&mut r, 3, 32_768).snapshot.is_none());
        set_x(&mut r, 1, 8.0, 4);
        let body = build(&mut r, 4, 32_768).snapshot.unwrap();
        c.on_snapshot(&body, &|_| Some(SCENE));
        // The ack for 4 says 2 never arrived; 8.0 already went with 4, so nothing is resent.
        r.on_ack(2, c.take_ack().unwrap(), 4);
        assert_eq!(r.pending_count(2, 1), 0);
        // A lost snapshot whose field did not change again is resent with its current value.
        set_x(&mut r, 1, 9.0, 5);
        let _lost = build(&mut r, 5, 32_768).snapshot.unwrap();
        r.record_part(1, SCENE, 1, &[(0, Value::Str("b".into()))], 6);
        let body = build(&mut r, 6, 32_768).snapshot.unwrap();
        c.drain_updates();
        c.on_snapshot(&body, &|_| Some(SCENE));
        assert_eq!(c.drain_updates().len(), 1, "the text");
        r.on_ack(2, c.take_ack().unwrap(), 6);
        assert_eq!(
            r.pending_count(2, 1),
            1,
            "position (lost at 5) is dirty again"
        );
        let body = build(&mut r, 7, 32_768).snapshot.unwrap();
        c.on_snapshot(&body, &|_| Some(SCENE));
        let ups = c.drain_updates();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].value, Value::Vector2([9.0, 0.0]));
    }

    #[test]
    fn no_ack_for_half_a_second_falls_back_to_full_state() {
        let mut r = server(1);
        let mut c = client();
        let body = build(&mut r, 1, 32_768).snapshot.unwrap();
        c.on_snapshot(&body, &|_| Some(SCENE));
        r.on_ack(2, c.take_ack().unwrap(), 1);
        set_x(&mut r, 1, 3.0, 2);
        assert!(build(&mut r, 2, 32_768).snapshot.is_some());
        for t in 3..=32 {
            assert!(build(&mut r, t, 32_768).snapshot.is_none(), "tick {t}");
        }
        let body = build(&mut r, 33, 32_768).snapshot.expect("fallback");
        assert_eq!(r.stats.fallbacks, 1);
        let d = snapshot::decode(&body, |_| r.table(SCENE)).unwrap();
        assert_eq!(d.header.baseline, 0, "full state has no baseline");
        assert_eq!(d.entities[0].fields.len(), 4, "every field");
    }

    #[test]
    fn the_accumulator_sends_priority_ten_twice_as_often_as_five() {
        // 10 entities of priority 10 and 20 of priority 5, all dirty every tick, a budget that fits a
        // few per tick.
        let mut r = server(30);
        for id in 1..=30 {
            r.set_priority(id, if id <= 10 { 10.0 } else { 5.0 });
        }
        let mut counts = [0u32; 31];
        for t in 1..=900u64 {
            for id in 1..=30 {
                set_x(&mut r, id, t as f32 + id as f32 * 1000.0, t);
            }
            let out = r.build(2, t, t, RATE, 60 * 60, &|_| Some(1), &|_| false);
            if let Some(body) = out.snapshot {
                let d = snapshot::decode(&body, |_| r.table(SCENE)).unwrap();
                for e in &d.entities {
                    counts[e.id as usize] += 1;
                }
                r.on_ack(
                    2,
                    Ack {
                        tick: t,
                        history: u32::MAX,
                    },
                    t,
                );
            }
        }
        let high: f64 = counts[1..=10].iter().map(|c| f64::from(*c)).sum::<f64>() / 10.0;
        let low: f64 = counts[11..=30].iter().map(|c| f64::from(*c)).sum::<f64>() / 20.0;
        let ratio = high / low;
        assert!(
            (1.75..=2.25).contains(&ratio),
            "{high} vs {low}: ratio {ratio}"
        );
        assert!(low > 20.0, "everyone is sent eventually ({low})");
    }

    #[test]
    fn the_owned_entity_goes_every_time_under_budget() {
        let mut r = server(20);
        let mut owned_sent = 0;
        for t in 1..=120u64 {
            for id in 1..=20 {
                // D17: x100 keeps the WP3 encodings' sizes at the 0.1 px step.
                set_x(&mut r, id, t as f32 * id as f32 * 100.0, t);
            }
            let out = r.build(
                2,
                t,
                t,
                RATE,
                60 * 100,
                &|id| Some(if id == 7 { 2 } else { 1 }),
                &|_| false,
            );
            if let Some(body) = out.snapshot {
                let d = snapshot::decode(&body, |_| r.table(SCENE)).unwrap();
                assert!(t < 30 || d.entities.len() < 20, "the budget bites");
                if d.entities.iter().any(|e| e.id == 7) {
                    owned_sent += 1;
                }
                r.on_ack(
                    2,
                    Ack {
                        tick: t,
                        history: u32::MAX,
                    },
                    t,
                );
            }
        }
        assert_eq!(owned_sent, r.stats.snapshots_sent, "in every snapshot");
        assert!(r.stats.snapshots_sent >= 100);
    }

    #[test]
    fn a_snapshot_never_exceeds_the_mtu_or_the_budget() {
        let mut r = server(500);
        let mut total = 0u64;
        for t in 1..=120u64 {
            for id in 1..=500 {
                set_x(&mut r, id, (t * 13 + u64::from(id)) as f32 * 3.7, t);
            }
            let out = build(&mut r, t, 32_768);
            if let Some(body) = out.snapshot {
                assert!(body.len() < MTU, "{} bytes", body.len() + 1);
                total += body.len() as u64 + 1;
                r.on_ack(
                    2,
                    Ack {
                        tick: t,
                        history: u32::MAX,
                    },
                    t,
                );
            }
        }
        // Two seconds at 32 KB/s plus the initial bucket.
        assert!(total <= 2 * 32_768 + 3 * MTU as u64, "{total}");
        assert!(total >= 60_000, "{total}");
    }

    #[test]
    fn stale_snapshots_are_dropped() {
        let mut r = server(1);
        let a = build(&mut r, 1, 1_000_000).snapshot.unwrap();
        set_x(&mut r, 1, 4.0, 2);
        let b = build(&mut r, 2, 1_000_000).snapshot.unwrap();
        let mut c = client();
        c.on_snapshot(&b, &|_| Some(SCENE));
        c.drain_updates();
        c.on_snapshot(&a, &|_| Some(SCENE));
        assert!(
            c.drain_updates().is_empty(),
            "the older tick is not applied"
        );
        assert_eq!(c.stats.snapshots_stale, 1);
        let ack = c.take_ack().unwrap();
        assert_eq!(
            ack,
            Ack {
                tick: 2,
                history: 0
            },
            "tick 1 is not acknowledged"
        );
        c.on_snapshot(&b, &|_| Some(SCENE));
        assert_eq!(c.stats.snapshots_stale, 2, "a duplicate too");
    }

    #[test]
    fn a_snapshot_naming_an_unknown_entity_is_applied_but_not_acked() {
        let mut r = server(2);
        let body = build(&mut r, 1, 1_000_000).snapshot.unwrap();
        let mut c = client();
        c.on_snapshot(&body, &|id| (id == 1).then_some(SCENE));
        assert_eq!(c.drain_updates().len(), 4, "entity 1 applied");
        assert!(c.take_ack().is_none());
        assert_eq!(c.stats.snapshots_unacked, 1);
    }

    #[test]
    fn full_state_round_trips_and_marks_the_peer_synced() {
        let mut r = server(0);
        let bytes = r.add_entity(5, SCENE, &values(12.5), 0);
        let c = client();
        let decoded = c.decode_state(SCENE, &bytes).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0], (0, 0, Value::Vector2([12.5, 0.0])));
        assert_eq!(decoded[3], (1, 0, Value::Str("a".into())));
        assert_eq!(r.pending_count(2, 5), 4);
        r.mark_synced(2, 5, false);
        assert_eq!(r.pending_count(2, 5), 0);
        assert!(build(&mut r, 1, 32_768).snapshot.is_none());
        // A burst later reproduces the current state.
        set_x(&mut r, 5, -3.0, 9);
        let burst = r.encode_full(5);
        let again = c.decode_state(SCENE, &burst).unwrap();
        assert_eq!(again[0], (0, 0, Value::Vector2([-3.0, 0.0])));
        assert_eq!(c.decode_state(SCENE, &[]), Some(Vec::new()));
    }

    #[test]
    fn large_and_reliable_fields_go_on_the_reliable_channel() {
        let mut t = table();
        t.parts[0][2].reliable = true;
        let mut r = Replication::default();
        r.register_table(SCENE, t.clone());
        r.add_peer(2, 0);
        let mut vals = values(0.0);
        vals[1][0] = Value::Str("x".repeat(3000));
        r.add_entity(1, SCENE, &vals, 0);
        let out = build(&mut r, 1, 1_000_000);
        assert_eq!(out.fields.len(), 2, "the big string and the reliable bool");
        let mut c = Replication::default();
        c.register_table(SCENE, t);
        for f in out.fields {
            c.on_field(f, &|_| Some(SCENE));
        }
        let ups = c.drain_updates();
        assert!(ups.iter().any(|u| u.value == Value::Str("x".repeat(3000))));
        let body = out.snapshot.unwrap();
        let d = snapshot::decode(&body, |_| r.table(SCENE)).unwrap();
        assert_eq!(
            d.entities[0].fields.len(),
            2,
            "position and rotation in the snapshot"
        );
    }

    #[test]
    fn slow_fields_follow_their_part_or_their_period() {
        let mut t = table();
        t.parts[0][1].slow_ticks = 60;
        let mut r = Replication::default();
        r.register_table(SCENE, t);
        r.add_entity(1, SCENE, &values(0.0), 0);
        let v = |r: &Replication| r.entity(1).unwrap().fields[1].version;
        assert_eq!(v(&r), 1);
        r.record_part(1, SCENE, 0, &[(1, Value::Float(0.5))], 10);
        assert_eq!(v(&r), 1, "alone it waits");
        r.record_part(
            1,
            SCENE,
            0,
            &[(0, Value::Vector2([1.0, 0.0])), (1, Value::Float(0.6))],
            11,
        );
        assert_eq!(v(&r), 2, "with a change of its part it goes");
        r.record_part(1, SCENE, 0, &[(1, Value::Float(0.7))], 71);
        assert_eq!(v(&r), 3, "after its period it goes");
    }

    #[test]
    fn interacting_entities_outrank_others() {
        let mut r = server(30);
        r.set_interacting(2, 5, true);
        let mut counts = [0u32; 31];
        for t in 1..=300u64 {
            for id in 1..=30 {
                set_x(&mut r, id, t as f32 + id as f32 * 100.0, t);
            }
            let out = r.build(2, t, t, RATE, 60 * 60, &|_| Some(1), &|_| false);
            if let Some(body) = out.snapshot {
                for e in snapshot::decode(&body, |_| r.table(SCENE))
                    .unwrap()
                    .entities
                {
                    counts[e.id as usize] += 1;
                }
                r.on_ack(
                    2,
                    Ack {
                        tick: t,
                        history: u32::MAX,
                    },
                    t,
                );
            }
        }
        let others = counts
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 5 && *i != 0);
        let avg = others.map(|(_, c)| f64::from(*c)).sum::<f64>() / 29.0;
        assert_eq!(
            u64::from(counts[5]),
            r.stats.snapshots_sent,
            "in every snapshot"
        );
        assert!(f64::from(counts[5]) > 3.0 * avg, "{counts:?}");
    }
}

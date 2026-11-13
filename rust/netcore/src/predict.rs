//! Prediction history and mismatch detection (docs/PLAN_NETCODE.md 2.5, WP4).
//!
//! The owner of an entity simulates it immediately and records its hot state after every tick it
//! simulates, keyed by the server tick it predicted, into a ring of `max_rollback_ticks` entries
//! ([`Prediction::record`]). The server's snapshots for the entity do not overwrite it: their hot fields
//! go into a *confirmed* mirror of the server's latest values ([`Prediction::confirm`]). After the
//! packets of a tick are processed, [`Prediction::evaluate`] compares, at the newest confirmed tick T,
//! the mirror with the history at T for every tracked entity (each property within its
//! `PropDesc.tolerance`: float components may differ by the tolerance, everything else must be equal).
//! Under policy `check` a mismatch of any entity produces one [`Rollback`] to T carrying every tracked
//! entity's state at T (the mirror over the history), so gdnet restores them all and replays T+1..now
//! together; `always` produces one at every confirmed tick; `disabled` never does.
//!
//! The mirror holds the server's latest value of each field: snapshots are most-recent-state, so with no
//! loss it is the server's state at T for every field, including fields that did not change at T (the
//! case "the server did not move the player at all" is caught). A field whose update was lost and not
//! yet resent can make one spurious rollback; the next snapshot corrects it. A confirmation older than
//! the history window is not compared (counted as `stale`), except while the history is still filling:
//! then a confirmation from before its first tick is compared at that tick (WP16: a player moved by the
//! server right after its spawn, before the owner predicted, was otherwise never corrected).

use crate::entity::NetId;
use crate::props::{Quant, ScenePropsTable, Schedule, TypeTag, Value};
use crate::quant::{self, BitReader, BitWriter};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// The default history length (D8).
pub const DEFAULT_MAX_ROLLBACK: u32 = 20;

/// `NetEntity.predict`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Policy {
    /// Roll back when a confirmed value differs from the prediction beyond its tolerance.
    #[default]
    Check,
    /// Roll back at every confirmed tick.
    Always,
    /// Never roll back.
    Disabled,
}

/// One entity's hot state: `(part, field, value)`.
pub type EntityState = Vec<(u16, u16, Value)>;

/// A request to restore entities to `to_tick` and replay to the present.
#[derive(Clone, Debug, PartialEq)]
pub struct Rollback {
    pub to_tick: u64,
    /// Per entity: its hot state at `to_tick`, as `(part, field, value)`.
    pub entities: Vec<(NetId, EntityState)>,
    /// The entities whose confirmed state differed (all tracked ones under `always`).
    pub mismatched: Vec<NetId>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PredictStats {
    pub rollbacks: u64,
    pub confirmations: u64,
    pub mismatches: u64,
    /// Confirmations older than the history (not compared).
    pub stale: u64,
}

#[derive(Clone, Copy, Debug)]
struct HotField {
    part: u16,
    field: u16,
    ty: TypeTag,
    quant: Quant,
    tolerance: f32,
}

#[derive(Clone, Debug)]
struct Tracked {
    policy: Policy,
    hot: Vec<HotField>,
    /// (part, field) -> index into `hot`.
    index: BTreeMap<(u16, u16), usize>,
    history: VecDeque<(u64, Vec<Value>)>,
    mirror: Vec<Option<Value>>,
}

impl Tracked {
    fn at(&self, tick: u64) -> Option<&Vec<Value>> {
        self.history
            .iter()
            .rev()
            .find(|(t, _)| *t == tick)
            .map(|(_, v)| v)
    }
}

/// The prediction state of one owner (a client) over its tracked entities.
#[derive(Clone, Debug)]
pub struct Prediction {
    tracked: BTreeMap<NetId, Tracked>,
    max_ticks: u32,
    /// The newest confirmed tick not evaluated yet.
    confirmed: Option<u64>,
    pub stats: PredictStats,
}

impl Default for Prediction {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ROLLBACK)
    }
}

/// A value after a round trip through its quantiser (what the receiver of a snapshot would see).
pub fn canonical(ty: TypeTag, q: Quant, v: &Value) -> Value {
    let mut w = BitWriter::new();
    quant::encode(&mut w, ty, q, v);
    let bytes = w.into_bytes();
    quant::decode(&mut BitReader::new(&bytes), ty, q).unwrap_or_else(|| v.clone())
}

fn floats(v: &Value) -> Option<Vec<f64>> {
    Some(match v {
        Value::Float(f) => std::vec![*f],
        Value::Vector2(a) => a.iter().map(|x| f64::from(*x)).collect(),
        Value::Vector3(a) => a.iter().map(|x| f64::from(*x)).collect(),
        Value::Vector4(a) | Value::Quaternion(a) | Value::Color(a) => {
            a.iter().map(|x| f64::from(*x)).collect()
        }
        Value::Transform2D(a) => a.iter().map(|x| f64::from(*x)).collect(),
        Value::Transform3D(a) => a.iter().map(|x| f64::from(*x)).collect(),
        _ => return None,
    })
}

/// True when `a` and `b` agree within `tolerance` (per float component; other types exactly).
pub fn within(a: &Value, b: &Value, tolerance: f32) -> bool {
    match (floats(a), floats(b)) {
        (Some(x), Some(y)) if x.len() == y.len() => {
            // A hair over the tolerance absorbs the quantiser's rounding of a value exactly on it.
            // (and f32 rounding of the components).
            let tol = f64::from(tolerance.max(0.0)) * (1.0 + 1e-6);
            x.iter()
                .zip(&y)
                .all(|(p, q)| (p - q).abs() <= tol + 1e-6 * (1.0 + p.abs()))
        }
        _ => a == b,
    }
}

impl Prediction {
    pub fn new(max_ticks: u32) -> Self {
        Self {
            tracked: BTreeMap::new(),
            max_ticks: max_ticks.max(1),
            confirmed: None,
            stats: PredictStats::default(),
        }
    }

    pub fn max_ticks(&self) -> u32 {
        self.max_ticks
    }

    pub fn set_max_ticks(&mut self, n: u32) {
        self.max_ticks = n.max(1);
    }

    /// The hot fields of `table` in (part, field) order: what [`Prediction::record`] takes.
    pub fn hot_fields(table: &ScenePropsTable) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        for (p, part) in table.parts.iter().enumerate() {
            for (f, d) in part.iter().enumerate() {
                if d.schedule == Schedule::Hot {
                    out.push((p as u16, f as u16));
                }
            }
        }
        out
    }

    /// Starts (or restarts, with an empty history) predicting `id` with the hot fields of `table`.
    /// `initial` seeds the confirmed mirror (a `Create`'s state).
    pub fn track(
        &mut self,
        id: NetId,
        table: &ScenePropsTable,
        policy: Policy,
        initial: &[(u16, u16, Value)],
    ) {
        let mut hot = Vec::new();
        let mut index = BTreeMap::new();
        for (p, f) in Self::hot_fields(table) {
            let Some(d) = table.get(p, f) else { continue };
            index.insert((p, f), hot.len());
            hot.push(HotField {
                part: p,
                field: f,
                ty: d.ty,
                quant: d.quant,
                tolerance: d.tolerance,
            });
        }
        let mut t = Tracked {
            policy,
            mirror: std::vec![None; hot.len()],
            hot,
            index,
            history: VecDeque::new(),
        };
        for (p, f, v) in initial {
            if let Some(&i) = t.index.get(&(*p, *f)) {
                t.mirror[i] = Some(v.clone());
            }
        }
        self.tracked.insert(id, t);
    }

    pub fn untrack(&mut self, id: NetId) {
        self.tracked.remove(&id);
    }

    pub fn is_tracked(&self, id: NetId) -> bool {
        self.tracked.contains_key(&id)
    }

    pub fn policy(&self, id: NetId) -> Option<Policy> {
        self.tracked.get(&id).map(|t| t.policy)
    }

    pub fn set_policy(&mut self, id: NetId, policy: Policy) {
        if let Some(t) = self.tracked.get_mut(&id) {
            t.policy = policy;
        }
    }

    /// True when a snapshot's value for `(id, part, field)` belongs to the mirror (a hot field of a
    /// tracked, predicted entity) instead of being applied.
    pub fn diverts(&self, id: NetId, part: u16, field: u16) -> bool {
        self.tracked
            .get(&id)
            .is_some_and(|t| t.policy != Policy::Disabled && t.index.contains_key(&(part, field)))
    }

    pub fn tracked_ids(&self) -> Vec<NetId> {
        self.tracked.keys().copied().collect()
    }

    /// Records the hot state of `id` after simulating `tick` (values in [`Prediction::hot_fields`]
    /// order, quantised here). A tick already recorded is replaced (a replay); later entries go.
    pub fn record(&mut self, id: NetId, tick: u64, values: Vec<Value>) {
        let max = self.max_ticks as usize;
        let Some(t) = self.tracked.get_mut(&id) else {
            return;
        };
        let vals: Vec<Value> = values
            .iter()
            .zip(&t.hot)
            .map(|(v, h)| canonical(h.ty, h.quant, v))
            .collect();
        while t.history.back().is_some_and(|(b, _)| *b >= tick) {
            t.history.pop_back();
        }
        t.history.push_back((tick, vals));
        while t.history.len() > max {
            t.history.pop_front();
        }
    }

    /// The recorded hot state of `id` at `tick`.
    pub fn history_at(&self, id: NetId, tick: u64) -> Option<Vec<(u16, u16, Value)>> {
        let t = self.tracked.get(&id)?;
        let v = t.at(tick)?;
        Some(
            t.hot
                .iter()
                .zip(v)
                .map(|(h, v)| (h.part, h.field, v.clone()))
                .collect(),
        )
    }

    pub fn history_len(&self, id: NetId) -> usize {
        self.tracked.get(&id).map_or(0, |t| t.history.len())
    }

    /// A confirmed value from the server for `tick`.
    pub fn confirm(&mut self, id: NetId, part: u16, field: u16, value: Value, tick: u64) {
        if let Some(t) = self.tracked.get_mut(&id) {
            if let Some(&i) = t.index.get(&(part, field)) {
                t.mirror[i] = Some(value);
            }
        }
        self.mark_confirmed(tick);
    }

    /// The server confirmed its state at `tick` (a snapshot arrived), whatever it carried.
    pub fn mark_confirmed(&mut self, tick: u64) {
        self.confirmed = Some(self.confirmed.map_or(tick, |c| c.max(tick)));
    }

    /// Compares the newest confirmation with the history (once per confirmation). `None` when nothing
    /// was confirmed since the last call, nothing differs, or every policy forbids a rollback.
    pub fn evaluate(&mut self) -> Option<Rollback> {
        let mut tick = self.confirmed.take()?;
        // A confirmation from before an entity's first recorded tick, while its history is not full
        // yet (tracking started after the server's tick: a player moved right after its spawn), is
        // compared at that first tick when no entity has a record at the confirmed tick: an idle
        // server sends no newer snapshot, and the mirror holds its latest values.
        let max = self.max_ticks as usize;
        let checked = self.tracked.values().filter(|t| t.policy != Policy::Disabled);
        if checked.clone().all(|t| t.at(tick).is_none()) {
            if let Some(start) = checked
                .filter(|t| t.history.len() < max)
                .filter_map(|t| t.history.front().map(|(f, _)| *f))
                .filter(|f| *f > tick)
                .min()
            {
                tick = start;
            }
        }
        let mut mismatched = Vec::new();
        let mut any_compared = false;
        for (&id, t) in &self.tracked {
            if t.policy == Policy::Disabled {
                continue;
            }
            let Some(hist) = t.at(tick) else {
                if t.history.front().is_some_and(|(f, _)| tick < *f) {
                    self.stats.stale += 1;
                }
                continue;
            };
            any_compared = true;
            let differs = t
                .hot
                .iter()
                .zip(hist)
                .zip(&t.mirror)
                .any(|((h, p), m)| m.as_ref().is_some_and(|m| !within(m, p, h.tolerance)));
            if differs {
                self.stats.mismatches += 1;
            }
            if differs || t.policy == Policy::Always {
                mismatched.push(id);
            }
        }
        if any_compared {
            self.stats.confirmations += 1;
        }
        if mismatched.is_empty() {
            return None;
        }
        let mut entities = Vec::new();
        for (&id, t) in &self.tracked {
            if t.policy == Policy::Disabled {
                continue;
            }
            let Some(hist) = t.at(tick) else { continue };
            let state = t
                .hot
                .iter()
                .zip(hist)
                .zip(&t.mirror)
                .map(|((h, p), m)| (h.part, h.field, m.clone().unwrap_or_else(|| p.clone())))
                .collect();
            entities.push((id, state));
        }
        self.stats.rollbacks += 1;
        Some(Rollback {
            to_tick: tick,
            entities,
            mismatched,
        })
    }
}

/// The client's prediction tick lead (docs/PLAN_NETCODE.md 2.5, the tick mapping): the owner stamps its
/// inputs with the server tick it predicts, `local tick + lead`, where `lead` covers the offset to the
/// server's tick, half the round trip, the jitter (at most a quarter of the round trip, so the large
/// first estimate of an RFC 6298 deviation does not inflate it) and [`LEAD_MARGIN`] ticks, so a frame
/// reaches the server before the server simulates its tick. The session raises the lead at once when
/// the target rises (gdnet simulates the skipped ticks as catch-up ticks, so nothing is lost) and lowers
/// it only when the target fell by [`LEAD_DROP`] ticks or more (a lowered lead repeats predicted ticks,
/// which costs one correction).
pub const LEAD_MARGIN: f64 = 2.0;
pub const LEAD_DROP: i64 = 4;

pub fn lead_target(offset_ticks: f64, rtt_ticks: f64, jitter_ticks: f64) -> i64 {
    let jitter = jitter_ticks.min(rtt_ticks / 4.0).max(0.0);
    offset_ticks.round() as i64 + (rtt_ticks / 2.0 + jitter + LEAD_MARGIN).ceil() as i64
}

/// The next lead from the current one and the target (see [`lead_target`]).
pub fn next_lead(current: Option<i64>, target: i64) -> i64 {
    match current {
        Some(l) if target <= l && target > l - LEAD_DROP => l,
        _ => target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::PropDesc;

    fn table() -> ScenePropsTable {
        let mut vel = PropDesc::new("velocity", TypeTag::Vector2).hot(Quant::Mm);
        vel.tolerance = 0.001;
        ScenePropsTable::new(std::vec![std::vec![
            PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
            PropDesc::new("visible", TypeTag::Bool),
            vel,
            PropDesc::new("frame", TypeTag::Int).hot(Quant::Raw),
        ]])
    }

    fn v2(x: f32, y: f32) -> Value {
        Value::Vector2([x, y])
    }

    fn rec(p: &mut Prediction, id: NetId, tick: u64, x: f32) {
        p.record(id, tick, std::vec![v2(x, 0.0), v2(1.0, 0.0), Value::Int(3)]);
    }

    fn confirm_all(p: &mut Prediction, id: NetId, tick: u64, x: f32) {
        p.confirm(id, 0, 0, v2(x, 0.0), tick);
        p.confirm(id, 0, 2, v2(1.0, 0.0), tick);
        p.confirm(id, 0, 3, Value::Int(3), tick);
    }

    #[test]
    fn hot_fields_skip_cold_ones() {
        assert_eq!(
            Prediction::hot_fields(&table()),
            std::vec![(0, 0), (0, 2), (0, 3)]
        );
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[]);
        assert!(p.diverts(1, 0, 0));
        assert!(!p.diverts(1, 0, 1), "cold fields are applied");
        assert!(!p.diverts(2, 0, 0));
    }

    #[test]
    fn a_match_within_tolerance_does_not_roll_back() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[]);
        for t in 1..=10 {
            rec(&mut p, 1, t, t as f32);
        }
        confirm_all(&mut p, 1, 5, 5.0009);
        assert_eq!(p.evaluate(), None);
        assert_eq!(p.evaluate(), None, "evaluated once");
        assert_eq!(p.stats.confirmations, 1);
    }

    /// A server change made before the owner's first predicted tick (a player teleported right after
    /// its spawn) is still found: the confirmation is compared at the first recorded tick, since an
    /// idle server sends nothing more.
    #[test]
    fn a_confirmation_from_before_the_history_is_compared_at_its_start() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[(0, 0, v2(1140.0, 0.0))]);
        for t in 12..=15 {
            rec(&mut p, 1, t, 1140.0);
        }
        confirm_all(&mut p, 1, 10, 4000.0);
        let r = p.evaluate().expect("a rollback");
        assert_eq!(r.to_tick, 12, "to the first recorded tick");
        assert_eq!(r.entities[0].1[0], (0, 0, v2(4000.0, 0.0)));
        // A matching early confirmation changes nothing; a full window keeps the old rule.
        let mut q = Prediction::default();
        q.track(1, &table(), Policy::Check, &[]);
        for t in 12..=15 {
            rec(&mut q, 1, t, 1140.0);
        }
        confirm_all(&mut q, 1, 10, 1140.0);
        assert_eq!(q.evaluate(), None);
        let mut full = Prediction::new(4);
        full.track(1, &table(), Policy::Check, &[]);
        for t in 12..=15 {
            rec(&mut full, 1, t, 1140.0);
        }
        confirm_all(&mut full, 1, 10, 4000.0);
        assert_eq!(full.evaluate(), None, "older than a full history: stale");
        assert_eq!(full.stats.stale, 1);
    }

    #[test]
    fn a_mismatch_at_t_rolls_back_to_t() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[]);
        for t in 1..=10 {
            rec(&mut p, 1, t, t as f32);
        }
        confirm_all(&mut p, 1, 6, 56.0);
        let r = p.evaluate().expect("a rollback");
        assert_eq!(r.to_tick, 6);
        assert_eq!(r.mismatched, std::vec![1]);
        assert_eq!(r.entities.len(), 1);
        let (id, state) = &r.entities[0];
        assert_eq!(*id, 1);
        assert_eq!(state[0], (0, 0, v2(56.0, 0.0)), "the confirmed position");
        assert_eq!(state[1], (0, 2, v2(1.0, 0.0)));
        assert_eq!(p.stats.rollbacks, 1);
    }

    #[test]
    fn an_unchanged_field_is_still_compared() {
        // The server never moved (no field arrived at tick 8) but its earlier state is in the mirror.
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[(0, 0, v2(0.0, 0.0))]);
        for t in 1..=10 {
            rec(&mut p, 1, t, t as f32);
        }
        p.mark_confirmed(8);
        let r = p
            .evaluate()
            .expect("the prediction moved, the server did not");
        assert_eq!(r.entities[0].1[0], (0, 0, v2(0.0, 0.0)));
    }

    #[test]
    fn disabled_never_and_always_always() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Disabled, &[]);
        assert!(!p.diverts(1, 0, 0), "a disabled entity applies snapshots");
        for t in 1..=5 {
            rec(&mut p, 1, t, t as f32);
            confirm_all(&mut p, 1, t, 99.0);
            assert_eq!(p.evaluate(), None);
        }
        let mut q = Prediction::default();
        q.track(1, &table(), Policy::Always, &[]);
        for t in 1..=5 {
            rec(&mut q, 1, t, t as f32);
            confirm_all(&mut q, 1, t, t as f32);
            let r = q.evaluate().expect("every confirmed tick");
            assert_eq!(r.to_tick, t);
        }
        assert_eq!(q.stats.rollbacks, 5);
        assert_eq!(q.stats.mismatches, 0);
    }

    #[test]
    fn several_mismatching_entities_make_one_rollback() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[]);
        p.track(2, &table(), Policy::Check, &[]);
        p.track(3, &table(), Policy::Check, &[]);
        for t in 1..=10 {
            for id in 1..=3 {
                rec(&mut p, id, t, t as f32);
            }
        }
        confirm_all(&mut p, 1, 4, 40.0);
        confirm_all(&mut p, 2, 4, 41.0);
        confirm_all(&mut p, 3, 4, 4.0);
        let r = p.evaluate().unwrap();
        assert_eq!(r.to_tick, 4);
        assert_eq!(r.mismatched, std::vec![1, 2]);
        assert_eq!(
            r.entities.len(),
            3,
            "every tracked entity is restored and replayed"
        );
        assert_eq!(p.evaluate(), None);
        assert_eq!(p.stats.rollbacks, 1);
    }

    #[test]
    fn the_newest_confirmation_wins_and_old_ones_are_stale() {
        let mut p = Prediction::new(5);
        p.track(1, &table(), Policy::Check, &[]);
        for t in 1..=20 {
            rec(&mut p, 1, t, t as f32);
        }
        assert_eq!(p.history_len(1), 5);
        confirm_all(&mut p, 1, 3, 0.0);
        assert_eq!(p.evaluate(), None, "older than the history: not compared");
        assert_eq!(p.stats.stale, 1);
        confirm_all(&mut p, 1, 17, 0.0);
        confirm_all(&mut p, 1, 18, 18.0);
        assert_eq!(p.evaluate(), None, "18 matches; 17 was superseded");
    }

    #[test]
    fn a_replay_replaces_later_entries() {
        let mut p = Prediction::default();
        p.track(1, &table(), Policy::Check, &[]);
        for t in 1..=10 {
            rec(&mut p, 1, t, t as f32);
        }
        rec(&mut p, 1, 7, 70.0);
        assert_eq!(p.history_len(1), 7);
        assert_eq!(p.history_at(1, 7).unwrap()[0].2, v2(70.0, 0.0));
        assert!(p.history_at(1, 8).is_none());
    }

    #[test]
    fn values_are_compared_after_quantisation() {
        assert!(within(&v2(1.0, 2.0), &v2(1.001, 2.0), 0.001));
        assert!(!within(&v2(1.0, 2.0), &v2(1.0025, 2.0), 0.001));
        assert!(within(&Value::Int(3), &Value::Int(3), 0.5));
        assert!(
            !within(&Value::Int(3), &Value::Int(4), 5.0),
            "ints are exact"
        );
        assert!(!within(&Value::Bool(true), &Value::Bool(false), 1.0));
        assert_eq!(
            canonical(TypeTag::Vector2, Quant::Mm, &v2(1.00049, 0.0)),
            v2(1.0, 0.0)
        );
    }

    #[test]
    fn lead_covers_half_the_round_trip() {
        assert_eq!(lead_target(0.0, 6.0, 0.0), 5);
        assert_eq!(lead_target(-1.2, 6.0, 0.4), 5);
        assert_eq!(lead_target(0.0, 0.0, 0.0), 2);
        assert_eq!(
            lead_target(0.0, 12.0, 6.0),
            11,
            "the jitter counts at most rtt/4"
        );
        assert_eq!(next_lead(None, 7), 7);
        assert_eq!(next_lead(Some(7), 8), 8, "up at once");
        assert_eq!(next_lead(Some(8), 5), 8, "small drops are ignored");
        assert_eq!(next_lead(Some(8), 4), 4, "a clear drop lowers it");
    }
}

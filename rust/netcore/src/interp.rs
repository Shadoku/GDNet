//! Interpolation and smoothing (docs/PLAN_NETCODE.md 2.1 "Profiles" `interpolate`, 2.2 step 7, 2.5, WP5).
//!
//! **The render timeline** ([`Timeline`]). A client shows replicas on a continuous tick value, the *render
//! tick*, that runs one jitter buffer behind the newest snapshot. Snapshots of server tick `T` arrive at
//! local time `now`; the arrival offset `T - local_ticks(now)` (local ticks = `now_us / tick_us`, a
//! continuous clock) is tracked with an estimator that follows early arrivals quickly and late ones
//! slowly, so it sits at the leading edge of the stream. The target render tick is `local_ticks(now) +
//! offset - buffer`; the render tick follows it at 0.9x..1.1x speed (never backwards, never a jump unless
//! it is more than 4 ticks off). A snapshot is *late* when it arrives after the render tick passed its
//! tick (the samples it carries were needed and the timeline extrapolated instead). The buffer adapts in
//! `1..=6` ticks: it grows by one tick when more than [`LATE_TARGET`] of the last second's snapshots were
//! late (at most every 250 ms), and shrinks by one tick when, for 4 s, every arrival's lateness stayed
//! more than half a tick below what a buffer one tick smaller would allow.
//!
//! **Samples** ([`Track`]). Each interpolated field of a replica keeps a ring of `(server tick, value)`
//! samples, filled from snapshots in tick order (a sample not newer than the newest one is ignored).
//! `sample(render_tick)` interpolates between the two surrounding samples: linearly for floats, vectors
//! and colours, along the shorter arc for angles (`deg`-quantised floats and properties named
//! `rotation`), by slerp for quaternions; ints, bools, strings and anything else step. Past the newest
//! sample it extrapolates from the last two, at most one tick, and only while no newer snapshot arrived
//! (one without the field means the field stopped changing: it holds); when no snapshot at all arrives for
//! two more ticks (a server sends none while nothing changes) it settles back onto the newest sample.
//! Before the oldest sample, and with one sample, it holds. Snapshots carry only changed fields, so a field with no sample between two of its updates was
//! unchanged: when the entity appeared in a snapshot after the field's last sample, or the gap is longer
//! than [`max_gap`], a hold sample with the previous value is inserted first (an entity that stood still
//! starts moving at the right tick instead of crawling across the gap).
//!
//! **Error offsets.** When a new sample disagrees with where the stream was heading (the extrapolation of
//! the last two samples) by more than a small epsilon, it is a *correction* (a server-side teleport, a
//! knock-back): the older samples are shifted by the difference, so the motion stays continuous, and the
//! difference becomes a visual offset that cancels it now and decays to zero, by [`DECAY_SMALL`] per
//! frame, or [`DECAY_LARGE`] while the offset is larger than the per-type `large` threshold (25 cm, 10
//! degrees; 25 px for `Vector2`, whose unit is a pixel at 100 px per metre). Decay rates are per 60 Hz
//! frame and scaled by the real frame time (`factor ^ (dt * 60)`), so the smoothing looks the same at 60
//! and 144 Hz (gate G5). Above the teleport threshold (2 m, 200 px, 90 degrees; per property
//! `teleport_distance`, globally `gdnet/interpolation/teleport_distance`) the field snaps: the ring is
//! cleared and no offset is kept. A correction followed at once by another one of the same step is a
//! change of speed, not a jump (a field starting to move faster than the epsilon per tick): the first
//! correction's shift is taken back off the older samples, so the stream takes the new heading, and the
//! offset keeps the shown value continuous (WP18). Offsets are visual only; they are never recorded
//! into any history.
//!
//! **Owned entities.** After a rollback gdnet compares, per interpolated hot field, what was shown before
//! (`simulated + offset`) with the replayed value and keeps the difference as an owned offset
//! ([`Interp::blend_owned`]), decaying the same way; gdnet adds it to the property for rendering only.

use crate::entity::NetId;
use crate::props::{PropDesc, Quant, TypeTag, Value};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// The starting jitter buffer, in ticks (D8).
pub const DEFAULT_JITTER_BUFFER: u32 = 2;
pub const MIN_JITTER_BUFFER: u32 = 1;
pub const MAX_JITTER_BUFFER: u32 = 6;
/// Offset decay per 60 Hz frame for small errors.
pub const DECAY_SMALL: f64 = 0.95;
/// Offset decay per 60 Hz frame while the error is above the type's `large` threshold.
pub const DECAY_LARGE: f64 = 0.85;
/// The frame rate the decay factors are expressed in.
pub const DECAY_REFERENCE_HZ: f64 = 60.0;
/// The adaptive buffer keeps the fraction of late snapshots below this.
pub const LATE_TARGET: f64 = 0.02;
/// Distance thresholds for `Vector2` properties are in pixels: 100 px per metre.
pub const PIXELS_PER_METRE: f64 = 100.0;
/// Errors above this decay at [`DECAY_LARGE`] (metres; x100 for `Vector2`).
pub const LARGE_DISTANCE: f64 = 0.25;
/// Errors above this snap (metres; x100 for `Vector2`).
pub const TELEPORT_DISTANCE: f64 = 2.0;
/// Errors below this are not corrections (metres; x100 for `Vector2`).
pub const EPSILON_DISTANCE: f64 = 0.05;
pub const LARGE_ANGLE: f64 = 10.0 * std::f64::consts::PI / 180.0;
pub const TELEPORT_ANGLE: f64 = 90.0 * std::f64::consts::PI / 180.0;
pub const EPSILON_ANGLE: f64 = 5.0 * std::f64::consts::PI / 180.0;
/// Samples kept per field.
const RING: usize = 32;
/// An offset below this fraction of the epsilon is dropped.
const TINY: f64 = 0.01;

/// How a field's samples are blended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Steps to each sample when the render tick reaches it.
    Snap,
    /// Componentwise linear (floats, vectors, colours).
    Linear,
    /// Componentwise linear along the shorter arc, results wrapped to `[-PI, PI)` (radians).
    Angle,
    /// Spherical linear interpolation of unit quaternions along the shorter arc.
    Quat,
}

/// A field's interpolation kind and thresholds (in the property's units).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub kind: Kind,
    pub epsilon: f64,
    pub large: f64,
    pub teleport: f64,
}

impl Params {
    /// The parameters of `d`: kind from its type, quantiser and name; `default_teleport` (> 0) replaces the
    /// automatic teleport distance of distance kinds; the property's own `teleport_distance` (> 0) wins.
    pub fn for_desc(d: &PropDesc, default_teleport: f64) -> Params {
        let angle = d.quant == Quant::Deg || d.name == "rotation";
        let (kind, scale) = match d.ty {
            TypeTag::Float | TypeTag::Vector3 if angle => (Kind::Angle, 1.0),
            TypeTag::Float | TypeTag::Vector3 | TypeTag::Vector4 | TypeTag::Color => {
                (Kind::Linear, 1.0)
            }
            TypeTag::Vector2 => (Kind::Linear, PIXELS_PER_METRE),
            TypeTag::Quaternion => (Kind::Quat, 1.0),
            _ => (Kind::Snap, 1.0),
        };
        let own = f64::from(d.teleport_distance);
        match kind {
            Kind::Angle | Kind::Quat => Params {
                kind,
                epsilon: EPSILON_ANGLE,
                large: LARGE_ANGLE,
                teleport: if own > 0.0 { own } else { TELEPORT_ANGLE },
            },
            _ => Params {
                kind,
                epsilon: EPSILON_DISTANCE * scale,
                large: LARGE_DISTANCE * scale,
                teleport: if own > 0.0 {
                    own
                } else if default_teleport > 0.0 {
                    default_teleport
                } else {
                    TELEPORT_DISTANCE * scale
                },
            },
        }
    }
}

/// The longest gap (ticks) a field's samples are interpolated across: 6 ticks or 0.1 s.
pub fn max_gap(tick_rate: u32) -> u64 {
    u64::from((tick_rate / 10).max(6))
}

// --- component arithmetic -------------------------------------------------------------------------

type Comps = [f64; 4];

fn comps(v: &Value) -> Option<(Comps, usize)> {
    Some(match v {
        Value::Float(x) => ([*x, 0.0, 0.0, 0.0], 1),
        Value::Vector2(a) => ([f64::from(a[0]), f64::from(a[1]), 0.0, 0.0], 2),
        Value::Vector3(a) => ([f64::from(a[0]), f64::from(a[1]), f64::from(a[2]), 0.0], 3),
        Value::Vector4(a) | Value::Color(a) | Value::Quaternion(a) => (
            [
                f64::from(a[0]),
                f64::from(a[1]),
                f64::from(a[2]),
                f64::from(a[3]),
            ],
            4,
        ),
        _ => return None,
    })
}

/// A value of the same variant as `like` with components `c`.
fn with_comps(like: &Value, c: &Comps) -> Value {
    let f = |i: usize| c[i] as f32;
    match like {
        Value::Float(_) => Value::Float(c[0]),
        Value::Vector2(_) => Value::Vector2([f(0), f(1)]),
        Value::Vector3(_) => Value::Vector3([f(0), f(1), f(2)]),
        Value::Vector4(_) => Value::Vector4([f(0), f(1), f(2), f(3)]),
        Value::Color(_) => Value::Color([f(0), f(1), f(2), f(3)]),
        Value::Quaternion(_) => Value::Quaternion([f(0), f(1), f(2), f(3)]),
        other => other.clone(),
    }
}

/// `x` wrapped to `[-PI, PI)`.
pub fn wrap_angle(x: f64) -> f64 {
    let tau = std::f64::consts::TAU;
    let mut r = (x + std::f64::consts::PI).rem_euclid(tau) - std::f64::consts::PI;
    if r >= std::f64::consts::PI {
        r -= tau;
    }
    r
}

fn q_mul(a: &Comps, b: &Comps) -> Comps {
    let (ax, ay, az, aw) = (a[0], a[1], a[2], a[3]);
    let (bx, by, bz, bw) = (b[0], b[1], b[2], b[3]);
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

fn q_conj(a: &Comps) -> Comps {
    [-a[0], -a[1], -a[2], a[3]]
}

fn q_norm(a: &Comps) -> Comps {
    let n = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2] + a[3] * a[3]).sqrt();
    if n < 1e-12 {
        return IDENTITY;
    }
    [a[0] / n, a[1] / n, a[2] / n, a[3] / n]
}

const IDENTITY: Comps = [0.0, 0.0, 0.0, 1.0];

/// Slerp from `a` to `b` along the shorter arc; `t` may exceed 1 (extrapolation).
fn slerp(a: &Comps, b: &Comps, t: f64) -> Comps {
    let a = q_norm(a);
    let mut b = q_norm(b);
    let mut dot = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
    if dot < 0.0 {
        b = [-b[0], -b[1], -b[2], -b[3]];
        dot = -dot;
    }
    let dot = dot.min(1.0);
    let theta = dot.acos();
    if theta < 1e-6 {
        // nlerp
        return q_norm(&[
            a[0] + (b[0] - a[0]) * t,
            a[1] + (b[1] - a[1]) * t,
            a[2] + (b[2] - a[2]) * t,
            a[3] + (b[3] - a[3]) * t,
        ]);
    }
    let s = theta.sin();
    let wa = ((1.0 - t) * theta).sin() / s;
    let wb = (t * theta).sin() / s;
    q_norm(&[
        a[0] * wa + b[0] * wb,
        a[1] * wa + b[1] * wb,
        a[2] * wa + b[2] * wb,
        a[3] * wa + b[3] * wb,
    ])
}

/// The rotation angle of a unit quaternion (0..PI).
fn q_angle(q: &Comps) -> f64 {
    2.0 * q[3].abs().min(1.0).acos()
}

/// Blends `a` towards `b` by `t` (t > 1 extrapolates).
pub fn blend(kind: Kind, a: &Value, b: &Value, t: f64) -> Value {
    let (Some((ca, n)), Some((cb, nb))) = (comps(a), comps(b)) else {
        return if t >= 1.0 { b.clone() } else { a.clone() };
    };
    if n != nb {
        return b.clone();
    }
    match kind {
        Kind::Snap => {
            if t >= 1.0 {
                b.clone()
            } else {
                a.clone()
            }
        }
        Kind::Linear => {
            let mut c = [0.0; 4];
            for i in 0..n {
                c[i] = ca[i] + (cb[i] - ca[i]) * t;
            }
            with_comps(b, &c)
        }
        Kind::Angle => {
            let mut c = [0.0; 4];
            for i in 0..n {
                c[i] = wrap_angle(ca[i] + wrap_angle(cb[i] - ca[i]) * t);
            }
            with_comps(b, &c)
        }
        Kind::Quat => with_comps(b, &slerp(&ca, &cb, t)),
    }
}

/// The difference `b - a` as an offset (a rotation `b * a^-1` for quaternions), and its magnitude.
fn diff(kind: Kind, a: &Value, b: &Value) -> Option<(Comps, f64)> {
    let (ca, n) = comps(a)?;
    let (cb, nb) = comps(b)?;
    if n != nb {
        return None;
    }
    match kind {
        Kind::Snap => None,
        Kind::Linear | Kind::Angle => {
            let mut d = [0.0; 4];
            for i in 0..n {
                d[i] = cb[i] - ca[i];
                if kind == Kind::Angle {
                    d[i] = wrap_angle(d[i]);
                }
            }
            let m = d.iter().map(|x| x * x).sum::<f64>().sqrt();
            Some((d, m))
        }
        Kind::Quat => {
            let mut q = q_norm(&q_mul(&q_norm(&cb), &q_conj(&q_norm(&ca))));
            if q[3] < 0.0 {
                q = [-q[0], -q[1], -q[2], -q[3]];
            }
            Some((q, q_angle(&q)))
        }
    }
}

/// `v` moved by offset `d`.
fn add(kind: Kind, v: &Value, d: &Comps) -> Value {
    let Some((c, n)) = comps(v) else {
        return v.clone();
    };
    match kind {
        Kind::Snap => v.clone(),
        Kind::Linear | Kind::Angle => {
            let mut o = [0.0; 4];
            for i in 0..n {
                o[i] = c[i] + d[i];
                if kind == Kind::Angle {
                    o[i] = wrap_angle(o[i]);
                }
            }
            with_comps(v, &o)
        }
        Kind::Quat => with_comps(v, &q_norm(&q_mul(d, &c))),
    }
}

fn offset_magnitude(kind: Kind, o: &Comps) -> f64 {
    match kind {
        Kind::Quat => q_angle(o),
        _ => o.iter().map(|x| x * x).sum::<f64>().sqrt(),
    }
}

/// `o` minus the correction `d` (the offset that keeps the displayed value where it was after the
/// samples moved by `d`).
fn offset_minus(kind: Kind, o: Option<Comps>, d: &Comps) -> Comps {
    match kind {
        Kind::Quat => q_norm(&q_mul(&o.unwrap_or(IDENTITY), &q_conj(d))),
        _ => {
            let o = o.unwrap_or([0.0; 4]);
            [o[0] - d[0], o[1] - d[1], o[2] - d[2], o[3] - d[3]]
        }
    }
}

/// `v` moved back by offset `d` (the inverse of [`add`]).
fn sub(kind: Kind, v: &Value, d: &Comps) -> Value {
    match kind {
        Kind::Quat => add(kind, v, &q_conj(d)),
        _ => add(kind, v, &[-d[0], -d[1], -d[2], -d[3]]),
    }
}

/// `o` plus `d`: undoes [`offset_minus`].
fn offset_plus(kind: Kind, o: Option<Comps>, d: &Comps) -> Comps {
    match kind {
        Kind::Quat => q_norm(&q_mul(&o.unwrap_or(IDENTITY), d)),
        _ => {
            let o = o.unwrap_or([0.0; 4]);
            [o[0] + d[0], o[1] + d[1], o[2] + d[2], o[3] + d[3]]
        }
    }
}

/// A decaying visual offset of one field.
#[derive(Clone, Debug, PartialEq)]
pub struct Offset {
    pub params: Params,
    pub value: Comps,
}

impl Offset {
    pub fn magnitude(&self) -> f64 {
        offset_magnitude(self.params.kind, &self.value)
    }

    /// One frame of decay over `dt` seconds; false once it is negligible (drop it).
    pub fn decay(&mut self, dt: f64) -> bool {
        let base = if self.magnitude() > self.params.large {
            DECAY_LARGE
        } else {
            DECAY_SMALL
        };
        let f = base.powf(dt.max(0.0) * DECAY_REFERENCE_HZ);
        match self.params.kind {
            Kind::Quat => self.value = slerp(&IDENTITY, &self.value, f),
            _ => self.value.iter_mut().for_each(|x| *x *= f),
        }
        self.magnitude() > self.params.epsilon * TINY
    }

    /// `v` with the offset added.
    pub fn apply(&self, v: &Value) -> Value {
        add(self.params.kind, v, &self.value)
    }
}

/// What pushing a sample did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// Not newer than the newest sample: ignored.
    Ignored,
    Added,
    /// It disagreed with the stream: the older samples moved, an offset was started.
    Corrected,
    /// It disagreed by more than the teleport threshold: the field snaps.
    Teleported,
}

/// The samples and offset of one interpolated field.
#[derive(Clone, Debug)]
pub struct Track {
    pub params: Params,
    samples: VecDeque<(u64, Value)>,
    offset: Option<Offset>,
    last_out: Option<Value>,
    /// The last push's correction: the tick of the sample it came with and the shift it applied to
    /// the samples before it (WP18: a second correction by the same step right after it means the
    /// field changed its velocity, not its place).
    last_correction: Option<(u64, Comps)>,
}

impl Track {
    pub fn new(params: Params) -> Self {
        Self {
            params,
            samples: VecDeque::new(),
            offset: None,
            last_out: None,
            last_correction: None,
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn offset(&self) -> Option<&Offset> {
        self.offset.as_ref()
    }

    pub fn newest_tick(&self) -> Option<u64> {
        self.samples.back().map(|s| s.0)
    }

    /// A known value with no tick (a replica's initial state): held until the first sample.
    pub fn seed(&mut self, value: Value) {
        if self.samples.is_empty() {
            self.samples.push_back((0, value));
        }
    }

    /// The stream's value at `t` from the last two samples, without the one-tick cap.
    fn heading(&self, t: f64) -> Option<Value> {
        let n = self.samples.len();
        let (tb, b) = self.samples.back()?;
        if n < 2 {
            return Some(b.clone());
        }
        let (ta, a) = &self.samples[n - 2];
        let span = (*tb - *ta).max(1) as f64;
        Some(blend(self.params.kind, a, b, (t - *ta as f64) / span))
    }

    /// Adds the sample `(tick, value)`. `seen` is a tick at which the entity appeared in a snapshot
    /// without this field (it was unchanged then); `max_gap` the longest gap interpolated across.
    pub fn push(&mut self, tick: u64, value: Value, seen: Option<u64>, max_gap: u64) -> Pushed {
        let mut held = false;
        let newest = self.samples.back().map(|s| s.0);
        if let Some(&(pt, ref pv)) = self.samples.back() {
            if tick <= pt {
                return Pushed::Ignored;
            }
            // Hold samples: the field kept its value until the entity was last seen without it, or
            // (a gap too long to interpolate across) until the tick before this one.
            let mut hold = seen.filter(|s| *s > pt && *s < tick);
            if tick - hold.unwrap_or(pt) > max_gap {
                hold = Some(tick - 1);
            }
            if let Some(h) = hold.filter(|h| *h > pt) {
                let pv = pv.clone();
                self.samples.push_back((h, pv));
                held = true;
            }
        }
        let last_correction = self.last_correction.take();
        let mut result = Pushed::Added;
        // A correction needs a heading: two earlier samples (a hold pair counts: standing still).
        if self.params.kind != Kind::Snap && self.samples.len() >= 2 {
            if let Some(expected) = self.heading(tick as f64) {
                if let Some((d, m)) = diff(self.params.kind, &expected, &value) {
                    let kind = self.params.kind;
                    // The previous sample was a correction by the same step: the field did not jump,
                    // it started moving at a new speed (a step over the epsilon per tick). Take that
                    // correction's shift back off the older samples, so the stream has the new heading,
                    // and keep the shown value continuous through the offset.
                    let restarted = last_correction.filter(|(t, prev)| {
                        !held
                            && Some(*t) == newest
                            && offset_magnitude(kind, &offset_minus(kind, Some(d), prev))
                                <= self.params.epsilon
                    });
                    if m > self.params.teleport {
                        self.samples.clear();
                        self.offset = None;
                        result = Pushed::Teleported;
                    } else if let Some((t, prev)) = restarted {
                        for s in self.samples.iter_mut().filter(|s| s.0 < t) {
                            s.1 = sub(kind, &s.1, &prev);
                        }
                        let o = offset_plus(kind, self.offset.as_ref().map(|o| o.value), &prev);
                        self.offset = (offset_magnitude(kind, &o) > self.params.epsilon * TINY)
                            .then_some(Offset {
                                params: self.params,
                                value: o,
                            });
                    } else if m > self.params.epsilon {
                        for s in self.samples.iter_mut() {
                            s.1 = add(kind, &s.1, &d);
                        }
                        let o = offset_minus(kind, self.offset.as_ref().map(|o| o.value), &d);
                        self.offset = Some(Offset {
                            params: self.params,
                            value: o,
                        });
                        self.last_correction = Some((tick, d));
                        result = Pushed::Corrected;
                    }
                }
            }
        }
        self.samples.push_back((tick, value));
        while self.samples.len() > RING {
            self.samples.pop_front();
        }
        result
    }

    /// Decays the offset over `dt` seconds.
    pub fn decay(&mut self, dt: f64) {
        if let Some(o) = self.offset.as_mut() {
            if !o.decay(dt) {
                self.offset = None;
            }
        }
    }

    /// The value at render tick `r` without the offset (None without samples). `None` for `r` means no
    /// timeline yet: the newest sample.
    pub fn value_at(&self, r: Option<f64>) -> Option<Value> {
        self.value_until(r, None)
    }

    /// `value_at`, knowing that snapshots up to tick `newest` arrived: past the newest sample the field
    /// extrapolates only while nothing newer arrived (a newer snapshot without the field means it did
    /// not change: it holds, so an entity that stopped does not drift a tick ahead).
    pub fn value_until(&self, r: Option<f64>, newest: Option<u64>) -> Option<Value> {
        let (tb, b) = self.samples.back()?;
        let Some(r) = r else {
            return Some(b.clone());
        };
        let (t0, v0) = self.samples.front()?;
        if r <= *t0 as f64 {
            return Some(v0.clone());
        }
        if r >= *tb as f64 {
            if newest.is_some_and(|n| n > *tb) {
                return Some(b.clone());
            }
            // Extrapolate at most one tick past the newest sample; when two more ticks pass with no
            // snapshot at all (the server sends none while nothing changes), settle back onto it over
            // the next tick instead of staying a tick ahead.
            let over = r - *tb as f64;
            let e = over.min(1.0) - (over - 2.0).clamp(0.0, 1.0);
            return self.heading(*tb as f64 + e);
        }
        let i = self.samples.partition_point(|s| (s.0 as f64) <= r);
        let (ta, a) = &self.samples[i - 1];
        let (tc, c) = &self.samples[i];
        let t = (r - *ta as f64) / (*tc - *ta) as f64;
        Some(blend(self.params.kind, a, c, t))
    }

    /// The displayed value at render tick `r` (with the offset); drops samples the timeline passed (one
    /// before `r` is kept).
    pub fn sample(&mut self, r: Option<f64>, newest: Option<u64>) -> Option<Value> {
        if let Some(r) = r {
            while self.samples.len() > 2 && (self.samples[1].0 as f64) <= r {
                self.samples.pop_front();
            }
        }
        let v = self.value_until(r, newest)?;
        Some(match &self.offset {
            Some(o) => o.apply(&v),
            None => v,
        })
    }

    /// Like `sample`, but only when the displayed value changed since the last call that returned one.
    pub fn sample_changed(&mut self, r: Option<f64>, newest: Option<u64>) -> Option<Value> {
        let v = self.sample(r, newest)?;
        if self.last_out.as_ref() == Some(&v) {
            return None;
        }
        self.last_out = Some(v.clone());
        Some(v)
    }
}

/// Timeline counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InterpStats {
    pub snapshots: u64,
    pub late_snapshots: u64,
    /// Corrections turned into offsets (replica corrections and owned blends).
    pub corrections: u64,
    pub teleports: u64,
    pub buffer_grown: u64,
    pub buffer_shrunk: u64,
}

/// The render timeline and its adaptive jitter buffer.
#[derive(Clone, Debug)]
pub struct Timeline {
    tick_us: f64,
    buffer: u32,
    adaptive: bool,
    /// Estimated `snapshot tick - local ticks` at the leading edge of the arrivals.
    offset: Option<f64>,
    render: Option<f64>,
    last_now: Option<u64>,
    newest: Option<u64>,
    /// (arrival time, late, lateness in ticks behind the leading edge).
    arrivals: VecDeque<(u64, bool, f64)>,
    last_change: u64,
    pub stats: InterpStats,
}

/// Buffer changes are at least this far apart.
const GROW_COOLDOWN_US: u64 = 250_000;
const SHRINK_AFTER_US: u64 = 4_000_000;
const WINDOW_US: u64 = 1_000_000;
const HISTORY_US: u64 = 4_000_000;

impl Timeline {
    pub fn new(tick_rate: u32) -> Self {
        Self {
            tick_us: 1_000_000.0 / f64::from(tick_rate.max(1)),
            buffer: DEFAULT_JITTER_BUFFER,
            adaptive: true,
            offset: None,
            render: None,
            last_now: None,
            newest: None,
            arrivals: VecDeque::new(),
            last_change: 0,
            stats: InterpStats::default(),
        }
    }

    /// Sets the buffer (clamped to 1..=6) and whether it adapts.
    pub fn configure(&mut self, buffer: u32, adaptive: bool) {
        let b = buffer.clamp(MIN_JITTER_BUFFER, MAX_JITTER_BUFFER);
        if b != self.buffer || adaptive != self.adaptive {
            self.buffer = b;
            self.adaptive = adaptive;
        }
    }

    pub fn buffer(&self) -> u32 {
        self.buffer
    }

    pub fn adaptive(&self) -> bool {
        self.adaptive
    }

    pub fn render_tick(&self) -> Option<f64> {
        self.render
    }

    pub fn newest(&self) -> Option<u64> {
        self.newest
    }

    /// Ticks the render tick runs behind the newest snapshot, as estimated at the last `advance`
    /// (the leading edge of the arrivals now, minus the render tick; 0 before both exist).
    pub fn render_lag(&self) -> f64 {
        match (self.offset, self.render, self.last_now) {
            (Some(o), Some(r), Some(now)) => self.local(now) + o - r,
            _ => 0.0,
        }
    }

    fn local(&self, now_us: u64) -> f64 {
        now_us as f64 / self.tick_us
    }

    /// A snapshot of server tick `tick` arrived at `now_us` (applied, or stale: overtaken by a newer one;
    /// both measure the jitter).
    pub fn on_snapshot(&mut self, tick: u64, now_us: u64) {
        let s = tick as f64 - self.local(now_us);
        // The render tick at this moment (it advances between frames at the clock's rate).
        let now_render = self
            .render
            .zip(self.last_now)
            .map(|(r, l)| r + now_us.saturating_sub(l) as f64 / self.tick_us);
        let late = now_render.is_some_and(|r| (tick as f64) < r);
        let o = match self.offset {
            None => {
                // Adaptation starts counting from the first snapshot.
                self.last_change = now_us;
                s
            }
            Some(o) if s > o => o + 0.5 * (s - o),
            Some(o) => o + 0.005 * (s - o),
        };
        self.offset = Some(o);
        self.newest = Some(self.newest.map_or(tick, |n| n.max(tick)));
        self.stats.snapshots += 1;
        if late {
            self.stats.late_snapshots += 1;
        }
        self.arrivals.push_back((now_us, late, o - s));
        while self
            .arrivals
            .front()
            .is_some_and(|a| now_us.saturating_sub(a.0) > HISTORY_US)
        {
            self.arrivals.pop_front();
        }
        if self.adaptive {
            self.adapt(now_us);
        }
    }

    fn adapt(&mut self, now: u64) {
        let since = self.last_change;
        let recent = self
            .arrivals
            .iter()
            .filter(|a| a.0 >= since && now.saturating_sub(a.0) <= WINDOW_US);
        let (mut n, mut late) = (0u32, 0u32);
        for a in recent {
            n += 1;
            late += u32::from(a.1);
        }
        if late >= 2
            && f64::from(late) > LATE_TARGET * f64::from(n.max(10))
            && now.saturating_sub(self.last_change) >= GROW_COOLDOWN_US
            && self.buffer < MAX_JITTER_BUFFER
        {
            self.buffer += 1;
            self.last_change = now;
            self.stats.buffer_grown += 1;
            return;
        }
        if self.buffer > MIN_JITTER_BUFFER
            && now.saturating_sub(self.last_change) >= SHRINK_AFTER_US
        {
            let worst = self
                .arrivals
                .iter()
                .filter(|a| a.0 >= since)
                .map(|a| a.2)
                .fold(0.0f64, f64::max);
            if worst + 0.5 < f64::from(self.buffer - 1) {
                self.buffer -= 1;
                self.stats.buffer_shrunk += 1;
            }
            self.last_change = now;
        }
    }

    /// Advances the render tick to `now_us`; returns it (None before the first snapshot).
    pub fn advance(&mut self, now_us: u64) -> Option<f64> {
        let dt_ticks = self
            .last_now
            .map_or(0.0, |l| now_us.saturating_sub(l) as f64 / self.tick_us);
        self.last_now = Some(now_us);
        let o = self.offset?;
        let target = self.local(now_us) + o - f64::from(self.buffer);
        self.render = Some(match self.render {
            Some(r) if (target - r - dt_ticks).abs() <= 4.0 => {
                let e = target - (r + dt_ticks);
                r + dt_ticks * (1.0 + (0.2 * e).clamp(-0.1, 0.1))
            }
            _ => target,
        });
        self.render
    }
}

/// Field key: (entity, part, field).
pub type FieldKey = (NetId, u16, u16);

/// Every replica's sample buffers, the owned offsets and the timeline (one per client session).
#[derive(Clone, Debug)]
pub struct Interp {
    pub timeline: Timeline,
    tracks: BTreeMap<FieldKey, Track>,
    owned: BTreeMap<FieldKey, Offset>,
    /// Per entity: the tick it last appeared in a received update, and the one before.
    seen: BTreeMap<NetId, (u64, u64)>,
    max_gap: u64,
    default_teleport: f64,
    last_frame: Option<u64>,
}

impl Interp {
    pub fn new(tick_rate: u32) -> Self {
        Self {
            timeline: Timeline::new(tick_rate),
            tracks: BTreeMap::new(),
            owned: BTreeMap::new(),
            seen: BTreeMap::new(),
            max_gap: max_gap(tick_rate),
            default_teleport: 0.0,
            last_frame: None,
        }
    }

    /// `gdnet/interpolation/teleport_distance` (0 = automatic).
    pub fn set_default_teleport(&mut self, d: f64) {
        self.default_teleport = d.max(0.0);
    }

    pub fn params(&self, d: &PropDesc) -> Params {
        Params::for_desc(d, self.default_teleport)
    }

    pub fn track(&self, key: FieldKey) -> Option<&Track> {
        self.tracks.get(&key)
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// A received value of an interpolated field of a replica.
    pub fn push(&mut self, key: FieldKey, desc: &PropDesc, tick: u64, value: Value) -> Pushed {
        let (id, _, _) = key;
        let seen = {
            let e = self.seen.entry(id).or_insert((0, 0));
            if tick > e.0 {
                *e = (tick, e.0);
            }
            if e.0 == tick {
                e.1
            } else {
                e.0
            }
        };
        let params = self.params(desc);
        let track = self.tracks.entry(key).or_insert_with(|| Track::new(params));
        let r = track.push(tick, value, (seen > 0).then_some(seen), self.max_gap);
        match r {
            Pushed::Corrected => self.timeline.stats.corrections += 1,
            Pushed::Teleported => self.timeline.stats.teleports += 1,
            _ => {}
        }
        r
    }

    /// A replica field's initial value (held until its first sample).
    pub fn seed(&mut self, key: FieldKey, desc: &PropDesc, value: Value) {
        let params = self.params(desc);
        self.tracks
            .entry(key)
            .or_insert_with(|| Track::new(params))
            .seed(value);
    }

    /// Forgets everything about entity `id` (destroyed, or its authority changed).
    pub fn reset(&mut self, id: NetId) {
        self.tracks.retain(|k, _| k.0 != id);
        self.owned.retain(|k, _| k.0 != id);
        self.seen.remove(&id);
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.owned.clear();
        self.seen.clear();
    }

    /// One render frame at `now_us`: advances the timeline, decays every offset, and returns the
    /// replica fields whose displayed value changed.
    pub fn frame(&mut self, now_us: u64) -> Vec<(FieldKey, Value)> {
        let dt = self
            .last_frame
            .map_or(0.0, |l| now_us.saturating_sub(l) as f64 / 1_000_000.0);
        self.last_frame = Some(now_us);
        let r = self.timeline.advance(now_us);
        let newest = self.timeline.newest();
        self.owned.retain(|_, o| o.decay(dt));
        let mut out = Vec::new();
        for (k, t) in self.tracks.iter_mut() {
            t.decay(dt);
            if let Some(v) = t.sample_changed(r, newest) {
                out.push((*k, v));
            }
        }
        out
    }

    /// After a rollback of an owned entity: what was shown before (`displayed`) against the replayed
    /// value; the difference becomes the field's owned offset (none above the teleport threshold).
    /// Returns true when an offset is kept.
    pub fn blend_owned(
        &mut self,
        key: FieldKey,
        desc: &PropDesc,
        displayed: &Value,
        replayed: &Value,
    ) -> bool {
        let params = self.params(desc);
        let Some((d, m)) = diff(params.kind, replayed, displayed) else {
            return false;
        };
        if m > params.teleport || m <= params.epsilon * TINY {
            if m > params.teleport {
                self.timeline.stats.teleports += 1;
            }
            self.owned.remove(&key);
            return false;
        }
        if m > params.epsilon {
            self.timeline.stats.corrections += 1;
        }
        self.owned.insert(key, Offset { params, value: d });
        true
    }

    /// The owned fields with an offset.
    pub fn owned_keys(&self) -> Vec<FieldKey> {
        self.owned.keys().copied().collect()
    }

    pub fn owned_offset(&self, key: FieldKey) -> Option<&Offset> {
        self.owned.get(&key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::PropDesc;

    fn desc(name: &str, ty: TypeTag, q: Quant) -> PropDesc {
        let mut d = PropDesc::new(name, ty).hot(q);
        d.interpolate = true;
        d
    }

    fn pos2() -> PropDesc {
        desc("position", TypeTag::Vector2, Quant::Mm)
    }

    fn v2(v: &Value) -> [f32; 2] {
        match v {
            Value::Vector2(a) => *a,
            other => panic!("{other:?}"),
        }
    }

    fn f(v: &Value) -> f64 {
        match v {
            Value::Float(x) => *x,
            other => panic!("{other:?}"),
        }
    }

    fn track(d: &PropDesc) -> Track {
        Track::new(Params::for_desc(d, 0.0))
    }

    #[test]
    fn linear_sampling_between_two_samples() {
        let mut t = track(&pos2());
        t.push(10, Value::Vector2([0.0, 0.0]), None, 6);
        t.push(11, Value::Vector2([0.5, 0.0]), None, 6);
        assert!(t.offset().is_none(), "below the epsilon");
        let v = t.value_at(Some(10.5)).unwrap();
        assert!((v2(&v)[0] - 0.25).abs() < 1e-6, "{v:?}");
        let v = t.value_at(Some(10.0)).unwrap();
        assert_eq!(v2(&v), [0.0, 0.0]);
        let v = t.value_at(Some(9.0)).unwrap();
        assert_eq!(v2(&v), [0.0, 0.0], "before the oldest sample: hold");
        // Steady motion: three samples on a line, no correction.
        let mut t = track(&pos2());
        for i in 0..4u64 {
            assert_ne!(
                t.push(20 + i, Value::Vector2([3.0 * i as f32, 0.0]), None, 6),
                Pushed::Corrected,
                "sample {i}"
            );
        }
        let v = t.value_at(Some(21.25)).unwrap();
        assert!((v2(&v)[0] - 3.75).abs() < 1e-5, "{v:?}");
    }

    #[test]
    fn quaternion_sampling_stays_unit_and_takes_the_short_arc() {
        let d = desc("quaternion", TypeTag::Quaternion, Quant::Smallest3);
        let mut t = track(&d);
        // 0 and +80 degrees about z, the second written with a negative w (same rotation, far hemisphere).
        let a = Value::Quaternion([0.0, 0.0, 0.0, 1.0]);
        let angle = 80f32.to_radians() / 2.0;
        let b = Value::Quaternion([0.0, 0.0, -angle.sin(), -angle.cos()]);
        t.push(1, a, None, 6);
        t.push(2, b, None, 6);
        for k in 0..=10 {
            let v = t.value_at(Some(1.0 + f64::from(k) / 10.0)).unwrap();
            let Value::Quaternion(q) = v else { panic!() };
            let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
            assert!((n - 1.0).abs() < 1e-5, "unit {q:?}");
            // The short arc: the rotation angle grows to 80 degrees, never through 280.
            let ang = 2.0 * f64::from(q[3].abs()).min(1.0).acos();
            assert!(ang.to_degrees() <= 80.0 + 1e-3, "{k}: {}", ang.to_degrees());
            if k == 5 {
                assert!(
                    (ang.to_degrees() - 40.0).abs() < 0.01,
                    "halfway {}",
                    ang.to_degrees()
                );
            }
        }
    }

    /// WP18: a steady turn about a fixed axis (each sample one tick's rotation further, some written
    /// as -q, as `smallest3` does) is a heading, never a correction.
    #[test]
    fn a_steady_quaternion_turn_is_no_correction() {
        let d = desc("quaternion", TypeTag::Quaternion, Quant::Smallest3);
        let mut t = track(&d);
        let axis = [0.3f64, 1.0, 0.2];
        let n = (axis[0] * axis[0] + axis[1] * axis[1] + axis[2] * axis[2]).sqrt();
        let half = 0.21f64 / 2.0;
        let r = [
            axis[0] / n * half.sin(),
            axis[1] / n * half.sin(),
            axis[2] / n * half.sin(),
            half.cos(),
        ];
        let mut q = q_norm(&[0.4, -0.2, 0.7, 0.5]);
        for i in 0..40u64 {
            let s = if i % 3 == 0 { -1.0 } else { 1.0 };
            let v = Value::Quaternion(q.map(|c| (c * s) as f32));
            assert_ne!(t.push(10 + i, v, None, 6), Pushed::Corrected, "sample {i}");
            q = q_norm(&q_mul(&r, &q));
        }
        assert!(t.offset().is_none());
    }

    /// WP18: a field that starts moving faster than the correction epsilon per tick (a body falling
    /// at 4 m/s, a wheel turning 12 rad/s) is corrected once at the start; from the second step on the
    /// stream has the new heading, so no further sample is a correction and the offset stays small.
    #[test]
    fn a_fast_start_is_corrected_once_then_followed() {
        // 0.08 m per tick (4.8 m/s at 60 Hz), above the 0.05 m epsilon.
        let d = desc("position", TypeTag::Vector3, Quant::Mm);
        let mut t = track(&d);
        t.push(10, Value::Vector3([1.0, 0.0, 0.0]), None, 6);
        t.push(11, Value::Vector3([1.0, 0.0, 0.0]), None, 6);
        let mut corrections = 0;
        for i in 1..20u64 {
            let v = Value::Vector3([1.0 + 0.08 * i as f32, 0.0, 0.0]);
            if t.push(11 + i, v, None, 6) == Pushed::Corrected {
                corrections += 1;
            }
        }
        assert_eq!(corrections, 1, "one correction at the start");
        let o = t.offset().map_or(0.0, Offset::magnitude);
        assert!(o < 0.08, "the offset is under one step: {o}");
        // Between the last two samples the value is on the line.
        let Value::Vector3(v) = t.value_at(Some(29.5)).unwrap() else {
            panic!()
        };
        assert!((v[0] - (1.0 + 0.08 * 18.5)).abs() < 1e-4, "{v:?}");
        // The same for a quaternion turning 0.21 rad a tick from rest.
        let d = desc("quaternion", TypeTag::Quaternion, Quant::Smallest3);
        let mut t = track(&d);
        let half = 0.21f64 / 2.0;
        let r = [0.0, half.sin(), 0.0, half.cos()];
        let mut q = q_norm(&[0.4, -0.2, 0.7, 0.5]);
        t.push(10, Value::Quaternion(q.map(|c| c as f32)), None, 6);
        t.push(11, Value::Quaternion(q.map(|c| c as f32)), None, 6);
        let mut corrections = 0;
        for i in 1..20u64 {
            q = q_norm(&q_mul(&r, &q));
            if t.push(11 + i, Value::Quaternion(q.map(|c| c as f32)), None, 6) == Pushed::Corrected
            {
                corrections += 1;
            }
        }
        assert_eq!(corrections, 1, "one correction at the start of the turn");
        let o = t.offset().map_or(0.0, Offset::magnitude);
        assert!(o < 0.21, "the offset is under one step: {o}");
    }

    #[test]
    fn angles_wrap_around_the_seam() {
        let d = desc("rotation", TypeTag::Float, Quant::Deg);
        let mut t = track(&d);
        t.push(1, Value::Float(3.10), None, 6);
        t.push(2, Value::Float(-3.10), None, 6);
        let mid = f(&t.value_at(Some(1.5)).unwrap());
        // Halfway across the seam is PI (wrapped to -PI), not 0.
        assert!(mid.abs() > 3.13, "{mid}");
        let q = f(&t.value_at(Some(1.25)).unwrap());
        assert!(q > 3.1 && q < std::f64::consts::PI, "{q}");
        assert!((wrap_angle(3.5) - (3.5 - std::f64::consts::TAU)).abs() < 1e-12);
        assert_eq!(Params::for_desc(&d, 0.0).kind, Kind::Angle);
        // `rotation` is angle-aware whatever the quantiser.
        let raw = desc("rotation", TypeTag::Float, Quant::Raw);
        assert_eq!(Params::for_desc(&raw, 0.0).kind, Kind::Angle);
    }

    #[test]
    fn extrapolation_is_capped_at_one_tick() {
        let mut t = track(&pos2());
        t.push(1, Value::Vector2([0.0, 0.0]), None, 6);
        t.push(2, Value::Vector2([0.5, 0.0]), None, 6);
        let v = t.value_at(Some(2.5)).unwrap();
        assert!((v2(&v)[0] - 0.75).abs() < 1e-6, "{v:?}");
        let v = t.value_at(Some(3.5)).unwrap();
        assert!(
            (v2(&v)[0] - 1.0).abs() < 1e-6,
            "one tick past the newest: {v:?}"
        );
        let v = t.value_at(Some(4.5)).unwrap();
        assert!(
            (v2(&v)[0] - 0.75).abs() < 1e-6,
            "no snapshot for two more ticks: settling {v:?}"
        );
        let v = t.value_at(Some(10.0)).unwrap();
        assert_eq!(
            v2(&v),
            [0.5, 0.0],
            "the stream stopped: back on the newest sample"
        );
        // A newer snapshot arrived without the field: it stopped, so it holds at once.
        let v = t.value_until(Some(2.5), Some(3)).unwrap();
        assert_eq!(v2(&v), [0.5, 0.0], "held after it stopped");
    }

    #[test]
    fn hold_without_samples_and_with_one() {
        let mut t = track(&pos2());
        assert_eq!(
            t.sample(Some(5.0), None),
            None,
            "no samples: nothing to write (hold)"
        );
        t.push(7, Value::Vector2([4.0, 4.0]), None, 6);
        assert_eq!(t.value_at(Some(3.0)), Some(Value::Vector2([4.0, 4.0])));
        assert_eq!(t.value_at(Some(9.0)), Some(Value::Vector2([4.0, 4.0])));
        let mut i = Interp::new(60);
        assert!(i.frame(1000).is_empty());
    }

    #[test]
    fn an_older_sample_is_ignored() {
        let mut t = track(&pos2());
        t.push(5, Value::Vector2([1.0, 0.0]), None, 6);
        assert_eq!(
            t.push(5, Value::Vector2([2.0, 0.0]), None, 6),
            Pushed::Ignored
        );
        assert_eq!(
            t.push(3, Value::Vector2([2.0, 0.0]), None, 6),
            Pushed::Ignored
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t.value_at(Some(5.0)), Some(Value::Vector2([1.0, 0.0])));
    }

    #[test]
    fn a_field_that_stood_still_holds_until_it_moves() {
        let mut t = track(&pos2());
        t.seed(Value::Vector2([0.0, 0.0]));
        t.push(100, Value::Vector2([0.5, 0.0]), None, 6);
        // Without the hold the field would crawl from tick 0; with it, it is still at 0 at tick 99.
        assert_eq!(t.value_at(Some(99.0)), Some(Value::Vector2([0.0, 0.0])));
        // The entity appeared at tick 40 without this field: unchanged then.
        let mut t = track(&pos2());
        t.push(38, Value::Vector2([0.0, 0.0]), None, 6);
        t.push(41, Value::Vector2([0.6, 0.0]), Some(40), 6);
        assert_eq!(t.value_at(Some(40.0)), Some(Value::Vector2([0.0, 0.0])));
        assert!(max_gap(60) == 6 && max_gap(120) == 12);
    }

    #[test]
    fn corrections_decay_small_and_large_and_teleports_snap() {
        let d = pos2();
        let p = Params::for_desc(&d, 0.0);
        assert_eq!((p.epsilon, p.large, p.teleport), (5.0, 25.0, 200.0));
        let d3 = desc("position", TypeTag::Vector3, Quant::Mm);
        let p3 = Params::for_desc(&d3, 0.0);
        assert_eq!((p3.large, p3.teleport), (0.25, 2.0));
        assert_eq!(Params::for_desc(&d3, 5.0).teleport, 5.0, "the setting");
        let mut d3t = d3.clone();
        d3t.teleport_distance = 0.5;
        assert_eq!(
            Params::for_desc(&d3t, 5.0).teleport,
            0.5,
            "the property wins"
        );

        // A 20 px correction: small, decays by 0.95 per 60 Hz frame, never overshoots.
        let mut t = track(&d);
        t.push(1, Value::Vector2([0.0, 0.0]), None, 6);
        t.push(2, Value::Vector2([0.0, 0.0]), None, 6);
        assert_eq!(
            t.push(3, Value::Vector2([20.0, 0.0]), None, 6),
            Pushed::Corrected
        );
        // The displayed value did not jump.
        let shown = t.sample(Some(2.0), None).unwrap();
        assert!(v2(&shown)[0].abs() < 1e-4, "{shown:?}");
        let mut last = 0.0f32;
        for k in 1..=60 {
            t.decay(1.0 / 60.0);
            let x = v2(&t.sample(Some(3.0), None).unwrap())[0];
            let expect = 20.0 - 20.0 * 0.95f32.powi(k);
            assert!((x - expect).abs() < 1e-3, "frame {k}: {x} vs {expect}");
            assert!(x >= last && x <= 20.0, "monotone, no overshoot");
            last = x;
        }
        // At 144 Hz the same time gives the same residual.
        let mut a = Offset {
            params: p,
            value: [-20.0, 0.0, 0.0, 0.0],
        };
        let mut b = a.clone();
        for _ in 0..60 {
            a.decay(1.0 / 60.0);
        }
        for _ in 0..144 {
            b.decay(1.0 / 144.0);
        }
        assert!((a.value[0] - b.value[0]).abs() < 1e-6);

        // A 50 px correction is large: 0.85 per frame while above 25 px.
        let mut o = Offset {
            params: p,
            value: [50.0, 0.0, 0.0, 0.0],
        };
        o.decay(1.0 / 60.0);
        assert!((o.value[0] - 42.5).abs() < 1e-9);
        let mut o = Offset {
            params: p,
            value: [20.0, 0.0, 0.0, 0.0],
        };
        o.decay(1.0 / 60.0);
        assert!((o.value[0] - 19.0).abs() < 1e-9);

        // Above 200 px: snap, no offset.
        let mut t = track(&d);
        t.push(1, Value::Vector2([0.0, 0.0]), None, 6);
        t.push(2, Value::Vector2([0.0, 0.0]), None, 6);
        assert_eq!(
            t.push(3, Value::Vector2([500.0, 0.0]), None, 6),
            Pushed::Teleported
        );
        assert!(t.offset().is_none());
        assert_eq!(
            t.sample(Some(2.0), None),
            Some(Value::Vector2([500.0, 0.0]))
        );
    }

    #[test]
    fn owned_offsets_blend_and_teleport() {
        let mut i = Interp::new(60);
        let d = pos2();
        let k = (1, 0, 0);
        assert!(i.blend_owned(
            k,
            &d,
            &Value::Vector2([0.0, 0.0]),
            &Value::Vector2([50.0, 0.0])
        ));
        let o = i.owned_offset(k).unwrap();
        assert_eq!(
            o.apply(&Value::Vector2([50.0, 0.0])),
            Value::Vector2([0.0, 0.0])
        );
        i.frame(0);
        i.frame(16_667);
        let o = i.owned_offset(k).unwrap();
        assert!((o.value[0] + 42.5).abs() < 0.01, "{:?}", o.value);
        assert!(!i.blend_owned(
            k,
            &d,
            &Value::Vector2([0.0, 0.0]),
            &Value::Vector2([900.0, 0.0])
        ));
        assert!(i.owned_offset(k).is_none(), "a teleport snaps");
        assert_eq!(i.timeline.stats.corrections, 1);
        assert_eq!(i.timeline.stats.teleports, 1);
    }

    fn feed(tl: &mut Timeline, start_tick: u64, n: u64, delays_us: &dyn Fn(u64) -> u64) -> u64 {
        // One snapshot per tick at 60 Hz; tick k is sent at k * tick_us and arrives `delay(k)` later.
        // Render frames run every 1/144 s in between. Returns the late snapshots.
        let tick_us = 1_000_000.0 / 60.0;
        let mut events: Vec<(u64, Option<u64>)> = Vec::new();
        for k in start_tick..start_tick + n {
            let at = (k as f64 * tick_us) as u64 + delays_us(k);
            events.push((at, Some(k)));
        }
        let t0 = (start_tick as f64 * tick_us) as u64;
        let t1 = ((start_tick + n) as f64 * tick_us) as u64;
        let mut t = t0;
        while t < t1 {
            events.push((t, None));
            t += 6_944;
        }
        events.sort_by_key(|e| (e.0, e.1.is_some()));
        let before = tl.stats.late_snapshots;
        for (at, k) in events {
            match k {
                Some(k) => tl.on_snapshot(k, at),
                None => {
                    tl.advance(at);
                }
            }
        }
        tl.stats.late_snapshots - before
    }

    #[test]
    fn jitter_buffer_grows_fast_and_shrinks_slowly() {
        let mut tl = Timeline::new(60);
        assert_eq!(tl.buffer(), 2);
        // A clean link: no late snapshots, the render tick sits about two ticks behind the newest.
        let late = feed(&mut tl, 0, 120, &|_| 30_000);
        assert_eq!(late, 0);
        assert!((tl.render_lag() - 2.0).abs() < 0.1, "{}", tl.render_lag());
        // 40 ms of jitter: the buffer grows within a second or two.
        let rng = std::cell::Cell::new(Pcg(7));
        let jitter = |_k: u64| {
            let mut g = rng.get();
            let x = g.next() % 40_000;
            rng.set(g);
            30_000 + x
        };
        feed(&mut tl, 120, 120, &jitter);
        let grown = tl.buffer();
        assert!(grown >= 3, "grew to {grown}");
        assert!(tl.stats.buffer_grown >= 1);
        // And then holds the late fraction under the target.
        let late = feed(&mut tl, 240, 600, &jitter);
        assert!(
            (late as f64) < 600.0 * LATE_TARGET * 1.5,
            "{late} late of 600 with a {} tick buffer",
            tl.buffer()
        );
        assert!(tl.buffer() <= MAX_JITTER_BUFFER);
        // Back on a clean link it shrinks, one tick per 4 s at most, never below 1.
        let b = tl.buffer();
        feed(&mut tl, 840, 60 * 3, &|_| 30_000);
        assert!(tl.buffer() >= b.saturating_sub(1), "slowly");
        feed(&mut tl, 1020, 60 * 40, &|_| 30_000);
        assert_eq!(tl.buffer(), MIN_JITTER_BUFFER, "down to the minimum");
        // Not adaptive: fixed.
        let mut fixed = Timeline::new(60);
        fixed.configure(4, false);
        feed(&mut fixed, 0, 300, &jitter);
        assert_eq!(fixed.buffer(), 4);
        fixed.configure(99, false);
        assert_eq!(fixed.buffer(), MAX_JITTER_BUFFER);
    }

    #[test]
    fn render_tick_never_runs_backwards() {
        let mut tl = Timeline::new(60);
        let tick_us = 1_000_000.0 / 60.0;
        let mut last = f64::MIN;
        let mut g = Pcg(3);
        let mut k = 0u64;
        let mut t = 0u64;
        while k < 600 {
            let send = (k as f64 * tick_us) as u64;
            if t >= send + 20_000 + g.next() % 30_000 {
                tl.on_snapshot(k, t);
                k += 1;
            }
            if let Some(r) = tl.advance(t) {
                assert!(r >= last, "{r} < {last}");
                last = r;
            }
            t += 2_000;
        }
    }

    #[derive(Clone, Copy)]
    struct Pcg(u64);
    impl Pcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn interp_frames_write_only_changes() {
        let mut i = Interp::new(60);
        let d = pos2();
        let k = (9, 0, 0);
        let tick_us = 1_000_000.0 / 60.0;
        for t in 0..10u64 {
            i.timeline
                .on_snapshot(t, (t as f64 * tick_us) as u64 + 1000);
            i.push(k, &d, t, Value::Vector2([t as f32 * 2.0, 0.0]));
        }
        let now = (10.0 * tick_us) as u64;
        let a = i.frame(now);
        assert_eq!(a.len(), 1);
        let x = v2(&a[0].1)[0];
        assert!(
            x > 10.0 && x < 18.0,
            "about two ticks behind the newest: {x}"
        );
        assert!(i.frame(now).is_empty(), "nothing changed");
        i.reset(9);
        assert_eq!(i.track_count(), 0);
    }
}

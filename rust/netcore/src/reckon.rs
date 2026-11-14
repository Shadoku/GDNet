//! Body prediction: dead reckoning of frozen rigid body replicas (docs/PLAN_NETCODE.md 12.4 "Body
//! prediction", WP19, gate G13, decisions D26 and D27).
//!
//! A replica shows the server's newest snapshot of a body carried forward to the present. This module
//! is the arithmetic, sans-IO: gdnet reads the snapshot values out of the interpolation buffers
//! ([`crate::interp`]), estimates the present server tick and calls in here.
//!
//! **The step** ([`step`]) is the integrator Godot's own engines run for a body in free flight, one call
//! per physics tick: with `c` a damping coefficient (1/s) and `dt` the step,
//!
//! ```text
//! v *= max(0, 1 - c_lin * dt)          w *= max(0, 1 - c_ang * dt)      (damping)
//! v += g * gravity_scale * dt                                           (gravity)
//! x += v * dt                          rotation advanced by w * dt      (semi-implicit Euler)
//! ```
//!
//! GodotPhysics2D and GodotPhysics3D damp first and then add gravity; Jolt adds gravity and then damps
//! ([`Forces::damp_after_gravity`]). In 2D the rotation is an angle plus `w * dt`; in 3D a quaternion
//! turned by the rotation of angle `|w| * dt` about `w`, applied on the left (world frame), as the
//! engines do. A step over a fraction of a tick uses the same formulas with the shorter `dt`, so the
//! shown pose is continuous as the horizon passes a whole tick.
//!
//! **The horizon** ([`horizon`]): ticks from the snapshot's server tick to the client's estimate of
//! the server's present tick, clamped to `0..=cap` (`gdnet/physics/predict_max_ms`).
//!
//! **Incremental stepping** ([`Reckoner`]): the motion at whole ticks after the base is kept, so a
//! frame advances only the ticks that passed since the last frame plus one fractional step; a new
//! snapshot re-bases it. The stepping function is a parameter, so gdnet passes either [`step`] or a
//! call to the game's `_net_predict_body(delta)` hook.
//!
//! **Corrections** ([`BodyTrack`]): when a new snapshot arrives, the pose the old prediction shows at
//! this moment is compared with the pose the new one shows; the difference becomes a decaying error
//! offset added to what is shown (WP5's mechanism: the shown pose stays continuous, a wrong prediction
//! bends back), by `decay` per 60 Hz frame scaled by the frame time. Above the teleport distance (or
//! angle) the body snaps. A difference above the `position` / `angle` thresholds counts as a
//! correction.
//!
//! **Supported bodies** ([`supported`]): a body resting or sliding on something does not accelerate
//! with gravity on the server. When the newest velocity samples show less than half the change
//! gravity would have made between them, the prediction leaves gravity out (a sliding crate does not
//! sink into the floor). A body whose newest velocities are zero is at rest and not predicted at all
//! ([`Motion::is_moving`]).

use crate::interp::{q_angle, q_conj, q_mul, q_norm, slerp, wrap_angle, DECAY_REFERENCE_HZ};

/// Speeds below this (in the property's units per second) count as zero.
pub const REST_SPEED: f64 = 1e-4;

const IDENTITY: [f64; 4] = [0.0, 0.0, 0.0, 1.0];

/// A body's pose and velocities. 2D uses `pos[0..2]`, `rot[0]` (the angle, radians), `lin[0..2]` and
/// `ang[0]` (radians per second); 3D uses all of `pos` and `lin`, `rot` as a unit quaternion `[x, y,
/// z, w]` and `ang` as the angular velocity vector.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    pub three_d: bool,
    pub pos: [f64; 3],
    pub rot: [f64; 4],
    pub lin: [f64; 3],
    pub ang: [f64; 3],
}

impl Motion {
    pub fn new_2d(pos: [f64; 2], angle: f64, lin: [f64; 2], ang: f64) -> Self {
        Self {
            three_d: false,
            pos: [pos[0], pos[1], 0.0],
            rot: [angle, 0.0, 0.0, 0.0],
            lin: [lin[0], lin[1], 0.0],
            ang: [ang, 0.0, 0.0],
        }
    }

    pub fn new_3d(pos: [f64; 3], quat: [f64; 4], lin: [f64; 3], ang: [f64; 3]) -> Self {
        Self {
            three_d: true,
            pos,
            rot: quat,
            lin,
            ang,
        }
    }

    /// Whether any velocity component is above [`REST_SPEED`].
    pub fn is_moving(&self) -> bool {
        self.lin
            .iter()
            .chain(self.ang.iter())
            .any(|v| v.abs() > REST_SPEED)
    }
}

/// What accelerates a body in free flight: the gravity acceleration (the space's gravity times the
/// body's `gravity_scale`, in the body's units per second squared), the total damping coefficients
/// (1/s), and the engine's order of the two.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Forces {
    pub gravity: [f64; 3],
    pub linear_damp: f64,
    pub angular_damp: f64,
    /// Jolt: gravity first, then damping. Godot's own engines: damping first.
    pub damp_after_gravity: bool,
}

impl Forces {
    /// The same forces without gravity (a supported body).
    pub fn without_gravity(&self) -> Self {
        Self {
            gravity: [0.0; 3],
            ..*self
        }
    }
}

/// One physics step of `dt` seconds (see the module docs for the formulas).
pub fn step(m: &mut Motion, f: &Forces, dt: f64) {
    if dt <= 0.0 {
        return;
    }
    let ld = (1.0 - dt * f.linear_damp).max(0.0);
    let ad = (1.0 - dt * f.angular_damp).max(0.0);
    let n = if m.three_d { 3 } else { 2 };
    if !f.damp_after_gravity {
        damp(m, n, ld, ad);
    }
    for i in 0..n {
        m.lin[i] += f.gravity[i] * dt;
    }
    if f.damp_after_gravity {
        damp(m, n, ld, ad);
    }
    for i in 0..n {
        m.pos[i] += m.lin[i] * dt;
    }
    if m.three_d {
        let w = m.ang;
        let speed = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
        if speed > 1e-12 {
            let half = 0.5 * speed * dt;
            let s = half.sin() / speed;
            let turn = [w[0] * s, w[1] * s, w[2] * s, half.cos()];
            m.rot = q_norm(&q_mul(&turn, &m.rot));
        }
    } else {
        m.rot[0] = wrap_angle(m.rot[0] + m.ang[0] * dt);
    }
}

fn damp(m: &mut Motion, n: usize, ld: f64, ad: f64) {
    for v in m.lin.iter_mut().take(n) {
        *v *= ld;
    }
    let na = if m.three_d { 3 } else { 1 };
    for v in m.ang.iter_mut().take(na) {
        *v *= ad;
    }
}

/// `m` advanced `ticks` ticks of `tick_dt` seconds by [`step`]: whole ticks, then one fractional step.
pub fn advance(m: &Motion, f: &Forces, ticks: f64, tick_dt: f64) -> Motion {
    let mut r = Reckoner::new(0, *m);
    r.at(ticks, tick_dt, &mut |m, dt| step(m, f, dt))
}

/// Ticks from `base_tick` to the estimated present server tick `present`, clamped to `0..=cap_ticks`.
pub fn horizon(present: f64, base_tick: u64, cap_ticks: f64) -> f64 {
    (present - base_tick as f64).clamp(0.0, cap_ticks.max(0.0))
}

/// Where the shown pose aims inside the server's current tick: the server's bodies change once per
/// tick, so a continuous estimate is at most half a tick of travel off when it aims at the middle.
pub const PRESENT_CENTRE: f64 = 0.5;

/// The client's estimate of the server's present tick, on the client's physics clock: the leading edge
/// of the snapshot arrivals (`snapshot tick - local physics tick` at arrival: it follows early arrivals
/// quickly and late ones slowly, as the render timeline does), plus the one-way delay.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edge {
    offset: Option<f64>,
}

impl Edge {
    /// A snapshot of server tick `tick` was first seen at local physics tick `local_tick`.
    pub fn observe(&mut self, tick: u64, local_tick: u64) {
        let s = tick as f64 - local_tick as f64;
        self.offset = Some(match self.offset {
            None => s,
            Some(o) if s > o => o + 0.5 * (s - o),
            Some(o) => o + 0.005 * (s - o),
        });
    }

    /// The server's present tick at local physics time `local` (ticks, fractional) with a one-way delay
    /// of `one_way` ticks, aimed at the middle of that tick ([`PRESENT_CENTRE`]); None before the first
    /// snapshot.
    pub fn present(&self, local: f64, one_way: f64) -> Option<f64> {
        self.offset.map(|o| local + o + one_way - PRESENT_CENTRE)
    }
}

/// Whether a body was supported between two velocity samples: `prev` at tick `prev_tick` and `lin` at
/// `tick`. Free flight from `prev` over the ticks between them would change the velocity along gravity
/// by some amount; when the observed change is less than half of it, something held the body (it rests
/// or slides on a surface) and its prediction leaves gravity out. False without gravity or a gap.
pub fn supported(
    prev: [f64; 3],
    prev_tick: u64,
    lin: [f64; 3],
    tick: u64,
    f: &Forces,
    tick_dt: f64,
    three_d: bool,
) -> bool {
    let n = if three_d { 3 } else { 2 };
    let g: f64 = f.gravity[..n].iter().map(|x| x * x).sum::<f64>().sqrt();
    if g < 1e-9 || tick <= prev_tick {
        return false;
    }
    let dir: Vec<f64> = f.gravity[..n].iter().map(|x| x / g).collect();
    let mut m = Motion {
        three_d,
        pos: [0.0; 3],
        rot: IDENTITY,
        lin: prev,
        ang: [0.0; 3],
    };
    for _ in prev_tick..tick {
        step(&mut m, f, tick_dt);
    }
    let along = |v: &[f64; 3]| -> f64 { (0..n).map(|i| (v[i] - prev[i]) * dir[i]).sum() };
    let expected = along(&m.lin);
    let observed = along(&lin);
    expected > 1e-9 && observed < 0.5 * expected
}

/// The motion of one body after its base snapshot, stepped incrementally (see the module docs).
#[derive(Clone, Debug)]
pub struct Reckoner {
    base_tick: u64,
    base: Motion,
    cur: Motion,
    whole: u32,
    calls: u64,
}

impl Reckoner {
    pub fn new(base_tick: u64, base: Motion) -> Self {
        Self {
            base_tick,
            base,
            cur: base,
            whole: 0,
            calls: 0,
        }
    }

    pub fn base_tick(&self) -> u64 {
        self.base_tick
    }

    pub fn base(&self) -> &Motion {
        &self.base
    }

    /// Calls of the stepping function so far.
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// The motion `h` ticks (fractional, at least 0) after the base: the kept whole ticks are advanced
    /// to `floor(h)` by calls of `step(motion, tick_dt)` (from the base again when `h` went back below
    /// them), then a copy is advanced by one call of `step(copy, fraction * tick_dt)`.
    pub fn at(&mut self, h: f64, tick_dt: f64, step: &mut dyn FnMut(&mut Motion, f64)) -> Motion {
        let h = h.max(0.0);
        let whole = h.floor() as u32;
        if whole < self.whole {
            self.cur = self.base;
            self.whole = 0;
        }
        while self.whole < whole {
            step(&mut self.cur, tick_dt);
            self.whole += 1;
            self.calls += 1;
        }
        let frac = h - f64::from(whole);
        let mut out = self.cur;
        if frac > 1e-9 {
            step(&mut out, frac * tick_dt);
            self.calls += 1;
        }
        out
    }
}

/// Correction settings: thresholds above which a disagreement counts as a correction (`position` in
/// the body's units, `angle` in radians), the teleport distance and angle above which the body snaps,
/// and the offset's decay per 60 Hz frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub position: f64,
    pub angle: f64,
    pub teleport: f64,
    pub teleport_angle: f64,
    pub decay: f64,
}

/// A decaying error offset on the shown pose: a translation, and an angle (2D, `rot[0]`) or a rotation
/// quaternion (3D) applied on the left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Offset {
    pub pos: [f64; 3],
    pub rot: [f64; 4],
}

impl Offset {
    /// The offset that turns `to` into `from` (`from = to + offset`).
    pub fn between(from: &Motion, to: &Motion) -> Self {
        let pos = [
            from.pos[0] - to.pos[0],
            from.pos[1] - to.pos[1],
            from.pos[2] - to.pos[2],
        ];
        let rot = if from.three_d {
            let mut q = q_norm(&q_mul(&q_norm(&from.rot), &q_conj(&q_norm(&to.rot))));
            if q[3] < 0.0 {
                q = [-q[0], -q[1], -q[2], -q[3]];
            }
            q
        } else {
            [wrap_angle(from.rot[0] - to.rot[0]), 0.0, 0.0, 0.0]
        };
        Self { pos, rot }
    }

    pub fn distance(&self) -> f64 {
        self.pos.iter().map(|x| x * x).sum::<f64>().sqrt()
    }

    /// The rotation's angle (radians, 0..PI).
    pub fn angle(&self, three_d: bool) -> f64 {
        if three_d {
            q_angle(&self.rot)
        } else {
            self.rot[0].abs()
        }
    }

    /// `m` with the offset added to its pose (velocities unchanged).
    pub fn apply(&self, m: &Motion) -> Motion {
        let mut out = *m;
        for i in 0..3 {
            out.pos[i] += self.pos[i];
        }
        if m.three_d {
            out.rot = q_norm(&q_mul(&self.rot, &m.rot));
        } else {
            out.rot[0] = wrap_angle(m.rot[0] + self.rot[0]);
        }
        out
    }

    /// One frame of decay over `dt` seconds by `decay` per 60 Hz frame; false once it is negligible.
    pub fn decay(&mut self, dt: f64, decay: f64, three_d: bool, th: &Thresholds) -> bool {
        let f = decay.clamp(0.0, 1.0).powf(dt.max(0.0) * DECAY_REFERENCE_HZ);
        self.pos.iter_mut().for_each(|x| *x *= f);
        if three_d {
            self.rot = slerp(&IDENTITY, &self.rot, f);
        } else {
            self.rot[0] *= f;
        }
        self.distance() > th.position * 0.001 || self.angle(three_d) > th.angle * 0.001
    }
}

/// What a re-base did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebased {
    /// The new prediction agreed with the old one within the thresholds (an offset may still smooth
    /// the small difference).
    Agreed,
    /// It disagreed by more than a threshold: a correction, shown as a decaying offset.
    Corrected,
    /// It disagreed by more than the teleport distance or angle: the body snaps.
    Snapped,
}

/// One predicted body: its [`Reckoner`], forces and decaying offset.
#[derive(Clone, Debug)]
pub struct BodyTrack {
    reck: Reckoner,
    forces: Forces,
    offset: Option<Offset>,
}

impl BodyTrack {
    /// Starts predicting from the snapshot `base` of `tick`; `shown` is what the body shows now (the
    /// interpolated pose), which the first prediction blends from like a correction (none when the two
    /// are farther apart than the teleport thresholds). `h` is the horizon now.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        tick: u64,
        base: Motion,
        forces: Forces,
        shown: Option<&Motion>,
        h: f64,
        tick_dt: f64,
        th: &Thresholds,
        step: &mut dyn FnMut(&mut Motion, &Forces, f64),
    ) -> (Self, Rebased) {
        let mut t = Self {
            reck: Reckoner::new(tick, base),
            forces,
            offset: None,
        };
        let r = match shown {
            Some(s) => {
                let raw = t.raw(h, tick_dt, step);
                t.blend_from(s, &raw, th)
            }
            None => Rebased::Agreed,
        };
        (t, r)
    }

    pub fn base_tick(&self) -> u64 {
        self.reck.base_tick()
    }

    pub fn base(&self) -> &Motion {
        self.reck.base()
    }

    pub fn forces(&self) -> &Forces {
        &self.forces
    }

    pub fn offset(&self) -> Option<&Offset> {
        self.offset.as_ref()
    }

    /// Calls of the stepping function since the last (re-)base.
    pub fn calls(&self) -> u64 {
        self.reck.calls()
    }

    /// The raw prediction `h` ticks after the base (no offset). A body at rest is not stepped.
    pub fn raw(
        &mut self,
        h: f64,
        tick_dt: f64,
        step: &mut dyn FnMut(&mut Motion, &Forces, f64),
    ) -> Motion {
        if !self.reck.base().is_moving() {
            return *self.reck.base();
        }
        let f = self.forces;
        self.reck.at(h, tick_dt, &mut |m, dt| step(m, &f, dt))
    }

    /// What is shown `h` ticks after the base: the raw prediction plus the offset.
    pub fn shown(
        &mut self,
        h: f64,
        tick_dt: f64,
        step: &mut dyn FnMut(&mut Motion, &Forces, f64),
    ) -> (Motion, Motion) {
        let raw = self.raw(h, tick_dt, step);
        let shown = self.offset.as_ref().map_or(raw, |o| o.apply(&raw));
        (raw, shown)
    }

    /// A newer snapshot: `h_old` is the horizon now measured from the old base, `h_new` from the new
    /// one. What the old prediction shows now and what the new one shows are compared; the difference
    /// becomes the offset.
    #[allow(clippy::too_many_arguments)]
    pub fn rebase(
        &mut self,
        tick: u64,
        base: Motion,
        forces: Forces,
        h_old: f64,
        h_new: f64,
        tick_dt: f64,
        th: &Thresholds,
        step: &mut dyn FnMut(&mut Motion, &Forces, f64),
    ) -> Rebased {
        let (_, before) = self.shown(h_old, tick_dt, step);
        self.reck = Reckoner::new(tick, base);
        self.forces = forces;
        let raw = self.raw(h_new, tick_dt, step);
        self.blend_from(&before, &raw, th)
    }

    fn blend_from(&mut self, before: &Motion, raw: &Motion, th: &Thresholds) -> Rebased {
        let o = Offset::between(before, raw);
        let three_d = raw.three_d;
        if o.distance() > th.teleport || o.angle(three_d) > th.teleport_angle {
            self.offset = None;
            return Rebased::Snapped;
        }
        let corrected = o.distance() > th.position || o.angle(three_d) > th.angle;
        self.offset = Some(o);
        if corrected {
            Rebased::Corrected
        } else {
            Rebased::Agreed
        }
    }

    /// Decays the offset over `dt` seconds.
    pub fn decay(&mut self, dt: f64, th: &Thresholds) {
        let three_d = self.reck.base().three_d;
        if let Some(o) = self.offset.as_mut() {
            if !o.decay(dt, th.decay, three_d, th) {
                self.offset = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn th() -> Thresholds {
        Thresholds {
            position: 5.0,
            angle: 5f64.to_radians(),
            teleport: 200.0,
            teleport_angle: 90f64.to_radians(),
            decay: 0.85,
        }
    }

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn velocity_alone_moves_the_position_and_the_angle() {
        let m = Motion::new_2d([10.0, 20.0], 0.5, [120.0, -60.0], 2.0);
        let f = Forces::default();
        let r = advance(&m, &f, 6.0, DT);
        assert!(close(r.pos[0], 10.0 + 120.0 * 0.1, 1e-9), "{r:?}");
        assert!(close(r.pos[1], 20.0 - 60.0 * 0.1, 1e-9), "{r:?}");
        assert!(close(r.rot[0], 0.5 + 0.2, 1e-9), "{r:?}");
        assert_eq!(r.lin, m.lin, "no forces: the velocity is unchanged");
        // A fractional horizon is a shorter step: 2.5 ticks = 2.5 ticks of travel.
        let r = advance(&m, &f, 2.5, DT);
        assert!(close(r.pos[0], 10.0 + 120.0 * 2.5 * DT, 1e-9), "{r:?}");
    }

    #[test]
    fn gravity_matches_semi_implicit_euler_per_tick() {
        // 2D: 980 px/s^2 down. After n ticks: v = v0 + g n dt, x = x0 + v0 n dt + g dt^2 n (n + 1) / 2.
        let f = Forces {
            gravity: [0.0, 980.0, 0.0],
            ..Default::default()
        };
        let m = Motion::new_2d([0.0, 0.0], 0.0, [300.0, -400.0], 0.0);
        let n = 9.0;
        let r = advance(&m, &f, n, DT);
        assert!(close(r.lin[1], -400.0 + 980.0 * n * DT, 1e-9), "{r:?}");
        let y = -400.0 * n * DT + 980.0 * DT * DT * n * (n + 1.0) / 2.0;
        assert!(close(r.pos[1], y, 1e-9), "{} vs {y}", r.pos[1]);
        assert!(close(r.pos[0], 300.0 * n * DT, 1e-9));
        // Continuous across a whole tick: 3.999 and 4.0 ticks are a hair apart.
        let a = advance(&m, &f, 3.999_999, DT);
        let b = advance(&m, &f, 4.0, DT);
        assert!(close(a.pos[1], b.pos[1], 1e-3), "{a:?} {b:?}");
    }

    #[test]
    fn damping_follows_the_engines_order() {
        let base = Motion::new_3d([0.0; 3], IDENTITY, [10.0, 0.0, 0.0], [0.0, 0.0, 2.0]);
        let f = Forces {
            gravity: [0.0, -9.8, 0.0],
            linear_damp: 0.5,
            angular_damp: 1.0,
            damp_after_gravity: false,
        };
        let mut m = base;
        step(&mut m, &f, DT);
        let d = 1.0 - 0.5 * DT;
        assert!(close(m.lin[0], 10.0 * d, 1e-12), "{m:?}");
        assert!(
            close(m.lin[1], -9.8 * DT, 1e-12),
            "Godot: damped first, gravity added undamped"
        );
        assert!(close(m.ang[2], 2.0 * (1.0 - DT), 1e-12));
        let mut j = base;
        step(
            &mut j,
            &Forces {
                damp_after_gravity: true,
                ..f
            },
            DT,
        );
        assert!(
            close(j.lin[1], -9.8 * DT * d, 1e-12),
            "Jolt: gravity, then damping"
        );
        // Damping never reverses a velocity.
        let mut k = base;
        step(
            &mut k,
            &Forces {
                linear_damp: 1000.0,
                ..f
            },
            DT,
        );
        assert!(k.lin[0] == 0.0, "{k:?}");
    }

    #[test]
    fn a_3d_rotation_turns_by_the_angular_velocity() {
        // 90 degrees per second about y for 60 ticks: a quarter turn.
        let w = std::f64::consts::FRAC_PI_2;
        let m = Motion::new_3d([0.0; 3], IDENTITY, [0.0; 3], [0.0, w, 0.0]);
        let r = advance(&m, &Forces::default(), 60.0, DT);
        let s = std::f64::consts::FRAC_PI_4;
        assert!(close(r.rot[1], s.sin(), 1e-9), "{r:?}");
        assert!(close(r.rot[3], s.cos(), 1e-9), "{r:?}");
        // The turn is in the world frame (left multiplication): a body already turned about z keeps
        // that turn under the new one.
        let qz = [0.0, 0.0, s.sin(), s.cos()];
        let m = Motion::new_3d([0.0; 3], qz, [0.0; 3], [0.0, w, 0.0]);
        let r = advance(&m, &Forces::default(), 60.0, DT);
        let want = q_mul(&[0.0, s.sin(), 0.0, s.cos()], &qz);
        for i in 0..4 {
            assert!(close(r.rot[i], want[i], 1e-9), "{r:?} vs {want:?}");
        }
        // 3D gravity and velocity on all three axes.
        let f = Forces {
            gravity: [0.0, -9.8, 0.0],
            ..Default::default()
        };
        let m = Motion::new_3d([1.0, 2.0, 3.0], IDENTITY, [1.0, 2.0, -3.0], [0.0; 3]);
        let r = advance(&m, &f, 6.0, DT);
        assert!(
            close(r.pos[0], 1.1, 1e-9) && close(r.pos[2], 2.7, 1e-9),
            "{r:?}"
        );
        assert!(close(r.lin[1], 2.0 - 9.8 * 0.1, 1e-9), "{r:?}");
    }

    #[test]
    fn the_horizon_is_capped_and_never_negative() {
        assert!(close(horizon(110.5, 104, 15.0), 6.5, 1e-12));
        assert_eq!(horizon(100.0, 104, 15.0), 0.0, "a snapshot from the future");
        assert_eq!(horizon(150.0, 104, 15.0), 15.0, "250 ms at 60 Hz");
    }

    #[test]
    fn the_edge_follows_early_arrivals_and_estimates_the_present() {
        let mut e = Edge::default();
        assert_eq!(e.present(10.0, 3.0), None);
        // Snapshots take 3 or 4 local ticks to arrive (the server's counter is 100 ahead).
        for k in 0..40u64 {
            let transit = if k % 3 == 0 { 3 } else { 4 };
            e.observe(100 + k, k + transit);
        }
        // The server is at local + 100 ticks (150.25 at local 50.25); aimed at the middle of its tick.
        let p = e.present(50.25, 3.0).unwrap();
        assert!((p - 149.75).abs() < 0.1, "{p}");
    }

    #[test]
    fn the_reckoner_steps_whole_ticks_once() {
        let m = Motion::new_2d([0.0, 0.0], 0.0, [60.0, 0.0], 0.0);
        let mut r = Reckoner::new(10, m);
        let f = Forces::default();
        let calls = std::cell::Cell::new(0);
        let mut st = |m: &mut Motion, dt: f64| {
            calls.set(calls.get() + 1);
            step(m, &f, dt)
        };
        let a = r.at(2.5, DT, &mut st);
        assert!(close(a.pos[0], 2.5, 1e-9), "{a:?}");
        let b = r.at(2.75, DT, &mut st);
        assert!(close(b.pos[0], 2.75, 1e-9), "{b:?}");
        let c = r.at(4.0, DT, &mut st);
        assert!(close(c.pos[0], 4.0, 1e-9), "{c:?}");
        // 2 whole + 1 fraction, 1 fraction, 2 whole: frames cost the ticks that passed plus one.
        assert_eq!(calls.get(), 6);
        // Going back below the kept ticks starts again from the base.
        let d = r.at(1.0, DT, &mut st);
        assert!(close(d.pos[0], 1.0, 1e-9), "{d:?}");
    }

    #[test]
    fn a_supported_body_leaves_gravity_out() {
        let f = Forces {
            gravity: [0.0, 980.0, 0.0],
            linear_damp: 0.1,
            ..Default::default()
        };
        // Free fall: the velocity grew by g dt per tick.
        let prev = [100.0, 50.0, 0.0];
        let mut m = Motion::new_2d([0.0, 0.0], 0.0, [100.0, 50.0], 0.0);
        step(&mut m, &f, DT);
        step(&mut m, &f, DT);
        assert!(!supported(prev, 10, m.lin, 12, &f, DT, false));
        // Sliding on a floor: the vertical velocity stays 0.
        assert!(supported(
            [100.0, 0.0, 0.0],
            10,
            [99.0, 0.0, 0.0],
            11,
            &f,
            DT,
            false
        ));
        // A bounce (the velocity turned upwards) counts as supported for that snapshot.
        assert!(supported(
            [0.0, 400.0, 0.0],
            10,
            [0.0, -200.0, 0.0],
            11,
            &f,
            DT,
            false
        ));
        // No gravity, or no gap: never.
        assert!(!supported(
            prev,
            10,
            prev,
            11,
            &Forces::default(),
            DT,
            false
        ));
        assert!(!supported(prev, 10, prev, 10, &f, DT, false));
        // 3D: gravity along -y.
        let g3 = Forces {
            gravity: [0.0, -9.8, 0.0],
            ..Default::default()
        };
        assert!(supported(
            [1.0, 0.0, 0.0],
            5,
            [1.0, 0.0, 0.0],
            6,
            &g3,
            DT,
            true
        ));
        assert!(!supported(
            [1.0, 0.0, 0.0],
            5,
            [1.0, -9.8 * DT, 0.0],
            6,
            &g3,
            DT,
            true
        ));
    }

    fn builtin() -> impl FnMut(&mut Motion, &Forces, f64) {
        |m: &mut Motion, f: &Forces, dt: f64| step(m, f, dt)
    }

    #[test]
    fn a_right_prediction_needs_no_correction_and_a_wrong_one_bends_back() {
        let f = Forces {
            gravity: [0.0, 980.0, 0.0],
            ..Default::default()
        };
        let mut st = builtin();
        let base = Motion::new_2d([0.0, 0.0], 0.0, [300.0, -400.0], 0.0);
        let (mut t, r) = BodyTrack::start(100, base, f, None, 6.0, DT, &th(), &mut st);
        assert_eq!(r, Rebased::Agreed);
        // The server followed the same physics: the snapshot of tick 101 is the base stepped once.
        let mut next = base;
        step(&mut next, &f, DT);
        let r = t.rebase(101, next, f, 6.5, 5.5, DT, &th(), &mut st);
        assert_eq!(r, Rebased::Agreed);
        assert!(
            t.offset().map_or(0.0, |o| o.distance()) < 1e-6,
            "{:?}",
            t.offset()
        );
        // A bounce the prediction did not know: the snapshot's velocity turned. The shown pose stays
        // where it was (the offset cancels the difference) and moves towards the new prediction.
        let mut bounced = next;
        step(&mut bounced, &f, DT);
        bounced.lin[1] = -bounced.lin[1].abs() - 300.0;
        let (_, before) = t.shown(6.5, DT, &mut st);
        let r = t.rebase(102, bounced, f, 6.5, 5.5, DT, &th(), &mut st);
        assert_eq!(r, Rebased::Corrected);
        let (raw, shown) = t.shown(5.5, DT, &mut st);
        assert!(
            close(shown.pos[1], before.pos[1], 1e-6),
            "continuous: {shown:?} {before:?}"
        );
        let err0 = (shown.pos[1] - raw.pos[1]).abs();
        for _ in 0..30 {
            t.decay(DT, &th());
        }
        let (raw, shown) = t.shown(5.5, DT, &mut st);
        let err1 = (shown.pos[1] - raw.pos[1]).abs();
        assert!(err1 < err0 * 0.85f64.powi(29), "{err0} -> {err1}");
        // Far off: a snap.
        let mut far = bounced;
        far.pos[0] += 500.0;
        let r = t.rebase(103, far, f, 6.5, 5.5, DT, &th(), &mut st);
        assert_eq!(r, Rebased::Snapped);
        assert!(t.offset().is_none());
    }

    #[test]
    fn decay_is_frame_rate_independent_and_works_in_3d() {
        let s = 0.3f64;
        let mut a = Offset {
            pos: [10.0, 0.0, 0.0],
            rot: [0.0, s.sin(), 0.0, s.cos()],
        };
        let mut b = a;
        for _ in 0..60 {
            a.decay(1.0 / 60.0, 0.85, true, &th());
        }
        for _ in 0..144 {
            b.decay(1.0 / 144.0, 0.85, true, &th());
        }
        assert!(close(a.pos[0], b.pos[0], 1e-9), "{a:?} {b:?}");
        assert!(close(a.angle(true), b.angle(true), 1e-9));
        assert!(close(a.angle(true), 0.6 * 0.85f64.powi(60), 1e-9));
        // A 3D offset applies on the left and between() inverts apply().
        let m = Motion::new_3d(
            [1.0, 2.0, 3.0],
            [0.0, 0.0, 0.2f64.sin(), 0.2f64.cos()],
            [0.0; 3],
            [0.0; 3],
        );
        let o = Offset {
            pos: [0.5, 0.0, 0.0],
            rot: [s.sin(), 0.0, 0.0, s.cos()],
        };
        let shown = o.apply(&m);
        let back = Offset::between(&shown, &m);
        for i in 0..4 {
            assert!(close(back.rot[i], o.rot[i], 1e-9), "{back:?}");
        }
        assert!(close(back.pos[0], 0.5, 1e-12));
    }

    #[test]
    fn a_body_at_rest_is_not_stepped() {
        let f = Forces {
            gravity: [0.0, 980.0, 0.0],
            ..Default::default()
        };
        let mut calls = 0;
        let mut st = |m: &mut Motion, f: &Forces, dt: f64| {
            calls += 1;
            step(m, f, dt)
        };
        let base = Motion::new_2d([5.0, 7.0], 0.1, [0.0, 0.0], 0.0);
        let (mut t, _) = BodyTrack::start(10, base, f, None, 6.0, DT, &th(), &mut st);
        let (raw, _) = t.shown(6.0, DT, &mut st);
        assert_eq!(raw, base);
        assert_eq!(calls, 0);
    }
}

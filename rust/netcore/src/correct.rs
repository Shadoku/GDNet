//! Physics modes and the corrections of `simulated` bodies (docs/PLAN_NETCODE.md 12.4, WP19).
//!
//! [`PhysicsMode`] says who simulates a body entity. It travels in the entity's `Create` and in
//! `EntityAction::SetPhysics` (a change on a running entity), beside its `client_owned` flag: `owner` is
//! the `client_owned` mechanism for bodies (D21), so the server sends the effective flag with the mode.
//!
//! A `simulated` body runs in every peer's physics; the server's snapshots are the reference. A client
//! keeps one [`Corrector`] per such body. Its local body lives in the client's *predicted* timeline
//! (the tick its own predicted player simulates, `local tick + lead`), so a predicted player and the
//! bodies it touches are at the same moment. Every tick the client records the body's pose under the
//! predicted tick. A snapshot of server tick `T` is compared with the pose recorded for `T`: both are
//! carried forward to the present at their velocities, and the position nudges applied since `T` are
//! taken off. Comparing like with like means that motion the local body has and the server's will have
//! too (a predicted player's push, a platform's swing) is never corrected, whatever the latency.
//!
//! What the comparison is not trusted with, it skips (a later sample compares again):
//!
//! - a sample not at least two ticks younger than the last correction (the poses recorded before it do
//!   not show what that
//!   correction's new velocity did to the motion since, against friction or a contact);
//! - a span with a contact: a server sample whose velocity changed faster than `max_accel` since the
//!   one before, or a recorded local tick that did (carrying a pose forward across an impact is wrong).
//!
//! **Settling.** Snapshots only carry changes, so a body the server holds still sends nothing. Once the
//! newest sample says the server's body rests (slower than `rest_speed`) and nothing newer came for
//! [`SETTLE_QUIET_S`] (the newest known server tick is that far past it), a local body that rests too
//! (and rested at that server tick in its own recorded history, not moved since) is *settled*: frozen, carried to the server's pose in
//! a few ticks ([`Step::Hold`]), then put there exactly, unfrozen with no velocity and asleep
//! ([`Corrector::finish_settle`], which gdnet calls for every held body of a manager together, once none of them is
//! still on its way, so a pile ends in the server's own resting arrangement instead of bodies pushing
//! each other off it one at a time). A body already settled onto that sample is left alone; one that
//! was knocked off it again settles again, at most once per [`SETTLE_RETRY_S`]. A newer sample that
//! moves ends a settle at once ([`Step::Release`]). Correcting a resting body toward the server's pose
//! with offsets instead never ends: each nudge wakes it, its neighbours push it back, and a stacked pile
//! is corrected forever.
//!
//! The outcome, for the part of the error the correction under way does not already cover:
//!
//! - beyond `teleport`: the pose and velocities snap to the server's, carried to the present;
//! - above `position` or `angle`: a new correction (counted, the body woken): the position and rotation
//!   error become a pending offset applied a fraction `1 - decay` per 60 Hz tick (scaled to the tick
//!   rate), the velocity errors are applied at once;
//! - below them: a moving body is refined quietly (not counted, not woken), so a body that runs
//!   alongside the server's keeps to it more closely than the thresholds, which a predicted player
//!   riding it needs; a resting one waits for its settle.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// `NetEntity.physics` (12.4): who simulates an entity's RigidBody parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PhysicsMode {
    /// The server simulates; every other peer has the body frozen and interpolated (v1, D19).
    #[default]
    Server,
    /// The entity's owner simulates; every other peer, the server included, has it frozen and
    /// interpolated (the `client_owned` mechanism, D21).
    Owner,
    /// Whoever interacts with it simulates it: a client that pushes it holds it (as its owner) until it
    /// rests, then the server has it again (handoff.rs).
    Handoff,
    /// Every peer simulates locally; the server's snapshots correct the replicas.
    Simulated,
}

impl PhysicsMode {
    /// 0 server, 1 owner, 2 handoff, 3 simulated (anything else: server).
    pub fn from_index(i: i64) -> Self {
        match i {
            1 => PhysicsMode::Owner,
            2 => PhysicsMode::Handoff,
            3 => PhysicsMode::Simulated,
            _ => PhysicsMode::Server,
        }
    }

    pub fn index(self) -> i64 {
        match self {
            PhysicsMode::Server => 0,
            PhysicsMode::Owner => 1,
            PhysicsMode::Handoff => 2,
            PhysicsMode::Simulated => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PhysicsMode::Server => "server",
            PhysicsMode::Owner => "owner",
            PhysicsMode::Handoff => "handoff",
            PhysicsMode::Simulated => "simulated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "server" => PhysicsMode::Server,
            "owner" => PhysicsMode::Owner,
            "handoff" => PhysicsMode::Handoff,
            "simulated" => PhysicsMode::Simulated,
            _ => return None,
        })
    }
}

pub type V3 = [f64; 3];
/// A unit quaternion (x, y, z, w).
pub type Quat = [f64; 4];

pub const IDENTITY: Quat = [0.0, 0.0, 0.0, 1.0];

/// A body's pose and velocities in its parent's frame: position, rotation, linear velocity, angular
/// velocity (a rotation vector per second). 2D bodies use z = 0 and rotations about z.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub pos: V3,
    pub rot: Quat,
    pub lin: V3,
    pub ang: V3,
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            pos: [0.0; 3],
            rot: IDENTITY,
            lin: [0.0; 3],
            ang: [0.0; 3],
        }
    }
}

/// One replicated component of a body's pose (what a snapshot field carries).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Component {
    Pos(V3),
    Rot(Quat),
    Lin(V3),
    Ang(V3),
}

/// The thresholds and decay of corrections, in the body's units (pixels in 2D, metres in 3D).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// A position error above this is corrected.
    pub position: f64,
    /// A rotation error above this (radians) is corrected.
    pub angle: f64,
    /// The fraction of a pending offset left after one 60 Hz tick (0 = applied at once).
    pub decay: f64,
    /// A position error beyond this snaps.
    pub teleport: f64,
    /// A sample slower than this (units per second) and turning slower than `rest_angular` (radians
    /// per second) says the server's body rests.
    pub rest_speed: f64,
    pub rest_angular: f64,
    /// A velocity that changes faster than this between two ticks (units per second squared, and
    /// radians per second squared for turning) marks a contact: such a span is not compared.
    pub max_accel: f64,
    pub max_angular_accel: f64,
}

/// Defaults in metres (2D multiplies lengths by 100 pixels per metre).
pub const DEFAULT_POSITION: f64 = 0.02;
pub const DEFAULT_ANGLE: f64 = 3.0 * std::f64::consts::PI / 180.0;
pub const DEFAULT_DECAY: f64 = 0.85;
pub const DEFAULT_TELEPORT: f64 = 2.0;
pub const DEFAULT_REST_SPEED: f64 = 0.15;
pub const DEFAULT_REST_ANGULAR: f64 = 0.25;
pub const DEFAULT_MAX_ACCEL: f64 = 20.0;
pub const DEFAULT_MAX_ANGULAR_ACCEL: f64 = 40.0;

impl Tuning {
    /// The defaults with lengths scaled by `scale` (1 in 3D, 100 in 2D).
    pub fn defaults(scale: f64) -> Self {
        Self {
            position: DEFAULT_POSITION * scale,
            angle: DEFAULT_ANGLE,
            decay: DEFAULT_DECAY,
            teleport: DEFAULT_TELEPORT * scale,
            rest_speed: DEFAULT_REST_SPEED * scale,
            rest_angular: DEFAULT_REST_ANGULAR,
            max_accel: DEFAULT_MAX_ACCEL * scale,
            max_angular_accel: DEFAULT_MAX_ANGULAR_ACCEL,
        }
    }
}

/// What a body gets this tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    None,
    /// Move by `dpos`, turn by the rotation vector `drot` (applied before the current rotation), add
    /// `dlin` and `dang` to the velocities; `wake` when a new correction started.
    Nudge {
        dpos: V3,
        drot: V3,
        dlin: V3,
        dang: V3,
        wake: bool,
    },
    /// Set the pose and velocities.
    Snap(Pose),
    /// Settling: the body is frozen (from now on, if it is not yet) and put at this pose.
    Hold(Pose),
    /// A settle ended early (the server's body moves again): the body is unfrozen where it is, with
    /// these velocities (the server's), awake.
    Release {
        lin: V3,
        ang: V3,
    },
}

/// A settle starts once the server's resting body sent nothing newer for this long (seconds).
pub const SETTLE_QUIET_S: f64 = 0.15;
/// A body knocked off the pose it settled onto settles again at most this often (seconds).
pub const SETTLE_RETRY_S: f64 = 0.5;
/// The most ticks a settle's carry takes (at 60 Hz; an error of one threshold takes three).
pub const SETTLE_MAX_TICKS: f64 = 15.0;
/// A body further than this fraction of the thresholds from the pose it settled onto settles again.
pub const SETTLE_SLACK: f64 = 0.05;

/// A settle under way.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Settling {
    from: Pose,
    to: Pose,
    /// The server tick of the sample it settles onto.
    tick: u64,
    k: u32,
    n: u32,
    /// Ticks spent at `to`, waiting for the manager's other settles.
    waited: u32,
}

/// Counters (`NetSession.stats()`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CorrectStats {
    /// Corrections started (an error above the thresholds).
    pub corrections: u64,
    pub snaps: u64,
    /// Settles finished (the body put at the server's resting pose, asleep).
    pub settles: u64,
}

/// The corrections applied so far, summed (position, rotation vector, velocities).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Applied {
    pos: V3,
    rot: V3,
    lin: V3,
    ang: V3,
}

/// The recorded poses kept per body (ticks); a sample older than this is compared with the present.
pub const HISTORY_TICKS: usize = 128;

/// A pose with the accelerations it was undergoing (per second squared), estimated from the tick
/// before.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Moving {
    pose: Pose,
    acc: V3,
    alpha: V3,
}

/// Where `p` is after `tau` seconds at its velocities. (Carrying accelerations forward was measured
/// and made things worse: a resting body's contact jitter, a tumbling box's contact torques and a
/// spring's changing pull all extrapolate badly over a round trip.)
fn carry(p: &Pose, tau: f64) -> Pose {
    Pose {
        pos: add(&p.pos, &scale(&p.lin, tau)),
        rot: q_norm(&q_mul(&rot_exp(&scale(&p.ang, tau)), &p.rot)),
        ..*p
    }
}

/// One `simulated` replica's correction state (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Corrector {
    history: VecDeque<(u64, Moving, Applied)>,
    applied: Applied,
    pending_pos: V3,
    pending_rot: V3,
    /// The server's newest known state and its tick, and the one before (for its accelerations).
    target: Option<(u64, Pose)>,
    before: Option<(u64, Pose)>,
    /// A sample arrived since the last step.
    fresh: bool,
    /// The predicted tick the last correction started at.
    epoch: Option<u64>,
    settle: Option<Settling>,
    /// The sample tick of the last finished settle and the predicted tick it finished at.
    settled: Option<(u64, u64)>,
    /// Set by gdnet: a predicted player of this client touched the body lately (its push is in the
    /// server's future), so it does not settle, and a settle under way ends.
    pub settle_blocked: bool,
    pub stats: CorrectStats,
}

impl Corrector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The server's state when the replica was created (its `Create`).
    pub fn seed(&mut self, pose: Pose) {
        self.target = Some((0, pose));
    }

    /// One snapshot field of server tick `tick`. Fields of older ticks than the newest are dropped.
    pub fn sample(&mut self, tick: u64, c: Component) {
        let (t, mut pose) = self.target.unwrap_or((0, Pose::default()));
        if tick < t {
            return;
        }
        if tick > t && self.target.is_some() {
            self.before = self.target;
        }
        match c {
            Component::Pos(v) => pose.pos = v,
            Component::Rot(q) => pose.rot = q_norm(&q),
            Component::Lin(v) => pose.lin = v,
            Component::Ang(v) => pose.ang = v,
        }
        self.target = Some((tick, pose));
        self.fresh = true;
    }

    /// The server's newest known state.
    pub fn target(&self) -> Option<(u64, Pose)> {
        self.target
    }

    /// The local body's pose at predicted tick `tick` (after this tick's correction); `dt` is the
    /// tick's length in seconds.
    pub fn record(&mut self, tick: u64, pose: Pose, dt: f64) {
        while self.history.back().is_some_and(|(t, _, _)| *t >= tick) {
            self.history.pop_back();
        }
        // The change of velocity over the tick, less the velocity corrections applied in it.
        let (acc, alpha) = match self.history.back() {
            Some((t, m, a)) if *t + 1 == tick && dt > 0.0 => (
                scale(
                    &sub(
                        &sub(&pose.lin, &m.pose.lin),
                        &sub(&self.applied.lin, &a.lin),
                    ),
                    1.0 / dt,
                ),
                scale(
                    &sub(
                        &sub(&pose.ang, &m.pose.ang),
                        &sub(&self.applied.ang, &a.ang),
                    ),
                    1.0 / dt,
                ),
            ),
            _ => ([0.0; 3], [0.0; 3]),
        };
        self.history
            .push_back((tick, Moving { pose, acc, alpha }, self.applied));
        while self.history.len() > HISTORY_TICKS {
            self.history.pop_front();
        }
    }

    fn recorded(&self, tick: u64) -> Option<&(u64, Moving, Applied)> {
        let first = self.history.front()?.0;
        let last = self.history.back()?.0;
        if tick < first || tick > last {
            return None;
        }
        // Ticks are increasing with occasional gaps (the lead grew): search from the estimate.
        let guess = ((tick - first) as usize).min(self.history.len() - 1);
        let mut i = guess;
        loop {
            let t = self.history[i].0;
            if t == tick {
                return Some(&self.history[i]);
            }
            if t > tick {
                if i == 0 {
                    return None;
                }
                i -= 1;
            } else {
                return None;
            }
        }
    }

    /// The pose recorded for predicted tick `tick`, if the history holds it.
    pub fn recorded_pose(&self, tick: u64) -> Option<Pose> {
        self.recorded(tick).map(|(_, m, _)| m.pose)
    }

    /// WP19 (handoff): the pose recorded for `tick`, else the newest one before it (a gap: the lead
    /// grew), else the oldest one (a tick before the history began: a body this side simulates since
    /// a claim was a frozen replica then, where its first recorded pose has it).
    pub fn recorded_pose_or_nearest(&self, tick: u64) -> Option<Pose> {
        self.history
            .iter()
            .rev()
            .find(|(t, _, _)| *t <= tick)
            .or_else(|| self.history.front())
            .map(|(_, m, _)| m.pose)
    }

    /// Whether a pending offset is still being applied, or a settle is under way.
    pub fn is_correcting(&self) -> bool {
        norm(&self.pending_pos) > 0.0 || norm(&self.pending_rot) > 0.0 || self.settle.is_some()
    }

    /// Whether a settle is carrying the body to the server's pose (not yet waiting at it).
    pub fn is_carrying(&self) -> bool {
        self.settle.is_some_and(|s| s.k < s.n)
    }

    /// Whether a settle has the body at the server's pose, frozen, waiting to finish.
    pub fn is_holding(&self) -> bool {
        self.settle.is_some_and(|s| s.k >= s.n)
    }

    /// Ticks a held body waited at its pose for the manager's other settles.
    pub fn held_ticks(&self) -> u32 {
        self.settle.map_or(0, |s| s.waited)
    }

    /// Whether a body at `present` rests within the settle slack of a resting server body.
    pub fn at_rest_target(&self, present: &Pose, tuning: &Tuning) -> bool {
        let Some((_, s)) = self.target else {
            return false;
        };
        let rests =
            |p: &Pose| norm(&p.lin) < tuning.rest_speed && norm(&p.ang) < tuning.rest_angular;
        rests(&s)
            && rests(present)
            && norm(&sub(&s.pos, &present.pos)) <= tuning.position * SETTLE_SLACK
            && norm(&rot_log(&q_mul(&s.rot, &q_conj(&present.rot)))) <= tuning.angle * SETTLE_SLACK
    }

    /// Ends a settle whose body waits at the server's pose: the pose to put it at (exactly, unfrozen,
    /// still, asleep). `now` is the predicted tick.
    pub fn finish_settle(&mut self, now: u64) -> Option<Pose> {
        let st = self.settle.filter(|s| s.k >= s.n)?;
        self.settle = None;
        self.settled = Some((st.tick, now));
        self.stats.settles += 1;
        self.pending_pos = [0.0; 3];
        self.pending_rot = [0.0; 3];
        Some(st.to)
    }

    /// The local body lives ahead of the newest known server tick `known` (the predicted timeline):
    /// what it did since then the server may not have simulated yet (a predicted player's push). True
    /// when it rested at `known` in its recorded history and has not moved since.
    fn still_since(&self, present: &Pose, known: u64, tuning: &Tuning) -> bool {
        let rests =
            |p: &Pose| norm(&p.lin) < tuning.rest_speed && norm(&p.ang) < tuning.rest_angular;
        match self.recorded(known) {
            Some((_, m, _)) => {
                rests(&m.pose)
                    && norm(&sub(&present.pos, &m.pose.pos)) < tuning.position * 0.5
                    && norm(&rot_log(&q_mul(&present.rot, &q_conj(&m.pose.rot))))
                        < tuning.angle * 0.5
            }
            None => true,
        }
    }

    /// Starts settling a body at `present` onto the server's resting pose `s` of tick `t`.
    fn start_settle(&mut self, t: u64, s: &Pose, present: &Pose, dt: f64, tuning: &Tuning) -> Step {
        let e_pos = norm(&sub(&s.pos, &present.pos));
        let e_rot = norm(&rot_log(&q_mul(&s.rot, &q_conj(&present.rot))));
        let size = (e_pos / tuning.position.max(1e-9)).max(e_rot / tuning.angle.max(1e-9));
        let most = (SETTLE_MAX_TICKS * dt * 60.0).max(1.0);
        let n = (size * 3.0).ceil().clamp(1.0, most) as u32;
        self.pending_pos = [0.0; 3];
        self.pending_rot = [0.0; 3];
        self.settle = Some(Settling {
            from: still(present),
            to: still(s),
            tick: t,
            k: 0,
            n,
            waited: 0,
        });
        self.settling(t, s, true).unwrap_or(Step::None)
    }

    /// Whether a settle is under way.
    pub fn is_settling(&self) -> bool {
        self.settle.is_some()
    }

    /// Joins a settle round another body of the same manager started this tick: a body whose
    /// server's body rests, quiet as long as a settle needs, settles too, whatever it does locally (a
    /// pile settles as one, so no settled body is knocked off again by a neighbour that was not), unless
    /// it is beyond the teleport distance. (gdnet keeps out a body a predicted player touched lately:
    /// its push is in the server's future.) Its settle step, when it joined.
    pub fn join_settle(
        &mut self,
        present: &Pose,
        known: u64,
        dt: f64,
        tuning: &Tuning,
    ) -> Option<Step> {
        if self.settle.is_some() {
            return None;
        }
        let (t, s) = self.target?;
        let rests =
            |p: &Pose| norm(&p.lin) < tuning.rest_speed && norm(&p.ang) < tuning.rest_angular;
        let quiet = known.saturating_sub(t) as f64 * dt;
        if !rests(&s)
            || quiet < SETTLE_QUIET_S
            || norm(&sub(&s.pos, &present.pos)) > tuning.teleport
        {
            return None;
        }
        Some(self.start_settle(t, &s, present, dt, tuning))
    }

    /// A settle's step while one is under way: carry, wait, or end it (a newer sample that moves).
    fn settling(&mut self, t: u64, s: &Pose, rests: bool) -> Option<Step> {
        let mut st = self.settle?;
        if t != st.tick {
            if !rests {
                self.settle = None;
                self.epoch = None;
                return Some(Step::Release {
                    lin: s.lin,
                    ang: s.ang,
                });
            }
            // A newer resting sample: settle onto it instead (its wait starts again).
            st.tick = t;
            st.to = still(s);
            st.waited = 0;
            if st.k >= st.n {
                self.settle = Some(st);
                return Some(Step::Hold(st.to));
            }
        }
        if st.k < st.n {
            st.k += 1;
        } else {
            st.waited += 1;
        }
        let f = f64::from(st.k) / f64::from(st.n.max(1));
        let f = f * f * (3.0 - 2.0 * f);
        let pose = if st.k >= st.n {
            st.to
        } else {
            Pose {
                pos: add(&st.from.pos, &scale(&sub(&st.to.pos, &st.from.pos), f)),
                rot: q_norm(&q_mul(
                    &rot_exp(&scale(
                        &rot_log(&q_mul(&st.to.rot, &q_conj(&st.from.rot))),
                        f,
                    )),
                    &st.from.rot,
                )),
                lin: [0.0; 3],
                ang: [0.0; 3],
            }
        };
        self.settle = Some(st);
        Some(Step::Hold(pose))
    }

    /// Whether the local body changed velocity faster than `max_accel` in any recorded tick after
    /// `tick` (it hit something: carrying its pose forward from `tick` would miss that).
    fn contact_since(&self, tick: u64, t: &Tuning) -> bool {
        self.history
            .iter()
            .rev()
            .take_while(|(k, _, _)| *k > tick)
            .any(|(_, m, _)| norm(&m.acc) > t.max_accel || norm(&m.alpha) > t.max_angular_accel)
    }

    /// This tick's correction of a body at `present`, at predicted tick `now`, when the server's
    /// state is known up to tick `known` (the newest snapshot tick, or later when nothing arrived because
    /// nothing changed); `dt` is the tick's length in seconds.
    pub fn step(&mut self, present: &Pose, now: u64, known: u64, dt: f64, tuning: &Tuning) -> Step {
        let Some((t, s)) = self.target else {
            return Step::None;
        };
        let rests =
            |p: &Pose| norm(&p.lin) < tuning.rest_speed && norm(&p.ang) < tuning.rest_angular;
        let fresh = core::mem::take(&mut self.fresh);
        if self.settle_blocked && self.settle.take().is_some() {
            return Step::Release {
                lin: [0.0; 3],
                ang: [0.0; 3],
            };
        }
        if let Some(step) = self.settling(t, &s, rests(&s)) {
            return step;
        }
        let tau = now.saturating_sub(t) as f64 * dt;
        // The server's body at tick `t`, with the accelerations of its last two samples.
        let mut server = Moving {
            pose: s,
            acc: [0.0; 3],
            alpha: [0.0; 3],
        };
        if let Some((b, p)) = self.before.filter(|(b, _)| t > *b && t - *b <= 3) {
            let span = (t - b) as f64 * dt;
            server.acc = scale(&sub(&s.lin, &p.lin), 1.0 / span);
            server.alpha = scale(&sub(&s.ang, &p.ang), 1.0 / span);
        }
        // A server sample whose velocity changed faster than `max_accel` since the one before was
        // taken in a contact, as is a local span with such a tick: neither is compared.
        let server_contact =
            norm(&server.acc) > tuning.max_accel || norm(&server.alpha) > tuning.max_angular_accel;
        // Each error is the full outstanding one at present (position, rotation vector, velocities).
        let mut errors = None;
        // Both at rest: the settle below puts the body where the server's is, without a correction
        // that would wake it.
        let both_rest =
            rests(&s) && rests(present) && norm(&sub(&s.pos, &present.pos)) <= tuning.teleport;
        if fresh && self.epoch.is_none_or(|e| t > e + 1) && !server_contact && !both_rest {
            // A sample is compared once it describes a tick two after the last correction started
            // (a comparison never looks back past a velocity correction, and the recorded pose then has
            // an acceleration measured after it). Both sides are carried from
            // tick `t` to now by their own motion then, the local body only across a span without a
            // contact (else the comparison waits for a later sample): motion the local body has and the
            // server will have too, a predicted player's push, cancels out. The position nudges
            // applied since `t` are taken off.
            errors = match self.recorded(t) {
                Some((_, h, a)) if !self.contact_since(t, tuning) => {
                    let (sp, lp) = (carry(&s, tau), carry(&h.pose, tau));
                    let since = |now: &V3, then: &V3| sub(now, then);
                    Some((
                        sub(&sub(&sp.pos, &lp.pos), &since(&self.applied.pos, &a.pos)),
                        sub(
                            &rot_log(&q_mul(&sp.rot, &q_conj(&lp.rot))),
                            &since(&self.applied.rot, &a.rot),
                        ),
                        sub(&sub(&sp.lin, &lp.lin), &since(&self.applied.lin, &a.lin)),
                        sub(&sub(&sp.ang, &lp.ang), &since(&self.applied.ang, &a.ang)),
                    ))
                }
                // A tick the history does not hold (before the replica's first record, or too old):
                // nothing to compare with; a later sample or the resting comparison will be.
                _ => None,
            };
        }
        let refine = errors.is_some();
        let ticks = |secs: f64| (secs / dt.max(1e-6)).ceil() as u64;
        let quiet = known.saturating_sub(t);
        let still_since_known = self.still_since(present, known, tuning);
        // And it has been still for the quiet time itself: every pose recorded in it within the slack
        // of the present one (a body a predicted player just started to push moves less than a
        // threshold in its first ticks).
        let window = ticks(SETTLE_QUIET_S);
        let (slack_pos, slack_rot) = (tuning.position * SETTLE_SLACK, tuning.angle * SETTLE_SLACK);
        let recent = self
            .history
            .iter()
            .rev()
            .take_while(|(k, _, _)| *k + window >= now);
        let still_lately = recent.clone().count() as u64 + 1 >= window
            && recent.clone().all(|(_, m, _)| {
                norm(&sub(&present.pos, &m.pose.pos)) <= slack_pos
                    && norm(&rot_log(&q_mul(&present.rot, &q_conj(&m.pose.rot)))) <= slack_rot
            });
        if errors.is_none()
            && rests(&s)
            && rests(present)
            && still_since_known
            && still_lately
            && !self.settle_blocked
            && quiet >= ticks(SETTLE_QUIET_S)
        {
            // The server's body rests, and nothing newer came: the local one belongs exactly where the
            // server's is, asleep (see the module docs).
            let (e_pos, e_rot) = (
                norm(&sub(&s.pos, &present.pos)),
                norm(&rot_log(&q_mul(&s.rot, &q_conj(&present.rot)))),
            );
            if e_pos > tuning.teleport {
                // Too far to carry: it snaps, as any body that far off does.
                self.stats.snaps += 1;
                self.pending_pos = [0.0; 3];
                self.pending_rot = [0.0; 3];
                self.applied = Applied::default();
                self.history.clear();
                self.epoch = Some(now);
                return Step::Snap(still(&s));
            }
            let off = e_pos > tuning.position * SETTLE_SLACK || e_rot > tuning.angle * SETTLE_SLACK;
            let due = match self.settled {
                None => true,
                Some((tick, _)) if tick != t => true,
                Some((_, at)) => off && now >= at + ticks(SETTLE_RETRY_S),
            };
            if due {
                return self.start_settle(t, &s, present, dt, tuning);
            }
        }
        let mut wake = false;
        let (mut dlin, mut dang) = ([0.0; 3], [0.0; 3]);
        if let Some((e_pos, e_rot, e_lin, e_ang)) = errors {
            // What the correction under way does not cover yet.
            let (dist, angle) = (
                norm(&sub(&e_pos, &self.pending_pos)),
                norm(&sub(&e_rot, &self.pending_rot)),
            );
            if norm(&e_pos) > tuning.teleport {
                self.stats.snaps += 1;
                self.pending_pos = [0.0; 3];
                self.pending_rot = [0.0; 3];
                self.applied = Applied::default();
                self.history.clear();
                self.epoch = Some(now);
                return Step::Snap(carry(&s, tau));
            }
            if dist > tuning.position || angle > tuning.angle {
                // A new correction: the pose error decays, the velocities are set at once.
                self.stats.corrections += 1;
                wake = true;
                self.pending_pos = e_pos;
                self.pending_rot = e_rot;
                dlin = e_lin;
                dang = e_ang;
                self.epoch = Some(now);
            } else if refine && !(rests(&s) && rests(present)) {
                // Below the thresholds, a moving body is refined: quietly (not counted, not woken),
                // once per comparison window, so a body that runs alongside the server's (a platform a
                // predicted player rides) keeps to it more closely than the thresholds. A resting one
                // is left alone.
                let small = norm(&e_pos) > tuning.position * 0.01
                    || norm(&e_rot) > tuning.angle * 0.01
                    || norm(&e_lin) > tuning.rest_speed * 0.1
                    || norm(&e_ang) > tuning.rest_angular * 0.1;
                if small {
                    self.pending_pos = e_pos;
                    self.pending_rot = e_rot;
                    dlin = e_lin;
                    dang = e_ang;
                    self.epoch = Some(now);
                }
            }
        }
        let moved = norm(&dlin) > 0.0 || norm(&dang) > 0.0;
        if !self.is_correcting() && !wake && !moved {
            return Step::None;
        }
        let keep = tuning.decay.clamp(0.0, 1.0).powf(dt.max(0.0) * 60.0);
        let mut dpos = scale(&self.pending_pos, 1.0 - keep);
        let mut drot = scale(&self.pending_rot, 1.0 - keep);
        self.pending_pos = sub(&self.pending_pos, &dpos);
        self.pending_rot = sub(&self.pending_rot, &drot);
        // What is left below a hundredth of the thresholds is applied now.
        if norm(&self.pending_pos) < tuning.position * 0.01
            && norm(&self.pending_rot) < tuning.angle * 0.01
        {
            dpos = add(&dpos, &self.pending_pos);
            drot = add(&drot, &self.pending_rot);
            self.pending_pos = [0.0; 3];
            self.pending_rot = [0.0; 3];
        }
        self.applied.pos = add(&self.applied.pos, &dpos);
        self.applied.rot = add(&self.applied.rot, &drot);
        self.applied.lin = add(&self.applied.lin, &dlin);
        self.applied.ang = add(&self.applied.ang, &dang);
        Step::Nudge {
            dpos,
            drot,
            dlin,
            dang,
            wake,
        }
    }
}

/// `p` with no velocity.
fn still(p: &Pose) -> Pose {
    Pose {
        lin: [0.0; 3],
        ang: [0.0; 3],
        ..*p
    }
}

/// `pose` moved by a nudge (the rotation vector applied before its rotation).
pub fn apply_nudge(pose: &Pose, dpos: &V3, drot: &V3, dlin: &V3, dang: &V3) -> Pose {
    Pose {
        pos: add(&pose.pos, dpos),
        rot: q_norm(&q_mul(&rot_exp(drot), &pose.rot)),
        lin: add(&pose.lin, dlin),
        ang: add(&pose.ang, dang),
    }
}

// --- vector and quaternion helpers ---------------------------------------------------------------

pub fn add(a: &V3, b: &V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn sub(a: &V3, b: &V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn scale(a: &V3, k: f64) -> V3 {
    [a[0] * k, a[1] * k, a[2] * k]
}

pub fn norm(a: &V3) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

pub fn q_mul(a: &Quat, b: &Quat) -> Quat {
    let (ax, ay, az, aw) = (a[0], a[1], a[2], a[3]);
    let (bx, by, bz, bw) = (b[0], b[1], b[2], b[3]);
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

pub fn q_conj(a: &Quat) -> Quat {
    [-a[0], -a[1], -a[2], a[3]]
}

pub fn q_norm(a: &Quat) -> Quat {
    let n = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2] + a[3] * a[3]).sqrt();
    if n < 1e-12 {
        return IDENTITY;
    }
    [a[0] / n, a[1] / n, a[2] / n, a[3] / n]
}

/// The rotation vector (axis times angle, the shorter way) of a quaternion.
pub fn rot_log(q: &Quat) -> V3 {
    let mut q = q_norm(q);
    if q[3] < 0.0 {
        q = [-q[0], -q[1], -q[2], -q[3]];
    }
    let s = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2]).sqrt();
    if s < 1e-12 {
        return [0.0; 3];
    }
    let angle = 2.0 * s.atan2(q[3]);
    [q[0] / s * angle, q[1] / s * angle, q[2] / s * angle]
}

/// The quaternion of a rotation vector.
pub fn rot_exp(v: &V3) -> Quat {
    let angle = norm(v);
    if angle < 1e-12 {
        return IDENTITY;
    }
    let h = angle * 0.5;
    let k = h.sin() / angle;
    [v[0] * k, v[1] * k, v[2] * k, h.cos()]
}

/// The quaternion of a 2D rotation (radians about z).
pub fn quat_of_angle(a: f64) -> Quat {
    [0.0, 0.0, (a * 0.5).sin(), (a * 0.5).cos()]
}

/// The 2D angle (about z) of a quaternion, in `[-PI, PI]`.
pub fn angle_of_quat(q: &Quat) -> f64 {
    let mut q = q_norm(q);
    if q[3] < 0.0 {
        q = [-q[0], -q[1], -q[2], -q[3]];
    }
    2.0 * q[2].atan2(q[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn tuning() -> Tuning {
        Tuning::defaults(100.0)
    }

    fn at(x: f64, vx: f64) -> Pose {
        Pose {
            pos: [x, 0.0, 0.0],
            lin: [vx, 0.0, 0.0],
            ..Pose::default()
        }
    }

    /// Applies a step to a modelled body; a settle that holds the body at the server's pose ends at
    /// once (gdnet ends a manager's settles together).
    fn apply_step(c: &mut Corrector, local: &mut Pose, step: Step, now: u64) {
        match step {
            Step::None => {}
            Step::Nudge {
                dpos,
                drot,
                dlin,
                dang,
                ..
            } => *local = apply_nudge(local, &dpos, &drot, &dlin, &dang),
            Step::Snap(p) | Step::Hold(p) => *local = p,
            Step::Release { lin, ang } => {
                local.lin = lin;
                local.ang = ang;
            }
        }
        if c.is_holding() {
            if let Some(p) = c.finish_settle(now) {
                *local = p;
            }
        }
    }

    /// Runs `ticks` ticks of a body moving at `v` px/s locally while the server's moves at `vs`, with
    /// snapshots `delay` ticks late; returns the corrector, the local pose and the largest step.
    fn run(corr: &mut Corrector, local: &mut Pose, ticks: u64, vs: f64, delay: u64) -> f64 {
        let mut largest: f64 = 0.0;
        for now in 1..=ticks {
            if now > delay {
                let t = now - delay;
                // Both sides read the pose before the tick's physics step.
                corr.sample(t, Component::Pos([vs * (t - 1) as f64 * DT, 0.0, 0.0]));
                corr.sample(t, Component::Lin([vs, 0.0, 0.0]));
            }
            let before = local.pos[0];
            let step = corr.step(local, now, now.saturating_sub(delay), DT, &tuning());
            apply_step(corr, local, step, now);
            corr.record(now, *local, DT);
            // The local physics step.
            local.pos[0] += local.lin[0] * DT;
            largest = largest.max((local.pos[0] - before).abs());
        }
        largest
    }

    #[test]
    fn modes_parse_and_index() {
        for m in [
            PhysicsMode::Server,
            PhysicsMode::Owner,
            PhysicsMode::Handoff,
            PhysicsMode::Simulated,
        ] {
            assert_eq!(PhysicsMode::from_index(m.index()), m);
            assert_eq!(PhysicsMode::parse(m.name()), Some(m));
        }
        assert_eq!(
            PhysicsMode::parse(" Simulated "),
            Some(PhysicsMode::Simulated)
        );
        assert_eq!(PhysicsMode::parse("rollback"), None);
        assert_eq!(PhysicsMode::from_index(9), PhysicsMode::Server);
    }

    #[test]
    fn agreeing_simulations_are_never_corrected_whatever_the_latency() {
        // Both bodies move at 120 px/s; snapshots arrive 8 ticks late. Against the present the error
        // would be 16 px every tick; against the recorded pose of the snapshot's tick it is zero.
        let mut c = Corrector::new();
        c.seed(at(0.0, 120.0));
        let mut local = at(0.0, 120.0);
        // The first record must exist before the first sample's tick is compared.
        run(&mut c, &mut local, 120, 120.0, 8);
        assert_eq!(c.stats.corrections, 0);
        assert_eq!(c.stats.snaps, 0);
        assert!((local.pos[0] - 120.0 * 120.0 * DT).abs() < 1e-6);
    }

    #[test]
    fn errors_below_the_thresholds_are_left_alone() {
        let mut c = Corrector::new();
        c.record(5, at(10.0, 0.0), DT);
        c.sample(5, Component::Pos([11.5, 0.0, 0.0]));
        let t = tuning();
        assert!(t.position > 1.5);
        assert_eq!(c.step(&at(10.0, 0.0), 6, 5, DT, &t), Step::None);
        assert_eq!(c.stats.corrections, 0);
        // An angle below its threshold too.
        c.sample(6, Component::Rot(quat_of_angle(t.angle * 0.5)));
        c.record(6, at(10.0, 0.0), DT);
        assert_eq!(c.step(&at(10.0, 0.0), 7, 6, DT, &t), Step::None);
    }

    #[test]
    fn a_correction_decays_geometrically_and_is_applied_exactly_once() {
        // The server's body is 20 px ahead; neither moves, but nothing counts as resting here (a
        // resting pair settles instead: see the settle tests), so the correction's offsets show.
        let mut c = Corrector::new();
        let mut local = at(0.0, 0.0);
        c.record(1, local, DT);
        c.sample(1, Component::Pos([20.0, 0.0, 0.0]));
        let t = Tuning {
            rest_speed: 0.0,
            ..tuning()
        };
        let mut steps = Vec::new();
        for now in 2..200 {
            match c.step(&local, now, 1, DT, &t) {
                Step::Nudge { dpos, wake, .. } => {
                    assert_eq!(
                        wake,
                        now == 2,
                        "only the start of a correction wakes the body"
                    );
                    local.pos = add(&local.pos, &dpos);
                    steps.push(dpos[0]);
                }
                Step::None => {}
                Step::Snap(_) => panic!("20 px is no teleport"),
                Step::Hold(_) | Step::Release { .. } => {
                    panic!("no settle: the sample is the newest")
                }
            }
            c.record(now, local, DT);
        }
        assert_eq!(c.stats.corrections, 1);
        assert!(
            (local.pos[0] - 20.0).abs() < 1e-9,
            "landed: {}",
            local.pos[0]
        );
        assert!((steps[0] - 20.0 * (1.0 - t.decay)).abs() < 1e-9);
        // (The last step also lands what was left below a hundredth of the threshold.)
        let n = steps.len() - 1;
        assert!(
            steps[..n].windows(2).all(|w| w[1] <= w[0] + 1e-12),
            "never speeds up"
        );
        assert!(steps[n] < t.position * 0.02);
        assert!(steps.len() < 60, "done within {} ticks", steps.len());
    }

    /// Runs a resting local body against a resting server sample from `from` to `to` (snapshot ticks
    /// go on, nothing changes); returns the largest step and the first tick the body sat exactly at
    /// `at_x` with the settle finished.
    fn rest_run(
        c: &mut Corrector,
        local: &mut Pose,
        from: u64,
        to: u64,
        at_x: f64,
    ) -> (f64, Option<u64>) {
        let (mut largest, mut landed) = (0.0f64, None);
        for now in from..to {
            let before = local.pos[0];
            let step = c.step(local, now, now - 1, DT, &tuning());
            assert!(
                !matches!(step, Step::Nudge { wake: true, .. }),
                "a resting body is never woken by a correction"
            );
            apply_step(c, local, step, now);
            largest = largest.max((local.pos[0] - before).abs());
            if landed.is_none() && local.pos[0] == at_x && !c.is_correcting() {
                landed = Some(now);
            }
            c.record(now, *local, DT);
        }
        (largest, landed)
    }

    #[test]
    fn a_resting_body_settles_exactly_onto_the_resting_server_body_once() {
        // The server's body rests at 30 px; snapshots stop (nothing changes) but the snapshot ticks go
        // on. The local body rests at 0: after the quiet time it is carried there in a few ticks and
        // put there exactly, once, with no correction (no offsets, nothing woken).
        let mut c = Corrector::new();
        let mut local = at(0.0, 0.0);
        c.record(1, local, DT);
        c.sample(1, Component::Pos([30.0, 0.0, 0.0]));
        let (largest, landed) = rest_run(&mut c, &mut local, 2, 400, 30.0);
        assert_eq!(c.stats.corrections, 0);
        assert_eq!(c.stats.settles, 1);
        assert_eq!(local.pos[0], 30.0, "exactly at the server's pose");
        let quiet = (SETTLE_QUIET_S / DT).ceil() as u64;
        let landed = landed.expect("settled");
        assert!(
            landed <= 2 + quiet + SETTLE_MAX_TICKS as u64,
            "settled at tick {landed}"
        );
        assert!(largest < 30.0 * 0.25, "carried, not jumped: {largest} px");
        // Pushed off by 5 px locally (a neighbour): settled once more, after the retry time.
        local.pos[0] += 5.0;
        let (_, again) = rest_run(&mut c, &mut local, 400, 600, 30.0);
        assert_eq!(c.stats.settles, 2);
        assert_eq!(local.pos[0], 30.0);
        assert!(again.is_some());
        // Within the slack it is left alone (Godot keeps it asleep there).
        local.pos[0] += SETTLE_SLACK * tuning().position * 0.5;
        let x = local.pos[0];
        rest_run(&mut c, &mut local, 600, 800, 30.0);
        assert_eq!(c.stats.settles, 2);
        assert_eq!(local.pos[0], x);
    }

    #[test]
    fn a_replica_resting_just_beyond_the_threshold_settles_within_a_third_of_a_second() {
        // The measured case: a replica resting 0.0201 m from the server's body (2.01 px at 100 px per
        // metre), just over `correction_position`, used to be nudged 3 to 4 times a second forever.
        let mut c = Corrector::new();
        let mut local = at(0.0, 0.0);
        c.record(1, local, DT);
        c.sample(1, Component::Pos([2.1, 0.0, 0.0]));
        let (_, landed) = rest_run(&mut c, &mut local, 2, 200, 2.1);
        let landed = landed.expect("settled") as f64 * DT;
        assert!(landed < 0.34, "settled after {landed} s");
        assert_eq!(c.stats.corrections, 0);
        assert_eq!(c.stats.settles, 1);
    }

    #[test]
    fn a_settle_ends_at_once_when_the_server_body_moves_again() {
        let mut c = Corrector::new();
        let local = at(0.0, 0.0);
        c.record(1, local, DT);
        c.sample(1, Component::Pos([30.0, 0.0, 0.0]));
        let quiet = (SETTLE_QUIET_S / DT).ceil() as u64;
        let mut held = false;
        for now in 2..(3 + quiet) {
            held |= matches!(c.step(&local, now, now - 1, DT, &tuning()), Step::Hold(_));
            c.record(now, local, DT);
        }
        assert!(held && c.is_carrying());
        c.sample(3 + quiet, Component::Lin([200.0, 0.0, 0.0]));
        assert_eq!(
            c.step(&local, 4 + quiet, 3 + quiet, DT, &tuning()),
            Step::Release {
                lin: [200.0, 0.0, 0.0],
                ang: [0.0; 3]
            }
        );
        assert!(!c.is_correcting());
    }

    #[test]
    fn a_diverging_body_is_pulled_back_without_a_jump() {
        // The server's body moves at 100 px/s, the local one at 60: the velocity error is applied with
        // the first correction, the position error decays.
        let mut c = Corrector::new();
        c.seed(at(0.0, 100.0));
        let mut local = at(0.0, 60.0);
        let largest = run(&mut c, &mut local, 240, 100.0, 6);
        let server_now = 100.0 * 240.0 * DT;
        assert!(
            (local.pos[0] - server_now).abs() < tuning().position,
            "converged: {} vs {server_now}",
            local.pos[0]
        );
        assert!(
            (local.lin[0] - 100.0).abs() < 1e-6,
            "velocity pulled to the server's"
        );
        assert!(
            largest < 4.0,
            "largest step {largest} px (100 px/s is 1.7 px per tick)"
        );
        assert!(
            c.stats.corrections <= 3,
            "{} corrections",
            c.stats.corrections
        );
    }

    #[test]
    fn beyond_the_teleport_distance_it_snaps_carried_to_the_present() {
        let mut c = Corrector::new();
        c.record(10, at(0.0, 0.0), DT);
        c.sample(10, Component::Pos([500.0, 0.0, 0.0]));
        c.sample(10, Component::Lin([60.0, 0.0, 0.0]));
        match c.step(&at(0.0, 0.0), 16, 10, DT, &tuning()) {
            Step::Snap(p) => {
                assert!(
                    (p.pos[0] - 506.0).abs() < 1e-9,
                    "carried 6 ticks: {}",
                    p.pos[0]
                );
                assert_eq!(p.lin, [60.0, 0.0, 0.0]);
            }
            other => panic!("expected a snap, got {other:?}"),
        }
        assert_eq!(c.stats.snaps, 1);
        assert_eq!(c.stats.corrections, 0);
    }

    #[test]
    fn rotations_are_corrected_the_shorter_way() {
        let mut c = Corrector::new();
        let mut local = Pose {
            rot: quat_of_angle(3.0),
            ..Pose::default()
        };
        c.record(1, local, DT);
        c.sample(1, Component::Rot(quat_of_angle(-3.0)));
        // Nothing counts as resting (a resting pair settles instead).
        let t = Tuning {
            rest_speed: 0.0,
            ..tuning()
        };
        for now in 2..200 {
            if let Step::Nudge {
                dpos,
                drot,
                dlin,
                dang,
                ..
            } = c.step(&local, now, 1, DT, &t)
            {
                local = apply_nudge(&local, &dpos, &drot, &dlin, &dang);
            }
            c.record(now, local, DT);
        }
        let a = angle_of_quat(&local.rot);
        assert!((a + 3.0).abs() < 1e-6, "turned through PI: {a}");
        assert_eq!(c.stats.corrections, 1);
    }

    /// One tick of a body sliding on a floor: friction takes `decel` off its speed until it stops.
    fn slide(p: &mut Pose, decel: f64) {
        p.pos[0] += p.lin[0] * DT;
        let v = p.lin[0];
        p.lin[0] = if v.abs() <= decel * DT {
            0.0
        } else {
            v - decel * DT * v.signum()
        };
    }

    #[test]
    fn a_push_only_the_server_saw_slides_the_body_there_against_friction() {
        // The server's box is pushed to 300 px/s at tick 10 and slides to a stop under friction; the
        // local one rests. Snapshots arrive 6 ticks late and the local body runs 6 ticks ahead (the
        // predicted timeline), so a comparison is 12 ticks old when it is made.
        let (decel, delay, lead) = (1000.0, 6u64, 6u64);
        let mut server = at(0.0, 0.0);
        let mut states = Vec::new();
        for t in 0..200u64 {
            if t == 10 {
                server.lin[0] = 300.0;
            }
            states.push(server);
            slide(&mut server, decel);
        }
        let mut c = Corrector::new();
        let mut local = at(0.0, 0.0);
        let (mut largest, mut worst_after) = (0.0f64, 0.0f64);
        for now in lead + delay..200 + lead {
            let newest = now - lead - delay;
            // Snapshots carry changes only: a body at rest sends nothing.
            let s = states[newest.min(199) as usize];
            let was = states[newest.saturating_sub(1).min(199) as usize];
            if s.pos != was.pos || newest == 0 {
                c.sample(newest, Component::Pos(s.pos));
            }
            if s.lin != was.lin || newest == 0 {
                c.sample(newest, Component::Lin(s.lin));
            }
            let before = local.pos[0];
            let step = c.step(&local, now, newest, DT, &tuning());
            apply_step(&mut c, &mut local, step, now);
            c.record(now, local, DT);
            slide(&mut local, decel);
            largest = largest.max((local.pos[0] - before).abs());
            if now > 120 {
                worst_after = worst_after.max((local.pos[0] - server.pos[0]).abs());
            }
        }
        // The server's box stops after 0.3 s, about 47 px on.
        assert!((server.pos[0] - 47.5).abs() < 0.5, "{}", server.pos[0]);
        assert!(
            worst_after < tuning().position,
            "settled {worst_after} px off"
        );
        assert!(largest < 15.0, "largest step {largest} px");
        assert!(
            c.stats.corrections <= 4,
            "{} corrections",
            c.stats.corrections
        );
        assert_eq!(c.stats.snaps, 0);
    }

    #[test]
    fn a_span_with_a_contact_is_not_compared() {
        // The local body hit something two ticks after the sample's tick (its velocity dropped from
        // 200 to 0 in a tick): carrying its recorded pose forward across that would be wrong.
        let mut c = Corrector::new();
        c.record(10, at(0.0, 200.0), DT);
        c.record(11, at(3.33, 200.0), DT);
        c.record(12, at(6.0, 0.0), DT);
        c.sample(10, Component::Pos([10.0, 0.0, 0.0]));
        c.sample(10, Component::Lin([200.0, 0.0, 0.0]));
        assert_eq!(c.step(&at(6.0, 0.0), 13, 10, DT, &tuning()), Step::None);
        assert_eq!(c.stats.corrections, 0);
        // A server sample taken in a contact (its velocity jumped since the one before) is not either.
        let mut c = Corrector::new();
        for t in 20..30 {
            c.record(t, at(0.0, 0.0), DT);
        }
        c.sample(24, Component::Lin([0.0, 0.0, 0.0]));
        c.sample(25, Component::Pos([8.0, 0.0, 0.0]));
        c.sample(25, Component::Lin([400.0, 0.0, 0.0]));
        assert_eq!(c.step(&at(0.0, 0.0), 29, 25, DT, &tuning()), Step::None);
        // The next one, a tick later and without the jump, is.
        c.sample(26, Component::Pos([14.6, 0.0, 0.0]));
        c.sample(26, Component::Lin([395.0, 0.0, 0.0]));
        assert!(matches!(
            c.step(&at(0.0, 0.0), 30, 26, DT, &tuning()),
            Step::Nudge { wake: true, .. }
        ));
    }

    #[test]
    fn quaternion_helpers_round_trip() {
        let v = [0.3, -0.2, 0.5];
        let back = rot_log(&rot_exp(&v));
        assert!(norm(&sub(&v, &back)) < 1e-12);
        assert!((angle_of_quat(&quat_of_angle(1.25)) - 1.25).abs() < 1e-12);
        assert_eq!(rot_log(&IDENTITY), [0.0; 3]);
    }

    #[test]
    fn the_nearest_recorded_pose_covers_gaps_and_ticks_before_the_history() {
        let mut c = Corrector::new();
        assert_eq!(c.recorded_pose_or_nearest(5), None);
        c.record(10, at(1.0, 0.0), DT);
        c.record(11, at(2.0, 0.0), DT);
        c.record(14, at(5.0, 0.0), DT);
        let x = |t: u64| c.recorded_pose_or_nearest(t).map(|p| p.pos[0]);
        assert_eq!(x(11), Some(2.0));
        assert_eq!(x(13), Some(2.0), "a gap: the newest pose before it");
        assert_eq!(x(3), Some(1.0), "before the history: the oldest pose");
        assert_eq!(x(20), Some(5.0));
        assert_eq!(
            c.recorded_pose(13),
            None,
            "the exact lookup still says none"
        );
    }
}

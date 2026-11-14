//! WP19 (docs/PLAN_NETCODE.md 12.4): the `handoff` physics mode, the Godot side. A child module of
//! `manager` (it reads the manager's entries); netcore's `handoff` has the arbitration and the rules.
//!
//! **The holder is the owner.** The server sends a `handoff` body with `client_owned = true`, so the
//! WP7 and modes.rs rules apply unchanged: the body is live on its owner (the server while nobody holds
//! it) and frozen everywhere else, where it is interpolated or predicted per `body_prediction`.
//!
//! **Contactors** (each tick, [`pass`]): on a client, the parts of the entities it owns and simulates:
//! its predicted entities (its player) and its live `client_owned` bodies (its `owner` bodies, the
//! `handoff` bodies it holds); on a listen server, its own input-driven entities (the host's player).
//! **Contacts**: a CharacterBody's slide collisions of the last `move_and_slide()`, a RigidBody's contacts
//! (the physics server's per-body contact list; gdnet raises `max_contacts_reported` to 4 on a body it
//! reads that has 0, else `get_colliding_bodies()` with `contact_monitor`). A contact *pushes* when a
//! CharacterBody hits it from the side or from below (a player walking into a frozen crate is stopped
//! by it), or lands on it with motion left over, or when the touching RigidBody moves faster than
//! `correction_rest_speed` (now or a tick ago: a body that hits another head-on has stopped when its
//! contact is read); a player standing still on a body, or a body resting on another, does not claim
//! it. A moving RigidBody contactor also pushes what it will hit within [`LOOK_AHEAD_TICKS`] of its
//! motion (`body_test_motion`): a body still frozen when it is hit stops the one that hits it dead. A
//! player's first contact needs no look-ahead: the claimed body moves on the next physics step.
//!
//! **Claims.** A client claims a `handoff` body the server holds when one of its contactors pushes it,
//! and thaws it in the same tick (optimistic: netcore's table names it the holder at once, the server
//! confirms or overrides), at most once per body and rest time. Automatic claims never take a body
//! another client holds; `Net.claim(node)` may (a grab), and such a body is held until the game
//! releases it (`Net.release_owner`). On a listen server a contact by the host's player keeps a body it
//! holds from being claimed for 100 ms.
//!
//! **Returns.** The holder gives a body back once it has been at rest (asleep, or slower than
//! `correction_rest_speed` and turning slower than its angular counterpart) for
//! `gdnet/physics/handoff_rest_s` (default 0.5 s), or once nothing of the holder's touched it, nor was
//! a player of the holder's within its manager's `interaction_radius` of it, for that long.

use super::*;
use crate::net::Net;
use godot::classes::{
    CharacterBody2D, CharacterBody3D, PhysicsServer2D, PhysicsServer3D,
    PhysicsTestMotionParameters2D, PhysicsTestMotionParameters3D, PhysicsTestMotionResult2D,
    PhysicsTestMotionResult3D, ProjectSettings, RigidBody2D, RigidBody3D,
};
use netcore::handoff::Holding;
use netcore::PhysicsMode as Mode;
use std::collections::HashMap;

pub const SETTING_REST_S: &str = "gdnet/physics/handoff_rest_s";
/// The default rest time: Godot puts a body to sleep after 0.5 s below its sleep threshold, so a body
/// returns about when it falls asleep.
pub const DEFAULT_REST_S: f64 = 0.5;
/// A moving body this client simulates claims what it will hit within this many ticks of its motion:
/// a claim thaws the body at once, but a body hit while still frozen stops the one that hits it dead.
const LOOK_AHEAD_TICKS: f64 = 2.0;
/// Contacts a held body reports when the game left `max_contacts_reported` at 0.
const REPORTED_CONTACTS: i32 = 4;
/// A CharacterBody's collision with motion left over beyond this (pixels in 2D, metres in 3D) was
/// stopped by what it hit.
const STOPPED_2D: f32 = 0.01;
const STOPPED_3D: f32 = 1e-4;

/// `gdnet/physics/handoff_rest_s` (seconds), default 0.5; read every tick.
pub fn rest_s() -> f64 {
    let ps = ProjectSettings::singleton();
    if !ps.has_setting(SETTING_REST_S) {
        return DEFAULT_REST_S;
    }
    crate::profiles::number(&ps.get_setting(SETTING_REST_S))
        .filter(|v| *v >= 0.0)
        .unwrap_or(DEFAULT_REST_S)
}

/// What a contact did this tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct Touch {
    /// It pushed the body (a claim on a body the server holds).
    pub pushed: bool,
}

/// One manager's results of a tick.
#[derive(Default)]
pub struct StepOut {
    pub claims: Vec<NetId>,
    pub releases: Vec<NetId>,
    /// Server: bodies its own player touched.
    pub touched: Vec<NetId>,
}

thread_local! {
    /// Each RigidBody contactor's speed at the previous pass (one that hit something head-on has
    /// stopped by the time its contact is read).
    /// (physics frame, speed) per node; every session of the process shares it.
    static SPEEDS: std::cell::RefCell<HashMap<InstanceId, (u64, f64)>> = std::cell::RefCell::new(HashMap::new());
}

/// Whether a RigidBody contactor moved faster than `rest` (its units) now or in the previous frame.
fn moving(n: &Gd<Node>, speed: f64, rest: f64) -> bool {
    let frame = godot::classes::Engine::singleton().get_physics_frames();
    let prev = SPEEDS.with(|m| m.borrow_mut().insert(n.instance_id(), (frame, speed)));
    let prev = prev.filter(|(f, _)| f + 1 == frame).map_or(0.0, |p| p.1);
    speed.max(prev) > rest
}

/// Lengths per metre of a node's world (2D: 100 pixels).
fn scale_of(n: &Gd<Node>) -> f64 {
    if n.is_class("Node3D") {
        1.0
    } else {
        netcore::interp::PIXELS_PER_METRE
    }
}

/// The nodes `n` touches and whether each contact pushed (see the module docs). `rest` is
/// `correction_rest_speed` in metres per second.
fn contacts(n: &Gd<Node>, rest: f64, dt: f64) -> Vec<(Gd<Node>, bool)> {
    let mut out = Vec::new();
    let as_node = |o: Gd<Object>| o.try_cast::<Node>().ok();
    if let Ok(b) = n.clone().try_cast::<CharacterBody2D>() {
        let (up, floor) = (b.get_up_direction(), b.get_floor_max_angle() + 0.01);
        for i in 0..b.get_slide_collision_count() {
            if let Some(c) = b.get_slide_collision(i) {
                let ground = c.get_angle_ex().up_direction(up).done() <= floor;
                let stopped = !ground || c.get_remainder().length() > STOPPED_2D;
                if let Some(node) = c.get_collider().and_then(as_node) {
                    out.push((node, stopped));
                }
            }
        }
    } else if let Ok(b) = n.clone().try_cast::<CharacterBody3D>() {
        let (up, floor) = (b.get_up_direction(), b.get_floor_max_angle() + 0.01);
        for i in 0..b.get_slide_collision_count() {
            if let Some(c) = b.get_slide_collision(i) {
                let ground = c.get_angle_ex().up_direction(up).done() <= floor;
                let stopped = !ground || c.get_remainder().length() > STOPPED_3D;
                if let Some(node) = c.get_collider().and_then(as_node) {
                    out.push((node, stopped));
                }
            }
        }
    } else if let Ok(mut b) = n.clone().try_cast::<RigidBody2D>() {
        let moving = moving(n, f64::from(b.get_linear_velocity().length()), rest * 100.0);
        if moving && !b.is_freeze_enabled() {
            // What it will hit within the look-ahead counts as pushed now (see the module docs).
            let mut params = PhysicsTestMotionParameters2D::new_gd();
            params.set_from(b.get_global_transform());
            params.set_motion(b.get_linear_velocity() * (dt * LOOK_AHEAD_TICKS) as f32);
            let result = PhysicsTestMotionResult2D::new_gd();
            if PhysicsServer2D::singleton()
                .body_test_motion_ex(b.get_rid(), &params)
                .result(&result)
                .done()
            {
                if let Some(node) = result.get_collider().and_then(as_node) {
                    out.push((node, true));
                }
            }
        }
        if b.get_max_contacts_reported() == 0 && !b.is_contact_monitor_enabled() {
            b.set_max_contacts_reported(REPORTED_CONTACTS);
        }
        if b.get_max_contacts_reported() > 0 {
            if let Some(s) = PhysicsServer2D::singleton().body_get_direct_state(b.get_rid()) {
                for i in 0..s.get_contact_count() {
                    if let Some(node) = s.get_contact_collider_object(i).and_then(as_node) {
                        out.push((node, moving));
                    }
                }
            }
        } else {
            out.extend(
                b.get_colliding_bodies()
                    .iter_shared()
                    .map(|c| (c.upcast::<Node>(), moving)),
            );
        }
    } else if let Ok(mut b) = n.clone().try_cast::<RigidBody3D>() {
        let moving = moving(n, f64::from(b.get_linear_velocity().length()), rest);
        if moving && !b.is_freeze_enabled() {
            let mut params = PhysicsTestMotionParameters3D::new_gd();
            params.set_from(b.get_global_transform());
            params.set_motion(b.get_linear_velocity() * (dt * LOOK_AHEAD_TICKS) as f32);
            let result = PhysicsTestMotionResult3D::new_gd();
            if PhysicsServer3D::singleton()
                .body_test_motion_ex(b.get_rid(), &params)
                .result(&result)
                .done()
            {
                if let Some(node) = result.get_collider().and_then(as_node) {
                    out.push((node, true));
                }
            }
        }
        if b.get_max_contacts_reported() == 0 && !b.is_contact_monitor_enabled() {
            b.set_max_contacts_reported(REPORTED_CONTACTS);
        }
        if b.get_max_contacts_reported() > 0 {
            if let Some(s) = PhysicsServer3D::singleton().body_get_direct_state(b.get_rid()) {
                for i in 0..s.get_contact_count() {
                    if let Some(node) = s.get_contact_collider_object(i).and_then(as_node) {
                        out.push((node, moving));
                    }
                }
            }
        } else {
            out.extend(
                b.get_colliding_bodies()
                    .iter_shared()
                    .map(|c| (c.upcast::<Node>(), moving)),
            );
        }
    }
    out
}

/// Whether a RigidBody part is at rest: asleep, or slower than the rest speeds.
fn at_rest(nodes: &[Option<Gd<Node>>]) -> bool {
    nodes.iter().flatten().all(|n| {
        let t = modes::tuning(scale_of(n));
        if let Ok(b) = n.clone().try_cast::<RigidBody2D>() {
            b.is_sleeping()
                || (f64::from(b.get_linear_velocity().length()) < t.rest_speed
                    && f64::from(b.get_angular_velocity().abs()) < t.rest_angular)
        } else if let Ok(b) = n.clone().try_cast::<RigidBody3D>() {
            b.is_sleeping()
                || (f64::from(b.get_linear_velocity().length()) < t.rest_speed
                    && f64::from(b.get_angular_velocity().length()) < t.rest_angular)
        } else {
            true
        }
    })
}

impl NetManager {
    /// Whether an entity here has been a `handoff` body (the pass costs nothing without one).
    pub fn has_handoff(&self) -> bool {
        self.any_handoff
    }

    /// The `handoff` bodies this peer holds here (a client: the ones it simulates).
    pub fn handoff_held(&self) -> usize {
        if self.is_server {
            return 0;
        }
        self.entries
            .values()
            .filter(|e| e.physics == Mode::Handoff && e.owner == self.local_id && e.id.is_some())
            .count()
    }

    /// This tick's contactors here (see the module docs): (node, part of a player).
    pub fn handoff_contactors(&self) -> Vec<(Gd<Node>, bool)> {
        let mut out = Vec::new();
        for e in self.entries.values() {
            if e.id.is_none() {
                continue;
            }
            let player = if self.is_server {
                e.owner == netcore::SERVER && e.input_driven
            } else {
                e.owner == self.local_id && e.predicted.is_some()
            };
            let body = !self.is_server
                && e.owner == self.local_id
                && e.client_owned
                && e.body_live == Some(true);
            if player || body {
                out.extend(e.nodes.iter().flatten().map(|n| (n.clone(), player)));
            }
        }
        out
    }

    /// One tick of this manager's `handoff` bodies. `touch`: the entities a contactor touched this tick
    /// and whether one pushed; `players`: the positions of this peer's players. Updates the holding
    /// timers and returns the claims and releases to make (and, on the server, the bodies its player
    /// touched).
    pub fn handoff_step(
        &mut self,
        touch: &HashMap<NetId, Touch>,
        players: &[[f64; 3]],
        dt: f64,
        rest_s: f64,
    ) -> StepOut {
        let mut out = StepOut::default();
        let frame = godot::classes::Engine::singleton().get_physics_frames();
        let cooldown = (rest_s / dt.max(1e-6)).round() as u64;
        self.last_claim
            .retain(|_, t| frame.saturating_sub(*t) < cooldown.max(1));
        let default_radius = crate::physics::setting_radius();
        let radius = self.interaction_radius_setting();
        let mut held_now = Vec::new();
        for (iid, e) in &self.entries {
            let Some(id) = e.id else {
                continue;
            };
            if e.physics != Mode::Handoff || e.has_body != Some(true) {
                continue;
            }
            let t = touch.get(&id);
            if self.is_server {
                if t.is_some() && e.owner == netcore::SERVER {
                    out.touched.push(id);
                }
                continue;
            }
            if e.owner == self.local_id {
                held_now.push((id, *iid));
            } else if e.owner == netcore::SERVER && t.is_some_and(|t| t.pushed) {
                // At most one claim by contact per body and rest time: a refused claimant (the
                // host's player holds it) tries again after that, not on every tick it touches.
                let due = self
                    .last_claim
                    .get(&id)
                    .is_none_or(|t| frame.saturating_sub(*t) >= cooldown);
                if due {
                    out.claims.push(id);
                }
            }
        }
        if self.is_server {
            return out;
        }
        for id in &out.claims {
            self.last_claim.insert(*id, frame);
        }
        self.holding
            .retain(|id, _| held_now.iter().any(|h| h.0 == *id));
        for (id, iid) in held_now {
            let Some(e) = self.entries.get(&iid) else {
                continue;
            };
            let root = Gd::<Node>::try_from_instance_id(iid).ok();
            let near = root
                .as_ref()
                .and_then(crate::physics::position_of)
                .is_some_and(|(p, three_d)| {
                    let r = if radius > 0.0 {
                        radius
                    } else if three_d {
                        default_radius / crate::physics::PX_PER_M
                    } else {
                        default_radius
                    };
                    players.iter().any(|q| {
                        let d = [p[0] - q[0], p[1] - q[1], p[2] - q[2]];
                        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() <= r
                    })
                });
            let rests = at_rest(&e.nodes);
            let h = self
                .holding
                .entry(id)
                .or_insert_with(|| Holding::new(false));
            if h.step(rests, near || touch.contains_key(&id), dt, rest_s)
                .is_some()
            {
                out.releases.push(id);
            }
        }
        out
    }

    /// A claim or release this client made: the entry names `owner` now and the bodies take their new
    /// role at once (the claimant's body thaws before the next physics step).
    pub fn handoff_local(&mut self, id: NetId, owner: u32, session: &Gd<NetSession>) {
        let Some(iid) = self.by_id.get(&id).copied() else {
            return;
        };
        if let Some(e) = self.entries.get_mut(&iid) {
            e.owner = owner;
        }
        if owner != self.local_id {
            self.holding.remove(&id);
        }
        self.owner_moved(iid);
        self.apply_body_role(iid, session);
    }

    /// `Net.claim(node)` for the entity rooted at `root`: true when this peer holds it after the call.
    fn claim_root(&mut self, root: &Gd<Node>) -> bool {
        let Some(e) = self.entries.get(&root.instance_id()) else {
            return false;
        };
        if !self.online {
            return false;
        }
        let Some(id) = e.id else {
            return false;
        };
        if e.physics != Mode::Handoff || e.has_body != Some(true) {
            godot_error!(
                "Net.claim: {} is not a handoff body (its physics is {})",
                root.get_path(),
                e.physics.name()
            );
            return false;
        }
        let held = e.owner == self.local_id;
        let Some(mut s) = self.session() else {
            return false;
        };
        if !held {
            let claimed = s
                .bind_mut()
                .with_core(|c, now| c.claim(id, now))
                .unwrap_or(false);
            if !claimed {
                return false;
            }
            if !self.is_server {
                let local = self.local_id;
                self.handoff_local(id, local, &s);
            }
        }
        if !self.is_server {
            self.holding.insert(id, Holding::new(true));
        }
        true
    }

    /// `Net.release_owner(node)` for a `handoff` body this client holds: gives it back at once.
    pub(super) fn release_handoff(&mut self, root: &Gd<Node>) -> Option<bool> {
        let e = self.entries.get(&root.instance_id())?;
        if e.physics != Mode::Handoff || e.has_body != Some(true) {
            return None;
        }
        let id = e.id?;
        let mut s = self.session()?;
        let released = s
            .bind_mut()
            .with_core(|c, now| c.release_handoff(id, now))
            .unwrap_or(false);
        if released && !self.is_server {
            self.handoff_local(id, netcore::SERVER, &s);
        }
        Some(released)
    }
}

/// The session's handoff pass, each tick after the physics step (see the module docs). `managers` are
/// the session's; returns the claims and releases this peer made, for the caller to hand to netcore
/// (`apply`).
pub fn pass(managers: &[(u64, Gd<NetManager>)], dt: f64) -> Vec<(Gd<NetManager>, StepOut)> {
    if !managers.iter().any(|(_, m)| m.bind().has_handoff()) {
        return Vec::new();
    }
    let rest = modes::tuning(1.0).rest_speed;
    let mut touch: HashMap<NetId, Touch> = HashMap::new();
    let mut players: Vec<[f64; 3]> = Vec::new();
    let mut seen_players = std::collections::HashSet::new();
    for (_, m) in managers {
        let list = m.bind().handoff_contactors();
        for (node, player) in list {
            if player {
                if let Some((root, _)) = entity_root_of(&node) {
                    if seen_players.insert(root.instance_id()) {
                        if let Some((p, _)) = crate::physics::position_of(&root) {
                            players.push(p);
                        }
                    }
                }
            }
            let own = entity_root_of(&node).map(|(r, _)| r.instance_id());
            for (other, pushed) in contacts(&node, rest, dt) {
                let Some((root, om)) = entity_root_of(&other) else {
                    continue;
                };
                if Some(root.instance_id()) == own {
                    continue;
                }
                let id = om.bind().net_id_of(&root);
                if id == 0 {
                    continue;
                }
                let t = touch.entry(id).or_default();
                t.pushed |= pushed;
            }
        }
    }
    let frame = godot::classes::Engine::singleton().get_physics_frames();
    SPEEDS.with(|m| m.borrow_mut().retain(|_, (f, _)| *f + 1 >= frame));
    let rest_s = rest_s();
    let mut out = Vec::new();
    for (_, m) in managers {
        let mut mm = m.clone();
        let step = mm.bind_mut().handoff_step(&touch, &players, dt, rest_s);
        if !step.claims.is_empty() || !step.releases.is_empty() || !step.touched.is_empty() {
            out.push((m.clone(), step));
        }
    }
    out
}

#[godot_api(secondary)]
impl Net {
    /// WP19: claims the `handoff` body `node` belongs to for this peer (a grab, a carried item): it is
    /// simulated here from now on, and held until `Net.release_owner(node)` (or until this peer leaves).
    /// A client's claim is optimistic: the body is live here at once, and the server confirms it or
    /// gives it to an earlier claim. True when this peer holds it after the call.
    #[func]
    fn claim(&self, node: Gd<Node>) -> bool {
        claim(&node)
    }
}

#[godot_api(secondary)]
impl NetSession {
    /// WP19: `Net.claim(node)` for this session's entities.
    #[func]
    fn claim(&self, node: Gd<Node>) -> bool {
        claim(&node)
    }
}

fn claim(node: &Gd<Node>) -> bool {
    match entity_root_of(node) {
        Some((root, mut m)) => m.bind_mut().claim_root(&root),
        None => false,
    }
}

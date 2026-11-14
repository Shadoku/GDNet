//! WP19 (docs/PLAN_NETCODE.md 12.4): physics modes, the Godot side. A child module of `manager` (it
//! reads the manager's entries).
//!
//! **The option.** `NetEntity.physics`, the `configure()` key `"physics"` (stored in the `_net_physics`
//! meta on an entity without a `NetEntity`), `NetManager.physics`, the project setting
//! `gdnet/physics/mode` (default `server`, D19): the first that is not `default` wins. It applies to an
//! entity with RigidBody parts (`RigidBody2D`, `RigidBody3D` and their subclasses); an entity without
//! one is `server` whatever it says. The server resolves it at the spawn and whenever the property,
//! the meta or the manager's default changes, or `Net.set_physics(node, mode)` is called; the mode and
//! the effective `client_owned` flag travel in the `Create` and in `SetPhysics`, so every peer agrees.
//!
//! **`owner`** is `client_owned` for bodies (D21): the server sends `client_owned = true` with it, and
//! a body with `client_owned = true` is `owner` whatever its option says; everything WP7 does for a
//! `client_owned` entity follows (the owner simulates and sends its state, the
//! server applies and relays it, the others interpolate it).
//!
//! **`handoff`** is `owner` with an owner that moves by itself (handoff.rs): the server sends
//! `client_owned = true` with it, and the holder of the body is its owner.
//!
//! **Who has a live body** ([`body_live`]): on the server every body but a `client_owned` one a client
//! owns; on a client a `client_owned` one it owns, and every `simulated` one. The others are frozen. A
//! client's body that stops being live here in `handoff` (a lost claim, a return) blends from where it
//! was shown to the new authority's stream; a held one is recorded and posed for replays like a
//! `simulated` one, and kept at its quantised position and rotation (`snap_held`).
//! The decision is re-applied every tick (`gate_and_track`), so an owner change or a mode change moves
//! the simulation; a body that thaws gets its velocities back (Godot drops them while it is frozen) and
//! is woken. The first decision on the peer whose game made the node (every entity on the server, a
//! client's own spawns) never thaws: a body the scene or a script froze stays frozen until the game
//! unfreezes it, as in single-player (WP19 slice B's thrown balls wait frozen in the air).
//!
//! **`simulated`** (client): each live replica has a [`Corrector`] (netcore `correct`). The snapshot
//! values of its body part's `position`, `rotation` / `quaternion`, `linear_velocity` and
//! `angular_velocity` go to the corrector instead of the node (`apply_states`); the other fields are
//! written as usual. Each tick, before anything simulates, `correct_simulated` nudges or snaps the body;
//! after the tick `record_simulated` stores its pose under the predicted tick. Around a rollback replay
//! the simulated bodies are saved and put back (`save_simulated` / `restore_simulated`), so impulses a
//! replayed `_physics_process` applies to them do not count twice.

use super::*;
use crate::net::Net;
use godot::classes::{
    physics_server_2d, physics_server_3d, PhysicsServer2D, PhysicsServer3D, ProjectSettings,
    RigidBody2D, RigidBody3D,
};
use netcore::correct::{self, Component, Corrector, Pose, Step, Tuning};
use netcore::PhysicsMode as Mode;

/// `NetEntity.physics` and `NetManager.physics`: who simulates the entity's RigidBody parts.
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum PhysicsOption {
    /// The manager's `physics`, else the project setting `gdnet/physics/mode` (default `server`).
    #[default]
    Default = 0,
    /// The server simulates; the others show it frozen and interpolated.
    Server = 1,
    /// The entity's owner simulates (`client_owned` for bodies); the others, the server included,
    /// show it frozen and interpolated.
    Owner = 2,
    /// Whoever interacts with it simulates it: a client whose player or bodies push it claims it, and
    /// it goes back to the server when it rests (handoff.rs). The others show it frozen.
    Handoff = 3,
    /// Every peer simulates; the server's snapshots correct the clients' bodies.
    Simulated = 4,
}

impl PhysicsOption {
    pub fn mode(self) -> Option<Mode> {
        match self {
            PhysicsOption::Default => None,
            PhysicsOption::Server => Some(Mode::Server),
            PhysicsOption::Owner => Some(Mode::Owner),
            PhysicsOption::Handoff => Some(Mode::Handoff),
            PhysicsOption::Simulated => Some(Mode::Simulated),
        }
    }

    pub fn of_mode(m: Mode) -> Self {
        match m {
            Mode::Server => PhysicsOption::Server,
            Mode::Owner => PhysicsOption::Owner,
            Mode::Handoff => PhysicsOption::Handoff,
            Mode::Simulated => PhysicsOption::Simulated,
        }
    }

    /// From a `configure()` / `set_physics` value: a name (`"default"`, `"server"`, `"owner"`,
    /// `"handoff"`, `"simulated"`) or the enum's integer.
    pub fn from_variant(v: &Variant) -> Option<Self> {
        if let Ok(i) = v.try_to::<i64>() {
            return Some(match i {
                0 => PhysicsOption::Default,
                1 => PhysicsOption::Server,
                2 => PhysicsOption::Owner,
                3 => PhysicsOption::Handoff,
                4 => PhysicsOption::Simulated,
                _ => return None,
            });
        }
        let s = v.to_string();
        if s.trim().eq_ignore_ascii_case("default") {
            return Some(PhysicsOption::Default);
        }
        Mode::parse(&s).map(PhysicsOption::of_mode)
    }
}

pub const SETTING_MODE: &str = "gdnet/physics/mode";
pub const SETTING_POSITION: &str = "gdnet/physics/correction_position";
pub const SETTING_ANGLE: &str = "gdnet/physics/correction_angle";
pub const SETTING_DECAY: &str = "gdnet/physics/correction_decay";
pub const SETTING_REST_SPEED: &str = "gdnet/physics/correction_rest_speed";
/// Meta on an entity root without a `NetEntity`: its `physics` option (`configure()`).
pub const META_PHYSICS: &str = "_net_physics";

fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name).then(|| ps.get_setting(name))
}

fn number(name: &str) -> Option<f64> {
    setting(name).and_then(|v| crate::profiles::number(&v))
}

/// `gdnet/physics/mode` (a name or 0..3 = server, owner, handoff, simulated), default `server`.
pub fn project_mode() -> Mode {
    match setting(SETTING_MODE) {
        Some(v) => match v.try_to::<i64>() {
            Ok(i) => Mode::from_index(i),
            Err(_) => Mode::parse(&v.to_string()).unwrap_or_else(|| {
                godot_error!(
                    "gdnet: {SETTING_MODE} = {v} is not a physics mode (server, owner, handoff, simulated); using server"
                );
                Mode::Server
            }),
        },
        None => Mode::Server,
    }
}

/// The correction tuning for bodies whose lengths are `scale` times metres (100 in 2D, 1 in 3D).
/// Settings (read at each use, so a change applies at once): `correction_position` (metres, default
/// 0.02), `correction_angle` (degrees, default 3), `correction_decay` (the fraction of a correction
/// left after one 60 Hz tick, default 0.85), `correction_rest_speed` (metres per second below which a
/// server sample counts as at rest, default 0.15, above Godot's own sleep threshold); a snap beyond `gdnet/interpolation/teleport_distance`
/// (default 2 m).
pub fn tuning(scale: f64) -> Tuning {
    let mut t = Tuning::defaults(scale);
    if let Some(p) = number(SETTING_POSITION).filter(|p| *p >= 0.0) {
        t.position = p * scale;
    }
    if let Some(a) = number(SETTING_ANGLE).filter(|a| *a >= 0.0) {
        t.angle = a.to_radians();
    }
    if let Some(d) = number(SETTING_DECAY) {
        t.decay = d.clamp(0.0, 1.0);
    }
    if let Some(r) = number(SETTING_REST_SPEED).filter(|r| *r >= 0.0) {
        t.rest_speed = r * scale;
    }
    let tp = crate::interp::teleport_distance();
    if tp > 0.0 {
        t.teleport = tp * scale;
    }
    t
}

/// The option an entity root declares: its `NetEntity`'s, else the `_net_physics` meta.
fn declared(root: &Gd<Node>) -> PhysicsOption {
    if let Some(ne) = net_entity_of(root) {
        return ne.bind().get_physics();
    }
    if root.has_meta(META_PHYSICS) {
        return PhysicsOption::from_variant(&root.get_meta(META_PHYSICS)).unwrap_or_default();
    }
    PhysicsOption::Default
}

/// Whether any part is a RigidBody (2D or 3D, subclasses included).
pub(super) fn has_body(nodes: &[Option<Gd<Node>>]) -> bool {
    nodes
        .iter()
        .flatten()
        .any(|n| n.is_instance_valid() && (n.is_class("RigidBody2D") || n.is_class("RigidBody3D")))
}

/// Who has a live (unfrozen) body: see the module docs.
pub(super) fn body_live(
    is_server: bool,
    local: u32,
    owner: u32,
    client_owned: bool,
    mode: Mode,
) -> bool {
    if is_server {
        !(client_owned && owner != netcore::SERVER)
    } else {
        (client_owned && owner == local) || mode == Mode::Simulated
    }
}

/// Freezes or thaws the RigidBody parts. A thawed body gets back the velocities its properties hold
/// (the last replicated or simulated ones) and is woken; a frozen 2D body's collider follows its
/// properties again (see below). The caller holds its base guard.
fn set_bodies_live(nodes: &[Option<Gd<Node>>], live: bool) {
    for n in nodes.iter().flatten() {
        if !n.is_instance_valid() {
            continue;
        }
        if let Ok(mut b) = n.clone().try_cast::<RigidBody2D>() {
            if live && b.is_freeze_enabled() {
                let (lv, av) = (b.get_linear_velocity(), b.get_angular_velocity());
                b.set_freeze_enabled(false);
                b.set_linear_velocity(lv);
                b.set_angular_velocity(av);
                b.set_sleeping(false);
            } else if !live && !b.is_freeze_enabled() {
                b.set_freeze_enabled(true);
                // Godot 4.6: once a `RigidBody2D` that simulated is frozen, writes to its local
                // `position` / `rotation` no longer reach the physics server (its collider stays where
                // it froze, and a later thaw jumps back there) until its global transform is set once.
                // Interpolation, body prediction and applied owner states write the local properties.
                let t = b.get_global_transform();
                b.set_global_transform(t);
            }
        } else if let Ok(mut b) = n.clone().try_cast::<RigidBody3D>() {
            if live && b.is_freeze_enabled() {
                let (lv, av) = (b.get_linear_velocity(), b.get_angular_velocity());
                b.set_freeze_enabled(false);
                b.set_linear_velocity(lv);
                b.set_angular_velocity(av);
                b.set_sleeping(false);
            } else if !live && !b.is_freeze_enabled() {
                b.set_freeze_enabled(true);
            }
        }
    }
}

/// Hands a RigidBody node's global transform to the physics server now (a frozen body's collider
/// otherwise follows a property write only after the tick's `_physics_process` calls).
pub(super) fn push_transform(n: &Gd<Node>) {
    if !n.is_instance_valid() {
        return;
    }
    if let Ok(b) = n.clone().try_cast::<RigidBody2D>() {
        PhysicsServer2D::singleton().body_set_state(
            b.get_rid(),
            physics_server_2d::BodyState::TRANSFORM,
            &b.get_global_transform().to_variant(),
        );
    } else if let Ok(b) = n.clone().try_cast::<RigidBody3D>() {
        PhysicsServer3D::singleton().body_set_state(
            b.get_rid(),
            physics_server_3d::BodyState::TRANSFORM,
            &b.get_global_transform().to_variant(),
        );
    }
}

/// A `simulated` replica's body node.
#[derive(Clone)]
enum Body {
    D2(Gd<RigidBody2D>),
    D3(Gd<RigidBody3D>),
}

impl Body {
    fn of(n: &Gd<Node>) -> Option<Self> {
        if let Ok(b) = n.clone().try_cast::<RigidBody2D>() {
            return Some(Body::D2(b));
        }
        n.clone().try_cast::<RigidBody3D>().ok().map(Body::D3)
    }

    fn valid(&self) -> bool {
        match self {
            Body::D2(b) => b.is_instance_valid(),
            Body::D3(b) => b.is_instance_valid(),
        }
    }

    /// Lengths per metre (2D: 100 pixels).
    fn scale(&self) -> f64 {
        match self {
            Body::D2(_) => netcore::interp::PIXELS_PER_METRE,
            Body::D3(_) => 1.0,
        }
    }

    fn pose(&self) -> Pose {
        match self {
            Body::D2(b) => {
                let (p, v) = (b.get_position(), b.get_linear_velocity());
                Pose {
                    pos: [f64::from(p.x), f64::from(p.y), 0.0],
                    rot: correct::quat_of_angle(f64::from(b.get_rotation())),
                    lin: [f64::from(v.x), f64::from(v.y), 0.0],
                    ang: [0.0, 0.0, f64::from(b.get_angular_velocity())],
                }
            }
            Body::D3(b) => {
                let (p, q, v, w) = (
                    b.get_position(),
                    b.get_quaternion(),
                    b.get_linear_velocity(),
                    b.get_angular_velocity(),
                );
                Pose {
                    pos: [f64::from(p.x), f64::from(p.y), f64::from(p.z)],
                    rot: [
                        f64::from(q.x),
                        f64::from(q.y),
                        f64::from(q.z),
                        f64::from(q.w),
                    ],
                    lin: [f64::from(v.x), f64::from(v.y), f64::from(v.z)],
                    ang: [f64::from(w.x), f64::from(w.y), f64::from(w.z)],
                }
            }
        }
    }

    /// Writes the velocities whatever the node's properties hold: an impulse applied since the last
    /// physics sync is in the physics server but not in the properties, so comparing with them can
    /// miss it (a replay's impulses would stay). The caller holds its base guard.
    /// A body still asleep in the physics server took no impulse and is left alone (a write would wake
    /// it).
    fn set_velocities(&mut self, p: &Pose) {
        match self {
            Body::D2(b) => {
                let asleep = PhysicsServer2D::singleton()
                    .body_get_state(b.get_rid(), physics_server_2d::BodyState::SLEEPING)
                    .try_to::<bool>()
                    .unwrap_or(false);
                if asleep {
                    return;
                }
                b.set_linear_velocity(Vector2::new(p.lin[0] as f32, p.lin[1] as f32));
                b.set_angular_velocity(p.ang[2] as f32);
            }
            Body::D3(b) => {
                let asleep = PhysicsServer3D::singleton()
                    .body_get_state(b.get_rid(), physics_server_3d::BodyState::SLEEPING)
                    .try_to::<bool>()
                    .unwrap_or(false);
                if asleep {
                    return;
                }
                let v3 = |a: &[f64; 3]| Vector3::new(a[0] as f32, a[1] as f32, a[2] as f32);
                b.set_linear_velocity(v3(&p.lin));
                b.set_angular_velocity(v3(&p.ang));
            }
        }
    }

    /// Writes the pose (only what changed by more than a float's noise, so a nudge of position alone
    /// leaves the rest of the body's state untouched). A RigidBody's own transform setters do not reach
    /// the physics server while the body simulates (the next sync puts the old transform back), so the
    /// node is moved and then its global transform is handed to the server. The caller holds its base
    /// guard.
    fn set_pose(&mut self, before: &Pose, p: &Pose, wake: bool) {
        let v3 = |a: &[f64; 3]| Vector3::new(a[0] as f32, a[1] as f32, a[2] as f32);
        let moved = |a: &[f64; 3], b: &[f64; 3]| correct::norm(&correct::sub(a, b)) > 1e-9;
        // (A rotation read back from the node differs from the one written by float noise.)
        let turned = correct::norm(&correct::rot_log(&correct::q_mul(
            &p.rot,
            &correct::q_conj(&before.rot),
        ))) > 1e-7;
        let placed = moved(&before.pos, &p.pos) || turned;
        match self {
            Body::D2(b) => {
                if (wake || placed) && b.is_sleeping() {
                    b.set_sleeping(false);
                }
                if moved(&before.pos, &p.pos) {
                    b.set_position(Vector2::new(p.pos[0] as f32, p.pos[1] as f32));
                }
                if turned {
                    b.set_rotation(correct::angle_of_quat(&p.rot) as f32);
                }
                if placed {
                    PhysicsServer2D::singleton().body_set_state(
                        b.get_rid(),
                        physics_server_2d::BodyState::TRANSFORM,
                        &b.get_global_transform().to_variant(),
                    );
                }
                if moved(&before.lin, &p.lin) {
                    b.set_linear_velocity(Vector2::new(p.lin[0] as f32, p.lin[1] as f32));
                }
                if moved(&before.ang, &p.ang) {
                    b.set_angular_velocity(p.ang[2] as f32);
                }
            }
            Body::D3(b) => {
                if (wake || placed) && b.is_sleeping() {
                    b.set_sleeping(false);
                }
                if moved(&before.pos, &p.pos) {
                    b.set_position(v3(&p.pos));
                }
                if turned {
                    let q = p.rot;
                    b.set_quaternion(Quaternion::new(
                        q[0] as f32,
                        q[1] as f32,
                        q[2] as f32,
                        q[3] as f32,
                    ));
                }
                if placed {
                    PhysicsServer3D::singleton().body_set_state(
                        b.get_rid(),
                        physics_server_3d::BodyState::TRANSFORM,
                        &b.get_global_transform().to_variant(),
                    );
                }
                if moved(&before.lin, &p.lin) {
                    b.set_linear_velocity(v3(&p.lin));
                }
                if moved(&before.ang, &p.ang) {
                    b.set_angular_velocity(v3(&p.ang));
                }
            }
        }
    }

    /// Puts a live body to sleep, still. The caller holds its base guard.
    fn sleep(&mut self) {
        match self {
            Body::D2(b) if !b.is_freeze_enabled() => {
                b.set_linear_velocity(Vector2::ZERO);
                b.set_angular_velocity(0.0);
                b.set_sleeping(true);
            }
            Body::D3(b) if !b.is_freeze_enabled() => {
                b.set_linear_velocity(Vector3::ZERO);
                b.set_angular_velocity(Vector3::ZERO);
                b.set_sleeping(true);
            }
            _ => {}
        }
    }

    /// WP19 (settling): freezes the body (`held`), or unfreezes it: with no velocity and asleep (a
    /// settle's end, `wake` None), or awake with `wake`'s velocities (a settle that ended early). The
    /// caller holds its base guard.
    fn set_held(&mut self, held: bool, wake: Option<(&[f64; 3], &[f64; 3])>) {
        let v3 = |a: &[f64; 3]| Vector3::new(a[0] as f32, a[1] as f32, a[2] as f32);
        match self {
            Body::D2(b) => {
                if held {
                    if !b.is_freeze_enabled() {
                        b.set_freeze_enabled(true);
                        // See `set_bodies_live`: a frozen 2D body's collider follows its properties
                        // only once its global transform is set.
                        let t = b.get_global_transform();
                        b.set_global_transform(t);
                    }
                    return;
                }
                b.set_freeze_enabled(false);
                let (lin, ang) = wake.map_or((Vector2::ZERO, 0.0), |(l, a)| {
                    (Vector2::new(l[0] as f32, l[1] as f32), a[2] as f32)
                });
                b.set_linear_velocity(lin);
                b.set_angular_velocity(ang);
                b.set_sleeping(wake.is_none());
            }
            Body::D3(b) => {
                if held {
                    if !b.is_freeze_enabled() {
                        b.set_freeze_enabled(true);
                    }
                    return;
                }
                b.set_freeze_enabled(false);
                let (lin, ang) =
                    wake.map_or((Vector3::ZERO, Vector3::ZERO), |(l, a)| (v3(l), v3(a)));
                b.set_linear_velocity(lin);
                b.set_angular_velocity(ang);
                b.set_sleeping(wake.is_none());
            }
        }
    }
}

/// A `simulated` body a predicted entity of this client touched within this many ticks joins no
/// settle round (the push is in the server's future).
const PUSHED_TICKS: u64 = 30;

/// A held (settling) body waits at its pose at most this many ticks for its manager's other settles.
const SETTLE_WAIT_TICKS: u32 = 30;

/// Which pose component a field of the body part is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Pos,
    Rot,
    Lin,
    Ang,
}

/// One `simulated` replica on a client.
pub(super) struct SimBody {
    body: Body,
    part: u16,
    /// Per field of the body part: the pose component it carries.
    fields: Vec<Option<Which>>,
    corr: Corrector,
    /// WP19: a `handoff` body this client holds: recorded and posed for replays, never corrected (its
    /// holder receives no state for it).
    held: bool,
}

impl SimBody {
    fn component(&self, field: u16, v: &Value) -> Option<Component> {
        let which = self.fields.get(usize::from(field)).copied().flatten()?;
        let f = |x: f32| f64::from(x);
        Some(match (which, v, &self.body) {
            (Which::Pos, Value::Vector2(a), _) => Component::Pos([f(a[0]), f(a[1]), 0.0]),
            (Which::Pos, Value::Vector3(a), _) => Component::Pos([f(a[0]), f(a[1]), f(a[2])]),
            (Which::Rot, Value::Float(a), _) => Component::Rot(correct::quat_of_angle(*a)),
            (Which::Rot, Value::Quaternion(q), _) => {
                Component::Rot([f(q[0]), f(q[1]), f(q[2]), f(q[3])])
            }
            (Which::Lin, Value::Vector2(a), _) => Component::Lin([f(a[0]), f(a[1]), 0.0]),
            (Which::Lin, Value::Vector3(a), _) => Component::Lin([f(a[0]), f(a[1]), f(a[2])]),
            (Which::Ang, Value::Float(a), Body::D2(_)) => Component::Ang([0.0, 0.0, *a]),
            (Which::Ang, Value::Vector3(a), _) => Component::Ang([f(a[0]), f(a[1]), f(a[2])]),
            _ => return None,
        })
    }
}

/// A held body's pose fields: (entity, part, [(field, value)]).
pub type HeldPose = (NetId, u16, Vec<(u16, Value)>);

/// Saved simulated bodies around a replay.
pub struct SavedBodies(Vec<(Body, Pose)>);

impl NetManager {
    /// The server's resolution of an entity's mode and effective `client_owned` flag: (mode, flag).
    pub(super) fn physics_spec(&self, root: &Gd<Node>, explicit: bool, body: bool) -> (Mode, bool) {
        if !body {
            return (Mode::Server, explicit);
        }
        // D21: `client_owned = true` on a body is `owner`, whatever the option says.
        let mode = if explicit {
            Mode::Owner
        } else {
            declared(root)
                .mode()
                .or(self.physics.mode())
                .unwrap_or_else(project_mode)
        };
        // `owner` and `handoff` send `client_owned`: the holder's state is accepted and relayed (for
        // `handoff` the holder is the owner, which moves by itself: handoff.rs).
        (
            mode,
            explicit || mode == Mode::Owner || mode == Mode::Handoff,
        )
    }

    /// Server: the entity rooted at `root` re-resolves its mode (its option, the manager's or the
    /// project's changed) and announces a change.
    pub fn refresh_physics(&mut self, root: &Gd<Node>) {
        if !self.online || !self.is_server {
            return;
        }
        let iid = root.instance_id();
        let Some(e) = self.entries.get(&iid) else {
            return;
        };
        let Some(id) = e.id else {
            return;
        };
        let explicit = net_entity_of(root).is_some_and(|n| n.bind().get_client_owned());
        let body = e.has_body.unwrap_or_else(|| has_body(&e.nodes));
        let (mode, co) = self.physics_spec(root, explicit, body);
        if (mode, co) == (e.physics, e.client_owned) {
            return;
        }
        if let Some(mut s) = self.session() {
            s.bind_mut()
                .with_core(|c, now| c.set_physics(id, mode, co, now));
        }
    }

    /// Server: every entity re-resolves (the manager's default changed).
    pub(super) fn refresh_physics_all(&mut self) {
        let roots: Vec<Gd<Node>> = self
            .order
            .iter()
            .filter_map(|i| Gd::<Node>::try_from_instance_id(*i).ok())
            .collect();
        for r in roots {
            self.refresh_physics(&r);
        }
    }

    /// Any peer: a `SetPhysics` (the server's own included). The bodies take their new role at once,
    /// before the state the change carries is applied, so a `simulated` replica's corrector receives it
    /// (written to the node, it would put the body back at the server's past).
    pub(super) fn on_set_physics(
        &mut self,
        iid: InstanceId,
        mode: Mode,
        client_owned: bool,
        session: &Gd<NetSession>,
    ) {
        let Some(e) = self.entries.get_mut(&iid) else {
            return;
        };
        e.physics = mode;
        self.any_handoff |= mode == Mode::Handoff;
        e.client_owned = client_owned;
        if let Some(id) = e.id {
            self.owned_shown.retain(|k, _| k.0 != id);
            if mode != Mode::Simulated {
                self.sim.remove(&id);
            }
        }
        if let Ok(node) = Gd::<Node>::try_from_instance_id(iid) {
            if !self.is_server {
                mirror_physics(&node, mode);
            }
        }
        // WP18: a mode change is polled at once (and a client-simulated body is never idle).
        self.owner_moved(iid);
        self.apply_body_role(iid, session);
    }

    /// Freezes or thaws the entity's bodies when who simulates them changed. Called each tick from
    /// `gate_and_track` and when a replica is created. On a client it also keeps body prediction's
    /// registry (`NetSession::bodies`, predict_body.rs) in step: a frozen replica is registered (it may
    /// be predicted), a live body (its owner's, every `simulated` one) is not.
    pub(super) fn apply_body_role(&mut self, iid: InstanceId, session: &Gd<NetSession>) {
        let (is_server, local) = (self.is_server, self.local_id);
        let Some(e) = self.entries.get_mut(&iid) else {
            return;
        };
        let body = *e.has_body.get_or_insert_with(|| has_body(&e.nodes));
        if !body {
            return;
        }
        let live = body_live(is_server, local, e.owner, e.client_owned, e.physics);
        if e.body_live == Some(live) {
            return;
        }
        // The first decision on the peer whose game made the node (the server's entities, a client's
        // own spawns) leaves a live body's `freeze` as the game set it: a body the scene or a script
        // froze waits until the game thaws it, as in single-player. gdnet only thaws on a change of who
        // simulates (an owner or mode change) and on a replica created from a `Create`.
        let keep = e.body_live.is_none() && live && e.origin != Origin::Remote;
        // WP19 (handoff): a client's body that stops being simulated here (a lost claim, a return)
        // blends from where it is shown to the new authority's state instead of jumping.
        let blend = !is_server && !live && e.body_live == Some(true) && e.physics == Mode::Handoff;
        e.body_live = Some(live);
        let nodes = e.nodes.clone();
        let id = e.id;
        let handoff = e.physics == Mode::Handoff;
        let simulated = !is_server && live && (e.physics == Mode::Simulated || handoff);
        if let (true, Some(id), Some(rt)) = (blend, id, e.props.clone()) {
            let shown = {
                let _guard = self.base_mut();
                read_hot_indexed(&rt, &nodes)
            };
            let mut s = session.clone();
            s.bind_mut().with_core(|c, _| {
                for (p, f, v) in &shown {
                    c.interp_blend_from(id, *p, *f, v);
                }
            });
        }
        if !keep {
            let _guard = self.base_mut();
            set_bodies_live(&nodes, live);
        }
        if let Some(id) = id {
            if simulated {
                // A held `handoff` body is recorded for replays like a `simulated` one, never corrected.
                self.add_sim(iid, id, handoff);
            } else {
                self.sim.remove(&id);
            }
            if !is_server {
                let rt = self.entries.get(&iid).and_then(|e| e.props.clone());
                let mut s = session.clone();
                let mut s = s.bind_mut();
                match (live, rt) {
                    (false, Some(rt)) => s.bodies.register(id, &nodes, &rt),
                    _ => s.bodies.unregister(id),
                }
            }
        }
    }

    /// Whether this peer shows the entity rooted at `iid` as a frozen body replica gdnet moves (body
    /// prediction's domain): a client's replica whose bodies are frozen here. None when it is not a
    /// spawned entity with a RigidBody part.
    pub(crate) fn shows_frozen_body(&self, iid: InstanceId) -> Option<bool> {
        let e = self.entries.get(&iid).filter(|e| e.id.is_some())?;
        if e.has_body != Some(true) {
            return None;
        }
        Some(!self.is_server && e.body_live == Some(false) && e.predicted.is_none())
    }

    /// Starts correcting a `simulated` replica: its first body part, the fields that carry its pose,
    /// seeded with what it holds now (the server's values from its `Create` or the last snapshot).
    fn add_sim(&mut self, iid: InstanceId, id: NetId, held: bool) {
        let Some(e) = self.entries.get(&iid) else {
            return;
        };
        let Some(rt) = e.props.clone() else {
            return;
        };
        let Some((part, body)) = e
            .nodes
            .iter()
            .enumerate()
            .find_map(|(p, n)| n.as_ref().and_then(Body::of).map(|b| (p, b)))
        else {
            return;
        };
        let names = rt.names.get(part).cloned().unwrap_or_default();
        let fields = names
            .iter()
            .map(|n| match n.as_str() {
                "position" => Some(Which::Pos),
                "rotation" if matches!(body, Body::D2(_)) => Some(Which::Rot),
                "quaternion" => Some(Which::Rot),
                "linear_velocity" => Some(Which::Lin),
                "angular_velocity" => Some(Which::Ang),
                _ => None,
            })
            .collect();
        let mut corr = Corrector::new();
        let pose = {
            let _guard = self.base_mut();
            body.pose()
        };
        corr.seed(pose);
        self.sim.insert(
            id,
            SimBody {
                body,
                part: part as u16,
                fields,
                corr,
                held,
            },
        );
    }

    /// Client: a snapshot value of a `simulated` replica's pose goes to its corrector; true when taken.
    pub(super) fn sim_sample(&mut self, u: &StateUpdate) -> bool {
        let Some(sb) = self.sim.get_mut(&u.id) else {
            return false;
        };
        if u.part != sb.part {
            return false;
        }
        match sb.component(u.field, &u.value) {
            Some(c) => {
                sb.corr.sample(u.tick, c);
                true
            }
            None => false,
        }
    }

    /// Client, each tick before anything simulates: the corrections of the `simulated` replicas.
    /// `now` is the predicted tick, `known` the newest server tick whose state this client has (see
    /// `NetSession`), `dt` the tick length.
    pub fn correct_simulated(&mut self, now: u64, known: u64, dt: f64) {
        if self.sim.is_empty() {
            return;
        }
        self.note_pushed(now);
        let mut sim = std::mem::take(&mut self.sim);
        sim.retain(|_, sb| sb.body.valid());
        let mut tunings: [Option<Tuning>; 2] = [None, None];
        let mut started = (0, 0);
        let mut settled = 0;
        // A settle started this tick: the manager's other resting bodies join it (a pile settles as
        // one).
        let mut round = false;
        let pushed = self.pushed.clone();
        let guard = self.base_mut();
        for (id, sb) in sim.iter_mut().filter(|(_, sb)| !sb.held) {
            let k = usize::from(matches!(sb.body, Body::D3(_)));
            let t = *tunings[k].get_or_insert_with(|| tuning(sb.body.scale()));
            let present = sb.body.pose();
            let before = sb.corr.stats;
            let was = sb.corr.is_settling();
            sb.corr.settle_blocked = pushed.get(id).is_some_and(|t| now < t + PUSHED_TICKS);
            let step = sb.corr.step(&present, now, known, dt, &t);
            round |= !was && sb.corr.is_settling();
            started.0 += sb.corr.stats.corrections - before.corrections;
            started.1 += sb.corr.stats.snaps - before.snaps;
            match step {
                Step::None => {}
                Step::Nudge {
                    dpos,
                    drot,
                    dlin,
                    dang,
                    wake,
                } => {
                    let p = correct::apply_nudge(&present, &dpos, &drot, &dlin, &dang);
                    sb.body.set_pose(&present, &p, wake);
                }
                Step::Snap(p) => sb.body.set_pose(&present, &p, true),
                Step::Hold(p) => {
                    sb.body.set_held(true, None);
                    // A body waiting at its pose is not written again: a node's transform reaches the
                    // physics server at the end of the tick, and wakes the body if it is released by
                    // then.
                    if sb.corr.held_ticks() == 0 {
                        sb.body.set_pose(&present, &p, false);
                    }
                }
                Step::Release { lin, ang } => sb.body.set_held(false, Some((&lin, &ang))),
            }
        }
        if round {
            for (_, sb) in sim.iter_mut().filter(|(id, sb)| {
                !sb.held && !pushed.get(*id).is_some_and(|t| now < t + PUSHED_TICKS)
            }) {
                let k = usize::from(matches!(sb.body, Body::D3(_)));
                let t = *tunings[k].get_or_insert_with(|| tuning(sb.body.scale()));
                let present = sb.body.pose();
                if let Some(Step::Hold(p)) = sb.corr.join_settle(&present, known, dt, &t) {
                    sb.body.set_held(true, None);
                    sb.body.set_pose(&present, &p, false);
                }
            }
        }
        // Settles end together, once none is still carrying its body (or after a wait): the pile is
        // unfrozen, asleep, in the server's own resting arrangement. A body put at its pose this tick
        // waits a tick first: Godot hands a node's new transform to the physics server only at the end
        // of the tick's `_physics_process` calls, and that hand-off wakes a body no longer frozen.
        let placed_now = |sb: &SimBody| sb.corr.is_holding() && sb.corr.held_ticks() == 0;
        let carrying = sim
            .values()
            .any(|sb| sb.corr.is_carrying() || placed_now(sb));
        let mut finished = false;
        for sb in sim.values_mut().filter(|sb| sb.corr.is_holding()) {
            if placed_now(sb) || (carrying && sb.corr.held_ticks() < SETTLE_WAIT_TICKS) {
                continue;
            }
            if sb.corr.finish_settle(now).is_some() {
                // It waits at its pose already (put there a tick or more ago).
                sb.body.set_held(false, None);
                settled += 1;
                finished = true;
            }
        }
        if finished {
            // The bodies the round left alone because they already rest where the server's do (the
            // carried ones' moves woke them) go to sleep with it, or they would wake it again; and
            // every settled body is put to sleep once more after all of them are unfrozen.
            for sb in sim.values_mut().filter(|sb| !sb.held) {
                let k = usize::from(matches!(sb.body, Body::D3(_)));
                let t = *tunings[k].get_or_insert_with(|| tuning(sb.body.scale()));
                if !sb.corr.is_correcting() && sb.corr.at_rest_target(&sb.body.pose(), &t) {
                    sb.body.sleep();
                }
            }
        }
        drop(guard);
        self.sim = sim;
        self.sim_counts.0 += started.0;
        self.sim_counts.1 += started.1;
        self.sim_counts.2 += settled;
    }

    /// Client: the `simulated` replicas this client's predicted entities touched in their last
    /// `move_and_slide()` (`pushed`, by predicted tick).
    fn note_pushed(&mut self, now: u64) {
        let mut hit = Vec::new();
        {
            let _guard = self.base();
            for e in self.entries.values().filter(|e| e.predicted.is_some()) {
                for n in e.nodes.iter().flatten() {
                    let mut colliders = Vec::new();
                    if let Ok(b) = n.clone().try_cast::<godot::classes::CharacterBody2D>() {
                        for i in 0..b.get_slide_collision_count() {
                            colliders
                                .extend(b.get_slide_collision(i).and_then(|c| c.get_collider()));
                        }
                    } else if let Ok(b) = n.clone().try_cast::<godot::classes::CharacterBody3D>() {
                        for i in 0..b.get_slide_collision_count() {
                            colliders
                                .extend(b.get_slide_collision(i).and_then(|c| c.get_collider()));
                        }
                    }
                    for c in colliders {
                        let Ok(node) = c.try_cast::<Node>() else {
                            continue;
                        };
                        let Some((root, _)) = entity_root_of(&node) else {
                            continue;
                        };
                        let id = self.entries.get(&root.instance_id()).and_then(|e| e.id);
                        if let Some(id) = id.filter(|id| self.sim.contains_key(id)) {
                            hit.push(id);
                        }
                    }
                }
            }
        }
        for id in hit {
            self.pushed.insert(id, now);
        }
        self.pushed.retain(|_, t| now < *t + PUSHED_TICKS);
    }

    /// Client, after the tick: each `simulated` replica's pose under predicted tick `tick`.
    pub fn record_simulated(&mut self, tick: u64, dt: f64) {
        if self.sim.is_empty() {
            return;
        }
        let mut sim = std::mem::take(&mut self.sim);
        {
            let _guard = self.base_mut();
            for sb in sim.values_mut() {
                if sb.body.valid() {
                    let p = sb.body.pose();
                    sb.corr.record(tick, p, dt);
                }
            }
        }
        self.sim = sim;
    }

    /// WP19 (handoff), client: the pose fields of each `handoff` body it holds, as (id, part, [(field,
    /// value)]), for netcore to quantise (`snap_held`).
    pub fn held_values(&self) -> Vec<HeldPose> {
        let _guard = self.base();
        let mut out = Vec::new();
        for (id, sb) in self.sim.iter().filter(|(_, s)| s.held && s.body.valid()) {
            let p = sb.body.pose();
            let d3 = matches!(sb.body, Body::D3(_));
            let v2 = |a: &[f64; 3]| Value::Vector2([a[0] as f32, a[1] as f32]);
            let v3 = |a: &[f64; 3]| Value::Vector3([a[0] as f32, a[1] as f32, a[2] as f32]);
            let vals = sb
                .fields
                .iter()
                .enumerate()
                .filter_map(|(f, w)| {
                    let v = match (w.as_ref()?, d3) {
                        (Which::Pos, false) => v2(&p.pos),
                        (Which::Pos, true) => v3(&p.pos),
                        (Which::Rot, false) => Value::Float(correct::angle_of_quat(&p.rot)),
                        (Which::Rot, true) => Value::Quaternion(p.rot.map(|x| x as f32)),
                        // Velocities stay as simulated: an impulse this tick's scripts applied is in
                        // the physics server already, not yet in the node's properties.
                        (Which::Lin | Which::Ang, _) => return None,
                    };
                    Some((f as u16, v))
                })
                .collect();
            out.push((*id, sb.part, vals));
        }
        out
    }

    /// WP19 (handoff), client: puts each held body at its position and rotation as the server will
    /// receive them (quantised as its fields are sent), so the server's copy, and the copy of this client's player there, meet
    /// the same body this client's player meets (a difference of a quantiser step decides whether a
    /// player touches a body, and so whether its prediction holds).
    pub fn snap_held(&mut self, snapped: Vec<(NetId, Vec<(u16, Value)>)>) {
        let mut sim = std::mem::take(&mut self.sim);
        let guard = self.base_mut();
        for (id, vals) in snapped {
            let Some(sb) = sim.get_mut(&id).filter(|s| s.held && s.body.valid()) else {
                continue;
            };
            let present = sb.body.pose();
            let mut p = present;
            for (f, v) in &vals {
                match sb.component(*f, v) {
                    Some(Component::Pos(x)) => p.pos = x,
                    Some(Component::Rot(q)) => p.rot = q,
                    Some(Component::Lin(x)) => p.lin = x,
                    Some(Component::Ang(x)) => p.ang = x,
                    None => {}
                }
            }
            sb.body.set_pose(&present, &p, false);
        }
        drop(guard);
        self.sim = sim;
    }

    /// (corrections started, snaps, bodies being corrected now, simulated replicas).
    pub fn sim_stats(&self) -> (u64, u64, usize, usize) {
        let busy = self.sim.values().filter(|s| s.corr.is_correcting()).count();
        let n = self.sim.values().filter(|s| !s.held).count();
        (self.sim_counts.0, self.sim_counts.1, busy, n)
    }

    /// Settles finished (bodies put at their server body's resting pose, asleep).
    pub fn sim_settles(&self) -> u64 {
        self.sim_counts.2
    }

    /// Before a replay: the simulated bodies' poses.
    pub fn save_simulated(&mut self) -> SavedBodies {
        let bodies: Vec<Body> = self
            .sim
            .values()
            .filter(|s| s.body.valid())
            .map(|s| s.body.clone())
            .collect();
        let _guard = self.base_mut();
        SavedBodies(
            bodies
                .into_iter()
                .map(|b| {
                    let p = b.pose();
                    (b, p)
                })
                .collect(),
        )
    }

    /// After a replay: puts them back.
    pub fn restore_simulated(&mut self, saved: SavedBodies) {
        let _guard = self.base_mut();
        for (mut b, p) in saved.0 {
            if b.valid() {
                let now = b.pose();
                b.set_pose(&now, &p, false);
                // The replayed scripts' impulses are in the physics server only.
                b.set_velocities(&p);
            }
        }
    }

    /// During a replay, before replayed tick `tick`: each simulated body takes the pose and velocities
    /// it had at that tick (its recorded history), so a replayed player meets it, and rides it, as it
    /// was then. `restore_simulated` puts the present back.
    pub fn pose_simulated_at(&mut self, tick: u64) {
        let work: Vec<(Body, Pose)> = self
            .sim
            .values()
            .filter(|s| s.body.valid())
            .filter_map(|s| {
                // A held `handoff` body is posed at its nearest recorded pose: a replay may reach back
                // before its claim (it was a frozen replica then) or across a gap in its history.
                let p = if s.held {
                    s.corr.recorded_pose_or_nearest(tick)
                } else {
                    s.corr.recorded_pose(tick)
                };
                p.map(|p| (s.body.clone(), p))
            })
            .collect();
        if work.is_empty() {
            return;
        }
        let _guard = self.base_mut();
        for (mut b, p) in work {
            let now = b.pose();
            b.set_pose(&now, &p, false);
            b.set_velocities(&p);
        }
    }

    /// The overlay's mode of an entity: its name ("" for an entity without a body).
    pub(super) fn physics_name(e: &Entry) -> &'static str {
        if e.has_body == Some(true) {
            e.physics.name()
        } else {
            ""
        }
    }
}

/// On a client, a body entity's `NetEntity.physics` shows the replicated mode.
fn mirror_physics(root: &Gd<Node>, mode: Mode) {
    if let Some(mut ne) = net_entity_of(root) {
        ne.bind_mut()
            .set_physics_quiet(PhysicsOption::of_mode(mode));
    }
}

/// Writes an option to the entity root (its `NetEntity` or the meta).
pub fn store_physics(root: &Gd<Node>, opt: PhysicsOption) {
    match net_entity_of(root) {
        Some(mut ne) => ne.bind_mut().set_physics_quiet(opt),
        None => root
            .clone()
            .set_meta(META_PHYSICS, &(opt as i64).to_variant()),
    }
}

/// The entity `node` belongs to re-resolves its mode now (server).
pub fn apply_live(node: &Gd<Node>) {
    if let Some((root, mut m)) = entity_root_of(node) {
        m.bind_mut().refresh_physics(&root);
    }
}

#[godot_api(secondary)]
impl Net {
    /// WP19: sets who simulates the RigidBody parts of the entity `node` belongs to: `"server"`,
    /// `"owner"`, `"handoff"`, `"simulated"` or `"default"` (the manager's, else the project's), or
    /// the `NetEntity.PHYSICS_*` value. On the server it reaches every peer at the next tick; on a
    /// client it only changes the local option (the server decides).
    #[func]
    fn set_physics(&self, node: Gd<Node>, mode: Variant) {
        let Some(opt) = PhysicsOption::from_variant(&mode) else {
            godot_error!(
                "Net.set_physics: {mode} is not a physics mode (default, server, owner, handoff, simulated)"
            );
            return;
        };
        let root = entity_root_of(&node).map_or(node.clone(), |(r, _)| r);
        store_physics(&root, opt);
        apply_live(&root);
    }

    /// WP19: the physics mode this peer holds for the entity `node` belongs to (`"server"`, `"owner"`,
    /// `"handoff"`, `"simulated"`), or `""` when it is not a spawned entity with a RigidBody part.
    #[func]
    fn physics_of(&self, node: Gd<Node>) -> GString {
        let Some((root, m)) = entity_root_of(&node) else {
            return GString::new();
        };
        let m = m.bind();
        let name = m
            .entries
            .get(&root.instance_id())
            .filter(|e| e.id.is_some())
            .map_or("", NetManager::physics_name);
        GString::from(name)
    }
}

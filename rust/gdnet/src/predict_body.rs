//! WP19 body prediction, the gdnet side (docs/PLAN_NETCODE.md 12.4 "Body prediction", gate G13, D26,
//! D27; the arithmetic is netcore's `reckon` module).
//!
//! **What it is.** A client shows a frozen `RigidBody2D/3D` replica one jitter buffer in the past
//! (WP5's interpolation). `body_prediction` chooses what it shows instead: `interpolate` (the default,
//! D27), `dead_reckoning` (the newest snapshot carried forward to the present by its velocities,
//! gravity and damping) or `custom` (the same, advanced by the body script's
//! `_net_predict_body(delta)`). It is a display choice of the client: nothing changes on the wire or on
//! the server.
//!
//! **Which bodies.** An entity whose root is a `RigidBody2D/3D` and that this client shows as a frozen
//! replica: [`shown_frozen`] is the one predicate (today: netcore interpolates its `position`, i.e. it
//! is a replica this peer neither owns nor predicts, and not a `simulated` one, which netcore never
//! interpolates). The registry follows the physics modes (modes.rs `apply_body_role`): a replica
//! registers when its bodies are frozen on this client (at its `Create`, or when an owner or mode change
//! freezes them) and unregisters when this client starts simulating it (its own `owner` body, a
//! `simulated` one) or it is destroyed.
//!
//! **Resolution** ([`resolve`]): the entity's own setting (`Net.configure(node, {"body_prediction":
//! ...})`, else its `NetEntity.body_prediction`), else `custom` when the root's script defines
//! `_net_predict_body`, else `NetManager.body_prediction`, else the project setting
//! `gdnet/physics/body_prediction` (default `interpolate`). `custom` without a hook is
//! `dead_reckoning`. A change to any of them applies at the next rendered frame (setters bump a
//! generation; the project setting is read once per frame while bodies are registered).
//!
//! **Each rendered frame** (`NetSession::_process`, client sessions, before the interpolated writes):
//! the bodies that received a snapshot since the last frame (netcore's interpolation records them only
//! while some body is predicted) join the active set; for each active body the newest samples of its
//! `position`, `rotation` / `quaternion`, `linear_velocity` and `angular_velocity` are its base, the
//! horizon is the estimated present server tick minus the base's tick, and `reckon::BodyTrack` steps
//! it (one tick at a time, incrementally; a new snapshot re-bases it and turns the disagreement into a
//! decaying offset). The shown pose is written to the body (so its collider is there too) and
//! interpolation's writes of those fields are dropped. A body at rest (its newest velocities zero, as a
//! sleeping body's are) is shown at its newest snapshot and leaves the active set once its offset has
//! decayed and the render tick passed that snapshot; interpolation then shows the same value. A
//! sleeping pile therefore costs nothing per frame.
//!
//! **The present** is estimated on the client's physics clock (`Engine.get_physics_frames()` plus the
//! physics interpolation fraction), because the server's bodies change once per physics tick: the
//! leading edge of the snapshot arrivals in physics ticks (`reckon::Edge`), plus half the smallest
//! recent round trip counted in the session's steps (`Session::min_rtt_steps`: the lower envelope pairs
//! with the leading edge, and counted on the clock the arrivals are, it waits for the next physics tick
//! at each end as a snapshot does; a round trip in milliseconds has those waits only in part, and the
//! part changed with the phase of the frames from run to run: the estimate lagged by 0.23 to 0.55 of a
//! tick; before the first step sample the round trip in milliseconds stands in), aimed at the middle of the
//! server's current tick (`reckon::PRESENT_CENTRE`), so a continuously moving replica is within half a
//! tick of travel of the server's stepwise present plus the estimate's error. Before the first
//! observation the render timeline's wall-clock edge stands in.
//!
//! **The cap** (`gdnet/physics/predict_max_ms`) holds at each re-base: when the newest snapshot is older
//! than the cap, the shown moment lags the present by the excess (kept until the next snapshot); between
//! snapshots the horizon runs on with the clock so the motion stays smooth, up to the cap plus 0.1 s
//! when snapshots stop.
//!
//! **Starting.** A body that starts moving (its first snapshot after resting, or its first at all)
//! starts at the prediction: it jumps ahead by its velocity times the horizon once, since the
//! interpolated pose was only late. A body whose mode changes to a predicted one while it moves blends
//! from the interpolated pose through the offset.
//!
//! **Forces** ([`forces_of`]): the space's default gravity and damping (`PhysicsServer2D/3D.
//! area_get_param` on the world's space, i.e. the project's `physics/2d|3d/default_*` unless a script
//! changed them), the body's `gravity_scale` and `linear_damp(_mode)` / `angular_damp(_mode)` (scene
//! values of the replica: the default profiles replicate only the pose and velocities), the engine's
//! damping order (Jolt: gravity first). `Area2D/3D` gravity and damping overrides are not applied (a
//! frozen body is not in the engine's area computation, and querying areas per step costs more than the
//! feature is worth; `custom` covers it). A body with `custom_integrator` gets no gravity or damping.
//! Gravity is applied in the parent's frame as if the parent were not rotated. A body whose two newest
//! velocity samples changed by less than half of what gravity would have done is supported (it rests
//! or slides on something) and its prediction leaves gravity out (`reckon::supported`), so a crate
//! sliding on a floor does not sink into it.
//!
//! **The hook** (`custom`): before each call the body's `position`, `rotation` / `quaternion`,
//! `linear_velocity` and `angular_velocity` hold the state to advance (at a re-base: the newest
//! snapshot, with the body's other interpolated properties also set to their newest values); the hook
//! changes them as its own movement code would over `delta` seconds; gdnet reads them back. Calls are
//! incremental: each whole tick of the horizon once (`delta` = one tick), plus one call per frame with
//! the fraction of a tick (its effect on the four properties is not kept). Per body and frame that is
//! one fractional call plus the whole ticks that passed (about one at 60 fps), plus, on a frame where a
//! snapshot arrived, the old prediction's remaining ticks and the new one's whole horizon: at most
//! `2 * ceil(max horizon / tick) + ceil(frame time / tick) + 2` calls (max horizon: the cap plus 0.1
//! s), about 1-2 on most frames and 5-7 on a snapshot's frame over a 100 ms link at 60 Hz; measured
//! 3.5 per frame on average at 145 fps with a 60 ms horizon (500 a second). `Net.dead_reckon(body, delta)` is the
//! built-in step, usable from the hook (inside a hook it uses the forces gdnet resolved, gravity left
//! out for a supported body).
//!
//! **Settings** (read when used): `gdnet/physics/body_prediction` (`interpolate`), `predict_max_ms`
//! (250), and the three settings shared with `simulated` corrections, read by one reader (modes.rs
//! `tuning`, [`correction_settings`]): `correction_position` (metres, 0.02: 2 px in 2D at 100 px per
//! metre), `correction_angle` (3 degrees), `correction_decay` (0.85 per 60 Hz frame). The teleport
//! thresholds are the position and rotation fields' interpolation ones (`teleport_distance`, WP5).
//!
//! **Stats**: `predicted_bodies` (moving bodies predicted in the last frame), `prediction_replicas`
//! (the frozen body replicas registered, predicted or not), `prediction_corrections`
//! and `prediction_corrections_per_s` (snapshots that disagreed beyond the thresholds),
//! `prediction_horizon_ms` (the mean horizon over the last frame's predicted bodies).

use crate::infer::RuntimeProps;
use crate::manager::{entity_root_of, net_entity_of};
use crate::net::Net;
use crate::session::NetSession;
use godot::classes::{
    physics_server_2d, physics_server_3d, rigid_body_2d, rigid_body_3d, Engine, PhysicsServer2D,
    PhysicsServer3D, ProjectSettings, RigidBody2D, RigidBody3D, Time,
};
use godot::prelude::*;
use netcore::reckon::{self, BodyTrack, Forces, Motion, Rebased, Thresholds};
use netcore::{NetId, Value};
use std::cell::Cell;
use std::collections::HashMap;

pub const SETTING_MODE: &str = "gdnet/physics/body_prediction";
pub const SETTING_MAX_MS: &str = "gdnet/physics/predict_max_ms";
pub const DEFAULT_MAX_MS: f64 = 250.0;
/// The root meta `Net.configure(node, {"body_prediction": ...})` stores.
pub const META_BODY_PREDICTION: &str = "_net_body_prediction";
/// The hook a body script may define.
pub const HOOK: &str = "_net_predict_body";

/// `body_prediction` on `NetEntity` and `NetManager` (12.4).
#[derive(GodotConvert, Var, Export, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[godot(via = i64)]
pub enum BodyPrediction {
    /// Not set here: the next level decides (entity, manager, project).
    #[default]
    Default = 0,
    /// The server's past, between two snapshots (v1's behaviour).
    Interpolate = 1,
    /// The newest snapshot carried forward to the present.
    DeadReckoning = 2,
    /// Like `dead_reckoning`, advanced by the script's `_net_predict_body(delta)`.
    Custom = 3,
}

impl BodyPrediction {
    pub fn parse(v: &Variant) -> Option<Self> {
        if let Some(n) = crate::profiles::number(v) {
            return Self::from_index(n.round() as i64);
        }
        match v.to_string().to_lowercase().as_str() {
            "default" | "" => Some(Self::Default),
            "interpolate" => Some(Self::Interpolate),
            "dead_reckoning" => Some(Self::DeadReckoning),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn from_index(i: i64) -> Option<Self> {
        Some(match i {
            0 => Self::Default,
            1 => Self::Interpolate,
            2 => Self::DeadReckoning,
            3 => Self::Custom,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Interpolate => "interpolate",
            Self::DeadReckoning => "dead_reckoning",
            Self::Custom => "custom",
        }
    }

    fn explicit(self) -> Option<Self> {
        (self != Self::Default).then_some(self)
    }
}

thread_local! {
    /// Bumped by every setter of a `body_prediction`; registered bodies resolve again when it moved.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// While a hook runs: the body and the forces gdnet resolved for it (`Net.dead_reckon` uses them).
    static IN_HOOK: Cell<Option<(InstanceId, Forces)>> = const { Cell::new(None) };
}

/// A `body_prediction` changed somewhere: registered bodies resolve again at the next frame.
pub fn changed() {
    GENERATION.with(|g| g.set(g.get() + 1));
}

fn generation() -> u64 {
    GENERATION.with(Cell::get)
}

fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name)
        .then(|| ps.get_setting(name))
        .filter(|v| !v.is_nil())
}

fn number_setting(name: &str, default: f64) -> f64 {
    setting(name)
        .and_then(|v| crate::profiles::number(&v))
        .unwrap_or(default)
}

/// `gdnet/physics/body_prediction` (default `interpolate`; `default` means `interpolate`).
pub fn project_mode() -> BodyPrediction {
    setting(SETTING_MODE)
        .and_then(|v| BodyPrediction::parse(&v))
        .and_then(BodyPrediction::explicit)
        .unwrap_or(BodyPrediction::Interpolate)
}

/// `gdnet/physics/predict_max_ms` (default 250).
pub fn predict_max_ms() -> f64 {
    number_setting(SETTING_MAX_MS, DEFAULT_MAX_MS).max(0.0)
}

/// The shared correction settings, read as `simulated` corrections read them (modes.rs `tuning`, one
/// reader): (`correction_position` in the body's length unit, i.e. metres times 100 in 2D and metres in
/// 3D; `correction_angle` in radians; `correction_decay` per 60 Hz frame).
pub fn correction_settings(three_d: bool) -> (f64, f64, f64) {
    let scale = if three_d {
        1.0
    } else {
        netcore::interp::PIXELS_PER_METRE
    };
    let t = crate::manager::modes::tuning(scale);
    (t.position, t.angle, t.decay)
}

/// `Net.configure(node, {"body_prediction": value})` on the entity root `root`.
pub fn configure(root: &Gd<Node>, v: &Variant) {
    match BodyPrediction::parse(v) {
        Some(m) => {
            let mut r = root.clone();
            if m == BodyPrediction::Default {
                if r.has_meta(META_BODY_PREDICTION) {
                    r.remove_meta(META_BODY_PREDICTION);
                }
            } else {
                r.set_meta(META_BODY_PREDICTION, &(m as i64).to_variant());
            }
            changed();
        }
        None => godot_error!(
            "Net.configure: body_prediction {v} is not known (default, interpolate, dead_reckoning, custom)"
        ),
    }
}

/// The effective `body_prediction` of the entity `node` belongs to (never `Default`; `custom` only with
/// a hook). See the module docs for the order.
pub fn resolve(node: &Gd<Node>) -> BodyPrediction {
    let (root, manager) = match entity_root_of(node) {
        Some((r, m)) => (r, Some(m)),
        None => (node.clone(), None),
    };
    let hook = root.has_method(HOOK);
    let configured = root
        .has_meta(META_BODY_PREDICTION)
        .then(|| root.get_meta(META_BODY_PREDICTION))
        .and_then(|v| BodyPrediction::parse(&v))
        .and_then(BodyPrediction::explicit);
    let entity = configured
        .or_else(|| net_entity_of(&root).and_then(|ne| ne.bind().get_body_prediction().explicit()));
    let mode = match entity {
        Some(m) => m,
        None if hook => BodyPrediction::Custom,
        None => manager
            .and_then(|m| m.bind().get_body_prediction().explicit())
            .unwrap_or_else(project_mode),
    };
    if mode == BodyPrediction::Custom && !hook {
        BodyPrediction::DeadReckoning
    } else {
        mode
    }
}

/// Whether this client shows entity `id` as a frozen replica gdnet moves: netcore interpolates its
/// position, so it is neither owned (an `owner` body's owner simulates it), nor predicted (WP4), nor
/// `simulated` (a live body the snapshots correct, modes.rs).
pub fn shown_frozen(core: &netcore::Session, id: NetId, position_field: u16) -> bool {
    core.is_interpolated(id, 0, position_field)
}

/// The gravity, damping and engine order a body's prediction uses (see the module docs).
pub fn forces_of(node: &Gd<Node>) -> Forces {
    if let Ok(b) = node.clone().try_cast::<RigidBody2D>() {
        if b.is_using_custom_integrator() {
            return Forces::default();
        }
        let ps = ProjectSettings::singleton();
        let space = b.get_world_2d().map(|w| w.get_space());
        let server = PhysicsServer2D::singleton();
        let param = |p: physics_server_2d::AreaParameter, setting: &str| -> Variant {
            space
                .map(|s| server.area_get_param(s, p))
                .filter(|v| !v.is_nil())
                .unwrap_or_else(|| ps.get_setting(setting))
        };
        let g = crate::profiles::number(&param(
            physics_server_2d::AreaParameter::GRAVITY,
            "physics/2d/default_gravity",
        ))
        .unwrap_or(980.0);
        let dir = param(
            physics_server_2d::AreaParameter::GRAVITY_VECTOR,
            "physics/2d/default_gravity_vector",
        )
        .try_to::<Vector2>()
        .unwrap_or(Vector2::new(0.0, 1.0));
        let space_lin = crate::profiles::number(&param(
            physics_server_2d::AreaParameter::LINEAR_DAMP,
            "physics/2d/default_linear_damp",
        ))
        .unwrap_or(0.1);
        let space_ang = crate::profiles::number(&param(
            physics_server_2d::AreaParameter::ANGULAR_DAMP,
            "physics/2d/default_angular_damp",
        ))
        .unwrap_or(1.0);
        let scale = f64::from(b.get_gravity_scale());
        let replace = |m: rigid_body_2d::DampMode| m == rigid_body_2d::DampMode::REPLACE;
        let lin = f64::from(b.get_linear_damp());
        let ang = f64::from(b.get_angular_damp());
        return Forces {
            gravity: [
                f64::from(dir.x) * g * scale,
                f64::from(dir.y) * g * scale,
                0.0,
            ],
            linear_damp: if replace(b.get_linear_damp_mode()) {
                lin
            } else {
                lin + space_lin
            },
            angular_damp: if replace(b.get_angular_damp_mode()) {
                ang
            } else {
                ang + space_ang
            },
            damp_after_gravity: false,
        };
    }
    if let Ok(b) = node.clone().try_cast::<RigidBody3D>() {
        if b.is_using_custom_integrator() {
            return Forces::default();
        }
        let ps = ProjectSettings::singleton();
        let space = b.get_world_3d().map(|w| w.get_space());
        let server = PhysicsServer3D::singleton();
        let param = |p: physics_server_3d::AreaParameter, setting: &str| -> Variant {
            space
                .map(|s| server.area_get_param(s, p))
                .filter(|v| !v.is_nil())
                .unwrap_or_else(|| ps.get_setting(setting))
        };
        let g = crate::profiles::number(&param(
            physics_server_3d::AreaParameter::GRAVITY,
            "physics/3d/default_gravity",
        ))
        .unwrap_or(9.8);
        let dir = param(
            physics_server_3d::AreaParameter::GRAVITY_VECTOR,
            "physics/3d/default_gravity_vector",
        )
        .try_to::<Vector3>()
        .unwrap_or(Vector3::new(0.0, -1.0, 0.0));
        let space_lin = crate::profiles::number(&param(
            physics_server_3d::AreaParameter::LINEAR_DAMP,
            "physics/3d/default_linear_damp",
        ))
        .unwrap_or(0.1);
        let space_ang = crate::profiles::number(&param(
            physics_server_3d::AreaParameter::ANGULAR_DAMP,
            "physics/3d/default_angular_damp",
        ))
        .unwrap_or(0.1);
        let scale = f64::from(b.get_gravity_scale());
        let replace = |m: rigid_body_3d::DampMode| m == rigid_body_3d::DampMode::REPLACE;
        let lin = f64::from(b.get_linear_damp());
        let ang = f64::from(b.get_angular_damp());
        let jolt = ps
            .get_setting("physics/3d/physics_engine")
            .to_string()
            .contains("Jolt");
        return Forces {
            gravity: [
                f64::from(dir.x) * g * scale,
                f64::from(dir.y) * g * scale,
                f64::from(dir.z) * g * scale,
            ],
            linear_damp: if replace(b.get_linear_damp_mode()) {
                lin
            } else {
                lin + space_lin
            },
            angular_damp: if replace(b.get_angular_damp_mode()) {
                ang
            } else {
                ang + space_ang
            },
            damp_after_gravity: jolt,
        };
    }
    Forces::default()
}

/// The pose and velocities of a body node (None for anything but a `RigidBody2D/3D`).
fn read_motion(node: &Gd<Node>) -> Option<Motion> {
    if let Ok(b) = node.clone().try_cast::<RigidBody2D>() {
        let p = b.get_position();
        let v = b.get_linear_velocity();
        return Some(Motion::new_2d(
            [f64::from(p.x), f64::from(p.y)],
            f64::from(b.get_rotation()),
            [f64::from(v.x), f64::from(v.y)],
            f64::from(b.get_angular_velocity()),
        ));
    }
    if let Ok(b) = node.clone().try_cast::<RigidBody3D>() {
        let p = b.get_position();
        let q = b.get_quaternion();
        let v = b.get_linear_velocity();
        let w = b.get_angular_velocity();
        return Some(Motion::new_3d(
            [f64::from(p.x), f64::from(p.y), f64::from(p.z)],
            [
                f64::from(q.x),
                f64::from(q.y),
                f64::from(q.z),
                f64::from(q.w),
            ],
            [f64::from(v.x), f64::from(v.y), f64::from(v.z)],
            [f64::from(w.x), f64::from(w.y), f64::from(w.z)],
        ));
    }
    None
}

/// Writes a pose (and, with `velocities`, the velocities) to a body node.
fn write_motion(node: &Gd<Node>, m: &Motion, rotation: bool, velocities: bool) {
    if m.three_d {
        let Ok(mut b) = node.clone().try_cast::<RigidBody3D>() else {
            return;
        };
        b.set_position(Vector3::new(
            m.pos[0] as f32,
            m.pos[1] as f32,
            m.pos[2] as f32,
        ));
        if rotation {
            b.set_quaternion(Quaternion::new(
                m.rot[0] as f32,
                m.rot[1] as f32,
                m.rot[2] as f32,
                m.rot[3] as f32,
            ));
        }
        if velocities {
            b.set_linear_velocity(Vector3::new(
                m.lin[0] as f32,
                m.lin[1] as f32,
                m.lin[2] as f32,
            ));
            b.set_angular_velocity(Vector3::new(
                m.ang[0] as f32,
                m.ang[1] as f32,
                m.ang[2] as f32,
            ));
        }
    } else {
        let Ok(mut b) = node.clone().try_cast::<RigidBody2D>() else {
            return;
        };
        b.set_position(Vector2::new(m.pos[0] as f32, m.pos[1] as f32));
        if rotation {
            b.set_rotation(m.rot[0] as f32);
        }
        if velocities {
            b.set_linear_velocity(Vector2::new(m.lin[0] as f32, m.lin[1] as f32));
            b.set_angular_velocity(m.ang[0] as f32);
        }
    }
}

/// `Net.dead_reckon(body, delta)`: advances the body's pose and velocities by the built-in step.
pub fn dead_reckon_node(node: &Gd<Node>, delta: f64) {
    let Some(mut m) = read_motion(node) else {
        godot_error!("Net.dead_reckon: {node} is not a RigidBody2D or RigidBody3D");
        return;
    };
    let iid = node.instance_id();
    let forces = IN_HOOK
        .with(Cell::get)
        .filter(|(i, _)| *i == iid)
        .map_or_else(|| forces_of(node), |(_, f)| f);
    reckon::step(&mut m, &forces, delta);
    write_motion(node, &m, true, true);
}

/// The fields of a registered body: indices on part 0 of the four the prediction uses, and every
/// interpolated field of part 0 (with its accessor) for `custom`.
#[derive(Clone)]
struct BodyFields {
    pos: u16,
    rot: Option<u16>,
    lin: Option<u16>,
    ang: Option<u16>,
    interp: Vec<(u16, crate::infer::RtField)>,
}

impl BodyFields {
    fn of(rt: &RuntimeProps, three_d: bool) -> Option<Self> {
        let names = rt.names.first()?;
        let fields = rt.fields.first()?;
        let find = |want: &str| {
            names
                .iter()
                .position(|n| n == want)
                .filter(|i| fields.get(*i).is_some_and(|f| f.interp))
                .map(|i| i as u16)
        };
        Some(Self {
            pos: find("position")?,
            rot: find(if three_d { "quaternion" } else { "rotation" }),
            lin: find("linear_velocity"),
            ang: find("angular_velocity"),
            interp: fields
                .iter()
                .enumerate()
                .filter(|(_, f)| f.interp)
                .map(|(i, f)| (i as u16, f.clone()))
                .collect(),
        })
    }

    fn pose_fields(&self) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(self.pos).chain([self.rot, self.lin, self.ang].into_iter().flatten())
    }
}

struct Reg {
    root: Gd<Node>,
    three_d: bool,
    fields: BodyFields,
    mode: BodyPrediction,
}

#[derive(Default)]
struct Active {
    track: Option<BodyTrack>,
    /// Ticks the shown moment lags the present by (the excess over the cap at the last re-base).
    lag: f64,
    /// Activated by a change of mode: the first prediction blends from the interpolated pose.
    blend: bool,
}

/// One active body's inputs for a frame, read from netcore.
struct Gathered {
    id: NetId,
    base: (u64, Motion),
    prev_lin: Option<(u64, [f64; 3])>,
    teleport: f64,
    teleport_angle: f64,
    /// `custom`: the newest value of every interpolated field of part 0.
    newest: Vec<(crate::infer::RtField, Value)>,
}

/// Every registered body replica of a client session and the active (predicted) ones.
#[derive(Default)]
pub struct Bodies {
    reg: HashMap<NetId, Reg>,
    active: HashMap<NetId, Active>,
    generation: u64,
    project: Option<BodyPrediction>,
    unresolved: bool,
    any_predicted: bool,
    /// Predicted bodies resolved since the last frame (they may already hold a snapshot).
    pending: Vec<NetId>,
    last_us: Option<u64>,
    /// The leading edge of the snapshot arrivals on the physics clock, and the newest tick seen.
    edge: reckon::Edge,
    seen_newest: u64,
    predicted_last: usize,
    horizon_ms_last: f64,
    corrections: u64,
    window: (u64, u64),
    corrections_per_s: u64,
}

impl Bodies {
    /// A replica entity's parts arrived (its `Create` was applied): bodies are remembered; their mode
    /// is resolved at the next frame.
    pub fn register(&mut self, id: NetId, nodes: &[Option<Gd<Node>>], rt: &RuntimeProps) {
        let Some(Some(root)) = nodes.first() else {
            return;
        };
        let three_d = if root.is_class("RigidBody3D") {
            true
        } else if root.is_class("RigidBody2D") {
            false
        } else {
            return;
        };
        let Some(fields) = BodyFields::of(rt, three_d) else {
            return;
        };
        self.reg.insert(
            id,
            Reg {
                root: root.clone(),
                three_d,
                fields,
                mode: BodyPrediction::Interpolate,
            },
        );
        self.active.remove(&id);
        self.unresolved = true;
    }

    /// The entity stopped being a frozen replica here (its owner or mode changed and this peer
    /// simulates it now, or it was destroyed): it is neither predicted nor listed any more.
    pub fn unregister(&mut self, id: NetId) {
        self.reg.remove(&id);
        self.active.remove(&id);
        self.pending.retain(|p| *p != id);
        self.any_predicted = self
            .reg
            .values()
            .any(|r| r.mode != BodyPrediction::Interpolate);
    }

    /// The effective mode of a registered entity (overlay rows), None when it is not a registered body.
    pub fn mode_of(&self, id: NetId) -> Option<BodyPrediction> {
        self.reg.get(&id).map(|r| r.mode)
    }

    /// `stats()` keys.
    pub fn stats_into(&self, d: &mut VarDictionary) {
        d.set("predicted_bodies", self.predicted_last as i64);
        // The frozen body replicas registered (the ones body prediction may show).
        d.set(
            "prediction_replicas",
            self.reg
                .values()
                .filter(|r| r.root.is_instance_valid())
                .count() as i64,
        );
        d.set("prediction_corrections", self.corrections as i64);
        d.set(
            "prediction_corrections_per_s",
            self.corrections_per_s as i64,
        );
        d.set("prediction_horizon_ms", self.horizon_ms_last);
    }

    fn resolve_all(&mut self, project: BodyPrediction) {
        self.reg.retain(|_, r| r.root.is_instance_valid());
        self.any_predicted = false;
        for (id, r) in self.reg.iter_mut() {
            let was = r.mode;
            r.mode = resolve(&r.root);
            if r.mode != BodyPrediction::Interpolate {
                self.any_predicted = true;
                if was == BodyPrediction::Interpolate || self.unresolved {
                    self.pending.push(*id);
                }
            }
        }
        self.project = Some(project);
        self.generation = generation();
        self.unresolved = false;
    }
}

impl NetSession {
    /// WP19: one rendered frame of body prediction (see the module docs). `values` are the fields
    /// interpolation sampled this frame; the predicted bodies' pose fields are taken out of it.
    pub(crate) fn predict_bodies(&mut self, values: &mut Vec<((NetId, u16, u16), Value)>) {
        if self.bodies.reg.is_empty() {
            // The last registered body left (destroyed, or this peer simulates it now): nothing is
            // predicted any more, and interpolation stops recording arrivals for it.
            if self.bodies.predicted_last != 0 || self.bodies.any_predicted {
                self.bodies.predicted_last = 0;
                self.bodies.horizon_ms_last = 0.0;
                self.bodies.any_predicted = false;
                self.bodies.active.clear();
                if let Some(core) = self.core.as_mut() {
                    core.interp_mut().set_watch(false);
                }
            }
            return;
        }
        let now = Time::singleton().get_ticks_usec();
        let project = project_mode();
        if self.bodies.unresolved
            || self.bodies.generation != generation()
            || self.bodies.project != Some(project)
        {
            self.bodies.resolve_all(project);
        }
        let dt = self
            .bodies
            .last_us
            .map_or(0.0, |l| now.saturating_sub(l) as f64 / 1e6);
        self.bodies.last_us = Some(now);
        let Some(core) = self.core.as_mut() else {
            return;
        };
        core.interp_mut().set_watch(self.bodies.any_predicted);
        let b = &mut self.bodies;
        if !b.any_predicted && b.active.is_empty() {
            b.predicted_last = 0;
            return;
        }
        if let Some(n) = core.interp().timeline.newest() {
            if n > b.seen_newest {
                b.seen_newest = n;
                b.edge.observe(n, Engine::singleton().get_physics_frames());
            }
        }
        let fresh = core.interp_mut().take_fresh();
        // A body whose mode just changed blends from what interpolation showed; one that received a
        // snapshot after resting (or its first) starts at the prediction.
        let changed: Vec<NetId> = b.pending.drain(..).collect();
        for (id, blend) in changed
            .into_iter()
            .map(|id| (id, true))
            .chain(fresh.into_iter().map(|id| (id, false)))
        {
            if b.reg
                .get(&id)
                .is_some_and(|r| r.mode != BodyPrediction::Interpolate)
            {
                b.active.entry(id).or_insert(Active {
                    blend,
                    ..Default::default()
                });
            }
        }
        if b.active.is_empty() {
            b.predicted_last = 0;
            return;
        }
        // What netcore knows: the newest samples, the present estimate, the render tick.
        let rate = f64::from(core.clock().tick_rate());
        let tick_dt = 1.0 / rate;
        let rtt_ticks = core.min_rtt_steps().map_or_else(
            || {
                core.min_rtt_ms(netcore::SERVER)
                    .or_else(|| core.rtt_ms(netcore::SERVER))
                    .unwrap_or(0.0)
                    / 1000.0
                    * rate
            },
            |s| s as f64,
        );
        // The present on the physics clock (the server's bodies change once per physics tick), with the
        // render timeline's wall-clock edge as the fallback before the first observation.
        let engine = Engine::singleton();
        let local =
            engine.get_physics_frames() as f64 + engine.get_physics_interpolation_fraction();
        let present = b.edge.present(local, rtt_ticks / 2.0).or_else(|| {
            core.interp()
                .timeline
                .leading_edge(now)
                .map(|e| e + rtt_ticks / 2.0 - reckon::PRESENT_CENTRE)
        });
        let render = core.interp().timeline.render_tick();
        let mut gathered = Vec::new();
        let mut dropped = Vec::new();
        let interp = core.interp();
        for id in b.active.keys().copied() {
            let Some(r) = b.reg.get(&id) else {
                dropped.push(id);
                continue;
            };
            if r.mode == BodyPrediction::Interpolate
                || !r.root.is_instance_valid()
                || !shown_frozen(core, id, r.fields.pos)
            {
                dropped.push(id);
                continue;
            }
            let newest = |f: Option<u16>| {
                f.and_then(|f| interp.track((id, 0, f)))
                    .and_then(|t| t.newest())
                    .map(|(t, v)| (t, v.clone()))
            };
            let Some((tick, pos)) = newest(Some(r.fields.pos)) else {
                dropped.push(id);
                continue;
            };
            let rot = newest(r.fields.rot);
            let lin = newest(r.fields.lin);
            let ang = newest(r.fields.ang);
            let tick = [&rot, &lin, &ang]
                .iter()
                .filter_map(|x| x.as_ref().map(|(t, _)| *t))
                .fold(tick, u64::max);
            let base = motion_of(
                r.three_d,
                &pos,
                rot.as_ref().map(|x| &x.1),
                lin.as_ref().map(|x| &x.1),
                ang.as_ref().map(|x| &x.1),
                &r.root,
            );
            let prev_lin = r
                .fields
                .lin
                .and_then(|f| interp.track((id, 0, f)))
                .and_then(|t| t.previous())
                .map(|(t, v)| (t, vec3(v)));
            let teleport = interp
                .track((id, 0, r.fields.pos))
                .map_or(f64::INFINITY, |t| t.params.teleport);
            let teleport_angle = r
                .fields
                .rot
                .and_then(|f| interp.track((id, 0, f)))
                .map_or(std::f64::consts::FRAC_PI_2, |t| t.params.teleport);
            let newest_all = if r.mode == BodyPrediction::Custom {
                r.fields
                    .interp
                    .iter()
                    .filter_map(|(f, rf)| newest(Some(*f)).map(|(_, v)| (rf.clone(), v)))
                    .collect()
            } else {
                Vec::new()
            };
            gathered.push(Gathered {
                id,
                base: (tick, base),
                prev_lin,
                teleport,
                teleport_angle,
                newest: newest_all,
            });
        }
        let mut settled = dropped.clone();
        for id in &dropped {
            b.active.remove(id);
        }
        // Interpolation does not write what prediction shows.
        values.retain(|((id, part, field), _)| {
            *part != 0
                || !b.active.contains_key(id)
                || !b.reg.get(id).is_some_and(|r| {
                    if r.mode == BodyPrediction::Custom {
                        r.fields.interp.iter().any(|(f, _)| f == field)
                    } else {
                        r.fields.pose_fields().any(|f| f == *field)
                    }
                })
        });
        let Some(present) = present else {
            return;
        };
        let cap = predict_max_ms() / 1000.0 * rate;
        let corr = [correction_settings(false), correction_settings(true)];
        // Hooks may call back into gdnet: run with the session's base guard, the bodies taken out.
        let mut bodies = std::mem::take(&mut self.bodies);
        let mut horizon_sum = 0.0;
        let mut predicted = 0usize;
        let mut corrections = 0u64;
        {
            let _guard = self.base_mut();
            for g in gathered {
                let Some(r) = bodies.reg.get(&g.id) else {
                    continue;
                };
                let Some(act) = bodies.active.get_mut(&g.id) else {
                    continue;
                };
                let (position, angle, decay) = corr[usize::from(r.three_d)];
                let th = Thresholds {
                    position,
                    angle,
                    teleport: g.teleport,
                    teleport_angle: g.teleport_angle,
                    decay,
                };
                let (tick, base) = g.base;
                // The cap holds at each re-base (the lag is what the newest snapshot's horizon exceeds
                // it by); between snapshots the horizon runs on with the clock, so motion stays smooth,
                // up to a hard limit of the cap plus a tenth of a second when snapshots stop.
                let hard = cap + 0.1 * rate;
                let lag_new = (present - tick as f64 - cap).max(0.0);
                let node = r.root.clone();
                let custom = r.mode == BodyPrediction::Custom;
                let mut builtin = |m: &mut Motion, f: &Forces, dt: f64| reckon::step(m, f, dt);
                let hook_node = node.clone();
                let mut hook = move |m: &mut Motion, f: &Forces, dt: f64| {
                    write_motion(&hook_node, m, true, true);
                    IN_HOOK.with(|c| c.set(Some((hook_node.instance_id(), *f))));
                    let mut n = hook_node.clone();
                    n.call(HOOK, &[dt.to_variant()]);
                    IN_HOOK.with(|c| c.set(None));
                    if let Some(back) = read_motion(&hook_node) {
                        *m = back;
                    }
                };
                let step: &mut dyn FnMut(&mut Motion, &Forces, f64) =
                    if custom { &mut hook } else { &mut builtin };
                let rebase = act.track.as_ref().is_none_or(|t| tick > t.base_tick());
                if let Some(t) = act.track.as_mut() {
                    t.decay(dt, &th);
                }
                let lag_old = act.lag;
                if rebase {
                    act.lag = lag_new;
                }
                let h = reckon::horizon(present - act.lag, tick, hard);
                if rebase {
                    let mut forces = forces_of(&node);
                    if let Some((pt, pl)) = g.prev_lin {
                        if reckon::supported(pl, pt, base.lin, tick, &forces, tick_dt, r.three_d) {
                            forces = forces.without_gravity();
                        }
                    }
                    if custom {
                        for (rf, v) in &g.newest {
                            crate::apply::write(&node, rf, v);
                        }
                    }
                    let result = match act.track.as_mut() {
                        Some(t) => {
                            let h_old = reckon::horizon(present - lag_old, t.base_tick(), hard);
                            t.rebase(tick, base, forces, h_old, h, tick_dt, &th, step)
                        }
                        None => {
                            // A mode change in flight blends from the interpolated pose; a body
                            // that starts moving starts at the prediction (the interpolated pose
                            // is only late, not wrong).
                            let shown = read_motion(&node).filter(|_| act.blend);
                            let (t, res) = BodyTrack::start(
                                tick,
                                base,
                                forces,
                                shown.as_ref(),
                                h,
                                tick_dt,
                                &th,
                                step,
                            );
                            act.track = Some(t);
                            res
                        }
                    };
                    if result == Rebased::Corrected {
                        corrections += 1;
                    }
                }
                let Some(t) = act.track.as_mut() else {
                    continue;
                };
                let (raw, shown) = t.shown(h, tick_dt, step);
                let mut out = shown;
                out.lin = raw.lin;
                out.ang = raw.ang;
                write_motion(&node, &out, r.fields.rot.is_some(), true);
                if base.is_moving() {
                    predicted += 1;
                    horizon_sum += h;
                } else if t.offset().is_none() && render.is_some_and(|rt| rt >= tick as f64) {
                    settled.push(g.id);
                }
            }
        }
        for id in &settled {
            bodies.active.remove(id);
        }
        bodies.predicted_last = predicted;
        bodies.horizon_ms_last = if predicted > 0 {
            horizon_sum / predicted as f64 / rate * 1000.0
        } else {
            0.0
        };
        bodies.corrections += corrections;
        if now.saturating_sub(bodies.window.0) >= 1_000_000 {
            bodies.corrections_per_s = bodies.corrections - bodies.window.1;
            bodies.window = (now, bodies.corrections);
        }
        self.bodies = bodies;
        if let Some(core) = self.core.as_mut() {
            for id in settled {
                core.interp_mut().settle(id);
            }
        }
    }
}

fn vec3(v: &Value) -> [f64; 3] {
    match v {
        Value::Vector2(a) => [f64::from(a[0]), f64::from(a[1]), 0.0],
        Value::Vector3(a) => [f64::from(a[0]), f64::from(a[1]), f64::from(a[2])],
        Value::Float(x) => [*x, 0.0, 0.0],
        _ => [0.0; 3],
    }
}

/// A base motion from the newest samples; a field that is not replicated keeps the node's value
/// (rotation) or zero (velocities).
fn motion_of(
    three_d: bool,
    pos: &Value,
    rot: Option<&Value>,
    lin: Option<&Value>,
    ang: Option<&Value>,
    node: &Gd<Node>,
) -> Motion {
    let current = || read_motion(node).map(|m| m.rot);
    let rot = match rot {
        Some(Value::Quaternion(q)) => [
            f64::from(q[0]),
            f64::from(q[1]),
            f64::from(q[2]),
            f64::from(q[3]),
        ],
        Some(Value::Float(a)) => [*a, 0.0, 0.0, 0.0],
        _ => current().unwrap_or(if three_d {
            [0.0, 0.0, 0.0, 1.0]
        } else {
            [0.0; 4]
        }),
    };
    Motion {
        three_d,
        pos: vec3(pos),
        rot,
        lin: lin.map_or([0.0; 3], vec3),
        ang: ang.map_or([0.0; 3], vec3),
    }
}

#[godot_api(secondary)]
impl Net {
    /// WP19: advances a `RigidBody2D/3D`'s `position`, rotation, `linear_velocity` and
    /// `angular_velocity` by `delta` seconds of free flight (velocity, gravity times `gravity_scale`,
    /// damping): the built-in step of `body_prediction = dead_reckoning`, for `_net_predict_body`.
    #[func]
    fn dead_reckon(&self, body: Gd<Node>, delta: f64) {
        dead_reckon_node(&body, delta);
    }

    /// WP19: the effective `body_prediction` of the entity `node` belongs to on this peer:
    /// `NetEntity.BODY_PREDICTION_INTERPOLATE`, `_DEAD_RECKONING` or `_CUSTOM` (the entity's setting or
    /// `configure()`, else custom when its script defines `_net_predict_body`, else the manager's, else
    /// `gdnet/physics/body_prediction`). Only frozen body replicas on a client are shown that way: for a
    /// spawned body entity this peer does not show as one (the server's copies, the owner's own `owner`
    /// body, every `simulated` replica, a body WP4 predicts) it is `NetEntity.BODY_PREDICTION_NONE`.
    #[func]
    fn body_prediction_of(&self, node: Gd<Node>) -> i64 {
        if let Some((root, m)) = entity_root_of(&node) {
            if m.bind().shows_frozen_body(root.instance_id()) == Some(false) {
                return crate::entity_node::NetEntity::BODY_PREDICTION_NONE;
            }
        }
        resolve(&node) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for m in [
            BodyPrediction::Default,
            BodyPrediction::Interpolate,
            BodyPrediction::DeadReckoning,
            BodyPrediction::Custom,
        ] {
            assert_eq!(BodyPrediction::from_index(m as i64), Some(m));
            assert!(!m.name().is_empty());
        }
        assert_eq!(BodyPrediction::from_index(9), None);
    }
}

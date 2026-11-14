//! WP18: direct reads of the built-in properties the shipped profiles poll most (transforms,
//! velocities, visibility, a collision shape's `disabled`, a body's sleeping and frozen state).
//!
//! Measured 2026-11-14 (release build, 5,000 entities): `Object::get` cost 0.37 us per property on a
//! `Node2D` and 0.68 us on a `RigidBody2D` (a name lookup up the class hierarchy, a Variant, its
//! conversion), and gdext's typed getters were slower still, since every `clone`, `try_cast` and call
//! checks the object's liveness again. Here a field's getter is called through the engine's method bind
//! (`object_method_bind_ptrcall`): about 0.1 us. [`resolve`] checks the node's class once
//! (`object_cast_to`) and picks the getter; the poll then only checks the node is alive
//! (`is_instance_valid`), on the main thread, and a getter of a built-in property runs no user code, so
//! the object cannot go away during the call. A field whose node is of another class (or whose property
//! has no getter here) is read with `Object::get`.

use godot::classes::Node;
use godot::prelude::*;
use godot::sys;
use netcore::{TypeTag, Value};
use std::ffi::c_void;
use std::sync::OnceLock;

/// The getter a field is read with, or `Generic` (`Object::get`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fast {
    #[default]
    Generic,
    Position2,
    Rotation2,
    Scale2,
    Skew2,
    Position3,
    Quaternion3,
    Scale3,
    Visible,
    Modulate,
    SelfModulate,
    ZIndex,
    LinearVelocity2,
    AngularVelocity2,
    LinearVelocity3,
    AngularVelocity3,
    Velocity2,
    Velocity3,
    Disabled,
}

/// The kind of a property `name` replicated as `ty`.
pub fn kind(name: &str, ty: TypeTag) -> Fast {
    use TypeTag as T;
    match (name, ty) {
        ("position", T::Vector2) => Fast::Position2,
        ("rotation", T::Float) => Fast::Rotation2,
        ("scale", T::Vector2) => Fast::Scale2,
        ("skew", T::Float) => Fast::Skew2,
        ("position", T::Vector3) => Fast::Position3,
        ("quaternion", T::Quaternion) => Fast::Quaternion3,
        ("scale", T::Vector3) => Fast::Scale3,
        ("visible", T::Bool) => Fast::Visible,
        ("modulate", T::Color) => Fast::Modulate,
        ("self_modulate", T::Color) => Fast::SelfModulate,
        ("z_index", T::Int) => Fast::ZIndex,
        ("linear_velocity", T::Vector2) => Fast::LinearVelocity2,
        ("angular_velocity", T::Float) => Fast::AngularVelocity2,
        ("linear_velocity", T::Vector3) => Fast::LinearVelocity3,
        ("angular_velocity", T::Vector3) => Fast::AngularVelocity3,
        ("velocity", T::Vector2) => Fast::Velocity2,
        ("velocity", T::Vector3) => Fast::Velocity3,
        ("disabled", T::Bool) => Fast::Disabled,
        _ => Fast::Generic,
    }
}

/// What a getter returns, in ptrcall layout (`real_t` vectors, `double` floats, `int64_t` ints).
#[derive(Clone, Copy)]
enum Ret {
    Bool,
    Int,
    Float,
    V2,
    V3,
    Quat,
    Color,
}

/// Getters: (class, method, hash from extension_api.json, return layout). The hashes are the ones gdext
/// itself loads (api-4-3; Godot keeps them stable).
const GETTERS: [(&str, &str, i64, Ret); 24] = [
    ("Node2D", "get_position", 3341600327, Ret::V2), // 0
    ("Node2D", "get_rotation", 1740695150, Ret::Float),
    ("Node2D", "get_scale", 3341600327, Ret::V2),
    ("Node2D", "get_skew", 1740695150, Ret::Float),
    ("Node3D", "get_position", 3360562783, Ret::V3), // 4
    ("Node3D", "get_quaternion", 1222331677, Ret::Quat),
    ("Node3D", "get_scale", 3360562783, Ret::V3),
    ("CanvasItem", "is_visible", 36873697, Ret::Bool), // 7
    ("Node3D", "is_visible", 36873697, Ret::Bool),
    ("CanvasItem", "get_modulate", 3444240500, Ret::Color), // 9
    ("CanvasItem", "get_self_modulate", 3444240500, Ret::Color),
    ("CanvasItem", "get_z_index", 3905245786, Ret::Int),
    ("RigidBody2D", "get_linear_velocity", 3341600327, Ret::V2), // 12
    (
        "RigidBody2D",
        "get_angular_velocity",
        1740695150,
        Ret::Float,
    ),
    ("RigidBody3D", "get_linear_velocity", 3360562783, Ret::V3),
    ("RigidBody3D", "get_angular_velocity", 3360562783, Ret::V3),
    ("CharacterBody2D", "get_velocity", 3341600327, Ret::V2), // 16
    ("CharacterBody3D", "get_velocity", 3360562783, Ret::V3),
    ("CollisionShape2D", "is_disabled", 36873697, Ret::Bool), // 18
    ("CollisionShape3D", "is_disabled", 36873697, Ret::Bool),
    ("RigidBody2D", "is_sleeping", 36873697, Ret::Bool), // 20
    ("RigidBody3D", "is_sleeping", 36873697, Ret::Bool),
    ("RigidBody2D", "is_freeze_enabled", 36873697, Ret::Bool), // 22
    ("RigidBody3D", "is_freeze_enabled", 36873697, Ret::Bool),
];

/// Getter indices for `is_sleeping` and `is_freeze_enabled`.
const SLEEPING_2D: usize = 20;
const SLEEPING_3D: usize = 21;
const FROZEN_2D: usize = 22;
const FROZEN_3D: usize = 23;

/// The getters to try for a kind, in order.
fn getters(k: Fast) -> &'static [usize] {
    match k {
        Fast::Generic => &[],
        Fast::Position2 => &[0],
        Fast::Rotation2 => &[1],
        Fast::Scale2 => &[2],
        Fast::Skew2 => &[3],
        Fast::Position3 => &[4],
        Fast::Quaternion3 => &[5],
        Fast::Scale3 => &[6],
        Fast::Visible => &[7, 8],
        Fast::Modulate => &[9],
        Fast::SelfModulate => &[10],
        Fast::ZIndex => &[11],
        Fast::LinearVelocity2 => &[12],
        Fast::AngularVelocity2 => &[13],
        Fast::LinearVelocity3 => &[14],
        Fast::AngularVelocity3 => &[15],
        Fast::Velocity2 => &[16],
        Fast::Velocity3 => &[17],
        Fast::Disabled => &[18, 19],
    }
}

#[derive(Clone, Copy)]
struct Bind {
    mb: sys::GDExtensionMethodBindPtr,
    tag: *mut c_void,
    ret: Ret,
}

/// The method binds and class tags, loaded once. They are engine-wide constants; gdnet only uses them
/// on the main thread (every `Gd` is touched there).
struct Binds(Vec<Option<Bind>>);

// SAFETY: the pointers are immutable engine data valid for the process; the calls through them happen
// on the main thread only.
unsafe impl Sync for Binds {}
// SAFETY: as above.
unsafe impl Send for Binds {}

static BINDS: OnceLock<Binds> = OnceLock::new();

fn binds() -> &'static [Option<Bind>] {
    &BINDS.get_or_init(load).0
}

fn load() -> Binds {
    Binds(
        GETTERS
            .iter()
            .map(|&(class, method, hash, ret)| {
                let class = StringName::from(class);
                let method = StringName::from(method);
                // SAFETY: the interface is initialised once the extension is loaded; both names live
                // for the calls.
                let (mb, tag) = unsafe {
                    let mb = sys::interface_fn!(classdb_get_method_bind)(
                        class.string_sys(),
                        method.string_sys(),
                        hash,
                    );
                    let tag = sys::interface_fn!(classdb_get_class_tag)(class.string_sys());
                    (mb, tag)
                };
                (!mb.is_null() && !tag.is_null()).then_some(Bind { mb, tag, ret })
            })
            .collect(),
    )
}

/// A getter checked against a node's class by [`resolve`]; `NONE` reads with `Object::get`.
pub type Getter = u8;
pub const NONE: Getter = u8::MAX;

/// Whether `obj` is of the class of getter `b`.
///
/// # Safety
/// `obj` must point to a live object.
unsafe fn is_of(obj: sys::GDExtensionObjectPtr, b: &Bind) -> bool {
    // SAFETY: `obj` is live (the caller's contract); `object_cast_to` only reads its class.
    !unsafe { sys::interface_fn!(object_cast_to)(obj, b.tag) }.is_null()
}

/// Calls getter `b` on `obj`.
///
/// # Safety
/// `obj` must point to a live object of the getter's class.
unsafe fn call(obj: sys::GDExtensionObjectPtr, b: &Bind) -> Value {
    let mut buf = [0u64; 2];
    let ret = buf.as_mut_ptr() as sys::GDExtensionTypePtr;
    // SAFETY: the method bind belongs to `obj`'s class (the caller's contract); it takes no arguments
    // and writes at most 16 bytes (a Color or a Quaternion of f32) into `ret`.
    unsafe {
        sys::interface_fn!(object_method_bind_ptrcall)(b.mb, obj, std::ptr::null(), ret);
    }
    let floats = buf.as_ptr() as *const f32;
    // SAFETY: `buf` holds 16 bytes, so four f32s; the getter wrote the ones its type has.
    let f = |i: usize| -> f32 { unsafe { *floats.add(i) } };
    match b.ret {
        Ret::Bool => Value::Bool(buf[0] as u8 != 0),
        Ret::Int => Value::Int(buf[0] as i64),
        Ret::Float => Value::Float(f64::from_bits(buf[0])),
        Ret::V2 => Value::Vector2([f(0), f(1)]),
        Ret::V3 => Value::Vector3([f(0), f(1), f(2)]),
        Ret::Quat => Value::Quaternion([f(0), f(1), f(2), f(3)]),
        Ret::Color => Value::Color([f(0), f(1), f(2), f(3)]),
    }
}

/// The getter of kind `k` that fits `node`'s class, or `NONE`. A node's class never changes, so the
/// answer holds for as long as the node lives.
pub fn resolve(node: &Gd<Node>, k: Fast) -> Getter {
    if k == Fast::Generic || !node.is_instance_valid() {
        return NONE;
    }
    let obj = node.obj_sys();
    let binds = binds();
    for &g in getters(k) {
        if let Some(b) = binds[g].as_ref() {
            // SAFETY: checked alive above, on this thread.
            if unsafe { is_of(obj, b) } {
                return g as Getter;
            }
        }
    }
    NONE
}

/// Reads `node` with a getter [`resolve`] picked for it (None for `NONE`).
///
/// # Safety
/// `node` must be alive and `g` must come from `resolve` on this node (or `NONE`).
pub unsafe fn read_resolved(node: &Gd<Node>, g: Getter) -> Option<Value> {
    let b = binds().get(usize::from(g))?.as_ref()?;
    // SAFETY: the caller's contract.
    Some(unsafe { call(node.obj_sys(), b) })
}

/// The `is_sleeping` and `is_freeze_enabled` getters of a `RigidBody2D` / `RigidBody3D` (None for
/// other nodes).
pub fn resolve_body(node: &Gd<Node>) -> Option<(Getter, Getter)> {
    if !node.is_instance_valid() {
        return None;
    }
    let obj = node.obj_sys();
    let binds = binds();
    [(SLEEPING_2D, FROZEN_2D), (SLEEPING_3D, FROZEN_3D)]
        .into_iter()
        .find(|&(s, _)| {
            // SAFETY: checked alive above, on this thread.
            binds[s].as_ref().is_some_and(|b| unsafe { is_of(obj, b) })
        })
        .map(|(s, f)| (s as Getter, f as Getter))
}

/// Whether a body rests (sleeps and is not frozen), read with the getters [`resolve_body`] gave.
///
/// # Safety
/// `node` must be alive and the getters must come from `resolve_body` on it.
pub unsafe fn body_rests(node: &Gd<Node>, getters: (Getter, Getter)) -> bool {
    // SAFETY: the caller's contract.
    let (s, f) = unsafe {
        (
            read_resolved(node, getters.0),
            read_resolved(node, getters.1),
        )
    };
    matches!(s, Some(Value::Bool(true))) && matches!(f, Some(Value::Bool(false)))
}

/// Reads `node`'s property of kind `k` through its getter; None when `k` is `Generic`, `node` is not of
/// a class the kind knows, or it was freed (`Object::get` then reports it as before).
pub fn read(node: &Gd<Node>, k: Fast) -> Option<Value> {
    let g = resolve(node, k);
    if g == NONE {
        return None;
    }
    // SAFETY: `resolve` checked the node alive and its class just now.
    unsafe { read_resolved(node, g) }
}

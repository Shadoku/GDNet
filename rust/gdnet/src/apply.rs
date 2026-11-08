//! Reading and writing replicated properties (docs/PLAN_NETCODE.md 2.2 steps 5 and 7, WP3): conversion
//! between Godot Variants and netcore [`Value`]s by type tag, through each field's [`Accessor`].
//!
//! Replicas snap to the latest value in WP3 (interpolation is WP5). Every call here may run user code
//! (setters, `play()`), so callers hold their base guard.

use crate::infer::{Accessor, RtField};
use godot::classes::{Node, Resource, ResourceLoader};
use godot::global::{bytes_to_var, var_to_bytes};
use godot::prelude::*;
use netcore::{TypeTag, Value};

/// Converts a Variant to a netcore value of type `ty` (a mismatching Variant gives `Value::Nil`, which
/// encodes as the type's zero value).
pub fn to_value(v: &Variant, ty: TypeTag) -> Value {
    match ty {
        TypeTag::Bool => v.try_to::<bool>().map(Value::Bool).unwrap_or_default(),
        TypeTag::Int => v.try_to::<i64>().map(Value::Int).unwrap_or_default(),
        TypeTag::Float => v.try_to::<f64>().map(Value::Float).unwrap_or_default(),
        TypeTag::Vector2 => v
            .try_to::<Vector2>()
            .map(|x| Value::Vector2([x.x, x.y]))
            .unwrap_or_default(),
        TypeTag::Vector3 => v
            .try_to::<Vector3>()
            .map(|x| Value::Vector3([x.x, x.y, x.z]))
            .unwrap_or_default(),
        TypeTag::Vector4 => v
            .try_to::<Vector4>()
            .map(|x| Value::Vector4([x.x, x.y, x.z, x.w]))
            .unwrap_or_default(),
        TypeTag::Vector2i => v
            .try_to::<Vector2i>()
            .map(|x| Value::Vector2i([x.x, x.y]))
            .unwrap_or_default(),
        TypeTag::Vector3i => v
            .try_to::<Vector3i>()
            .map(|x| Value::Vector3i([x.x, x.y, x.z]))
            .unwrap_or_default(),
        TypeTag::Quaternion => v
            .try_to::<Quaternion>()
            .map(|q| Value::Quaternion([q.x, q.y, q.z, q.w]))
            .unwrap_or_default(),
        TypeTag::Transform2D => v
            .try_to::<Transform2D>()
            .map(|t| Value::Transform2D([t.a.x, t.a.y, t.b.x, t.b.y, t.origin.x, t.origin.y]))
            .unwrap_or_default(),
        TypeTag::Transform3D => v
            .try_to::<Transform3D>()
            .map(|t| {
                let r = t.basis.rows;
                Value::Transform3D([
                    r[0].x, r[0].y, r[0].z, r[1].x, r[1].y, r[1].z, r[2].x, r[2].y, r[2].z,
                    t.origin.x, t.origin.y, t.origin.z,
                ])
            })
            .unwrap_or_default(),
        TypeTag::Color => v
            .try_to::<Color>()
            .map(|c| Value::Color([c.r, c.g, c.b, c.a]))
            .unwrap_or_default(),
        TypeTag::String | TypeTag::StringName | TypeTag::NodePath => Value::Str(v.to_string()),
        TypeTag::Resource => Value::Str(
            v.try_to::<Gd<Resource>>()
                .map(|r| r.get_path().to_string())
                .unwrap_or_default(),
        ),
        TypeTag::Variant => Value::Bytes(var_to_bytes(v).to_vec()),
    }
}

/// Converts a netcore value back to a Variant of type `ty` (resources are not loaded here).
pub fn to_variant(v: &Value, ty: TypeTag) -> Variant {
    match (ty, v) {
        (_, Value::Bool(b)) => b.to_variant(),
        (_, Value::Int(i)) => i.to_variant(),
        (_, Value::Float(f)) => f.to_variant(),
        (_, Value::Vector2(a)) => Vector2::new(a[0], a[1]).to_variant(),
        (_, Value::Vector3(a)) => Vector3::new(a[0], a[1], a[2]).to_variant(),
        (_, Value::Vector4(a)) => Vector4::new(a[0], a[1], a[2], a[3]).to_variant(),
        (_, Value::Vector2i(a)) => Vector2i::new(a[0], a[1]).to_variant(),
        (_, Value::Vector3i(a)) => Vector3i::new(a[0], a[1], a[2]).to_variant(),
        (_, Value::Quaternion(a)) => Quaternion::new(a[0], a[1], a[2], a[3]).to_variant(),
        (_, Value::Transform2D(a)) => Transform2D::from_cols(
            Vector2::new(a[0], a[1]),
            Vector2::new(a[2], a[3]),
            Vector2::new(a[4], a[5]),
        )
        .to_variant(),
        (_, Value::Transform3D(a)) => Transform3D::new(
            Basis::from_rows(
                Vector3::new(a[0], a[1], a[2]),
                Vector3::new(a[3], a[4], a[5]),
                Vector3::new(a[6], a[7], a[8]),
            ),
            Vector3::new(a[9], a[10], a[11]),
        )
        .to_variant(),
        (_, Value::Color(a)) => Color::from_rgba(a[0], a[1], a[2], a[3]).to_variant(),
        (TypeTag::StringName, Value::Str(s)) => StringName::from(s.as_str()).to_variant(),
        (TypeTag::NodePath, Value::Str(s)) => NodePath::from(s.as_str()).to_variant(),
        (_, Value::Str(s)) => GString::from(s.as_str()).to_variant(),
        (_, Value::Bytes(b)) => bytes_to_var(&PackedByteArray::from(b.as_slice())),
        (_, Value::Nil) => Variant::nil(),
    }
}

/// Reads a field of `node`.
pub fn read(node: &Gd<Node>, f: &RtField) -> Value {
    match &f.acc {
        Accessor::Prop(name) | Accessor::AnimPosition(name) | Accessor::Resource(name) => {
            to_value(&node.get(name), f.ty)
        }
        Accessor::Playing => {
            let mut n = node.clone();
            to_value(&n.call("is_playing", &[]), f.ty)
        }
    }
}

/// Writes a field of `node` (snapping to the value).
pub fn write(node: &Gd<Node>, f: &RtField, v: &Value) {
    let mut n = node.clone();
    match &f.acc {
        Accessor::Prop(name) => n.set(name, &to_variant(v, f.ty)),
        Accessor::Playing => {
            let want = matches!(v, Value::Bool(true));
            let playing = n.call("is_playing", &[]).try_to::<bool>().unwrap_or(false);
            if want && !playing {
                n.call("play", &[]);
            } else if !want && playing {
                n.call("pause", &[]);
            }
        }
        Accessor::AnimPosition(name) => {
            let Value::Float(pos) = v else {
                return;
            };
            let now = n.get(name).try_to::<f64>().unwrap_or(0.0);
            let playing = n.call("is_playing", &[]).try_to::<bool>().unwrap_or(false);
            if playing && (now - pos).abs() > 0.05 {
                n.call("seek", &[pos.to_variant(), true.to_variant()]);
            }
        }
        Accessor::Resource(name) => {
            let Value::Str(path) = v else {
                return;
            };
            let current = n
                .get(name)
                .try_to::<Gd<Resource>>()
                .map(|r| r.get_path().to_string())
                .unwrap_or_default();
            if &current == path {
                return;
            }
            if path.is_empty() {
                n.set(name, &Variant::nil());
            } else if let Some(res) = ResourceLoader::singleton().load(path.as_str()) {
                n.set(name, &res.to_variant());
            }
        }
    }
}

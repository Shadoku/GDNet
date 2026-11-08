//! Property tables (docs/PLAN_NETCODE.md 2.1 "Profiles", 2.4 `Snapshot`, 3.3, WP3).
//!
//! A replicated property is described by a [`PropDesc`]: its name (kept for the hash and for readable
//! mismatch reasons; names never travel in snapshots), a [`TypeTag`] saying how gdnet converts it, a
//! [`Quant`] saying how the value is packed, its [`Schedule`] (hot = read every tick, cold = every
//! `cold_poll_ticks`), the interpolate flag (stored for WP5), the prediction tolerance (stored for WP4),
//! `reliable` (sent on the reliable channel instead of in snapshots) and `slow_ticks` (a cold property
//! that is only resent when another property of its part changed or after that many ticks, e.g. an
//! `AnimationPlayer`'s playback position).
//!
//! A scene has one table per part ([`ScenePropsTable`]): part index `i` of the scene's part table has the
//! property list `parts[i]`, so a field is addressed by (part index, field index) and nothing is named on
//! the wire. Both peers derive the table the same way; its hash goes with the scene's announcement and a
//! difference ends the session with a reason naming the scene, the part and the property.

use crate::entity::fnv1a64;
use serde::{Deserialize, Serialize};
use std::string::String;
use std::vec::Vec;

/// How gdnet converts a property between Godot and netcore [`Value`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeTag {
    Bool,
    Int,
    Float,
    Vector2,
    Vector3,
    Vector4,
    Vector2i,
    Vector3i,
    Quaternion,
    Transform2D,
    Transform3D,
    Color,
    String,
    StringName,
    NodePath,
    /// A resource, sent as its resource path (empty for none).
    Resource,
    /// Anything else, sent as Godot's `var_to_bytes` of the value (cold only).
    Variant,
}

impl TypeTag {
    /// Strings, resource paths and Variants are variable-length: never hot.
    pub fn cold_only(self) -> bool {
        matches!(
            self,
            TypeTag::String
                | TypeTag::StringName
                | TypeTag::NodePath
                | TypeTag::Resource
                | TypeTag::Variant
        )
    }

    pub fn name(self) -> &'static str {
        match self {
            TypeTag::Bool => "bool",
            TypeTag::Int => "int",
            TypeTag::Float => "float",
            TypeTag::Vector2 => "Vector2",
            TypeTag::Vector3 => "Vector3",
            TypeTag::Vector4 => "Vector4",
            TypeTag::Vector2i => "Vector2i",
            TypeTag::Vector3i => "Vector3i",
            TypeTag::Quaternion => "Quaternion",
            TypeTag::Transform2D => "Transform2D",
            TypeTag::Transform3D => "Transform3D",
            TypeTag::Color => "Color",
            TypeTag::String => "String",
            TypeTag::StringName => "StringName",
            TypeTag::NodePath => "NodePath",
            TypeTag::Resource => "Resource",
            TypeTag::Variant => "Variant",
        }
    }
}

/// How a value is packed (2.1 "Options per property").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Quant {
    /// Full width: f32 per vector component, f64 for a float.
    #[default]
    Raw,
    /// Fixed point, 0.001 units per step (zigzag varint per component).
    Mm,
    /// Fixed point, 0.01 units per step.
    Cm,
    /// An angle in radians wrapped to one turn, 16 bits per component.
    Deg,
    /// A unit quaternion as the three smallest components, 2 + 3 x 15 bits.
    Smallest3,
    /// IEEE half precision per component.
    F16,
}

impl Quant {
    pub fn parse(s: &str) -> Option<Quant> {
        Some(match s {
            "raw" => Quant::Raw,
            "mm" => Quant::Mm,
            "cm" => Quant::Cm,
            "deg" => Quant::Deg,
            "smallest3" => Quant::Smallest3,
            "f16" => Quant::F16,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Quant::Raw => "raw",
            Quant::Mm => "mm",
            Quant::Cm => "cm",
            Quant::Deg => "deg",
            Quant::Smallest3 => "smallest3",
            Quant::F16 => "f16",
        }
    }
}

/// When a property is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Schedule {
    /// Every tick.
    Hot,
    /// Every `cold_poll_ticks` (and right after a `Net.sync` touching the entity, WP6).
    #[default]
    Cold,
}

impl Schedule {
    pub fn parse(s: &str) -> Option<Schedule> {
        match s {
            "hot" => Some(Schedule::Hot),
            "cold" => Some(Schedule::Cold),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Schedule::Hot => "hot",
            Schedule::Cold => "cold",
        }
    }
}

/// One replicated property.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropDesc {
    pub name: String,
    pub ty: TypeTag,
    pub quant: Quant,
    pub schedule: Schedule,
    /// Replicas interpolate it (WP5; stored only in WP3).
    pub interpolate: bool,
    /// Prediction mismatch tolerance (WP4; stored only in WP3).
    pub tolerance: f32,
    /// Sent on the reliable channel instead of in snapshots.
    pub reliable: bool,
    /// 0, or: only resent when another property of the part changed, or after this many ticks.
    pub slow_ticks: u32,
    /// WP5: a correction larger than this snaps on replicas instead of blending (property units; 0 =
    /// `gdnet/interpolation/teleport_distance`, else automatic: see `interp::Params`).
    #[serde(default)]
    pub teleport_distance: f32,
}

// Tolerances are compared bit for bit: a table is equal only when both sides wrote the same numbers.
impl Eq for PropDesc {}

impl PropDesc {
    /// A cold, raw, non-interpolated property with the default tolerance.
    pub fn new(name: &str, ty: TypeTag) -> Self {
        Self {
            name: String::from(name),
            ty,
            quant: Quant::Raw,
            schedule: Schedule::Cold,
            interpolate: false,
            tolerance: 0.001,
            reliable: false,
            slow_ticks: 0,
            teleport_distance: 0.0,
        }
    }

    pub fn hot(mut self, quant: Quant) -> Self {
        self.schedule = Schedule::Hot;
        self.quant = quant;
        self
    }

    /// A readable description (mismatch reasons).
    pub fn describe(&self) -> String {
        std::format!(
            "{}: {} {} {}{}{} tolerance {}{}{}",
            self.name,
            self.ty.name(),
            self.schedule.name(),
            self.quant.name(),
            if self.interpolate {
                " interpolated"
            } else {
                ""
            },
            if self.reliable { " reliable" } else { "" },
            self.tolerance,
            if self.slow_ticks > 0 {
                std::format!(" slow {}", self.slow_ticks)
            } else {
                String::new()
            },
            if self.teleport_distance > 0.0 {
                std::format!(" teleport {}", self.teleport_distance)
            } else {
                String::new()
            }
        )
    }

    fn hash_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.name.as_bytes());
        out.push(0);
        out.extend_from_slice(self.ty.name().as_bytes());
        out.push(0);
        out.extend_from_slice(self.quant.name().as_bytes());
        out.push(0);
        out.push(self.schedule as u8);
        out.push(u8::from(self.interpolate));
        out.push(u8::from(self.reliable));
        out.extend_from_slice(&self.tolerance.to_bits().to_le_bytes());
        out.extend_from_slice(&self.slow_ticks.to_le_bytes());
        // WP5: tables without a teleport distance hash as before.
        if self.teleport_distance > 0.0 {
            out.extend_from_slice(b"teleport");
            out.extend_from_slice(&self.teleport_distance.to_bits().to_le_bytes());
        }
    }
}

/// The property lists of every part of a scene, in part order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenePropsTable {
    pub parts: Vec<Vec<PropDesc>>,
    /// WP4: the input actions of the scene's entities, in index order (an input frame's pressed bits
    /// index this list, so both peers must agree on it). Empty when no input is replicated.
    pub inputs: Vec<String>,
}

impl ScenePropsTable {
    pub fn new(parts: Vec<Vec<PropDesc>>) -> Self {
        Self {
            parts,
            inputs: Vec::new(),
        }
    }

    /// The flat index of each part's first field, plus the total at the end.
    pub fn offsets(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.parts.len() + 1);
        let mut n = 0u32;
        for p in &self.parts {
            out.push(n);
            n += p.len() as u32;
        }
        out.push(n);
        out
    }

    pub fn field_count(&self) -> usize {
        self.parts.iter().map(Vec::len).sum()
    }

    pub fn get(&self, part: u16, field: u16) -> Option<&PropDesc> {
        self.parts
            .get(usize::from(part))
            .and_then(|p| p.get(usize::from(field)))
    }

    /// FNV-1a 64 over every descriptor, with part separators. Part of the protocol (3.3).
    pub fn hash(&self) -> u64 {
        let mut bytes = Vec::new();
        for (i, part) in self.parts.iter().enumerate() {
            bytes.extend_from_slice(&(i as u32).to_le_bytes());
            for d in part {
                d.hash_into(&mut bytes);
            }
            bytes.push(0xff);
        }
        // WP4: tables without input actions hash as before.
        if !self.inputs.is_empty() {
            bytes.extend_from_slice(b"inputs");
            for a in &self.inputs {
                bytes.extend_from_slice(a.as_bytes());
                bytes.push(0);
            }
        }
        fnv1a64(&bytes)
    }

    /// `None` when equal; otherwise a readable reason naming the first differing part and property.
    /// `part_name(i)` names part `i` (its path in the scene).
    pub fn diff(
        &self,
        theirs: &ScenePropsTable,
        scene: &str,
        part_name: &dyn Fn(usize) -> String,
        local_label: &str,
        remote_label: &str,
    ) -> Option<String> {
        if self == theirs {
            return None;
        }
        let head = std::format!("protocol mismatch: the property table of scene {scene} differs");
        if self.parts == theirs.parts && self.inputs != theirs.inputs {
            return Some(std::format!(
                "{head} in its input actions ({local_label} [{}]; {remote_label} [{}]); every peer must have the same InputMap actions and input_actions settings",
                self.inputs.join(", "),
                theirs.inputs.join(", ")
            ));
        }
        let n = self.parts.len().max(theirs.parts.len());
        for i in 0..n {
            let (a, b) = (self.parts.get(i), theirs.parts.get(i));
            let empty = Vec::new();
            let (a, b) = (a.unwrap_or(&empty), b.unwrap_or(&empty));
            if a == b {
                continue;
            }
            let part = part_name(i);
            let m = a.len().max(b.len());
            for j in 0..m {
                match (a.get(j), b.get(j)) {
                    (Some(x), Some(y)) if x == y => continue,
                    (Some(x), Some(y)) if x.name == y.name => {
                        return Some(std::format!(
                            "{head} at part {part}, property {} ({local_label} {}; {remote_label} {}); both peers must derive the same profiles and configure() calls",
                            x.name,
                            x.describe(),
                            y.describe()
                        ))
                    }
                    (Some(x), Some(y)) => {
                        return Some(std::format!(
                            "{head} at part {part}, property {} ({local_label} has {}, {remote_label} has {}); both peers must derive the same profiles and configure() calls",
                            x.name, x.name, y.name
                        ))
                    }
                    (Some(x), None) => {
                        return Some(std::format!(
                            "{head} at part {part}, property {} (missing on the {remote_label}); both peers must derive the same profiles and configure() calls",
                            x.name
                        ))
                    }
                    (None, Some(y)) => {
                        return Some(std::format!(
                            "{head} at part {part}, property {} (missing on the {local_label}); both peers must derive the same profiles and configure() calls",
                            y.name
                        ))
                    }
                    (None, None) => {}
                }
            }
        }
        Some(head)
    }
}

/// A property value as netcore sees it (gdnet converts to and from Godot Variants by [`TypeTag`]).
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Vector2([f32; 2]),
    Vector3([f32; 3]),
    Vector4([f32; 4]),
    Vector2i([i32; 2]),
    Vector3i([i32; 3]),
    Quaternion([f32; 4]),
    /// x axis, y axis, origin.
    Transform2D([f32; 6]),
    /// Basis rows x, y, z (9 values), then origin (3).
    Transform3D([f32; 12]),
    Color([f32; 4]),
    /// String, StringName, NodePath and resource paths.
    Str(String),
    /// `var_to_bytes` of a Variant.
    Bytes(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> ScenePropsTable {
        ScenePropsTable::new(std::vec![
            std::vec![
                PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("visible", TypeTag::Bool),
            ],
            std::vec![],
            std::vec![PropDesc::new("text", TypeTag::String)],
        ])
    }

    #[test]
    fn offsets_and_lookup() {
        let t = table();
        assert_eq!(t.offsets(), std::vec![0, 2, 2, 3]);
        assert_eq!(t.field_count(), 3);
        assert_eq!(t.get(2, 0).unwrap().name, "text");
        assert!(t.get(1, 0).is_none());
    }

    #[test]
    fn hash_is_sensitive_to_every_option() {
        let base = table();
        let h = base.hash();
        assert_eq!(h, table().hash());
        let mut variants = Vec::new();
        let mut t = table();
        t.parts[0][0].quant = Quant::Cm;
        variants.push(t);
        let mut t = table();
        t.parts[0][0].interpolate = true;
        variants.push(t);
        let mut t = table();
        t.parts[0][1].schedule = Schedule::Hot;
        variants.push(t);
        let mut t = table();
        t.parts[0][1].tolerance = 0.5;
        variants.push(t);
        let mut t = table();
        t.parts[2][0].name = "texts".into();
        variants.push(t);
        let mut t = table();
        t.parts.swap(1, 2);
        variants.push(t);
        for v in variants {
            assert_ne!(v.hash(), h, "{v:?}");
        }
    }

    #[test]
    fn diff_names_scene_part_and_property() {
        let a = table();
        let mut b = table();
        b.parts[0][0].quant = Quant::Raw;
        let name = |i: usize| ["."; 3][i].to_string();
        let r = a
            .diff(&b, "res://a.tscn", &name, "client", "server")
            .unwrap();
        assert!(
            r.starts_with("protocol mismatch: the property table of scene res://a.tscn differs at part ., property position"),
            "{r}"
        );
        assert!(r.contains("mm") && r.contains("raw"), "{r}");
        assert_eq!(a.diff(&table(), "s", &name, "a", "b"), None);
        let mut c = table();
        c.parts[2].clear();
        let r = a.diff(&c, "s", &name, "client", "server").unwrap();
        assert!(r.contains("property text (missing on the server)"), "{r}");
    }

    #[test]
    fn input_actions_are_part_of_the_table() {
        let a = table();
        let h = a.hash();
        let mut b = table();
        b.inputs = std::vec!["left".into(), "right".into()];
        assert_ne!(b.hash(), h);
        let name = |_: usize| ".".to_string();
        let mut c = b.clone();
        c.inputs.swap(0, 1);
        let r = b.diff(&c, "s", &name, "client", "server").unwrap();
        assert!(
            r.contains("input actions (client [left, right]; server [right, left])"),
            "{r}"
        );
        assert_eq!(b.diff(&b.clone(), "s", &name, "a", "b"), None);
    }

    #[test]
    fn parse_names() {
        for q in [
            Quant::Raw,
            Quant::Mm,
            Quant::Cm,
            Quant::Deg,
            Quant::Smallest3,
            Quant::F16,
        ] {
            assert_eq!(Quant::parse(q.name()), Some(q));
        }
        assert_eq!(Quant::parse("nope"), None);
        assert_eq!(Schedule::parse("hot"), Some(Schedule::Hot));
        assert!(TypeTag::String.cold_only());
        assert!(!TypeTag::Vector2.cold_only());
    }
}

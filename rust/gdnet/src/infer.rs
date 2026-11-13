//! Inference (docs/PLAN_NETCODE.md 2.1 "Profiles", rung 3, WP3): the property table of a scene, derived
//! from a live instance.
//!
//! Per part, most-derived class first (`ClassDB.get_parent_class`), the effective profiles' entries are
//! united (a class mapped to an empty list stops the walk); properties the part does not have are left
//! out. Then, with `gdnet/sync/export_vars` on (D12), every `@export` variable of the part's script is
//! added as a cold property. A `NetEntity` child of the root with `inherit_profile = false` replaces all
//! of that with its `state_properties` (`"Part:prop"` or `"prop"` paths, options from `state_options`);
//! with `inherit_profile = true` they are added. Then the `Net.configure()` operations recorded for the
//! scene apply. The result is cached per (scene, part table, profiles, debug perturbation): both peers
//! derive it from the same scene and the same calls, and compare hashes.
//!
//! Type tags come from the property's declared type (`ClassDB.class_get_property_list` for engine
//! classes, the script's property list for scripts). Pseudo-properties: `playing` (read `is_playing()`,
//! written with `play()` / `pause()`) and `AnimationPlayer.current_animation_position` (written with
//! `seek()`). Resources travel as their resource path.

use crate::configure::{self, Op, PropPath};
use crate::entity_node::NetEntity;
use crate::manager::net_entity_of;
use crate::parts::SceneInfo;
use crate::profiles::{self, Entry, Profiles};
use godot::classes::{ClassDb, Node, Script};
use godot::prelude::*;
use netcore::{PropDesc, Quant, ScenePropsTable, Schedule, TypeTag};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// How a field is read and written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessKind {
    Prop,
    /// `is_playing()` / `play()` / `pause()`.
    Playing,
    /// `current_animation_position` / `seek()`.
    AnimPosition,
    /// A resource property, as its path.
    Resource,
}

#[derive(Clone, Debug)]
pub struct FieldSpec {
    pub name: String,
    pub kind: AccessKind,
}

/// A derived table: the descriptors netcore hashes and packs, and how gdnet reaches each field.
#[derive(Debug)]
pub struct Inferred {
    pub table: ScenePropsTable,
    pub specs: Vec<Vec<FieldSpec>>,
    pub part_names: Vec<String>,
}

#[derive(Clone, Debug)]
struct PropInfo {
    ty: i32,
    class_name: String,
}

const USAGE_EDITOR: i64 = 4;
const USAGE_GROUP: i64 = 64;
const USAGE_CATEGORY: i64 = 128;
const USAGE_SUBGROUP: i64 = 256;
const USAGE_SCRIPT_VARIABLE: i64 = 4096;

thread_local! {
    static TABLES: RefCell<HashMap<String, Rc<Inferred>>> = RefCell::new(HashMap::new());
    static CLASS_PROPS: RefCell<HashMap<String, Rc<HashMap<String, PropInfo>>>> = RefCell::new(HashMap::new());
}

fn dict_str(d: &VarDictionary, k: &str) -> String {
    d.get(k)
        .and_then(|v| {
            v.try_to::<GString>()
                .ok()
                .map(|g| g.to_string())
                .or_else(|| v.try_to::<StringName>().ok().map(|s| s.to_string()))
        })
        .unwrap_or_default()
}

fn dict_i64(d: &VarDictionary, k: &str) -> i64 {
    d.get(k).and_then(|v| v.try_to::<i64>().ok()).unwrap_or(0)
}

fn class_props(class: &str) -> Rc<HashMap<String, PropInfo>> {
    if let Some(hit) = CLASS_PROPS.with(|c| c.borrow().get(class).cloned()) {
        return hit;
    }
    let mut map = HashMap::new();
    for d in ClassDb::singleton()
        .class_get_property_list(class)
        .iter_shared()
    {
        map.insert(
            dict_str(&d, "name"),
            PropInfo {
                ty: dict_i64(&d, "type") as i32,
                class_name: dict_str(&d, "class_name"),
            },
        );
    }
    let rc = Rc::new(map);
    CLASS_PROPS.with(|c| c.borrow_mut().insert(class.to_string(), rc.clone()));
    rc
}

/// The script's property list as (name, info, usage), in order.
fn script_props(node: &Gd<Node>) -> Vec<(String, PropInfo, i64)> {
    let Some(script) = node.get_script().and_then(|s| s.try_cast::<Script>().ok()) else {
        return Vec::new();
    };
    script
        .get_script_property_list()
        .iter_shared()
        .map(|d| {
            (
                dict_str(&d, "name"),
                PropInfo {
                    ty: dict_i64(&d, "type") as i32,
                    class_name: dict_str(&d, "class_name"),
                },
                dict_i64(&d, "usage"),
            )
        })
        .collect()
}

fn tag_of(info: &PropInfo) -> Option<(TypeTag, AccessKind)> {
    let t = VariantType::from_ord(info.ty);
    let tag = match t {
        VariantType::NIL => return None,
        VariantType::BOOL => TypeTag::Bool,
        VariantType::INT => TypeTag::Int,
        VariantType::FLOAT => TypeTag::Float,
        VariantType::VECTOR2 => TypeTag::Vector2,
        VariantType::VECTOR3 => TypeTag::Vector3,
        VariantType::VECTOR4 => TypeTag::Vector4,
        VariantType::VECTOR2I => TypeTag::Vector2i,
        VariantType::VECTOR3I => TypeTag::Vector3i,
        VariantType::QUATERNION => TypeTag::Quaternion,
        VariantType::TRANSFORM2D => TypeTag::Transform2D,
        VariantType::TRANSFORM3D => TypeTag::Transform3D,
        VariantType::COLOR => TypeTag::Color,
        VariantType::STRING => TypeTag::String,
        VariantType::STRING_NAME => TypeTag::StringName,
        VariantType::NODE_PATH => TypeTag::NodePath,
        VariantType::OBJECT => {
            let db = ClassDb::singleton();
            let c = info.class_name.as_str();
            if c.is_empty() || (db.class_exists(c) && db.is_parent_class(c, "Resource")) {
                return Some((TypeTag::Resource, AccessKind::Resource));
            }
            // Nodes and other objects cannot travel.
            return None;
        }
        _ => TypeTag::Variant,
    };
    Some((tag, AccessKind::Prop))
}

/// The type and access of property `name` on `node`, or None when it has no such property.
fn lookup(
    node: &Gd<Node>,
    name: &str,
    script: &[(String, PropInfo, i64)],
) -> Option<(TypeTag, AccessKind)> {
    if name == "playing" {
        return node
            .has_method("is_playing")
            .then_some((TypeTag::Bool, AccessKind::Playing));
    }
    if name == "current_animation_position" && node.has_method("seek") {
        return Some((TypeTag::Float, AccessKind::AnimPosition));
    }
    if let Some((_, info, _)) = script.iter().find(|(n, _, _)| n == name) {
        return tag_of(info);
    }
    let props = class_props(&node.get_class().to_string());
    props.get(name).and_then(tag_of)
}

/// A descriptor with gdnet's defaults, overridden by `entry`'s options.
fn make_desc(name: &str, tag: TypeTag, entry: Option<&Entry>, tick_rate: u32) -> PropDesc {
    let mut d = PropDesc::new(name, tag);
    let schedule = entry.and_then(|e| e.schedule).unwrap_or(Schedule::Cold);
    d.schedule = if tag.cold_only() {
        Schedule::Cold
    } else {
        schedule
    };
    let hot = d.schedule == Schedule::Hot;
    d.quant = entry.and_then(|e| e.quant).unwrap_or(match (hot, tag) {
        (true, TypeTag::Vector2 | TypeTag::Vector3 | TypeTag::Vector4) => Quant::Mm,
        (true, TypeTag::Transform2D | TypeTag::Transform3D) => Quant::Mm,
        (true, TypeTag::Color) => Quant::F16,
        (_, TypeTag::Quaternion) => Quant::Smallest3,
        _ => Quant::Raw,
    });
    let numeric = matches!(
        tag,
        TypeTag::Float
            | TypeTag::Vector2
            | TypeTag::Vector3
            | TypeTag::Quaternion
            | TypeTag::Transform2D
            | TypeTag::Transform3D
            | TypeTag::Color
    );
    d.interpolate = entry.and_then(|e| e.interpolate).unwrap_or(hot && numeric);
    // WP4: the prediction tolerance. Positions and vectors 1 mm: 0.001 in 3D, 0.1 px for the 2D
    // pixel types (D17: one `mm` step at 100 px per metre); a float 1e-4; other types compare
    // exactly whatever the number (predict.rs `within`).
    match tag {
        TypeTag::Float => d.tolerance = 1e-4,
        TypeTag::Vector2 | TypeTag::Transform2D => d.tolerance = 0.1,
        _ => {}
    }
    if let Some(t) = entry.and_then(|e| e.tolerance) {
        d.tolerance = t;
    }
    d.reliable = entry.and_then(|e| e.reliable).unwrap_or(false);
    if let Some(t) = entry.and_then(|e| e.teleport_distance) {
        d.teleport_distance = t.max(0.0);
    }
    if let Some(a) = entry.and_then(|e| e.audience) {
        d.audience = a;
    }
    if let Some(s) = entry.and_then(|e| e.poll_seconds) {
        d.slow_ticks = (s * tick_rate as f32).round().max(1.0) as u32;
    }
    d
}

type PartList = Vec<(PropDesc, FieldSpec)>;

fn add(list: &mut PartList, desc: PropDesc, kind: AccessKind) {
    let spec = FieldSpec {
        name: desc.name.clone(),
        kind,
    };
    match list.iter().position(|(d, _)| d.name == desc.name) {
        Some(i) => list[i] = (desc, spec),
        None => list.push((desc, spec)),
    }
}

/// The profile entries of `node`'s class hierarchy plus its script exports.
fn infer_part(node: &Gd<Node>, profiles: &Profiles, export_vars: bool, tick_rate: u32) -> PartList {
    let mut out: PartList = Vec::new();
    let script = script_props(node);
    let db = ClassDb::singleton();
    let mut class = node.get_class().to_string();
    while !class.is_empty() {
        if let Some(entries) = profiles.classes.get(&class) {
            if entries.is_empty() {
                break;
            }
            for e in entries {
                if e.remove || out.iter().any(|(d, _)| d.name == e.name) {
                    continue;
                }
                if let Some((tag, kind)) = lookup(node, &e.name, &script) {
                    out.push((
                        make_desc(&e.name, tag, Some(e), tick_rate),
                        FieldSpec {
                            name: e.name.clone(),
                            kind,
                        },
                    ));
                }
            }
        }
        class = db.get_parent_class(class.as_str()).to_string();
    }
    if export_vars {
        for (name, info, usage) in &script {
            let exported = usage & USAGE_EDITOR != 0 && usage & USAGE_SCRIPT_VARIABLE != 0;
            let heading = usage & (USAGE_GROUP | USAGE_CATEGORY | USAGE_SUBGROUP) != 0;
            if !exported || heading || out.iter().any(|(d, _)| &d.name == name) {
                continue;
            }
            if let Some((tag, kind)) = tag_of(info) {
                out.push((
                    make_desc(name, tag, None, tick_rate),
                    FieldSpec {
                        name: name.clone(),
                        kind,
                    },
                ));
            }
        }
    }
    out
}

fn part_index(scene: &SceneInfo, part: &str) -> Option<usize> {
    scene.table.index_of(part).map(usize::from)
}

fn entry_from_options(name: &str, opts: Option<&VarDictionary>) -> Entry {
    let mut e = opts.and_then(|d| {
        let mut d = d.clone();
        d.set("name", name);
        Entry::from_dict(&d)
    });
    if e.is_none() {
        e = Some(Entry {
            name: name.into(),
            ..Entry::default()
        });
    }
    e.unwrap_or_default()
}

/// The table of the scene `root` is an instance of, derived (or taken from the cache).
pub fn derive(
    root: &Gd<Node>,
    scene: &SceneInfo,
    profiles: &Rc<Profiles>,
    perturb: &str,
    tick_rate: u32,
) -> Rc<Inferred> {
    let cache_key = format!(
        "{}\u{1}{:016x}\u{1}{:016x}\u{1}{}\u{1}{}",
        scene.path,
        scene.parts_hash,
        profiles.hash(),
        perturb,
        tick_rate
    );
    if let Some(hit) = TABLES.with(|t| t.borrow().get(&cache_key).cloned()) {
        return hit;
    }
    let export_vars = profiles::export_vars();
    let ne: Option<Gd<NetEntity>> = net_entity_of(root);
    let replace = ne.as_ref().is_some_and(|n| !n.bind().get_inherit_profile());
    let nodes: Vec<Option<Gd<Node>>> = (0..scene.part_count())
        .map(|i| scene.part_node(root, i))
        .collect();
    let mut parts: Vec<PartList> = nodes
        .iter()
        .map(|n| match n {
            Some(n) if !replace => infer_part(n, profiles, export_vars, tick_rate),
            _ => Vec::new(),
        })
        .collect();
    let script_of: Vec<Vec<(String, PropInfo, i64)>> = nodes
        .iter()
        .map(|n| n.as_ref().map(script_props).unwrap_or_default())
        .collect();
    let add_path = |parts: &mut Vec<PartList>, path: &PropPath, entry: &Entry, what: &str| {
        let Some(i) = part_index(scene, &path.0) else {
            godot_error!(
                "gdnet: {what} names part {:?} of {}, which it does not have",
                path.0,
                scene.path
            );
            return;
        };
        let Some(node) = nodes[i].as_ref() else {
            return;
        };
        match lookup(node, &path.1, &script_of[i]) {
            Some((tag, kind)) => add(
                &mut parts[i],
                make_desc(&path.1, tag, Some(entry), tick_rate),
                kind,
            ),
            None => godot_error!(
                "gdnet: {what} names {}:{} of {}, which is not a property that can replicate",
                path.0,
                path.1,
                scene.path
            ),
        }
    };
    // NetEntity lists.
    if let Some(ne) = &ne {
        let b = ne.bind();
        let opts = b.get_state_options();
        for p in b.get_state_properties().iter_shared() {
            let text = p.to_string();
            let path = configure::parse_path(&text, "");
            let o = opts
                .get(text.as_str())
                .or_else(|| opts.get(path.1.as_str()))
                .and_then(|v| v.try_to::<VarDictionary>().ok());
            let entry = entry_from_options(&path.1, o.as_ref());
            add_path(&mut parts, &path, &entry, "NetEntity.state_properties");
        }
    }
    // configure() operations.
    for op in configure::ops_for(&scene.path) {
        match op {
            Op::Add(path) => {
                let present = part_index(scene, &path.0)
                    .is_some_and(|i| parts[i].iter().any(|(d, _)| d.name == path.1));
                if !present {
                    let entry = entry_from_options(&path.1, None);
                    add_path(&mut parts, &path, &entry, "Net.configure() state");
                }
            }
            Op::Remove(path) => {
                if let Some(i) = part_index(scene, &path.0) {
                    parts[i].retain(|(d, _)| d.name != path.1);
                }
            }
            Op::Quantize(path, q) => with_desc(scene, &mut parts, &path, |d| d.quant = q),
            Op::Send(path, s) => with_desc(scene, &mut parts, &path, |d| {
                if !d.ty.cold_only() {
                    d.schedule = s;
                }
            }),
            Op::Tolerance(path, t) => match path {
                Some(path) => with_desc(scene, &mut parts, &path, |d| d.tolerance = t),
                None => parts
                    .iter_mut()
                    .flatten()
                    .for_each(|(d, _)| d.tolerance = t),
            },
            Op::Interpolate(path, on) => match path {
                Some(path) => with_desc(scene, &mut parts, &path, |d| d.interpolate = on),
                None => parts
                    .iter_mut()
                    .flatten()
                    .for_each(|(d, _)| d.interpolate = on),
            },
            Op::Teleport(path, t) => match path {
                Some(path) => with_desc(scene, &mut parts, &path, |d| d.teleport_distance = t),
                None => parts
                    .iter_mut()
                    .flatten()
                    .for_each(|(d, _)| d.teleport_distance = t),
            },
            Op::Audience(path, a) => with_desc(scene, &mut parts, &path, |d| d.audience = a),
        }
    }
    // Debug hook: perturb one property's quantiser on this side.
    if !perturb.is_empty() {
        for (d, _) in parts.iter_mut().flatten() {
            if d.name == perturb {
                d.quant = if d.quant == Quant::Raw {
                    Quant::Mm
                } else {
                    Quant::Raw
                };
            }
        }
    }
    let mut table = Vec::new();
    let mut specs = Vec::new();
    for list in parts {
        let (d, s): (Vec<PropDesc>, Vec<FieldSpec>) = list.into_iter().unzip();
        table.push(d);
        specs.push(s);
    }
    let inferred = Rc::new(Inferred {
        table: ScenePropsTable::new(table),
        specs,
        part_names: scene.table.entries.iter().map(|(p, _)| p.clone()).collect(),
    });
    configure::mark_built(&scene.path);
    TABLES.with(|t| t.borrow_mut().insert(cache_key, inferred.clone()));
    inferred
}

fn with_desc(
    scene: &SceneInfo,
    parts: &mut [PartList],
    path: &PropPath,
    f: impl FnOnce(&mut PropDesc),
) {
    if let Some(i) = part_index(scene, &path.0) {
        if let Some((d, _)) = parts[i].iter_mut().find(|(d, _)| d.name == path.1) {
            f(d);
        }
    }
}

/// How the manager reaches a field at run time.
#[derive(Clone, Debug)]
pub enum Accessor {
    Prop(StringName),
    Playing,
    AnimPosition(StringName),
    Resource(StringName),
}

#[derive(Clone, Debug)]
pub struct RtField {
    pub acc: Accessor,
    pub ty: TypeTag,
    /// WP5: the `interpolate` option (replicas sample it; owners blend it after a rollback).
    pub interp: bool,
}

/// A derived table with cached `StringName`s and hot/cold field lists, owned by a manager.
pub struct RuntimeProps {
    pub fields: Vec<Vec<RtField>>,
    pub hot: Vec<Vec<u16>>,
    pub cold: Vec<Vec<u16>>,
}

impl RuntimeProps {
    pub fn new(inferred: Rc<Inferred>) -> Self {
        let mut fields = Vec::new();
        let mut hot = Vec::new();
        let mut cold = Vec::new();
        for (descs, specs) in inferred.table.parts.iter().zip(&inferred.specs) {
            let mut f = Vec::new();
            let mut h = Vec::new();
            let mut c = Vec::new();
            for (i, (d, s)) in descs.iter().zip(specs).enumerate() {
                let name = StringName::from(s.name.as_str());
                let acc = match s.kind {
                    AccessKind::Prop => Accessor::Prop(name),
                    AccessKind::Playing => Accessor::Playing,
                    AccessKind::AnimPosition => Accessor::AnimPosition(name),
                    AccessKind::Resource => Accessor::Resource(name),
                };
                f.push(RtField {
                    acc,
                    ty: d.ty,
                    interp: d.interpolate,
                });
                if d.schedule == Schedule::Hot {
                    h.push(i as u16);
                } else {
                    c.push(i as u16);
                }
            }
            fields.push(f);
            hot.push(h);
            cold.push(c);
        }
        Self { fields, hot, cold }
    }
}

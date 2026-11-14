//! `NetProfiles` (docs/PLAN_NETCODE.md 2.1 "Profiles", 3.1, 3.4, WP3): which properties of which classes
//! replicate, as data.
//!
//! A `NetProfiles` resource maps class names to arrays of property entries:
//! `{name, schedule? ("hot" | "cold"; "send" is accepted too), quantize? ("raw" | "mm" | "cm" | "deg" |
//! "smallest3" | "f16"), interpolate?, tolerance?, reliable?, poll_seconds?, teleport_distance? (WP5),
//! audience? ("all" | "owner", WP16), remove?}`. A class mapped
//! to an empty array syncs nothing and stops the walk up its class hierarchy (cameras, timers, audio
//! players). `merge(other)` lays `other` over this one: an entry replaces the entry of the same name,
//! `{name, remove: true}` drops it, an empty array empties the class.
//!
//! The effective profiles are: `addons/gdnet/default_profiles.tres` (the shipped table; the same as
//! [`builtin`], a test keeps them equal), then the resource named by the project setting
//! `gdnet/sync/profiles`, then `Net.profiles`; a `NetManager.profiles` override goes on top for that
//! manager. Their hash is a `Hello` protocol item (3.3).
//!
//! Globals (the effective profiles and the `gdnet/sync/*`, `gdnet/budget/*` settings, with `Net`'s
//! overrides) live in a thread-local of plain Rust data, never `Gd` handles.

use godot::classes::{IResource, ProjectSettings, Resource, ResourceLoader};
use godot::prelude::*;
use netcore::entity::fnv1a64;
use netcore::{Quant, Schedule};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

pub const DEFAULT_PROFILES_PATH: &str = "res://addons/gdnet/default_profiles.tres";
pub const SETTING_PROFILES: &str = "gdnet/sync/profiles";
pub const SETTING_COLD_POLL: &str = "gdnet/sync/cold_poll_ticks";
/// WP18: polls without a change before an entity is idle (polled at the resting rate); 0 = never.
pub const SETTING_IDLE_AFTER: &str = "gdnet/sync/idle_after_ticks";
/// WP18: idle and sleeping entities are polled every this many ticks (never below the cold rate).
pub const SETTING_RESTING_POLL: &str = "gdnet/sync/resting_poll_ticks";
/// The default of [`SETTING_RESTING_POLL`].
pub const DEFAULT_RESTING_POLL: u32 = 24;
pub const SETTING_EXPORT_VARS: &str = "gdnet/sync/export_vars";
pub const SETTING_BUDGET: &str = "gdnet/budget/bytes_per_second";

/// A number from an int or float Variant (gdext's `try_to::<f64>()` refuses ints).
pub fn number(v: &Variant) -> Option<f64> {
    v.try_to::<f64>()
        .ok()
        .or_else(|| v.try_to::<i64>().ok().map(|i| i as f64))
}

/// One property entry of a profile, parsed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub name: String,
    pub schedule: Option<Schedule>,
    pub quant: Option<Quant>,
    pub interpolate: Option<bool>,
    pub tolerance: Option<f32>,
    pub reliable: Option<bool>,
    pub poll_seconds: Option<f32>,
    /// WP5: a replica snaps instead of blending a correction larger than this (property units).
    pub teleport_distance: Option<f32>,
    /// WP16: `"all"` or `"owner"` (only the entity's owner receives the value).
    pub audience: Option<netcore::Audience>,
    pub remove: bool,
}

impl Entry {
    pub fn from_dict(d: &VarDictionary) -> Option<Entry> {
        let s = |k: &str| {
            d.get(k)
                .and_then(|v| v.try_to::<GString>().ok())
                .map(|g| g.to_string())
        };
        let b = |k: &str| d.get(k).and_then(|v| v.try_to::<bool>().ok());
        let f = |k: &str| d.get(k).and_then(|v| number(&v)).map(|x| x as f32);
        let name = s("name")?;
        Some(Entry {
            name,
            schedule: s("schedule")
                .or_else(|| s("send"))
                .and_then(|x| Schedule::parse(&x)),
            quant: s("quantize").and_then(|x| Quant::parse(&x)),
            interpolate: b("interpolate"),
            tolerance: f("tolerance"),
            reliable: b("reliable"),
            poll_seconds: f("poll_seconds"),
            teleport_distance: f("teleport_distance"),
            audience: s("audience").and_then(|x| netcore::Audience::parse(&x)),
            remove: b("remove").unwrap_or(false),
        })
    }

    pub fn to_dict(&self) -> VarDictionary {
        let mut d = VarDictionary::new();
        d.set("name", self.name.as_str());
        if let Some(s) = self.schedule {
            d.set("schedule", s.name());
        }
        if let Some(q) = self.quant {
            d.set("quantize", q.name());
        }
        if let Some(i) = self.interpolate {
            d.set("interpolate", i);
        }
        if let Some(t) = self.tolerance {
            d.set("tolerance", f64::from(t));
        }
        if let Some(r) = self.reliable {
            d.set("reliable", r);
        }
        if let Some(p) = self.poll_seconds {
            d.set("poll_seconds", f64::from(p));
        }
        if let Some(t) = self.teleport_distance {
            d.set("teleport_distance", f64::from(t));
        }
        if let Some(a) = self.audience {
            d.set("audience", a.name());
        }
        if self.remove {
            d.set("remove", true);
        }
        d
    }

    fn render(&self) -> String {
        let teleport = self
            .teleport_distance
            .map(|t| format!("|teleport {}", t.to_bits()))
            .unwrap_or_default();
        // WP16: entries without an audience render (and hash) as before.
        let audience = self
            .audience
            .map(|a| format!("|audience {}", a.name()))
            .unwrap_or_default();
        format!(
            "{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}{}{}",
            self.name,
            self.schedule.map(Schedule::name),
            self.quant.map(Quant::name),
            self.interpolate,
            self.tolerance.map(f32::to_bits),
            self.reliable,
            self.poll_seconds.map(f32::to_bits),
            self.remove,
            teleport,
            audience
        )
    }
}

/// Parsed profiles: class name -> entries (an empty list = sync nothing, stop the walk).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profiles {
    pub classes: BTreeMap<String, Vec<Entry>>,
}

impl Profiles {
    pub fn from_dict(d: &VarDictionary) -> Profiles {
        let mut classes = BTreeMap::new();
        for (k, v) in d.iter_shared() {
            let Ok(class) = k.try_to::<GString>() else {
                continue;
            };
            let list: Vec<Entry> = v
                .try_to::<VarArray>()
                .map(|a| {
                    a.iter_shared()
                        .filter_map(|e| e.try_to::<VarDictionary>().ok())
                        .filter_map(|e| Entry::from_dict(&e))
                        .collect()
                })
                .unwrap_or_default();
            classes.insert(class.to_string(), list);
        }
        Profiles { classes }
    }

    pub fn to_dict(&self) -> VarDictionary {
        let mut d = VarDictionary::new();
        for (class, list) in &self.classes {
            let mut a = VarArray::new();
            for e in list {
                a.push(&e.to_dict().to_variant());
            }
            d.set(class.as_str(), &a);
        }
        d
    }

    /// Lays `other` over this.
    pub fn merge(&mut self, other: &Profiles) {
        for (class, list) in &other.classes {
            if list.is_empty() {
                self.classes.insert(class.clone(), Vec::new());
                continue;
            }
            let mine = self.classes.entry(class.clone()).or_default();
            for e in list {
                let at = mine.iter().position(|m| m.name == e.name);
                match (at, e.remove) {
                    (Some(i), true) => {
                        mine.remove(i);
                    }
                    (None, true) => {}
                    (Some(i), false) => mine[i] = e.clone(),
                    (None, false) => mine.push(e.clone()),
                }
            }
        }
    }

    /// A stable hash of every class and entry (part of the protocol, 3.3).
    pub fn hash(&self) -> u64 {
        let mut s = String::new();
        for (class, list) in &self.classes {
            s.push_str(class);
            s.push('{');
            for e in list {
                s.push_str(&e.render());
                s.push(';');
            }
            s.push('}');
        }
        fnv1a64(s.as_bytes())
    }
}

fn e(name: &str) -> Entry {
    Entry {
        name: name.into(),
        schedule: Some(Schedule::Cold),
        ..Entry::default()
    }
}

fn hot(name: &str, q: Quant, interpolate: bool) -> Entry {
    Entry {
        name: name.into(),
        schedule: Some(Schedule::Hot),
        quant: Some(q),
        interpolate: Some(interpolate),
        ..Entry::default()
    }
}

fn cold_q(name: &str, q: Quant) -> Entry {
    Entry {
        quant: Some(q),
        ..e(name)
    }
}

/// The shipped defaults: the table of docs/PLAN_NETCODE.md 2.1.
pub fn builtin() -> Profiles {
    let mut c: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    let mut put = |names: &[&str], list: Vec<Entry>| {
        for n in names {
            c.insert((*n).into(), list.clone());
        }
    };
    put(
        &["Node2D"],
        vec![
            hot("position", Quant::Mm, true),
            hot("rotation", Quant::Deg, true),
            hot("scale", Quant::F16, true),
            e("visible"),
            cold_q("modulate", Quant::F16),
            e("z_index"),
        ],
    );
    put(
        &["Node3D"],
        vec![
            hot("position", Quant::Mm, true),
            hot("quaternion", Quant::Smallest3, true),
            hot("scale", Quant::F16, true),
            e("visible"),
        ],
    );
    put(
        &["CharacterBody2D", "CharacterBody3D"],
        vec![hot("velocity", Quant::Mm, true)],
    );
    put(
        &["RigidBody2D", "RigidBody3D"],
        vec![
            hot("linear_velocity", Quant::Mm, true),
            hot("angular_velocity", Quant::Mm, true),
        ],
    );
    put(
        &["AnimatedSprite2D", "AnimatedSprite3D"],
        vec![
            e("animation"),
            e("frame"),
            e("flip_h"),
            e("flip_v"),
            e("speed_scale"),
            e("playing"),
        ],
    );
    put(
        &["AnimationPlayer"],
        vec![
            e("current_animation"),
            e("speed_scale"),
            Entry {
                poll_seconds: Some(1.0),
                ..e("current_animation_position")
            },
        ],
    );
    put(
        &["Sprite2D", "Sprite3D", "TextureRect"],
        vec![e("frame"), e("flip_h"), e("flip_v"), e("texture")],
    );
    put(&["Label", "RichTextLabel"], vec![e("text")]);
    put(
        &["CanvasItem", "Control"],
        vec![e("visible"), cold_q("modulate", Quant::F16)],
    );
    put(
        &[
            "CollisionShape2D",
            "CollisionShape3D",
            "CollisionPolygon2D",
            "CollisionPolygon3D",
        ],
        vec![e("disabled")],
    );
    put(
        &["Light2D"],
        vec![e("energy"), cold_q("color", Quant::F16), e("enabled")],
    );
    put(
        &["Light3D"],
        vec![
            e("light_energy"),
            cold_q("light_color", Quant::F16),
            e("visible"),
        ],
    );
    put(
        &[
            "GPUParticles2D",
            "GPUParticles3D",
            "CPUParticles2D",
            "CPUParticles3D",
        ],
        vec![e("emitting")],
    );
    put(
        &[
            "AudioStreamPlayer",
            "AudioStreamPlayer2D",
            "AudioStreamPlayer3D",
            "Camera2D",
            "Camera3D",
            "Timer",
            "Tween",
        ],
        Vec::new(),
    );
    Profiles { classes: c }
}

/// The profile resource (3.1).
#[derive(GodotClass)]
#[class(tool, init, base = Resource)]
pub struct NetProfiles {
    base: Base<Resource>,
    /// Class name -> Array of property entries (see the module docs and docs/guide/03_state.md).
    #[export]
    classes: VarDictionary,
}

#[godot_api]
impl IResource for NetProfiles {}

#[godot_api]
impl NetProfiles {
    /// Lays `other` over these profiles (entries replace by name, `remove: true` drops, an empty array
    /// empties a class).
    #[func]
    pub fn merge(&mut self, other: Gd<NetProfiles>) {
        let mut mine = self.parsed();
        mine.merge(&other.bind().parsed());
        self.classes = mine.to_dict();
    }

    /// A new resource holding gdnet's built-in defaults (the plan's table).
    #[func]
    pub fn builtin() -> Gd<NetProfiles> {
        Self::from_profiles(&builtin())
    }

    /// The effective global profiles now (defaults, project setting, `Net.profiles`).
    #[func]
    pub fn effective() -> Gd<NetProfiles> {
        Self::from_profiles(&global_profiles())
    }

    /// A stable hash of these profiles.
    #[func]
    pub fn profile_hash(&self) -> i64 {
        self.parsed().hash() as i64
    }
}

impl NetProfiles {
    pub fn parsed(&self) -> Profiles {
        Profiles::from_dict(&self.classes)
    }

    pub fn from_profiles(p: &Profiles) -> Gd<NetProfiles> {
        let mut r = NetProfiles::new_gd();
        r.bind_mut().classes = p.to_dict();
        r
    }
}

/// `Net`'s overrides and the cached effective profiles.
#[derive(Default)]
struct Globals {
    net_profiles: Option<Profiles>,
    cold_poll_ticks: Option<u32>,
    resting_poll_ticks: Option<u32>,
    budget: Option<u32>,
    effective: Option<Rc<Profiles>>,
}

thread_local! {
    static GLOBALS: RefCell<Globals> = RefCell::new(Globals::default());
}

fn setting(name: &str) -> Option<Variant> {
    let ps = ProjectSettings::singleton();
    ps.has_setting(name).then(|| ps.get_setting(name))
}

fn load_profiles(path: &str) -> Option<Profiles> {
    if path.is_empty() || !ResourceLoader::singleton().exists(path) {
        return None;
    }
    let res = ResourceLoader::singleton().load(path)?;
    let p = res.try_cast::<NetProfiles>().ok()?;
    let parsed = p.bind().parsed();
    Some(parsed)
}

/// The effective global profiles (cached until `Net.profiles` changes).
pub fn global_profiles() -> Rc<Profiles> {
    if let Some(p) = GLOBALS.with(|g| g.borrow().effective.clone()) {
        return p;
    }
    let mut p = load_profiles(DEFAULT_PROFILES_PATH).unwrap_or_else(builtin);
    if let Some(path) = setting(SETTING_PROFILES).and_then(|v| v.try_to::<GString>().ok()) {
        if let Some(extra) = load_profiles(&path.to_string()) {
            p.merge(&extra);
        }
    }
    if let Some(extra) = GLOBALS.with(|g| g.borrow().net_profiles.clone()) {
        p.merge(&extra);
    }
    let p = Rc::new(p);
    GLOBALS.with(|g| g.borrow_mut().effective = Some(p.clone()));
    p
}

/// `Net.profiles` changed.
pub fn set_net_profiles(p: Option<Profiles>) {
    GLOBALS.with(|g| {
        let mut g = g.borrow_mut();
        g.net_profiles = p;
        g.effective = None;
    });
}

pub fn set_cold_poll_ticks(v: Option<u32>) {
    GLOBALS.with(|g| g.borrow_mut().cold_poll_ticks = v);
}

pub fn set_resting_poll_ticks(v: Option<u32>) {
    GLOBALS.with(|g| g.borrow_mut().resting_poll_ticks = v);
}

pub fn set_budget(v: Option<u32>) {
    GLOBALS.with(|g| g.borrow_mut().budget = v);
}

/// `Net.cold_poll_ticks`, else `gdnet/sync/cold_poll_ticks`, else 6.
pub fn cold_poll_ticks() -> u32 {
    GLOBALS
        .with(|g| g.borrow().cold_poll_ticks)
        .or_else(|| {
            setting(SETTING_COLD_POLL)
                .and_then(|v| v.try_to::<i64>().ok())
                .map(|v| v as u32)
        })
        .unwrap_or(6)
        .max(1)
}

/// WP18: `Net.resting_poll_ticks`, else `gdnet/sync/resting_poll_ticks`, else 24 (read every tick).
/// Not raised to the cold rate here: the caller does that with its own effective cold rate.
pub fn resting_poll_ticks() -> u32 {
    GLOBALS
        .with(|g| g.borrow().resting_poll_ticks)
        .or_else(|| {
            setting(SETTING_RESTING_POLL)
                .and_then(|v| v.try_to::<i64>().ok())
                .map(|v| v.clamp(1, i64::from(u32::MAX)) as u32)
        })
        .unwrap_or(DEFAULT_RESTING_POLL)
        .max(1)
}

/// WP18: `gdnet/sync/idle_after_ticks`, else 30 (read every tick, so a change applies at once).
pub fn idle_after_ticks() -> u32 {
    setting(SETTING_IDLE_AFTER)
        .and_then(|v| v.try_to::<i64>().ok())
        .map_or(30, |v| v.clamp(0, i64::from(u32::MAX)) as u32)
}

/// `Net.budget_bytes_per_s`, else `gdnet/budget/bytes_per_second`, else 32,768.
pub fn budget_bytes_per_s() -> u32 {
    GLOBALS
        .with(|g| g.borrow().budget)
        .or_else(|| {
            setting(SETTING_BUDGET)
                .and_then(|v| v.try_to::<i64>().ok())
                .map(|v| v as u32)
        })
        .unwrap_or(netcore::session::DEFAULT_BUDGET)
        .max(1)
}

/// `gdnet/sync/export_vars` (D12), default true.
pub fn export_vars() -> bool {
    setting(SETTING_EXPORT_VARS)
        .and_then(|v| v.try_to::<bool>().ok())
        .unwrap_or(true)
}

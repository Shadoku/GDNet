//! `NetInput` and input sampling (docs/PLAN_NETCODE.md 2.1 "Input (`NetInput`)", 3.1, WP4).
//!
//! `Net.input(node)` returns the `NetInput` of the entity `node` belongs to. It mirrors `Input`'s query
//! API (`is_action_pressed`, `is_action_just_pressed`, `is_action_just_released`, `get_action_strength`,
//! `get_axis`, `get_vector`) plus custom values (`get(name)` / `set(name, value)`), `actions()` and
//! `tick`. It has two modes:
//!
//! * *live*: every query goes to the real `Input`. Used for entities the server owns (on any peer),
//!   offline, for unmanaged nodes and before an entity is spawned, so single-player code is unchanged.
//!   Custom values are kept on the object.
//! * *frame*: queries read an input frame. On the owner (a client) the frame its manager sampled from
//!   the real `Input` for this tick (with `input_delay_ticks`, the frame sampled that many ticks ago;
//!   during a replay, the frame of the replayed tick); on the server, for an entity a client owns, the
//!   frame received for this tick (or the last one repeated). Edges come from this frame and the one
//!   the tick before used, so they are the same on both sides and in a replay. `set()` writes into the
//!   frame being sampled, on the owner only and only in a fresh tick; elsewhere it is ignored.
//!
//! The action list of an entity ([`ActionTable`]) is the manager's `input_actions` (default: the project
//! setting `gdnet/input/actions`, else every `InputMap` action except `ui_*`, sorted), narrowed by the
//! entity's `NetEntity.input_actions`. It is part of the scene's property table, so peers agree on the
//! indices.

use crate::tick;
use godot::classes::{IRefCounted, Input, InputMap, RefCounted};
use godot::global::{bytes_to_var, var_to_bytes};
use godot::prelude::*;
use netcore::input::{name_hash, InputFrame};
use netcore::Value;
use std::collections::HashMap;
use std::rc::Rc;

/// An entity's input actions, in index order.
#[derive(Debug, Default)]
pub struct ActionTable {
    pub names: Vec<StringName>,
    pub strings: Vec<String>,
    index: HashMap<String, usize>,
}

impl ActionTable {
    pub fn new(strings: Vec<String>) -> Self {
        let index = strings
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i))
            .collect();
        Self {
            names: strings
                .iter()
                .map(|s| StringName::from(s.as_str()))
                .collect(),
            strings,
            index,
        }
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }
}

/// The action list from a manager's `input_actions` (empty = the default) and an entity's
/// `NetEntity.input_actions` (empty = no narrowing).
pub fn action_list(manager_actions: &[String], entity_actions: &[String]) -> Vec<String> {
    let mut base: Vec<String> = if !manager_actions.is_empty() {
        manager_actions.to_vec()
    } else {
        let s = tick::input_actions_setting();
        if !s.is_empty() {
            s
        } else {
            let mut all: Vec<String> = InputMap::singleton()
                .get_actions()
                .iter_shared()
                .map(|a| a.to_string())
                .filter(|a| !a.starts_with("ui_"))
                .collect();
            all.sort();
            all
        }
    };
    base.dedup();
    if !entity_actions.is_empty() {
        base.retain(|a| entity_actions.contains(a));
    }
    base
}

/// Samples the real `Input` for the actions of `table`.
pub fn sample(table: &ActionTable) -> InputFrame {
    let input = Input::singleton();
    let mut f = InputFrame::new(table.len());
    for (i, name) in table.names.iter().enumerate() {
        let pressed = input.is_action_pressed(name);
        let raw = if pressed {
            input.get_action_raw_strength(name)
        } else {
            0.0
        };
        f.set_action(i, pressed, raw);
    }
    f
}

/// A Variant as a custom input value (`None` for objects and other values that cannot travel).
pub fn to_input_value(v: &Variant) -> Option<Value> {
    Some(match v.get_type() {
        VariantType::NIL => Value::Nil,
        VariantType::BOOL => Value::Bool(v.to()),
        VariantType::INT => Value::Int(v.to()),
        VariantType::FLOAT => Value::Float(v.to()),
        VariantType::VECTOR2 => {
            let x: Vector2 = v.to();
            Value::Vector2([x.x, x.y])
        }
        VariantType::VECTOR3 => {
            let x: Vector3 = v.to();
            Value::Vector3([x.x, x.y, x.z])
        }
        VariantType::VECTOR4 => {
            let x: Vector4 = v.to();
            Value::Vector4([x.x, x.y, x.z, x.w])
        }
        VariantType::VECTOR2I => {
            let x: Vector2i = v.to();
            Value::Vector2i([x.x, x.y])
        }
        VariantType::VECTOR3I => {
            let x: Vector3i = v.to();
            Value::Vector3i([x.x, x.y, x.z])
        }
        VariantType::COLOR => {
            let c: Color = v.to();
            Value::Color([c.r, c.g, c.b, c.a])
        }
        VariantType::STRING | VariantType::STRING_NAME | VariantType::NODE_PATH => {
            Value::Str(v.to_string())
        }
        VariantType::OBJECT | VariantType::CALLABLE | VariantType::SIGNAL | VariantType::RID => {
            return None
        }
        _ => Value::Bytes(var_to_bytes(v).to_vec()),
    })
}

/// A custom input value as a Variant.
pub fn from_input_value(v: &Value) -> Variant {
    match v {
        Value::Str(s) => GString::from(s.as_str()).to_variant(),
        Value::Bytes(b) => bytes_to_var(&PackedByteArray::from(b.as_slice())),
        other => crate::apply::to_variant(other, netcore::TypeTag::Variant),
    }
}

#[derive(GodotClass)]
#[class(base = RefCounted)]
pub struct NetInput {
    base: Base<RefCounted>,
    live: bool,
    actions: Rc<ActionTable>,
    current: InputFrame,
    prev: InputFrame,
    /// The frame being sampled (owner, fresh tick): where `set()` writes.
    sample: Option<InputFrame>,
    /// `current` is the sample itself (no input delay): `set()` writes both.
    current_is_sample: bool,
    live_custom: HashMap<String, Variant>,
    /// The tick this input is for (the predicted tick on an owner, the server's tick on the server).
    #[var(get = get_tick, no_set)]
    tick: i64,
}

#[godot_api]
impl IRefCounted for NetInput {
    fn init(base: Base<RefCounted>) -> Self {
        Self {
            base,
            live: true,
            actions: Rc::new(ActionTable::default()),
            current: InputFrame::default(),
            prev: InputFrame::default(),
            sample: None,
            current_is_sample: false,
            live_custom: HashMap::new(),
            tick: 0,
        }
    }
}

#[godot_api]
impl NetInput {
    #[func]
    fn get_tick(&self) -> i64 {
        self.tick
    }

    /// True when the action is pressed in this tick's frame (live: `Input.is_action_pressed`).
    #[func]
    fn is_action_pressed(&self, action: StringName) -> bool {
        if self.live {
            return Input::singleton().is_action_pressed(&action);
        }
        self.index(&action)
            .is_some_and(|i| self.current.is_pressed(i))
    }

    /// True when the action is pressed in this tick's frame and was not in the frame the tick before used.
    #[func]
    fn is_action_just_pressed(&self, action: StringName) -> bool {
        if self.live {
            return Input::singleton().is_action_just_pressed(&action);
        }
        self.index(&action)
            .is_some_and(|i| self.current.just_pressed(&self.prev, i))
    }

    #[func]
    fn is_action_just_released(&self, action: StringName) -> bool {
        if self.live {
            return Input::singleton().is_action_just_released(&action);
        }
        self.index(&action)
            .is_some_and(|i| self.current.just_released(&self.prev, i))
    }

    /// The action's strength with its deadzone applied, 0..1.
    #[func]
    fn get_action_strength(&self, action: StringName) -> f32 {
        if self.live {
            return Input::singleton().get_action_strength(&action);
        }
        let raw = self.raw(&action);
        if raw <= 0.0 {
            return 0.0;
        }
        let dz = deadzone(&action);
        if raw >= 1.0 || dz <= 0.0 {
            return raw.min(1.0);
        }
        ((raw - dz) / (1.0 - dz)).clamp(0.0, 1.0)
    }

    /// `get_action_strength(positive) - get_action_strength(negative)`.
    #[func]
    fn get_axis(&self, negative_action: StringName, positive_action: StringName) -> f32 {
        if self.live {
            return Input::singleton().get_axis(&negative_action, &positive_action);
        }
        self.get_action_strength(positive_action) - self.get_action_strength(negative_action)
    }

    /// As `Input.get_vector`: raw strengths, a circular deadzone (default: the mean of the four actions'
    /// deadzones), the length clamped to 1.
    #[func]
    fn get_vector(
        &self,
        negative_x: StringName,
        positive_x: StringName,
        negative_y: StringName,
        positive_y: StringName,
        #[opt(default = -1.0)] deadzone_value: f32,
    ) -> Vector2 {
        if self.live {
            return Input::singleton()
                .get_vector_ex(&negative_x, &positive_x, &negative_y, &positive_y)
                .deadzone(deadzone_value)
                .done();
        }
        let v = Vector2::new(
            self.raw(&positive_x) - self.raw(&negative_x),
            self.raw(&positive_y) - self.raw(&negative_y),
        );
        let dz = if deadzone_value < 0.0 {
            0.25 * (deadzone(&positive_x)
                + deadzone(&negative_x)
                + deadzone(&positive_y)
                + deadzone(&negative_y))
        } else {
            deadzone_value
        };
        let len = v.length();
        if len <= dz {
            Vector2::ZERO
        } else if len > 1.0 {
            v / len
        } else {
            v * (((len - dz) / (1.0 - dz)) / len)
        }
    }

    /// A custom value written with `set(name, value)` this tick (null when absent).
    #[func(rename = get)]
    fn get_value(&self, name: GString) -> Variant {
        if self.live {
            return self
                .live_custom
                .get(&name.to_string())
                .cloned()
                .unwrap_or_default();
        }
        self.current
            .get(name_hash(&name.to_string()))
            .map(from_input_value)
            .unwrap_or_default()
    }

    /// Writes a custom value into the frame being sampled (on the owner, in a fresh tick, typically from
    /// `_net_input()`); ignored elsewhere. Objects cannot travel.
    #[func(rename = set)]
    fn set_value(&mut self, name: GString, value: Variant) {
        if self.live {
            self.live_custom.insert(name.to_string(), value);
            return;
        }
        if self.sample.is_none() || !tick::is_fresh() {
            return;
        }
        let Some(v) = to_input_value(&value) else {
            godot_error!(
                "NetInput.set({name:?}): a {:?} cannot travel as input; use a number, vector, string or other plain value",
                value.get_type()
            );
            return;
        };
        let h = name_hash(&name.to_string());
        if let Some(s) = self.sample.as_mut() {
            s.set(h, v.clone());
        }
        if self.current_is_sample {
            self.current.set(h, v);
        }
    }

    /// The actions this input carries (live: every InputMap action but `ui_*`).
    #[func]
    fn actions(&self) -> PackedStringArray {
        if self.live && self.actions.len() == 0 {
            return action_list(&[], &[])
                .iter()
                .map(|s| GString::from(s.as_str()))
                .collect();
        }
        self.actions
            .strings
            .iter()
            .map(|s| GString::from(s.as_str()))
            .collect()
    }

    /// True while this input reads the real `Input` (entities the server owns, offline, unmanaged).
    #[func]
    fn is_live(&self) -> bool {
        self.live
    }
}

fn deadzone(action: &StringName) -> f32 {
    let mut map = InputMap::singleton();
    if map.has_action(action) {
        map.action_get_deadzone(action)
    } else {
        0.0
    }
}

impl NetInput {
    fn index(&self, action: &StringName) -> Option<usize> {
        self.actions.index_of(&action.to_string())
    }

    fn raw(&self, action: &StringName) -> f32 {
        self.index(action).map_or(0.0, |i| self.current.strength(i))
    }

    /// A live input (reads the real `Input`).
    pub fn new_live() -> Gd<NetInput> {
        NetInput::new_gd()
    }

    pub fn set_actions(&mut self, actions: Rc<ActionTable>) {
        self.actions = actions;
    }

    pub fn set_live(&mut self, tick: i64) {
        self.live = true;
        self.sample = None;
        self.current_is_sample = false;
        self.tick = tick;
    }

    /// Frame mode: queries read `current`, edges against `prev`; `sample` (owner, fresh) takes `set()`.
    pub fn set_frames(
        &mut self,
        tick: i64,
        current: InputFrame,
        prev: InputFrame,
        sample: Option<InputFrame>,
        current_is_sample: bool,
    ) {
        self.live = false;
        self.tick = tick;
        self.current = current;
        self.prev = prev;
        self.sample = sample;
        self.current_is_sample = current_is_sample;
    }

    /// Writes a custom value into the sample (and the current frame when it is the sample): what the
    /// manager does for `input_properties`.
    pub fn write_custom(&mut self, hash: u32, v: Value) {
        if let Some(s) = self.sample.as_mut() {
            s.set(hash, v.clone());
        }
        if self.current_is_sample {
            self.current.set(hash, v);
        }
    }

    /// The sample with the values `set()` wrote, taken for sending (owner, post-tick).
    pub fn take_sample(&mut self) -> Option<InputFrame> {
        self.sample.take()
    }

    pub fn current(&self) -> &InputFrame {
        &self.current
    }
}

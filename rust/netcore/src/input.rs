//! Input frames (docs/PLAN_NETCODE.md 2.1 "Input (`NetInput`)", 2.4 `Input`, 2.5, WP4).
//!
//! The owner of an entity samples its input map once per tick into an [`InputFrame`]: a bitset of the
//! pressed actions (indices into the entity's action list, which is part of the scene's property table
//! so both peers agree on it), one byte per analog strength for pressed actions whose strength is not
//! full (a stick), and custom values written with `NetInput.set(name, value)` (names hashed to `u32`,
//! values packed with the raw quantiser and a 5-bit type tag). Frames travel in the unreliable `Input`
//! message, stamped with the server tick they are meant for, the last [`REDUNDANCY`] frames per owned
//! entity in every packet, so up to two consecutive losses cost nothing.
//!
//! The server keeps an [`InputBuffer`] per client-owned entity indexed by tick. Each tick it takes the
//! frame for that tick, or repeats the last frame it used when the frame did not arrive (counted as
//! `repeated`). The owner keeps its own frames ([`OwnerInputs`]) for replay. Edges (just pressed / just
//! released) are never sent: they are derived from the frame a tick used and the frame the tick before
//! used, on both sides, so a replay sees exactly the edges the first run saw.
//!
//! [`AdaptiveDelay`] is the experimental controller of `Net.adaptive_delay` (D10): while an interaction
//! is flagged it ramps the extra input delay toward a target, and back down gradually when it ends.

use crate::entity::{fnv1a64, NetId};
use crate::props::{Quant, TypeTag, Value};
use crate::quant::{self, BitReader, BitWriter};
use std::collections::{BTreeMap, VecDeque};
use std::vec::Vec;

/// Frames per entity in each `Input` message (the newest and the two before it).
pub const REDUNDANCY: usize = 3;
/// At most this many actions per entity (the frame's action count is a varint; this bounds decoding).
pub const MAX_ACTIONS: usize = 1024;
/// At most this many custom values per frame.
pub const MAX_CUSTOM: usize = 64;
/// A full strength byte (a pressed key).
pub const FULL: u8 = 255;

/// The wire hash of a custom input name.
pub fn name_hash(name: &str) -> u32 {
    let h = fnv1a64(name.as_bytes());
    (h ^ (h >> 32)) as u32
}

/// One tick of one entity's input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputFrame {
    /// Per action: pressed.
    pub pressed: Vec<bool>,
    /// Per action: raw strength, 0..=255 (0 when not pressed).
    pub strength: Vec<u8>,
    /// Custom values by name hash, sorted by hash.
    pub custom: Vec<(u32, Value)>,
}

impl InputFrame {
    /// A frame with `actions` actions, nothing pressed.
    pub fn new(actions: usize) -> Self {
        Self {
            pressed: std::vec![false; actions],
            strength: std::vec![0; actions],
            custom: Vec::new(),
        }
    }

    pub fn action_count(&self) -> usize {
        self.pressed.len()
    }

    /// Sets action `i`: pressed or not, with a raw strength in 0..=1 (quantised to a byte).
    pub fn set_action(&mut self, i: usize, pressed: bool, strength: f32) {
        if i >= self.pressed.len() {
            self.pressed.resize(i + 1, false);
            self.strength.resize(i + 1, 0);
        }
        self.pressed[i] = pressed;
        self.strength[i] = if pressed {
            (strength.clamp(0.0, 1.0) * 255.0).round() as u8
        } else {
            0
        };
    }

    pub fn is_pressed(&self, i: usize) -> bool {
        self.pressed.get(i).copied().unwrap_or(false)
    }

    /// Raw strength of action `i` in 0..=1 (0 when not pressed).
    pub fn strength(&self, i: usize) -> f32 {
        if !self.is_pressed(i) {
            return 0.0;
        }
        f32::from(self.strength.get(i).copied().unwrap_or(0)) / 255.0
    }

    /// Pressed in this frame and not in `prev` (the frame the tick before used).
    pub fn just_pressed(&self, prev: &InputFrame, i: usize) -> bool {
        self.is_pressed(i) && !prev.is_pressed(i)
    }

    pub fn just_released(&self, prev: &InputFrame, i: usize) -> bool {
        !self.is_pressed(i) && prev.is_pressed(i)
    }

    pub fn get(&self, hash: u32) -> Option<&Value> {
        self.custom
            .binary_search_by_key(&hash, |(h, _)| *h)
            .ok()
            .map(|i| &self.custom[i].1)
    }

    /// Sets a custom value (a value without a wire type, `Nil`, removes it).
    pub fn set(&mut self, hash: u32, value: Value) {
        let tag = tag_of(&value);
        match self.custom.binary_search_by_key(&hash, |(h, _)| *h) {
            Ok(i) => {
                if tag.is_some() {
                    self.custom[i].1 = value;
                } else {
                    self.custom.remove(i);
                }
            }
            Err(i) => {
                if tag.is_some() && self.custom.len() < MAX_CUSTOM {
                    self.custom.insert(i, (hash, value));
                }
            }
        }
    }

    /// Custom values pass through the raw quantiser (f32 vector components), so the owner reads what
    /// the server will read. Call after writing them.
    pub fn canonical(&self) -> InputFrame {
        let mut w = BitWriter::new();
        write_frame(&mut w, self);
        let bytes = w.into_bytes();
        read_frame(&mut BitReader::new(&bytes)).unwrap_or_else(|| self.clone())
    }
}

/// The wire tags of custom value types (5 bits).
const TAGS: [TypeTag; 17] = [
    TypeTag::Bool,
    TypeTag::Int,
    TypeTag::Float,
    TypeTag::Vector2,
    TypeTag::Vector3,
    TypeTag::Vector4,
    TypeTag::Vector2i,
    TypeTag::Vector3i,
    TypeTag::Quaternion,
    TypeTag::Transform2D,
    TypeTag::Transform3D,
    TypeTag::Color,
    TypeTag::String,
    TypeTag::StringName,
    TypeTag::NodePath,
    TypeTag::Resource,
    TypeTag::Variant,
];

/// The type tag a custom value travels with (None for `Nil`).
pub fn tag_of(v: &Value) -> Option<TypeTag> {
    Some(match v {
        Value::Nil => return None,
        Value::Bool(_) => TypeTag::Bool,
        Value::Int(_) => TypeTag::Int,
        Value::Float(_) => TypeTag::Float,
        Value::Vector2(_) => TypeTag::Vector2,
        Value::Vector3(_) => TypeTag::Vector3,
        Value::Vector4(_) => TypeTag::Vector4,
        Value::Vector2i(_) => TypeTag::Vector2i,
        Value::Vector3i(_) => TypeTag::Vector3i,
        Value::Quaternion(_) => TypeTag::Quaternion,
        Value::Transform2D(_) => TypeTag::Transform2D,
        Value::Transform3D(_) => TypeTag::Transform3D,
        Value::Color(_) => TypeTag::Color,
        Value::Str(_) => TypeTag::String,
        Value::Bytes(_) => TypeTag::Variant,
    })
}

/// Packs one frame: action count, pressed bits, an analog bit (+ byte) per pressed action, then the
/// custom values.
pub fn write_frame(w: &mut BitWriter, f: &InputFrame) {
    let n = f.pressed.len().min(MAX_ACTIONS);
    w.write_varint(n as u64);
    for i in 0..n {
        w.write_bool(f.pressed[i]);
    }
    for i in 0..n {
        if f.pressed[i] {
            let s = f.strength.get(i).copied().unwrap_or(FULL);
            w.write_bool(s != FULL);
            if s != FULL {
                w.write_bits(u64::from(s), 8);
            }
        }
    }
    let customs: Vec<&(u32, Value)> = f
        .custom
        .iter()
        .filter(|(_, v)| tag_of(v).is_some())
        .take(MAX_CUSTOM)
        .collect();
    w.write_varint(customs.len() as u64);
    for (h, v) in customs {
        let Some(tag) = tag_of(v) else { continue };
        let idx = TAGS.iter().position(|t| *t == tag).unwrap_or(0);
        w.write_bits(u64::from(*h), 32);
        w.write_bits(idx as u64, 5);
        quant::encode(w, tag, Quant::Raw, v);
    }
}

/// Unpacks a frame [`write_frame`] wrote.
pub fn read_frame(r: &mut BitReader) -> Option<InputFrame> {
    let n = r.read_varint()? as usize;
    if n > MAX_ACTIONS {
        return None;
    }
    let mut f = InputFrame::new(n);
    for i in 0..n {
        f.pressed[i] = r.read_bool()?;
    }
    for i in 0..n {
        if f.pressed[i] {
            f.strength[i] = if r.read_bool()? {
                r.read_bits(8)? as u8
            } else {
                FULL
            };
        }
    }
    let c = r.read_varint()? as usize;
    if c > MAX_CUSTOM {
        return None;
    }
    for _ in 0..c {
        let h = r.read_bits(32)? as u32;
        let tag = *TAGS.get(r.read_bits(5)? as usize)?;
        let v = quant::decode(r, tag, Quant::Raw)?;
        if let Err(i) = f.custom.binary_search_by_key(&h, |(k, _)| *k) {
            f.custom.insert(i, (h, v));
        }
    }
    Some(f)
}

/// The `Input` message body: `tick` (the server tick of the newest frame) and, per owned entity, its
/// frames for `tick`, `tick - 1`, ... (at most [`REDUNDANCY`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputMsg {
    pub tick: u64,
    pub entities: Vec<(NetId, Vec<InputFrame>)>,
}

pub fn encode_msg(m: &InputMsg) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_varint(m.tick);
    w.write_varint(m.entities.len() as u64);
    for (id, frames) in &m.entities {
        w.write_varint(u64::from(*id));
        let n = frames.len().min(REDUNDANCY);
        w.write_bits(n as u64, 2);
        for f in &frames[..n] {
            write_frame(&mut w, f);
        }
    }
    w.into_bytes()
}

pub fn decode_msg(body: &[u8]) -> Option<InputMsg> {
    let mut r = BitReader::new(body);
    let tick = r.read_varint()?;
    let n = r.read_varint()? as usize;
    if n > 65_536 {
        return None;
    }
    let mut entities = Vec::new();
    for _ in 0..n {
        let id = NetId::try_from(r.read_varint()?).ok()?;
        let k = r.read_bits(2)? as usize;
        let mut frames = Vec::with_capacity(k);
        for _ in 0..k {
            frames.push(read_frame(&mut r)?);
        }
        entities.push((id, frames));
    }
    Some(InputMsg { tick, entities })
}

/// What the server uses for one tick of one entity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TakenFrame {
    pub tick: u64,
    pub frame: InputFrame,
    /// The frame the tick before used (for the edges).
    pub prev: InputFrame,
    /// True when the frame for this tick had not arrived and the last one was repeated.
    pub repeated: bool,
}

/// The server's frames of one client-owned entity, by tick.
#[derive(Clone, Debug, Default)]
pub struct InputBuffer {
    frames: BTreeMap<u64, InputFrame>,
    last: Option<(u64, InputFrame)>,
    pub received: u64,
    pub repeated: u64,
}

/// Frames further ahead of the server's tick than this many ticks are dropped (a confused client).
pub const MAX_AHEAD: u64 = 600;

impl InputBuffer {
    /// Stores a frame for `tick`; frames for ticks already used or too far ahead are ignored.
    pub fn insert(&mut self, tick: u64, frame: InputFrame, now_tick: u64) {
        if self.last.as_ref().is_some_and(|(t, _)| tick <= *t) || tick > now_tick + MAX_AHEAD {
            return;
        }
        if !self.frames.contains_key(&tick) {
            self.received += 1;
        }
        self.frames.insert(tick, frame);
    }

    /// The frame for `tick`, or a repeat of the last one used when it did not arrive. Call once per
    /// tick, in tick order.
    pub fn take(&mut self, tick: u64) -> TakenFrame {
        let prev = self
            .last
            .as_ref()
            .map(|(_, f)| f.clone())
            .unwrap_or_default();
        let (frame, repeated) = match self.frames.remove(&tick) {
            Some(f) => (f, false),
            None => {
                self.repeated += 1;
                (prev.clone(), true)
            }
        };
        self.frames = self.frames.split_off(&(tick + 1));
        self.last = Some((tick, frame.clone()));
        TakenFrame {
            tick,
            frame,
            prev,
            repeated,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// The owner's frames per entity, by the tick they apply to (kept for sending and replay).
#[derive(Clone, Debug, Default)]
pub struct OwnerInputs {
    frames: BTreeMap<NetId, BTreeMap<u64, InputFrame>>,
    /// The newest tick a frame was stored for since the last send.
    pending: Option<u64>,
    keep: u64,
}

impl OwnerInputs {
    /// Keeps frames for `keep` ticks behind the newest (at least [`REDUNDANCY`]).
    pub fn new(keep: u64) -> Self {
        Self {
            keep: keep.max(REDUNDANCY as u64),
            ..Self::default()
        }
    }

    pub fn set_keep(&mut self, keep: u64) {
        self.keep = keep.max(REDUNDANCY as u64);
    }

    /// Stores `frame` for `tick`. Ticks skipped since the entity's previous frame (the delay grew) get
    /// a copy of that previous frame, so the server never repeats a stale one.
    pub fn store(&mut self, id: NetId, tick: u64, frame: InputFrame) {
        let m = self.frames.entry(id).or_default();
        if let Some((&last, f)) = m.iter().next_back() {
            if last < tick && tick - last <= 16 {
                let f = f.clone();
                for t in last + 1..tick {
                    m.insert(t, f.clone());
                }
            }
        }
        m.insert(tick, frame);
        let cut = tick.saturating_sub(self.keep);
        if m.keys().next().is_some_and(|k| *k < cut) {
            *m = m.split_off(&cut);
        }
        self.pending = Some(self.pending.map_or(tick, |p| p.max(tick)));
    }

    /// The frame for `tick`, or the newest frame before it (what the server repeats for a tick it has
    /// no frame for, and what a skipped tick gets filled with).
    pub fn get(&self, id: NetId, tick: u64) -> Option<&InputFrame> {
        self.frames
            .get(&id)?
            .range(..=tick)
            .next_back()
            .map(|(_, f)| f)
    }

    pub fn forget(&mut self, id: NetId) {
        self.frames.remove(&id);
    }

    /// The message to send when frames were stored since the last call.
    pub fn take_msg(&mut self) -> Option<InputMsg> {
        let tick = self.pending.take()?;
        let mut entities = Vec::new();
        for (id, m) in &self.frames {
            let mut frames = Vec::new();
            for k in 0..REDUNDANCY as u64 {
                match tick.checked_sub(k).and_then(|t| m.get(&t)) {
                    Some(f) => frames.push(f.clone()),
                    None => break,
                }
            }
            if !frames.is_empty() {
                entities.push((*id, frames));
            }
        }
        (!entities.is_empty()).then_some(InputMsg { tick, entities })
    }
}

/// `Net.adaptive_delay` (experimental, D10): extra input delay that ramps toward `target` ticks while
/// an interaction is flagged (`Session::set_interaction`) and back to zero gradually afterwards.
#[derive(Clone, Debug, PartialEq)]
pub struct AdaptiveDelay {
    pub target: f64,
    /// Ticks of delay added per tick while interacting.
    pub ramp_up: f64,
    /// Ticks of delay removed per tick otherwise.
    pub ramp_down: f64,
    current: f64,
    interacting: bool,
}

impl Default for AdaptiveDelay {
    fn default() -> Self {
        Self {
            target: 0.0,
            ramp_up: 0.25,
            ramp_down: 0.05,
            current: 0.0,
            interacting: false,
        }
    }
}

/// The adaptive delay never exceeds this many ticks.
pub const ADAPTIVE_MAX: f64 = 8.0;

impl AdaptiveDelay {
    pub fn set_interacting(&mut self, on: bool) {
        self.interacting = on;
    }

    pub fn is_interacting(&self) -> bool {
        self.interacting
    }

    /// One tick: moves toward the target (interacting) or toward zero. `target` is clamped to
    /// [0, [`ADAPTIVE_MAX`]].
    pub fn step(&mut self, target: f64) {
        self.target = target.clamp(0.0, ADAPTIVE_MAX);
        if self.interacting {
            if self.current < self.target {
                self.current = (self.current + self.ramp_up).min(self.target);
            } else {
                self.current = (self.current - self.ramp_down).max(self.target);
            }
        } else {
            self.current = (self.current - self.ramp_down).max(0.0);
        }
    }

    /// The fractional delay now.
    pub fn current(&self) -> f64 {
        self.current
    }

    /// The whole ticks of delay to apply now.
    pub fn ticks(&self) -> u32 {
        self.current.round() as u32
    }
}

/// The recent ticks' frames, for edge checks in tests and diagnostics.
pub fn edges(frames: &VecDeque<InputFrame>, i: usize) -> Vec<(bool, bool)> {
    let empty = InputFrame::default();
    frames
        .iter()
        .enumerate()
        .map(|(k, f)| {
            let prev = if k == 0 { &empty } else { &frames[k - 1] };
            (f.just_pressed(prev, i), f.just_released(prev, i))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pressed: &[usize], n: usize) -> InputFrame {
        let mut f = InputFrame::new(n);
        for &i in pressed {
            f.set_action(i, true, 1.0);
        }
        f
    }

    #[test]
    fn frame_round_trip_with_analog_and_custom() {
        let mut f = InputFrame::new(5);
        f.set_action(0, true, 1.0);
        f.set_action(3, true, 0.5);
        f.set(name_hash("aim"), Value::Vector2([3.5, -2.0]));
        f.set(name_hash("slot"), Value::Int(4));
        f.set(name_hash("name"), Value::Str("x".into()));
        let mut w = BitWriter::new();
        write_frame(&mut w, &f);
        // 5 actions, two pressed, one analog byte: the action part fits in under 3 bytes.
        let g = read_frame(&mut BitReader::new(w.as_bytes())).unwrap();
        assert_eq!(g, f);
        assert!(g.is_pressed(3) && !g.is_pressed(1));
        assert!((g.strength(3) - 0.5).abs() < 0.01);
        assert_eq!(g.strength(0), 1.0);
        assert_eq!(g.get(name_hash("slot")), Some(&Value::Int(4)));
        let small = frame(&[1], 6);
        let mut w = BitWriter::new();
        write_frame(&mut w, &small);
        assert!(w.byte_len() <= 3, "{}", w.byte_len());
    }

    #[test]
    fn setting_nil_removes_a_custom_value() {
        let mut f = InputFrame::new(0);
        f.set(7, Value::Float(1.0));
        f.set(7, Value::Nil);
        assert_eq!(f.get(7), None);
    }

    #[test]
    fn message_round_trip() {
        let m = InputMsg {
            tick: 1234,
            entities: std::vec![
                (5, std::vec![frame(&[0], 3), frame(&[], 3), frame(&[2], 3)]),
                (70_000, std::vec![frame(&[1], 2)]),
            ],
        };
        assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        assert_eq!(decode_msg(&[0xff]), None);
    }

    #[test]
    fn frames_land_on_their_tick() {
        let mut b = InputBuffer::default();
        b.insert(10, frame(&[0], 2), 5);
        b.insert(11, frame(&[1], 2), 5);
        let t = b.take(10);
        assert!(!t.repeated && t.frame.is_pressed(0));
        let t = b.take(11);
        assert!(!t.repeated && t.frame.is_pressed(1) && !t.frame.is_pressed(0));
        assert!(t.prev.is_pressed(0), "prev is the frame tick 10 used");
        assert_eq!(b.repeated, 0);
    }

    #[test]
    fn a_gap_repeats_the_last_frame() {
        let mut b = InputBuffer::default();
        b.insert(10, frame(&[0], 2), 5);
        b.take(10);
        let t = b.take(11);
        assert!(t.repeated);
        assert!(t.frame.is_pressed(0));
        assert_eq!(b.repeated, 1);
        // A late frame for a used tick is ignored.
        b.insert(11, frame(&[1], 2), 12);
        assert!(b.is_empty());
        // Too far ahead is ignored.
        b.insert(10_000, frame(&[1], 2), 12);
        assert!(b.is_empty());
    }

    #[test]
    fn redundancy_covers_two_of_three_lost() {
        // The owner stores ticks 1..=9 and sends after each; only every third message arrives.
        let mut owner = OwnerInputs::new(20);
        let mut server = InputBuffer::default();
        for t in 1..=9u64 {
            owner.store(1, t, frame(&[(t % 2) as usize], 2));
            let msg = owner.take_msg().unwrap();
            assert_eq!(msg.tick, t);
            if t % 3 == 0 {
                let m = decode_msg(&encode_msg(&msg)).unwrap();
                for (id, frames) in m.entities {
                    assert_eq!(id, 1);
                    for (k, f) in frames.into_iter().enumerate() {
                        server.insert(m.tick - k as u64, f, 0);
                    }
                }
            }
        }
        for t in 1..=9u64 {
            let got = server.take(t);
            assert!(!got.repeated, "tick {t}");
            assert!(got.frame.is_pressed((t % 2) as usize), "tick {t}");
        }
        assert_eq!(server.repeated, 0);
    }

    #[test]
    fn edges_are_identical_when_recomputed() {
        // A press at 3 held to 5, released at 6: the server's edges equal the owner's from its ring.
        let mut owner = OwnerInputs::new(20);
        let mut server = InputBuffer::default();
        let pattern = [false, false, true, true, true, false, false];
        for (k, p) in pattern.iter().enumerate() {
            let t = k as u64 + 1;
            owner.store(9, t, if *p { frame(&[0], 1) } else { frame(&[], 1) });
            server.insert(t, owner.get(9, t).unwrap().clone(), 0);
        }
        let mut server_edges = Vec::new();
        for t in 1..=7u64 {
            let got = server.take(t);
            server_edges.push((
                got.frame.just_pressed(&got.prev, 0),
                got.frame.just_released(&got.prev, 0),
            ));
        }
        let replayed: VecDeque<InputFrame> = (1..=7u64)
            .map(|t| owner.get(9, t).unwrap().clone())
            .collect();
        assert_eq!(edges(&replayed, 0), server_edges);
        assert_eq!(server_edges.iter().filter(|e| e.0).count(), 1);
        assert_eq!(server_edges.iter().filter(|e| e.1).count(), 1);
        assert_eq!(server_edges[2], (true, false));
        assert_eq!(server_edges[5], (false, true));
    }

    #[test]
    fn a_grown_delay_fills_skipped_ticks() {
        let mut owner = OwnerInputs::new(20);
        owner.store(1, 10, frame(&[0], 1));
        owner.store(1, 13, frame(&[], 1));
        assert!(owner.get(1, 11).unwrap().is_pressed(0));
        assert!(owner.get(1, 12).unwrap().is_pressed(0));
        assert!(!owner.get(1, 13).unwrap().is_pressed(0));
        // Old frames are pruned.
        owner.store(1, 100, frame(&[], 1));
        assert!(owner.get(1, 10).is_none());
    }

    #[test]
    fn adaptive_ramp_is_monotone_and_bounded() {
        let mut a = AdaptiveDelay::default();
        a.step(4.0);
        assert_eq!(a.current(), 0.0, "nothing without an interaction");
        a.set_interacting(true);
        let mut last = 0.0;
        let mut ups = Vec::new();
        for _ in 0..40 {
            a.step(4.0);
            assert!(a.current() >= last, "rises monotonically");
            assert!(a.current() <= 4.0, "never above the target");
            last = a.current();
            ups.push(a.ticks());
        }
        assert_eq!(a.current(), 4.0);
        assert_eq!(ups[0], 0, "ramps in, does not jump");
        a.step(100.0);
        assert!(a.current() <= ADAPTIVE_MAX);
        a.set_interacting(false);
        let mut down = 0;
        while a.current() > 0.0 {
            let before = a.current();
            a.step(4.0);
            assert!(a.current() < before, "falls monotonically");
            down += 1;
        }
        assert!(down > 40, "slower down than up ({down} ticks)");
    }
}

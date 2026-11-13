//! Bit packing and quantisers (docs/PLAN_NETCODE.md 2.1 "Options per property", 2.4 `Snapshot`, WP3).
//!
//! [`BitWriter`] / [`BitReader`] pack values least significant bit first. Integers are zigzag varints in
//! 7-bit groups with a continuation bit; strings and byte strings are a varint length then bytes. A
//! property value is encoded by [`encode`] according to its [`TypeTag`] and [`Quant`] and decoded by
//! [`decode`]. The same packer will carry WP4's input frames.
//!
//! Precision (round trip, see the tests): `mm` 0.0005, `cm` 0.005 of the unit (D17: a 2D `Vector2` and
//! a `Transform2D` origin are pixels at 100 px per metre, so `mm` is 0.05 px and `cm` 0.5 px there), `deg` one turn / 65,536
//! (0.0055 degrees), `smallest3` 2.2e-5 per sent component (6e-5 for the derived largest one), `f16`
//! about 1/2048 relative.

use crate::props::{Quant, TypeTag, Value};
use std::string::String;
use std::vec::Vec;

/// Appends bits to a byte buffer, least significant bit first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitWriter {
    buf: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.bits = 0;
    }

    pub fn bit_len(&self) -> usize {
        self.bits
    }

    pub fn byte_len(&self) -> usize {
        self.buf.len()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Writes the low `n` bits of `value` (n <= 64).
    pub fn write_bits(&mut self, mut value: u64, mut n: u32) {
        if n < 64 {
            value &= (1u64 << n) - 1;
        }
        while n > 0 {
            let off = (self.bits % 8) as u32;
            if off == 0 {
                self.buf.push(0);
            }
            let take = (8 - off).min(n);
            let last = self.buf.len() - 1;
            self.buf[last] |= ((value & ((1u64 << take) - 1)) as u8) << off;
            value = if take == 64 { 0 } else { value >> take };
            n -= take;
            self.bits += take as usize;
        }
    }

    pub fn write_bool(&mut self, b: bool) {
        self.write_bits(u64::from(b), 1);
    }

    pub fn write_varint(&mut self, mut v: u64) {
        loop {
            let group = v & 0x7f;
            v >>= 7;
            self.write_bits(group, 7);
            self.write_bool(v != 0);
            if v == 0 {
                break;
            }
        }
    }

    pub fn write_zigzag(&mut self, v: i64) {
        self.write_varint(((v << 1) ^ (v >> 63)) as u64);
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_varint(bytes.len() as u64);
        self.write_raw(bytes, bytes.len() * 8);
    }

    /// Appends the first `bits` bits of `data` (as another writer produced them).
    pub fn write_raw(&mut self, data: &[u8], bits: usize) {
        let full = bits / 8;
        if self.bits.is_multiple_of(8) {
            self.buf.extend_from_slice(&data[..full]);
            self.bits += full * 8;
        } else {
            for &b in &data[..full] {
                self.write_bits(u64::from(b), 8);
            }
        }
        let rest = (bits % 8) as u32;
        if rest > 0 {
            self.write_bits(u64::from(data[full]), rest);
        }
    }

    pub fn append(&mut self, other: &BitWriter) {
        self.write_raw(&other.buf, other.bits);
    }
}

/// Reads what a [`BitWriter`] wrote. Every read returns `None` past the end.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        (self.buf.len() * 8).saturating_sub(self.pos)
    }

    pub fn skip(&mut self, bits: usize) -> Option<()> {
        if bits > self.remaining() {
            return None;
        }
        self.pos += bits;
        Some(())
    }

    pub fn read_bits(&mut self, mut n: u32) -> Option<u64> {
        if n as usize > self.remaining() {
            return None;
        }
        let mut out = 0u64;
        let mut shift = 0u32;
        while n > 0 {
            let byte = self.buf[self.pos / 8];
            let off = (self.pos % 8) as u32;
            let take = (8 - off).min(n);
            let bits = (u64::from(byte) >> off) & ((1u64 << take) - 1);
            out |= bits << shift;
            shift += take;
            n -= take;
            self.pos += take as usize;
        }
        Some(out)
    }

    pub fn read_bool(&mut self) -> Option<bool> {
        self.read_bits(1).map(|b| b == 1)
    }

    pub fn read_varint(&mut self) -> Option<u64> {
        let mut out = 0u64;
        for i in 0..10 {
            let group = self.read_bits(7)?;
            if i == 9 && group > 1 {
                return None;
            }
            out |= group << (7 * i);
            if !self.read_bool()? {
                return Some(out);
            }
        }
        None
    }

    pub fn read_zigzag(&mut self) -> Option<i64> {
        let v = self.read_varint()?;
        Some(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    pub fn read_bytes(&mut self) -> Option<Vec<u8>> {
        let len = usize::try_from(self.read_varint()?).ok()?;
        if len.checked_mul(8)? > self.remaining() {
            return None;
        }
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(self.read_bits(8)? as u8);
        }
        Some(out)
    }
}

/// Converts an f32 to IEEE half precision bits (round half up on the dropped bits).
pub fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let mant = b & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = (mant | 0x0080_0000) >> (1 - e);
        let half = (m >> 13) + ((m >> 12) & 1);
        return sign | half as u16;
    }
    let half = (u32::from(sign) | ((e as u32) << 10) | (mant >> 13)) + ((mant >> 12) & 1);
    half as u16
}

pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1f);
    let mant = u32::from(h & 0x3ff);
    if exp == 0 {
        let v = mant as f32 * (1.0 / 16_777_216.0);
        return if sign != 0 { -v } else { v };
    }
    if exp == 31 {
        return f32::from_bits(sign | 0x7f80_0000 | (mant << 13));
    }
    f32::from_bits(sign | ((exp + 112) << 23) | (mant << 13))
}

const TURN: f64 = core::f64::consts::TAU;
const S3_MAX: f64 = 32_767.0;

/// D17: 2D lengths are pixels at [`PX_PER_METRE`] pixels per metre, 3D lengths (and every other type)
/// are in the type's own unit, read as metres.
pub const PX_PER_METRE: f64 = 100.0;

/// Steps per unit of a fixed-point quantiser for a component of a value in `units` units per metre
/// (1 for metres and unitless values, [`PX_PER_METRE`] for 2D pixel types): `mm` is a thousandth of a
/// metre, `cm` a hundredth, so in 2D `mm` steps by 0.1 px and `cm` by 1 px.
fn fixed_scale_in(q: Quant, units: f64) -> Option<f64> {
    match q {
        Quant::Mm => Some(1000.0 / units),
        Quant::Cm => Some(100.0 / units),
        _ => None,
    }
}

fn fixed_scale(q: Quant) -> Option<f64> {
    fixed_scale_in(q, 1.0)
}

/// Units per metre of a type's length components (D17): pixels for the 2D vector types.
fn units_of(ty: TypeTag) -> f64 {
    match ty {
        TypeTag::Vector2 | TypeTag::Transform2D => PX_PER_METRE,
        _ => 1.0,
    }
}

/// The step of a fixed-point quantiser for `ty` (`None` for the others): `mm` on a `Vector2` is 0.1.
pub fn fixed_step(ty: TypeTag, q: Quant) -> Option<f64> {
    fixed_scale_in(q, units_of(ty)).map(|s| 1.0 / s)
}

fn write_fixed(w: &mut BitWriter, x: f64, scale: f64) {
    let v = (x * scale).round();
    let v = if v.is_finite() {
        v.clamp(-9.0e15, 9.0e15) as i64
    } else {
        0
    };
    w.write_zigzag(v);
}

fn write_angle(w: &mut BitWriter, a: f64) {
    let a = if a.is_finite() { a } else { 0.0 };
    let q = ((a / TURN) * 65_536.0).round() as i64;
    w.write_bits((q as u64) & 0xffff, 16);
}

fn read_angle(r: &mut BitReader) -> Option<f64> {
    let q = r.read_bits(16)? as u16 as i16;
    Some(f64::from(q) / 65_536.0 * TURN)
}

/// One f32 component with `q` (raw 32 bits, fixed point, half, or an angle), lengths in `units`
/// units per metre.
fn write_comp(w: &mut BitWriter, x: f32, q: Quant, units: f64) {
    if let Some(scale) = fixed_scale_in(q, units) {
        write_fixed(w, f64::from(x), scale);
        return;
    }
    match q {
        Quant::F16 => w.write_bits(u64::from(f32_to_f16(x)), 16),
        Quant::Deg => write_angle(w, f64::from(x)),
        _ => w.write_bits(u64::from(x.to_bits()), 32),
    }
}

fn read_comp(r: &mut BitReader, q: Quant, units: f64) -> Option<f32> {
    if let Some(scale) = fixed_scale_in(q, units) {
        return Some((r.read_zigzag()? as f64 / scale) as f32);
    }
    Some(match q {
        Quant::F16 => f16_to_f32(r.read_bits(16)? as u16),
        Quant::Deg => read_angle(r)? as f32,
        _ => f32::from_bits(r.read_bits(32)? as u32),
    })
}

fn write_comps(w: &mut BitWriter, xs: &[f32], q: Quant) {
    write_comps_in(w, xs, q, 1.0);
}

fn write_comps_in(w: &mut BitWriter, xs: &[f32], q: Quant, units: f64) {
    for &x in xs {
        write_comp(w, x, q, units);
    }
}

fn read_comps<const N: usize>(r: &mut BitReader, q: Quant) -> Option<[f32; N]> {
    read_comps_in(r, q, 1.0)
}

fn read_comps_in<const N: usize>(r: &mut BitReader, q: Quant, units: f64) -> Option<[f32; N]> {
    let mut out = [0.0f32; N];
    for v in out.iter_mut() {
        *v = read_comp(r, q, units)?;
    }
    Some(out)
}

fn write_smallest3(w: &mut BitWriter, q: [f32; 4]) {
    let mut v = q.map(f64::from);
    let len = v.iter().map(|c| c * c).sum::<f64>().sqrt();
    if !(len.is_finite() && len > 1e-12) {
        v = [0.0, 0.0, 0.0, 1.0];
    } else {
        for c in v.iter_mut() {
            *c /= len;
        }
    }
    let mut largest = 0usize;
    for i in 1..4 {
        if v[i].abs() > v[largest].abs() {
            largest = i;
        }
    }
    if v[largest] < 0.0 {
        for c in v.iter_mut() {
            *c = -*c;
        }
    }
    w.write_bits(largest as u64, 2);
    for (i, c) in v.iter().enumerate() {
        if i == largest {
            continue;
        }
        let unit = (c * core::f64::consts::SQRT_2 + 1.0) / 2.0;
        let q = (unit * S3_MAX).round().clamp(0.0, S3_MAX) as u64;
        w.write_bits(q, 15);
    }
}

fn read_smallest3(r: &mut BitReader) -> Option<[f32; 4]> {
    let largest = r.read_bits(2)? as usize;
    let mut v = [0.0f64; 4];
    let mut sum = 0.0;
    for (i, c) in v.iter_mut().enumerate() {
        if i == largest {
            continue;
        }
        let q = r.read_bits(15)? as f64;
        *c = (q / S3_MAX * 2.0 - 1.0) / core::f64::consts::SQRT_2;
        sum += *c * *c;
    }
    v[largest] = (1.0 - sum).max(0.0).sqrt();
    Some(v.map(|c| c as f32))
}

/// The zero value of a type (what a missing or mistyped value encodes as).
pub fn default_value(ty: TypeTag) -> Value {
    match ty {
        TypeTag::Bool => Value::Bool(false),
        TypeTag::Int => Value::Int(0),
        TypeTag::Float => Value::Float(0.0),
        TypeTag::Vector2 => Value::Vector2([0.0; 2]),
        TypeTag::Vector3 => Value::Vector3([0.0; 3]),
        TypeTag::Vector4 => Value::Vector4([0.0; 4]),
        TypeTag::Vector2i => Value::Vector2i([0; 2]),
        TypeTag::Vector3i => Value::Vector3i([0; 3]),
        TypeTag::Quaternion => Value::Quaternion([0.0, 0.0, 0.0, 1.0]),
        TypeTag::Transform2D => Value::Transform2D([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
        TypeTag::Transform3D => {
            Value::Transform3D([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0])
        }
        TypeTag::Color => Value::Color([1.0; 4]),
        TypeTag::String | TypeTag::StringName | TypeTag::NodePath | TypeTag::Resource => {
            Value::Str(String::new())
        }
        TypeTag::Variant => Value::Bytes(Vec::new()),
    }
}

/// Encodes `value` as a property of type `ty` packed with `q`. A value of another shape encodes the
/// type's zero value (it decodes as that).
pub fn encode(w: &mut BitWriter, ty: TypeTag, q: Quant, value: &Value) {
    let fallback;
    let value = if fits(ty, value) {
        value
    } else {
        fallback = default_value(ty);
        &fallback
    };
    match (ty, value) {
        (TypeTag::Bool, Value::Bool(b)) => w.write_bool(*b),
        (TypeTag::Int, Value::Int(i)) => w.write_zigzag(*i),
        (TypeTag::Float, Value::Float(f)) => match q {
            Quant::Mm | Quant::Cm => write_fixed(w, *f, fixed_scale(q).unwrap_or(1.0)),
            Quant::Deg => write_angle(w, *f),
            Quant::F16 => w.write_bits(u64::from(f32_to_f16(*f as f32)), 16),
            _ => w.write_bits(f.to_bits(), 64),
        },
        (TypeTag::Vector2, Value::Vector2(v)) => write_comps_in(w, v, vec_quant(q), PX_PER_METRE),
        (TypeTag::Vector3, Value::Vector3(v)) => write_comps(w, v, vec_quant(q)),
        (TypeTag::Vector4, Value::Vector4(v)) => write_comps(w, v, vec_quant(q)),
        (TypeTag::Vector2i, Value::Vector2i(v)) => {
            v.iter().for_each(|c| w.write_zigzag(i64::from(*c)))
        }
        (TypeTag::Vector3i, Value::Vector3i(v)) => {
            v.iter().for_each(|c| w.write_zigzag(i64::from(*c)))
        }
        (TypeTag::Quaternion, Value::Quaternion(v)) => match q {
            Quant::Smallest3 => write_smallest3(w, *v),
            Quant::F16 => write_comps(w, v, Quant::F16),
            _ => write_comps(w, v, Quant::Raw),
        },
        (TypeTag::Transform2D, Value::Transform2D(v)) => {
            write_comps(w, &v[..4], basis_quant(q));
            write_comps_in(w, &v[4..], vec_quant(q), PX_PER_METRE);
        }
        (TypeTag::Transform3D, Value::Transform3D(v)) => {
            write_comps(w, &v[..9], basis_quant(q));
            write_comps(w, &v[9..], vec_quant(q));
        }
        (TypeTag::Color, Value::Color(v)) => write_comps(w, v, color_quant(q)),
        (
            TypeTag::String | TypeTag::StringName | TypeTag::NodePath | TypeTag::Resource,
            Value::Str(s),
        ) => w.write_bytes(s.as_bytes()),
        (TypeTag::Variant, Value::Bytes(b)) => w.write_bytes(b),
        _ => {}
    }
}

/// Decodes a value [`encode`] wrote with the same type and quantiser.
pub fn decode(r: &mut BitReader, ty: TypeTag, q: Quant) -> Option<Value> {
    Some(match ty {
        TypeTag::Bool => Value::Bool(r.read_bool()?),
        TypeTag::Int => Value::Int(r.read_zigzag()?),
        TypeTag::Float => Value::Float(match q {
            Quant::Mm | Quant::Cm => r.read_zigzag()? as f64 / fixed_scale(q).unwrap_or(1.0),
            Quant::Deg => read_angle(r)?,
            Quant::F16 => f64::from(f16_to_f32(r.read_bits(16)? as u16)),
            _ => f64::from_bits(r.read_bits(64)?),
        }),
        TypeTag::Vector2 => Value::Vector2(read_comps_in(r, vec_quant(q), PX_PER_METRE)?),
        TypeTag::Vector3 => Value::Vector3(read_comps(r, vec_quant(q))?),
        TypeTag::Vector4 => Value::Vector4(read_comps(r, vec_quant(q))?),
        TypeTag::Vector2i => Value::Vector2i([read_i32(r)?, read_i32(r)?]),
        TypeTag::Vector3i => Value::Vector3i([read_i32(r)?, read_i32(r)?, read_i32(r)?]),
        TypeTag::Quaternion => Value::Quaternion(match q {
            Quant::Smallest3 => read_smallest3(r)?,
            Quant::F16 => read_comps(r, Quant::F16)?,
            _ => read_comps(r, Quant::Raw)?,
        }),
        TypeTag::Transform2D => {
            let b: [f32; 4] = read_comps(r, basis_quant(q))?;
            let o: [f32; 2] = read_comps_in(r, vec_quant(q), PX_PER_METRE)?;
            Value::Transform2D([b[0], b[1], b[2], b[3], o[0], o[1]])
        }
        TypeTag::Transform3D => {
            let b: [f32; 9] = read_comps(r, basis_quant(q))?;
            let o: [f32; 3] = read_comps(r, vec_quant(q))?;
            let mut v = [0.0f32; 12];
            v[..9].copy_from_slice(&b);
            v[9..].copy_from_slice(&o);
            Value::Transform3D(v)
        }
        TypeTag::Color => Value::Color(read_comps(r, color_quant(q))?),
        TypeTag::String | TypeTag::StringName | TypeTag::NodePath | TypeTag::Resource => {
            Value::Str(String::from_utf8(r.read_bytes()?).ok()?)
        }
        TypeTag::Variant => Value::Bytes(r.read_bytes()?),
    })
}

fn read_i32(r: &mut BitReader) -> Option<i32> {
    i32::try_from(r.read_zigzag()?).ok()
}

/// Vector components: smallest3 means nothing for a vector (raw).
fn vec_quant(q: Quant) -> Quant {
    if q == Quant::Smallest3 {
        Quant::Raw
    } else {
        q
    }
}

/// A transform's basis is raw unless half precision was asked for (fixed point and angles are for
/// the origin).
fn basis_quant(q: Quant) -> Quant {
    if q == Quant::F16 {
        Quant::F16
    } else {
        Quant::Raw
    }
}

fn color_quant(q: Quant) -> Quant {
    match q {
        Quant::F16 => Quant::F16,
        Quant::Mm | Quant::Cm => q,
        _ => Quant::Raw,
    }
}

fn fits(ty: TypeTag, v: &Value) -> bool {
    matches!(
        (ty, v),
        (TypeTag::Bool, Value::Bool(_))
            | (TypeTag::Int, Value::Int(_))
            | (TypeTag::Float, Value::Float(_))
            | (TypeTag::Vector2, Value::Vector2(_))
            | (TypeTag::Vector3, Value::Vector3(_))
            | (TypeTag::Vector4, Value::Vector4(_))
            | (TypeTag::Vector2i, Value::Vector2i(_))
            | (TypeTag::Vector3i, Value::Vector3i(_))
            | (TypeTag::Quaternion, Value::Quaternion(_))
            | (TypeTag::Transform2D, Value::Transform2D(_))
            | (TypeTag::Transform3D, Value::Transform3D(_))
            | (TypeTag::Color, Value::Color(_))
            | (
                TypeTag::String | TypeTag::StringName | TypeTag::NodePath | TypeTag::Resource,
                Value::Str(_)
            )
            | (TypeTag::Variant, Value::Bytes(_))
    )
}

/// One encoded field value: `bits` bits of `data`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoded {
    pub bits: u32,
    pub data: Vec<u8>,
}

impl Encoded {
    pub fn of(w: &BitWriter) -> Self {
        Self {
            bits: w.bit_len() as u32,
            data: w.as_bytes().to_vec(),
        }
    }

    /// True when `w` holds exactly this encoding.
    pub fn same_as(&self, w: &BitWriter) -> bool {
        self.bits as usize == w.bit_len() && self.data == w.as_bytes()
    }

    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    pub fn decode(&self, ty: TypeTag, q: Quant) -> Option<Value> {
        decode(&mut BitReader::new(&self.data), ty, q)
    }
}

/// True when `needle` appears in `hay` starting at any bit offset, as [`BitWriter::write_raw`] puts
/// bytes into a bit-packed message (byte-aligned messages included). For tests that scan packets for
/// a value (gate G9).
pub fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len() * 8;
    let total = hay.len() * 8;
    if n == 0 || n > total {
        return false;
    }
    (0..=total - n).any(|offset| {
        let mut r = BitReader::new(hay);
        r.skip(offset).is_some() && needle.iter().all(|b| r.read_bits(8) == Some(u64::from(*b)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_bytes_finds_bytes_written_at_any_bit_offset() {
        let needle = b"G9-SECRET";
        for skip in 0..16u32 {
            let mut w = BitWriter::new();
            w.write_bits(0b1011, skip.min(4));
            w.write_bits(0, skip.saturating_sub(4));
            w.write_bytes(needle);
            w.write_bits(0x5, 3);
            let bytes = w.into_bytes();
            assert!(contains_bytes(&bytes, needle), "offset {skip}");
            assert!(!contains_bytes(&bytes, b"G9-SECRFT"), "offset {skip}");
        }
        assert!(!contains_bytes(b"short", b"longer than it"));
        assert!(!contains_bytes(b"anything", b""));
    }

    fn roundtrip(ty: TypeTag, q: Quant, v: &Value) -> Value {
        let mut w = BitWriter::new();
        encode(&mut w, ty, q, v);
        let bytes = w.clone().into_bytes();
        let mut r = BitReader::new(&bytes);
        let out = decode(&mut r, ty, q).expect("decodes");
        assert_eq!(
            r.position(),
            w.bit_len(),
            "{ty:?} {q:?} read what was written"
        );
        out
    }

    fn close(a: &[f32], b: &[f32], tol: f32) {
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() <= tol, "{a:?} vs {b:?} (tol {tol})");
        }
    }

    #[test]
    fn bits_varints_and_bytes_round_trip() {
        let mut w = BitWriter::new();
        w.write_bits(5, 3);
        w.write_bool(true);
        w.write_varint(300);
        w.write_zigzag(-7);
        w.write_zigzag(i64::MIN);
        w.write_varint(u64::MAX);
        w.write_bits(u64::MAX, 64);
        w.write_bytes(b"hello");
        let mut other = BitWriter::new();
        other.write_bits(0b101, 3);
        w.append(&other);
        let bytes = w.clone().into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(3), Some(5));
        assert_eq!(r.read_bool(), Some(true));
        assert_eq!(r.read_varint(), Some(300));
        assert_eq!(r.read_zigzag(), Some(-7));
        assert_eq!(r.read_zigzag(), Some(i64::MIN));
        assert_eq!(r.read_varint(), Some(u64::MAX));
        assert_eq!(r.read_bits(64), Some(u64::MAX));
        assert_eq!(r.read_bytes().as_deref(), Some(&b"hello"[..]));
        assert_eq!(r.read_bits(3), Some(0b101));
        assert!(r.read_bits(8).is_none(), "past the end");
        assert!(BitReader::new(&[0xff; 4]).read_varint().is_none());
        assert!(BitReader::new(&[0x7f]).read_bytes().is_none());
    }

    #[test]
    fn mm_and_cm_positions() {
        // D17: a Vector2 is in pixels at 100 px per metre, so mm steps by 0.1 px and cm by 1 px.
        let v = Value::Vector2([1234.5678, -0.04]);
        let Value::Vector2(mm) = roundtrip(TypeTag::Vector2, Quant::Mm, &v) else {
            panic!()
        };
        close(&mm, &[1234.5678, -0.04], 0.05 + 1e-4);
        assert_eq!(mm, [1234.6, 0.0], "0.1 px steps");
        let Value::Vector2(cm) = roundtrip(TypeTag::Vector2, Quant::Cm, &v) else {
            panic!()
        };
        close(&cm, &[1234.5678, 0.0], 0.5 + 1e-4);
        assert_eq!(cm, [1235.0, 0.0], "1 px steps");
        assert_eq!(fixed_step(TypeTag::Vector2, Quant::Mm), Some(0.1));
        assert_eq!(fixed_step(TypeTag::Vector2, Quant::Cm), Some(1.0));
        assert_eq!(fixed_step(TypeTag::Vector3, Quant::Mm), Some(0.001));
        assert_eq!(fixed_step(TypeTag::Vector3, Quant::Cm), Some(0.01));
        assert_eq!(fixed_step(TypeTag::Vector2, Quant::Deg), None);
        // A Transform2D's origin is in pixels too; its basis stays raw.
        let t = Value::Transform2D([0.6, 0.8, -0.8, 0.6, 10.04, -20.06]);
        let Value::Transform2D(t2) = roundtrip(TypeTag::Transform2D, Quant::Mm, &t) else {
            panic!()
        };
        assert_eq!(&t2[..4], &[0.6, 0.8, -0.8, 0.6]);
        close(&t2[4..], &[10.0, -20.1], 1e-4);
        // 3D keeps metres: mm is 0.001 m.
        let Value::Vector3(v3) = roundtrip(
            TypeTag::Vector3,
            Quant::Mm,
            &Value::Vector3([1e6, -1e6, 3.0001]),
        ) else {
            panic!()
        };
        close(&v3, &[1e6, -1e6, 3.0001], 0.07);
        let Value::Vector3(c3) = roundtrip(
            TypeTag::Vector3,
            Quant::Cm,
            &Value::Vector3([1.234, -0.004, 0.0]),
        ) else {
            panic!()
        };
        close(&c3, &[1.23, 0.0, 0.0], 1e-6);
        // A float has no dimension known to netcore: mm stays 0.001 of its unit (radians, rad/s).
        let Value::Float(f) = roundtrip(TypeTag::Float, Quant::Mm, &Value::Float(-2.5813)) else {
            panic!()
        };
        assert!((f + 2.5813).abs() <= 0.0005 + 1e-9);
        // A small mm value is small on the wire: a 2D position a few px from the origin is one byte
        // per axis, a typical on-screen one (hundreds of px) two.
        let mut w = BitWriter::new();
        encode(
            &mut w,
            TypeTag::Vector2,
            Quant::Mm,
            &Value::Vector2([5.0, 0.0]),
        );
        assert_eq!(w.bit_len(), 16, "{} bits", w.bit_len());
        let mut w = BitWriter::new();
        encode(
            &mut w,
            TypeTag::Vector2,
            Quant::Mm,
            &Value::Vector2([640.0, -360.0]),
        );
        assert_eq!(w.bit_len(), 32, "{} bits", w.bit_len());
    }

    #[test]
    fn deg_angles() {
        for a in [0.0f64, 1.0, -3.0, 3.1, 100.0] {
            let Value::Float(b) = roundtrip(TypeTag::Float, Quant::Deg, &Value::Float(a)) else {
                panic!()
            };
            let wrapped = (a + core::f64::consts::PI).rem_euclid(TURN) - core::f64::consts::PI;
            let d = (b - wrapped).abs().min(TURN - (b - wrapped).abs());
            assert!(d <= TURN / 65_536.0, "{a} -> {b}");
        }
        let Value::Vector3(e) = roundtrip(
            TypeTag::Vector3,
            Quant::Deg,
            &Value::Vector3([0.5, -0.5, 1.5]),
        ) else {
            panic!()
        };
        close(&e, &[0.5, -0.5, 1.5], 1e-4);
    }

    #[test]
    fn smallest3_quaternions() {
        let cases = [
            [0.0f32, 0.0, 0.0, 1.0],
            [0.0, 0.0, 0.0, -1.0],
            [0.5, 0.5, 0.5, 0.5],
            [0.1, -0.7, 0.2, 0.676],
            [0.9, 0.1, -0.3, 0.2],
        ];
        for q in cases {
            let len = q.iter().map(|c| c * c).sum::<f32>().sqrt();
            let n = q.map(|c| c / len);
            let Value::Quaternion(out) =
                roundtrip(TypeTag::Quaternion, Quant::Smallest3, &Value::Quaternion(n))
            else {
                panic!()
            };
            // q and -q are the same rotation.
            let dot: f32 = out.iter().zip(&n).map(|(a, b)| a * b).sum();
            let s = dot.signum();
            close(&out, &n.map(|c| c * s), 6e-5);
        }
        let mut w = BitWriter::new();
        encode(
            &mut w,
            TypeTag::Quaternion,
            Quant::Smallest3,
            &Value::Quaternion([0.0, 0.0, 0.0, 1.0]),
        );
        assert_eq!(w.bit_len(), 47);
    }

    #[test]
    fn f16_values() {
        for x in [0.0f32, 1.0, -2.5, 0.333, 1000.0, 65504.0, 1e-5, -6.1e-5] {
            let back = f16_to_f32(f32_to_f16(x));
            let tol = (x.abs() / 1024.0).max(6e-8);
            assert!((back - x).abs() <= tol, "{x} -> {back}");
        }
        assert!(f16_to_f32(f32_to_f16(1e9)).is_infinite());
        assert!(f16_to_f32(f32_to_f16(f32::NAN)).is_nan());
        let Value::Color(c) = roundtrip(
            TypeTag::Color,
            Quant::F16,
            &Value::Color([0.25, 0.5, 1.0, 1.5]),
        ) else {
            panic!()
        };
        close(&c, &[0.25, 0.5, 1.0, 1.5], 1e-3);
    }

    #[test]
    fn raw_and_other_types_are_exact() {
        let cases = [
            (TypeTag::Bool, Value::Bool(true)),
            (TypeTag::Int, Value::Int(-123_456_789_012)),
            (TypeTag::Float, Value::Float(core::f64::consts::E)),
            (TypeTag::Vector2, Value::Vector2([1.5, -2.25])),
            (TypeTag::Vector4, Value::Vector4([1.0, 2.0, 3.0, 4.0])),
            (TypeTag::Vector2i, Value::Vector2i([-5, 7])),
            (TypeTag::Vector3i, Value::Vector3i([i32::MIN, 0, i32::MAX])),
            (TypeTag::Quaternion, Value::Quaternion([0.1, 0.2, 0.3, 0.9])),
            (
                TypeTag::Transform2D,
                Value::Transform2D([0.0, 1.0, -1.0, 0.0, 10.5, -3.25]),
            ),
            (
                TypeTag::Transform3D,
                Value::Transform3D([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 4.0, 5.0, 6.0]),
            ),
            (TypeTag::Color, Value::Color([0.1, 0.2, 0.3, 0.4])),
            (TypeTag::String, Value::Str("Hëllo ✓".into())),
            (TypeTag::StringName, Value::Str("walk".into())),
            (TypeTag::NodePath, Value::Str("../Sprite:frame".into())),
            (TypeTag::Resource, Value::Str("res://icon.svg".into())),
            (TypeTag::Variant, Value::Bytes(std::vec![1, 2, 3, 0, 255])),
        ];
        for (ty, v) in cases {
            assert_eq!(roundtrip(ty, Quant::Raw, &v), v, "{ty:?}");
        }
        // Transforms quantise only the origin with mm.
        let Value::Transform2D(t) = roundtrip(
            TypeTag::Transform2D,
            Quant::Mm,
            &Value::Transform2D([0.6, 0.8, -0.8, 0.6, 10.12345, 2.0]),
        ) else {
            panic!()
        };
        close(&t, &[0.6, 0.8, -0.8, 0.6, 10.12345, 2.0], 0.05 + 1e-4);
        assert_eq!(&t[..4], &[0.6, 0.8, -0.8, 0.6]);
    }

    #[test]
    fn a_mistyped_value_encodes_the_zero_value() {
        assert_eq!(
            roundtrip(TypeTag::Vector2, Quant::Mm, &Value::Nil),
            Value::Vector2([0.0; 2])
        );
        assert_eq!(
            roundtrip(TypeTag::String, Quant::Raw, &Value::Int(3)),
            Value::Str(String::new())
        );
    }

    #[test]
    fn truncated_input_is_none() {
        let mut w = BitWriter::new();
        encode(
            &mut w,
            TypeTag::String,
            Quant::Raw,
            &Value::Str("abcdef".into()),
        );
        let mut bytes = w.into_bytes();
        bytes.truncate(3);
        assert!(decode(&mut BitReader::new(&bytes), TypeTag::String, Quant::Raw).is_none());
        assert!(decode(&mut BitReader::new(&[]), TypeTag::Vector3, Quant::Raw).is_none());
    }
}

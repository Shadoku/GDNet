//! The `Snapshot` message (docs/PLAN_NETCODE.md 2.4, WP3): unreliable, bit-packed, at most one MTU.
//!
//! ```text
//! kind byte (7)
//! varint server tick, varint baseline tick (the newest tick the peer acknowledged; 0 = full state)
//! per entity: 1 (more), varint net id, varint record length in bits, record
//! 0 (end)
//! record: per part of the scene, 1 bit "changed"; for a changed part, 1 bit per field (the dirty
//!         mask), then the dirty fields' values in field order, packed by their quantisers
//! ```
//!
//! The record length lets a receiver skip an entity it cannot decode yet (its `Create` has not arrived,
//! or its property table is not registered); such a snapshot is applied for the entities it knows and
//! not acknowledged, so the sender resends what it carried.

use crate::entity::NetId;
use crate::props::{ScenePropsTable, Value};
use crate::quant::{decode as decode_value, BitReader, BitWriter, Encoded};
use std::vec::Vec;

/// Snapshot header fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Header {
    pub tick: u64,
    pub baseline: u64,
}

/// Bits a varint of `v` takes.
pub fn varint_bits(mut v: u64) -> usize {
    let mut n = 8;
    v >>= 7;
    while v != 0 {
        n += 8;
        v >>= 7;
    }
    n
}

/// Bits the header and the end marker take (the kind byte included).
pub fn frame_bits(h: Header) -> usize {
    8 + varint_bits(h.tick) + varint_bits(h.baseline) + 1
}

pub fn write_header(w: &mut BitWriter, h: Header) {
    w.write_varint(h.tick);
    w.write_varint(h.baseline);
}

pub fn write_end(w: &mut BitWriter) {
    w.write_bool(false);
}

/// The record bits of an entity carrying `fields` (part, field, value bits), which must be sorted.
pub fn record_bits(table: &ScenePropsTable, fields: &[(u16, u16, u32)]) -> usize {
    let mut bits = table.parts.len();
    let mut last_part: Option<u16> = None;
    for &(p, _, b) in fields {
        if last_part != Some(p) {
            last_part = Some(p);
            bits += table.parts.get(usize::from(p)).map_or(0, Vec::len);
        }
        bits += b as usize;
    }
    bits
}

/// The bits an entity entry takes beyond its record.
pub fn entry_overhead(id: NetId, record: usize) -> usize {
    1 + varint_bits(u64::from(id)) + varint_bits(record as u64)
}

/// Writes one entity entry. `fields` (part, field, value) must be sorted by part then field and name
/// fields of `table`.
pub fn write_entity(
    w: &mut BitWriter,
    id: NetId,
    table: &ScenePropsTable,
    fields: &[(u16, u16, &Encoded)],
) {
    let sizes: Vec<(u16, u16, u32)> = fields.iter().map(|(p, f, e)| (*p, *f, e.bits)).collect();
    let record = record_bits(table, &sizes);
    w.write_bool(true);
    w.write_varint(u64::from(id));
    w.write_varint(record as u64);
    let start = w.bit_len();
    let mut i = 0;
    for (p, part) in table.parts.iter().enumerate() {
        let p = p as u16;
        let begin = i;
        while i < fields.len() && fields[i].0 == p {
            i += 1;
        }
        let mine = &fields[begin..i];
        w.write_bool(!mine.is_empty());
        if mine.is_empty() {
            continue;
        }
        let mut k = 0;
        for f in 0..part.len() as u16 {
            let dirty = k < mine.len() && mine[k].1 == f;
            w.write_bool(dirty);
            if dirty {
                k += 1;
            }
        }
        for (_, _, e) in mine {
            w.write_raw(&e.data, e.bits as usize);
        }
    }
    debug_assert_eq!(w.bit_len() - start, record);
}

/// One decoded entity: its id and the values it carried.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedEntity {
    pub id: NetId,
    pub fields: Vec<(u16, u16, Value)>,
}

/// A decoded snapshot body (after the kind byte).
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub header: Header,
    pub entities: Vec<DecodedEntity>,
    /// Entities skipped because `table_of` did not know them.
    pub skipped: usize,
}

/// Decodes a snapshot body. `table_of(id)` gives the property table of a known entity. `None` for a
/// malformed body.
pub fn decode<'t>(
    body: &[u8],
    table_of: impl Fn(NetId) -> Option<&'t ScenePropsTable>,
) -> Option<Decoded> {
    let mut r = BitReader::new(body);
    let header = Header {
        tick: r.read_varint()?,
        baseline: r.read_varint()?,
    };
    let mut entities = Vec::new();
    let mut skipped = 0;
    while r.read_bool()? {
        let id = u32::try_from(r.read_varint()?).ok()?;
        let len = usize::try_from(r.read_varint()?).ok()?;
        if len > r.remaining() {
            return None;
        }
        let Some(table) = table_of(id) else {
            r.skip(len)?;
            skipped += 1;
            continue;
        };
        let start = r.position();
        let mut fields = Vec::new();
        for (p, part) in table.parts.iter().enumerate() {
            if !r.read_bool()? {
                continue;
            }
            let mut dirty = Vec::new();
            for f in 0..part.len() {
                if r.read_bool()? {
                    dirty.push(f);
                }
            }
            for f in dirty {
                let d = &part[f];
                let v = decode_value(&mut r, d.ty, d.quant)?;
                fields.push((p as u16, f as u16, v));
            }
        }
        if r.position() - start != len {
            return None;
        }
        entities.push(DecodedEntity { id, fields });
    }
    Some(Decoded {
        header,
        entities,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::props::{PropDesc, Quant, TypeTag};
    use crate::quant::encode;

    fn table() -> ScenePropsTable {
        ScenePropsTable::new(std::vec![
            std::vec![
                PropDesc::new("position", TypeTag::Vector2).hot(Quant::Mm),
                PropDesc::new("rotation", TypeTag::Float).hot(Quant::Deg),
                PropDesc::new("visible", TypeTag::Bool),
            ],
            std::vec![PropDesc::new("text", TypeTag::String)],
        ])
    }

    fn enc(t: &ScenePropsTable, p: u16, f: u16, v: &Value) -> Encoded {
        let d = t.get(p, f).unwrap();
        let mut w = BitWriter::new();
        encode(&mut w, d.ty, d.quant, v);
        Encoded::of(&w)
    }

    #[test]
    fn entities_round_trip_and_unknown_ones_are_skipped() {
        let t = table();
        let pos = enc(&t, 0, 0, &Value::Vector2([1.0, 2.0]));
        let vis = enc(&t, 0, 2, &Value::Bool(true));
        let txt = enc(&t, 1, 0, &Value::Str("hi".into()));
        let h = Header {
            tick: 1000,
            baseline: 990,
        };
        let mut w = BitWriter::new();
        write_header(&mut w, h);
        write_entity(&mut w, 7, &t, &[(0, 0, &pos), (0, 2, &vis), (1, 0, &txt)]);
        write_entity(&mut w, 9, &t, &[(1, 0, &txt)]);
        write_entity(&mut w, 11, &t, &[(0, 0, &pos)]);
        write_end(&mut w);
        let body = w.into_bytes();
        let d = decode(&body, |id| (id != 9).then_some(&t)).unwrap();
        assert_eq!(d.header, h);
        assert_eq!(d.skipped, 1);
        assert_eq!(d.entities.len(), 2);
        assert_eq!(d.entities[0].id, 7);
        assert_eq!(
            d.entities[0].fields,
            std::vec![
                (0, 0, Value::Vector2([1.0, 2.0])),
                (0, 2, Value::Bool(true)),
                (1, 0, Value::Str("hi".into())),
            ]
        );
        assert_eq!(d.entities[1].id, 11);
    }

    #[test]
    fn sizes_are_predicted_exactly() {
        let t = table();
        let pos = enc(&t, 0, 0, &Value::Vector2([123.0, -4.5]));
        let txt = enc(&t, 1, 0, &Value::Str("hello".into()));
        let fields = [(0u16, 0u16, &pos), (1, 0, &txt)];
        let sizes: Vec<(u16, u16, u32)> = fields.iter().map(|(p, f, e)| (*p, *f, e.bits)).collect();
        let rec = record_bits(&t, &sizes);
        let h = Header {
            tick: 5,
            baseline: 0,
        };
        let mut w = BitWriter::new();
        w.write_bits(7, 8);
        write_header(&mut w, h);
        write_entity(&mut w, 300, &t, &fields);
        write_end(&mut w);
        assert_eq!(w.bit_len(), frame_bits(h) + entry_overhead(300, rec) + rec);
    }

    #[test]
    fn malformed_bodies_are_none() {
        let t = table();
        assert!(decode(&[], |_| Some(&t)).is_none());
        let mut w = BitWriter::new();
        write_header(&mut w, Header::default());
        w.write_bool(true);
        w.write_varint(1);
        w.write_varint(10_000);
        assert!(decode(w.as_bytes(), |_| Some(&t)).is_none());
    }
}

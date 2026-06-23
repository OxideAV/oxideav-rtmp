//! AMF0 (Action Message Format 0) value encoder + decoder.
//!
//! RTMP command / data messages are AMF0-encoded. This module covers
//! every serializable AMF0 marker from Table (§2) of the spec: Number,
//! Boolean, String, Object, Null, Undefined, Reference, ECMA array,
//! strict array, Date, long string, Unsupported (0x0D), XML Document
//! (0x0F), Typed Object (0x10), and the end-of-object sentinel. The two
//! deliberately-unserializable markers (Movieclip 0x04, RecordSet 0x0E,
//! per spec §2.6 / §2.16) are rejected as unknown on decode. We never
//! emit nor consume AMF3 — RTMP occasionally upgrades a channel to AMF3
//! via the `avmplus` marker (0x11), but we simply keep the channel on
//! AMF0; every commodity client / server we have interoperated with
//! stays in AMF0 for publish flows.
//!
//! AMF0 object references (marker 0x07) are **dereferenced
//! transparently** on decode: a reference resolves to a clone of the
//! complex value it points at, so callers never see a `Reference`
//! variant in the value graph. Encoding always emits the expanded
//! (inline) form — references are an optional wire compression, so the
//! expanded form is byte-valid AMF0 and a decode→encode round-trip of a
//! reference-using stream produces an equivalent inline stream.
//!
//! See the "AMF0 File Format Specification" (Adobe, 2007) for marker
//! tables; kept minimal here to avoid drift from the spec.

use std::collections::HashMap;

use crate::error::{Error, Result};

// Marker bytes — Table 4.1 in the AMF0 spec.
const M_NUMBER: u8 = 0x00;
const M_BOOLEAN: u8 = 0x01;
const M_STRING: u8 = 0x02;
const M_OBJECT: u8 = 0x03;
const M_NULL: u8 = 0x05;
const M_UNDEFINED: u8 = 0x06;
const M_REFERENCE: u8 = 0x07;
const M_ECMA_ARRAY: u8 = 0x08;
const M_OBJECT_END: u8 = 0x09;
const M_STRICT_ARRAY: u8 = 0x0A;
const M_DATE: u8 = 0x0B;
const M_LONG_STRING: u8 = 0x0C;
const M_UNSUPPORTED: u8 = 0x0D;
const M_XML_DOCUMENT: u8 = 0x0F;
const M_TYPED_OBJECT: u8 = 0x10;

/// One AMF0 value. Ordered-object semantics are preserved through the
/// `Vec<(key, value)>` — RTMP command handlers sometimes care about
/// the order of fields in the command object (some commodity peers walk
/// them positionally), so we don't dedup into a map.
#[derive(Debug, Clone, PartialEq)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    /// Ordered association list. Kept ordered because some commodity
    /// peers expect the `tcUrl` field at a fixed offset in connect's
    /// command object.
    Object(Vec<(String, Amf0Value)>),
    Null,
    Undefined,
    /// ECMA array — in AMF0 an associative array with a u32 length
    /// prefix. We keep the pairs in order and ignore the length on
    /// decode (the actual count comes from walking until OBJECT_END).
    EcmaArray(Vec<(String, Amf0Value)>),
    StrictArray(Vec<Amf0Value>),
    /// Milliseconds since the UNIX epoch + timezone offset. Rarely
    /// sent by RTMP peers; kept round-trippable for completeness.
    Date {
        millis: f64,
        timezone: i16,
    },
    /// AMF0 Unsupported (marker 0x0D, spec §2.15). Emitted in place of a
    /// value the producer could not serialize. No payload is carried;
    /// some endpoints throw on receipt, so we surface it as a distinct
    /// variant rather than collapsing it into `Undefined`.
    Unsupported,
    /// AMF0 XML Document (marker 0x0F, spec §2.17). On the wire this is a
    /// long UTF-8 string holding the serialized DOM; we keep it as a
    /// dedicated variant so a decode→encode round-trip preserves the
    /// marker (rather than re-emitting a bare `String`/long-string).
    XmlDocument(String),
    /// AMF0 Typed Object (marker 0x10, spec §2.18): a class-name string
    /// followed by the same property/object-end body as an anonymous
    /// object. Typed objects are complex types and can be referenced.
    TypedObject {
        class_name: String,
        properties: Vec<(String, Amf0Value)>,
    },
}

impl Amf0Value {
    /// Look up a direct child of an Object / EcmaArray by name.
    pub fn get(&self, key: &str) -> Option<&Amf0Value> {
        match self {
            Amf0Value::Object(v) | Amf0Value::EcmaArray(v) => {
                v.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            Amf0Value::TypedObject { properties, .. } => {
                properties.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf0Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf0Value::Number(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Amf0Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Hard cap on nested-container decode depth. Real RTMP `onMetaData`
/// objects nest at most 2-3 levels (an outer ECMA-array of scalars,
/// occasionally an inner object for `videodatarate` style sub-records).
/// A forged AMF0 frame can otherwise nest Object-inside-Object until
/// the call stack runs out — guard against that DoS vector before we
/// even reach the byte cursor.
pub const MAX_DECODE_DEPTH: usize = 64;

/// Decode one AMF0 value from `buf` starting at `*pos`. Advances `pos`
/// past the value on success. Returns `InvalidAmf0` for unknown markers,
/// truncated input, or container nesting deeper than
/// [`MAX_DECODE_DEPTH`].
///
/// AMF0 object references (marker 0x07, `SCRIPTDATAVALUE.Type == 7` per
/// the FLV v10.1 spec §E.4.4.2) are resolved against a reference table
/// scoped to this single call: each complex value (Object, typed
/// object, ECMA array, strict array) is appended to the table as it is
/// decoded, and a reference resolves to the `UI16`-indexed prior entry.
/// Because the resolution table is per-call, a reference only "sees"
/// complex values that appeared earlier within the same top-level value.
/// To resolve references across a multi-value packet (where the table
/// spans the whole message), use [`decode_all`].
pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Amf0Value> {
    let mut refs = Vec::new();
    decode_at_depth(buf, pos, 0, &mut refs)
}

/// The reference table threaded through one AMF0 serialization context.
/// Each entry is a fully-decoded complex value, in the order the spec's
/// "complex object" rule appends them. A `REFERENCE` (UI16) indexes into
/// this list.
type RefTable = Vec<Amf0Value>;

fn decode_at_depth(
    buf: &[u8],
    pos: &mut usize,
    depth: usize,
    refs: &mut RefTable,
) -> Result<Amf0Value> {
    if depth >= MAX_DECODE_DEPTH {
        return Err(Error::InvalidAmf0(format!(
            "nested container depth exceeded {MAX_DECODE_DEPTH}"
        )));
    }
    let marker = read_u8(buf, pos)?;
    match marker {
        M_NUMBER => {
            let bits = read_u64_be(buf, pos)?;
            Ok(Amf0Value::Number(f64::from_bits(bits)))
        }
        M_BOOLEAN => Ok(Amf0Value::Boolean(read_u8(buf, pos)? != 0)),
        M_STRING => Ok(Amf0Value::String(read_utf8_short(buf, pos)?)),
        M_LONG_STRING => Ok(Amf0Value::String(read_utf8_long(buf, pos)?)),
        M_NULL => Ok(Amf0Value::Null),
        M_UNDEFINED => Ok(Amf0Value::Undefined),
        M_OBJECT => {
            // Reserve the table slot *before* decoding the body so a
            // reference appearing inside the body still indexes this
            // object at its eventual position. We can't fill the value
            // until the body is decoded, so we push a placeholder and
            // overwrite it; a self-reference inside the body therefore
            // resolves to the placeholder (an empty Object) rather than
            // looping — acyclic graphs (every onMetaData in practice)
            // are unaffected.
            let slot = refs.len();
            refs.push(Amf0Value::Object(Vec::new()));
            let body = read_object_body_at_depth(buf, pos, depth + 1, refs)?;
            let v = Amf0Value::Object(body);
            refs[slot] = v.clone();
            Ok(v)
        }
        M_ECMA_ARRAY => {
            // Spec says this is preceded by a u32 associative count, then
            // the body is the same key/value/OBJECT_END terminator
            // sequence as a plain Object. The count is advisory — walk
            // until OBJECT_END.
            let _count = read_u32_be(buf, pos)?;
            let slot = refs.len();
            refs.push(Amf0Value::EcmaArray(Vec::new()));
            let body = read_object_body_at_depth(buf, pos, depth + 1, refs)?;
            let v = Amf0Value::EcmaArray(body);
            refs[slot] = v.clone();
            Ok(v)
        }
        M_STRICT_ARRAY => {
            let count = read_u32_be(buf, pos)? as usize;
            let slot = refs.len();
            refs.push(Amf0Value::StrictArray(Vec::new()));
            let mut out = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                out.push(decode_at_depth(buf, pos, depth + 1, refs)?);
            }
            let v = Amf0Value::StrictArray(out);
            refs[slot] = v.clone();
            Ok(v)
        }
        M_DATE => {
            let bits = read_u64_be(buf, pos)?;
            let tz = read_i16_be(buf, pos)?;
            Ok(Amf0Value::Date {
                millis: f64::from_bits(bits),
                timezone: tz,
            })
        }
        M_UNSUPPORTED => Ok(Amf0Value::Unsupported),
        M_XML_DOCUMENT => {
            // §2.17: an XML document is always encoded as a long UTF-8
            // string (32-bit length prefix), same body shape as a
            // long-string but a distinct marker.
            Ok(Amf0Value::XmlDocument(read_utf8_long(buf, pos)?))
        }
        M_TYPED_OBJECT => {
            // §2.18: `object-marker class-name *(object-property)`. The
            // class name is a short UTF-8 string preceding the same
            // property/OBJECT_END body as an anonymous object. Typed
            // objects are complex types, so reserve the ref-table slot
            // before decoding the body (mirrors M_OBJECT) so an inner
            // reference can point back at this instance.
            let class_name = read_utf8_short(buf, pos)?;
            let slot = refs.len();
            refs.push(Amf0Value::TypedObject {
                class_name: class_name.clone(),
                properties: Vec::new(),
            });
            let body = read_object_body_at_depth(buf, pos, depth + 1, refs)?;
            let v = Amf0Value::TypedObject {
                class_name,
                properties: body,
            };
            refs[slot] = v.clone();
            Ok(v)
        }
        M_REFERENCE => {
            // §E.4.4.2: `IF Type == 7 { UI16 }` — a 16-bit big-endian
            // index into the table of complex objects serialized so far
            // in this context. Resolve by cloning the referenced value.
            let idx = read_u16_be(buf, pos)? as usize;
            refs.get(idx).cloned().ok_or_else(|| {
                Error::InvalidAmf0(format!(
                    "AMF0 reference index {idx} out of range (table has {} entries)",
                    refs.len()
                ))
            })
        }
        other => Err(Error::InvalidAmf0(format!("unknown marker {other:#x}"))),
    }
}

/// Decode a sequence of AMF0 values until the input is exhausted.
/// Useful for parsing command payloads where the count isn't known in
/// advance (connect / _result / onStatus etc.).
///
/// All top-level values share one reference table, so an AMF0 reference
/// (marker 0x07) in a later value can resolve a complex object that
/// appeared in an earlier value of the same packet — the serialization
/// context the spec describes.
pub fn decode_all(buf: &[u8]) -> Result<Vec<Amf0Value>> {
    let mut pos = 0;
    let mut out = Vec::new();
    let mut refs = Vec::new();
    while pos < buf.len() {
        out.push(decode_at_depth(buf, &mut pos, 0, &mut refs)?);
    }
    Ok(out)
}

fn read_object_body_at_depth(
    buf: &[u8],
    pos: &mut usize,
    depth: usize,
    refs: &mut RefTable,
) -> Result<Vec<(String, Amf0Value)>> {
    let mut out = Vec::new();
    loop {
        // Object keys are the short-string form WITHOUT a leading
        // marker byte. An empty key followed by OBJECT_END marker
        // terminates the object.
        let key = read_utf8_short(buf, pos)?;
        if key.is_empty() {
            let end = read_u8(buf, pos)?;
            if end == M_OBJECT_END {
                return Ok(out);
            } else {
                return Err(Error::InvalidAmf0(format!(
                    "expected OBJECT_END (0x09) after empty key, got {end:#x}"
                )));
            }
        }
        let value = decode_at_depth(buf, pos, depth, refs)?;
        out.push((key, value));
    }
}

#[inline]
fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| Error::InvalidAmf0("truncated".into()))?;
    *pos += 1;
    Ok(b)
}

#[inline]
fn read_u16_be(buf: &[u8], pos: &mut usize) -> Result<u16> {
    if *pos + 2 > buf.len() {
        return Err(Error::InvalidAmf0("truncated u16".into()));
    }
    let v = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]);
    *pos += 2;
    Ok(v)
}

#[inline]
fn read_i16_be(buf: &[u8], pos: &mut usize) -> Result<i16> {
    Ok(read_u16_be(buf, pos)? as i16)
}

#[inline]
fn read_u32_be(buf: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(Error::InvalidAmf0("truncated u32".into()));
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Ok(v)
}

#[inline]
fn read_u64_be(buf: &[u8], pos: &mut usize) -> Result<u64> {
    if *pos + 8 > buf.len() {
        return Err(Error::InvalidAmf0("truncated u64".into()));
    }
    let v = u64::from_be_bytes([
        buf[*pos],
        buf[*pos + 1],
        buf[*pos + 2],
        buf[*pos + 3],
        buf[*pos + 4],
        buf[*pos + 5],
        buf[*pos + 6],
        buf[*pos + 7],
    ]);
    *pos += 8;
    Ok(v)
}

fn read_utf8_short(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u16_be(buf, pos)? as usize;
    read_utf8_body(buf, pos, len)
}

fn read_utf8_long(buf: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32_be(buf, pos)? as usize;
    read_utf8_body(buf, pos, len)
}

fn read_utf8_body(buf: &[u8], pos: &mut usize, len: usize) -> Result<String> {
    if *pos + len > buf.len() {
        return Err(Error::InvalidAmf0(format!(
            "truncated string: need {len}, have {}",
            buf.len() - *pos
        )));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|e| Error::InvalidAmf0(format!("non-UTF8 string: {e}")))?
        .to_owned();
    *pos += len;
    Ok(s)
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Append one AMF0 value to `out`.
pub fn encode(out: &mut Vec<u8>, v: &Amf0Value) {
    match v {
        Amf0Value::Number(n) => {
            out.push(M_NUMBER);
            out.extend_from_slice(&n.to_bits().to_be_bytes());
        }
        Amf0Value::Boolean(b) => {
            out.push(M_BOOLEAN);
            out.push(if *b { 1 } else { 0 });
        }
        Amf0Value::String(s) => {
            if s.len() <= u16::MAX as usize {
                out.push(M_STRING);
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            } else {
                out.push(M_LONG_STRING);
                out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
        Amf0Value::Null => out.push(M_NULL),
        Amf0Value::Undefined => out.push(M_UNDEFINED),
        Amf0Value::Object(pairs) => {
            out.push(M_OBJECT);
            write_object_body(out, pairs);
        }
        Amf0Value::EcmaArray(pairs) => {
            out.push(M_ECMA_ARRAY);
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            write_object_body(out, pairs);
        }
        Amf0Value::StrictArray(items) => {
            out.push(M_STRICT_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                encode(out, item);
            }
        }
        Amf0Value::Date { millis, timezone } => {
            out.push(M_DATE);
            out.extend_from_slice(&millis.to_bits().to_be_bytes());
            out.extend_from_slice(&timezone.to_be_bytes());
        }
        Amf0Value::Unsupported => out.push(M_UNSUPPORTED),
        Amf0Value::XmlDocument(s) => {
            // §2.17: always a long UTF-8 string (32-bit length prefix).
            out.push(M_XML_DOCUMENT);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Amf0Value::TypedObject {
            class_name,
            properties,
        } => {
            out.push(M_TYPED_OBJECT);
            out.extend_from_slice(&(class_name.len() as u16).to_be_bytes());
            out.extend_from_slice(class_name.as_bytes());
            write_object_body(out, properties);
        }
    }
}

fn write_object_body(out: &mut Vec<u8>, pairs: &[(String, Amf0Value)]) {
    for (k, v) in pairs {
        out.extend_from_slice(&(k.len() as u16).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
        encode(out, v);
    }
    // Empty key + OBJECT_END marker → object terminator.
    out.extend_from_slice(&0u16.to_be_bytes());
    out.push(M_OBJECT_END);
}

/// Build an AMF0-encoded command payload: command name + transaction
/// id + optional command object + trailing args. This is the exact
/// shape every RTMP NetConnection / NetStream command uses.
pub fn encode_command(
    name: &str,
    transaction_id: f64,
    command_object: Amf0Value,
    args: &[Amf0Value],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + args.len() * 8);
    encode(&mut out, &Amf0Value::String(name.into()));
    encode(&mut out, &Amf0Value::Number(transaction_id));
    encode(&mut out, &command_object);
    for a in args {
        encode(&mut out, a);
    }
    out
}

/// Convenience: build an `Amf0Value::Object` from an iterable of
/// `(name, value)` pairs.
#[allow(dead_code)]
pub fn obj<I, S>(pairs: I) -> Amf0Value
where
    I: IntoIterator<Item = (S, Amf0Value)>,
    S: Into<String>,
{
    Amf0Value::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
}

/// Tiny map-style helper when the caller doesn't care about key order.
/// Useful for test setup; uses HashMap's arbitrary iteration order so
/// don't rely on output byte layout.
#[allow(dead_code)]
pub fn obj_unordered(map: HashMap<String, Amf0Value>) -> Amf0Value {
    Amf0Value::Object(map.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_number() {
        let v = Amf0Value::Number(42.5);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_string_and_object() {
        let v = Amf0Value::Object(vec![
            ("app".into(), Amf0Value::String("live".into())),
            ("flashVer".into(), Amf0Value::String("FMLE/3.0".into())),
            ("capabilities".into(), Amf0Value::Number(239.0)),
        ]);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn decode_all_splits_command_payload() {
        let payload = {
            let mut v = Vec::new();
            encode(&mut v, &Amf0Value::String("connect".into()));
            encode(&mut v, &Amf0Value::Number(1.0));
            encode(
                &mut v,
                &Amf0Value::Object(vec![("app".into(), Amf0Value::String("live".into()))]),
            );
            v
        };
        let values = decode_all(&payload).unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0].as_str(), Some("connect"));
        assert_eq!(values[1].as_f64(), Some(1.0));
        assert_eq!(
            values[2].get("app").and_then(Amf0Value::as_str),
            Some("live")
        );
    }

    #[test]
    fn reference_resolves_to_prior_complex_object() {
        // Two top-level values in one packet: an object, then a
        // reference (marker 0x07, UI16 index 0) pointing back at it.
        // §E.4.4.2: `IF Type == 7 { UI16 }`.
        let obj = Amf0Value::Object(vec![
            ("app".into(), Amf0Value::String("live".into())),
            ("n".into(), Amf0Value::Number(7.0)),
        ]);
        let mut b = Vec::new();
        encode(&mut b, &obj);
        b.push(M_REFERENCE);
        b.extend_from_slice(&0u16.to_be_bytes()); // reference index 0

        let values = decode_all(&b).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], obj);
        // The reference dereferences transparently to a clone of the
        // object — callers never see a Reference variant.
        assert_eq!(values[1], obj);
    }

    #[test]
    fn reference_to_second_complex_object() {
        // Object index 0, ECMA array index 1, strict array index 2, then
        // a reference to index 1. Confirms the table appends each complex
        // value in serialization order across the whole packet.
        let o0 = Amf0Value::Object(vec![("a".into(), Amf0Value::Number(1.0))]);
        let e1 = Amf0Value::EcmaArray(vec![("b".into(), Amf0Value::Boolean(true))]);
        let s2 = Amf0Value::StrictArray(vec![Amf0Value::Number(9.0)]);
        let mut b = Vec::new();
        encode(&mut b, &o0);
        encode(&mut b, &e1);
        encode(&mut b, &s2);
        b.push(M_REFERENCE);
        b.extend_from_slice(&1u16.to_be_bytes()); // → e1

        let values = decode_all(&b).unwrap();
        assert_eq!(values.len(), 4);
        assert_eq!(values[3], e1);
    }

    #[test]
    fn nested_reference_inside_object_body() {
        // Outer object whose "self" key references index 0 (the outer
        // object itself, decoded so far as its placeholder). Proves a
        // reference inside a body resolves against the in-flight table.
        let mut body = Vec::new();
        // key "x" → number
        body.extend_from_slice(&(1u16).to_be_bytes());
        body.extend_from_slice(b"x");
        encode(&mut body, &Amf0Value::Number(5.0));
        // key "self" → reference index 0
        body.extend_from_slice(&(4u16).to_be_bytes());
        body.extend_from_slice(b"self");
        body.push(M_REFERENCE);
        body.extend_from_slice(&0u16.to_be_bytes());
        // terminator
        body.extend_from_slice(&0u16.to_be_bytes());
        body.push(M_OBJECT_END);

        let mut frame = vec![M_OBJECT];
        frame.extend_from_slice(&body);

        let mut p = 0;
        let v = decode(&frame, &mut p).unwrap();
        if let Amf0Value::Object(pairs) = &v {
            assert_eq!(pairs[0].0, "x");
            // The self-reference resolves to the placeholder (an empty
            // Object) — acyclic, no infinite loop.
            assert_eq!(pairs[1].0, "self");
            assert_eq!(pairs[1].1, Amf0Value::Object(Vec::new()));
        } else {
            panic!("expected object, got {v:?}");
        }
    }

    #[test]
    fn out_of_range_reference_is_clean_error() {
        // Reference index 0 with an empty table → clean error, no panic.
        let b = [M_REFERENCE, 0x00, 0x05];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }

    #[test]
    fn truncated_reference_index_is_clean_error() {
        // Marker present but only one of the two UI16 bytes follows.
        let b = [M_REFERENCE, 0x00];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }

    #[test]
    fn roundtrip_unsupported() {
        // §2.15: marker-only, no payload.
        let v = Amf0Value::Unsupported;
        let mut b = Vec::new();
        encode(&mut b, &v);
        assert_eq!(b, [M_UNSUPPORTED]);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
        assert_eq!(p, 1);
    }

    #[test]
    fn roundtrip_xml_document() {
        // §2.17: always a long UTF-8 string (32-bit length prefix).
        let v = Amf0Value::XmlDocument("<root><a/></root>".into());
        let mut b = Vec::new();
        encode(&mut b, &v);
        assert_eq!(b[0], M_XML_DOCUMENT);
        // 32-bit length prefix, not 16-bit.
        assert_eq!(&b[1..5], &(17u32).to_be_bytes());
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_typed_object() {
        // §2.18: class-name (short UTF-8) then property/OBJECT_END body.
        let v = Amf0Value::TypedObject {
            class_name: "com.example.Point".into(),
            properties: vec![
                ("x".into(), Amf0Value::Number(3.0)),
                ("y".into(), Amf0Value::Number(4.0)),
            ],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        assert_eq!(b[0], M_TYPED_OBJECT);
        // class-name length prefix is a UI16 (short string).
        assert_eq!(&b[1..3], &(17u16).to_be_bytes());
        let mut p = 0;
        let decoded = decode(&b, &mut p).unwrap();
        assert_eq!(decoded, v);
        // get() reaches into typed-object properties.
        assert_eq!(decoded.get("y").and_then(Amf0Value::as_f64), Some(4.0));
    }

    #[test]
    fn typed_object_is_reference_eligible() {
        // §2.18: "reoccurring instances can be sent by reference." A
        // typed object occupies a ref-table slot like any complex value.
        let to = Amf0Value::TypedObject {
            class_name: "Foo".into(),
            properties: vec![("a".into(), Amf0Value::Number(1.0))],
        };
        let mut b = Vec::new();
        encode(&mut b, &to);
        b.push(M_REFERENCE);
        b.extend_from_slice(&0u16.to_be_bytes());

        let values = decode_all(&b).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], to);
        assert_eq!(values[1], to);
    }

    #[test]
    fn typed_object_truncated_class_name_is_clean_error() {
        // Marker + a class-name length claiming 4 bytes but none present.
        let b = [M_TYPED_OBJECT, 0x00, 0x04];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }

    #[test]
    fn xml_document_distinct_from_long_string_on_wire() {
        // An XmlDocument must NOT decode as a plain String, and vice
        // versa — the marker byte distinguishes them.
        let xml = Amf0Value::XmlDocument("<x/>".into());
        let mut b = Vec::new();
        encode(&mut b, &xml);
        let mut p = 0;
        match decode(&b, &mut p).unwrap() {
            Amf0Value::XmlDocument(s) => assert_eq!(s, "<x/>"),
            other => panic!("expected XmlDocument, got {other:?}"),
        }
    }

    #[test]
    fn reference_scope_is_per_top_level_value_in_decode() {
        // Single-value `decode` has a fresh table: a reference at the
        // top level of its own call has nothing to point at.
        let b = [M_REFERENCE, 0x00, 0x00];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }
}

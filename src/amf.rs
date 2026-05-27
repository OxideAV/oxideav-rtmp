//! AMF0 (Action Message Format 0) value encoder + decoder.
//!
//! RTMP command / data messages are AMF0-encoded. This module implements
//! the subset of markers that real RTMP traffic uses in practice:
//! Number, Boolean, String, Object, Null, Undefined, ECMA array, strict
//! array, date, long string, end-of-object sentinel. We never emit nor
//! consume AMF3 — RTMP occasionally upgrades a channel to AMF3 via the
//! `avmplus` command, but we simply keep the channel on AMF0; every
//! client / server we care about (OBS, Wirecast, nginx-rtmp, libavformat)
//! stays in AMF0 for publish flows.
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

/// One AMF0 value. Ordered-object semantics are preserved through the
/// `Vec<(key, value)>` — RTMP command handlers sometimes care about
/// the order of fields in the command object (libavformat walks them
/// positionally), so we don't dedup into a map.
#[derive(Debug, Clone, PartialEq)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    /// Ordered association list. Kept ordered because some peers
    /// (libavformat's rtmpproto) expect the `tcUrl` field at a fixed
    /// offset in connect's command object.
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
}

impl Amf0Value {
    /// Look up a direct child of an Object / EcmaArray by name.
    pub fn get(&self, key: &str) -> Option<&Amf0Value> {
        match self {
            Amf0Value::Object(v) | Amf0Value::EcmaArray(v) => {
                v.iter().find(|(k, _)| k == key).map(|(_, v)| v)
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
pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Amf0Value> {
    decode_at_depth(buf, pos, 0)
}

fn decode_at_depth(buf: &[u8], pos: &mut usize, depth: usize) -> Result<Amf0Value> {
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
        M_OBJECT => Ok(Amf0Value::Object(read_object_body_at_depth(
            buf,
            pos,
            depth + 1,
        )?)),
        M_ECMA_ARRAY => {
            // Spec says this is preceded by a u32 associative count, then
            // the body is the same key/value/OBJECT_END terminator
            // sequence as a plain Object. The count is advisory — walk
            // until OBJECT_END.
            let _count = read_u32_be(buf, pos)?;
            Ok(Amf0Value::EcmaArray(read_object_body_at_depth(
                buf,
                pos,
                depth + 1,
            )?))
        }
        M_STRICT_ARRAY => {
            let count = read_u32_be(buf, pos)? as usize;
            let mut out = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                out.push(decode_at_depth(buf, pos, depth + 1)?);
            }
            Ok(Amf0Value::StrictArray(out))
        }
        M_DATE => {
            let bits = read_u64_be(buf, pos)?;
            let tz = read_i16_be(buf, pos)?;
            Ok(Amf0Value::Date {
                millis: f64::from_bits(bits),
                timezone: tz,
            })
        }
        M_REFERENCE => Err(Error::InvalidAmf0(
            "REFERENCE marker not supported (AMF0 object references are \
             exceedingly rare in RTMP)"
                .into(),
        )),
        other => Err(Error::InvalidAmf0(format!("unknown marker {other:#x}"))),
    }
}

/// Decode a sequence of AMF0 values until the input is exhausted.
/// Useful for parsing command payloads where the count isn't known in
/// advance (connect / _result / onStatus etc.).
pub fn decode_all(buf: &[u8]) -> Result<Vec<Amf0Value>> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < buf.len() {
        out.push(decode(buf, &mut pos)?);
    }
    Ok(out)
}

fn read_object_body_at_depth(
    buf: &[u8],
    pos: &mut usize,
    depth: usize,
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
        let value = decode_at_depth(buf, pos, depth)?;
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
    fn rejects_reference_marker() {
        // Marker 0x07 = REFERENCE — not supported.
        let b = [0x07, 0x00, 0x00];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }
}

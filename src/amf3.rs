//! AMF3 (Action Message Format 3) value encoder + decoder.
//!
//! AMF3 is the wire format Flash Player 9+ uses to serialize ActionScript
//! 3.0 object graphs. RTMP can switch a channel from AMF0 to AMF3 via the
//! AMF0 `avmplus-object-marker` (0x11), and also dedicates message type
//! IDs 15 (Data), 16 (Shared Object) and 17 (Command) for streams that
//! are AMF3-encoded from the start. Real-world ingest endpoints
//! (nginx-rtmp / OBS / Wirecast) negotiate down to AMF0 in practice, but
//! a small fraction of Adobe Media Server clients open AMF3 channels —
//! this module gives us the parser surface those clients need.
//!
//! Reference: Adobe "Action Message Format -- AMF 3" (January 2013),
//! mirrored under `docs/streaming/rtmp/amf3-file-format-spec-adobe.pdf`.
//!
//! Scope of this implementation:
//!
//! * All thirteen value markers (§3.1).
//! * U29 variable-length-integer (§1.3.1) with strict 29-bit range
//!   enforcement.
//! * U29 length flag dispatch — every type that uses U29 as "value or
//!   reference" pulls from its appropriate reference table (string /
//!   object / traits).
//! * Object types — anonymous (no class name), typed (named class name,
//!   sealed properties), dynamic (sealed + trailing string-keyed
//!   members), externalizable (caller decides body bytes — surfaced as
//!   an opaque blob).
//! * Three reference tables maintained per `decode_all` / `encode_all`
//!   invocation (§2.2). Strings, complex-objects and traits each get
//!   their own table.
//!
//! Out of scope (intentionally — empty-stub variants in `Amf3Value`
//! would mislead callers):
//!
//! * Round-tripping by-reference into by-reference on encode. We always
//!   emit literal values; the spec allows that, and decoded references
//!   are resolved into owned values. A caller that needs cycle-preserving
//!   serialization can use the `encode_with_refs` builder API once we
//!   need it; the on-wire bytes of any ingest endpoint we encounter today
//!   are dominated by literal payloads.

use std::collections::HashMap;

use crate::error::{Error, Result};

// Marker bytes — §3.1 Table.
const M_UNDEFINED: u8 = 0x00;
const M_NULL: u8 = 0x01;
const M_FALSE: u8 = 0x02;
const M_TRUE: u8 = 0x03;
const M_INTEGER: u8 = 0x04;
const M_DOUBLE: u8 = 0x05;
const M_STRING: u8 = 0x06;
const M_XML_DOC: u8 = 0x07;
const M_DATE: u8 = 0x08;
const M_ARRAY: u8 = 0x09;
const M_OBJECT: u8 = 0x0A;
const M_XML: u8 = 0x0B;
const M_BYTE_ARRAY: u8 = 0x0C;
const M_VECTOR_INT: u8 = 0x0D;
const M_VECTOR_UINT: u8 = 0x0E;
const M_VECTOR_DOUBLE: u8 = 0x0F;
const M_VECTOR_OBJECT: u8 = 0x10;
const M_DICTIONARY: u8 = 0x11;

/// One AMF3 value. References from the on-wire stream are resolved into
/// owned values before this enum is constructed — callers see the same
/// value twice if the wire stream re-used a string / object reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Amf3Value {
    Undefined,
    Null,
    Boolean(bool),
    /// AMF3 integer: sign-extended 29-bit signed value (§3.6).
    Integer(i32),
    /// IEEE-754 double — also used for ActionScript Number / uint values
    /// outside the 28-bit signed integer range (§3.7).
    Double(f64),
    String(String),
    /// Legacy `flash.xml.XMLDocument` (§3.9). Body is UTF-8.
    XmlDocument(String),
    /// `flash.xml.XML` (E4X) (§3.13). Body is UTF-8.
    Xml(String),
    /// Milliseconds since UNIX epoch, UTC (§3.10). No timezone field —
    /// AMF3 deliberately dropped the AMF0 timezone slot.
    Date(f64),
    /// AMF3 Array — dense ordinal portion + name/value associative
    /// portion (§3.11). The dense slot is the index-ordered `Vec`; the
    /// associative slot is the trailing key/value list.
    Array {
        dense: Vec<Amf3Value>,
        assoc: Vec<(String, Amf3Value)>,
    },
    /// AMF3 Object — anonymous, typed, dynamic or externalizable
    /// (§3.12). `class_name` is the empty string for anonymous objects.
    /// `dynamic` flags the presence of the trailing string-keyed member
    /// section. `externalizable_body` is `Some(bytes)` for an
    /// externalizable type and `None` for normal sealed-and-maybe-dynamic
    /// objects; spec §3.12 leaves the bytes' interpretation to the
    /// caller (the class implements `IExternalizable.writeExternal`).
    Object {
        class_name: String,
        dynamic: bool,
        sealed: Vec<(String, Amf3Value)>,
        dynamic_members: Vec<(String, Amf3Value)>,
        externalizable_body: Option<Vec<u8>>,
    },
    /// `flash.utils.ByteArray` (§3.14). Raw octets.
    ByteArray(Vec<u8>),
    /// `Vector.<int>` (§3.15). Fixed-length signed 32-bit values.
    VectorInt {
        fixed: bool,
        items: Vec<i32>,
    },
    /// `Vector.<uint>` (§3.15). Fixed-length unsigned 32-bit values.
    VectorUInt {
        fixed: bool,
        items: Vec<u32>,
    },
    /// `Vector.<Number>` (§3.15). Fixed-length IEEE-754 doubles.
    VectorDouble {
        fixed: bool,
        items: Vec<f64>,
    },
    /// `Vector.<*>` / `Vector.<Object>` (§3.15). `object_type_name` is
    /// the ActionScript class name; `*` means "any".
    VectorObject {
        fixed: bool,
        object_type_name: String,
        items: Vec<Amf3Value>,
    },
    /// `flash.utils.Dictionary` (§3.16). Keys can be arbitrary values
    /// (not just strings); `weak_keys` mirrors the AS3 construction
    /// flag.
    Dictionary {
        weak_keys: bool,
        entries: Vec<(Amf3Value, Amf3Value)>,
    },
}

impl Amf3Value {
    /// Look up a sealed / dynamic property of an Object by name. Returns
    /// `None` for any non-Object value.
    pub fn get(&self, key: &str) -> Option<&Amf3Value> {
        if let Amf3Value::Object {
            sealed,
            dynamic_members,
            ..
        } = self
        {
            sealed
                .iter()
                .chain(dynamic_members.iter())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
        } else {
            None
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Amf3Value::String(s) | Amf3Value::Xml(s) | Amf3Value::XmlDocument(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Amf3Value::Integer(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Amf3Value::Double(n) => Some(*n),
            Amf3Value::Integer(n) => Some(f64::from(*n)),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Amf3Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Project this AMF3 value onto the matching [`Amf0Value`](crate::amf::Amf0Value) shape.
    ///
    /// RTMP data / command messages can be carried either AMF0-encoded
    /// (message type ids 18 / 20) or AMF3-encoded (15 / 17), and a
    /// publisher chooses one or the other per channel. Consumers of
    /// `server::StreamPacket::Metadata` shouldn't have to branch on which
    /// encoding the publisher happened to pick — both carry the same
    /// `onMetaData` object semantics. This bridge maps the AMF3 value
    /// graph onto the AMF0 enum so a single metadata-handling path covers
    /// both.
    ///
    /// The two formats are not isomorphic, so the mapping is lossy in the
    /// directions AMF0 can't represent:
    ///
    /// * `Integer` / `Date` collapse to `Amf0Value::Number` /
    ///   `Amf0Value::Date` (AMF0 has no integer type; its `Date` keeps
    ///   a timezone slot AMF3 dropped, filled with 0).
    /// * Both AMF3 `Object` member sections (sealed + dynamic) concatenate
    ///   into one ordered `Amf0Value::Object`; the class name and the
    ///   externalizable body are not representable in AMF0 and are
    ///   dropped.
    /// * The AMF3 `Array` dense slot becomes an ECMA-array under
    ///   stringified ordinal keys merged with the associative slot — the
    ///   shape `onMetaData` arrays use in practice.
    /// * `ByteArray` / `Vector` / `Dictionary` / `Xml` have no AMF0
    ///   counterpart; `ByteArray` and the vectors become a
    ///   `Amf0Value::StrictArray` of numbers, `Xml*` become
    ///   `Amf0Value::String`, and `Dictionary` becomes an
    ///   `Amf0Value::EcmaArray` keyed by each entry's stringified key.
    pub fn to_amf0(&self) -> crate::amf::Amf0Value {
        use crate::amf::Amf0Value as A0;
        match self {
            Amf3Value::Undefined => A0::Undefined,
            Amf3Value::Null => A0::Null,
            Amf3Value::Boolean(b) => A0::Boolean(*b),
            Amf3Value::Integer(n) => A0::Number(f64::from(*n)),
            Amf3Value::Double(n) => A0::Number(*n),
            Amf3Value::String(s) => A0::String(s.clone()),
            Amf3Value::XmlDocument(s) | Amf3Value::Xml(s) => A0::String(s.clone()),
            Amf3Value::Date(ms) => A0::Date {
                millis: *ms,
                timezone: 0,
            },
            Amf3Value::Array { dense, assoc } => {
                // onMetaData "arrays" are normally pure-associative ECMA
                // arrays; preserve both portions by emitting the dense
                // entries under their stringified ordinal then the named
                // members.
                let mut pairs = Vec::with_capacity(dense.len() + assoc.len());
                for (i, v) in dense.iter().enumerate() {
                    pairs.push((i.to_string(), v.to_amf0()));
                }
                for (k, v) in assoc {
                    pairs.push((k.clone(), v.to_amf0()));
                }
                A0::EcmaArray(pairs)
            }
            Amf3Value::Object {
                sealed,
                dynamic_members,
                ..
            } => {
                let mut pairs = Vec::with_capacity(sealed.len() + dynamic_members.len());
                for (k, v) in sealed.iter().chain(dynamic_members.iter()) {
                    pairs.push((k.clone(), v.to_amf0()));
                }
                A0::Object(pairs)
            }
            Amf3Value::ByteArray(bytes) => {
                A0::StrictArray(bytes.iter().map(|b| A0::Number(f64::from(*b))).collect())
            }
            Amf3Value::VectorInt { items, .. } => {
                A0::StrictArray(items.iter().map(|n| A0::Number(f64::from(*n))).collect())
            }
            Amf3Value::VectorUInt { items, .. } => {
                A0::StrictArray(items.iter().map(|n| A0::Number(f64::from(*n))).collect())
            }
            Amf3Value::VectorDouble { items, .. } => {
                A0::StrictArray(items.iter().map(|n| A0::Number(*n)).collect())
            }
            Amf3Value::VectorObject { items, .. } => {
                A0::StrictArray(items.iter().map(Amf3Value::to_amf0).collect())
            }
            Amf3Value::Dictionary { entries, .. } => {
                let pairs = entries
                    .iter()
                    .map(|(k, v)| (amf3_key_to_string(k), v.to_amf0()))
                    .collect();
                A0::EcmaArray(pairs)
            }
        }
    }
}

/// Best-effort stringification of an AMF3 value used as a Dictionary key.
/// AS3 Dictionary keys may be any value; AMF0 ECMA-array keys must be
/// strings. Scalar keys map to their natural text; complex keys fall back
/// to a stable placeholder so the entry is still surfaced.
fn amf3_key_to_string(k: &Amf3Value) -> String {
    match k {
        Amf3Value::String(s) | Amf3Value::Xml(s) | Amf3Value::XmlDocument(s) => s.clone(),
        Amf3Value::Integer(n) => n.to_string(),
        Amf3Value::Double(n) => n.to_string(),
        Amf3Value::Boolean(b) => b.to_string(),
        Amf3Value::Null => "null".to_string(),
        Amf3Value::Undefined => "undefined".to_string(),
        _ => "[object]".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Decoder state — owns the three reference tables that survive across
/// values inside one packet (§4.1).
#[derive(Default)]
pub struct Decoder {
    strings: Vec<String>,
    objects: Vec<Amf3Value>,
    traits: Vec<TraitDef>,
}

#[derive(Debug, Clone)]
struct TraitDef {
    class_name: String,
    dynamic: bool,
    externalizable: bool,
    sealed_members: Vec<String>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset all three reference tables. Per §4.1.2 / §4.2, encoders
    /// must reset tables at packet / context-header boundaries; callers
    /// who reuse a `Decoder` across packets must call this at each
    /// boundary.
    pub fn reset_tables(&mut self) {
        self.strings.clear();
        self.objects.clear();
        self.traits.clear();
    }

    /// Decode one AMF3 value from `buf` starting at `*pos`. Advances
    /// `pos` past the value on success.
    pub fn decode(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let marker = read_u8(buf, pos)?;
        match marker {
            M_UNDEFINED => Ok(Amf3Value::Undefined),
            M_NULL => Ok(Amf3Value::Null),
            M_FALSE => Ok(Amf3Value::Boolean(false)),
            M_TRUE => Ok(Amf3Value::Boolean(true)),
            M_INTEGER => Ok(Amf3Value::Integer(read_i29(buf, pos)?)),
            M_DOUBLE => {
                let bits = read_u64_be(buf, pos)?;
                Ok(Amf3Value::Double(f64::from_bits(bits)))
            }
            M_STRING => Ok(Amf3Value::String(self.read_string(buf, pos)?)),
            M_XML_DOC => Ok(Amf3Value::XmlDocument(self.read_object_string(buf, pos)?)),
            M_XML => Ok(Amf3Value::Xml(self.read_object_string(buf, pos)?)),
            M_DATE => self.read_date(buf, pos),
            M_ARRAY => self.read_array(buf, pos),
            M_OBJECT => self.read_object(buf, pos),
            M_BYTE_ARRAY => self.read_byte_array(buf, pos),
            M_VECTOR_INT => self.read_vector_int(buf, pos),
            M_VECTOR_UINT => self.read_vector_uint(buf, pos),
            M_VECTOR_DOUBLE => self.read_vector_double(buf, pos),
            M_VECTOR_OBJECT => self.read_vector_object(buf, pos),
            M_DICTIONARY => self.read_dictionary(buf, pos),
            other => Err(Error::InvalidAmf0(format!(
                "amf3: unknown marker {other:#x}"
            ))),
        }
    }

    fn read_string(&mut self, buf: &[u8], pos: &mut usize) -> Result<String> {
        let header = read_u29(buf, pos)?;
        // §3.8 / §1.3.2: low bit is the literal flag.
        if (header & 1) == 0 {
            let idx = (header >> 1) as usize;
            let s = self
                .strings
                .get(idx)
                .ok_or_else(|| Error::InvalidAmf0(format!("amf3 string ref {idx} out of range")))?;
            Ok(s.clone())
        } else {
            let len = (header >> 1) as usize;
            let s = read_utf8_body(buf, pos, len)?;
            // Empty string is never sent by reference and never goes
            // into the table (§1.3.2 "UTF-8-empty").
            if !s.is_empty() {
                self.strings.push(s.clone());
            }
            Ok(s)
        }
    }

    /// Helper for XML / XMLDocument — they store *into* the object
    /// reference table (not the string table — §3.9 / §3.13). The U29
    /// header's bit 0 is "literal flag"; same shape as a string but
    /// distinct table.
    fn read_object_string(&mut self, buf: &[u8], pos: &mut usize) -> Result<String> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            let idx = (header >> 1) as usize;
            let v = self
                .objects
                .get(idx)
                .ok_or_else(|| Error::InvalidAmf0(format!("amf3 xml ref {idx} out of range")))?;
            match v {
                Amf3Value::Xml(s) | Amf3Value::XmlDocument(s) => Ok(s.clone()),
                _ => Err(Error::InvalidAmf0(format!(
                    "amf3 xml ref {idx} resolved to non-xml value"
                ))),
            }
        } else {
            let len = (header >> 1) as usize;
            let s = read_utf8_body(buf, pos, len)?;
            // Insert as XmlDocument; callers reading XML / XmlDocument
            // both use this helper and the discriminating marker is
            // restored at the call site.
            self.objects.push(Amf3Value::XmlDocument(s.clone()));
            Ok(s)
        }
    }

    fn read_date(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            let idx = (header >> 1) as usize;
            return self.lookup_object(idx);
        }
        let bits = read_u64_be(buf, pos)?;
        let v = Amf3Value::Date(f64::from_bits(bits));
        self.objects.push(v.clone());
        Ok(v)
    }

    fn read_byte_array(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let len = (header >> 1) as usize;
        let bytes = read_bytes(buf, pos, len)?;
        let v = Amf3Value::ByteArray(bytes);
        self.objects.push(v.clone());
        Ok(v)
    }

    fn read_array(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let dense_count = (header >> 1) as usize;
        // Reserve a placeholder slot in the object table BEFORE we
        // decode children so any recursive reference inside the array
        // can resolve to this same array. We push a sentinel and
        // overwrite once the dense + assoc portions are known.
        let slot = self.objects.len();
        self.objects.push(Amf3Value::Null);

        let mut assoc = Vec::new();
        loop {
            let k = self.read_string(buf, pos)?;
            if k.is_empty() {
                break;
            }
            let v = self.decode(buf, pos)?;
            assoc.push((k, v));
        }
        let mut dense = Vec::with_capacity(dense_count.min(1024));
        for _ in 0..dense_count {
            dense.push(self.decode(buf, pos)?);
        }
        let v = Amf3Value::Array { dense, assoc };
        self.objects[slot] = v.clone();
        Ok(v)
    }

    fn read_object(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        // Bit 0 of the U29 header: 0 = object reference, 1 = inline.
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        // Inline object. Bit 1 distinguishes traits-by-reference (0)
        // from inline traits (1).
        let trait_def = if (header & 2) == 0 {
            let idx = (header >> 2) as usize;
            self.traits
                .get(idx)
                .ok_or_else(|| Error::InvalidAmf0(format!("amf3 trait ref {idx} out of range")))?
                .clone()
        } else {
            // Inline traits. Bit 2 = externalizable, bit 3 = dynamic,
            // upper 25 bits = sealed-member count. Note: for
            // externalizable traits, bit 3 (dynamic) is always 0 and
            // the sealed-member count is always 0 — the body is the
            // class's IExternalizable payload.
            let externalizable = (header & 4) != 0;
            let dynamic = (header & 8) != 0;
            let sealed_count = (header >> 4) as usize;
            let class_name = self.read_string(buf, pos)?;
            let mut sealed_members = Vec::with_capacity(sealed_count.min(1024));
            for _ in 0..sealed_count {
                sealed_members.push(self.read_string(buf, pos)?);
            }
            let t = TraitDef {
                class_name,
                dynamic,
                externalizable,
                sealed_members,
            };
            self.traits.push(t.clone());
            t
        };

        // Reserve the object slot before decoding members so cyclic
        // references inside the body resolve to this object.
        let slot = self.objects.len();
        self.objects.push(Amf3Value::Null);

        if trait_def.externalizable {
            // The class's IExternalizable.writeExternal output is
            // an opaque byte stream — the spec leaves length / shape
            // to the class. We can't decode it generically; surface
            // the *remaining* buffer as the externalizable body and
            // expect the caller to know how much to consume by other
            // means. To keep the decoder's `pos` honest, we treat the
            // body as "what the caller registers via a class
            // handler" — for now, refuse so a downstream caller can
            // implement the hook explicitly rather than silently
            // produce garbage. This matches `decode`'s policy
            // elsewhere: unknown shapes are loud, not lossy.
            return Err(Error::InvalidAmf0(format!(
                "amf3: externalizable class {:?} requires a registered handler; \
                 generic decoder cannot determine body length",
                trait_def.class_name
            )));
        }

        let mut sealed = Vec::with_capacity(trait_def.sealed_members.len());
        for name in &trait_def.sealed_members {
            let value = self.decode(buf, pos)?;
            sealed.push((name.clone(), value));
        }
        let mut dynamic_members = Vec::new();
        if trait_def.dynamic {
            loop {
                let k = self.read_string(buf, pos)?;
                if k.is_empty() {
                    break;
                }
                let v = self.decode(buf, pos)?;
                dynamic_members.push((k, v));
            }
        }
        let v = Amf3Value::Object {
            class_name: trait_def.class_name.clone(),
            dynamic: trait_def.dynamic,
            sealed,
            dynamic_members,
            externalizable_body: None,
        };
        self.objects[slot] = v.clone();
        Ok(v)
    }

    fn read_vector_int(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let count = (header >> 1) as usize;
        let fixed = read_u8(buf, pos)? != 0;
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(read_u32_be(buf, pos)? as i32);
        }
        let v = Amf3Value::VectorInt { fixed, items };
        self.objects.push(v.clone());
        Ok(v)
    }

    fn read_vector_uint(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let count = (header >> 1) as usize;
        let fixed = read_u8(buf, pos)? != 0;
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(read_u32_be(buf, pos)?);
        }
        let v = Amf3Value::VectorUInt { fixed, items };
        self.objects.push(v.clone());
        Ok(v)
    }

    fn read_vector_double(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let count = (header >> 1) as usize;
        let fixed = read_u8(buf, pos)? != 0;
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let bits = read_u64_be(buf, pos)?;
            items.push(f64::from_bits(bits));
        }
        let v = Amf3Value::VectorDouble { fixed, items };
        self.objects.push(v.clone());
        Ok(v)
    }

    fn read_vector_object(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let count = (header >> 1) as usize;
        let fixed = read_u8(buf, pos)? != 0;
        let object_type_name = self.read_string(buf, pos)?;
        let slot = self.objects.len();
        self.objects.push(Amf3Value::Null);
        let mut items = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            items.push(self.decode(buf, pos)?);
        }
        let v = Amf3Value::VectorObject {
            fixed,
            object_type_name,
            items,
        };
        self.objects[slot] = v.clone();
        Ok(v)
    }

    fn read_dictionary(&mut self, buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
        let header = read_u29(buf, pos)?;
        if (header & 1) == 0 {
            return self.lookup_object((header >> 1) as usize);
        }
        let count = (header >> 1) as usize;
        let weak_keys = read_u8(buf, pos)? != 0;
        let slot = self.objects.len();
        self.objects.push(Amf3Value::Null);
        let mut entries = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let k = self.decode(buf, pos)?;
            let v = self.decode(buf, pos)?;
            entries.push((k, v));
        }
        let v = Amf3Value::Dictionary { weak_keys, entries };
        self.objects[slot] = v.clone();
        Ok(v)
    }

    fn lookup_object(&self, idx: usize) -> Result<Amf3Value> {
        self.objects
            .get(idx)
            .cloned()
            .ok_or_else(|| Error::InvalidAmf0(format!("amf3 object ref {idx} out of range")))
    }
}

/// Decode one AMF3 value at top level, using a fresh reference-table
/// context.
pub fn decode(buf: &[u8], pos: &mut usize) -> Result<Amf3Value> {
    Decoder::new().decode(buf, pos)
}

/// Decode a sequence of AMF3 values until the input is exhausted, all
/// sharing a single reference-table context (per §4.1 — context resets
/// at packet boundaries, not value boundaries).
pub fn decode_all(buf: &[u8]) -> Result<Vec<Amf3Value>> {
    let mut dec = Decoder::new();
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < buf.len() {
        out.push(dec.decode(buf, &mut pos)?);
    }
    Ok(out)
}

/// AMF0 `avmplus-object-marker` — signals a switch from the AMF0 outer
/// framing to an AMF3-encoded value (AMF0 spec §3.1).
pub const AVMPLUS_OBJECT_MARKER: u8 = 0x11;

/// Decode an RTMP AMF3 data / command message body (message type ids 15
/// and 17) into a flat sequence of [`Amf3Value`]s.
///
/// Per AMF 3 spec §4.1 and AMF 0 spec §3.1, the outer NetConnection
/// messaging structure is AMF0; an individual value switches to AMF3 by
/// prefixing it with the `avmplus-object-marker` (`0x11`). RTMP type-15 /
/// type-17 bodies therefore consist of one-or-more values, each
/// optionally introduced by that marker. This decoder accepts both:
///
/// * a leading `0x11` marker before a value (the spec-mandated AMF3
///   switch), and
/// * a value with no marker at the top of the body — emitted by some
///   senders that drop straight into AMF3 because the channel was
///   already negotiated to AMF3 — by treating any non-`0x11` first byte
///   as the start of an AMF3 marker directly.
///
/// All values in one body share a single reference-table context, reset
/// once at entry per §4.1.
pub fn decode_data_message(buf: &[u8]) -> Result<Vec<Amf3Value>> {
    let mut dec = Decoder::new();
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < buf.len() {
        // A `0x11` here is the AMF0 avmplus switch; consume it and decode
        // the AMF3 value that follows. Any other byte is taken to be an
        // AMF3 marker directly (already-AMF3 channel).
        if buf[pos] == AVMPLUS_OBJECT_MARKER {
            pos += 1;
            if pos >= buf.len() {
                return Err(Error::InvalidAmf0(
                    "avmplus marker (0x11) at end of body with no AMF3 value".into(),
                ));
            }
        }
        out.push(dec.decode(buf, &mut pos)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Append one AMF3 value to `out`. Every value is encoded literally; the
/// reference tables are not used on the encode side (the spec permits
/// this — a U29 with the literal-flag set is always valid). Callers who
/// need round-tripping by-reference can compose a custom encoder using
/// the lower-level helpers below.
pub fn encode(out: &mut Vec<u8>, v: &Amf3Value) {
    match v {
        Amf3Value::Undefined => out.push(M_UNDEFINED),
        Amf3Value::Null => out.push(M_NULL),
        Amf3Value::Boolean(false) => out.push(M_FALSE),
        Amf3Value::Boolean(true) => out.push(M_TRUE),
        Amf3Value::Integer(n) => {
            // §3.6: AMF3 integers are 29-bit signed; values outside
            // [-2^28, 2^28) must be encoded as doubles. The caller is
            // responsible for picking the right variant; if they pass
            // an out-of-range Integer we fall back to Double for
            // correctness.
            if (-(1 << 28)..(1 << 28)).contains(n) {
                out.push(M_INTEGER);
                write_i29(out, *n);
            } else {
                out.push(M_DOUBLE);
                out.extend_from_slice(&(f64::from(*n)).to_bits().to_be_bytes());
            }
        }
        Amf3Value::Double(d) => {
            out.push(M_DOUBLE);
            out.extend_from_slice(&d.to_bits().to_be_bytes());
        }
        Amf3Value::String(s) => {
            out.push(M_STRING);
            write_u29_string(out, s);
        }
        Amf3Value::XmlDocument(s) => {
            out.push(M_XML_DOC);
            write_u29_string(out, s);
        }
        Amf3Value::Xml(s) => {
            out.push(M_XML);
            write_u29_string(out, s);
        }
        Amf3Value::Date(ms) => {
            out.push(M_DATE);
            // Literal flag: U29 = (0<<1)|1 = 1.
            write_u29(out, 1);
            out.extend_from_slice(&ms.to_bits().to_be_bytes());
        }
        Amf3Value::ByteArray(b) => {
            out.push(M_BYTE_ARRAY);
            // U29 = (len<<1)|1.
            write_u29(out, ((b.len() as u32) << 1) | 1);
            out.extend_from_slice(b);
        }
        Amf3Value::Array { dense, assoc } => {
            out.push(M_ARRAY);
            // U29 = (dense_count<<1)|1.
            write_u29(out, ((dense.len() as u32) << 1) | 1);
            // Associative section: name/value pairs terminated by the
            // empty string (literal-flag set, len 0).
            for (k, v) in assoc {
                write_u29_string(out, k);
                encode(out, v);
            }
            write_u29(out, 1); // empty string literal terminator
            for v in dense {
                encode(out, v);
            }
        }
        Amf3Value::Object {
            class_name,
            dynamic,
            sealed,
            dynamic_members,
            externalizable_body,
        } => {
            out.push(M_OBJECT);
            if let Some(body) = externalizable_body {
                // U29O-traits-ext: lower nibble = 0b0111
                //   bit0=1 (literal object)
                //   bit1=1 (literal traits, not by reference)
                //   bit2=1 (externalizable flag)
                //   bit3=0 (dynamic flag — always 0 for externalizable)
                //   sealed-member count = 0
                write_u29(out, 0b0111);
                write_u29_string(out, class_name);
                out.extend_from_slice(body);
            } else {
                // U29O-traits: lower nibble = 0b1011 with dynamic bit
                //   bit0=1 (literal object)
                //   bit1=1 (literal traits, not by reference)
                //   bit2=0 (not externalizable)
                //   bit3=dynamic
                //   bits 4+ = sealed-member count
                let mut header: u32 = 0b0011;
                if *dynamic {
                    header |= 0b1000;
                }
                header |= (sealed.len() as u32) << 4;
                write_u29(out, header);
                write_u29_string(out, class_name);
                for (name, _) in sealed {
                    write_u29_string(out, name);
                }
                for (_, value) in sealed {
                    encode(out, value);
                }
                if *dynamic {
                    for (k, v) in dynamic_members {
                        write_u29_string(out, k);
                        encode(out, v);
                    }
                    write_u29(out, 1); // empty string literal terminator
                }
            }
        }
        Amf3Value::VectorInt { fixed, items } => {
            out.push(M_VECTOR_INT);
            write_u29(out, ((items.len() as u32) << 1) | 1);
            out.push(if *fixed { 1 } else { 0 });
            for n in items {
                out.extend_from_slice(&(*n as u32).to_be_bytes());
            }
        }
        Amf3Value::VectorUInt { fixed, items } => {
            out.push(M_VECTOR_UINT);
            write_u29(out, ((items.len() as u32) << 1) | 1);
            out.push(if *fixed { 1 } else { 0 });
            for n in items {
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
        Amf3Value::VectorDouble { fixed, items } => {
            out.push(M_VECTOR_DOUBLE);
            write_u29(out, ((items.len() as u32) << 1) | 1);
            out.push(if *fixed { 1 } else { 0 });
            for d in items {
                out.extend_from_slice(&d.to_bits().to_be_bytes());
            }
        }
        Amf3Value::VectorObject {
            fixed,
            object_type_name,
            items,
        } => {
            out.push(M_VECTOR_OBJECT);
            write_u29(out, ((items.len() as u32) << 1) | 1);
            out.push(if *fixed { 1 } else { 0 });
            write_u29_string(out, object_type_name);
            for v in items {
                encode(out, v);
            }
        }
        Amf3Value::Dictionary { weak_keys, entries } => {
            out.push(M_DICTIONARY);
            write_u29(out, ((entries.len() as u32) << 1) | 1);
            out.push(if *weak_keys { 1 } else { 0 });
            for (k, v) in entries {
                encode(out, k);
                encode(out, v);
            }
        }
    }
}

/// Encode each value in order against a single output buffer.
pub fn encode_all(values: &[Amf3Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 * values.len());
    for v in values {
        encode(&mut out, v);
    }
    out
}

/// Convenience builder for an anonymous (no-class-name, non-dynamic) AMF3
/// object — the shape RTMP AMF3 command-objects almost always take.
#[allow(dead_code)]
pub fn anon_object<I, S>(pairs: I) -> Amf3Value
where
    I: IntoIterator<Item = (S, Amf3Value)>,
    S: Into<String>,
{
    let sealed: Vec<(String, Amf3Value)> = pairs.into_iter().map(|(k, v)| (k.into(), v)).collect();
    Amf3Value::Object {
        class_name: String::new(),
        dynamic: false,
        sealed,
        dynamic_members: Vec::new(),
        externalizable_body: None,
    }
}

/// Convenience builder for a dynamic anonymous object (no sealed
/// members, all properties in the dynamic section). Mirrors the typical
/// AS3 `{a: 1, b: 2}` literal shape.
#[allow(dead_code)]
pub fn dynamic_object<I, S>(pairs: I) -> Amf3Value
where
    I: IntoIterator<Item = (S, Amf3Value)>,
    S: Into<String>,
{
    let members: Vec<(String, Amf3Value)> = pairs.into_iter().map(|(k, v)| (k.into(), v)).collect();
    Amf3Value::Object {
        class_name: String::new(),
        dynamic: true,
        sealed: Vec::new(),
        dynamic_members: members,
        externalizable_body: None,
    }
}

/// `obj_unordered` parallel to the AMF0 helper — same caveat about
/// non-deterministic key order.
#[allow(dead_code)]
pub fn anon_object_unordered(map: HashMap<String, Amf3Value>) -> Amf3Value {
    Amf3Value::Object {
        class_name: String::new(),
        dynamic: false,
        sealed: map.into_iter().collect(),
        dynamic_members: Vec::new(),
        externalizable_body: None,
    }
}

// ---------------------------------------------------------------------------
// Primitive helpers — U29, UTF-8, etc.
// ---------------------------------------------------------------------------

/// Read a U29 variable-length unsigned 29-bit integer (§1.3.1).
///
/// Layout:
/// * 1 byte  — `0xxxxxxx`             — 7 bits (0..=0x7F)
/// * 2 bytes — `1xxxxxxx 0xxxxxxx`    — 14 bits (..=0x3FFF)
/// * 3 bytes — `1xxxxxxx 1xxxxxxx 0xxxxxxx` — 21 bits (..=0x1FFFFF)
/// * 4 bytes — `1xxxxxxx 1xxxxxxx 1xxxxxxx xxxxxxxx` — 29 bits
pub fn read_u29(buf: &[u8], pos: &mut usize) -> Result<u32> {
    let mut value: u32 = 0;
    for i in 0..3 {
        let b = read_u8(buf, pos)? as u32;
        if (b & 0x80) == 0 {
            value = (value << 7) | b;
            return Ok(value);
        }
        value = (value << 7) | (b & 0x7F);
        // Three high bytes have used their MSB-as-continuation; on the
        // 4th iteration the byte is consumed whole (8 bits).
        if i == 2 {
            let b4 = read_u8(buf, pos)? as u32;
            value = (value << 8) | b4;
            // Top three flag bits + 8 full bits = 29 bits used.
            return Ok(value);
        }
    }
    unreachable!("loop returns or falls into i==2 branch")
}

/// Read a U29 and sign-extend to a 29-bit signed value (§3.6).
fn read_i29(buf: &[u8], pos: &mut usize) -> Result<i32> {
    let v = read_u29(buf, pos)?;
    // Sign-extend bit 28 into the upper 3 bits of an i32.
    if v & 0x1000_0000 != 0 {
        Ok((v | 0xE000_0000) as i32)
    } else {
        Ok(v as i32)
    }
}

/// Write a U29-encoded unsigned 29-bit integer (§1.3.1).
pub fn write_u29(out: &mut Vec<u8>, mut v: u32) {
    debug_assert!(v < (1 << 29), "U29 input out of range: {v:#x}");
    v &= 0x1FFF_FFFF;
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x4000 {
        out.push(((v >> 7) | 0x80) as u8);
        out.push((v & 0x7F) as u8);
    } else if v < 0x20_0000 {
        out.push(((v >> 14) | 0x80) as u8);
        out.push((((v >> 7) & 0x7F) | 0x80) as u8);
        out.push((v & 0x7F) as u8);
    } else {
        // 4-byte form: top 3 bytes use 7 bits each, last byte uses 8.
        out.push((((v >> 22) & 0x7F) | 0x80) as u8);
        out.push((((v >> 15) & 0x7F) | 0x80) as u8);
        out.push((((v >> 8) & 0x7F) | 0x80) as u8);
        out.push((v & 0xFF) as u8);
    }
}

/// Write a signed 29-bit integer using the U29 encoding (§3.6 — values
/// in `[-2^28, 2^28)`).
fn write_i29(out: &mut Vec<u8>, v: i32) {
    debug_assert!(
        (-(1 << 28)..(1 << 28)).contains(&v),
        "i29 input out of range: {v}"
    );
    write_u29(out, (v as u32) & 0x1FFF_FFFF);
}

/// Write a UTF-8 string in the AMF3 "literal" form — U29 = (len << 1) | 1
/// followed by the UTF-8 bytes.
fn write_u29_string(out: &mut Vec<u8>, s: &str) {
    write_u29(out, ((s.len() as u32) << 1) | 1);
    out.extend_from_slice(s.as_bytes());
}

#[inline]
fn read_u8(buf: &[u8], pos: &mut usize) -> Result<u8> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| Error::InvalidAmf0("amf3 truncated".into()))?;
    *pos += 1;
    Ok(b)
}

#[inline]
fn read_u32_be(buf: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(Error::InvalidAmf0("amf3 truncated u32".into()));
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Ok(v)
}

#[inline]
fn read_u64_be(buf: &[u8], pos: &mut usize) -> Result<u64> {
    if *pos + 8 > buf.len() {
        return Err(Error::InvalidAmf0("amf3 truncated u64".into()));
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

fn read_bytes(buf: &[u8], pos: &mut usize, len: usize) -> Result<Vec<u8>> {
    if *pos + len > buf.len() {
        return Err(Error::InvalidAmf0(format!(
            "amf3 truncated bytes: need {len}, have {}",
            buf.len() - *pos
        )));
    }
    let v = buf[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(v)
}

fn read_utf8_body(buf: &[u8], pos: &mut usize, len: usize) -> Result<String> {
    if *pos + len > buf.len() {
        return Err(Error::InvalidAmf0(format!(
            "amf3 truncated string: need {len}, have {}",
            buf.len() - *pos
        )));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|e| Error::InvalidAmf0(format!("amf3 non-UTF8 string: {e}")))?
        .to_owned();
    *pos += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- U29 primitive -----

    #[test]
    fn u29_roundtrips_each_length() {
        // One byte (0..=0x7F).
        for v in [0u32, 1, 0x7F] {
            let mut b = Vec::new();
            write_u29(&mut b, v);
            assert_eq!(b.len(), 1, "v={v}");
            let mut p = 0;
            assert_eq!(read_u29(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
        // Two bytes (0x80..=0x3FFF).
        for v in [0x80u32, 0x100, 0x3FFF] {
            let mut b = Vec::new();
            write_u29(&mut b, v);
            assert_eq!(b.len(), 2, "v={v}");
            let mut p = 0;
            assert_eq!(read_u29(&b, &mut p).unwrap(), v);
        }
        // Three bytes (0x4000..=0x1F_FFFF).
        for v in [0x4000u32, 0x10_0000, 0x1F_FFFF] {
            let mut b = Vec::new();
            write_u29(&mut b, v);
            assert_eq!(b.len(), 3, "v={v}");
            let mut p = 0;
            assert_eq!(read_u29(&b, &mut p).unwrap(), v);
        }
        // Four bytes (0x20_0000..=0x1FFF_FFFF).
        for v in [0x20_0000u32, 0x1234_5678, 0x1FFF_FFFF] {
            let mut b = Vec::new();
            write_u29(&mut b, v);
            assert_eq!(b.len(), 4, "v={v}");
            let mut p = 0;
            assert_eq!(read_u29(&b, &mut p).unwrap(), v);
        }
    }

    #[test]
    fn u29_spec_examples() {
        // Boundary cases from §1.3.1: each row's binary should be the
        // canonical wire form.
        let mut b = Vec::new();
        write_u29(&mut b, 0x7F);
        assert_eq!(b, vec![0x7F]);

        let mut b = Vec::new();
        write_u29(&mut b, 0x80);
        assert_eq!(b, vec![0x81, 0x00]);

        let mut b = Vec::new();
        write_u29(&mut b, 0x4000);
        assert_eq!(b, vec![0x81, 0x80, 0x00]);

        // 0x20_0000 = bits [22:21] set; 4-byte form fills the high 7
        // bits of byte 0 (zero), then the next 7 bits in byte 1 cover
        // bit 21 → 0x40 (with continuation MSB → 0xC0), then 7 bits in
        // byte 2 (zero) and a final 8-bit byte (zero).
        let mut b = Vec::new();
        write_u29(&mut b, 0x20_0000);
        assert_eq!(b, vec![0x80, 0xC0, 0x80, 0x00]);
        let mut p = 0;
        assert_eq!(read_u29(&b, &mut p).unwrap(), 0x20_0000);
    }

    // ----- Simple values -----

    #[test]
    fn roundtrip_simple_markers() {
        for v in [
            Amf3Value::Undefined,
            Amf3Value::Null,
            Amf3Value::Boolean(false),
            Amf3Value::Boolean(true),
        ] {
            let mut b = Vec::new();
            encode(&mut b, &v);
            let mut p = 0;
            assert_eq!(decode(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
    }

    #[test]
    fn roundtrip_integer_and_double() {
        for n in [-3, -1, 0, 1, 1024, (1 << 28) - 1, -(1 << 28)] {
            let v = Amf3Value::Integer(n);
            let mut b = Vec::new();
            encode(&mut b, &v);
            let mut p = 0;
            assert_eq!(decode(&b, &mut p).unwrap(), v);
        }
        // Out-of-range integers fall back to doubles on encode.
        let v = Amf3Value::Integer(1 << 28);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        let decoded = decode(&b, &mut p).unwrap();
        assert!(matches!(decoded, Amf3Value::Double(d) if d == f64::from(1 << 28)));

        let d = Amf3Value::Double(123.456_789);
        let mut b = Vec::new();
        encode(&mut b, &d);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), d);
    }

    // ----- Strings + reference table -----

    #[test]
    fn roundtrip_string_literal_then_reference() {
        // Literal followed by a reference to the SAME literal — the
        // decoder must consult its string table.
        let mut bytes = Vec::new();
        encode(&mut bytes, &Amf3Value::String("hello".into()));
        // Manually craft a string-reference (U29: idx<<1, low bit = 0)
        // pointing at index 0. Use `M_STRING` + U29 = 0.
        bytes.push(M_STRING);
        write_u29(&mut bytes, 0); // idx 0, ref flag.
        let mut dec = Decoder::new();
        let mut p = 0;
        let a = dec.decode(&bytes, &mut p).unwrap();
        let b = dec.decode(&bytes, &mut p).unwrap();
        assert_eq!(a, Amf3Value::String("hello".into()));
        assert_eq!(b, Amf3Value::String("hello".into()));
        assert_eq!(p, bytes.len());
    }

    #[test]
    fn empty_string_never_in_table() {
        let mut bytes = Vec::new();
        encode(&mut bytes, &Amf3Value::String(String::new()));
        encode(&mut bytes, &Amf3Value::String("after".into()));
        // After parsing, table index 0 should be "after", not "".
        let mut dec = Decoder::new();
        let mut p = 0;
        dec.decode(&bytes, &mut p).unwrap();
        dec.decode(&bytes, &mut p).unwrap();
        // Append a reference to index 0 — should resolve to "after".
        let mut more = vec![M_STRING];
        write_u29(&mut more, 0);
        let mut p2 = 0;
        let resolved = dec.decode(&more, &mut p2).unwrap();
        assert_eq!(resolved, Amf3Value::String("after".into()));
    }

    // ----- Date / ByteArray -----

    #[test]
    fn roundtrip_date() {
        let v = Amf3Value::Date(1_700_000_000_000.0);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_byte_array() {
        let v = Amf3Value::ByteArray(vec![0u8, 1, 2, 0xFE, 0xFF]);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    // ----- Array -----

    #[test]
    fn roundtrip_dense_array() {
        let v = Amf3Value::Array {
            dense: vec![
                Amf3Value::Integer(1),
                Amf3Value::Integer(2),
                Amf3Value::String("c".into()),
            ],
            assoc: Vec::new(),
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_associative_array() {
        let v = Amf3Value::Array {
            dense: vec![Amf3Value::Integer(7), Amf3Value::Integer(8)],
            assoc: vec![
                ("color".into(), Amf3Value::String("red".into())),
                ("count".into(), Amf3Value::Integer(2)),
            ],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    // ----- Object -----

    #[test]
    fn roundtrip_anonymous_object() {
        let v = anon_object(vec![
            ("app".to_string(), Amf3Value::String("live".into())),
            ("flashVer".to_string(), Amf3Value::String("FMLE/3.0".into())),
            ("capabilities".to_string(), Amf3Value::Integer(239)),
        ]);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        let decoded = decode(&b, &mut p).unwrap();
        assert_eq!(decoded, v);
        assert_eq!(decoded.get("app").and_then(Amf3Value::as_str), Some("live"));
    }

    #[test]
    fn roundtrip_dynamic_object() {
        let v = dynamic_object(vec![
            ("name".to_string(), Amf3Value::String("alice".into())),
            ("age".to_string(), Amf3Value::Integer(30)),
        ]);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_typed_object_with_sealed_and_dynamic() {
        let v = Amf3Value::Object {
            class_name: "com.example.Camera".into(),
            dynamic: true,
            sealed: vec![
                ("width".into(), Amf3Value::Integer(1920)),
                ("height".into(), Amf3Value::Integer(1080)),
            ],
            dynamic_members: vec![("extra".into(), Amf3Value::Boolean(true))],
            externalizable_body: None,
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn externalizable_object_refuses_to_decode_without_handler() {
        // Build an externalizable object on the wire with a 1-byte
        // body. Generic decode should refuse rather than guess length.
        let mut bytes = vec![M_OBJECT];
        write_u29(&mut bytes, 0b0111); // U29O-traits-ext
        write_u29_string(&mut bytes, "MyExternalClass");
        bytes.push(0xAA);
        let mut p = 0;
        assert!(matches!(decode(&bytes, &mut p), Err(Error::InvalidAmf0(_))));
    }

    // ----- Vectors -----

    #[test]
    fn roundtrip_vector_int() {
        let v = Amf3Value::VectorInt {
            fixed: false,
            items: vec![-1, 0, 1, i32::MIN, i32::MAX],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_vector_uint() {
        let v = Amf3Value::VectorUInt {
            fixed: true,
            items: vec![0u32, 1, 4_000_000_000],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    #[test]
    fn roundtrip_vector_double() {
        let v = Amf3Value::VectorDouble {
            fixed: false,
            items: vec![0.0, 1.5, -2.5, f64::INFINITY],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        let r = decode(&b, &mut p).unwrap();
        // f64 round-trips exactly bit-for-bit (no NaN here so eq is OK).
        assert_eq!(r, v);
    }

    #[test]
    fn roundtrip_vector_object() {
        let v = Amf3Value::VectorObject {
            fixed: false,
            object_type_name: "*".into(),
            items: vec![
                Amf3Value::Integer(1),
                Amf3Value::String("two".into()),
                Amf3Value::Null,
            ],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    // ----- Dictionary -----

    #[test]
    fn roundtrip_dictionary() {
        let v = Amf3Value::Dictionary {
            weak_keys: false,
            entries: vec![
                (Amf3Value::String("k1".into()), Amf3Value::Integer(1)),
                (Amf3Value::Integer(42), Amf3Value::String("v".into())),
            ],
        };
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
    }

    // ----- XML -----

    #[test]
    fn roundtrip_xml_and_xmldoc() {
        for v in [
            Amf3Value::Xml("<root/>".into()),
            Amf3Value::XmlDocument("<doc/>".into()),
        ] {
            let mut b = Vec::new();
            encode(&mut b, &v);
            let mut p = 0;
            assert_eq!(decode(&b, &mut p).unwrap(), v);
        }
    }

    // ----- Multi-value packet sharing tables -----

    #[test]
    fn decode_all_shares_string_table_across_values() {
        // Two values in one packet — second references the first's
        // string. decode_all must keep the table alive across them.
        let mut bytes = Vec::new();
        encode(&mut bytes, &Amf3Value::String("shared".into()));
        bytes.push(M_STRING);
        write_u29(&mut bytes, 0); // ref to index 0 in string table
        let values = decode_all(&bytes).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], Amf3Value::String("shared".into()));
        assert_eq!(values[1], Amf3Value::String("shared".into()));
    }

    #[test]
    fn decode_all_rejects_dangling_reference() {
        // Refer to a string at index 0 with the string table empty.
        let mut bytes = vec![M_STRING];
        write_u29(&mut bytes, 0); // idx 0, ref flag — but table empty
        assert!(matches!(decode_all(&bytes), Err(Error::InvalidAmf0(_))));
    }

    #[test]
    fn rejects_unknown_marker() {
        let b = [0xFE_u8];
        let mut p = 0;
        assert!(matches!(decode(&b, &mut p), Err(Error::InvalidAmf0(_))));
    }

    #[test]
    fn integer_sign_extension_at_negative_boundary() {
        // -1 should encode as 0x1FFFFFFF (all 29 bits set) and
        // round-trip back to -1.
        let v = Amf3Value::Integer(-1);
        let mut b = Vec::new();
        encode(&mut b, &v);
        let mut p = 0;
        assert_eq!(decode(&b, &mut p).unwrap(), v);
        // The U29 payload should be 4 bytes since the top bit is set.
        // (marker + 4-byte U29).
        assert_eq!(b.len(), 5);
    }

    #[test]
    fn trait_reference_round_trip() {
        // Two consecutive typed-object encodings of the same class
        // share traits: decoder caches traits[0] from value 1, value 2
        // is encoded as a literal-object-with-trait-by-reference.
        let class = "com.foo.Bar";
        // First object: literal traits.
        let mut bytes = Vec::new();
        bytes.push(M_OBJECT);
        // U29O-traits: bit0=1 literal, bit1=1 literal traits, bit2=0
        // (not ext), bit3=0 (not dynamic), sealed_count=1<<4.
        write_u29(&mut bytes, 0b0011 | (1u32 << 4));
        write_u29_string(&mut bytes, class);
        write_u29_string(&mut bytes, "x");
        encode(&mut bytes, &Amf3Value::Integer(1));
        // Second object: trait reference — U29 = (trait_idx << 2) | 0b01.
        // i.e. bit0=1 literal, bit1=0 trait-reference, upper bits =
        // trait index 0; collapses to plain 0b01 for index 0.
        bytes.push(M_OBJECT);
        write_u29(&mut bytes, 0b01);
        encode(&mut bytes, &Amf3Value::Integer(2));

        let values = decode_all(&bytes).unwrap();
        assert_eq!(values.len(), 2);
        if let Amf3Value::Object {
            class_name, sealed, ..
        } = &values[0]
        {
            assert_eq!(class_name, class);
            assert_eq!(sealed, &vec![("x".into(), Amf3Value::Integer(1))]);
        } else {
            panic!("expected object, got {:?}", values[0]);
        }
        if let Amf3Value::Object {
            class_name, sealed, ..
        } = &values[1]
        {
            assert_eq!(class_name, class);
            assert_eq!(sealed, &vec![("x".into(), Amf3Value::Integer(2))]);
        } else {
            panic!("expected object, got {:?}", values[1]);
        }
    }

    #[test]
    fn object_reference_resolves_to_same_value() {
        // Encode a string, then an object containing a literal date,
        // then a reference to the date. The reference should resolve
        // back to the date value.
        let mut bytes = Vec::new();
        // value 1: date
        encode(&mut bytes, &Amf3Value::Date(1234.0));
        // value 2: reference to object index 0
        bytes.push(M_DATE);
        write_u29(&mut bytes, 0); // ref flag = 0, idx = 0

        let values = decode_all(&bytes).unwrap();
        assert_eq!(
            values,
            vec![Amf3Value::Date(1234.0), Amf3Value::Date(1234.0)]
        );
    }

    // ----- RTMP data/command message framing (§4.1) -----

    /// Build the AMF3 wire bytes for a single value, then wrap it the way
    /// an RTMP type-15 / type-17 body does: an AMF0 `avmplus` marker
    /// (0x11) introducing the AMF3-encoded value.
    fn avmplus_wrap(v: &Amf3Value) -> Vec<u8> {
        let mut out = vec![AVMPLUS_OBJECT_MARKER];
        encode(&mut out, v);
        out
    }

    #[test]
    fn data_message_decodes_avmplus_wrapped_sequence() {
        // ["onMetaData", {duration: 12.5, width: 1920}] — each value
        // introduced by the avmplus switch marker, the spec-mandated
        // framing for a NetConnection AMF3 message body.
        let meta = dynamic_object([
            ("duration", Amf3Value::Double(12.5)),
            ("width", Amf3Value::Integer(1920)),
        ]);
        let mut body = avmplus_wrap(&Amf3Value::String("onMetaData".into()));
        body.extend(avmplus_wrap(&meta));

        let values = decode_data_message(&body).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_str(), Some("onMetaData"));
        assert_eq!(
            values[1].get("duration").and_then(Amf3Value::as_f64),
            Some(12.5)
        );
        assert_eq!(
            values[1].get("width").and_then(Amf3Value::as_i32),
            Some(1920)
        );
    }

    #[test]
    fn data_message_decodes_unprefixed_amf3() {
        // Some senders drop straight into AMF3 with no leading 0x11
        // (channel already negotiated to AMF3). decode_data_message must
        // still parse those.
        let mut body = Vec::new();
        encode(&mut body, &Amf3Value::String("onMetaData".into()));
        encode(
            &mut body,
            &dynamic_object([("fps", Amf3Value::Integer(30))]),
        );

        let values = decode_data_message(&body).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_str(), Some("onMetaData"));
        assert_eq!(values[1].get("fps").and_then(Amf3Value::as_i32), Some(30));
    }

    #[test]
    fn data_message_shares_one_reference_context() {
        // A string emitted once then referenced should resolve back to
        // the same text across the whole body.
        let mut body = Vec::new();
        encode(&mut body, &Amf3Value::String("repeat".into()));
        // Second value: string-by-reference (ref flag 0, index 0).
        body.push(M_STRING);
        write_u29(&mut body, 0);

        let values = decode_data_message(&body).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_str(), Some("repeat"));
        assert_eq!(values[1].as_str(), Some("repeat"));
    }

    #[test]
    fn data_message_dangling_avmplus_marker_errors() {
        assert!(decode_data_message(&[AVMPLUS_OBJECT_MARKER]).is_err());
    }

    // ----- AMF3 -> AMF0 bridge -----

    #[test]
    fn bridge_scalars() {
        use crate::amf::Amf0Value as A0;
        assert_eq!(Amf3Value::Null.to_amf0(), A0::Null);
        assert_eq!(Amf3Value::Undefined.to_amf0(), A0::Undefined);
        assert_eq!(Amf3Value::Boolean(true).to_amf0(), A0::Boolean(true));
        // Integer + Double both collapse to AMF0 Number.
        assert_eq!(Amf3Value::Integer(7).to_amf0(), A0::Number(7.0));
        assert_eq!(Amf3Value::Double(2.5).to_amf0(), A0::Number(2.5));
        assert_eq!(
            Amf3Value::String("x".into()).to_amf0(),
            A0::String("x".into())
        );
        // Date drops the (absent) timezone into a 0 slot.
        assert_eq!(
            Amf3Value::Date(100.0).to_amf0(),
            A0::Date {
                millis: 100.0,
                timezone: 0
            }
        );
    }

    #[test]
    fn bridge_object_merges_sealed_and_dynamic_in_order() {
        use crate::amf::Amf0Value as A0;
        let obj = Amf3Value::Object {
            class_name: "Some.Class".into(),
            dynamic: true,
            sealed: vec![("a".into(), Amf3Value::Integer(1))],
            dynamic_members: vec![("b".into(), Amf3Value::String("two".into()))],
            externalizable_body: None,
        };
        // Class name is dropped; sealed then dynamic members, in order.
        assert_eq!(
            obj.to_amf0(),
            A0::Object(vec![
                ("a".into(), A0::Number(1.0)),
                ("b".into(), A0::String("two".into())),
            ])
        );
    }

    #[test]
    fn bridge_array_to_ecma_with_ordinal_keys() {
        use crate::amf::Amf0Value as A0;
        let arr = Amf3Value::Array {
            dense: vec![Amf3Value::Integer(10), Amf3Value::Integer(20)],
            assoc: vec![("name".into(), Amf3Value::String("v".into()))],
        };
        assert_eq!(
            arr.to_amf0(),
            A0::EcmaArray(vec![
                ("0".into(), A0::Number(10.0)),
                ("1".into(), A0::Number(20.0)),
                ("name".into(), A0::String("v".into())),
            ])
        );
    }

    #[test]
    fn bridge_vectors_and_bytearray_to_strict_array() {
        use crate::amf::Amf0Value as A0;
        let vi = Amf3Value::VectorInt {
            fixed: true,
            items: vec![-1, 2],
        };
        assert_eq!(
            vi.to_amf0(),
            A0::StrictArray(vec![A0::Number(-1.0), A0::Number(2.0)])
        );
        let ba = Amf3Value::ByteArray(vec![0, 255]);
        assert_eq!(
            ba.to_amf0(),
            A0::StrictArray(vec![A0::Number(0.0), A0::Number(255.0)])
        );
    }

    #[test]
    fn bridge_full_onmetadata_roundtrips_into_amf0_object() {
        use crate::amf::Amf0Value as A0;
        // Decode a realistic AMF3 onMetaData body and verify the bridged
        // AMF0 object exposes the fields a metadata consumer reads.
        let meta = dynamic_object([
            ("width", Amf3Value::Integer(1280)),
            ("height", Amf3Value::Integer(720)),
            ("framerate", Amf3Value::Double(29.97)),
            ("videocodecid", Amf3Value::String("avc1".into())),
        ]);
        let mut body = avmplus_wrap(&Amf3Value::String("onMetaData".into()));
        body.extend(avmplus_wrap(&meta));

        let bridged: Vec<A0> = decode_data_message(&body)
            .unwrap()
            .iter()
            .map(Amf3Value::to_amf0)
            .collect();
        let obj = bridged.last().unwrap();
        assert_eq!(obj.get("width").and_then(A0::as_f64), Some(1280.0));
        assert_eq!(obj.get("height").and_then(A0::as_f64), Some(720.0));
        assert_eq!(obj.get("framerate").and_then(A0::as_f64), Some(29.97));
        assert_eq!(obj.get("videocodecid").and_then(A0::as_str), Some("avc1"));
    }
}

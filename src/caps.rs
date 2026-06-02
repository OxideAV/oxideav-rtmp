//! Enhanced RTMP NetConnection `connect` capability negotiation.
//!
//! When a client opens a NetConnection it sends the `connect` command
//! whose Command Object carries a small bag of name/value pairs that
//! declare its protocol level. Enhanced RTMP v1 (Veovera, 2023) added
//! the `fourCcList` strict-array of supported FourCC codecs. Enhanced
//! RTMP v2 (Veovera, 2026, `enhanced-rtmp-v2.pdf` §"Enhancing
//! NetConnection connect Command") added two further entries:
//!
//! * `videoFourCcInfoMap` / `audioFourCcInfoMap` — per-codec object
//!   maps whose values are bitmask numbers built from `FourCcInfoMask`
//!   (`CanDecode` / `CanEncode` / `CanForward`). A FourCC key of `"*"`
//!   acts as a catch-all for any codec.
//! * `capsEx` — a single u32 bitfield built from `CapsExMask` declaring
//!   extended capabilities: `Reconnect`, `Multitrack`, `ModEx`,
//!   `TimestampNanoOffset`.
//!
//! Servers echo their own capabilities back in the `_result` reply's
//! properties object using the same names so both sides converge on a
//! common feature subset before any media flows.
//!
//! [`ConnectCapabilities`] is the strongly-typed representation of all
//! four properties plus the legacy `objectEncoding` u8 (0 = AMF0, 3 =
//! AMF0+AMF3 switch via `avmplus-object-marker`). It encodes/decodes
//! against an [`Amf0Value`] graph via [`ConnectCapabilities::encode_into`]
//! / [`ConnectCapabilities::from_amf0`] without disturbing the surrounding
//! command-object key order — additions append after the legacy
//! `audioCodecs` / `videoCodecs` / `videoFunction` block, so a pre-2023
//! receiver still parses everything it understands.

use crate::amf::Amf0Value;

// ---------------------------------------------------------------------------
// FourCcInfoMask — per-codec capability bits (v2 §"Enhancing connect")
// ---------------------------------------------------------------------------

/// `FourCcInfoMask.CanDecode` — the endpoint can decode this codec.
pub const FOURCC_INFO_CAN_DECODE: u32 = 0x01;
/// `FourCcInfoMask.CanEncode` — the endpoint can encode this codec.
pub const FOURCC_INFO_CAN_ENCODE: u32 = 0x02;
/// `FourCcInfoMask.CanForward` — the endpoint can forward the codec
/// without decoding (relay / recorder / forwarding ingest).
pub const FOURCC_INFO_CAN_FORWARD: u32 = 0x04;

// ---------------------------------------------------------------------------
// CapsExMask — extended-capability bitfield (v2 §"Enhancing connect")
// ---------------------------------------------------------------------------

/// `CapsExMask.Reconnect` — the endpoint honours the
/// `NetConnection.Connect.ReconnectRequest` onStatus event.
pub const CAPS_EX_RECONNECT: u32 = 0x01;
/// `CapsExMask.Multitrack` — the endpoint understands the v2 Multitrack
/// audio + video PacketTypes (per-track FourCC + size-prefixed track
/// chunks).
pub const CAPS_EX_MULTITRACK: u32 = 0x02;
/// `CapsExMask.ModEx` — the endpoint can parse the v2 ModEx
/// packet-type prelude (size-prefixed extension chain ahead of the
/// real packet type).
pub const CAPS_EX_MOD_EX: u32 = 0x04;
/// `CapsExMask.TimestampNanoOffset` — the endpoint applies the
/// ModEx `TimestampOffsetNano = 0` sub-millisecond presentation offset
/// to its decode pipeline.
pub const CAPS_EX_TIMESTAMP_NANO_OFFSET: u32 = 0x08;

/// `objectEncoding` value 0 — AMF0-only.
pub const OBJECT_ENCODING_AMF0: u8 = 0;
/// `objectEncoding` value 3 — AMF0 with `avmplus-object-marker`
/// switching to AMF3 per the AMF0 spec.
pub const OBJECT_ENCODING_AMF3: u8 = 3;

/// FourCC wildcard string — a key of `"*"` in a FourCcInfoMap means
/// "applies to every codec in the relevant audio/video bucket".
pub const FOURCC_WILDCARD: &str = "*";

// ---------------------------------------------------------------------------
// FourCcInfoMap — `videoFourCcInfoMap` / `audioFourCcInfoMap`
// ---------------------------------------------------------------------------

/// `videoFourCcInfoMap` / `audioFourCcInfoMap` payload — an ordered
/// list of `(fourCC-or-wildcard, mask)` pairs.
///
/// The spec stores this as an AMF0 Object whose property names are
/// FourCC ASCII (e.g. `"hvc1"`, `"Opus"`) or the catch-all `"*"`, and
/// whose values are numbers built from [`FOURCC_INFO_CAN_DECODE`] etc.
/// We keep the entries in insertion order — most peers walk the
/// properties top-down looking for a wildcard first, then per-codec
/// overrides, so order is informally load-bearing even though the spec
/// doesn't mandate it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FourCcInfoMap {
    entries: Vec<(String, u32)>,
}

impl FourCcInfoMap {
    /// Empty map — declares no per-codec capabilities.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an entry with the given FourCC string and mask. If `key`
    /// already exists the new mask replaces the existing value while
    /// preserving insertion position.
    pub fn insert<S: Into<String>>(&mut self, key: S, mask: u32) -> &mut Self {
        let key = key.into();
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == &key) {
            slot.1 = mask;
        } else {
            self.entries.push((key, mask));
        }
        self
    }

    /// Convenience: insert a `[u8; 4]` FourCC verbatim. The bytes are
    /// taken to be ASCII (e.g. `*b"hvc1"`); a non-ASCII byte falls back
    /// to the lossy UTF-8 conversion, which keeps round-tripping clean
    /// against a forwarding peer.
    pub fn insert_fourcc(&mut self, fourcc: [u8; 4], mask: u32) -> &mut Self {
        let s = String::from_utf8_lossy(&fourcc).into_owned();
        self.insert(s, mask)
    }

    /// Look up a mask for the given key. Returns `None` if the key
    /// isn't present — callers that want wildcard fallback should also
    /// check [`Self::wildcard`].
    pub fn get(&self, key: &str) -> Option<u32> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, m)| *m)
    }

    /// Mask carried by the catch-all `"*"` key, if any.
    pub fn wildcard(&self) -> Option<u32> {
        self.get(FOURCC_WILDCARD)
    }

    /// Effective mask for `key`, applying the v2 spec rule that a
    /// wildcard entry overrides per-codec entries for any flag it sets.
    pub fn effective_mask(&self, key: &str) -> u32 {
        let direct = self.get(key).unwrap_or(0);
        direct | self.wildcard().unwrap_or(0)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if the map carries no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate `(key, mask)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.entries.iter().map(|(k, m)| (k.as_str(), *m))
    }

    /// Encode as the AMF0 Object the spec expects.
    pub fn to_amf0(&self) -> Amf0Value {
        Amf0Value::Object(
            self.entries
                .iter()
                .map(|(k, m)| (k.clone(), Amf0Value::Number(*m as f64)))
                .collect(),
        )
    }

    /// Lift from an AMF0 Object (or ECMA-array, which some peers emit).
    /// Non-numeric / non-finite / negative-valued entries are silently
    /// skipped — the spec demands numeric mask bits and a forged
    /// `String` or `Date` payload here would otherwise pull the rest of
    /// the connect command into a hard error. Out-of-u32 numbers
    /// saturate to `u32::MAX`.
    pub fn from_amf0(v: &Amf0Value) -> Self {
        let pairs = match v {
            Amf0Value::Object(p) | Amf0Value::EcmaArray(p) => p,
            _ => return Self::new(),
        };
        let mut out = Self::new();
        for (k, val) in pairs {
            if let Amf0Value::Number(n) = val {
                if n.is_finite() && *n >= 0.0 {
                    let m = if *n >= u32::MAX as f64 {
                        u32::MAX
                    } else {
                        *n as u32
                    };
                    out.insert(k.clone(), m);
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// ConnectCapabilities — the full v1+v2 capability block.
// ---------------------------------------------------------------------------

/// Capability block exchanged in the NetConnection `connect` command.
///
/// Owns all four spec entries (`fourCcList`, `videoFourCcInfoMap`,
/// `audioFourCcInfoMap`, `capsEx`) plus the long-standing
/// `objectEncoding` byte. Encoded into the existing Command Object by
/// [`Self::encode_into`] without touching the surrounding key order, and
/// parsed back with [`Self::from_amf0`] from either the client's
/// Command Object or the server's `_result` properties object.
///
/// A default-constructed instance is empty — `is_empty()` is true and
/// `encode_into` writes nothing, so a caller composing a legacy AVC /
/// AAC-only `connect` command keeps the pre-2023 byte layout exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectCapabilities {
    /// `fourCcList` — Enhanced RTMP v1 strict-array of supported FourCC
    /// strings (e.g. `"av01"`, `"hvc1"`). The v2 spec deprecates this on
    /// the client side in favour of `audio/videoFourCcInfoMap`, but
    /// servers are encouraged to keep supporting both for older clients.
    pub fourcc_list: Vec<String>,
    /// `videoFourCcInfoMap` — v2 per-codec capability bits for video
    /// codecs.
    pub video_fourcc_info_map: FourCcInfoMap,
    /// `audioFourCcInfoMap` — v2 per-codec capability bits for audio
    /// codecs.
    pub audio_fourcc_info_map: FourCcInfoMap,
    /// `capsEx` — v2 bag of extended capability bits.
    pub caps_ex: u32,
    /// `objectEncoding` — 0 for AMF0-only, 3 for AMF0+AMF3.
    pub object_encoding: Option<u8>,
}

impl ConnectCapabilities {
    /// Empty capability block — encodes to nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// True when every field is empty / default.
    pub fn is_empty(&self) -> bool {
        self.fourcc_list.is_empty()
            && self.video_fourcc_info_map.is_empty()
            && self.audio_fourcc_info_map.is_empty()
            && self.caps_ex == 0
            && self.object_encoding.is_none()
    }

    /// Test for a specific `CapsExMask` flag.
    pub fn supports_caps_ex(&self, mask: u32) -> bool {
        self.caps_ex & mask != 0
    }

    /// True when `fourcc_list` includes either the wildcard `"*"` or
    /// the literal FourCC `key`.
    pub fn has_fourcc(&self, key: &str) -> bool {
        self.fourcc_list
            .iter()
            .any(|s| s == key || s == FOURCC_WILDCARD)
    }

    /// Append our capability properties to a command-object pair list.
    ///
    /// Each property is only appended when the corresponding field is
    /// non-default, so encoding an empty block adds zero bytes. Caller
    /// keeps any other Command Object properties they want around the
    /// call site — `pairs` is mutated in place.
    pub fn encode_into(&self, pairs: &mut Vec<(String, Amf0Value)>) {
        if let Some(enc) = self.object_encoding {
            pairs.push(("objectEncoding".into(), Amf0Value::Number(enc as f64)));
        }
        if !self.fourcc_list.is_empty() {
            let arr = Amf0Value::StrictArray(
                self.fourcc_list
                    .iter()
                    .map(|s| Amf0Value::String(s.clone()))
                    .collect(),
            );
            pairs.push(("fourCcList".into(), arr));
        }
        if !self.video_fourcc_info_map.is_empty() {
            pairs.push((
                "videoFourCcInfoMap".into(),
                self.video_fourcc_info_map.to_amf0(),
            ));
        }
        if !self.audio_fourcc_info_map.is_empty() {
            pairs.push((
                "audioFourCcInfoMap".into(),
                self.audio_fourcc_info_map.to_amf0(),
            ));
        }
        if self.caps_ex != 0 {
            pairs.push(("capsEx".into(), Amf0Value::Number(self.caps_ex as f64)));
        }
    }

    /// Parse any subset of capability properties out of an Object /
    /// ECMA-array. Missing properties stay at their default; malformed
    /// values are silently ignored (the spec's "fail gracefully" rule —
    /// a forged `capsEx = "abc"` from a stale peer must not abort the
    /// connect handshake).
    pub fn from_amf0(v: &Amf0Value) -> Self {
        let mut out = Self::new();
        let pairs: &[(String, Amf0Value)] = match v {
            Amf0Value::Object(p) | Amf0Value::EcmaArray(p) => p.as_slice(),
            _ => return out,
        };
        for (k, val) in pairs {
            match k.as_str() {
                "objectEncoding" => {
                    if let Amf0Value::Number(n) = val {
                        if n.is_finite() && *n >= 0.0 && *n <= u8::MAX as f64 {
                            out.object_encoding = Some(*n as u8);
                        }
                    }
                }
                "fourCcList" => {
                    if let Amf0Value::StrictArray(items) = val {
                        out.fourcc_list = items
                            .iter()
                            .filter_map(|it| match it {
                                Amf0Value::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .collect();
                    }
                }
                "videoFourCcInfoMap" => {
                    out.video_fourcc_info_map = FourCcInfoMap::from_amf0(val);
                }
                "audioFourCcInfoMap" => {
                    out.audio_fourcc_info_map = FourCcInfoMap::from_amf0(val);
                }
                "capsEx" => {
                    if let Amf0Value::Number(n) = val {
                        if n.is_finite() && *n >= 0.0 {
                            out.caps_ex = if *n >= u32::MAX as f64 {
                                u32::MAX
                            } else {
                                *n as u32
                            };
                        }
                    }
                }
                _ => { /* not a capability property — ignored */ }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amf;
    use crate::flv::{
        FOURCC_AAC, FOURCC_AC3, FOURCC_AV1, FOURCC_AVC, FOURCC_EAC3, FOURCC_FLAC, FOURCC_HEVC,
        FOURCC_MP3, FOURCC_OPUS, FOURCC_VP8, FOURCC_VP9, FOURCC_VVC,
    };

    fn fourcc_str(b: [u8; 4]) -> String {
        std::str::from_utf8(&b).unwrap().to_owned()
    }

    /// `FourCcInfoMask` constants match the spec table verbatim.
    #[test]
    fn fourcc_info_mask_constants_match_spec() {
        // From enhanced-rtmp-v2.pdf §"Enhancing NetConnection connect Command":
        //   enum FourCcInfoMask { CanDecode = 0x01, CanEncode = 0x02, CanForward = 0x04 }
        assert_eq!(FOURCC_INFO_CAN_DECODE, 0x01);
        assert_eq!(FOURCC_INFO_CAN_ENCODE, 0x02);
        assert_eq!(FOURCC_INFO_CAN_FORWARD, 0x04);
    }

    /// `CapsExMask` constants match the spec table verbatim.
    #[test]
    fn caps_ex_mask_constants_match_spec() {
        // enum CapsExMask {
        //   Reconnect = 0x01, Multitrack = 0x02, ModEx = 0x04, TimestampNanoOffset = 0x08
        // }
        assert_eq!(CAPS_EX_RECONNECT, 0x01);
        assert_eq!(CAPS_EX_MULTITRACK, 0x02);
        assert_eq!(CAPS_EX_MOD_EX, 0x04);
        assert_eq!(CAPS_EX_TIMESTAMP_NANO_OFFSET, 0x08);
    }

    /// FourCC wildcard is the single-byte `"*"`.
    #[test]
    fn fourcc_wildcard_is_star() {
        assert_eq!(FOURCC_WILDCARD, "*");
    }

    /// `FourCcInfoMap::insert` preserves insertion order and replaces
    /// duplicate keys without moving them.
    #[test]
    fn fourcc_info_map_insert_preserves_order() {
        let mut m = FourCcInfoMap::new();
        m.insert("hvc1", FOURCC_INFO_CAN_DECODE);
        m.insert_fourcc(FOURCC_AV1, FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
        m.insert("hvc1", FOURCC_INFO_CAN_FORWARD); // replace
        let keys: Vec<_> = m.iter().map(|(k, _)| k.to_owned()).collect();
        assert_eq!(keys, vec!["hvc1", "av01"]);
        assert_eq!(m.get("hvc1"), Some(FOURCC_INFO_CAN_FORWARD));
    }

    /// `effective_mask` ORs in the wildcard entry per spec.
    #[test]
    fn fourcc_info_map_wildcard_overrides_per_codec() {
        let mut m = FourCcInfoMap::new();
        m.insert("*", FOURCC_INFO_CAN_FORWARD);
        m.insert("vp09", FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
        // Per-codec entry is preserved, wildcard adds CanForward on top.
        assert_eq!(
            m.effective_mask("vp09"),
            FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE | FOURCC_INFO_CAN_FORWARD,
        );
        // Unknown codec inherits wildcard alone.
        assert_eq!(m.effective_mask("xxxx"), FOURCC_INFO_CAN_FORWARD);
    }

    /// `FourCcInfoMap` round-trips through the AMF0 Object shape.
    #[test]
    fn fourcc_info_map_amf0_roundtrip() {
        let mut m = FourCcInfoMap::new();
        m.insert("*", FOURCC_INFO_CAN_FORWARD);
        m.insert_fourcc(FOURCC_VP9, FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
        let v = m.to_amf0();
        let back = FourCcInfoMap::from_amf0(&v);
        assert_eq!(back, m);
    }

    /// Malformed mask entries are dropped, not propagated.
    #[test]
    fn fourcc_info_map_skips_non_number_values() {
        let v = Amf0Value::Object(vec![
            ("hvc1".into(), Amf0Value::Number(7.0)),
            ("Opus".into(), Amf0Value::String("nope".into())),
            ("avc1".into(), Amf0Value::Number(f64::NAN)),
            ("vp08".into(), Amf0Value::Number(-1.0)),
        ]);
        let m = FourCcInfoMap::from_amf0(&v);
        let keys: Vec<_> = m.iter().map(|(k, _)| k.to_owned()).collect();
        assert_eq!(keys, vec!["hvc1"]);
        assert_eq!(m.get("hvc1"), Some(7));
    }

    /// Out-of-u32 numeric mask saturates to `u32::MAX`.
    #[test]
    fn fourcc_info_map_saturates_oversize_mask() {
        let v = Amf0Value::Object(vec![("hvc1".into(), Amf0Value::Number(1e20))]);
        let m = FourCcInfoMap::from_amf0(&v);
        assert_eq!(m.get("hvc1"), Some(u32::MAX));
    }

    /// Default capability block is empty and writes no bytes.
    #[test]
    fn default_capabilities_emit_nothing() {
        let caps = ConnectCapabilities::default();
        assert!(caps.is_empty());
        let mut pairs = Vec::new();
        caps.encode_into(&mut pairs);
        assert!(pairs.is_empty());
    }

    /// Encoded properties land in the documented v1+v2 order:
    /// `objectEncoding`, `fourCcList`, `videoFourCcInfoMap`,
    /// `audioFourCcInfoMap`, `capsEx`.
    #[test]
    fn encode_into_uses_documented_order() {
        let mut video = FourCcInfoMap::new();
        video.insert("*", FOURCC_INFO_CAN_FORWARD);
        let mut audio = FourCcInfoMap::new();
        audio.insert_fourcc(FOURCC_OPUS, FOURCC_INFO_CAN_DECODE);
        let caps = ConnectCapabilities {
            object_encoding: Some(OBJECT_ENCODING_AMF3),
            fourcc_list: vec![fourcc_str(FOURCC_HEVC), fourcc_str(FOURCC_AV1)],
            video_fourcc_info_map: video,
            audio_fourcc_info_map: audio,
            caps_ex: CAPS_EX_RECONNECT | CAPS_EX_MULTITRACK,
        };

        let mut pairs = Vec::new();
        caps.encode_into(&mut pairs);
        let names: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "objectEncoding",
                "fourCcList",
                "videoFourCcInfoMap",
                "audioFourCcInfoMap",
                "capsEx",
            ],
        );
    }

    /// Round-trip a fully-populated capability block through encode →
    /// AMF0 wire → decode and assert every field comes back equal.
    #[test]
    fn full_capabilities_amf0_roundtrip() {
        let mut video = FourCcInfoMap::new();
        video.insert("*", FOURCC_INFO_CAN_FORWARD);
        video.insert_fourcc(FOURCC_HEVC, FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
        video.insert_fourcc(FOURCC_VP9, FOURCC_INFO_CAN_DECODE);
        let mut audio = FourCcInfoMap::new();
        audio.insert("*", FOURCC_INFO_CAN_FORWARD);
        audio.insert_fourcc(FOURCC_OPUS, FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
        let caps = ConnectCapabilities {
            object_encoding: Some(OBJECT_ENCODING_AMF0),
            fourcc_list: vec![
                fourcc_str(FOURCC_AV1),
                fourcc_str(FOURCC_VP9),
                fourcc_str(FOURCC_VP8),
                fourcc_str(FOURCC_HEVC),
                fourcc_str(FOURCC_AVC),
                fourcc_str(FOURCC_VVC),
                fourcc_str(FOURCC_AC3),
                fourcc_str(FOURCC_EAC3),
                fourcc_str(FOURCC_OPUS),
                fourcc_str(FOURCC_MP3),
                fourcc_str(FOURCC_FLAC),
                fourcc_str(FOURCC_AAC),
            ],
            video_fourcc_info_map: video,
            audio_fourcc_info_map: audio,
            caps_ex: CAPS_EX_RECONNECT
                | CAPS_EX_MULTITRACK
                | CAPS_EX_MOD_EX
                | CAPS_EX_TIMESTAMP_NANO_OFFSET,
        };

        let mut pairs = vec![("app".into(), Amf0Value::String("live".into()))];
        caps.encode_into(&mut pairs);
        let obj = Amf0Value::Object(pairs);
        // Encode-decode the wire bytes so the round-trip walks the same
        // AMF0 path used by the live `connect` handshake.
        let mut buf = Vec::new();
        amf::encode(&mut buf, &obj);
        let mut pos = 0;
        let decoded = amf::decode(&buf, &mut pos).unwrap();
        let back = ConnectCapabilities::from_amf0(&decoded);
        assert_eq!(back, caps);
    }

    /// `has_fourcc` recognises both the exact entry and a `"*"`
    /// wildcard.
    #[test]
    fn fourcc_list_wildcard_and_explicit() {
        let caps = ConnectCapabilities {
            fourcc_list: vec!["*".into()],
            ..Default::default()
        };
        assert!(caps.has_fourcc("av01"));
        assert!(caps.has_fourcc("xxxx"));

        let caps = ConnectCapabilities {
            fourcc_list: vec![fourcc_str(FOURCC_HEVC)],
            ..Default::default()
        };
        assert!(caps.has_fourcc("hvc1"));
        assert!(!caps.has_fourcc("av01"));
    }

    /// `supports_caps_ex` is a bit-wise AND.
    #[test]
    fn caps_ex_bit_test() {
        let caps = ConnectCapabilities {
            caps_ex: CAPS_EX_RECONNECT | CAPS_EX_MOD_EX,
            ..Default::default()
        };
        assert!(caps.supports_caps_ex(CAPS_EX_RECONNECT));
        assert!(caps.supports_caps_ex(CAPS_EX_MOD_EX));
        assert!(!caps.supports_caps_ex(CAPS_EX_MULTITRACK));
        // Combined-mask test: requires BOTH bits set.
        assert!(caps.supports_caps_ex(CAPS_EX_RECONNECT | CAPS_EX_MOD_EX));
    }

    /// A forged Object whose `capsEx` is a String parses cleanly with
    /// the rest of the block intact.
    #[test]
    fn malformed_caps_ex_falls_back_to_default() {
        let obj = Amf0Value::Object(vec![
            ("capsEx".into(), Amf0Value::String("oops".into())),
            (
                "fourCcList".into(),
                Amf0Value::StrictArray(vec![Amf0Value::String("av01".into())]),
            ),
        ]);
        let caps = ConnectCapabilities::from_amf0(&obj);
        assert_eq!(caps.caps_ex, 0);
        assert_eq!(caps.fourcc_list, vec!["av01"]);
    }

    /// Non-object inputs return an empty block (a forwarder may pass a
    /// pre-resolved capability struct in via a top-level Number for
    /// instance — that's silently ignored).
    #[test]
    fn from_amf0_non_object_returns_empty() {
        let caps = ConnectCapabilities::from_amf0(&Amf0Value::Number(7.0));
        assert!(caps.is_empty());
        let caps = ConnectCapabilities::from_amf0(&Amf0Value::Null);
        assert!(caps.is_empty());
    }

    /// `objectEncoding` must round-trip the documented 0 / 3 values.
    #[test]
    fn object_encoding_values() {
        for &enc in &[OBJECT_ENCODING_AMF0, OBJECT_ENCODING_AMF3] {
            let caps = ConnectCapabilities {
                object_encoding: Some(enc),
                ..Default::default()
            };
            let mut pairs = Vec::new();
            caps.encode_into(&mut pairs);
            let back = ConnectCapabilities::from_amf0(&Amf0Value::Object(pairs));
            assert_eq!(back.object_encoding, Some(enc));
        }
    }

    /// An ECMA-array carrying the capability properties parses the same
    /// way an Object does. Some commodity peers use ECMA-array for the
    /// `_result` properties slot.
    #[test]
    fn ecma_array_parses_as_capability_block() {
        let arr = Amf0Value::EcmaArray(vec![
            ("capsEx".into(), Amf0Value::Number(0x05 as f64)),
            (
                "videoFourCcInfoMap".into(),
                Amf0Value::Object(vec![("hvc1".into(), Amf0Value::Number(3.0))]),
            ),
        ]);
        let caps = ConnectCapabilities::from_amf0(&arr);
        assert_eq!(caps.caps_ex, 0x05);
        assert_eq!(caps.video_fourcc_info_map.get("hvc1"), Some(3));
    }
}

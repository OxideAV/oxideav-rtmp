//! FLV Encryption metadata — Annex F of the Adobe Flash Video File
//! Format Specification (`docs/container/flv/flv_v10_1.pdf`).
//!
//! When a `FLVTAG` carries `Filter = 1` (§E.4.1), the tag is encrypted
//! per Annex F. The tag body is then *not* a plain audio / video /
//! script payload — it begins with the in-clear encryption metadata
//! the player needs to drive the decryptor, followed by the
//! ciphertext:
//!
//! ```text
//!   FLVTAG body (Filter = 1)
//!   ├─ EncryptionTagHeader   (§F.3.1) — NumFilters, FilterName, Length
//!   ├─ FilterParams          (§F.3.2) — IV for AES-CBC (full or SE)
//!   └─ EncryptedBody         (§F.3.3) — ciphertext + RFC-2630 padding
//! ```
//!
//! This module parses (and re-serialises) the §F.3.1 + §F.3.2 metadata
//! so a caller can route the still-ciphered body to a decryption stage
//! holding the content key, or skip the tag. It does **not** perform
//! the AES-CBC decryption itself: the decryption key is retrieved from
//! a DRM server via the §F.2.5 Key Information protocol whose details
//! are explicitly *outside the scope of this specification*, so there
//! is nothing in the staged docs that would let us decrypt — only the
//! envelope is parseable from the bytes on the wire.
//!
//! The §F.2 `|AdditionalHeader` / Encryption Header ScriptData object
//! (carried as an ordinary script-data tag, not in the per-tag body)
//! is left to the caller's AMF0 decode of the named tag; this module is
//! concerned with the per-tag envelope only.

use crate::error::{Error, Result};

/// `NumFilters` (§F.3.1) — the spec mandates exactly one filter per
/// encrypted packet.
pub const NUM_FILTERS: u8 = 1;

/// `FilterName` (§F.3.1) for whole-packet encryption
/// (`EncryptionHeader.Version == 1`).
pub const FILTER_NAME_ENCRYPTION: &str = "Encryption";

/// `FilterName` (§F.3.1) for Selective Encryption
/// (`EncryptionHeader.Version == 2`). "SE" stands for Selective
/// Encryption.
pub const FILTER_NAME_SELECTIVE: &str = "SE";

/// AES-CBC initialisation-vector length in bytes (§F.3.2). Both the
/// `EncryptionFilterParams` and the (encrypted) `SelectiveEncryption`
/// branch carry a 16-byte IV.
pub const IV_LEN: usize = 16;

/// `FilterParams` (§F.3.2): the per-packet parameters selected by the
/// `FilterName` in the [`EncryptionTagHeader`].
///
/// * [`FilterParams::Encryption`] — `FilterName = "Encryption"`. Every
///   packet carrying this filter is encrypted; the only parameter is
///   the 16-byte AES-CBC `IV`.
/// * [`FilterParams::Selective`] — `FilterName = "SE"`. A 1-bit
///   `EncryptedAU` flag (plus 7 reserved bits that shall be 0)
///   indicates whether *this* packet is encrypted; the IV is present
///   only when it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterParams {
    /// §F.3.2 `EncryptionFilterParams` — whole-packet encryption. The
    /// packet body is always ciphertext; `iv` is the 16-byte AES-CBC
    /// initialisation vector.
    Encryption { iv: [u8; IV_LEN] },
    /// §F.3.2 `SelectiveEncryptionFilterParams` — selective
    /// encryption. `iv` is `Some` iff the `EncryptedAU` bit was set
    /// (the packet is encrypted); `None` means an in-clear packet that
    /// still carries the filter envelope.
    Selective { iv: Option<[u8; IV_LEN]> },
}

impl FilterParams {
    /// `true` when this packet's body is ciphertext: always for
    /// whole-packet [`FilterParams::Encryption`], and for
    /// [`FilterParams::Selective`] only when the `EncryptedAU` bit was
    /// set.
    pub fn is_encrypted(&self) -> bool {
        match self {
            FilterParams::Encryption { .. } => true,
            FilterParams::Selective { iv } => iv.is_some(),
        }
    }

    /// The 16-byte AES-CBC IV for this packet, if the packet is
    /// encrypted.
    pub fn iv(&self) -> Option<&[u8; IV_LEN]> {
        match self {
            FilterParams::Encryption { iv } => Some(iv),
            FilterParams::Selective { iv } => iv.as_ref(),
        }
    }

    /// Serialised length of the `FilterParams` body in bytes — the
    /// value carried in the `EncryptionTagHeader.Length` UI24 field.
    fn encoded_len(&self) -> usize {
        match self {
            FilterParams::Encryption { .. } => IV_LEN,
            // 1 byte for the EncryptedAU/Reserved bitfield, then the IV
            // iff encrypted.
            FilterParams::Selective { iv } => 1 + if iv.is_some() { IV_LEN } else { 0 },
        }
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            FilterParams::Encryption { iv } => out.extend_from_slice(iv),
            FilterParams::Selective { iv } => {
                // UB[1] EncryptedAU (MSB) + UB[7] Reserved (0).
                out.push(if iv.is_some() { 0x80 } else { 0x00 });
                if let Some(iv) = iv {
                    out.extend_from_slice(iv);
                }
            }
        }
    }
}

/// §F.3.1 `EncryptionTagHeader` + the §F.3.2 `FilterParams` it selects,
/// followed by the still-ciphered (or, for an unencrypted SE packet,
/// plaintext) body.
///
/// Produced by [`parse_encrypted_body`] from the body bytes of a tag
/// whose `Filter` bit (§E.4.1) is set; re-serialised by
/// [`EncryptedTag::encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedTag {
    /// `FilterName` (§F.3.1). Either [`FILTER_NAME_ENCRYPTION`] or
    /// [`FILTER_NAME_SELECTIVE`]; preserved verbatim so an unknown
    /// future filter name still round-trips even though its params are
    /// refused on parse.
    pub filter_name: String,
    /// §F.3.2 parameters selected by `filter_name`.
    pub params: FilterParams,
    /// The §F.3.3 `EncryptedBody` (ciphertext + RFC-2630 padding) for
    /// an encrypted packet, or the plaintext body for an unencrypted
    /// Selective-Encryption packet. Preserved verbatim — decryption is
    /// out of scope (§F.2.5 key retrieval is DRM-server-defined).
    pub body: Vec<u8>,
}

impl EncryptedTag {
    /// `true` iff [`EncryptedTag::body`] is ciphertext (see
    /// [`FilterParams::is_encrypted`]).
    pub fn is_encrypted(&self) -> bool {
        self.params.is_encrypted()
    }

    /// The 16-byte AES-CBC IV, if the body is encrypted.
    pub fn iv(&self) -> Option<&[u8; IV_LEN]> {
        self.params.iv()
    }

    /// Re-serialise to the on-wire body layout (`EncryptionTagHeader` +
    /// `FilterParams` + body). The inverse of [`parse_encrypted_body`]:
    /// `parse_encrypted_body(&tag.encode()) == Ok(tag)` for any tag
    /// this crate produced.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.filter_name.len() + 1 + 3 + self.body.len() + 17);
        // EncryptionTagHeader: NumFilters (UI8, == 1).
        out.push(NUM_FILTERS);
        // FilterName — SWF null-terminated STRING (§E.1: FLV reuses the
        // SWF data types; the STRING type has no length prefix and is
        // NUL-terminated).
        out.extend_from_slice(self.filter_name.as_bytes());
        out.push(0);
        // Length (UI24) of FilterParams in bytes.
        let len = self.params.encoded_len() as u32;
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push(len as u8);
        // FilterParams.
        self.params.encode_into(&mut out);
        // EncryptedBody (or plaintext for an unencrypted SE packet).
        out.extend_from_slice(&self.body);
        out
    }
}

/// Parse the body of an encrypted `FLVTAG` (one whose §E.4.1 `Filter`
/// bit is set) into its §F.3.1 header + §F.3.2 params + trailing body.
///
/// The `EncryptionHeader.Version` is not carried per-tag, so the filter
/// branch is selected by the `FilterName` string itself rather than the
/// version: `"Encryption"` → whole-packet, `"SE"` → selective. An
/// unrecognised filter name is refused (the params layout is filter-
/// specific and cannot be guessed).
///
/// Returns [`Error::Other`] on any structural violation
/// (`NumFilters != 1`, unterminated `FilterName`, a `Length` that
/// overruns the body, or a `FilterParams` block too short for its IV).
pub fn parse_encrypted_body(body: &[u8]) -> Result<EncryptedTag> {
    let mut pos = 0usize;

    // NumFilters (UI8) — §F.3.1 mandates exactly 1.
    let num_filters = *body
        .get(pos)
        .ok_or_else(|| Error::Other("FLV crypt: truncated NumFilters (§F.3.1)".into()))?;
    pos += 1;
    if num_filters != NUM_FILTERS {
        return Err(Error::Other(format!(
            "FLV crypt: NumFilters {num_filters} != 1 (§F.3.1)"
        )));
    }

    // FilterName — NUL-terminated SWF STRING.
    let name_start = pos;
    let nul = body[pos..]
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Other("FLV crypt: unterminated FilterName (§F.3.1)".into()))?;
    let filter_name = std::str::from_utf8(&body[name_start..name_start + nul])
        .map_err(|_| Error::Other("FLV crypt: FilterName is not UTF-8 (§F.3.1)".into()))?
        .to_owned();
    pos = name_start + nul + 1; // skip the NUL terminator

    // Length (UI24) of FilterParams in bytes.
    if pos + 3 > body.len() {
        return Err(Error::Other(
            "FLV crypt: truncated FilterParams Length (§F.3.1)".into(),
        ));
    }
    let params_len =
        ((body[pos] as usize) << 16) | ((body[pos + 1] as usize) << 8) | body[pos + 2] as usize;
    pos += 3;
    let params_end = pos
        .checked_add(params_len)
        .filter(|&e| e <= body.len())
        .ok_or_else(|| {
            Error::Other(format!(
                "FLV crypt: FilterParams Length {params_len} overruns body (§F.3.1)"
            ))
        })?;
    let params_bytes = &body[pos..params_end];

    let params = match filter_name.as_str() {
        FILTER_NAME_ENCRYPTION => {
            // §F.3.2 EncryptionFilterParams: IV UI8[16].
            if params_bytes.len() < IV_LEN {
                return Err(Error::Other(format!(
                    "FLV crypt: 'Encryption' FilterParams {} bytes < 16-byte IV (§F.3.2)",
                    params_bytes.len()
                )));
            }
            let mut iv = [0u8; IV_LEN];
            iv.copy_from_slice(&params_bytes[..IV_LEN]);
            FilterParams::Encryption { iv }
        }
        FILTER_NAME_SELECTIVE => {
            // §F.3.2 SelectiveEncryptionFilterParams: UB[1] EncryptedAU
            // + UB[7] Reserved, then IV iff EncryptedAU.
            let flags = *params_bytes.first().ok_or_else(|| {
                Error::Other("FLV crypt: truncated SE EncryptedAU bitfield (§F.3.2)".into())
            })?;
            let encrypted = (flags & 0x80) != 0;
            if encrypted {
                if params_bytes.len() < 1 + IV_LEN {
                    return Err(Error::Other(format!(
                        "FLV crypt: encrypted SE FilterParams {} bytes < 1 + 16-byte IV (§F.3.2)",
                        params_bytes.len()
                    )));
                }
                let mut iv = [0u8; IV_LEN];
                iv.copy_from_slice(&params_bytes[1..1 + IV_LEN]);
                FilterParams::Selective { iv: Some(iv) }
            } else {
                FilterParams::Selective { iv: None }
            }
        }
        other => {
            return Err(Error::Other(format!(
                "FLV crypt: unknown FilterName {other:?} (§F.3.1 defines 'Encryption' / 'SE')"
            )));
        }
    };

    let body = body[params_end..].to_vec();
    Ok(EncryptedTag {
        filter_name,
        params,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iv16(seed: u8) -> [u8; IV_LEN] {
        let mut iv = [0u8; IV_LEN];
        for (i, b) in iv.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
        iv
    }

    #[test]
    fn encryption_filter_round_trips() {
        let tag = EncryptedTag {
            filter_name: FILTER_NAME_ENCRYPTION.into(),
            params: FilterParams::Encryption { iv: iv16(0x10) },
            body: vec![0xAA; 48], // 32 bytes plaintext -> 48 with 16 padding
        };
        let bytes = tag.encode();
        // NumFilters(1) + "Encryption"(10) + NUL(1) + Length(3) +
        // IV(16) + body(48).
        assert_eq!(bytes.len(), 1 + 10 + 1 + 3 + 16 + 48);
        assert_eq!(bytes[0], NUM_FILTERS);
        // Length field == 16 (just the IV).
        assert_eq!(&bytes[12..15], &[0, 0, 16]);
        let parsed = parse_encrypted_body(&bytes).expect("parse");
        assert_eq!(parsed, tag);
        assert!(parsed.is_encrypted());
        assert_eq!(parsed.iv(), Some(&iv16(0x10)));
    }

    #[test]
    fn selective_encrypted_round_trips() {
        let tag = EncryptedTag {
            filter_name: FILTER_NAME_SELECTIVE.into(),
            params: FilterParams::Selective {
                iv: Some(iv16(0x20)),
            },
            body: vec![0xBB; 16],
        };
        let bytes = tag.encode();
        // Length == 1 (flags) + 16 (IV) = 17.
        let parsed = parse_encrypted_body(&bytes).expect("parse");
        assert_eq!(parsed, tag);
        assert!(parsed.is_encrypted());
        assert_eq!(parsed.iv(), Some(&iv16(0x20)));
    }

    #[test]
    fn selective_unencrypted_round_trips() {
        let tag = EncryptedTag {
            filter_name: FILTER_NAME_SELECTIVE.into(),
            params: FilterParams::Selective { iv: None },
            body: vec![1, 2, 3, 4, 5],
        };
        let bytes = tag.encode();
        let parsed = parse_encrypted_body(&bytes).expect("parse");
        assert_eq!(parsed, tag);
        assert!(!parsed.is_encrypted());
        assert_eq!(parsed.iv(), None);
        // The plaintext body survives verbatim.
        assert_eq!(parsed.body, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn selective_flags_reserved_bits_set_still_decodes_encrypted() {
        // EncryptedAU is the MSB; reserved bits are *supposed* to be 0
        // but a forgiving parser keys only off the MSB.
        let mut bytes = Vec::new();
        bytes.push(NUM_FILTERS);
        bytes.extend_from_slice(b"SE\0");
        bytes.extend_from_slice(&[0, 0, 17]); // Length = 1 + 16
        bytes.push(0xFF); // EncryptedAU=1, reserved bits also set
        bytes.extend_from_slice(&iv16(0x30));
        bytes.extend_from_slice(&[0xCC; 32]);
        let parsed = parse_encrypted_body(&bytes).expect("parse");
        assert!(parsed.is_encrypted());
        assert_eq!(parsed.iv(), Some(&iv16(0x30)));
    }

    #[test]
    fn rejects_num_filters_not_one() {
        let mut bytes = Vec::new();
        bytes.push(2); // NumFilters != 1
        bytes.extend_from_slice(b"Encryption\0");
        bytes.extend_from_slice(&[0, 0, 16]);
        bytes.extend_from_slice(&iv16(0));
        let err = parse_encrypted_body(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("NumFilters")),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_unknown_filter_name() {
        let mut bytes = Vec::new();
        bytes.push(NUM_FILTERS);
        bytes.extend_from_slice(b"Bogus\0");
        bytes.extend_from_slice(&[0, 0, 0]);
        let err = parse_encrypted_body(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("FilterName")),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_unterminated_filter_name() {
        let mut bytes = Vec::new();
        bytes.push(NUM_FILTERS);
        bytes.extend_from_slice(b"Encryption"); // no NUL
        let err = parse_encrypted_body(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("FilterName")),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_length_overrun() {
        let mut bytes = Vec::new();
        bytes.push(NUM_FILTERS);
        bytes.extend_from_slice(b"Encryption\0");
        bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // huge Length
        bytes.extend_from_slice(&iv16(0));
        let err = parse_encrypted_body(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("overruns")),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_encryption_params_too_short_for_iv() {
        let mut bytes = Vec::new();
        bytes.push(NUM_FILTERS);
        bytes.extend_from_slice(b"Encryption\0");
        bytes.extend_from_slice(&[0, 0, 8]); // only 8 bytes of "IV"
        bytes.extend_from_slice(&[0u8; 8]);
        let err = parse_encrypted_body(&bytes).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("IV")),
            "{err:?}"
        );
    }

    #[test]
    fn empty_body_truncated_num_filters() {
        let err = parse_encrypted_body(&[]).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("NumFilters")),
            "{err:?}"
        );
    }
}

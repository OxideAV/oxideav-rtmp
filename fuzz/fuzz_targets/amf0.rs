#![no_main]

//! AMF0: arbitrary bytes through `decode_all` / `decode`. Whatever
//! decodes must re-encode, and that encoding must be a fixed point:
//! decoding it and encoding again yields the same bytes.

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::amf;

fuzz_target!(|data: &[u8]| {
    let mut pos = 0usize;
    if amf::decode(data, &mut pos).is_ok() {
        assert!(pos <= data.len());
    }
    let Ok(values) = amf::decode_all(data) else {
        return;
    };
    let mut first = Vec::new();
    for v in &values {
        amf::encode(&mut first, v);
    }
    let again = amf::decode_all(&first).expect("own AMF0 output must decode");
    assert_eq!(again.len(), values.len());
    let mut second = Vec::new();
    for v in &again {
        amf::encode(&mut second, v);
    }
    assert_eq!(first, second, "AMF0 encoding is not a fixed point");
});

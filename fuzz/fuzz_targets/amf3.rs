#![no_main]

//! AMF3: arbitrary bytes through every decode framing (bare values,
//! the type-15 data-message shape, the format-selector shape bridged
//! to AMF0) and the U29 reader. Whatever `decode_all` accepts must
//! re-encode to a fixed point.

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::amf3;

fuzz_target!(|data: &[u8]| {
    let mut pos = 0usize;
    if amf3::read_u29(data, &mut pos).is_ok() {
        assert!(pos <= data.len() && pos <= 4);
    }
    let mut pos = 0usize;
    if amf3::decode(data, &mut pos).is_ok() {
        assert!(pos <= data.len());
    }
    let _ = amf3::decode_data_message(data);
    if let Ok(values) = amf3::decode_message_to_amf0(data) {
        // The bridged values are ordinary AMF0 and must encode.
        let mut out = Vec::new();
        for v in &values {
            oxideav_rtmp::amf::encode(&mut out, v);
        }
    }
    let Ok(values) = amf3::decode_all(data) else {
        return;
    };
    for v in &values {
        let _ = v.to_amf0();
    }
    let first = amf3::encode_all(&values);
    let again = amf3::decode_all(&first).expect("own AMF3 output must decode");
    assert_eq!(again.len(), values.len());
    let second = amf3::encode_all(&again);
    assert_eq!(first, second, "AMF3 encoding is not a fixed point");
});

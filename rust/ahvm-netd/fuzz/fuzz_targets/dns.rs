#![no_main]
#[path = "../../src/dns.rs"]
mod dns;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let seed =
        b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01";
    let _ = dns::Question::query(seed).unwrap().matches(data);
    if let Some(q) = dns::Question::query(data) {
        let mut reply = data.to_vec();
        reply[2] |= 0x80;
        assert!(q.matches(&reply));
        assert!(q.matches(&q.truncated_reply(&reply).unwrap()));
        reply[0] ^= 1;
        assert!(!q.matches(&reply));
    }
});

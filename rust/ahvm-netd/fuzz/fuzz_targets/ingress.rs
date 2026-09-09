#![no_main]
#[path = "../../src/wire.rs"]
mod wire;
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    if let Some(wire::Packet::Ipv4(ip)) = wire::validate(data) {
        assert_eq!(ip.src_addr(), wire::GUEST);
        assert!(!ip.more_frags());
        assert_eq!(ip.frag_offset(), 0);
        assert!(data.len() <= 1514);
    }
});

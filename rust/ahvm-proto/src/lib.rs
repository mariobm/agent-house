//! Agent wire protocol, v2.
//!
//! Clean break from the Go implementation's framing: v2 keeps the good
//! properties (tiny header, single-write frames, stream-safe parsing) and
//! changes the rest — explicit version nibble so future revisions negotiate
//! instead of breaking. No legacy readers, no migration shims: pre-release
//! project, no production data to carry.
//!
//! Design notes live with the dashboard-first API plan: the daemon API and
//! this transport are both redesigned, similar in spirit to v1 where it was
//! good, better where it wasn't.

pub mod v2;

pub use v2::{read_frame, write_frame, Frame, FrameType, MAX_FRAME_SIZE, PROTOCOL_VERSION};

/// Host-authorized TCP exceptions are exact endpoints, never wildcard networks.
/// Link-local metadata, guest shared-address space and non-unicast destinations
/// remain forbidden even when accidentally listed by an administrator.
pub fn valid_private_endpoint(endpoint: std::net::SocketAddrV4) -> bool {
    let [a, b, _, _] = endpoint.ip().octets();
    endpoint.port() != 0
        && !matches!(a, 0 | 224..=255)
        && !(a == 169 && b == 254)
        && !(a == 100 && (64..=127).contains(&b))
}

#[test]
fn private_grants_never_allow_metadata_or_wildcards() {
    for endpoint in [
        "169.254.169.254:80",
        "0.0.0.0:80",
        "224.0.0.1:80",
        "100.64.0.2:80",
        "10.0.0.1:0",
    ] {
        assert!(
            !valid_private_endpoint(endpoint.parse().unwrap()),
            "{endpoint}"
        );
    }
    for endpoint in ["10.0.0.1:5432", "127.0.0.1:8000", "192.168.1.5:443"] {
        assert!(
            valid_private_endpoint(endpoint.parse().unwrap()),
            "{endpoint}"
        );
    }
}

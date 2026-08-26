//! The accept-key derivation: base64 and the RFC 6455 vector.

use super::*;
use crate::ws::tests::{RFC_ACCEPT, RFC_KEY};

#[test]
fn base64_rfc4648_vectors() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
}

#[test]
fn accept_key_matches_rfc6455_vector() {
    assert_eq!(accept_key(RFC_KEY.as_bytes()), RFC_ACCEPT);
}

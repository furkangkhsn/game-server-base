//! Known-answer tests: RFC 8439 §2.8.2 for the AEAD, the cacophony Noise
//! vector for `Noise_NK_25519_ChaChaPoly_BLAKE2s` (as shipped in snow
//! 0.10.0's tests/vectors/cacophony.txt) through our own wrapper, and our
//! REKEY against snow's.

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{AeadInPlace, KeyInit};

use super::super::handshake::{builder, session};
use super::super::key::PhaseKey;
use super::super::wire::Direction;
use super::*;

#[test]
fn rfc8439_2_8_2_aead_known_answer() {
    let key: Vec<u8> = (0x80..=0x9f).collect();
    let nonce = hex("07000000 4041424344454647");
    let aad = hex("50515253 c0c1c2c3c4c5c6c7");
    let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let ciphertext = hex(
        "d31a8d34648e60db7b86afbc53ef7ec2 a4aded51296e08fea9e2b5a736ee62d6
         3dbea45e8ca9671282fafb69da92728b 1a71de0a9e060b2905d6a5b67ecd3b36
         92ddbd7f2d778b8c9803aee328091b58 fab324e4fad675945585808b4831d7bc
         3ff4def08e4b7a9de576d26586cec64b 6116",
    );
    let tag = hex("1ae10b594f09e26a7e902ecbd0600691");
    let aead = ChaCha20Poly1305::new_from_slice(&key).unwrap();
    let mut buf = plaintext.to_vec();
    let got = aead
        .encrypt_in_place_detached(nonce[..].into(), &aad, &mut buf)
        .unwrap();
    assert_eq!(buf, ciphertext);
    assert_eq!(&got[..], &tag[..]);
}

struct Vector {
    prologue: Vec<u8>,
    init_e: Vec<u8>,
    resp_static: [u8; 32],
    resp_static_public: Vec<u8>,
    resp_e: Vec<u8>,
    hash: Vec<u8>,
    /// (payload, ciphertext): two handshake messages, then transport
    /// messages alternating initiator→responder / responder→initiator.
    msgs: Vec<(Vec<u8>, Vec<u8>)>,
}

fn nk_blake2s() -> Vector {
    let m = |p: &str, c: &str| (hex(p), hex(c));
    Vector {
        prologue: hex("4a6f686e2047616c74"),
        init_e: hex("893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a"),
        resp_static: hex("4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893")
            .try_into()
            .unwrap(),
        resp_static_public: hex("31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62"),
        resp_e: hex("bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b"),
        hash: hex("d7244d974066aae2376f7ba5534f60a6e4e82cd7c9751e226cae3928e6b49f14"),
        msgs: vec![
            m(
                "4c756477696720766f6e204d69736573",
                "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c7944
                 54ae7612d1724af42adb130160a9a94e67b5b169b4e00c189f6467cd17eb7cad",
            ),
            m(
                "4d757272617920526f746862617264",
                "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f144808843
                 986a5c929337e337ac8b4a074af12ab9f76318a5f18c8b599a443af07383ce",
            ),
            m(
                "462e20412e20486179656b",
                "550027c7a5d450017bcb5e12b8253b1c53fd2213aeda84891d5f95",
            ),
            m(
                "4361726c204d656e676572",
                "dfbce0c38210ccee35e830aca9dd8b8b3997b933e75bfc8864b759",
            ),
            m(
                "4a65616e2d426170746973746520536179",
                "4c487a88330c7c65e44d430addf3d92d2a15b081a2892b96693e00b68aec0adac2",
            ),
            m(
                "457567656e2042f6686d20766f6e2042617765726b",
                "471cb9f8252d8ae7b25c93f4b4aebdbf25e5baa23f14bc743559e3ef7fd065e69cfaef55ee",
            ),
        ],
    }
}

#[test]
fn cacophony_nk_blake2s_through_our_wrapper() {
    let v = nk_blake2s();
    let server = StaticKey::from_private(v.resp_static).unwrap();
    assert_eq!(
        &server.public()[..],
        &v.resp_static_public[..],
        "public key derivation"
    );

    let b = builder(&v.prologue)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&v.init_e);
    let mut init = Initiator::start(b, &server.public(), &v.msgs[0].0).unwrap();
    assert_eq!(init.msg1(), &v.msgs[0].1[..], "message 1");

    let b = builder(&v.prologue)
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&v.resp_e);
    let msg1 = Msg1::parse(init.msg1()).unwrap();
    let (msg2, payload1, mut resp) = msg1.respond(b, &server, &v.msgs[1].0).unwrap();
    assert_eq!(payload1, v.msgs[0].0);
    assert_eq!(msg2, v.msgs[1].1, "message 2");

    let mut out = [0u8; 64];
    let n = init.read(&msg2, &mut out).unwrap();
    assert_eq!(&out[..n], &v.msgs[1].0[..]);
    let client = session(init.state(), Some(9));
    let server_side = session(&mut resp, None);
    assert_eq!(&client.handshake_hash()[..], &v.hash[..], "handshake hash");
    assert_eq!(server_side.handshake_hash(), client.handshake_hash());

    // Transport messages: our nonce encoding and Split() order, empty AAD.
    let (mut k_c2s, mut k_s2c) = init.state().dangerously_get_raw_split();
    let (mut ref_c2s_raw, mut ref_s2c_raw) = (k_c2s, k_s2c);
    let c2s = PhaseKey::new(&mut k_c2s);
    let s2c = PhaseKey::new(&mut k_s2c);
    for (i, (payload, ct)) in v.msgs[2..].iter().enumerate() {
        let key = if i % 2 == 0 { &c2s } else { &s2c };
        let counter = (i / 2) as u64;
        let mut sealed = Vec::new();
        key.seal(counter, &[], payload, &mut sealed);
        assert_eq!(&sealed, ct, "transport message {}", i + 2);
        assert_eq!(key.open(counter, &[], ct).as_ref(), Some(payload));
    }

    // Our session halves use exactly those keys, each in its direction.
    let (mut cs, _) = client.into_halves();
    let (mut ss, _) = server_side.into_halves();
    let mut ref_c2s = Opener::new(&mut ref_c2s_raw, Direction::ClientToServer);
    let mut ref_s2c = Opener::new(&mut ref_s2c_raw, Direction::ServerToClient);
    let (mut up, mut down) = (Vec::new(), Vec::new());
    cs.seal(b"up", &mut up).unwrap();
    ss.seal(b"down", &mut down).unwrap();
    assert_eq!(ref_c2s.open(&up).unwrap().plaintext, b"up");
    assert_eq!(ref_s2c.open(&down).unwrap().plaintext, b"down");
}

#[test]
fn our_rekey_equals_snows_rekey() {
    let v = nk_blake2s();
    let p: snow::params::NoiseParams = NOISE_PATTERN.parse().unwrap();
    let mut i = snow::Builder::new(p.clone())
        .prologue(&v.prologue)
        .unwrap()
        .remote_public_key(&v.resp_static_public)
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut r = snow::Builder::new(p)
        .prologue(&v.prologue)
        .unwrap()
        .local_private_key(&v.resp_static)
        .unwrap()
        .build_responder()
        .unwrap();
    let (mut m, mut scratch) = ([0u8; 128], [0u8; 128]);
    let n = i.write_message(&[], &mut m).unwrap();
    r.read_message(&m[..n], &mut scratch).unwrap();
    let n = r.write_message(&[], &mut m).unwrap();
    i.read_message(&m[..n], &mut scratch).unwrap();
    let (mut raw, _) = i.dangerously_get_raw_split();
    let mut t = i.into_stateless_transport_mode().unwrap();

    let ours = PhaseKey::new(&mut raw).rekey().rekey();
    t.rekey_outgoing();
    t.rekey_outgoing();
    let n = t.write_message(5, b"after two rekeys", &mut m).unwrap();
    let mut sealed = Vec::new();
    ours.seal(5, &[], b"after two rekeys", &mut sealed);
    assert_eq!(sealed, &m[..n]);
}

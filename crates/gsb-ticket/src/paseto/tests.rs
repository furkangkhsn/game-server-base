//! Known answers: the official PASETO v4 test vectors
//! (github.com/paseto-standard/test-vectors, `v4.json`): the three
//! `v4.public` signing vectors must sign to the exact token and verify,
//! and the failure vectors that are tokens of another version or purpose
//! must be refused.

use super::*;

const SECRET: &str = "b4cbfb43df4ce210727d953e4a713307fa19bb7d9f85041438d9e11b942a3774";
const PUBLIC: &str = "1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2";
const PAYLOAD: &str = r#"{"data":"this is a signed message","exp":"2022-01-01T00:00:00+00:00"}"#;
const FOOTER: &str = r#"{"kid":"zVhMiPBP9fRf2snEcT7gFTioeA9COcNy9DfgL1W60haN"}"#;

/// (name, footer, implicit assertion, token)
const SIGNED: [(&str, &str, &str, &str); 3] = [
    (
        "4-S-1",
        "",
        "",
        "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9bg_XBBzds8lTZShVlwwKSgeKpLT3yukTw6JUz3W4h_ExsQV-P0V54zemZDcAxFaSeef1QlXEFtkqxT1ciiQEDA",
    ),
    (
        "4-S-2",
        FOOTER,
        "",
        "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9v3Jt8mx_TdM2ceTGoqwrh4yDFn0XsHvvV_D0DtwQxVrJEBMl0F2caAdgnpKlt4p7xBnx1HcO-SPo8FPp214HDw.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
    ),
    (
        "4-S-3",
        FOOTER,
        r#"{"test-vector":"4-S-3"}"#,
        "v4.public.eyJkYXRhIjoidGhpcyBpcyBhIHNpZ25lZCBtZXNzYWdlIiwiZXhwIjoiMjAyMi0wMS0wMVQwMDowMDowMCswMDowMCJ9NPWciuD3d0o5eXJXG5pJy-DiVEoyPYWs1YSTwWHNJq6DZD3je5gf-0M4JR9ipdUSJbIovzmBECeaWmaqcaP0DQ.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
    ),
];

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex");
    }
    out
}

fn keys() -> (SigningKey, VerifyingKey) {
    let sk = SigningKey::from_bytes(&hex32(SECRET));
    let pk = VerifyingKey::from_bytes(&hex32(PUBLIC)).expect("valid key");
    assert_eq!(sk.verifying_key(), pk, "the vectors' key pair");
    (sk, pk)
}

#[test]
fn the_official_v4_public_vectors_sign_and_verify() {
    let (sk, pk) = keys();
    for (name, footer, implicit, token) in SIGNED {
        let (f, i) = (footer.as_bytes(), implicit.as_bytes());
        assert_eq!(sign(&sk, PAYLOAD.as_bytes(), f, i), token, "{name} signs");
        let message = verify(&pk, token, f, i).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(message, PAYLOAD.as_bytes(), "{name} verifies");
    }
}

#[test]
fn a_changed_footer_assertion_or_byte_is_refused() {
    let (_, pk) = keys();
    let (_, footer, implicit, token) = SIGNED[2];
    let (f, i) = (footer.as_bytes(), implicit.as_bytes());
    assert_eq!(
        verify(&pk, token, f, b""),
        Err(Refusal::Signature),
        "assertion"
    );
    assert_eq!(
        verify(&pk, token, b"", i),
        Err(Refusal::Malformed),
        "footer"
    );
    // One flipped character inside the signed body.
    let mut bad = token.to_string();
    let at = HEADER.len() + 5;
    let flip = if &bad[at..=at] == "A" { "B" } else { "A" };
    bad.replace_range(at..=at, flip);
    assert_eq!(verify(&pk, &bad, f, i), Err(Refusal::Signature), "body");
}

#[test]
fn the_failure_vectors_of_another_version_or_purpose_are_refused() {
    let (_, pk) = keys();
    for (name, token, footer) in [
        (
            "4-F-1 (v4.local)",
            "v4.local.vngXfCISbnKgiP6VWGuOSlYrFYU300fy9ijW33rznDYgxHNPwWluAY2Bgb0z54CUs6aYYkIJ-bOOOmJHPuX_34Agt_IPlNdGDpRdGNnBz2MpWJvB3cttheEc1uyCEYltj7wBQQYX.YXJiaXRyYXJ5LXN0cmluZy10aGF0LWlzbid0LWpzb24",
            "arbitrary-string-that-isn't-json",
        ),
        (
            "4-F-2 (a public token under a local key)",
            "v4.public.eyJpbnZhbGlkIjoidGhpcyBzaG91bGQgbmV2ZXIgZGVjb2RlIn22Sp4gjCaUw0c7EH84ZSm_jN_Qr41MrgLNu5LIBCzUr1pn3Z-Wukg9h3ceplWigpoHaTLcwxj0NsI1vjTh67YB.eyJraWQiOiJ6VmhNaVBCUDlmUmYyc25FY1Q3Z0ZUaW9lQTlDT2NOeTlEZmdMMVc2MGhhTiJ9",
            FOOTER,
        ),
        (
            "4-F-3 (v3.local)",
            "v3.local.23e_2PiqpQBPvRFKzB0zHhjmxK3sKo2grFZRRLM-U7L0a8uHxuF9RlVz3Ic6WmdUUWTxCaYycwWV1yM8gKbZB2JhygDMKvHQ7eBf8GtF0r3K0Q_gF1PXOxcOgztak1eD1dPe9rLVMSgR0nHJXeIGYVuVrVoLWQ.YXJiaXRyYXJ5LXN0cmluZy10aGF0LWlzbid0LWpzb24",
            "arbitrary-string-that-isn't-json",
        ),
    ] {
        assert!(
            verify(&pk, token, footer.as_bytes(), b"").is_err(),
            "{name}"
        );
    }
}

#[test]
fn pae_is_the_specifications() {
    // The specification's own examples (`docs/01-Protocol-Versions/Common.md`).
    assert_eq!(pae(&[]), b"\x00\x00\x00\x00\x00\x00\x00\x00");
    assert_eq!(
        pae(&[b""]),
        b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
    );
    let mut want = vec![1, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0];
    want.extend_from_slice(b"test");
    assert_eq!(pae(&[b"test"]), want);
}

#[test]
fn a_split_refuses_what_is_not_one_token() {
    for bad in [
        "",
        "v4.public.",
        "v4.public.AAAA",
        "v4.public.!!!!",
        "v4.public.AAAA.",
        "v4.public.AAAA.AA.AA",
        "v3.public.AAAA",
    ] {
        assert_eq!(split(bad), Err(Refusal::Malformed), "{bad:?}");
    }
}

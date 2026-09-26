//! The endpoint grammar: `http://host[:port][/path]`, nothing else.

use super::*;

fn parsed(url: &str) -> (String, u16, String, String) {
    let e = Endpoint::parse(url).expect("parses");
    (e.host, e.port, e.authority, e.path)
}

#[test]
fn a_bare_authority_gets_the_otlp_metrics_path_and_port_80() {
    assert_eq!(
        parsed("http://collector"),
        (
            "collector".into(),
            80,
            "collector".into(),
            "/v1/metrics".into()
        )
    );
    assert_eq!(
        parsed("http://127.0.0.1:4318/"),
        (
            "127.0.0.1".into(),
            4318,
            "127.0.0.1:4318".into(),
            "/v1/metrics".into()
        )
    );
}

#[test]
fn a_written_path_and_an_ipv6_literal_are_kept() {
    assert_eq!(
        parsed("http://[::1]:4318/otlp/v1/metrics"),
        (
            "::1".into(),
            4318,
            "[::1]:4318".into(),
            "/otlp/v1/metrics".into()
        )
    );
}

#[test]
fn https_and_malformed_urls_refuse() {
    assert_eq!(
        Endpoint::parse("https://collector:4318"),
        Err(OtlpError::Https("https://collector:4318".into()))
    );
    for url in [
        "collector:4318",
        "grpc://collector:4317",
        "http://",
        "http://:4318",
        "http://collector:port",
        "http://collector:99999",
        "http://[::1:4318",
        "http://collector/a b",
    ] {
        assert!(
            matches!(Endpoint::parse(url), Err(OtlpError::BadEndpoint(..))),
            "{url} must refuse"
        );
    }
}

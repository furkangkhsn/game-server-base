//! The `[metrics]` grammar: absent is empty, `[metrics.otlp]` takes an
//! endpoint with defaults for the rest, and a misspelled key refuses.

use crate::Config;

fn parse(text: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(text)
}

#[test]
fn no_metrics_table_means_no_push_exporter() {
    assert_eq!(parse("").expect("parses").metrics.otlp, None);
    assert_eq!(parse("[metrics]\n").expect("parses").metrics.otlp, None);
}

#[test]
fn an_otlp_table_takes_the_endpoint_and_defaults_the_rest() {
    let cfg = parse("[metrics.otlp]\nendpoint = \"http://127.0.0.1:4318\"\n").expect("parses");
    let otlp = cfg.metrics.otlp.expect("the table");
    assert_eq!(otlp.endpoint, "http://127.0.0.1:4318");
    assert_eq!(otlp.interval_secs, 10);
    assert_eq!(otlp.service_name, "gsb");
    let cfg = parse(
        "[metrics.otlp]\nendpoint = \"http://c/v1/metrics\"\ninterval_secs = 3\nservice_name = \"edge-1\"\n",
    )
    .expect("parses");
    let otlp = cfg.metrics.otlp.expect("the table");
    assert_eq!(
        (otlp.interval_secs, otlp.service_name.as_str()),
        (3, "edge-1")
    );
}

#[test]
fn a_misspelled_or_missing_key_refuses() {
    assert!(parse("[metrics.otlp]\nendpoint = \"http://c\"\ninterval = 3\n").is_err());
    assert!(
        parse("[metrics.otlp]\ninterval_secs = 3\n").is_err(),
        "no endpoint"
    );
    assert!(
        parse("[metrics.prometheus]\n").is_err(),
        "no such exporter table"
    );
}

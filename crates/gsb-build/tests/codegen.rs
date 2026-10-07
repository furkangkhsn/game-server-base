//! The codegen end to end: real protoc (the vendored one) over the
//! fixtures in `tests/fixtures`, into a scratch directory.
//!
//! - `good.proto` compiles; the generated package file holds exactly the
//!   golden impls (`tests/golden/fixture.good.rs`) and nothing for the
//!   ordinary service;
//! - every rule refuses its own fixture with its own [`Problem`].

use std::path::{Path, PathBuf};

use gsb_build::{Builder, Error, Problem};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// gsb-protocol's proto directory (`gsb/options.proto`): inside the
/// workspace a test has no `DEP_GSB_BASE_PROTO_DIR`.
fn engine_protos() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../gsb-protocol/proto")
}

fn compile(name: &str) -> (Result<gsb_build::Plan, Error>, PathBuf) {
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gsb-build-{name}"));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("scratch dir");
    let mut b = Builder::new();
    b.out_dir(&out);
    let r = b.compile(
        &[fixtures().join(format!("{name}.proto"))],
        &[fixtures(), engine_protos()],
    );
    (r, out)
}

fn problems(name: &str) -> Vec<Problem> {
    match compile(name).0 {
        Err(Error::Invalid(p)) => p,
        Err(e) => panic!("{name}: not a rule refusal: {e}"),
        Ok(_) => panic!("{name}: must be refused"),
    }
}

/// The impls land in the request's own package file, as the golden says.
#[test]
fn good_generates_the_golden_impls() {
    let (plan, out) = compile("good");
    let plan = plan.unwrap_or_else(|e| panic!("{e}"));
    let ops: Vec<(String, u16)> = plan.requests.iter().map(|d| (d.rpc(), d.op)).collect();
    assert_eq!(
        ops,
        [
            ("fixture.good.Game.Transfer".to_owned(), 1001),
            ("fixture.good.Workshop.Make".to_owned(), 1003),
        ]
    );
    assert_eq!(plan.requests[1].error, ".fixture.common.SharedError");
    assert_eq!(
        plan.reserved.iter().copied().collect::<Vec<_>>(),
        [1002, 1004]
    );
    let generated = std::fs::read_to_string(out.join("fixture.good.rs")).expect("generated");
    let impls: String = generated
        .split_inclusive('\n')
        .skip_while(|l| !l.starts_with("/// gsb request"))
        .collect();
    let golden = include_str!("golden/fixture.good.rs");
    assert_eq!(
        impls, golden,
        "the generated impls changed (update tests/golden if intended)"
    );
    assert!(!generated.contains("impl ::gsb_protocol::request::Call for Plain"));
}

#[test]
fn a_duplicate_op_is_refused() {
    assert_eq!(
        problems("dup_op"),
        [Problem::DuplicateOp {
            op: 1001,
            first: "fixture.bad.S.One".into(),
            second: "fixture.bad.S.Two".into(),
        }]
    );
}

#[test]
fn an_op_in_the_engine_range_is_refused() {
    assert_eq!(
        problems("engine_range"),
        [Problem::EngineRange {
            rpc: "fixture.bad.S.One".into(),
            op: 999,
        }]
    );
}

#[test]
fn an_op_past_16_bits_is_refused() {
    assert_eq!(
        problems("too_large"),
        [Problem::TooLarge {
            rpc: "fixture.bad.S.One".into(),
            op: 70000,
        }]
    );
}

#[test]
fn a_missing_error_is_refused() {
    assert_eq!(
        problems("missing_error"),
        [Problem::MissingError {
            rpc: "fixture.bad.S.One".into(),
        }]
    );
}

#[test]
fn an_empty_error_name_is_a_missing_error() {
    assert_eq!(
        problems("empty_error"),
        [Problem::MissingError {
            rpc: "fixture.bad.S.One".into(),
        }]
    );
}

#[test]
fn an_unknown_error_is_refused() {
    assert_eq!(
        problems("unknown_error"),
        [Problem::UnknownError {
            rpc: "fixture.bad.S.One".into(),
            error: "Nope".into(),
        }]
    );
}

#[test]
fn a_missing_op_in_a_gsb_service_is_refused() {
    assert_eq!(
        problems("missing_op"),
        [Problem::MissingOp {
            rpc: "fixture.bad.S.Two".into(),
        }]
    );
}

#[test]
fn a_reserved_op_is_refused() {
    assert_eq!(
        problems("reserved"),
        [Problem::ReservedOp {
            op: 1007,
            rpc: "fixture.bad.S.One".into(),
        }]
    );
}

#[test]
fn one_request_behind_two_rpcs_is_refused() {
    assert_eq!(
        problems("shared_request"),
        [Problem::SharedRequest {
            request: ".fixture.bad.A".into(),
            first: "fixture.bad.S.One".into(),
            second: "fixture.bad.S.Two".into(),
        }]
    );
}

#[test]
fn a_streaming_rpc_is_refused() {
    assert_eq!(
        problems("streaming"),
        [Problem::Streaming {
            rpc: "fixture.bad.S.One".into(),
        }]
    );
}

/// The refusal names every problem, readable in a build log.
#[test]
fn the_refusal_reads_well() {
    let e = compile("dup_op").0.expect_err("refused");
    let text = e.to_string();
    assert!(
        text.contains("op 1001 is taken twice: by fixture.bad.S.One and by fixture.bad.S.Two"),
        "{text}"
    );
}

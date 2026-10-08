//! The codegen end to end: real protoc (the vendored one) over the
//! fixtures in `tests/fixtures`, into a scratch directory.
//!
//! - `good.proto` compiles; the generated package file holds exactly the
//!   golden impls (`tests/golden/fixture.good.rs`: requests, then events)
//!   and nothing for the ordinary service; the imported file's event
//!   lands in its own package file;
//! - every rule refuses its own fixture with its own [`Problem`].

use std::path::{Path, PathBuf};

use gsb_build::{Builder, Destination, Error, Problem};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// gsb-protocol's proto directory (`gsb/options.proto`): inside the
/// workspace a test has no `DEP_GSB_BASE_PROTO_DIR`.
fn engine_protos() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../gsb-protocol/proto")
}

fn compile(name: &str) -> (Result<gsb_build::Plan, Error>, PathBuf) {
    // One scratch directory per CALL: two tests compiling the same
    // fixture in parallel must not delete each other's descriptor set.
    static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("gsb-build-{name}-{n}"));
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
        [1002, 1004, 2099]
    );
    let events: Vec<(String, u16, Destination)> = plan
        .events
        .iter()
        .map(|e| (e.event(), e.op, e.to))
        .collect();
    assert_eq!(
        events,
        [
            (
                "fixture.common.Announce".to_owned(),
                2010,
                Destination::Client
            ),
            (
                "fixture.good.GoldReceived".to_owned(),
                2001,
                Destination::World
            ),
            (
                "fixture.good.ChatLine".to_owned(),
                2002,
                Destination::Client
            ),
            (
                "fixture.good.BalanceChanged".to_owned(),
                2005,
                Destination::Both
            ),
            (
                "fixture.good.Presence.Online".to_owned(),
                2006,
                Destination::Client
            ),
        ]
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
    assert!(!generated.contains("Announce"), "another file's event");
    let common = std::fs::read_to_string(out.join("fixture.common.rs")).expect("generated");
    let impls: String = common
        .split_inclusive('\n')
        .skip_while(|l| !l.starts_with("/// gsb event"))
        .collect();
    assert_eq!(
        impls,
        "/// gsb event `fixture.common.Announce` (op 2010, to CLIENT).\n\
         impl ::gsb_protocol::event::Event for Announce {\n    \
             const OP: u16 = 2010;\n    \
             const TO: ::gsb_protocol::event::Dest = ::gsb_protocol::event::Dest::Client;\n\
         }\n\
         impl ::gsb_protocol::event::ClientEvent for Announce {}\n"
    );
}

/// `event_contract_path` moves the event impls (and only them) to
/// another path for the same traits.
#[test]
fn the_event_contract_path_is_configurable() {
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("gsb-build-event-path");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("scratch dir");
    let mut b = Builder::new();
    b.out_dir(&out).event_contract_path("::gsb_core::events");
    b.compile(
        &[fixtures().join("good.proto")],
        &[fixtures(), engine_protos()],
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let generated = std::fs::read_to_string(out.join("fixture.good.rs")).expect("generated");
    assert!(generated.contains("impl ::gsb_core::events::WorldEvent for GoldReceived {}"));
    assert!(!generated.contains("::gsb_protocol::event"));
    assert!(generated.contains("impl ::gsb_protocol::request::Request for TransferGold"));
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

#[test]
fn an_event_op_taken_by_a_request_is_refused() {
    assert_eq!(
        problems("event_dup_request"),
        [Problem::DuplicateOp {
            op: 1001,
            first: "fixture.bad.S.One".into(),
            second: "fixture.bad.Ev".into(),
        }]
    );
}

#[test]
fn two_events_on_one_op_are_refused() {
    assert_eq!(
        problems("event_dup_event"),
        [Problem::DuplicateOp {
            op: 2001,
            first: "fixture.bad.One".into(),
            second: "fixture.bad.Two".into(),
        }]
    );
}

#[test]
fn an_event_op_in_the_engine_range_is_refused() {
    assert_eq!(
        problems("event_engine_range"),
        [Problem::EventEngineRange {
            event: "fixture.bad.Low".into(),
            op: 17,
        }]
    );
}

#[test]
fn an_event_op_past_16_bits_is_refused() {
    assert_eq!(
        problems("event_too_large"),
        [Problem::EventTooLarge {
            event: "fixture.bad.Big".into(),
            op: 70000,
        }]
    );
}

#[test]
fn an_event_without_an_op_is_refused() {
    assert_eq!(
        problems("event_missing_op"),
        [Problem::EventMissingOp {
            event: "fixture.bad.NoOp".into(),
        }]
    );
}

#[test]
fn an_event_without_a_destination_is_refused() {
    let want = [Problem::EventMissingDestination {
        event: "fixture.bad.NoWhere".into(),
    }];
    assert_eq!(problems("event_missing_to"), want);
    assert_eq!(problems("event_unspecified"), want);
}

/// `(gsb.reserved_ops)` (file) closes an op to rpcs and events alike, as
/// does `(gsb.reserved_op)` (service).
#[test]
fn a_reserved_op_is_refused_in_both_spaces() {
    assert_eq!(
        problems("event_reserved"),
        [
            Problem::ReservedOp {
                op: 1001,
                rpc: "fixture.bad.S.One".into(),
            },
            Problem::EventReservedOp {
                op: 2001,
                event: "fixture.bad.Retired".into(),
            },
            Problem::EventReservedOp {
                op: 2002,
                event: "fixture.bad.AlsoRetired".into(),
            },
        ]
    );
}

#[test]
fn a_message_that_is_a_request_and_an_event_is_refused() {
    assert_eq!(
        problems("event_is_request"),
        [Problem::EventIsRequest {
            event: "fixture.bad.A".into(),
            rpc: "fixture.bad.S.One".into(),
        }]
    );
}

/// An event refusal reads as well as a request one.
#[test]
fn an_event_refusal_reads_well() {
    let e = compile("event_dup_request").0.expect_err("refused");
    let text = e.to_string();
    assert!(
        text.contains("op 1001 is taken twice: by fixture.bad.S.One and by fixture.bad.Ev"),
        "{text}"
    );
    let e = compile("event_missing_to").0.expect_err("refused");
    assert!(
        e.to_string().contains(
            "event fixture.bad.NoWhere: `(gsb.event)` has no `to` (WORLD, CLIENT or BOTH)"
        ),
        "{e}"
    );
}

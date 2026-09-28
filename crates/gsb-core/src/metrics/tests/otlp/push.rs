//! A real push: the exporter hands a report to its push task, which
//! POSTs it to a tiny in-test HTTP receiver; the receiver decodes the
//! protobuf body and the test reads the values back. Failed pushes (an
//! error status, a peer that never answers) are counted into the next.

use super::*;
use crate::metrics::otlp::{OtlpConfig, otlp};
use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Read one request: its head and its `Content-Length` body.
async fn read_request(s: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await.expect("read");
        assert!(n > 0, "request cut short");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).expect("ascii head");
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .expect("content length")
        .parse()
        .expect("numeric length");
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).await.expect("read body");
        assert!(n > 0, "body cut short");
        buf.extend_from_slice(&chunk[..n]);
    }
    (head, buf[head_end..head_end + len].to_vec())
}

/// Accept one push, answer `status`, return its head and decoded body.
async fn receive(l: &TcpListener, status: &str) -> (String, proto::ExportMetricsServiceRequest) {
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(5), l.accept())
        .await
        .expect("a push arrived")
        .expect("accept");
    let (head, body) = read_request(&mut s).await;
    let answer = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    s.write_all(answer.as_bytes()).await.expect("answer");
    let req = proto::ExportMetricsServiceRequest::decode(body.as_slice()).expect("protobuf");
    (head, req)
}

/// A report with one room (30 steps, 2 members) and a registry.
fn report(t: Instant) -> MetricReport {
    let mut acc = MetricAccumulator::default();
    let mut s = room_sample(RoomId(4), t, 30);
    s.members = 2;
    acc.apply(MetricsEvent::Room(s));
    acc.apply(MetricsEvent::Registry(RegistrySample {
        rooms: 1,
        conns: 2,
        rooms_created: 1,
        rooms_destroyed: 0,
        rooms_died: 0,
        joins: 2,
        leaves: 0,
        opens: 2,
        closes: 0,
        metrics_dropped: 0,
        join_ops_dropped: 0,
        close_ops_dropped: 0,
        team_relays_dropped_full: 0,
        team_relays_dropped_closed: 0,
        joins_unread: 0,
        team_exports_unread: 0,
    }));
    acc.report(t)
}

fn config(l: &TcpListener, interval: Duration) -> OtlpConfig {
    OtlpConfig {
        endpoint: format!("http://{}", l.local_addr().expect("bound")),
        interval,
        service_name: "gsb-e2e".to_owned(),
    }
}

/// The counter a push carries for the exporter's own health.
fn health(req: &proto::ExportMetricsServiceRequest, name: &str) -> String {
    match find(req, name).data.as_ref() {
        Some(Data::Sum(s)) => number(&s.data_points[0]),
        other => panic!("{name} is not a sum: {other:?}"),
    }
}

#[tokio::test]
async fn a_due_report_arrives_as_otlp_protobuf() {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let (mut exporter, pusher) = otlp(&config(&l, Duration::from_secs(5))).expect("config");
    tokio::spawn(pusher.run());
    exporter.export(&report(Instant::now()));

    let (head, req) = receive(&l, "200 OK").await;
    assert!(head.starts_with("POST /v1/metrics HTTP/1.1\r\n"), "{head}");
    assert!(head.contains("\r\nContent-Type: application/x-protobuf\r\n"));
    let resource = req.resource_metrics[0].resource.as_ref().expect("resource");
    assert_eq!(attrs(&resource.attributes), "service.name=gsb-e2e");
    let Some(Data::Sum(steps)) = find(&req, "gsb_room_steps").data.as_ref() else {
        panic!("steps is a sum");
    };
    assert!(steps.is_monotonic);
    let p = &steps.data_points[0];
    assert_eq!(
        (attrs(&p.attributes), number(p)),
        ("room=r4".into(), "int 30".into())
    );
    assert!(p.time_unix_nano >= p.start_time_unix_nano && p.start_time_unix_nano > 0);
    let Some(Data::Gauge(members)) = find(&req, "gsb_room_members").data.as_ref() else {
        panic!("members is a gauge");
    };
    assert_eq!(number(&members.data_points[0]), "double 2");
    let Some(Data::Gauge(rooms)) = find(&req, "gsb_registry_rooms").data.as_ref() else {
        panic!("registry rooms is a gauge");
    };
    assert_eq!(number(&rooms.data_points[0]), "int 1");
    assert_eq!(health(&req, "gsb_export_otlp_push_failures"), "int 0");
    assert_eq!(health(&req, "gsb_export_otlp_reports_dropped"), "int 0");
}

/// A 503, then a peer that accepts and never answers (the push times
/// out after one interval), then a 200: the third push carries both
/// failures, and each push was one report (nothing retried).
#[tokio::test]
async fn failed_pushes_are_counted_into_the_next_one() {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let interval = Duration::from_millis(300);
    let (mut exporter, pusher) = otlp(&config(&l, interval)).expect("config");
    tokio::spawn(pusher.run());
    let t0 = Instant::now();

    exporter.export(&report(t0));
    let (_, first) = receive(&l, "503 Service Unavailable").await;
    assert_eq!(health(&first, "gsb_export_otlp_push_failures"), "int 0");

    exporter.export(&report(t0 + interval));
    let (mut silent, _) = l.accept().await.expect("the second push connects");
    let _ = read_request(&mut silent).await;
    exporter.export(&report(t0 + interval * 2));
    let (_, third) = receive(&l, "200 OK").await;
    assert_eq!(health(&third, "gsb_export_otlp_push_failures"), "int 2");
    assert_eq!(
        exporter.dropped(),
        0,
        "every due report found the slot free"
    );
    drop(silent);
}

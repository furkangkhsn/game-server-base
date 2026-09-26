//! `[metrics.otlp]` end to end: a build without the `otlp` feature
//! refuses the table at startup by name; a build with it pushes the
//! server's own reports to an OTLP/HTTP receiver (an in-test one that
//! decodes the protobuf), and refuses an endpoint it cannot push to.
//! The example config's commented section parses as documented.

use gsb_server::{Config, OtlpSection, ServerError};

const EXAMPLE: &str = include_str!("../../../config.example.toml");

fn with_otlp(endpoint: &str, interval_secs: u64) -> Config {
    let mut cfg = Config {
        bind: "127.0.0.1:0".into(),
        room_count: 1,
        ..Default::default()
    };
    cfg.metrics.otlp = Some(OtlpSection {
        endpoint: endpoint.into(),
        interval_secs,
        service_name: "gsb-e2e".into(),
    });
    cfg
}

/// The example's commented `[metrics.otlp]` block, uncommented, is the
/// table this grammar reads (with the documented values).
#[test]
fn the_example_otlp_section_parses_as_documented() {
    let block: String = EXAMPLE
        .lines()
        .skip_while(|l| *l != "#[metrics.otlp]")
        .take_while(|l| l.starts_with('#') && !l.starts_with("# ──"))
        .filter(|l| !l.starts_with("# "))
        .map(|l| format!("{}\n", l.trim_start_matches('#')))
        .collect();
    let cfg: Config = toml::from_str(&block).expect("the uncommented block parses");
    let otlp = cfg.metrics.otlp.expect("the table");
    assert_eq!(otlp.endpoint, "http://127.0.0.1:4318/v1/metrics");
    assert_eq!(otlp.interval_secs, 10);
    assert_eq!(otlp.service_name, "gsb");
}

#[cfg(not(feature = "otlp"))]
#[tokio::test]
async fn an_otlp_table_refuses_startup_without_the_feature() {
    let err = gsb_server::start_server(with_otlp("http://127.0.0.1:4318", 10))
        .await
        .err()
        .expect("startup refused");
    assert!(matches!(err, ServerError::OtlpNotBuilt), "{err}");
    assert!(err.to_string().contains("`otlp` cargo feature"), "{err}");
}

#[cfg(feature = "otlp")]
mod pushing {
    use super::*;
    use gsb_core::metrics::otlp::proto::{self, metric::Data, number_data_point::Value};
    use prost::Message;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Accept one push, answer 200, decode its body.
    async fn receive(l: &TcpListener) -> proto::ExportMetricsServiceRequest {
        let (mut s, _) = tokio::time::timeout(Duration::from_secs(10), l.accept())
            .await
            .expect("a push within the interval")
            .expect("accept");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let (head_end, len) = loop {
            let n = s.read(&mut chunk).await.expect("read");
            assert!(n > 0, "request cut short");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).into_owned();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .expect("length")
                    .parse::<usize>()
                    .expect("numeric");
                break (i + 4, len);
            }
        };
        while buf.len() < head_end + len {
            let n = s.read(&mut chunk).await.expect("read body");
            assert!(n > 0, "body cut short");
            buf.extend_from_slice(&chunk[..n]);
        }
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("answer");
        let body = &buf[head_end..head_end + len];
        proto::ExportMetricsServiceRequest::decode(body).expect("protobuf")
    }

    fn int_of(req: &proto::ExportMetricsServiceRequest, name: &str) -> Option<i64> {
        let metrics = &req.resource_metrics[0].scope_metrics[0].metrics;
        let m = metrics.iter().find(|m| m.name == name)?;
        let points = match m.data.as_ref()? {
            Data::Gauge(g) => &g.data_points,
            Data::Sum(s) => &s.data_points,
            Data::Histogram(_) => return None,
        };
        match points.first()?.value {
            Some(Value::AsInt(v)) => Some(v),
            _ => None,
        }
    }

    /// The server's own reports reach the receiver: the registry's room,
    /// then (once the room has sampled) the room's steps.
    #[tokio::test]
    async fn the_server_pushes_its_reports_to_an_otlp_receiver() {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}/v1/metrics", l.local_addr().expect("bound"));
        let handle = gsb_server::start_server(with_otlp(&endpoint, 1))
            .await
            .expect("server starts");
        let (mut rooms, mut steps) = (None, None);
        for _ in 0..8 {
            let req = receive(&l).await;
            let resource = req.resource_metrics[0].resource.as_ref().expect("resource");
            assert_eq!(resource.attributes[0].key, "service.name");
            rooms = int_of(&req, "gsb_registry_rooms");
            steps = int_of(&req, "gsb_room_steps");
            if rooms == Some(1) && steps.is_some_and(|s| s > 0) {
                break;
            }
        }
        assert_eq!(rooms, Some(1), "the registry's room arrived");
        assert!(
            steps.is_some_and(|s| s > 0),
            "the room's steps arrived: {steps:?}"
        );
        handle.stop().await;
    }

    #[tokio::test]
    async fn an_endpoint_it_cannot_push_to_refuses_startup() {
        for (endpoint, interval) in [("https://127.0.0.1:4318", 10), ("http://127.0.0.1:4318", 0)] {
            let err = gsb_server::start_server(with_otlp(endpoint, interval))
                .await
                .err()
                .expect("startup refused");
            assert!(matches!(err, ServerError::BadOtlp(_)), "{err}");
        }
    }
}

//! A tracing subscriber that captures warn events, so a test can
//! assert on the warning a room emits rather than on its side effects.

use super::*;

/// Lock-free WARN-capturing subscriber: events are pushed over an mpsc
/// channel (never blocking); no shared state to protect. The `warn!`
/// macro carries its text in a `message` field, so the field list is
/// the log line.
pub(in crate::room::tests) struct WarnCapture {
    pub(in crate::room::tests) tx: mpsc::Sender<String>,
}

impl tracing::Subscriber for WarnCapture {
    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        *meta.level() == tracing::Level::WARN
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        // The room code creates no spans; placeholder never used.
        tracing::span::Id::from_non_zero_u64(std::num::NonZeroU64::MIN)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields: Vec<(String, String)> = Vec::new();
        event.record(&mut WarnFieldSink {
            fields: &mut fields,
        });
        let line = fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = self.tx.try_send(line);
    }
}

pub(in crate::room::tests) struct WarnFieldSink<'a> {
    fields: &'a mut Vec<(String, String)>,
}

impl tracing::field::Visit for WarnFieldSink<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

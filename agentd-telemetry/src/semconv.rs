//! OpenTelemetry semantic conventions for `CloudEvents` and the daemon's spans.
//!
//! The daemon acts as the producer for every event it appends, but the trace
//! context originates elsewhere — a client, an approval flow, or the session
//! supervisor. Following the `CloudEvents` semantic conventions, a received event
//! is never allowed to rewrite the trace context it carried: it is attached to
//! the current span as a *link*, and the context of the work that *emits* an
//! event is injected into it as the `traceparent` extension.

use agentd_events::{Event, Traceparent};
use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId};
use opentelemetry_sdk::propagation::TraceContextPropagator;

pub use opentelemetry::KeyValue as Attribute;

/// The `cloudevents.event_id` attribute.
pub const EVENT_ID: &str = "cloudevents.event_id";
/// The `cloudevents.event_type` attribute.
pub const EVENT_TYPE: &str = "cloudevents.event_type";
/// The `cloudevents.event_source` attribute.
pub const EVENT_SOURCE: &str = "cloudevents.event_source";
/// The `cloudevents.event_subject` attribute.
pub const EVENT_SUBJECT: &str = "cloudevents.event_subject";

/// The W3C `traceparent` header name the propagator injects.
const TRACEPARENT_HEADER: &str = agentd_events::TRACEPARENT_ATTR;

/// Converts a validated [`Traceparent`] into an OpenTelemetry [`SpanContext`].
///
/// Returns `None` when an id is the invalid all-zero value, which the parser
/// already rejects but OpenTelemetry would otherwise treat as no context.
#[must_use]
pub fn remote_span_context(traceparent: &Traceparent) -> Option<SpanContext> {
    let trace_id = TraceId::from_hex(traceparent.trace_id()).ok()?;
    let span_id = SpanId::from_hex(traceparent.span_id()).ok()?;
    if trace_id == TraceId::INVALID || span_id == SpanId::INVALID {
        return None;
    }
    Some(SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::new(traceparent.flags()),
        true,
        opentelemetry::trace::TraceState::default(),
    ))
}

/// The `cloudevents.*` span attributes for `event`.
#[must_use]
pub fn event_values(event: &Event) -> Vec<KeyValue> {
    let mut values = vec![
        KeyValue::new(EVENT_ID, event.id.clone()),
        KeyValue::new(EVENT_TYPE, event.r#type.clone()),
        KeyValue::new(EVENT_SOURCE, event.source.clone()),
    ];
    if let Some(subject) = &event.subject {
        values.push(KeyValue::new(EVENT_SUBJECT, subject.clone()));
    }
    values
}

/// The `traceparent` of the current span, for injecting into an event the daemon
/// is about to produce.
///
/// Returns `None` when there is no valid active span. The value is taken from
/// the active context regardless of the sampled flag, so an event can be
/// correlated even when the trace is not exported.
#[must_use]
pub fn current_traceparent() -> Option<Traceparent> {
    let mut carrier = std::collections::HashMap::<String, String>::new();
    TraceContextPropagator::new().inject_context(&opentelemetry::Context::current(), &mut carrier);
    let header = carrier.remove(TRACEPARENT_HEADER)?;
    Traceparent::parse(&header).ok()
}

/// Attaches `event`'s remote trace context to the current span as a link.
///
/// This is the consumer side of the convention: the event's context is not
/// adopted as a parent and is not modified, only correlated.
pub fn link_event(event: &Event) {
    let Some(traceparent) = event.traceparent() else {
        return;
    };
    link_traceparent(&traceparent, event_values(event));
}

/// Attaches a remote trace context to the current span as a link, with `values`
/// describing what it came from.
pub fn link_traceparent(
    traceparent: &Traceparent,
    values: Vec<KeyValue>,
) {
    use opentelemetry::trace::TraceContextExt as _;
    let Some(span_context) = remote_span_context(traceparent) else {
        return;
    };
    opentelemetry::Context::current()
        .span()
        .add_link(span_context, values);
}

#[cfg(test)]
mod tests {
    use super::{current_traceparent, event_values, link_event, remote_span_context};
    use agentd_events::{Event, Traceparent};
    use serde_json::json;

    fn traceparent() -> Traceparent {
        Traceparent::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .expect("valid")
    }

    #[test]
    fn converts_a_traceparent_into_a_remote_span_context() {
        let context = remote_span_context(&traceparent()).expect("context");

        assert!(context.is_remote());
        assert!(context.is_sampled());
        assert_eq!(
            context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(context.span_id().to_string(), "00f067aa0ba902b7");
    }

    #[test]
    fn event_values_carry_the_cloud_events_attributes() {
        let event = Event::new("session.started", json!({})).with_subject("session:s1");
        let values = event_values(&event);

        let names: Vec<&str> = values.iter().map(|value| value.key.as_str()).collect();
        assert!(names.contains(&"cloudevents.event_id"));
        assert!(names.contains(&"cloudevents.event_type"));
        assert!(names.contains(&"cloudevents.event_source"));
        assert!(names.contains(&"cloudevents.event_subject"));
    }

    #[test]
    fn no_active_span_means_no_current_traceparent() {
        assert_eq!(current_traceparent(), None);
    }

    #[test]
    fn linking_an_event_without_a_traceparent_is_a_noop() {
        link_event(&Event::new("agent.inbox", json!({})));
    }

    #[test]
    fn a_link_is_recorded_on_the_active_span() {
        use opentelemetry::trace::{TraceContextExt as _, Tracer as _, TracerProvider as _};
        use opentelemetry_sdk::trace::InMemorySpanExporterBuilder;

        let exporter = InMemorySpanExporterBuilder::new().build();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("test");

        // Make a span current and attach a link to it.
        let span = tracer.start("test.span");
        let context = opentelemetry::Context::current_with_span(span);
        let guard = context.clone().attach();
        link_event(&Event::new("agent.inbox", json!({})).with_traceparent(&traceparent()));
        context.span().end();
        drop(guard);

        let spans = exporter.get_finished_spans().expect("finished spans");
        let recorded = spans
            .iter()
            .find(|span| span.name == "test.span")
            .expect("the span must be exported");
        assert_eq!(recorded.links.len(), 1, "the event link must be recorded");
        assert_eq!(
            recorded.links[0].span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }
}

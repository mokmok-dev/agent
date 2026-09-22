---
type: Design
title: telemetry
description: W3C Trace Context propagation on the CloudEvents envelope and OpenTelemetry export. traceparent を拡張属性としてイベントに載せ、受信側は link として相関し、OTLP/HTTP で collector へ送る。サンドボックス内の子はネットワークを持たないため、trace はイベント経由でホスト側へ渡る。
tags:
  - telemetry
  - opentelemetry
  - tracing
  - traceparent
  - cloudevents
generated:
  by: human
  at: 2026-09-21T00:00:00Z
---

# agentd telemetry design

The event log records what happened; telemetry records the journey that produced
it. This document fixes how the two connect: a W3C Trace Context `traceparent`
travels as a `CloudEvents` extension attribute, and each process exports its
spans over OTLP. It closes the observability gap recorded in
[reconciliation](reconciliation.md).

The nouns are fixed in [vocabulary](vocabulary.md).

## The carrier: a traceparent extension

An event may carry a `traceparent` extension attribute
(`agentd-events/src/trace.rs`), the W3C Trace Context header value:

```text
version-trace-id-parent-id-trace-flags
00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
```

Only version `00` is accepted, ids must be non-zero hex of the documented
length, and unknown flag bits are zeroed on parse. A value is validated on
ingress (`Event::validate`) and canonicalised before it is stored, so the log
never holds a malformed or non-lowercase value. The attribute is optional and
omitted when absent, so existing events on the wire are unaffected.

It is the `CloudEvents` Distributed Tracing Extension. Because CloudEvents
attributes are not protocol headers, the context travels with the event across
hops and is the *starting* trace of the transmission, not each hop's own span.

## The convention: link, never adopt

The daemon is the producer of every event it appends, but the trace context
originates in the client, the approval flow, or the session manager.
Following the `CloudEvents` span semantic conventions
(`agentd-telemetry/src/semconv.rs`):

- A **received** event's `traceparent` is attached to the current span as a
  **link**, not adopted as a parent, and never rewritten. The event is a
  checkpoint on someone else's journey.
- An event the daemon **emits** is stamped with the current span's context, so
  its downstream consumers can continue the trace.

`remote_span_context` converts a parsed `Traceparent` into an OpenTelemetry
`SpanContext`; `current_traceparent` extracts the active context for injection.

## The egress path

A confined child has no IP route (`docs/egress.md`), so it cannot push spans to
a collector. Trace context therefore leaves the sandbox **through the event
log**, not through the network: the child stamps its events, the daemon receives
them over its Unix socket and links their context to its own spans, and the
daemon — which may reach the network — exports the combined trace.

The egress proxy follows the same rule: a `CONNECT` carrying a `traceparent`
header (`agentd/src/proxy.rs`) has that context linked to the proxy's connection
span, and the `session.egress.requested`/`denied` approval events carry it, so a
request, its HitL approval, and the tunnel share one trace.

For a link to be recorded, the receiving task needs an active span. The inbound
handler and the proxy connection each enter one (`event.inbound`,
`egress.proxy`), so a received context has somewhere to attach.

## Export

`agentd-telemetry` installs the tracing subscriber for the daemon and every node
binary. It always emits the JSON log layer and adds an OpenTelemetry export
layer only when a collector is configured, following the standard environment
variables:

- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`: the traces endpoint, used verbatim;
- `OTEL_EXPORTER_OTLP_ENDPOINT`: the base collector, to which the exporter
  appends `/v1/traces`;
- `OTEL_SERVICE_NAME`: the reported service name (default `agentd`).

With none set, no exporter is created and no spans are sent, so the default is
inert. Spans are batched on the process's tokio runtime (the default batch
processor's own thread has no reactor for the async HTTP client) and flushed by
`Telemetry::shutdown` at exit.

The dev shell ships `opentelemetry-collector` and `nix/otelcol.yaml`, whose
debug exporter prints spans to its stdout: point the daemon's
`OTEL_EXPORTER_OTLP_ENDPOINT` at `127.0.0.1:4318` and run `otelcol --config
nix/otelcol.yaml` to see a trace. Jaeger is not in nixpkgs; any OTLP endpoint can
replace the debug exporter.

## What is traced

- Each agent turn is an `agent.turn` span; the `agent.*` events it publishes
  carry that span's context.
- Each inbound event is handled under an `event.inbound` span that links the
  event's received context.
- Each egress proxy connection is an `egress.proxy` span that links the child's
  context and stamps the approval events.

## Testing

- `agentd-events`: parser shape, version, hex length, zero-id rejection,
  uppercase normalisation, reserved-flag masking, `FromStr`.
- `agentd-telemetry`: endpoint resolution (verbatim traces variable vs appending
  base variable), the link actually recorded on an active span, and an
  end-to-end `OTLP` export test that stands up a receiver and asserts a `POST`
  with a protobuf body.
- `agentd`: a valid traceparent is preserved on the appended event and a
  malformed one is rejected; the child's traceparent reaches the approval event.

## Stated gaps

- Only the sampled flag is carried; `tracestate` is not propagated.
- A confined child's own spans are not exported (it has no network); its trace
  appears through the daemon's links, and joins fully only if something gives
  the child a collector.
- Spans are sampled by the collector's default (`ParentBased(AlwaysOn)`); no
  sampler is configured.

//! OpenTelemetry tracing export and `CloudEvents` semantic conventions shared
//! by the daemon and its node binaries.
//!
//! Tracing is exported over OTLP/HTTP to a collector (Jaeger accepts OTLP
//! directly) when an OTLP endpoint is configured, and is inert otherwise: the
//! process's JSON logs are always emitted, and a deployment that names no
//! collector pays for no exporter. The endpoint and resource are read from the
//! standard OpenTelemetry environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, `OTEL_SERVICE_NAME`), so no new flag is
//! introduced.
//!
//! [`semconv`] follows the `CloudEvents` span conventions: a received
//! `traceparent` is correlated with the current span as a link, never adopted as
//! a parent, and the context of the work that emits an event is injected into
//! it.

pub mod semconv;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use thiserror::Error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The environment variable naming the OTLP collector endpoint.
const OTLP_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
/// The environment variable naming the OTLP traces endpoint, which takes
/// precedence over [`OTLP_ENDPOINT`].
const OTLP_TRACES_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";
/// The default service name reported to the collector.
const DEFAULT_SERVICE_NAME: &str = "agentd";

/// Why telemetry initialization failed.
#[derive(Debug, Error)]
pub enum TelemetryError {
    /// The OTLP exporter could not be built.
    #[error("failed to build the OTLP span exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
}

/// The live tracing provider, kept so it can be shut down explicitly at exit
/// (dropping it does not flush buffered spans).
#[derive(Debug)]
pub struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    /// Flushes buffered spans and stops the exporter.
    pub fn shutdown(&self) {
        if let Err(error) = self.provider.shutdown() {
            eprintln!("failed to shut down the trace exporter: {error}");
        }
    }
}

/// Installs the tracing subscriber: always the JSON log layer, plus an
/// OpenTelemetry export layer when a collector is configured.
///
/// `default_filter` is the [`EnvFilter`] directive used when `RUST_LOG` is
/// unset, so each binary keeps its own diagnostic target.
///
/// Returns `None` when no OTLP endpoint is set, so there is no exporter to shut
/// down.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when a collector is configured but the
/// exporter cannot be built.
pub fn init(default_filter: &str) -> Result<Option<Telemetry>, TelemetryError> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let json_layer = tracing_subscriber::fmt::layer().json();
    let base = tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer);

    if !collector_configured() {
        base.init();
        return Ok(None);
    }

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource())
        .build();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer(DEFAULT_SERVICE_NAME);

    base.with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();
    Ok(Some(Telemetry { provider }))
}

/// Whether an OTLP endpoint is named in the environment.
fn collector_configured() -> bool {
    [OTLP_TRACES_ENDPOINT, OTLP_ENDPOINT]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// The resource describing this process, with the service name from
/// `OTEL_SERVICE_NAME` or the default.
fn resource() -> Resource {
    let name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| String::from(DEFAULT_SERVICE_NAME));
    Resource::builder().with_service_name(name).build()
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SERVICE_NAME, OTLP_ENDPOINT, OTLP_TRACES_ENDPOINT, collector_configured};

    #[test]
    fn no_endpoint_means_no_exporter() {
        // The test binary does not set the OTLP variables; assert the negative
        // branch so the default stays inert.
        if std::env::var(OTLP_ENDPOINT).is_err() && std::env::var(OTLP_TRACES_ENDPOINT).is_err() {
            assert!(!collector_configured());
        }
    }

    #[test]
    fn the_default_service_name_is_agentd() {
        assert_eq!(DEFAULT_SERVICE_NAME, "agentd");
    }
}

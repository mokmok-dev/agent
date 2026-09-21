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
use opentelemetry_otlp::WithExportConfig as _;
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
    ///
    /// The cause is a boxed error rather than the exporter's own type, so this
    /// crate does not force consumers to depend on `opentelemetry-otlp` to read
    /// or match the failure.
    #[error("failed to build the OTLP span exporter: {0}")]
    Exporter(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl From<opentelemetry_otlp::ExporterBuildError> for TelemetryError {
    fn from(error: opentelemetry_otlp::ExporterBuildError) -> Self {
        Self::Exporter(Box::new(error))
    }
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
/// The endpoint follows the OpenTelemetry environment-variable rules:
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, when set, names the traces endpoint
/// verbatim; otherwise `OTEL_EXPORTER_OTLP_ENDPOINT` names the base collector
/// and the `/v1/traces` path is appended by the exporter. With neither set there
/// is no exporter and no spans are sent.
///
/// Returns `None` when no OTLP endpoint is set, so there is no exporter to shut
/// down.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when a collector is configured but the
/// exporter cannot be built.
///
/// # Panics
///
/// Panics if a global tracing subscriber is already installed, like
/// `tracing_subscriber::util::SubscriberInitExt::init`. Call it once, at startup.
pub fn init(default_filter: &str) -> Result<Option<Telemetry>, TelemetryError> {
    match endpoint_choice(
        non_empty_env(OTLP_TRACES_ENDPOINT),
        non_empty_env(OTLP_ENDPOINT),
    ) {
        Endpoint::Unset => {
            install_logs_only(default_filter);
            Ok(None)
        },
        Endpoint::FromEnvironment => install(default_filter, None).map(Some),
        Endpoint::Verbatim(endpoint) => install(default_filter, Some(&endpoint)).map(Some),
    }
}

/// Which endpoint to give the exporter.
enum Endpoint {
    /// Neither variable is set: no exporter.
    Unset,
    /// Only the base variable is set: let the exporter resolve it and append
    /// `/v1/traces`.
    FromEnvironment,
    /// The traces variable is set: use it verbatim.
    Verbatim(String),
}

/// Chooses the endpoint from the two variables, preserving the distinction
/// between "use this URL verbatim" and "let the exporter resolve the base URL".
fn endpoint_choice(
    traces: Option<String>,
    base: Option<String>,
) -> Endpoint {
    match (traces, base) {
        (None, None) => Endpoint::Unset,
        (Some(traces), _) => Endpoint::Verbatim(traces),
        (None, Some(_)) => Endpoint::FromEnvironment,
    }
}

/// Installs the subscriber with a **verbatim** traces endpoint, for tests and
/// callers that hold a complete URL (including the `/v1/traces` path).
///
/// This is not the base collector URL: unlike [`init`], the path is not
/// appended. For a base URL, set `OTEL_EXPORTER_OTLP_ENDPOINT` and call
/// [`init`] instead.
///
/// # Errors
///
/// Returns [`TelemetryError::Exporter`] when the exporter cannot be built.
///
/// # Panics
///
/// Panics if a global tracing subscriber is already installed; see [`init`].
pub fn init_with_traces_endpoint(
    default_filter: &str,
    endpoint: &str,
) -> Result<Telemetry, TelemetryError> {
    install(default_filter, Some(endpoint))
}

/// Installs the log-only subscriber when no collector is configured.
fn install_logs_only(default_filter: &str) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let json_layer = tracing_subscriber::fmt::layer().json();
    tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer)
        .init();
}

/// Installs the JSON log layer plus the OTLP layer, with `traces_endpoint`
/// passed verbatim when `Some` and resolved from the environment otherwise.
fn install(
    default_filter: &str,
    traces_endpoint: Option<&str>,
) -> Result<Telemetry, TelemetryError> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let json_layer = tracing_subscriber::fmt::layer().json();
    let base = tracing_subscriber::registry()
        .with(env_filter)
        .with(json_layer);

    let builder = opentelemetry_otlp::SpanExporter::builder().with_http();
    let exporter = match traces_endpoint {
        Some(endpoint) => builder.with_endpoint(endpoint).build()?,
        None => builder.build()?,
    };
    // The default batch processor batches on its own thread, which has no tokio
    // reactor for the async HTTP client; the async-runtime processor batches on
    // the runtime that is driving the process.
    let processor =
        opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
            exporter,
            opentelemetry_sdk::runtime::Tokio,
        )
        .build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource())
        .build();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    opentelemetry::global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer(DEFAULT_SERVICE_NAME);

    base.with(tracing_opentelemetry::layer().with_tracer(tracer))
        .init();
    Ok(Telemetry { provider })
}

/// The value of `name` when it is set and not blank.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
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
    use super::{
        DEFAULT_SERVICE_NAME, Endpoint, OTLP_ENDPOINT, OTLP_TRACES_ENDPOINT, endpoint_choice,
        non_empty_env,
    };

    #[test]
    fn no_endpoint_means_no_exporter() {
        // The test binary does not set the OTLP variables; assert the negative
        // branch so the default stays inert.
        if std::env::var(OTLP_ENDPOINT).is_err() && std::env::var(OTLP_TRACES_ENDPOINT).is_err() {
            assert!(non_empty_env(OTLP_ENDPOINT).is_none());
            assert!(non_empty_env(OTLP_TRACES_ENDPOINT).is_none());
        }
    }

    #[test]
    fn the_traces_variable_is_passed_verbatim() {
        let choice = endpoint_choice(
            Some(String::from("http://collector:4318/v1/traces")),
            Some(String::from("http://ignored:4318")),
        );
        let Endpoint::Verbatim(endpoint) = choice else {
            unreachable!("the traces variable wins");
        };
        assert_eq!(endpoint, "http://collector:4318/v1/traces");
    }

    #[test]
    fn the_base_variable_is_left_for_the_exporter_to_resolve() {
        // `FromEnvironment` means "do not pass an endpoint", so the exporter
        // appends `/v1/traces` to the base variable itself.
        assert!(matches!(
            endpoint_choice(None, Some(String::from("http://collector:4318"))),
            Endpoint::FromEnvironment
        ));
    }

    #[test]
    fn neither_variable_means_no_exporter() {
        assert!(matches!(endpoint_choice(None, None), Endpoint::Unset));
    }

    #[test]
    fn the_default_service_name_is_agentd() {
        assert_eq!(DEFAULT_SERVICE_NAME, "agentd");
    }
}

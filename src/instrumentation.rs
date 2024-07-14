use std::{str::FromStr, time::Duration};

use anyhow::Result;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    resource::{EnvResourceDetector, SdkProvidedResourceDetector, TelemetryResourceDetector},
    trace::Tracer,
    Resource,
};
use tonic::metadata::MetadataMap;
use tracing::{info_span, Dispatch, Level, Span, Subscriber};
use tracing_subscriber::{
    fmt, layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt, EnvFilter, Layer,
    Registry,
};

use crate::config::LogConfig;

pub struct TracingGuard;
impl Drop for TracingGuard {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

pub fn init_tracing(config: &LogConfig) -> Result<(TracingGuard, Span)> {
    let filter = get_env_filter(config)?;
    if config.json {
        let subscriber = Registry::default().with(fmt::layer().json().with_filter(filter));
        init_opentelemetry(config, subscriber)?;
    } else {
        let subscriber = Registry::default().with(fmt::layer().compact().with_filter(filter));
        init_opentelemetry(config, subscriber)?;
    }

    Ok((
        TracingGuard,
        info_span!("oracles", version = env!("CARGO_PKG_VERSION")),
    ))
}

fn init_opentelemetry<S>(config: &LogConfig, subscriber: S) -> Result<()>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    match config.otlp_endpoint.as_ref() {
        Some(endpoint) => {
            let filter = get_env_filter(config)?;
            let tracer = init_tracer(endpoint, config.uptrace_dsn.as_ref())?;
            let layer = tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(filter);
            Dispatch::new(subscriber.with(layer)).init();
        }
        None => {
            Dispatch::new(subscriber).init();
        }
    };
    Ok(())
}

fn get_env_filter(config: &LogConfig) -> Result<EnvFilter> {
    let level = Level::from_str(&config.level)?;
    Ok(EnvFilter::builder()
        .with_default_directive(level.into())
        .from_env_lossy())
}

fn init_tracer(endpoint: &str, uptrace_dsn: Option<&String>) -> Result<Tracer> {
    opentelemetry::global::set_error_handler(|error| {
        tracing::error!("OpenTelemetry error occurred: {:#}", anyhow::anyhow!(error),);
    })?;

    let resource = Resource::from_detectors(
        Duration::from_secs(0),
        vec![
            Box::new(SdkProvidedResourceDetector),
            Box::new(EnvResourceDetector::new()),
            Box::new(TelemetryResourceDetector),
        ],
    );

    let mut metadata = MetadataMap::with_capacity(1);
    if let Some(dsn) = uptrace_dsn {
        metadata.insert("uptrace-dsn", dsn.parse()?);
    }

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(endpoint)
                .with_timeout(Duration::from_secs(5))
                .with_tls_config(Default::default())
                .with_metadata(metadata),
        )
        .with_batch_config(
            opentelemetry_sdk::trace::BatchConfigBuilder::default()
                .with_max_queue_size(30000)
                .with_max_export_batch_size(10000)
                .with_scheduled_delay(Duration::from_millis(5000))
                .build(),
        )
        .with_trace_config(opentelemetry_sdk::trace::config().with_resource(resource))
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    Ok(tracer)
}

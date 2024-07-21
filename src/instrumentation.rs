use std::time::Duration;

use anyhow::Result;
use opentelemetry::{global, trace::TracerProvider, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{self, Config},
    Resource,
};
use tonic::metadata::MetadataMap;
use tracing::{Dispatch, Level, Subscriber};
use tracing_subscriber::{
    filter::Targets, fmt, layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt,
    Layer, Registry,
};

use crate::{config::LogConfig, network::NodeId};

pub struct TracingGuard;
impl Drop for TracingGuard {
    fn drop(&mut self) {
        opentelemetry::global::shutdown_tracer_provider();
    }
}

pub fn init_tracing(config: &LogConfig) -> Result<TracingGuard> {
    let filter = get_filter(config);
    if config.json {
        let subscriber = Registry::default().with(fmt::layer().json().with_filter(filter));
        init_opentelemetry(config, subscriber)?;
    } else {
        let subscriber = Registry::default().with(fmt::layer().compact().with_filter(filter));
        init_opentelemetry(config, subscriber)?;
    }

    Ok(TracingGuard)
}

fn init_opentelemetry<S>(config: &LogConfig, subscriber: S) -> Result<()>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    match config.otlp_endpoint.as_ref() {
        Some(endpoint) => {
            let filter = get_filter(config);
            let provider = init_tracer_provider(
                &config.id,
                &config.label,
                endpoint,
                config.uptrace_dsn.as_ref(),
            )?;
            let tracer = provider.tracer("oracle");
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

fn get_filter(config: &LogConfig) -> Targets {
    Targets::new()
        .with_default(Level::INFO)
        .with_target("oracles", config.level)
}

fn init_tracer_provider(
    id: &NodeId,
    name: &str,
    endpoint: &str,
    uptrace_dsn: Option<&String>,
) -> Result<trace::TracerProvider> {
    opentelemetry::global::set_error_handler(|error| {
        tracing::error!("OpenTelemetry error occurred: {:#}", anyhow::anyhow!(error),);
    })?;

    let resource = Resource::default().merge(&Resource::new([
        KeyValue::new("service.name", name.to_string()),
        KeyValue::new("service.namespace", "oracles"),
        KeyValue::new("service.instance.id", id.to_string()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]));

    let mut metadata = MetadataMap::with_capacity(1);
    if let Some(dsn) = uptrace_dsn {
        metadata.insert("uptrace-dsn", dsn.parse()?);
    }

    let provider = opentelemetry_otlp::new_pipeline()
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
        .with_trace_config(Config::default().with_resource(resource))
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    global::set_tracer_provider(provider.clone());

    Ok(provider)
}

use std::time::Duration;

use anyhow::{Context, Result};
use opentelemetry::{global, trace::TracerProvider, KeyValue};
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{metrics, trace, Resource};
use tonic::{metadata::MetadataMap, transport::ClientTlsConfig};
use tracing::{Dispatch, Level, Subscriber};
use tracing_subscriber::{
    filter::Targets, fmt, layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt,
    Layer, Registry,
};

use crate::{config::LogConfig, network::NodeId};

pub enum OtelGuard {
    Enabled(metrics::SdkMeterProvider),
    Disabled,
}
impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Self::Enabled(meter_provider) = &self {
            if let Err(err) = meter_provider.shutdown() {
                eprintln!("{:?}", err);
            }
            global::shutdown_tracer_provider();
        }
    }
}

pub fn init_tracing(config: &LogConfig) -> Result<OtelGuard> {
    let filter = get_filter(config.level);
    if config.json {
        let subscriber = Registry::default().with(fmt::layer().json().with_filter(filter));
        init_opentelemetry(config, subscriber)
    } else {
        let subscriber = Registry::default().with(fmt::layer().compact().with_filter(filter));
        init_opentelemetry(config, subscriber)
    }
}

fn init_opentelemetry<S>(config: &LogConfig, subscriber: S) -> Result<OtelGuard>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    match config.otlp_endpoint.as_ref() {
        Some(endpoint) => {
            let uptrace_dsn = config.uptrace_dsn.as_ref();
            let (tracer_provider, meter_provider) =
                init_providers(&config.id, &config.label, endpoint, uptrace_dsn)?;

            let tracer = tracer_provider.tracer("oracle");
            let tracer_layer = tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(get_filter(Level::DEBUG));

            let metrics_layer = tracing_opentelemetry::MetricsLayer::new(meter_provider.clone());

            Dispatch::new(subscriber.with(metrics_layer).with(tracer_layer)).init();
            Ok(OtelGuard::Enabled(meter_provider))
        }
        None => {
            Dispatch::new(subscriber).init();
            Ok(OtelGuard::Disabled)
        }
    }
}

fn get_filter(level: Level) -> Targets {
    Targets::new()
        .with_default(Level::INFO)
        .with_target("oracles", level)
}

fn init_providers(
    id: &NodeId,
    name: &str,
    endpoint: &str,
    uptrace_dsn: Option<&String>,
) -> Result<(trace::TracerProvider, metrics::SdkMeterProvider)> {
    let resource = Resource::default().merge(&Resource::new([
        KeyValue::new("service.name", name.to_string()),
        KeyValue::new("service.namespace", "oracles"),
        KeyValue::new("service.instance.id", id.to_string()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ]));

    let exporter_provider = ExporterProvider::new(endpoint, uptrace_dsn)?;

    let tracer_provider = init_tracer_provider(resource.clone(), &exporter_provider)?;
    let meter_provider = init_meter_provider(resource, &exporter_provider)?;

    Ok((tracer_provider, meter_provider))
}

fn init_tracer_provider(
    resource: Resource,
    exporter_provider: &ExporterProvider,
) -> Result<trace::TracerProvider> {
    let exporter = exporter_provider.span()?;
    let processor = trace::BatchSpanProcessor::builder(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_batch_config(
            opentelemetry_sdk::trace::BatchConfigBuilder::default()
                .with_max_queue_size(30000)
                .with_max_export_batch_size(10000)
                .with_scheduled_delay(Duration::from_millis(5000))
                .build(),
        )
        .build();

    let provider = trace::TracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource)
        .build();

    global::set_tracer_provider(provider.clone());

    Ok(provider)
}

fn init_meter_provider(
    resource: Resource,
    exporter_provider: &ExporterProvider,
) -> Result<metrics::SdkMeterProvider> {
    let exporter = exporter_provider.metric()?;
    let reader =
        metrics::PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio).build();
    let provider = metrics::SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();

    global::set_meter_provider(provider.clone());

    Ok(provider)
}

struct ExporterProvider {
    endpoint: String,
    metadata: MetadataMap,
}
impl ExporterProvider {
    fn new(endpoint: &str, uptrace_dsn: Option<&String>) -> Result<Self> {
        let mut metadata = MetadataMap::with_capacity(1);
        if let Some(dsn) = uptrace_dsn {
            metadata.insert("uptrace-dsn", dsn.parse()?);
        };
        Ok(Self {
            endpoint: endpoint.to_string(),
            metadata,
        })
    }

    fn span(&self) -> Result<SpanExporter> {
        SpanExporter::builder()
            .with_tonic()
            .with_compression(opentelemetry_otlp::Compression::Gzip)
            .with_endpoint(&self.endpoint)
            .with_timeout(Duration::from_secs(5))
            .with_tls_config(ClientTlsConfig::new().with_webpki_roots())
            .with_metadata(self.metadata.clone())
            .build()
            .context("could not build span exporter")
    }

    fn metric(&self) -> Result<MetricExporter> {
        MetricExporter::builder()
            .with_tonic()
            .with_compression(opentelemetry_otlp::Compression::Gzip)
            .with_endpoint(&self.endpoint)
            .with_timeout(Duration::from_secs(5))
            .with_tls_config(ClientTlsConfig::new().with_webpki_roots())
            .with_metadata(self.metadata.clone())
            .build()
            .context("could not build metric exporter")
    }
}

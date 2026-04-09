use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;
use opentelemetry::{KeyValue, global, trace::TracerProvider};
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{Resource, error::OTelSdkResult, metrics, trace};
use tonic::{metadata::MetadataMap, transport::ClientTlsConfig};
use tracing::{Dispatch, Level, Subscriber};
use tracing_subscriber::{
    Layer, Registry, filter::Targets, fmt, layer::SubscriberExt, registry::LookupSpan,
    util::SubscriberInitExt,
};

use crate::{
    config::{LogConfig, OtlpEndpoint},
    network::NodeId,
};

pub enum OtelGuard {
    Enabled(trace::SdkTracerProvider, metrics::SdkMeterProvider),
    Disabled,
}
impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Self::Enabled(tracer_provider, meter_provider) = &self {
            if let Err(err) = meter_provider.shutdown() {
                eprintln!("{:?}", err);
            }
            if let Err(err) = tracer_provider.shutdown() {
                eprintln!("{:?}", err);
            }
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
    if !config.otlp.is_empty() {
        let (tracer_provider, meter_provider) =
            init_providers(&config.id, &config.label, &config.otlp)?;

        let tracer = tracer_provider.tracer("oracle");
        let tracer_layer = tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(get_filter(Level::DEBUG));

        let metrics_layer = tracing_opentelemetry::MetricsLayer::new(meter_provider.clone());

        Dispatch::new(subscriber.with(metrics_layer).with(tracer_layer)).init();
        Ok(OtelGuard::Enabled(tracer_provider, meter_provider))
    } else {
        Dispatch::new(subscriber).init();
        Ok(OtelGuard::Disabled)
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
    endpoints: &[OtlpEndpoint],
) -> Result<(trace::SdkTracerProvider, metrics::SdkMeterProvider)> {
    let resource = Resource::builder()
        .with_attributes([
            KeyValue::new("service.name", name.to_string()),
            KeyValue::new("service.namespace", "oracles"),
            KeyValue::new("service.instance.id", id.to_string()),
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        ])
        .build();

    let exporter_provider = MultiplexingExporterProvider::new(endpoints)?;

    let tracer_provider = init_tracer_provider(resource.clone(), &exporter_provider)?;
    let meter_provider = init_meter_provider(resource, &exporter_provider)?;

    Ok((tracer_provider, meter_provider))
}

fn init_tracer_provider(
    resource: Resource,
    exporter_provider: &MultiplexingExporterProvider,
) -> Result<trace::SdkTracerProvider> {
    let exporter = exporter_provider.span()?;
    let processor = trace::BatchSpanProcessor::builder(exporter)
        .with_batch_config(
            trace::BatchConfigBuilder::default()
                .with_max_queue_size(30000)
                .with_max_export_batch_size(10000)
                .with_scheduled_delay(Duration::from_millis(5000))
                .build(),
        )
        .build();

    let provider = trace::SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource)
        .build();

    global::set_tracer_provider(provider.clone());

    Ok(provider)
}

fn init_meter_provider(
    resource: Resource,
    exporter_provider: &MultiplexingExporterProvider,
) -> Result<metrics::SdkMeterProvider> {
    let exporter = exporter_provider.metric()?;
    let provider = metrics::SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource)
        .build();

    global::set_meter_provider(provider.clone());

    Ok(provider)
}

struct MultiplexingExporterProvider {
    exporters: Vec<ExporterProvider>,
}
impl MultiplexingExporterProvider {
    fn new(endpoints: &[OtlpEndpoint]) -> Result<Self> {
        let mut exporters = vec![];
        for endpoint in endpoints {
            exporters.push(ExporterProvider::new(endpoint)?);
        }
        Ok(Self { exporters })
    }

    fn span(&self) -> Result<MultiplexingSpanExporter> {
        let mut span_exporters = vec![];
        for exporter in &self.exporters {
            span_exporters.push(exporter.span()?);
        }
        Ok(MultiplexingSpanExporter(span_exporters))
    }

    fn metric(&self) -> Result<MultiplexingMetricExporter> {
        let mut metric_exporters = vec![];
        for exporter in &self.exporters {
            metric_exporters.push(exporter.metric()?);
        }
        Ok(MultiplexingMetricExporter(metric_exporters))
    }
}

#[derive(Debug)]
struct MultiplexingSpanExporter(Vec<SpanExporter>);
impl trace::SpanExporter for MultiplexingSpanExporter {
    async fn export(&self, batch: Vec<trace::SpanData>) -> OTelSdkResult {
        let results = join_all(self.0.iter().map(|e| e.export(batch.clone()))).await;
        for result in results {
            result?;
        }
        Ok(())
    }
}

struct MultiplexingMetricExporter(Vec<MetricExporter>);
impl metrics::exporter::PushMetricExporter for MultiplexingMetricExporter {
    async fn export(&self, metrics: &metrics::data::ResourceMetrics) -> OTelSdkResult {
        let results = join_all(self.0.iter().map(|e| e.export(metrics))).await;
        for result in results {
            result?;
        }
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        for exporter in &self.0 {
            exporter.force_flush()?
        }
        Ok(())
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        for exporter in &self.0 {
            exporter.shutdown_with_timeout(timeout)?
        }
        Ok(())
    }

    fn temporality(&self) -> metrics::Temporality {
        self.0.first().unwrap().temporality()
    }
}

struct ExporterProvider {
    endpoint: String,
    metadata: MetadataMap,
}
impl ExporterProvider {
    fn new(endpoint: &OtlpEndpoint) -> Result<Self> {
        let mut metadata = MetadataMap::with_capacity(1);
        if let Some(dsn) = &endpoint.uptrace_dsn {
            metadata.insert("uptrace-dsn", dsn.parse()?);
        };
        Ok(Self {
            endpoint: endpoint.endpoint.clone(),
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

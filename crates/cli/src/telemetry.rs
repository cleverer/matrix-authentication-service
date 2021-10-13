// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use opentelemetry::{
    global,
    sdk::{
        self,
        metrics::{self, PushController},
        trace::{self, Tracer},
        Resource,
    },
};
use opentelemetry_semantic_conventions as semcov;

pub fn setup() -> anyhow::Result<(Tracer, PushController)> {
    global::set_error_handler(|e| tracing::error!("{}", e))?;

    Ok((tracer()?, meter()?))
}

pub fn shutdown() {
    global::shutdown_tracer_provider();
}

fn tracer() -> anyhow::Result<Tracer> {
    let exporter = opentelemetry_otlp::new_exporter().tonic();

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(trace_config())
        .install_batch(opentelemetry::runtime::Tokio)?;

    Ok(tracer)
}

fn interval(duration: Duration) -> impl Stream<Item = tokio::time::Instant> {
    // Skip first immediate tick from tokio
    opentelemetry::util::tokio_interval_stream(duration).skip(1)
}

fn meter() -> anyhow::Result<PushController> {
    let exporter = opentelemetry_otlp::new_exporter().tonic();

    let meter = opentelemetry_otlp::new_pipeline()
        .metrics(tokio::spawn, interval)
        .with_exporter(exporter)
        .with_aggregator_selector(metrics::selectors::simple::Selector::Exact)
        .build()?;

    Ok(meter)
}

fn trace_config() -> trace::Config {
    trace::config().with_resource(resource())
}

fn resource() -> Resource {
    let resource = Resource::new(vec![
        semcov::resource::SERVICE_NAME.string(env!("CARGO_PKG_NAME")),
        semcov::resource::SERVICE_VERSION.string(env!("CARGO_PKG_VERSION")),
    ]);

    let detected = Resource::from_detectors(
        Duration::from_secs(5),
        vec![
            Box::new(sdk::resource::EnvResourceDetector::new()),
            Box::new(sdk::resource::OsResourceDetector),
            Box::new(sdk::resource::ProcessResourceDetector),
        ],
    );

    resource.merge(&detected)
}
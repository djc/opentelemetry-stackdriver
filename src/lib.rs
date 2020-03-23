/*
   Copyright 2020 Vivint Smarthome

   Licensed under the Apache License, Version 2.0 (the "License");
   you may not use this file except in compliance with the License.
   You may obtain a copy of the License at

       http://www.apache.org/licenses/LICENSE-2.0

   Unless required by applicable law or agreed to in writing, software
   distributed under the License is distributed on an "AS IS" BASIS,
   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
   See the License for the specific language governing permissions and
   limitations under the License.
*/

#![cfg(not(doctest))]
// unfortunately the proto code includes comments from the google proto files
// that are interpreted as "doc tests" and will fail to build.

use derivative::Derivative;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use opentelemetry::api::core::Value;
use opentelemetry::exporter::trace::{ExportResult, SpanData, SpanExporter};
use std::any::Any;
use std::sync::Arc;
use tonic::transport::Channel; // ClientTlsConfig};

pub mod proto {
    pub mod google {
        pub mod api {
            tonic::include_proto!("google.api");
        }
        pub mod devtools {
            pub mod cloudtrace {
                pub mod v2 {
                    tonic::include_proto!("google.devtools.cloudtrace.v2");
                }
            }
        }
        pub mod protobuf {
            tonic::include_proto!("google.protobuf");
        }
        pub mod rpc {
            tonic::include_proto!("google.rpc");
        }
    }
}

use proto::google::devtools::cloudtrace::v2::span::time_event::Annotation;
use proto::google::devtools::cloudtrace::v2::span::TimeEvent;
use proto::google::devtools::cloudtrace::v2::trace_service_client::TraceServiceClient;
use proto::google::devtools::cloudtrace::v2::{AttributeValue, TruncatableString};

/// Exports opentelemetry tracing spans to Google StackDriver.
///
/// As of the time of this writing, the opentelemetry crate exposes no link information
/// so this struct does not send link information.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct StackDriverExporter {
    #[derivative(Debug = "ignore")]
    tx: futures::channel::mpsc::Sender<Vec<Arc<SpanData>>>,
}

impl StackDriverExporter {
    pub async fn connect<S: futures::task::Spawn>(
        project_name: impl Into<String>,
        spawn: &S,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let uri = http::uri::Uri::from_static("https://cloudtrace.googleapis.com:443");
        let channel = Channel::builder(uri).connect().await?;
        let (tx, rx) = futures::channel::mpsc::channel(64);
        spawn.spawn_obj(
            Box::new(Self::export_inner(
                TraceServiceClient::new(channel),
                project_name.into(),
                rx,
            ))
            .into(),
        )?;

        Ok(Self { tx })
    }

    async fn export_inner(
        mut client: TraceServiceClient<Channel>,
        project_name: String,
        mut rx: futures::channel::mpsc::Receiver<Vec<Arc<SpanData>>>,
    ) {
        while let Some(batch) = rx.next().await {
            use proto::google::devtools::cloudtrace::v2::span::time_event::Value;
            use proto::google::devtools::cloudtrace::v2::span::{Attributes, TimeEvents};
            use proto::google::devtools::cloudtrace::v2::{BatchWriteSpansRequest, Span};

            let spans = batch
                .iter()
                .map(|span| {
                    let new_attributes = Attributes {
                        attribute_map: span
                            .attributes
                            .iter()
                            .map(|kv| {
                                (
                                    kv.key.inner().clone().into_owned(),
                                    attribute_value_conversion(kv.value.clone()),
                                )
                            })
                            .collect(),
                        ..Default::default()
                    };
                    let new_time_events = TimeEvents {
                        time_event: span
                            .message_events
                            .iter()
                            .map(|event| TimeEvent {
                                time: Some(event.timestamp.into()),
                                value: Some(Value::Annotation(Annotation {
                                    description: Some(to_truncate(event.message.clone())),
                                    ..Default::default()
                                })),
                            })
                            .collect(),
                        ..Default::default()
                    };

                    Span {
                        name: format!(
                            "projects/{}/traces/{}/spans/{}",
                            project_name,
                            hex::encode(span.context.trace_id().to_be_bytes()),
                            hex::encode(span.context.span_id().to_be_bytes())
                        ),
                        display_name: Some(to_truncate(span.name.clone())),
                        span_id: hex::encode(span.context.span_id().to_be_bytes()),
                        parent_span_id: hex::encode(span.parent_span_id.to_be_bytes()),
                        start_time: Some(span.start_time.into()),
                        end_time: Some(span.end_time.into()),
                        attributes: Some(new_attributes),
                        time_events: Some(new_time_events),
                        ..Default::default()
                    }
                })
                .collect::<Vec<_>>();

            // let mut req = BatchWriteSpansRequest::default();
            let req = BatchWriteSpansRequest {
                name: format!("projects/{}", project_name),
                spans,
            };
            client
                .batch_write_spans(req)
                .await
                .map_err(|e| {
                    log::error!("StackDriver push failed {:?}", e);
                })
                .ok(); // TODO: run this
        }
    }
}

impl SpanExporter for StackDriverExporter {
    /// # Safety
    ///
    /// Panics if called outside of executor.
    fn export(&self, batch: Vec<Arc<SpanData>>) -> ExportResult {
        match futures::executor::block_on(self.tx.clone().send(batch)) {
            Err(e) => {
                log::error!(
                    "Unable to send to export_inner; this should never occur {:?}",
                    e
                );
                ExportResult::FailedNotRetryable
            }
            _ => ExportResult::Success,
        }
    }

    fn shutdown(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn attribute_value_conversion(v: Value) -> AttributeValue {
    use proto::google::devtools::cloudtrace::v2::attribute_value;
    let new_value = match v {
        Value::Bool(v) => attribute_value::Value::BoolValue(v),
        Value::Bytes(v) => attribute_value::Value::StringValue(to_truncate(hex::encode(&v))),
        Value::F64(v) => attribute_value::Value::StringValue(to_truncate(v.to_string())),
        Value::I64(v) => attribute_value::Value::IntValue(v),
        Value::String(v) => attribute_value::Value::StringValue(to_truncate(v)),
        Value::U64(v) => attribute_value::Value::IntValue(v as i64),
    };
    AttributeValue {
        value: Some(new_value),
    }
}

fn to_truncate(s: String) -> TruncatableString {
    TruncatableString {
        value: s,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let mut rt = tokio::runtime::Runtime::new().unwrap();
        let tp = futures::executor::ThreadPool::new().unwrap();
        rt.block_on(StackDriverExporter::connect("fake-project", &tp))
            .unwrap();
    }
}

//! Persist accepted payloads independently of frame routing.
use super::*;
use crate::{
    job::output::CaptureCollector,
    media::{Image, ImageRef, MAX_IMAGE_BYTES},
    remote::{
        flow::{CHUNK_BYTES, Credits, WINDOW},
        protocol::{ImageId, PayloadEvent, PayloadId, PayloadOpen, RemoteToolOutput},
    },
    tool::{
        diagnostic::{FailureSite, Operation, PartialContext, Subject},
        output::FieldPointer,
        output::{CaptureEvent, OutputEvent, OutputSink},
        source::{Source, Spool},
    },
};
use tokio::{sync::mpsc, task::JoinSet};

#[derive(Debug)]
pub(super) struct ReceivedResult(pub(super) Result<Received, RemoteError>);

/// A completed remote call, with the spooled contents of a source read.
#[derive(Debug)]
pub(super) struct Received {
    pub(super) output: ToolOutput,
    pub(super) source: Option<Source>,
}

enum Message {
    Payload(PayloadEvent, tokio::sync::OwnedSemaphorePermit),
    Terminal,
}

enum Transfer {
    Receiving(mpsc::Sender<Message>),
    Finishing,
}

#[derive(Default)]
pub(super) struct Results {
    transfers: HashMap<RequestId, Transfer>,
    admission: Credits,
    stopped: tokio_util::sync::CancellationToken,
    pub(super) tasks: JoinSet<(RequestId, Result<ReceivedResult, RemoteError>)>,
}

impl Results {
    pub(super) async fn payload(
        &mut self,
        state: &Mutex<ConnectionState>,
        writer: &Arc<Mutex<RequestWriter>>,
        request_id: RequestId,
        event: PayloadEvent,
    ) -> Result<(), RemoteError> {
        let bytes = match &event {
            PayloadEvent::Data { data, .. }
            | PayloadEvent::Capture(CaptureEvent::Write { data, .. }) => data.len(),
            PayloadEvent::Capture(CaptureEvent::Open { field, .. }) => field.as_str().len(),
            PayloadEvent::Open(PayloadOpen::Image { file, .. }) => {
                file.as_ref().map_or(0, String::len)
            }
            _ => 0,
        };
        if bytes > CHUNK_BYTES {
            return Err(protocol("oversized payload chunk or metadata"));
        }
        let permit = self
            .admission
            .reserve()
            .map_err(|_| protocol("payload flow-control overflow"))?;
        if let std::collections::hash_map::Entry::Vacant(entry) = self.transfers.entry(request_id) {
            let (context, destination) = state
                .lock()
                .await
                .pending
                .get(&request_id)
                .map(|pending| (pending.context.clone(), pending.destination.clone()))
                .ok_or_else(|| protocol("payload for unknown request"))?;
            let (sender, receiver) = mpsc::channel(WINDOW + 1);
            let writer = writer.clone();
            let stopped = self.stopped.clone();
            self.tasks.spawn(async move {
                let result = Ingestion::new(&context)
                    .run(context, destination, writer, receiver, stopped)
                    .await
                    .map_err(|error| host_output_error(error, Operation::Receive));
                (request_id, result)
            });
            entry.insert(Transfer::Receiving(sender));
        }
        let Some(Transfer::Receiving(sender)) = self.transfers.get(&request_id) else {
            return Err(protocol("payload after terminal response"));
        };
        sender
            .try_send(Message::Payload(event, permit))
            .map_err(|_| protocol("payload flow-control overflow"))
    }

    pub(super) fn terminal(&mut self, request_id: RequestId) -> Result<(), RemoteError> {
        let Some(transfer) = self.transfers.get_mut(&request_id) else {
            return Err(protocol("terminal response for unknown request"));
        };
        let Transfer::Receiving(sender) = std::mem::replace(transfer, Transfer::Finishing) else {
            return Err(protocol("duplicate terminal response"));
        };
        sender
            .try_send(Message::Terminal)
            .map_err(|_| protocol("payload flow-control overflow"))
    }

    pub(super) async fn shutdown(&mut self, state: &Mutex<ConnectionState>) {
        self.stopped.cancel();
        self.transfers.clear();
        while let Some(completed) = self.tasks.join_next().await {
            let _ = self.complete(state, completed).await;
        }
    }

    pub(super) async fn complete(
        &mut self,
        state: &Mutex<ConnectionState>,
        completed: Result<(RequestId, Result<ReceivedResult, RemoteError>), tokio::task::JoinError>,
    ) -> Result<(), RemoteError> {
        let (request_id, result) = completed.map_err(|_| {
            host_output_error(
                RemoteError::ConnectionTask("remote output persistence task failed".into()),
                Operation::Receive,
            )
        })?;
        self.transfers.remove(&request_id);
        let result = result?;
        let pending = state
            .lock()
            .await
            .pending
            .remove(&request_id)
            .ok_or_else(|| protocol("response for unknown request"))?;
        let _ = pending.sender.send(Ok(result));
        Ok(())
    }
}

fn host_output_error(error: impl Into<RemoteError>, operation: Operation) -> RemoteError {
    error.into().or(
        PartialContext::new(operation, Subject::Label("remote output".into()))
            .at(FailureSite::Host),
    )
}

fn protocol(message: &'static str) -> RemoteError {
    ProtocolError::Violation(message).into()
}

enum ImagePayload {
    Receiving {
        file: Option<String>,
        bytes: Vec<u8>,
    },
    Invalid,
    Finished(Option<ImageRef>),
}

#[derive(Default)]
enum ResultPayload {
    #[default]
    Absent,
    Receiving(Spool),
    Finished(RemoteToolResult),
}

#[derive(Default)]
enum SourcePayload {
    #[default]
    Absent,
    Receiving(Spool),
    Finished(Source),
}

struct Ingestion {
    captures: Arc<CaptureCollector>,
    images: HashMap<ImageId, ImagePayload>,
    source: SourcePayload,
    result: ResultPayload,
}

impl Ingestion {
    fn new(context: &ToolContext) -> Self {
        Self {
            captures: Arc::new(CaptureCollector::new(
                context.store().clone(),
                context.job(),
            )),
            images: HashMap::new(),
            source: SourcePayload::Absent,
            result: ResultPayload::Absent,
        }
    }

    async fn run(
        mut self,
        context: ToolContext,
        destination: ExecutionLocation,
        writer: Arc<Mutex<RequestWriter>>,
        mut receiver: mpsc::Receiver<Message>,
        stopped: tokio_util::sync::CancellationToken,
    ) -> Result<ReceivedResult, RemoteError> {
        while let Some(message) = receiver.recv().await {
            match message {
                Message::Payload(event, permit) => {
                    self.receive(&context, event).await?;
                    let acknowledging = async {
                        let mut writer = writer.lock().await;
                        drop(permit);
                        write_frame(&mut writer.input, &Request::PayloadAck).await
                    };
                    let acknowledgement = tokio::select! {
                        biased;
                        () = stopped.cancelled() => Ok(()),
                        result = acknowledging => result,
                    };
                    if let Err(error) = acknowledgement {
                        receiver.close();
                        while let Some(message) = receiver.recv().await {
                            if let Message::Payload(event, _) = message {
                                self.receive(&context, event).await?;
                            }
                        }
                        return Err(transport_error(error, Operation::Send));
                    }
                }
                Message::Terminal => return self.finish(&destination),
            }
        }
        Err(protocol("payload stream closed before terminal response"))
    }

    async fn receive(
        &mut self,
        context: &ToolContext,
        event: PayloadEvent,
    ) -> Result<(), RemoteError> {
        match event {
            PayloadEvent::Capture(event) => {
                let result = FieldPointer::result();
                if let CaptureEvent::Open { field, .. } = &event
                    && *field != result
                    && !result.contains(field)
                {
                    return Err(protocol("invalid capture field"));
                }
                let captures = self.captures.clone();
                tokio::task::spawn_blocking(move || captures.send(OutputEvent::Capture(event)))
                    .await
                    .map_err(|_| {
                        host_output_error(
                            RemoteError::ConnectionTask(
                                "remote capture persistence task failed".into(),
                            ),
                            Operation::Capture,
                        )
                    })?
                    .map_err(|error| host_output_error(error, Operation::Capture))?;
            }
            PayloadEvent::Open(PayloadOpen::Image { id, file }) => {
                let std::collections::hash_map::Entry::Vacant(entry) = self.images.entry(id) else {
                    return Err(protocol("duplicate image payload"));
                };
                entry.insert(ImagePayload::Receiving {
                    file,
                    bytes: Vec::new(),
                });
            }
            PayloadEvent::Data {
                id: PayloadId::Image(id),
                data,
            } => match self.images.get_mut(&id) {
                Some(ImagePayload::Receiving { bytes, .. })
                    if bytes.len() + data.len() <= MAX_IMAGE_BYTES as usize =>
                {
                    bytes.extend_from_slice(&data);
                }
                Some(image @ ImagePayload::Receiving { .. }) => *image = ImagePayload::Invalid,
                Some(ImagePayload::Invalid) => {}
                _ => return Err(protocol("data for inactive image")),
            },
            PayloadEvent::Finish {
                id: PayloadId::Image(id),
            } => {
                let image = match self.images.remove(&id) {
                    Some(ImagePayload::Receiving { file, bytes }) => match Image::new(bytes) {
                        Ok(image) => context.store().store_image(file, &image).await.ok(),
                        Err(_) => None,
                    },
                    Some(ImagePayload::Invalid) => None,
                    _ => return Err(protocol("finish for inactive image")),
                };
                self.images.insert(id, ImagePayload::Finished(image));
            }
            PayloadEvent::Open(PayloadOpen::Source) => {
                if !matches!(self.source, SourcePayload::Absent) {
                    return Err(protocol("duplicate source payload"));
                }
                self.source = SourcePayload::Receiving(Spool::new().await?);
            }
            PayloadEvent::Data {
                id: PayloadId::Source,
                data,
            } => {
                let SourcePayload::Receiving(spool) = &mut self.source else {
                    return Err(protocol("data for inactive source"));
                };
                spool
                    .append(&data)
                    .await
                    .map_err(|error| host_output_error(error, Operation::Write))?;
            }
            PayloadEvent::Finish {
                id: PayloadId::Source,
            } => {
                let SourcePayload::Receiving(spool) = std::mem::take(&mut self.source) else {
                    return Err(protocol("finish for inactive source"));
                };
                let source = spool
                    .finish()
                    .await
                    .map_err(|error| host_output_error(error, Operation::Write))?;
                self.source = SourcePayload::Finished(source);
            }
            PayloadEvent::Open(PayloadOpen::Result) => {
                if !matches!(self.result, ResultPayload::Absent) {
                    return Err(protocol("duplicate result payload"));
                }
                self.result = ResultPayload::Receiving(Spool::new().await?);
            }
            PayloadEvent::Data {
                id: PayloadId::Result,
                data,
            } => {
                let ResultPayload::Receiving(spool) = &mut self.result else {
                    return Err(protocol("data for inactive result"));
                };
                spool
                    .append(&data)
                    .await
                    .map_err(|error| host_output_error(error, Operation::WriteCapture))?;
            }
            PayloadEvent::Finish {
                id: PayloadId::Result,
            } => {
                let ResultPayload::Receiving(spool) = std::mem::take(&mut self.result) else {
                    return Err(protocol("finish for inactive result"));
                };
                let file = spool
                    .finish()
                    .await
                    .map_err(|error| host_output_error(error, Operation::FinishCapture))?
                    .reader()
                    .map_err(|error| host_output_error(error, Operation::ReadCapture))?;
                let result = tokio::task::spawn_blocking(move || {
                    serde_json::from_reader(std::io::BufReader::new(file))
                })
                .await
                .map_err(|_| {
                    host_output_error(
                        RemoteError::ConnectionTask("remote result decoding task failed".into()),
                        Operation::Deserialize,
                    )
                })?
                .map_err(|error| {
                    host_output_error(
                        ProtocolError::Decode(error.to_string()),
                        Operation::Deserialize,
                    )
                })?;
                self.result = ResultPayload::Finished(result);
            }
        }
        Ok(())
    }

    fn output(
        &mut self,
        mut output: RemoteToolOutput,
        location: &ExecutionLocation,
    ) -> Result<ToolOutput, RemoteError> {
        if let Some(diagnostic) = &mut output.diagnostic {
            diagnostic.bind_worker(location);
        }
        let captures = self.captures.select(output.captures)?;
        let images = output
            .images
            .into_iter()
            .filter(|image| {
                self.images.values().any(|payload| {
            matches!(payload, ImagePayload::Finished(Some(stored)) if stored == image)
        })
            })
            .collect();
        let mut local = ToolOutput::new(output.value)
            .with_images(images)
            .with_captures(captures);
        local.streams = output.streams;
        local.diagnostic = output.diagnostic.map(Into::into);
        Ok(local)
    }

    fn finish(mut self, location: &ExecutionLocation) -> Result<ReceivedResult, RemoteError> {
        if self
            .images
            .values()
            .any(|image| !matches!(image, ImagePayload::Finished(_)))
        {
            return Err(protocol("terminal response before image completion"));
        }
        let ResultPayload::Finished(result) = std::mem::take(&mut self.result) else {
            return Err(protocol(
                "terminal response without completed result payload",
            ));
        };
        // A failed read may stop its source mid-stream; the partial spool is discarded.
        let source = match (std::mem::take(&mut self.source), result.is_ok()) {
            (SourcePayload::Absent, _) | (SourcePayload::Receiving(_), false) => None,
            (SourcePayload::Receiving(_), true) => {
                return Err(protocol("terminal response before source completion"));
            }
            (SourcePayload::Finished(file), _) => Some(file),
        };
        Ok(match result {
            Ok(output) => ReceivedResult(Ok(Received {
                output: self.output(output, location)?,
                source,
            })),
            Err(mut error) => {
                let error_output = error
                    .output
                    .take()
                    .map(|output| self.output(*output, location))
                    .transpose()?;
                // A shim's internal root (and any other claimed site) is relative
                // to this routed invocation, not evidence of a host-side location.
                error.diagnostic.bind_worker(location);
                ReceivedResult(Err(RemoteError::Remote {
                    diagnostic: Box::new((*error.diagnostic).into()),
                    output: error_output.map(Box::new),
                }))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture_context, route_fixture, test_connection, write_result};
    use super::*;
    use crate::{
        job::output::CaptureKind,
        media::BlobRef,
        tool::{StreamEnd, output::CaptureId},
    };
    use serde_json::json;

    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), future)
            .await
            .unwrap()
    }

    fn open(id: u64, field: &str) -> PayloadEvent {
        PayloadEvent::Capture(CaptureEvent::Open {
            id: CaptureId::new(id).unwrap(),
            field: field.parse().unwrap(),
            kind: CaptureKind::Text,
        })
    }

    fn data(id: u64, bytes: &[u8]) -> PayloadEvent {
        PayloadEvent::Capture(CaptureEvent::Write {
            id: CaptureId::new(id).unwrap(),
            data: bytes.to_vec(),
        })
    }

    fn finish(id: u64) -> PayloadEvent {
        PayloadEvent::Capture(CaptureEvent::Finish {
            id: CaptureId::new(id).unwrap(),
        })
    }

    /// A failed source read may stop mid-stream: its partial spool is dropped
    /// with the call, while a completed call must have finished its source.
    #[tokio::test]
    async fn partial_sources_are_discarded_only_with_failed_calls() {
        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        for failed in [true, false] {
            let mut ingest = Ingestion::new(&context);
            for event in [
                PayloadEvent::Open(PayloadOpen::Source),
                PayloadEvent::Data {
                    id: PayloadId::Source,
                    data: b"partial".to_vec(),
                },
            ] {
                ingest.receive(&context, event).await.unwrap();
            }
            let output = RemoteToolOutput {
                diagnostic: None,
                value: json!(null),
                images: Vec::new(),
                captures: Vec::new(),
                streams: StreamEnd::Finished,
            };
            ingest.result = ResultPayload::Finished(if failed {
                Err(crate::remote::worker::remote_error(
                    crate::tool::invocation::LocalError::cancelled(),
                ))
            } else {
                Ok(output)
            });
            let received = ingest.finish(context.execution_location());
            if failed {
                assert!(matches!(received, Ok(ReceivedResult(Err(_)))));
            } else {
                assert!(
                    received.is_err(),
                    "a completed call left its source unfinished"
                );
            }
        }
    }

    #[tokio::test]
    async fn wire_diagnostics_bind_only_to_the_trusted_invocation_location() {
        use crate::{
            execution::ExecutionLocation,
            tool::diagnostic::{Cause, Diagnostic, IoKind},
        };

        let runtime = crate::tests::TestRuntime::new().await;
        let context = fixture_context(&runtime);
        let trusted =
            ExecutionLocation::named("bastion".parse().unwrap(), "/trusted/workspace".into());
        for claimed in [
            FailureSite::Invocation,
            FailureSite::Host,
            FailureSite::Execution(ExecutionLocation::root("/worker/root".into())),
            FailureSite::Execution(ExecutionLocation::named(
                "forged".parse().unwrap(),
                "/forged".into(),
            )),
        ] {
            for failed in [false, true] {
                let diagnostic = Diagnostic::new(
                    PartialContext::new(Operation::Read, Subject::path("missing"))
                        .at(claimed.clone())
                        .effects(Effects::Unchanged)
                        .resolve(),
                    Cause::Io {
                        kind: IoKind::NotFound,
                        code: Some(2),
                        detail: None,
                    },
                );
                let output = RemoteToolOutput {
                    diagnostic: Some(diagnostic.clone()),
                    value: json!({"kind":"error", "error":{"message":"unrendered"}}),
                    images: Vec::new(),
                    captures: Vec::new(),
                    streams: StreamEnd::Cut,
                };
                let result = if failed {
                    Err(RemoteToolError {
                        diagnostic: Box::new(diagnostic),
                        output: Some(Box::new(output)),
                    })
                } else {
                    Ok(output)
                };
                let mut ingest = Ingestion::new(&context);
                ingest.result = ResultPayload::Finished(
                    serde_json::from_slice(&serde_json::to_vec(&result).unwrap()).unwrap(),
                );
                let output = match ingest.finish(&trusted).unwrap().0 {
                    Ok(received) => received.output,
                    Err(error) => {
                        let (diagnostic, output) = error.into_tool_error().into_parts();
                        assert_eq!(diagnostic.context.operation, Operation::Read);
                        assert_eq!(diagnostic.context.subject, Subject::path("missing"));
                        assert_eq!(diagnostic.context.effects, Effects::Unchanged);
                        assert_eq!(
                            diagnostic.context.site,
                            FailureSite::Execution(trusted.clone())
                        );
                        output.unwrap()
                    }
                };
                assert_eq!(output.streams, StreamEnd::Cut);
                let diagnostic = output.diagnostic.unwrap().resolve();
                assert_eq!(diagnostic.context.operation, Operation::Read);
                assert_eq!(diagnostic.context.subject, Subject::path("missing"));
                assert_eq!(diagnostic.context.effects, Effects::Unchanged);
                assert_eq!(
                    diagnostic.context.site,
                    FailureSite::Execution(trusted.clone())
                );
            }
        }
    }

    #[tokio::test]
    async fn streamed_payloads_preserve_cut_errors_and_only_host_finished_receipts() {
        for (failed, selected) in [
            (false, &[1, 2, 4][..]),
            (true, &[1, 2, 4][..]),
            (false, &[5][..]),
            (false, &[3][..]),
            (false, &[99][..]),
            (false, &[1, 1][..]),
        ] {
            let valid_selection = selected == [1, 2, 4];
            let runtime = crate::tests::TestRuntime::new().await;
            runtime
                .jobs
                .test_create(crate::job::JobSpec::test(runtime.agent.clone(), "payload"))
                .await;
            let context = fixture_context(&runtime);
            let saved = runtime.jobs.output(context.job());
            let mut ingest = Ingestion::new(&context);
            for event in [
                open(1, "/result/content"),
                data(1, b"a unwanted"),
                PayloadEvent::Capture(CaptureEvent::Truncate {
                    id: CaptureId::FIRST,
                    length: 1,
                }),
                data(1, b"\xf0\x9f"),
                data(1, b"\x8c\x8d\n"),
                finish(1),
                open(2, "/result/empty"),
                finish(2),
                open(3, "/result/replaced"),
                data(3, b"discard"),
                PayloadEvent::Capture(CaptureEvent::Discard {
                    id: CaptureId::new(3).unwrap(),
                }),
                open(4, "/result/replaced"),
                data(4, b"new"),
                finish(4),
                open(5, "/result/partial"),
                data(5, b"retained prefix"),
            ] {
                ingest.receive(&context, event).await.unwrap();
            }
            let png = crate::tests::png(b"remote source bytes");
            let valid = ImageRef {
                file: Some("source.png".into()),
                format: png.format(),
                blob: BlobRef::of(png.bytes()),
            };
            let invalid = ImageRef {
                file: None,
                format: png.format(),
                blob: BlobRef::of(b"not an image"),
            };
            let id = |id| ImageId(std::num::NonZeroU64::new(id).unwrap());
            let mut payloads = vec![
                (valid.file.clone(), png.bytes().to_vec()),
                (None, b"not an image".to_vec()),
            ];
            if !failed && valid_selection {
                payloads.push((None, vec![0; MAX_IMAGE_BYTES as usize + 1]));
            }
            for (index, (file, bytes)) in payloads.iter().enumerate() {
                let id = id(index as u64 + 1);
                let events = std::iter::once(PayloadEvent::Open(PayloadOpen::Image {
                    id,
                    file: file.clone(),
                }))
                .chain(bytes.chunks(CHUNK_BYTES).map(|data| {
                    PayloadEvent::Data {
                        id: PayloadId::Image(id),
                        data: data.to_vec(),
                    }
                }));
                for event in events {
                    ingest.receive(&context, event).await.unwrap();
                }
            }
            if !failed && valid_selection {
                assert!(matches!(ingest.images[&id(3)], ImagePayload::Invalid));
            }
            for index in 1..=payloads.len() {
                ingest
                    .receive(
                        &context,
                        PayloadEvent::Finish {
                            id: PayloadId::Image(id(index as u64)),
                        },
                    )
                    .await
                    .unwrap();
            }
            let large = if failed {
                "x".repeat(17 * 1024 * 1024)
            } else {
                String::new()
            };
            let output = RemoteToolOutput {
                diagnostic: None,
                value: json!({"content":"", "empty":"", "replaced":"", "partial":"", "large": large}),
                images: vec![valid.clone(), invalid],
                captures: selected
                    .iter()
                    .map(|&id| CaptureId::new(id).unwrap())
                    .collect(),
                streams: StreamEnd::Cut,
            };
            let result = if failed {
                Err(RemoteToolError {
                    diagnostic: Box::new(
                        crate::tool::ToolError::failed("failure with output").diagnostic(),
                    ),
                    output: Some(Box::new(output)),
                })
            } else {
                Ok(output)
            };
            ingest
                .receive(&context, PayloadEvent::Open(PayloadOpen::Result))
                .await
                .unwrap();
            for data in serde_json::to_vec(&result).unwrap().chunks(CHUNK_BYTES) {
                ingest
                    .receive(
                        &context,
                        PayloadEvent::Data {
                            id: PayloadId::Result,
                            data: data.to_vec(),
                        },
                    )
                    .await
                    .unwrap();
            }
            ingest
                .receive(
                    &context,
                    PayloadEvent::Finish {
                        id: PayloadId::Result,
                    },
                )
                .await
                .unwrap();
            let received = ingest.finish(context.execution_location());
            if !valid_selection {
                assert!(received.is_err());
                continue;
            }
            let output = match received.unwrap().0 {
                Ok(received) if !failed => received.output,
                Err(RemoteError::Remote {
                    output: Some(output),
                    ..
                }) if failed => *output,
                other => panic!("unexpected result: {other:?}"),
            };
            assert_eq!(output.value["large"].as_str().unwrap(), large);
            assert_eq!(output.streams, StreamEnd::Cut);
            assert_eq!(output.images, vec![valid]);
            assert_eq!(output.captures.len(), 3);
            for field in ["/result/content", "/result/empty", "/result/replaced"] {
                assert!(
                    output
                        .captures
                        .iter()
                        .any(|capture| capture.matches(context.job(), field))
                );
            }
            assert_eq!(
                saved.test_bytes("/result/content").unwrap(),
                "a🌍\n".as_bytes()
            );
            assert_eq!(saved.test_bytes("/result/empty").unwrap(), b"");
            assert_eq!(saved.test_bytes("/result/replaced").unwrap(), b"new");
            assert_eq!(
                saved.test_bytes("/result/partial").unwrap(),
                b"retained prefix"
            );
        }
    }

    #[tokio::test]
    async fn cancelled_call_keeps_late_payloads_until_terminal_and_disconnect_keeps_prefixes() {
        for terminal in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            runtime
                .jobs
                .test_create(crate::job::JobSpec::test(runtime.agent.clone(), "payload"))
                .await;
            let context = fixture_context(&runtime);
            let saved = runtime.jobs.output(context.job());
            let connection = Arc::new(test_connection().await);
            let (input, mut requests) = tokio::io::duplex(4096);
            connection.writer.lock().await.input = Box::new(input);
            let call_connection = connection.clone();
            let call_context = context.clone();
            let call = tokio::spawn(async move {
                call_tool(
                    &call_connection,
                    "read".into(),
                    json!({}),
                    &call_context,
                    call_context.execution_location().clone(),
                )
                .await
            });
            assert!(matches!(
                bounded(read_frame::<_, Request>(&mut requests))
                    .await
                    .unwrap(),
                Some(Request::Tool { .. })
            ));
            context.cancellation_token().cancel();
            assert!(matches!(
                bounded(read_frame::<_, Request>(&mut requests))
                    .await
                    .unwrap(),
                Some(Request::Cancel { .. })
            ));
            assert!(matches!(
                bounded(call).await.unwrap(),
                Err(RemoteError::Cancelled)
            ));
            assert!(
                connection
                    .state
                    .lock()
                    .await
                    .pending
                    .contains_key(&RequestId::FIRST)
            );
            let (mut peer, responses) = tokio::io::duplex(4096);
            let state = connection.state.clone();
            let route =
                tokio::spawn(async move { route_fixture(responses, &state, "fixture").await });
            for event in [open(1, "/result/content"), data(1, b"late accepted prefix")] {
                write_frame(
                    &mut peer,
                    &Response::Payload {
                        request_id: RequestId::FIRST,
                        event,
                    },
                )
                .await
                .unwrap();
            }
            if terminal {
                write_result(
                    &mut peer,
                    RequestId::FIRST,
                    Ok(RemoteToolOutput {
                        diagnostic: None,
                        value: json!({}),
                        images: Vec::new(),
                        captures: Vec::new(),
                        streams: StreamEnd::Cut,
                    }),
                )
                .await;
            }
            drop(peer);
            bounded(route).await.unwrap();
            assert!(connection.state.lock().await.pending.is_empty());
            assert_eq!(
                saved.test_bytes("/result/content").unwrap(),
                b"late accepted prefix"
            );
        }
    }
}

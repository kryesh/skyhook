//! Capture streamed result artifacts and assemble chunked tool results.
use super::*;
use crate::job::output::{AsyncCapture, CaptureKind, CompletedCapture};

pub(super) struct ArtifactFrame {
    pub request_id: RequestId,
    pub field: String,
    pub kind: crate::job::output::CaptureKind,
    pub offset: u64,
    pub data: Vec<u8>,
    pub finished: bool,
}

/// Local evidence travels alongside the unchanged wire result, never inside it.
#[derive(Debug)]
pub(super) struct ReceivedResult {
    result: RemoteToolResult,
    captures: Vec<CompletedCapture>,
}

impl ReceivedResult {
    pub(super) async fn into_output(
        self,
        store: &crate::session::SessionStore,
    ) -> Result<ToolOutput, RemoteError> {
        // A bad attached image must not cost either outcome its value, captures
        // or stream-end marker; only that image is dropped.
        match self.result {
            Ok(output) => Ok(output.store(store).await.with_captures(self.captures)),
            Err(error) if error.denial.is_some() => {
                Err(RemoteError::OperationDenied(error.message))
            }
            Err(error) => {
                let output = match error.output {
                    Some(output) => Some(Box::new(
                        output.store(store).await.with_captures(self.captures),
                    )),
                    None => None,
                };
                Err(RemoteError::Remote {
                    message: error.message,
                    output,
                })
            }
        }
    }
}

// The terminal frame consumes the only writer.
struct Artifact {
    kind: CaptureKind,
    offset: u64,
    state: ArtifactState,
}

enum ArtifactState {
    Receiving(AsyncCapture),
    Finished(CompletedCapture),
}

impl Artifact {
    async fn receive(
        mut self,
        kind: CaptureKind,
        offset: u64,
        data: &[u8],
        finished: bool,
    ) -> std::io::Result<Self> {
        // Capture kind is bound by registration, not mutable frame metadata.
        let ArtifactState::Receiving(mut writer) = self.state else {
            return Err(std::io::Error::other("artifact already finished"));
        };
        if offset != self.offset || kind != self.kind {
            return Err(std::io::Error::other("invalid artifact offset or kind"));
        }
        self.offset = self
            .offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| std::io::Error::other("artifact offset overflow"))?;
        writer.write_all(data).await?;
        writer.flush().await?;
        self.state = if finished {
            ArtifactState::Finished(writer.finish().await?)
        } else {
            ArtifactState::Receiving(writer)
        };
        Ok(self)
    }
}

#[derive(Default)]
pub(super) struct Results {
    artifacts: HashMap<(RequestId, String), Artifact>,
    transfers: HashMap<RequestId, (std::fs::File, u64)>,
}
impl Results {
    pub(super) async fn artifact(
        &mut self,
        state: &Mutex<ConnectionState>,
        frame: ArtifactFrame,
    ) -> Result<(), RemoteError> {
        let ArtifactFrame {
            request_id,
            field,
            kind,
            offset,
            data,
            finished,
        } = frame;
        let context = state
            .lock()
            .await
            .pending
            .get(&request_id)
            .map(|pending| pending.context.clone())
            .ok_or_else(|| RemoteError::Protocol("artifact for unknown request".into()))?;
        if !(field == "/error" || field == "/result" || field.starts_with("/result/"))
            || data.len() > 64 * 1024
            || (finished && !data.is_empty())
        {
            return Err(RemoteError::Protocol("invalid artifact frame".into()));
        }
        let key = (request_id, field.clone());
        // Any error below fails the connection, so a removed artifact is never revisited.
        let received = async {
            let artifact = match self.artifacts.remove(&key) {
                Some(artifact) => artifact,
                None if offset != 0 => {
                    return Err(std::io::Error::other("invalid initial artifact offset"));
                }
                None => Artifact {
                    kind,
                    offset: 0,
                    state: ArtifactState::Receiving(
                        context
                            .pending_stream_capture(&field, kind)
                            .await
                            .map_err(|e| std::io::Error::other(e.to_string()))?
                            .open_async(),
                    ),
                },
            };
            let artifact = artifact.receive(kind, offset, &data, finished).await?;
            self.artifacts.insert(key, artifact);
            Ok(())
        }
        .await;
        received.map_err(RemoteError::io)
    }
    pub(super) fn chunk(
        &mut self,
        request_id: RequestId,
        offset: u64,
        data: Vec<u8>,
        finished: bool,
    ) -> std::io::Result<Option<RemoteToolResult>> {
        use std::io::{Seek as _, Write as _};
        if let std::collections::hash_map::Entry::Vacant(entry) = self.transfers.entry(request_id) {
            if offset != 0 {
                return Err(std::io::Error::other("invalid initial result offset"));
            }
            entry.insert((tempfile::tempfile()?, 0));
        }
        let (file, expected) = self
            .transfers
            .get_mut(&request_id)
            .expect("inserted transfer");
        if offset != *expected || data.len() > 64 * 1024 || (finished && !data.is_empty()) {
            return Err(std::io::Error::other("invalid result chunk"));
        }
        file.write_all(&data)?;
        *expected = expected
            .checked_add(data.len() as u64)
            .ok_or_else(|| std::io::Error::other("result offset overflow"))?;
        if !finished {
            return Ok(None);
        }
        let (mut file, _) = self
            .transfers
            .remove(&request_id)
            .expect("completed transfer");
        file.rewind()?;
        Ok(Some(serde_json::from_reader(std::io::BufReader::new(
            file,
        ))?))
    }
    pub(super) async fn finish(
        &mut self,
        state: &Mutex<ConnectionState>,
        request_id: RequestId,
        result: RemoteToolResult,
    ) -> Result<(), RemoteError> {
        if self.artifacts.iter().any(|((id, _), artifact)| {
            *id == request_id && matches!(artifact.state, ArtifactState::Receiving(_))
        }) {
            return Err(RemoteError::Protocol(
                "result before artifact completion".into(),
            ));
        }
        if self.transfers.contains_key(&request_id) {
            return Err(RemoteError::Protocol(
                "result before chunk completion".into(),
            ));
        }
        let pending = state
            .lock()
            .await
            .pending
            .remove(&request_id)
            .ok_or_else(|| {
                RemoteError::Protocol(format!(
                    "response used unknown request ID {}",
                    request_id.get()
                ))
            })?;
        let mut captures: Vec<_> = self
            .artifacts
            .extract_if(|(id, _), _| *id == request_id)
            .filter_map(|(_, artifact)| match artifact.state {
                ArtifactState::Finished(capture) => Some(capture),
                ArtifactState::Receiving(_) => None,
            })
            .collect();
        // A terminal stream frame also transfers incomplete, unreferenced
        // captures. Keep their bytes available for explicit paging, but
        // only carry result-referenced evidence to output projection.
        let value = match &result {
            Ok(output) => Some(&output.value),
            Err(error) => error.output.as_ref().map(|output| &output.value),
        };
        captures.retain(|capture| {
            capture
                .field()
                .strip_prefix("/result")
                .filter(|pointer| pointer.is_empty() || pointer.starts_with('/'))
                .is_some_and(|pointer| value.is_some_and(|value| value.pointer(pointer).is_some()))
        });
        let _ = pending.sender.send(Ok(ReceivedResult { result, captures }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::route_fixture;
    use super::*;
    use crate::job::output::transfer_fields;
    use crate::tool::ToolError;
    use base64::Engine as _;

    fn artifact(
        field: &str,
        kind: CaptureKind,
        offset: u64,
        data: &[u8],
        finished: bool,
    ) -> ArtifactFrame {
        let (request_id, field, data) = (RequestId::FIRST, field.into(), data.into());
        ArtifactFrame {
            request_id,
            field,
            kind,
            offset,
            data,
            finished,
        }
    }

    #[tokio::test]
    async fn owned_artifact_finalizes_exact_bytes_and_materializes_explicit_empty_content() {
        for (field, bytes, kind) in [
            ("/result/content", &b""[..], CaptureKind::Text),
            (
                "/result/stdout",
                &b"a\xf0\x9f\x8c\x8d\n"[..],
                CaptureKind::Unknown,
            ),
            (
                "/result/custom~1partial",
                &b"{\"key\":"[..],
                CaptureKind::Json,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let job = crate::identity::JobId::new(1).unwrap();
            let pending =
                crate::job::output::PendingCapture::create(job, directory.path(), field, kind)
                    .unwrap();
            let state = ArtifactState::Receiving(pending.open_async());
            let mut artifact = Artifact {
                kind,
                offset: 0,
                state,
            };
            // Transport chunks need not align with UTF-8 boundaries and JSON
            // streams may be prefixes only. Neither is a text decoder input.
            for (offset, byte) in bytes.iter().enumerate() {
                artifact = artifact
                    .receive(kind, offset as u64, &[*byte], false)
                    .await
                    .unwrap();
            }
            let end = bytes.len() as u64;
            let artifact = artifact.receive(kind, end, b"", true).await.unwrap();
            let ArtifactState::Finished(capture) = &artifact.state else {
                panic!("missing completion evidence")
            };
            assert!(capture.matches(job, field));
            assert_eq!(capture.kind(), kind);
            // Exactly one field: no absent sibling streams are manufactured.
            let fields = transfer_fields(directory.path()).unwrap();
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0, field);
            assert_eq!(std::fs::read(&fields[0].2).unwrap(), bytes);
            assert!(artifact.receive(kind, end, b"", true).await.is_err());
        }
    }

    #[tokio::test]
    async fn terminal_result_carries_only_referenced_proofs_on_success_and_failure() {
        for failed in [false, true] {
            let runtime = crate::tests::TestRuntime::new().await;
            let context = super::super::tests::fixture_context(&runtime);
            let job = context.job();
            let (sender, receiver) = oneshot::channel();
            let pending = HashMap::from([(RequestId::FIRST, PendingCall { sender, context })]);
            let state = Mutex::new(ConnectionState {
                pending,
                ..Default::default()
            });
            let mut results = Results::default();
            for (field, bytes, kind) in [
                ("/result/content", &b""[..], CaptureKind::Unknown),
                (
                    "/result/custom~1partial",
                    &b"{\"key\":"[..],
                    CaptureKind::Json,
                ),
                ("/error", &b"error prefix"[..], CaptureKind::Text),
            ] {
                results
                    .artifact(&state, artifact(field, kind, 0, bytes, false))
                    .await
                    .unwrap();
                let end = artifact(field, kind, bytes.len() as u64, b"", true);
                results.artifact(&state, end).await.unwrap();
            }
            let value = serde_json::json!({"content": ""});
            // One valid and one undecodable image on a cut-short result: the
            // bad image alone is dropped, never the value, captures or marker.
            let png = crate::tests::png(b"remote result image");
            let image = |data_base64| crate::remote::protocol::RemoteImage {
                file: None,
                data_base64,
            };
            let output = RemoteToolOutput {
                streams: crate::tool::StreamEnd::Cut,
                value: value.clone(),
                images: vec![
                    image(base64::engine::general_purpose::STANDARD.encode(png.bytes())),
                    image(base64::engine::general_purpose::STANDARD.encode(b"not an image")),
                ],
            };
            let completion = if failed {
                let output = Some(Box::new(output));
                Err(RemoteToolError {
                    message: "failed".into(),
                    denial: None,
                    output,
                })
            } else {
                Ok(output)
            };
            results
                .finish(&state, RequestId::FIRST, completion)
                .await
                .unwrap();
            assert!(results.artifacts.is_empty());
            let received = receiver.await.unwrap().unwrap();
            assert_eq!(received.captures.len(), 1);
            assert!(received.captures[0].matches(job, "/result/content"));
            let output = match received.into_output(&runtime.store).await {
                Ok(output) if !failed => output,
                Err(RemoteError::Remote {
                    message,
                    output: Some(output),
                }) if failed => {
                    assert_eq!(message, "failed");
                    *output
                }
                other => panic!("unexpected output: {other:?}"),
            };
            assert_eq!(output.value, value);
            assert_eq!(output.streams, crate::tool::StreamEnd::Cut);
            let [stored] = output.images.as_slice() else {
                panic!("expected only the valid image")
            };
            assert_eq!(stored.blob, crate::media::BlobRef::of(png.bytes()));
            assert_eq!(output.captures.len(), 1);
            assert_eq!(output.captures[0].kind(), CaptureKind::Unknown);
            let fields = transfer_fields(&runtime.jobs.output_directory(job)).unwrap();
            assert_eq!(fields.len(), 3);
            let partial = fields
                .iter()
                .find(|(field, _, _)| field == "/result/custom~1partial");
            assert_eq!(std::fs::read(&partial.unwrap().2).unwrap(), b"{\"key\":");
        }
    }

    #[tokio::test]
    async fn artifact_transfer_hydrates_native_results_and_preserves_interrupted_prefixes() {
        for complete in [true, false] {
            let runtime = crate::tests::TestRuntime::new().await;
            let payload = "line\n".repeat(250_000);
            let source = runtime.root.path().join("payload.txt");
            tokio::fs::write(&source, &payload).await.unwrap();
            // No result/index exists for this small, unfinished JSON capture.
            let captures = runtime.root.path().join("remote-captures");
            let partial = crate::job::output::register_capture(
                &captures,
                "/result/custom~1partial",
                crate::job::output::CaptureKind::Json,
            )
            .unwrap();
            std::fs::write(partial, b"{\"key\":").unwrap();
            let mut builder = crate::tool::ToolRegistryBuilder::default();
            builder.register_dynamic("remote_fixture", "remote fixture", serde_json::json!({"type":"object","properties":{},"additionalProperties":false}), crate::tool::ToolOptions::default().output_schema(serde_json::to_value(schemars::schema_for!(crate::tool::builtins::ProcessOutput)).unwrap()), move |context, _| {
                let source = source.clone();
                let captures = captures.clone();
                let store = context.store().clone();
                async move {
                    let (peer, stream) = tokio::io::duplex(64 * 1024);
                    let (sender, receiver) = oneshot::channel();
                    let state = Arc::new(Mutex::new(ConnectionState { pending:HashMap::from([(RequestId::FIRST, PendingCall {sender,context})]), failure:None,resolutions:HashMap::new(),streams:HashMap::new() }));
                    let reader_state = state.clone();
                    let reader = tokio::spawn(async move { route_fixture(stream, &reader_state, "fixture").await; });
                    let writer = tokio::spawn(async move {
                        let peer = Mutex::new(peer);
                        if complete {
                            let fields = crate::job::output::transfer_fields(&captures).unwrap();
                            assert_eq!(fields.len(), 1);
                            for (field, kind, path) in fields {
                                crate::remote::protocol::write_artifact(&peer, RequestId::FIRST, field, kind, &path).await.unwrap();
                            }
                            crate::remote::protocol::write_artifact(&peer,RequestId::FIRST,"/result/stdout".into(),crate::job::output::CaptureKind::Text,&source).await.unwrap();
                            write_frame(&mut *peer.lock().await,&Response::Tool {request_id:RequestId::FIRST,result:Ok(RemoteToolOutput { streams: Default::default(),value:serde_json::json!({"stdout":"","exit_code":0}),images:Vec::new()})}).await.unwrap();
                        } else {
                            write_frame(&mut *peer.lock().await,&Response::ToolArtifact {request_id:RequestId::FIRST,field:"/result/stdout".into(),kind:crate::job::output::CaptureKind::Text,offset:0,data:b"retained prefix\n".to_vec(),finished:false}).await.unwrap();
                        }
                    });
                    let result = receiver.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    writer.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    reader.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    result.map_err(RemoteError::into_tool_error)?.into_output(&store).await.map_err(RemoteError::into_tool_error)
                }
            }).unwrap();
            let script_slot = Arc::new(std::sync::OnceLock::new());
            crate::tool::builtins::install_script_tool(&mut builder, Arc::downgrade(&script_slot))
                .unwrap();
            let executor = runtime.executor(builder);
            script_slot.set(executor.clone()).ok().unwrap();
            let result = executor
                .execute(
                    runtime.agent.clone(),
                    "remote_fixture",
                    serde_json::json!({}),
                    None,
                )
                .await;
            if complete {
                let result = result.unwrap();
                assert_eq!(result.output.value["stdout"], payload);
                let view = runtime
                    .jobs
                    .present_output(
                        crate::job::output::OutputArgs::new(result.job),
                        &Default::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(view["result"]["stdout"], "line\n".repeat(100));
                assert_eq!(view["result"]["exit_code"], 0);
                assert!(view["result"].get("custom/partial").is_none());
                assert!(view["captures"].as_array().unwrap().iter().any(
                    |capture| capture["field"] == "/result/custom~1partial"
                        && capture["kind"] == "json"
                        && capture["complete"] == false
                ));
                let mut partial = crate::job::output::OutputArgs::new(result.job);
                partial.field = Some("/result/custom~1partial".into());
                let partial = runtime
                    .jobs
                    .present_output(partial, &Default::default())
                    .await
                    .unwrap();
                assert_eq!(partial["preview"]["lines"][0], "{\"key\":");
                assert_eq!(view["truncated"][0]["field"], "/result/stdout");
                let script = executor.execute_model(runtime.agent.clone(), "script", serde_json::json!({
                    "source":"const remote = await tool.remote_fixture({}); if (remote.stdout.length !== 1250000) throw new Error('truncated inside script'); return {remote};"
                }), None).await.unwrap();
                let child = &script.output.value["result"]["value"]["remote"];
                assert_eq!(child["tool"], "remote_fixture");
                assert_eq!(child["result"]["stdout"], "line\n".repeat(100));
                let mut query = crate::job::output::OutputArgs::new(
                    serde_json::from_value(child["id"].clone()).unwrap(),
                );
                query.field = Some(child["truncated"][0]["field"].as_str().unwrap().into());
                query.start = Some(child["truncated"][0]["next_start"].as_u64().unwrap() as usize);
                query.offset =
                    Some(child["truncated"][0]["next_offset"].as_u64().unwrap_or(0) as usize);
                let page = runtime
                    .jobs
                    .present_output(query, &Default::default())
                    .await
                    .unwrap();
                assert_eq!(child["truncated"][0]["next_start"], 101);
                assert_eq!(page["preview"]["lines"][0], "line");
            } else {
                assert!(result.is_err());
                let job = runtime.jobs.list(&runtime.agent).await[0].id;
                let mut args = crate::job::output::OutputArgs::new(job);
                args.field = Some("/result/stdout".into());
                let view = runtime
                    .jobs
                    .present_output(args, &Default::default())
                    .await
                    .unwrap();
                assert_eq!(view["state"], "failed");
                assert_eq!(view["preview"]["lines"][0], "retained prefix");
                assert_eq!(view["notice"], "Output incomplete.");
                assert!(view["preview"].get("capture_complete").is_none());
            }
        }
    }
}

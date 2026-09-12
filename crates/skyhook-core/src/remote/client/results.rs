//! Capture streamed result artifacts and assemble chunked tool results.
use super::*;

pub(super) struct ArtifactFrame {
    pub request_id: u64,
    pub field: String,
    pub kind: crate::job::output::CaptureKind,
    pub offset: u64,
    pub data: Vec<u8>,
    pub finished: bool,
}

#[derive(Default)]
pub(super) struct Results {
    artifacts: HashMap<(u64, String), (tokio::fs::File, u64, bool)>,
    transfers: HashMap<u64, (std::fs::File, u64)>,
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
            .map(|pending| pending.context.clone());
        let context =
            context.ok_or_else(|| RemoteError::Protocol("artifact for unknown request".into()))?;
        if !(field == "/error" || field == "/result" || field.starts_with("/result/"))
            || data.len() > 64 * 1024
            || (finished && !data.is_empty())
        {
            return Err(RemoteError::Protocol("invalid artifact frame".into()));
        }
        let key = (request_id, field.clone());
        let received = async {
            use tokio::io::AsyncWriteExt as _;
            if let std::collections::hash_map::Entry::Vacant(entry) =
                self.artifacts.entry(key.clone())
            {
                if offset != 0 {
                    return Err(std::io::Error::other("invalid initial artifact offset"));
                }
                let path = context
                    .capture_path_with_kind(&field, kind)
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                entry.insert((tokio::fs::File::create(path).await?, 0, false));
            }
            let (file, expected, done) = self.artifacts.get_mut(&key).expect("inserted artifact");
            if *done || offset != *expected {
                return Err(std::io::Error::other("invalid artifact offset"));
            }
            file.write_all(&data).await?;
            file.flush().await?;
            *expected += data.len() as u64;
            if finished {
                file.sync_data().await?;
                *done = true;
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        received.map_err(RemoteError::io)
    }
    pub(super) fn chunk(
        &mut self,
        request_id: u64,
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
        *expected += data.len() as u64;
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
        request_id: u64,
        result: RemoteToolResult,
    ) -> Result<(), RemoteError> {
        if self
            .artifacts
            .iter()
            .any(|((id, _), (_, _, done))| *id == request_id && !done)
        {
            return Err(RemoteError::Protocol(
                "result before artifact completion".into(),
            ));
        }
        self.artifacts.retain(|(id, _), _| *id != request_id);
        let pending = state
            .lock()
            .await
            .pending
            .remove(&request_id)
            .ok_or_else(|| {
                RemoteError::Protocol(format!("response used unknown request ID {request_id}"))
            })?;
        let _ = pending.sender.send(Ok(result));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::route_fixture;
    use super::*;
    use crate::tool::ToolError;
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
                async move {
                    let (peer, stream) = tokio::io::duplex(64 * 1024);
                    let (sender, receiver) = oneshot::channel();
                    let state = Arc::new(Mutex::new(ConnectionState { pending:HashMap::from([(1, PendingCall {sender,context})]), failure:None,resolutions:HashMap::new(),streams:HashMap::new() }));
                    let reader_state = state.clone();
                    let reader = tokio::spawn(async move { route_fixture(stream, &reader_state, "fixture").await; });
                    let writer = tokio::spawn(async move {
                        let peer = Mutex::new(peer);
                        if complete {
                            let fields = crate::job::output::transfer_fields(&captures).unwrap();
                            assert_eq!(fields.len(), 1);
                            for (field, kind, path) in fields {
                                crate::remote::protocol::write_artifact(&peer, 1, field, kind, &path).await.unwrap();
                            }
                            crate::remote::protocol::write_artifact(&peer,1,"/result/stdout".into(),crate::job::output::CaptureKind::Text,&source).await.unwrap();
                            write_frame(&mut *peer.lock().await,&Response::Tool {request_id:1,result:Ok(RemoteToolOutput {value:serde_json::json!({"stdout":"","exit_code":0}),images:Vec::new()})}).await.unwrap();
                        } else {
                            write_frame(&mut *peer.lock().await,&Response::ToolArtifact {request_id:1,field:"/result/stdout".into(),kind:crate::job::output::CaptureKind::Text,offset:0,data:b"retained prefix\n".to_vec(),finished:false}).await.unwrap();
                        }
                    });
                    let result = receiver.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    writer.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    reader.await.map_err(|e| ToolError::Failed(e.to_string()))?;
                    result.map_err(RemoteError::into_tool_error)?.map(Into::into).map_err(|e| ToolError::Failed(e.message))
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

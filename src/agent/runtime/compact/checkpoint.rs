//! Build and atomically install compaction checkpoints against current runtime state.

use super::retention::{included_message_jobs, included_output_jobs, retained_sources};
use super::{HarnessError, SessionRuntime, TurnContext, compaction, context_sources};
use crate::{
    agent::runtime::state,
    identity::JobId,
    provider::{
        ProviderContext,
        protocol::{HistoryLifetime, Message, ModelRequest, ResponseSchema},
    },
    session::{CompactionCheckpoint, ModelCallOrigin, ModelPurpose, SessionEvent, project_history},
};
use std::collections::BTreeSet;

pub(super) struct CompactionInput<'a> {
    pub(super) context: u64,
    pub(super) request: &'a ModelRequest,
    pub(super) max_context: u64,
    pub(super) model_attempt: &'a mut u64,
}

impl SessionRuntime {
    pub(super) async fn compact_inner(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        source: CompactionInput<'_>,
        request_sequence: &mut Option<u64>,
        launches: &mut Vec<(JobId, Option<ModelCallOrigin>)>,
    ) -> Result<(), HarnessError> {
        let CompactionInput {
            context,
            request: input,
            max_context,
            model_attempt,
        } = source;
        let agent = turn.agent;
        let records = self.store.records().await;
        let frontier = records.last().map_or(0, |record| record.sequence);
        let projected = project_history(&records, agent)?;
        let previous = records.iter().rev().find_map(|record| {
            (&record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. }))
                .then_some(record.sequence)
        });
        for launch in self.jobs.active_launches(agent).await {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        // Retry against current history and runtime state, including any todo
        // changes that invalidated an earlier attempt's snapshot.
        let runtime = state::runtime_state_content(
            &self.jobs,
            &self.todos,
            agent,
            &turn.capabilities,
            turn.location,
        )
        .await;
        let mut input = ModelRequest {
            model: input.model.clone(),
            system: input.system.clone(),
            history: crate::session::merge_tool_results(
                projected.iter().map(|(_, message)| message.clone()),
            ),
            // A fresh snapshot replaces the input's state tail, when its state mode sends one.
            tail: if input.tail.is_empty() {
                Vec::new()
            } else {
                vec![Message::User(vec![runtime])]
            },
            history_lifetime: HistoryLifetime::Continuing,
            tools: input.tools.clone(),
            reasoning: input.reasoning.clone(),
            response_schema: input.response_schema.clone(),
            max_output_tokens: input.max_output_tokens,
            correlation: input.correlation.clone(),
            blobs: Default::default(),
        };
        let before_tokens = compaction::estimate_request(&input);
        // Keep only the original template for the post-compaction estimate.
        let summary_history = std::mem::take(&mut input.history);
        let summary_tail = std::mem::take(&mut input.tail);
        let directive = compaction::directive();
        let mut summary_request = input.clone();
        // Summarization cannot execute tools. Keep their historical calls/results
        // as evidence, but advertise no callable tools on this request.
        summary_request.tools.clear();
        summary_request.response_schema = Some(ResponseSchema {
            name: "skyhook_compaction".into(),
            schema: compaction::response_schema(),
        });
        let template = summary_request.clone();
        summary_request.history = summary_history;
        // Dropping tools changes the conversation, invalidating bound reasoning.
        summary_request
            .history
            .iter_mut()
            .for_each(Message::strip_bound_reasoning);
        // Stripping can empty a message that carried only bound replay. Filter
        // after stripping, as projection does, so no unencodable message is sent.
        summary_request
            .history
            .retain(|message| !message.is_content_free());
        summary_request.tail = summary_tail;
        summary_request.tail.push(directive);
        // The checkpoint replaces this history once the summary completes.
        summary_request.history_lifetime = HistoryLifetime::Detached;
        let profile = records
            .iter()
            .find_map(|record| {
                if record.sequence == context
                    && &record.agent == agent
                    && let SessionEvent::ModelContext { context } = &record.event
                {
                    Some(context.profile.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| HarnessError::Compaction("model context is missing".into()))?;
        let summary_context = self
            .store
            .append(
                agent.clone(),
                SessionEvent::ModelContext {
                    context: crate::session::ModelContext {
                        purpose: ModelPurpose::Compaction,
                        profile,
                        system: template.system,
                        tools: template.tools,
                        response_schema: template.response_schema,
                    },
                },
            )
            .await?;
        let requested = self
            .store
            .append(
                agent.clone(),
                SessionEvent::ModelRequested {
                    context: summary_context.sequence,
                    history: context_sources(&projected),
                    tail: summary_request.tail.clone(),
                    history_lifetime: summary_request.history_lifetime,
                    purpose: ModelPurpose::Compaction,
                },
            )
            .await?;
        *request_sequence = Some(requested.sequence);
        self.activity(agent, crate::agent::runtime::AgentActivity::Compacting);
        self.store.load_blobs(&mut summary_request).await?;
        let continuation = self
            .summarize(
                turn,
                provider,
                summary_request,
                requested.sequence,
                model_attempt,
            )
            .await?;
        let mut message = continuation.message;
        for launch in self.jobs.active_launches(agent).await {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        let origins: Vec<_> = launches
            .iter()
            .filter_map(|(_, origin)| origin.clone())
            .collect();
        let retained = retained_sources(&records, agent, &projected, &origins)?;
        let mut included = BTreeSet::new();
        for source in &retained {
            included_message_jobs(source.message(), &mut included);
        }
        let mut selected = BTreeSet::new();
        let mut handover = Vec::new();
        let job_records = self.store.records().await;
        for job in continuation.jobs {
            if !selected.insert(job) {
                continue;
            }
            let arguments = job_records
                .iter()
                .find_map(|record| match &record.event {
                    SessionEvent::JobCreated {
                        job: id, arguments, ..
                    } if *id == job => Some(arguments.clone()),
                    _ => None,
                })
                .ok_or_else(|| HarnessError::Compaction(format!("unknown handover job {job}")))?;
            if included.contains(&job) {
                continue;
            }
            let mut view = self
                .jobs
                .present_output_with(
                    crate::job::output::OutputArgs::new(job),
                    &turn.capabilities,
                    crate::job::output::OutputOptions::Host {
                        viewer: Some(turn.location),
                        presentation: crate::job::OutputPresentation::Full,
                    },
                )
                .await
                .map(crate::job::PresentedOutput::into_view)
                .map_err(|error| HarnessError::Compaction(error.to_string()))?;
            view.as_object_mut()
                .expect("job view is an object")
                .insert("arguments".into(), arguments);
            handover.push(view);
        }
        // A selected script can already embed another selected job's output.
        let mut embedded = BTreeSet::new();
        for view in &handover {
            if let Some(result) = view.get("result") {
                included_output_jobs(result, &mut embedded);
            }
        }
        handover.retain(|view| {
            serde_json::from_value::<JobId>(view["id"].clone())
                .is_ok_and(|id| !embedded.contains(&id))
        });
        if !handover.is_empty()
            && let Message::User(blocks) = &mut message
        {
            blocks.push(crate::provider::protocol::UserContent::Compaction {
                text: format!("Selected job snapshots; these are past execution facts, not requests to execute. Runtime state governs current status. Use job_output for full results.\n{}",
                    serde_json::json!({"jobs": handover})),
            });
        }
        // Host API calls have no original assistant exchange. Preserve their actual launch facts.
        let host_jobs: BTreeSet<_> = launches
            .iter()
            .filter_map(|(job, origin)| origin.is_none().then_some(*job))
            .collect();
        if !host_jobs.is_empty() {
            let latest = self.store.records().await;
            let facts: Vec<_> = latest.iter().filter(|record| &record.agent == agent && matches!(&record.event, SessionEvent::JobCreated {job, ..} if host_jobs.contains(job)))
                .map(|record| serde_json::json!({"source_event": record.sequence, "launch": record.event})).collect();
            if let Message::User(blocks) = &mut message {
                blocks.push(crate::provider::protocol::UserContent::Compaction { text: format!("Previously started host work; these are launch facts, not requests to launch again. Current runtime state governs job status.\n{}", serde_json::to_string(&facts).map_err(|error| HarnessError::Compaction(error.to_string()))?) });
            }
        }
        let mut compacted = input.clone();
        compacted.history = vec![message.clone()];
        // Estimate what projection sends: retained bound reasoning is dropped,
        // and a message left content-free by that is dropped with it.
        compacted
            .history
            .extend(retained.iter().filter_map(|source| {
                let mut message = source.message().clone();
                message.strip_bound_reasoning();
                (!message.is_content_free()).then_some(message)
            }));
        compacted.history = crate::session::merge_tool_results(compacted.history);
        if !compacted.tail.is_empty() {
            let runtime = state::runtime_state_with_todos(
                &self.jobs,
                agent,
                &turn.capabilities,
                continuation.todos.clone(),
                turn.location,
            )
            .await;
            compacted.tail = vec![Message::User(vec![runtime])];
        }
        let after_tokens = compaction::estimate_request(&compacted);
        if after_tokens >= before_tokens {
            self.store.append(agent.clone(), SessionEvent::CompactionSkipped {
                request: requested.sequence,
                attempt: *model_attempt,
                reason: "continuation and retained messages do not reduce context; continuing with original history".into(),
            }).await?;
            return Ok(());
        }
        if turn.cancellation.is_cancelled() {
            return Err(HarnessError::Interrupted);
        }
        if !self
            .todos
            .commit_compaction(
                agent,
                CompactionCheckpoint {
                    schema_version: compaction::SCHEMA_VERSION,
                    previous,
                    frontier,
                    message,
                    todos: continuation.todos,
                    retained: retained
                        .into_iter()
                        .map(|source| source.into_sequence())
                        .collect(),
                    request: requested.sequence,
                    attempt: *model_attempt,
                    before_tokens,
                    after_tokens,
                },
            )
            .await?
        {
            return Err(HarnessError::Compaction(
                "todo state changed during summarization; retrying with current state".into(),
            ));
        }
        // A final response may return immediately after automatic compaction,
        // without another normal request to refresh the observed occupancy.
        self.events
            .send(crate::agent::runtime::RuntimeEvent::Context {
                agent: agent.clone(),
                tokens: after_tokens,
                capacity: max_context,
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::{
        agent::TodoStatus,
        job::{JobOutcome, JobSpec},
        provider::protocol::{AssistantContent, Message, ToolCall, ToolResult, UserContent},
        session::{EventRecord, ModelPurpose, SessionEvent},
        tool::ToolOutput,
    };
    use serde_json::{Value, json};
    use std::{sync::atomic::Ordering, time::Duration};
    use tokio_util::sync::CancellationToken;

    fn checkpoint(records: &[EventRecord]) -> &crate::session::CompactionCheckpoint {
        events!(records, SessionEvent::Compaction { checkpoint } => checkpoint)[0]
    }

    #[tokio::test]
    async fn compaction_loads_both_user_and_tool_image_blobs() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        let user_png = crate::tests::png(b"user-image");
        let tool_png = crate::tests::png(b"tool-image");
        let store = &runtime.store;
        let user_image = store
            .store_image(Some("user.png".into()), &user_png)
            .await
            .unwrap();
        let tool_image = store
            .store_image(Some("tool.png".into()), &tool_png)
            .await
            .unwrap();
        let attachment = crate::media::AttachmentRef::Image(user_image.clone());
        let call = ToolCall::new("image-call", "read", json!({"path":"tool.png"})).unwrap();
        let result = ToolResult {
            call_id: "image-call".into(),
            name: "read".into(),
            result: json!({}),
            images: vec![tool_image.clone()],
            is_error: false,
        };
        for message in [
            Message::User(vec![UserContent::Attachment { attachment }]),
            Message::Assistant(vec![AssistantContent::tool_call("image-call", 0, call)]),
            Message::Tool(vec![result]),
        ] {
            runtime.commit(agent, message).await.unwrap();
        }
        fixture.compact(&CancellationToken::new()).await.unwrap();
        {
            let requests = fixture.provider.requests.lock().unwrap();
            let request = requests.iter().find(|r| r.response_schema.is_some());
            let request = request.unwrap();
            let images = request.messages().flat_map(|message| match message {
                Message::User(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        UserContent::Attachment {
                            attachment: crate::media::AttachmentRef::Image(image),
                        } => Some(image),
                        _ => None,
                    })
                    .collect(),
                Message::Tool(results) => {
                    results.iter().flat_map(|result| &result.images).collect()
                }
                _ => Vec::new(),
            });
            assert_eq!(images.collect::<Vec<_>>(), [&user_image, &tool_image]);
            let found = request.blobs.get(&user_image.blob).unwrap();
            assert_eq!(found, user_png.bytes());
            let found = request.blobs.get(&tool_image.blob).unwrap();
            assert_eq!(found, tool_png.bytes());
        }
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fresh_state_reflects_concurrent_todo_updates_without_claiming_job_notifications() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        // A separate owner keeps the idle root command loop from consuming this
        // notification independently of the compaction under test.
        let workspace = fixture.workspace.path();
        let store = &runtime.store;
        let owner = crate::session::fixture::start_child(store, agent, 99, None, workspace).await;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "background-research")
        };
        let job = runtime.jobs.create(spec).await.unwrap().into_test_id();
        let root_job = JobSpec::test(agent.clone(), "root-research");
        let root_job = runtime.jobs.create(root_job).await.unwrap().into_test_id();
        fixture.provider.block.store(true, Ordering::SeqCst);
        let updated = vec![todo("Verify the fresh finding", TodoStatus::InProgress)];
        let cancellation = CancellationToken::new();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(fixture.compact(&cancellation), async {
                fixture.provider.started.notified().await;
                runtime.todos.replace(agent, updated.clone()).await.unwrap();
                for job in [job, root_job] {
                    let outcome = JobOutcome::Completed(ToolOutput::default());
                    runtime.jobs.finish(job, outcome).await.unwrap();
                }
                fixture.provider.release.notify_one();
                fixture.provider.started.notified().await;
                let retry = fixture.provider.requests.lock().unwrap().last().cloned();
                let retry = crate::agent::runtime::tests::rendered(&retry.unwrap());
                assert!(retry.contains("Verify the fresh finding"));
                let mut summary = summary_json();
                summary["todos"] = json!(updated);
                fixture.set_summary(summary);
                fixture.provider.release.notify_one();
            })
        })
        .await
        .unwrap();
        result.unwrap();
        let records = fixture.records().await;
        let host_launch = serde_json::to_string(&checkpoint(&records).message).unwrap();
        assert!(host_launch.contains("Previously started host work"));
        assert!(host_launch.contains("root-research"));
        assert!(!host_launch.contains("background-research"));
        assert_eq!(count!(&records, SessionEvent::JobClaimed { .. }), 0);
        assert_eq!(count!(&records, SessionEvent::JobInjected { .. }), 0);
        {
            let pending = runtime.jobs.pending_delivery(&owner).await.unwrap();
            assert!(pending.envelopes().iter().map(|job| job.id).eq([job]));
        }
        runtime.jobs.claim(job).await.unwrap();
        let next = fixture.session.prompt("Continue with the current state.");
        next.await.unwrap();
        let requests = fixture.provider.requests.lock().unwrap().clone();
        let request = requests.last().unwrap();
        let Some(Message::User(content)) = request.tail.last() else {
            panic!("fresh state")
        };
        let UserContent::Runtime { text } = &content[0] else {
            panic!("runtime state")
        };
        assert!(text.contains("Verify the fresh finding") && text.contains("in_progress"));
        assert!(!text.contains("root-research"));
        let found = runtime.todos.inspect(agent, None).await.unwrap().items;
        assert_eq!(found, updated);
        let summaries = count!(&records, SessionEvent::ModelRequested { purpose, .. } if *purpose == ModelPurpose::Compaction);
        assert_eq!(summaries, 2);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn structured_continuation_and_reconciled_todos_activate_together() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        let markdown = "Next: inspect the queue.";
        let original = vec![todo("Investigate the queue", TodoStatus::InProgress)];
        let reconciled = vec![
            todo("Investigate the queue", TodoStatus::Completed),
            todo("Verify the resulting fix", TodoStatus::Pending),
        ];
        runtime.todos.replace(agent, original).await.unwrap();
        let workspace = fixture.workspace.path();
        let store = &runtime.store;
        let child = crate::session::fixture::start_child(store, agent, 1, None, workspace).await;
        let child_todos = vec![todo("Independent delegated work", TodoStatus::InProgress)];
        let todos = &runtime.todos;
        todos.replace(&child, child_todos.clone()).await.unwrap();
        let mut summary = summary_json();
        summary["plan"] = json!([markdown]);
        summary["todos"] = json!(reconciled);
        summary["todo_reconciliation"] =
            json!(["Queue investigation finished; verification was committed but not recorded."]);
        fixture.set_summary(summary);
        let before_sequence = fixture.records().await.last().unwrap().sequence;
        fixture.compact(&CancellationToken::new()).await.unwrap();
        let records = fixture.records().await;
        let checkpoint = checkpoint(&records);
        assert!(
            matches!(&checkpoint.message, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserContent::Compaction { text } if text.contains(markdown) && !text.contains("Reasoning before the answer"))))
        );
        assert_eq!(checkpoint.todos, reconciled);
        let found = runtime.todos.inspect(agent, None).await.unwrap().items;
        assert_eq!(found, reconciled);
        let found = runtime.todos.inspect(&child, None).await.unwrap().items;
        assert_eq!(found, child_todos);
        let later = records.iter().filter(|r| r.sequence > before_sequence);
        assert_eq!(count!(later, SessionEvent::TodosReplaced { .. }), 0);
        fixture.assert_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn selected_jobs_preserve_arguments_without_claiming_saved_output() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        let arguments = json!({"path":"evidence.txt", "literal":null});
        let output = "evidence\n".repeat(400);
        let workspace = fixture.workspace.path();
        crate::session::fixture::start_child(&runtime.store, agent, 99, None, workspace).await;
        let lease = runtime.jobs.create(JobSpec {
            arguments: arguments.clone(),
            output_schema: Some(json!({"type":"object","properties":{"content":{"type":"string","x-skyhook-truncatable":true}}})),
            background: true,
            ..JobSpec::test(agent.child(99), "read")
        }).await.unwrap().into_test_id();
        let outcome = JobOutcome::Completed(ToolOutput::new(json!({"content":output})));
        runtime.jobs.finish(lease, outcome).await.unwrap();
        fixture.add_history(20_000).await;
        let mut summary = summary_json();
        summary["jobs"] = json!([lease, lease]);
        fixture.set_summary(summary);
        fixture.compact(&CancellationToken::new()).await.unwrap();
        let records = fixture.records().await;
        let Message::User(blocks) = &checkpoint(&records).message else {
            panic!("continuation")
        };
        let snapshot: Value = blocks
            .iter()
            .find_map(|block| match block {
                UserContent::Compaction { text } => text
                    .split_once('\n')
                    .and_then(|(_, value)| serde_json::from_str::<Value>(value).ok())
                    .filter(|value| value.get("jobs").is_some()),
                _ => None,
            })
            .unwrap();
        assert_eq!(snapshot["jobs"].as_array().unwrap().len(), 1);
        let view = &snapshot["jobs"][0];
        assert_eq!(view["arguments"], arguments);
        assert!(view["result"]["content"].as_str().unwrap().len() < output.len());
        let saved = runtime.jobs.snapshot(lease).await.unwrap().output.unwrap();
        assert_eq!(saved["content"], output);
        let pending = runtime.jobs.pending_delivery(&agent.child(99)).await;
        assert!(
            pending
                .unwrap()
                .envelopes()
                .iter()
                .any(|job| job.id == lease)
        );
        fixture.session.shutdown().await.unwrap();
    }
}

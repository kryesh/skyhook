//! Build and atomically install compaction checkpoints against current runtime state.

use super::retention::{included_message_jobs, included_output_jobs, retained_sources};
use super::{HarnessError, SessionRuntime, TurnContext, compaction, context_sources};
use crate::{
    agent::runtime::prompt,
    identity::JobId,
    provider::{
        ProviderContext,
        protocol::{Message, ModelRequest, ResponseSchema},
    },
    session::{
        CompactionCheckpoint, ContextMessage, ModelCallOrigin, ModelPurpose, SessionEvent,
        project_history,
    },
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
        let mut input = ModelRequest {
            model: input.model.clone(),
            system: input.system.clone(),
            messages: projected
                .iter()
                .map(|(_, message)| message.clone())
                .collect(),
            tools: input.tools.clone(),
            reasoning: input.reasoning.clone(),
            response_schema: input.response_schema.clone(),
            max_output_tokens: input.max_output_tokens,
            correlation: input.correlation.clone(),
        };
        input.messages.push(Message::User(vec![
            prompt::runtime_state_content(
                &self.jobs,
                &self.todos,
                agent,
                turn.capabilities,
                turn.location,
            )
            .await,
        ]));
        let before_tokens = compaction::estimate_request(&input);
        // Keep only the original template for the post-compaction estimate.
        let summary_messages = std::mem::take(&mut input.messages);
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
        summary_request.messages = summary_messages;
        summary_request.messages.push(directive.clone());
        let mut messages = context_sources(&projected);
        // The penultimate message is the exact transient state for this attempt.
        messages.push(ContextMessage::Inline {
            message: summary_request
                .messages
                .iter()
                .rev()
                .nth(1)
                .expect("runtime state is present")
                .clone(),
        });
        messages.push(ContextMessage::Inline { message: directive });
        let provider_name = records
            .iter()
            .find_map(|record| {
                if record.sequence == context
                    && &record.agent == agent
                    && let SessionEvent::ModelContext { provider, .. } = &record.event
                {
                    Some(provider.clone())
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
                    provider: provider_name,
                    template,
                },
            )
            .await?;
        let requested = self
            .store
            .append(
                agent.clone(),
                SessionEvent::ModelRequested {
                    context: summary_context.sequence,
                    messages,
                    purpose: ModelPurpose::Compaction,
                },
            )
            .await?;
        *request_sequence = Some(requested.sequence);
        self.activity(agent, crate::agent::runtime::AgentActivity::Compacting);
        self.store
            .hydrate_model_request(&mut summary_request)
            .await?;
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
        for record in &records {
            if retained.contains(&record.sequence)
                && let SessionEvent::MessageCommitted { message } = &record.event
            {
                included_message_jobs(message, &mut included);
            }
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
                .inspect_output_for(
                    crate::job::output::OutputArgs::new(job),
                    turn.capabilities,
                    turn.location,
                )
                .await
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
        compacted.messages = vec![message.clone()];
        for sequence in &retained {
            let message = records
                .iter()
                .find_map(|record| {
                    if record.sequence != *sequence {
                        return None;
                    }
                    match &record.event {
                        SessionEvent::MessageCommitted { message } => Some(message.clone()),
                        _ => None,
                    }
                })
                .expect("retention references validated original messages");
            compacted.messages.push(message);
        }
        let runtime = prompt::runtime_state_with_todos(
            &self.jobs,
            agent,
            turn.capabilities,
            continuation.todos.clone(),
            turn.location,
        )
        .await;
        compacted.messages.push(Message::User(vec![runtime]));
        let after_tokens = compaction::estimate_request(&compacted);
        if after_tokens >= before_tokens {
            self.store.append(agent.clone(), SessionEvent::CompactionSkipped {
                request: requested.sequence,
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
                    retained,
                    request: requested.sequence,
                    before_tokens,
                    after_tokens,
                    max_context,
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
        agent::{TodoItem, TodoStatus},
        job::{JobOutcome, JobSpec},
        provider::protocol::{AssistantContent, Message, UserContent},
        session::{ModelPurpose, SessionEvent},
        tool::ToolOutput,
    };
    use serde_json::{Value, json};
    use std::{sync::atomic::Ordering, time::Duration};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn compaction_hydrates_both_user_and_tool_image_blobs() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        let user_image = runtime
            .store
            .import_blob(b"user-image", "user.png".into(), "image/png".into())
            .await
            .unwrap();
        let tool_image = runtime
            .store
            .import_blob(b"tool-image", "tool.png".into(), "image/png".into())
            .await
            .unwrap();
        assert!(user_image.data_base64.is_none());
        assert!(tool_image.data_base64.is_none());
        runtime
            .commit(
                agent,
                Message::User(vec![UserContent::Image {
                    image: user_image.clone(),
                }]),
            )
            .await
            .unwrap();
        runtime
            .commit(
                agent,
                Message::Assistant(vec![AssistantContent::tool_call(
                    "image-call",
                    0,
                    crate::provider::protocol::ToolCall {
                        id: "image-call".into(),
                        name: "read".into(),
                        arguments: json!({"path":"tool.png"}),
                    },
                )]),
            )
            .await
            .unwrap();
        runtime
            .commit(
                agent,
                Message::Tool(vec![crate::provider::protocol::ToolResult {
                    call_id: "image-call".into(),
                    name: "read".into(),
                    result: json!({}),
                    images: vec![tool_image.clone()],
                    is_error: false,
                }]),
            )
            .await
            .unwrap();
        fixture.compact(&CancellationToken::new()).await.unwrap();
        {
            let requests = fixture.provider.requests.lock().unwrap();
            let request = requests
                .iter()
                .find(|request| request.response_schema.is_some())
                .unwrap();
            let mut images = Vec::new();
            for message in &request.messages {
                match message {
                    Message::User(parts) => {
                        images.extend(parts.iter().filter_map(|part| match part {
                            UserContent::Image { image } => Some(image),
                            _ => None,
                        }))
                    }
                    Message::Tool(results) => {
                        images.extend(results.iter().flat_map(|result| &result.images))
                    }
                    _ => {}
                }
            }
            assert_eq!(images.len(), 2);
            assert_eq!(images[0].sha256, user_image.sha256);
            assert_eq!(images[0].data_base64.as_deref(), Some("dXNlci1pbWFnZQ=="));
            assert_eq!(images[1].sha256, tool_image.sha256);
            assert_eq!(images[1].data_base64.as_deref(), Some("dG9vbC1pbWFnZQ=="));
        }
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fresh_state_reflects_concurrent_todo_updates_without_claiming_job_notifications() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        // Use a separate owner so the idle root command loop cannot consume this
        // notification independently of the compaction under test.
        let owner = agent.child(99);
        let job = runtime
            .jobs
            .create(JobSpec {
                background: true,
                ..JobSpec::test(owner.clone(), "background-research")
            })
            .await
            .unwrap();
        let root_job = runtime
            .jobs
            .create(JobSpec::test(agent.clone(), "root-research"))
            .await
            .unwrap();
        fixture.provider.block.store(true, Ordering::SeqCst);
        let updated = vec![TodoItem {
            text: "Verify the fresh finding".into(),
            status: TodoStatus::InProgress,
        }];
        let cancellation = CancellationToken::new();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(fixture.compact(&cancellation), async {
                fixture.provider.started.notified().await;
                runtime.todos.replace(agent, updated.clone()).await.unwrap();
                runtime
                    .jobs
                    .finish(job.id, JobOutcome::Completed(ToolOutput::default()))
                    .await
                    .unwrap();
                runtime
                    .jobs
                    .finish(root_job.id, JobOutcome::Completed(ToolOutput::default()))
                    .await
                    .unwrap();
                fixture.provider.release.add_permits(1);
                fixture.provider.started.notified().await;
                let requests = fixture.provider.requests.lock().unwrap();
                let retry = requests.last().unwrap();
                assert!(
                    serde_json::to_string(&retry.messages)
                        .unwrap()
                        .contains("Verify the fresh finding")
                );
                drop(requests);
                let mut summary = summary_value();
                summary["todos"] = json!(updated);
                *fixture.provider.summary.lock().unwrap() = summary.to_string();
                fixture.provider.release.add_permits(1);
            })
        })
        .await
        .unwrap();
        result.unwrap();
        let records = runtime.store.records().await;
        let host_launch = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::Compaction { checkpoint } => {
                    Some(serde_json::to_string(&checkpoint.message).unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert!(host_launch.contains("Previously started host work"));
        assert!(host_launch.contains("root-research"));
        assert!(!host_launch.contains("background-research"));
        assert!(!records.iter().any(|record| matches!(
            record.event,
            SessionEvent::JobClaimed { .. } | SessionEvent::JobInjected { .. }
        )));
        assert_eq!(
            runtime.jobs.take_pending(&owner).await.unwrap()[0].id,
            job.id
        );
        assert!(runtime.jobs.take_pending(&owner).await.unwrap().is_empty());
        fixture
            .session
            .prompt("Continue with the current state.")
            .await
            .unwrap();
        let request = fixture
            .provider
            .requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        let Some(Message::User(content)) = request.messages.last() else {
            panic!("fresh state")
        };
        let UserContent::Runtime { text } = &content[0] else {
            panic!("runtime state")
        };
        assert!(text.contains("Verify the fresh finding"));
        assert!(text.contains("in_progress"));
        assert!(!text.contains("root-research"));
        assert_eq!(
            runtime.todos.inspect(agent, None).await.unwrap().items,
            updated
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record.event,
                    SessionEvent::ModelRequested {
                        purpose: ModelPurpose::Compaction,
                        ..
                    }
                ))
                .count(),
            2
        );

        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn structured_continuation_and_reconciled_todos_activate_together() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        let markdown = "Next: inspect the queue.";
        let original = vec![TodoItem {
            text: "Investigate the queue".into(),
            status: TodoStatus::InProgress,
        }];
        let reconciled = vec![
            TodoItem {
                text: "Investigate the queue".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                text: "Verify the resulting fix".into(),
                status: TodoStatus::Pending,
            },
        ];
        runtime.todos.replace(agent, original).await.unwrap();
        let child = agent.child(1);
        let child_todos = vec![TodoItem {
            text: "Independent delegated work".into(),
            status: TodoStatus::InProgress,
        }];
        runtime
            .todos
            .replace(&child, child_todos.clone())
            .await
            .unwrap();
        let mut summary = summary_value();
        summary["plan"] = json!([markdown]);
        summary["todos"] = json!(reconciled);
        summary["todo_reconciliation"] =
            json!(["Queue investigation finished; verification was committed but not recorded."]);
        *fixture.provider.summary.lock().unwrap() = summary.to_string();
        let before_sequence = runtime.store.records().await.last().unwrap().sequence;
        fixture.compact(&CancellationToken::new()).await.unwrap();
        let records = runtime.store.records().await;
        let checkpoint = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::Compaction { checkpoint } => Some(checkpoint),
                _ => None,
            })
            .unwrap();
        let expected = checkpoint.message.clone();
        assert!(
            matches!(&expected, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserContent::Compaction { text } if text.contains(markdown) && !text.contains("Reasoning before the answer"))))
        );
        assert_eq!(checkpoint.todos, reconciled);
        assert_eq!(
            runtime.todos.inspect(agent, None).await.unwrap().items,
            reconciled
        );
        assert_eq!(
            runtime.todos.inspect(&child, None).await.unwrap().items,
            child_todos
        );
        assert!(
            !records
                .iter()
                .any(|record| record.sequence > before_sequence
                    && matches!(record.event, SessionEvent::TodosReplaced { .. }))
        );
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
        let lease = runtime.jobs.create(JobSpec {
            arguments: arguments.clone(),
            output_schema: Some(json!({"type":"object","properties":{"content":{"type":"string","x-skyhook-truncatable":true}}})),
            background: true,
            ..JobSpec::test(agent.child(99), "read")
        }).await.unwrap();
        runtime
            .jobs
            .finish(
                lease.id,
                JobOutcome::Completed(ToolOutput::new(json!({"content":output}))),
            )
            .await
            .unwrap();
        fixture.add_history(20_000).await;
        let mut summary = summary_value();
        summary["jobs"] = json!([lease.id, lease.id]);
        *fixture.provider.summary.lock().unwrap() = summary.to_string();
        fixture.compact(&CancellationToken::new()).await.unwrap();
        let records = runtime.store.records().await;
        let checkpoint = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::Compaction { checkpoint } => Some(checkpoint),
                _ => None,
            })
            .unwrap();
        let Message::User(blocks) = &checkpoint.message else {
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
        assert_eq!(
            runtime
                .jobs
                .snapshot(lease.id)
                .await
                .unwrap()
                .output
                .unwrap()["content"],
            output
        );
        assert!(
            runtime
                .jobs
                .take_pending(&agent.child(99))
                .await
                .unwrap()
                .iter()
                .any(|job| job.id == lease.id)
        );
        fixture.session.shutdown().await.unwrap();
    }
}

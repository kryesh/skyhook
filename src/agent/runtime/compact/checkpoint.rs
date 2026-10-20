//! Build and atomically install compaction checkpoints against current runtime state.

use super::retention::{
    Retain, included_message_jobs, included_output_jobs, retained_sources, retention_budget,
};
use super::{AgentContext, HarnessError, SessionRuntime, TurnContext, continuation};
use crate::{
    agent::{CompactionError, runtime::state},
    identity::JobId,
    job::JobView,
    provider::protocol::{HistoryLifetime, ModelRequest},
    session::{
        CheckpointError, Command, CompactionCheckpoint, CompactionTrigger, CoreState, Message,
        ModelCallOrigin, ModelPurpose, Plan, ProfileSnapshot, RequestSeq, SessionEvent,
    },
};
use std::collections::BTreeSet;

pub(super) struct CompactionInput<'a> {
    pub(super) profile: &'a ProfileSnapshot,
    pub(super) request: &'a ModelRequest,
    pub(super) trigger: CompactionTrigger,
    pub(super) fit: super::SummaryFit,
    /// The output limit the round's last attempt sent, if any.
    pub(super) ceiling: &'a mut Option<std::num::NonZeroU64>,
    pub(super) model_attempt: &'a mut u64,
}

impl SessionRuntime {
    pub(super) async fn compact_inner(
        &self,
        turn: &TurnContext<'_>,
        context: &mut AgentContext,
        source: CompactionInput<'_>,
        request_sequence: &mut Option<RequestSeq>,
        launches: &mut Vec<(JobId, Option<ModelCallOrigin>)>,
    ) -> Result<(), HarnessError> {
        let CompactionInput {
            profile,
            request: input,
            trigger,
            fit,
            ceiling,
            model_attempt,
        } = source;
        let agent = turn.agent;
        context.refresh(&self.store, agent).await?;
        let frontier = context.through();
        let projected = context.projected.clone();
        let provider = context.provider.as_mut();
        for launch in self.jobs.active_launches(agent) {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        // Retry against current history and runtime state, including any todo
        // changes that invalidated an earlier attempt's snapshot.
        let runtime = state::runtime_state_content(
            &self.jobs,
            &self.store,
            agent,
            &turn.capabilities,
            turn.location,
        );
        // The summary always gets the state: it must return the reconciled todo list.
        let state = Message::State(runtime);
        let mut blobs = input.blobs.clone();
        let mut summary_tail = Vec::new();
        let mut summary_request = ModelRequest {
            model: input.model.clone(),
            system: input.system.clone(),
            context: Vec::new(),
            history_lifetime: HistoryLifetime::Detached,
            // Summarization cannot execute tools. Keep their historical calls/results
            // as evidence, but advertise no callable tools on this request.
            tools: Vec::new(),
            reasoning: input.reasoning.clone(),
            response_schema: Some(continuation::response_schema()),
            max_output_tokens: input.max_output_tokens,
            blobs: Default::default(),
        };
        let template = summary_request.clone();
        let mut summary_history = projected.clone();
        let pending = summary_history.messages.last().and_then(|last| {
            let Message::Assistant(items) = &last.message else {
                return None;
            };
            let call = items.iter().find_map(|item| item.call())?;
            Some(ModelCallOrigin {
                message: last.sequence.message(),
                call_id: call.id().to_owned(),
            })
        });
        // The summary sees this response as evidence in its tail, not unanswered
        // native calls. Retain the original for actual results after compaction.
        if pending.is_some()
            && let Some(Message::Assistant(items)) =
                summary_history.messages.pop().map(|last| last.message)
        {
            summary_tail.push(continuation::pending_response(&items));
        }
        // The state ends the request, where every codec has a slot for it.
        summary_tail.extend([continuation::directive(), state]);
        let summary_through = summary_history.through();
        let summary_from = match fit {
            super::SummaryFit::HalfHistory => summary_history.trim(),
            super::SummaryFit::Full | super::SummaryFit::HalfReserve => None,
        };
        // Dropping tools changes the conversation, invalidating bound reasoning and the
        // states sent for it.
        summary_history.unbind();
        // The checkpoint replaces this history once the summary completes.
        summary_request.context = summary_history.context(&summary_tail);
        // Once history is dropped, the completed response's occupancy no longer
        // describes this input.
        let occupied = match trigger {
            CompactionTrigger::Occupancy(tokens) if summary_from.is_none() => Some(tokens),
            CompactionTrigger::Occupancy(_) | CompactionTrigger::Overflow => None,
        };
        let limit = fit.reserve(super::summary_max_output(profile, occupied)?);
        let limit = ceiling.map_or(limit, |ceiling| limit.min(ceiling));
        *ceiling = Some(limit);
        summary_request.max_output_tokens = Some(limit);
        let mut summary_profile = profile.clone();
        summary_profile.profile.max_output = limit;
        let summary_context = crate::session::ModelContext {
            purpose: ModelPurpose::Compaction,
            profile: summary_profile,
            system: template.system,
            tools: template.tools,
            response_schema: template.response_schema,
        };
        let (checkpoint, history_lifetime) =
            (projected.checkpoint, summary_request.history_lifetime);
        let requested = move |context| SessionEvent::ModelRequested {
            context,
            checkpoint,
            through: summary_through,
            from: summary_from,
            tail: summary_tail,
            history_lifetime,
        };
        let requested = self.journal_request(agent, summary_context, requested);
        let sequence = requested.await?;
        *request_sequence = Some(sequence);
        self.store
            .load_blobs(&mut summary_request, &mut blobs)
            .await?;
        let summarized = self.summarize(turn, provider, &summary_request, sequence, model_attempt);
        let (attempt, continuation) = summarized.await?;
        let mut message = continuation.message;
        for launch in self.jobs.active_launches(agent) {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        let origins: Vec<_> = launches
            .iter()
            .filter_map(|(_, origin)| origin.clone())
            .chain(pending)
            .collect();
        // The messages an overflowing request added are unsized and may be what
        // overflowed, so only its last exchange stays.
        let retain = match trigger {
            CompactionTrigger::Occupancy(_) => {
                Retain::Within(retention_budget(profile.profile.max_context.get()))
            }
            CompactionTrigger::Overflow => Retain::LastExchange,
        };
        let checkpointed = projected.checkpoint.unwrap_or_default();
        let messages = origins.iter().map(|origin| origin.message).collect();
        let retention = self
            .store
            .retention(agent, checkpointed, frontier, messages);
        let retention = retention.await?.map_err(CompactionError::Checkpoint)?;
        let (requests, exchanges) = (&retention.requests, &retention.exchanges);
        let retained = retained_sources(&projected, requests, exchanges, retain);
        let mut included = BTreeSet::new();
        for source in &retained {
            included_message_jobs(source.message(), &mut included);
        }
        let mut selected = BTreeSet::new();
        let jobs: Vec<JobId> = (continuation.jobs.into_iter())
            .filter(|job| selected.insert(*job))
            .collect();
        // Host API calls have no original assistant exchange. Preserve their actual launch facts.
        let host_jobs: BTreeSet<_> = launches
            .iter()
            .filter_map(|(job, origin)| origin.is_none().then_some(*job))
            .collect();
        let creations = self
            .store
            .job_creations(host_jobs.iter().copied().collect(), agent);
        let facts = (creations.await?.into_iter())
            .map(|(created, launch)| serde_json::json!({"source_event": created, "launch": launch}))
            .collect();
        let mut handover = Vec::new();
        for job in jobs {
            let created = self.jobs.snapshot(job).await;
            let created = created.map_err(|_| CompactionError::UnknownJob(job))?;
            let arguments = (*created.arguments).clone();
            if included.contains(&job) {
                continue;
            }
            let view = self
                .jobs
                .present_output_with(
                    crate::job::output::OutputArgs::new(job),
                    crate::job::CancellationToken::new(),
                    turn.diagnostic_viewer(),
                    crate::job::output::OutputOptions::Host,
                )
                .await
                .map_err(|source| CompactionError::HandoverOutput { job, source })?
                .into_job_view();
            handover.push(Handover { view, arguments });
        }
        // A selected script can already embed another selected job's output.
        let mut embedded = BTreeSet::new();
        for result in handover
            .iter()
            .filter_map(|handed| handed.view.result.as_ref())
        {
            included_output_jobs(result, &mut embedded);
        }
        handover.retain(|handed| handed.view.id.is_some_and(|id| !embedded.contains(&id)));
        if !handover.is_empty()
            && let Message::User(blocks) = &mut message
        {
            blocks.push(crate::session::UserPart::Compaction {
                text: format!("Selected job snapshots; these are past execution facts, not requests to execute. Runtime state governs current status. Use jobs for full results.\n{}",
                    serde_json::json!({"jobs": handover})),
            });
        }
        if !host_jobs.is_empty()
            && let Message::User(blocks) = &mut message
        {
            blocks.push(crate::session::UserPart::Compaction { text: format!("Previously started host work; these are launch facts, not requests to launch again. Current runtime state governs job status.\n{}", serde_json::Value::Array(facts)) });
        }
        let checkpoint = CompactionCheckpoint {
            frontier,
            message,
            todos: continuation.todos,
            retained: retained
                .into_iter()
                .map(|source| source.into_sequence())
                .collect(),
            attempt,
            trigger,
        };
        // Only a busy turn installs its compaction. Todos changed since the frontier
        // make those the summary reconciled stale, which a fresh summary can correct.
        let owner = agent.clone();
        let compaction = move |state: &CoreState| {
            if !state.agents.busy(&owner) {
                return Err(HarnessError::Interrupted);
            }
            if crate::agent::todo::changed_after(&state.todos, &owner, checkpoint.frontier) {
                return Err(CompactionError::Checkpoint(CheckpointError::StaleTodos).into());
            }
            let compaction = SessionEvent::Compaction { checkpoint };
            Ok::<_, HarnessError>(Plan::journal(vec![(owner, compaction)]))
        };
        self.store.commit(Command::Journal, compaction).await?;
        Ok(())
    }
}

/// A selected job as handed over: its view and the arguments it started with.
#[derive(serde::Serialize)]
struct Handover {
    #[serde(flatten)]
    view: JobView,
    arguments: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::{
        agent::TodoStatus,
        job::{JobOutcome, JobSpec},
        provider::protocol::{AssistantItem, Entry, State, StateLifetime, ToolCall, ToolResult},
        session::{EventRecord, Message, ModelPurpose, SessionEvent, UserPart},
        tool::ToolOutput,
    };
    use serde_json::{Value, json};
    use std::sync::Arc;

    fn checkpoint(records: &[Arc<EventRecord>]) -> &crate::session::CompactionCheckpoint {
        events!(records, SessionEvent::Compaction { checkpoint } => checkpoint)[0]
    }

    #[tokio::test]
    async fn summary_sees_pending_response_as_evidence_and_checkpoint_retains_original_calls() {
        use crate::provider::{
            codec::common::tests::envelope,
            protocol::{Binding, ReplayFormat},
        };
        use crate::session::render_history;

        let fixture = Fixture::new([
            Step::new(summary(summary_json())),
            Step::new(summary(summary_json())),
        ])
        .await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        fixture.compact().await.unwrap();
        fixture.add_history(20_000).await;
        let prefix = fixture.history(agent).await;
        let replay = envelope(
            ReplayFormat::Messages,
            "test",
            json!({"signature":"bound-to-old-history"}),
            Binding::Conversation,
        );
        let call = |id: &str, position, name: &str, arguments| {
            AssistantItem::tool_call(id, position, ToolCall::new(id, name, arguments).unwrap())
        };
        let items = vec![
            AssistantItem::reasoning("reasoning", 0, "visible reasoning", Some(replay)),
            AssistantItem::text("text", 1, "These calls are not executed yet."),
            call("pending-a", 2, "read", json!({"path":"latest-finding.txt"})),
            call(
                "pending-b",
                3,
                "todo",
                json!({"items":[{"text":"Verify latest finding", "status":"pending"}]}),
            ),
        ];
        let snapshot = super::continuation::pending_response(&items);
        let assistant = Message::Assistant(items);
        let origin = runtime.commit(agent, assistant.clone()).await.unwrap();
        fixture.compact().await.unwrap();
        fixture.assert_no_tool_execution();
        let records = fixture.records();
        let checkpoints = events!(&records, SessionEvent::Compaction { checkpoint } => checkpoint);
        assert_eq!(checkpoints.len(), 2);
        let checkpoint = checkpoints[1];
        assert!(checkpoint.retained.contains(&origin));
        assert!(checkpoint.frontier >= origin.into());
        let requested =
            crate::session::tests::record_at(&records, checkpoint.attempt.request.into()).unwrap();
        let SessionEvent::ModelRequested { through, tail, .. } = &requested.event else {
            panic!("summary request")
        };
        assert_eq!(*through, prefix.through());
        assert!(tail.contains(&snapshot));
        let summary = fixture.script.request(2).await;
        assert!(
            summary
                .context
                .starts_with(&render_history(prefix.history()))
        );
        let evidence = crate::agent::runtime::tests::sent(&snapshot);
        assert!(summary.messages().any(|message| message == &evidence));
        // The summary's own state ends it.
        let last = summary.context.last();
        assert!(
            matches!(last, Some(Entry::State(state)) if state.lifetime == StateLifetime::Current)
        );
        assert!(!summary.messages().any(|message| matches!(
            message, Sent::Assistant(items) if items.iter().any(|item| item.call().is_some())
        )));
        let replayed = runtime
            .store
            .reconstruct_model_request(checkpoint.attempt.request);
        let (_, replayed) = replayed.await.unwrap();
        assert_eq!(replayed, summary);
        let projected = fixture.history(agent).await;
        assert_eq!(
            projected.messages.last().unwrap().message,
            assistant.without_bound_reasoning()
        );
        assert!(!projected.history().any(|(message, _)| message == &snapshot));
        for (id, name) in [("pending-b", "todo"), ("pending-a", "read")] {
            runtime
                .commit(
                    agent,
                    Message::Tool(ToolResult {
                        call_id: id.into(),
                        name: name.into(),
                        result: json!({"finished": id}),
                        images: vec![],
                        is_error: false,
                    }),
                )
                .await
                .unwrap();
        }
        let projected = fixture.history(agent).await;
        let history = render_history(projected.history());
        let Some(Entry::Message(Sent::Tool(results))) = history.last() else {
            panic!("real results follow the retained assistant")
        };
        assert!(
            results
                .iter()
                .map(|result| result.call_id.as_str())
                .eq(["pending-a", "pending-b"])
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn compaction_loads_both_user_and_tool_image_blobs() {
        let fixture = Fixture::new([Step::new(summary(summary_json()))]).await;
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
            Message::User(vec![UserPart::Attachment { attachment }]),
            Message::Assistant(vec![AssistantItem::tool_call("image-call", 0, call)]),
            Message::Tool(result),
        ] {
            runtime.commit(agent, message).await.unwrap();
        }
        fixture.compact().await.unwrap();
        {
            let request = fixture.script.request(1).await;
            assert!(request.response_schema.is_some());
            let images = request.messages().flat_map(|message| match message {
                Sent::User(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        SentPart::Attachment {
                            attachment: crate::media::AttachmentRef::Image(image),
                        } => Some(image),
                        _ => None,
                    })
                    .collect(),
                Sent::Tool(results) => results.iter().flat_map(|result| &result.images).collect(),
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
        let updated = vec![todo("Verify the fresh finding", TodoStatus::InProgress)];
        let mut reconciled = summary_json();
        reconciled["todos"] = json!(updated);
        let fixture = Fixture::new([
            Step::new(summary(summary_json())).gated(),
            Step::new(summary(reconciled)).gated(),
            Step::new(answer("done")),
        ])
        .await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        // A separate owner keeps the idle root command loop from consuming this
        // notification independently of the compaction under test.
        let workspace = fixture.workspace.path();
        let store = &runtime.store;
        let owner = crate::session::tests::start_child(store, agent, 99, None, workspace).await;
        let spec = JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "background-research")
        };
        let job = runtime.jobs.create(spec).await.unwrap().into_test_id();
        let root_job = JobSpec::test(agent.clone(), "root-research");
        let root_job = runtime.jobs.create(root_job).await.unwrap().into_test_id();
        let (result, ()) = bounded(async {
            tokio::join!(fixture.compact(), async {
                fixture.script.request(1).await;
                runtime
                    .store
                    .replace_todos(agent, updated.clone(), None)
                    .await
                    .unwrap();
                for job in [job, root_job] {
                    let outcome = JobOutcome::Completed(ToolOutput::default());
                    runtime.jobs.finish(job, outcome).await.unwrap();
                }
                fixture.script.release(1);
                let retry = fixture.script.pass(2).await;
                let retry = crate::agent::runtime::tests::rendered(&retry);
                assert!(retry.contains("Verify the fresh finding"));
            })
        })
        .await;
        result.unwrap();
        let records = fixture.records();
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
        runtime.jobs.test_claim(job).await.unwrap();
        let next = fixture.session.prompt("Continue with the current state.");
        next.await.unwrap();
        let request = fixture.script.request(3).await;
        let Some(Entry::State(State { text, .. })) = request.context.last() else {
            panic!("fresh state")
        };
        assert!(text.contains("Verify the fresh finding") && text.contains("in_progress"));
        assert!(!text.contains("root-research"));
        let found = runtime.store.inspect_todos(agent, None).unwrap();
        assert_eq!(found, updated);
        let summaries = records
            .iter()
            .filter(|record| {
                let purpose = crate::session::tests::request_purpose(&records, record);
                purpose == Some(ModelPurpose::Compaction)
            })
            .count();
        assert_eq!(summaries, 2);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn valid_nonshrinking_continuation_and_reconciled_todos_activate_together() {
        let markdown = "Next: inspect the queue.";
        let original = vec![todo("Investigate the queue", TodoStatus::InProgress)];
        let reconciled = vec![
            todo("Investigate the queue", TodoStatus::Completed),
            todo("Verify the resulting fix", TodoStatus::Pending),
        ];
        let mut continuation = summary_json();
        continuation["plan"] = json!([markdown]);
        continuation["findings"] = json!(["Additional context for the continuation. ".repeat(500)]);
        continuation["todos"] = json!(reconciled);
        continuation["todo_reconciliation"] =
            json!(["Queue investigation finished; verification was committed but not recorded."]);
        let fixture = Fixture::new([Step::new(summary(continuation))]).await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        runtime
            .store
            .replace_todos(agent, original, None)
            .await
            .unwrap();
        let workspace = fixture.workspace.path();
        let store = &runtime.store;
        let child = crate::session::tests::start_child(store, agent, 1, None, workspace).await;
        let child_todos = vec![todo("Independent delegated work", TodoStatus::InProgress)];
        let todos = &runtime.store;
        todos
            .replace_todos(&child, child_todos.clone(), None)
            .await
            .unwrap();
        let before_sequence = fixture.records().last().unwrap().sequence;
        fixture.compact().await.unwrap();
        let records = fixture.records();
        let checkpoint = checkpoint(&records);
        // No response reported the occupancy this compaction ran at.
        assert_eq!(
            checkpoint.trigger,
            crate::session::CompactionTrigger::Overflow
        );
        assert!(
            matches!(&checkpoint.message, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserPart::Compaction { text } if text.contains(markdown) && !text.contains("Reasoning before the answer"))))
        );
        assert_eq!(checkpoint.todos, reconciled);
        let found = runtime.store.inspect_todos(agent, None).unwrap();
        assert_eq!(found, reconciled);
        let found = runtime.store.inspect_todos(&child, None).unwrap();
        assert_eq!(found, child_todos);
        let later = records.iter().filter(|r| r.sequence > before_sequence);
        assert_eq!(count!(later, SessionEvent::TodosReplaced { .. }), 0);
        fixture.assert_no_tool_execution();
        fixture.session.shutdown().await.unwrap();
    }

    /// The summary must return the reconciled todo list, so it sees the current one
    /// even when the model's state mode sends normal requests no runtime state.
    #[tokio::test]
    async fn stateless_summary_still_sees_the_current_todos() {
        let current = vec![todo("Keep the stateless todo", TodoStatus::InProgress)];
        let mut continuation = summary_json();
        continuation["todos"] = json!(current);
        let steps = [Step::new(summary(continuation))];
        let state_mode = crate::provider::profile::StateMode::None;
        let fixture = Fixture::with_state_mode(steps, state_mode).await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        assert_eq!(fixture.script.request(0).await.states().count(), 0);
        fixture.add_history(20_000).await;
        runtime
            .store
            .replace_todos(agent, current.clone(), None)
            .await
            .unwrap();
        fixture.compact().await.unwrap();
        let request = fixture.script.request(1).await;
        let sent = crate::agent::runtime::tests::rendered(&request);
        assert!(sent.contains("Keep the stateless todo"));
        let found = runtime.store.inspect_todos(agent, None).unwrap();
        assert_eq!(found, current);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn selected_jobs_preserve_arguments_without_claiming_saved_output() {
        // The summary names a job created below.
        let fixture = Fixture::new([Step::new(Vec::new())]).await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        let arguments = json!({"path":"evidence.txt", "literal":null});
        let output = "evidence\n".repeat(400);
        let workspace = fixture.workspace.path();
        crate::session::tests::start_child(&runtime.store, agent, 99, None, workspace).await;
        let lease = runtime
            .jobs
            .create(JobSpec {
                arguments: arguments.clone(),
                output_schema: Some(
                    json!({"type":"object","properties":{"content":{"type":"string"}}}),
                ),
                background: true,
                ..JobSpec::test(agent.child(99), "read")
            })
            .await
            .unwrap()
            .into_test_id();
        let outcome = JobOutcome::Completed(ToolOutput::new(json!({"content":output})));
        runtime.jobs.finish(lease, outcome).await.unwrap();
        fixture.add_history(20_000).await;
        let mut selected = summary_json();
        selected["jobs"] = json!([lease, lease]);
        fixture.script.respond(1, summary(selected));
        fixture.compact().await.unwrap();
        let records = fixture.records();
        let Message::User(blocks) = &checkpoint(&records).message else {
            panic!("continuation")
        };
        let snapshot: Value = blocks
            .iter()
            .find_map(|block| match block {
                UserPart::Compaction { text } => text
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
        let saved = runtime
            .jobs
            .test_snapshot(lease)
            .await
            .unwrap()
            .output
            .unwrap();
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

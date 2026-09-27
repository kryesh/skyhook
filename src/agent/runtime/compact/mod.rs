//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::{
    identity::AgentId,
    provider::{
        ProviderContext,
        protocol::{ModelRequest, Usage},
    },
    session::{AttemptRef, CompactionFailure, EventRecord, ProfileSnapshot, SessionEvent},
};

/// Context/validation recovery is bounded independently of transient retries.
pub(super) const MAX_COMPACTION_ATTEMPTS: u8 = 3;
/// Each validation retry waits this much longer than the one before.
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

mod checkpoint;
mod retention;
mod summary;
use checkpoint::CompactionInput;
pub(super) use retention::retention_budget;

pub(super) async fn retry_delay(
    cancellation: &crate::job::CancellationToken,
    attempt: u8,
) -> Result<(), HarnessError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(HarnessError::Interrupted),
        () = tokio::time::sleep(RETRY_BACKOFF * u32::from(attempt)) => Ok(()),
    }
}

/// Scale estimates by the last provider-reported prompt size. A ratio, unlike an
/// offset, stays valid when compaction shrinks the history.
#[derive(Default)]
pub(super) struct TokenMeter {
    baseline: Option<(u64, u64)>,
}

impl TokenMeter {
    /// The agent's latest reported request since its model or mode last changed.
    pub(super) fn restore(records: &[EventRecord], agent: &AgentId) -> Self {
        let mut meter = Self::default();
        for record in records.iter().rev().filter(|record| &record.agent == agent) {
            match &record.event {
                SessionEvent::ModelChanged { .. } | SessionEvent::ModeChanged { .. } => break,
                SessionEvent::Usage { request, usage } => {
                    if let Ok((_, request)) =
                        crate::session::reconstruct_model_request(records, *request)
                    {
                        meter.observe(compaction::estimate_request(&request), *usage);
                    }
                    if meter.baseline.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }
        meter
    }

    pub(super) fn estimate(&self, request: &ModelRequest) -> u64 {
        self.scale(compaction::estimate_request(request))
    }

    pub(super) fn scale(&self, estimate: u64) -> u64 {
        self.baseline.map_or(estimate, |(old_estimate, actual)| {
            let scaled = u128::from(estimate) * u128::from(actual) / u128::from(old_estimate);
            u64::try_from(scaled).unwrap_or(u64::MAX)
        })
    }

    pub(super) fn observe(&mut self, estimate: u64, usage: Usage) {
        let actual = usage.input_tokens.saturating_add(usage.cached_input_tokens);
        if actual > 0 && estimate > 0 {
            self.baseline = Some((estimate, actual));
        }
    }
}

impl SessionRuntime {
    /// Whether a checkpoint was installed; a summary that cannot shrink the context is skipped.
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        meter: &mut TokenMeter,
        profile: &ProfileSnapshot,
        input: &ModelRequest,
        max_context: u64,
    ) -> Result<bool, HarnessError> {
        let mut launches = self.jobs.active_launches(turn.agent).await;
        let mut model_attempt = 0;
        for attempt in 1..=MAX_COMPACTION_ATTEMPTS {
            let mut request_sequence = None;
            let attempted = model_attempt;
            let result = self
                .compact_inner(
                    turn,
                    provider,
                    CompactionInput {
                        meter,
                        profile,
                        request: input,
                        max_context,
                        model_attempt: &mut model_attempt,
                    },
                    &mut request_sequence,
                    &mut launches,
                )
                .await;
            // Only an attempt of this summary request belongs to its outcome.
            let failure = match request_sequence {
                None => CompactionFailure::BeforeRequest,
                Some(request) if model_attempt > attempted => {
                    CompactionFailure::Attempted(AttemptRef {
                        request,
                        attempt: model_attempt,
                    })
                }
                Some(request) => CompactionFailure::Requested(request),
            };
            match result {
                Ok(installed) => return Ok(installed),
                Err(error) => {
                    self.store
                        .append(
                            turn.agent.clone(),
                            SessionEvent::CompactionFailed {
                                failure,
                                error: (&error).into(),
                            },
                        )
                        .await?;
                    let retryable =
                        matches!(&error, HarnessError::Compaction(error) if error.retryable());
                    if !retryable || attempt == MAX_COMPACTION_ATTEMPTS {
                        return Err(error);
                    }
                    retry_delay(turn.cancellation, attempt).await?;
                }
            }
        }
        unreachable!("bounded attempts return a result")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering};

    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    pub(super) use crate::agent::runtime::tests::{
        Requests, Script, Sent, SentPart, Step, answer, bounded, count, cut, delta, events, models,
        provider_name, recoverable, response, summary_json, test_builder, todo, usage,
    };
    use crate::agent::runtime::{HarnessError, SessionHandle, TurnContext, compaction, state};
    use crate::provider::profile::StateMode;
    use crate::{
        agent::{CompactionFault, FaultKind, TodoStatus},
        execution::ExecutionLocation,
        provider::{
            Provider, ProviderErrorKind,
            protocol::{
                AssistantItem, ContextId, CutReason, ItemKind, ModelRequest, ResponseEvent,
                ToolCall, Usage,
            },
        },
        session::{Message, SessionEvent, UserPart, project_history},
        tool::policy::CapabilitySet,
    };

    /// A summary answer after reasoning, which must not enter the continuation.
    pub(super) fn summary(text: impl std::fmt::Display) -> Vec<ResponseEvent> {
        let reasoning =
            "Reasoning before the answer is not JSON and must not enter the continuation.";
        response(vec![
            AssistantItem::reasoning("reasoning/0", 0, reasoning, None),
            AssistantItem::text("text/1", 1, text.to_string()),
        ])
    }

    /// A stream that fails transiently after reporting `usage`, if any.
    pub(super) fn failing_stream(usage: Option<Usage>) -> Step {
        let usage = usage.map(|usage| Ok(ResponseEvent::Usage(usage)));
        Step::stream(usage.into_iter().chain([Err(recoverable())]).collect())
    }

    /// Streams `usage` and a tool call's start, then holds until released.
    pub(super) fn held_after(usage: Usage) -> Step {
        let arguments = json!({"path":"must-not-exist", "content":"side effect"}).to_string();
        let started = delta("interrupted-tool", ItemKind::ToolCall, &arguments);
        let mut events = vec![ResponseEvent::Usage(usage), started];
        events.extend(answer("never released"));
        Step::new(events).midstream()
    }

    pub(super) struct Fixture {
        pub(super) workspace: tempfile::TempDir,
        pub(super) session: SessionHandle,
        pub(super) script: Arc<Script>,
        pub(super) template: ModelRequest,
    }

    impl Fixture {
        /// A session that answered one research prompt; `steps` serve what follows,
        /// from step 1.
        pub(super) async fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self::with_state_mode(steps, StateMode::default()).await
        }

        /// A fixture whose model receives runtime state as `state_mode` directs.
        pub(super) async fn with_state_mode(
            steps: impl IntoIterator<Item = Step>,
            state_mode: StateMode,
        ) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let steps = [Step::new(answer("done"))].into_iter().chain(steps);
            let script = Script::new(steps, &Requests::default());
            let sessions = workspace.path().join("sessions");
            let profile = crate::provider::profile::ModelProfile {
                max_output: crate::tests::limit(16_384),
                state_mode,
                ..crate::tests::profile("test", false)
            };
            let builder = test_builder(workspace.path(), &sessions, script.clone(), false)
                .provider(
                    provider_name("test"),
                    script.clone(),
                    models([("test", profile)]),
                );
            let session = builder.build().await.unwrap().new_session().await.unwrap();
            let research =
                session.prompt("Research the existing task and preserve its constraints.");
            research.await.unwrap();
            let mut template = script.request(0).await;
            (template.history, template.tail) = (Vec::new(), Vec::new());
            template.history_lifetime = Default::default();
            Self {
                workspace,
                session,
                script,
                template,
            }
        }

        pub(super) fn requests(&self) -> Vec<ModelRequest> {
            let requests = self.script.requests.lock().unwrap();
            requests
                .iter()
                .map(|served| served.request.clone())
                .collect()
        }

        pub(super) async fn add_history(&self, tokens: usize) {
            let (runtime, root) = (&self.session.runtime, &self.session.root);
            let research = "research ".repeat(tokens * 4 / 9);
            let research = Message::Assistant(vec![AssistantItem::text("text/0", 0, research)]);
            runtime.commit(root, research).await.unwrap();
            let text = "Continue the existing task.".into();
            let next = Message::User(vec![UserPart::Text { text }]);
            runtime.commit(root, next).await.unwrap();
        }

        pub(super) async fn assert_no_tool_execution(&self) {
            let records = self.session.runtime.store.records().await;
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
            assert!(!self.workspace.path().join("must-not-exist").exists());
        }

        pub(super) async fn records(&self) -> Vec<crate::session::EventRecord> {
            self.session.runtime.store.records().await
        }

        pub(super) async fn compact(
            &self,
            cancellation: &CancellationToken,
        ) -> Result<(), HarnessError> {
            let runtime = &self.session.runtime;
            let agent = &self.session.root;
            let records = runtime.store.records().await;
            let profile = records.iter().find_map(|record| match &record.event {
                SessionEvent::ModelContext { context } => Some(context.profile.clone()),
                _ => None,
            });
            let profile = profile.expect("the fixture prompt journaled a model context");
            let mut input = self.template.clone();
            let history = project_history(&records, agent);
            input.history = crate::session::render_history(history.history());
            let capabilities = CapabilitySet::default();
            let location = ExecutionLocation::root(self.workspace.path().to_path_buf());
            let (jobs, todos) = (&runtime.jobs, &runtime.todos);
            let state =
                state::runtime_state_content(jobs, todos, agent, &capabilities, &location).await;
            // As the agent's context builds it: only a stateless mode sends no tail.
            if profile.profile.state_mode != StateMode::None {
                input.tail = vec![crate::session::Message::User(vec![state]).render()];
            }
            let turn = TurnContext {
                agent,
                owner_job: None,
                cancellation,
                location: &location,
                capabilities: capabilities.clone(),
            };
            let mut provider = self.script.open_context(ContextId::from(agent))?;
            let (provider, meter) = (provider.as_mut(), &mut Default::default());
            let compacted =
                runtime.compact_history(&turn, provider, meter, &profile, &input, 128_000);
            compacted.await.map(drop)
        }
    }

    #[tokio::test]
    async fn stream_overflow_below_threshold_compacts_once() {
        let overflow = ProviderErrorKind::ContextWindowExceeded.error("prompt is too long");
        let steps = [
            Step::stream(vec![Err(overflow)]),
            Step::new(summary(summary_json())),
            Step::new(answer("done")),
        ];
        let fixture = Fixture::new(steps).await;
        fixture.add_history(20_000).await;
        assert_eq!(fixture.session.prompt("Continue.").await.unwrap(), "done");
        // Summarization and retry retain the agent's context.
        assert_eq!(fixture.script.opened.load(Ordering::SeqCst), 1);
        let requests = fixture.requests();
        assert_eq!(requests.len(), 4); // initial response, rejected stream, summary, retry
        assert!(compaction::estimate_request(&requests[1]) < 128_000 * 4 / 5);
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_blocked_summarizer_does_not_activate_a_checkpoint() {
        let fixture = Fixture::new([Step::new(summary(summary_json())).gated()]).await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        let todos = vec![todo("Keep on interruption", TodoStatus::InProgress)];
        runtime.todos.replace(agent, todos.clone()).await.unwrap();
        let before = project_history(&fixture.records().await, agent);
        let cancellation = CancellationToken::new();
        let (result, ()) = bounded(async {
            tokio::join!(fixture.compact(&cancellation), async {
                fixture.script.request(1).await;
                cancellation.cancel();
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        let records = fixture.records().await;
        assert_eq!(project_history(&records, agent), before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let found = runtime.todos.inspect(agent, None).await.unwrap();
        assert_eq!(found, todos);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_summary_failures_preserve_history_and_never_execute_tools() {
        // Parser field cases live in compaction.rs; this covers rollback and tool contracts.
        let arguments = json!({"path":"must-not-exist", "content":"side effect"});
        let call = ToolCall::new("never-execute", "write", arguments).unwrap();
        let tool_call = response(vec![AssistantItem::tool_call("never-execute", 0, call)]);
        let reasoning = AssistantItem::reasoning("reasoning/0", 0, "thinking", None);
        let text = AssistantItem::text("text/1", 1, summary_json().to_string());
        let truncated = cut(vec![reasoning, text], CutReason::MaxTokens);
        let mut blank_todo = summary_json();
        blank_todo["todos"] = json!([{"text":" \t", "status":"pending"}]);
        let failures = [
            (FaultKind::Truncated, truncated),
            (FaultKind::ToolCall, tool_call),
            (FaultKind::Continuation, summary(blank_todo)),
        ];
        for (fault, events) in failures {
            let steps = (0..3).map(|_| Step::new(events.clone()));
            let fixture = Fixture::new(steps).await;
            let (todos, agent) = (&fixture.session.runtime.todos, &fixture.session.root);
            fixture.add_history(20_000).await;
            let old_todos = vec![todo("Preserve unfinished work", TodoStatus::InProgress)];
            todos.replace(agent, old_todos.clone()).await.unwrap();
            let before = project_history(&fixture.records().await, agent);
            let error = fixture
                .compact(&CancellationToken::new())
                .await
                .unwrap_err();
            assert_eq!(CompactionFault::from(&error).kind(), fault);
            let records = fixture.records().await;
            let faults = events!(&records, SessionEvent::CompactionFailed { failure, error }
                if failure.request().is_some() => error.kind());
            assert!(
                !faults.is_empty() && faults.iter().all(|kind| *kind == fault),
                "{faults:?}"
            );
            let requests = fixture.requests().len();
            let unchanged = project_history(&records, agent) == before;
            let compactions = count!(&records, SessionEvent::Compaction { .. });
            let kept_todos = todos.inspect(agent, None).await.unwrap() == old_todos;
            let outcome = (requests, unchanged, compactions, kept_todos);
            assert_eq!(outcome, (4, true, 0, true), "{fault}");
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// A summary selecting an unknown job, or one whose output cannot be
    /// presented, may succeed on retry; a checkpoint the journal refuses, here
    /// for an active job's unanswered call, cannot.
    #[tokio::test(start_paused = true)]
    async fn only_a_retryable_failure_retries_without_installing_compaction() {
        for (journaled, invariant) in [(false, false), (true, false), (false, true)] {
            let mut selected = summary_json();
            if !invariant {
                selected["jobs"] = json!([99999]);
            }
            let fixture = Fixture::new((0..3).map(|_| Step::new(summary(&selected)))).await;
            fixture.add_history(20_000).await;
            if journaled {
                // Journaled but unknown to the job manager, so its output cannot be presented.
                let (runtime, root) = (&fixture.session.runtime, &fixture.session.root);
                let spec = crate::job::JobSpec::test(root.clone(), "read");
                let created = SessionEvent::JobCreated {
                    job: crate::identity::JobId::new(99999).unwrap(),
                    parent: None,
                    origin: None,
                    tool: spec.tool,
                    role: spec.role,
                    name: None,
                    arguments: spec.arguments,
                    output_schema: None,
                    accepts_input: false,
                    background: false,
                    location: spec.location,
                };
                runtime.store.append(root.clone(), created).await.unwrap();
            }
            if invariant {
                let (runtime, root) = (&fixture.session.runtime, &fixture.session.root);
                let call = ToolCall::new("unanswered", "read", json!({"path":"file"})).unwrap();
                let call = Message::Assistant(vec![AssistantItem::tool_call("call", 0, call)]);
                let message = runtime.commit(root, call).await.unwrap();
                let call_id = "unanswered".into();
                let spec = crate::job::JobSpec {
                    origin: Some(crate::session::ModelCallOrigin { message, call_id }),
                    ..crate::job::JobSpec::test(root.clone(), "read")
                };
                runtime.jobs.create(spec).await.unwrap().into_test_id();
            }
            let error = fixture
                .compact(&CancellationToken::new())
                .await
                .unwrap_err();
            let records = fixture.records().await;
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            let failed = count!(&records, SessionEvent::CompactionFailed { .. });
            assert_eq!(failed, if invariant { 1 } else { 3 }, "{error}");
            fixture.session.shutdown().await.unwrap();
        }
    }
}

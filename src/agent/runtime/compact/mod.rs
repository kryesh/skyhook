//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use std::num::NonZeroU64;

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::{
    provider::{ProviderContext, ProviderErrorKind, protocol::ModelRequest},
    session::{AttemptRef, CompactionFailure, CompactionTrigger, ProfileSnapshot, SessionEvent},
};

/// Context/validation recovery is bounded independently of transient retries.
pub(super) const MAX_COMPACTION_ATTEMPTS: u8 = 3;
/// Each validation retry waits this much longer than the one before.
pub(super) const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

mod checkpoint;
mod retention;
mod summary;
use checkpoint::CompactionInput;

/// How a summary request is shrunk after one overflowed the context window: each stage
/// changes something that can make the next fit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SummaryFit {
    #[default]
    Full,
    /// Half the output reserve.
    HalfReserve,
    /// Half the output reserve, and about the older half of the history after its
    /// opening exchange dropped (`Projection::trim`).
    HalfHistory,
}

impl SummaryFit {
    /// The next stage after an overflow. Without a sent limit the reported occupancy left
    /// no room at all, which a smaller reserve cannot fix, so history goes first.
    fn after_overflow(self, sent: bool) -> Self {
        match self {
            Self::Full if sent => Self::HalfReserve,
            Self::Full | Self::HalfReserve | Self::HalfHistory => Self::HalfHistory,
        }
    }

    fn reserve(self, limit: NonZeroU64) -> NonZeroU64 {
        match self {
            Self::Full => limit,
            Self::HalfReserve | Self::HalfHistory => {
                NonZeroU64::new(limit.get() / 2).unwrap_or(limit)
            }
        }
    }
}

/// The summary's output limit: the configured one, within the space the completed
/// response's reported occupancy leaves. A summary that overflows anyway is retried
/// smaller (`SummaryFit`).
fn summary_max_output(
    profile: &ProfileSnapshot,
    occupied: Option<u64>,
) -> Result<NonZeroU64, HarnessError> {
    let occupied = occupied.unwrap_or_default();
    let available = profile.profile.max_context.get().saturating_sub(occupied);
    NonZeroU64::new(profile.profile.max_output.get().min(available)).ok_or_else(|| {
        ProviderErrorKind::ContextWindowExceeded
            .error("compaction summary leaves no room for output within the model context window")
            .into()
    })
}

impl SessionRuntime {
    /// Install a valid checkpoint, retrying recoverable summary failures.
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        profile: &ProfileSnapshot,
        input: &ModelRequest,
        trigger: CompactionTrigger,
    ) -> Result<(), HarnessError> {
        let mut launches = self.jobs.active_launches(turn.agent);
        let mut model_attempt = 0;
        let mut fit = SummaryFit::default();
        // A smaller stage never raises the output limit an earlier attempt sent.
        let mut ceiling = None;
        for attempt in 1..=MAX_COMPACTION_ATTEMPTS {
            let mut request_sequence = None;
            let attempted = model_attempt;
            let result = self
                .compact_inner(
                    turn,
                    provider,
                    CompactionInput {
                        profile,
                        request: input,
                        trigger,
                        fit,
                        ceiling: &mut ceiling,
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
                Ok(()) => return Ok(()),
                Err(error) => {
                    let fault = crate::agent::CompactionFault::from(&error);
                    let retryable = fault.retryable();
                    if fault == crate::agent::CompactionFault::Overflowed {
                        fit = fit.after_overflow(ceiling.is_some());
                    }
                    let failed = SessionEvent::CompactionFailed {
                        failure,
                        error: fault,
                    };
                    // The backoff runs from the failure, however long its record takes to commit.
                    let backoff = tokio::time::sleep(RETRY_BACKOFF * u32::from(attempt));
                    self.store.append(turn.agent.clone(), failed).await?;
                    if !retryable || attempt == MAX_COMPACTION_ATTEMPTS {
                        return Err(error);
                    }
                    tokio::select! {
                        biased;
                        () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
                        () = backoff => {}
                    }
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

    use super::summary_max_output;
    pub(super) use crate::agent::runtime::tests::{
        Requests, Script, Sent, SentPart, Step, answer, bounded, count, cut, delta, events, models,
        provider_name, recoverable, response, retrying, set_root_turn, summary_json, test_builder,
        todo, usage,
    };
    use crate::agent::runtime::{HarnessError, SessionHandle, Turn, TurnContext, state};
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
        session::{CompactionTrigger, Message, SessionEvent, UserPart, project_history},
        tool::policy::CapabilitySet,
    };

    /// The summary's output limit is the configured one within the space the completed
    /// response's reported occupancy leaves; no space left fails as an overflow.
    #[test]
    fn summary_output_limit_fits_the_reported_occupancy() {
        let mut profile = crate::session::tests::profile();
        profile.profile.max_context = crate::tests::limit(1_000);
        profile.profile.max_output = crate::tests::limit(400);
        for (completed, expected) in [
            (None, Some(400)),
            (Some(500), Some(400)),
            (Some(900), Some(100)),
            (Some(1_000), None),
        ] {
            match summary_max_output(&profile, completed) {
                Ok(limit) => assert_eq!(Some(limit.get()), expected, "{completed:?}"),
                Err(error) => assert!(
                    expected.is_none()
                        && matches!(error, HarnessError::Provider(ref error)
                            if error.kind() == ProviderErrorKind::ContextWindowExceeded),
                    "{completed:?}: {error}"
                ),
            }
        }
    }

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
            template.context.clear();
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

        pub(super) fn assert_no_tool_execution(&self) {
            let records = self.session.runtime.store.records();
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
            assert!(!self.workspace.path().join("must-not-exist").exists());
        }

        pub(super) fn records(&self) -> Vec<crate::session::EventRecord> {
            self.session.runtime.store.records()
        }

        pub(super) async fn compact(
            &self,
            cancellation: &CancellationToken,
        ) -> Result<(), HarnessError> {
            self.compact_for(cancellation, CompactionTrigger::Overflow)
                .await
        }

        pub(super) async fn compact_for(
            &self,
            cancellation: &CancellationToken,
            trigger: CompactionTrigger,
        ) -> Result<(), HarnessError> {
            let runtime = &self.session.runtime;
            let agent = &self.session.root;
            let records = runtime.store.records();
            let profile = records.iter().find_map(|record| match &record.event {
                SessionEvent::ModelContext { context } => Some(context.profile.clone()),
                _ => None,
            });
            let profile = profile.expect("the fixture prompt journaled a model context");
            let mut input = self.template.clone();
            let history = project_history(&records, agent);
            let capabilities = CapabilitySet::default();
            let location = ExecutionLocation::root(self.workspace.path().to_path_buf());
            let (jobs, todos) = (&runtime.jobs, &runtime.store);
            let state = state::runtime_state_content(jobs, todos, agent, &capabilities, &location);
            // As the agent's context builds it: only a stateless mode sends no state.
            let state = crate::session::Message::State(state);
            let sends = profile.profile.state_mode != StateMode::None;
            input.context = history.context(sends.then_some(&state));
            let turn = TurnContext {
                agent,
                owner_job: None,
                cancellation,
                location: &location,
                capabilities: capabilities.clone(),
            };
            let mut provider = self.script.open_context(ContextId::from(agent))?;
            // Only a busy turn requests a summary.
            set_root_turn(&self.session, Turn::Busy);
            let compacted =
                runtime.compact_history(&turn, provider.as_mut(), &profile, &input, trigger);
            let compacted = compacted.await;
            set_root_turn(&self.session, Turn::Idle);
            compacted
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
        let records = fixture.records();
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
        runtime
            .store
            .replace_todos(agent, todos.clone(), None)
            .await
            .unwrap();
        let before = project_history(&fixture.records(), agent);
        let cancellation = CancellationToken::new();
        let (result, ()) = bounded(async {
            tokio::join!(fixture.compact(&cancellation), async {
                fixture.script.request(1).await;
                cancellation.cancel();
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        let records = fixture.records();
        assert_eq!(project_history(&records, agent), before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let found = runtime.store.inspect_todos(agent, None).unwrap();
        assert_eq!(found, todos);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
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
            let (todos, agent) = (&fixture.session.runtime.store, &fixture.session.root);
            fixture.add_history(20_000).await;
            let old_todos = vec![todo("Preserve unfinished work", TodoStatus::InProgress)];
            todos
                .replace_todos(agent, old_todos.clone(), None)
                .await
                .unwrap();
            let before = project_history(&fixture.records(), agent);
            let cancellation = CancellationToken::new();
            let compacted = retrying(&fixture.session, fixture.compact(&cancellation));
            let error = compacted.await.unwrap_err();
            assert_eq!(CompactionFault::from(&error).kind(), fault);
            let records = fixture.records();
            let faults = events!(&records, SessionEvent::CompactionFailed { failure, error }
                if failure.request().is_some() => error.kind());
            assert!(
                !faults.is_empty() && faults.iter().all(|kind| *kind == fault),
                "{faults:?}"
            );
            let requests = fixture.requests().len();
            let unchanged = project_history(&records, agent) == before;
            let compactions = count!(&records, SessionEvent::Compaction { .. });
            let kept_todos = todos.inspect_todos(agent, None).unwrap() == old_todos;
            let outcome = (requests, unchanged, compactions, kept_todos);
            assert_eq!(outcome, (4, true, 0, true), "{fault}");
            fixture.assert_no_tool_execution();
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// A summary selecting an unknown job may succeed on retry; a checkpoint the
    /// journal refuses, here for an active job's non-trailing unanswered call,
    /// stops the session.
    #[tokio::test]
    async fn only_a_retryable_failure_retries_without_installing_compaction() {
        for invariant in [false, true] {
            let mut selected = summary_json();
            if !invariant {
                selected["jobs"] = json!([99999]);
            }
            let fixture = Fixture::new((0..3).map(|_| Step::new(summary(&selected)))).await;
            fixture.add_history(20_000).await;
            if invariant {
                let (runtime, root) = (&fixture.session.runtime, &fixture.session.root);
                let call = ToolCall::new("unanswered", "read", json!({"path":"file"})).unwrap();
                let call = Message::Assistant(vec![AssistantItem::tool_call("call", 0, call)]);
                let message = runtime.commit(root, call).await.unwrap();
                runtime
                    .commit(
                        root,
                        Message::User(vec![UserPart::Text {
                            text: "Continue before the tool result.".into(),
                        }]),
                    )
                    .await
                    .unwrap();
                let call_id = "unanswered".into();
                let spec = crate::job::JobSpec {
                    origin: Some(crate::session::ModelCallOrigin { message, call_id }),
                    ..crate::job::JobSpec::test(root.clone(), "read")
                };
                set_root_turn(&fixture.session, Turn::Busy);
                runtime.jobs.create(spec).await.unwrap().into_test_id();
            }
            let cancellation = CancellationToken::new();
            let compacted = retrying(&fixture.session, fixture.compact(&cancellation));
            let error = compacted.await.unwrap_err();
            let failed = matches!(
                error,
                HarnessError::Session(crate::session::SessionError::JournalFailed(_))
            );
            assert_eq!(failed, invariant, "{error}");
            let records = fixture.records();
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            let failed = count!(&records, SessionEvent::CompactionFailed { .. });
            assert_eq!(failed, if invariant { 0 } else { 3 }, "{error}");
            let shutdown = fixture.session.shutdown().await;
            assert_eq!(shutdown.is_ok(), !invariant);
        }
    }
}

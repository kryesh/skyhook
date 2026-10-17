//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use std::num::NonZeroU64;

use super::{
    HarnessError, SessionRuntime, TurnContext,
    recovery::{FailureRecord, TurnError},
};
use crate::{
    provider::{ProviderContext, ProviderErrorKind, protocol::ModelRequest},
    session::{AttemptRef, CompactionFailure, CompactionTrigger, ProfileSnapshot},
};

/// Context/validation recovery is bounded independently of transient retries.
pub(super) const MAX_COMPACTION_ATTEMPTS: u8 = 3;
/// Each validation retry waits this much longer than the one before.
pub(super) const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

mod checkpoint;
pub(super) mod continuation;
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
    /// Install a valid checkpoint, retrying recoverable summary failures. A failure
    /// that ends the turn, unless an interrupt, is handed back with its record.
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        profile: &ProfileSnapshot,
        input: &ModelRequest,
        trigger: CompactionTrigger,
    ) -> Result<(), TurnError> {
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
            // Only an attempt of this summary request belongs to its outcome, and the
            // count moves only once an attempt's start commits.
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
                // Refused before it requested anything, the turn ends interrupted
                // with no compaction to fail.
                Err(HarnessError::Interrupted) if request_sequence.is_none() => {
                    return Err(HarnessError::Interrupted.into());
                }
                Err(error) => {
                    use crate::agent::CompactionFault;
                    let fault = CompactionFault::from(&error);
                    let ends = !fault.retryable() || attempt == MAX_COMPACTION_ATTEMPTS;
                    if fault == CompactionFault::Overflowed {
                        fit = fit.after_overflow(ceiling.is_some());
                    }
                    let interrupted = fault == CompactionFault::Interrupted;
                    let record = FailureRecord::compaction(failure, fault);
                    if ends && !interrupted {
                        let record = Some(record);
                        return Err(TurnError { error, record });
                    }
                    // The backoff runs from the failure, however long its record takes to commit.
                    let backoff = tokio::time::sleep(RETRY_BACKOFF * u32::from(attempt));
                    self.record_failure(turn.agent, record).await?;
                    if ends {
                        return Err(error.into());
                    }
                    tokio::select! {
                        biased;
                        () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted.into()),
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

    use super::{TurnError, summary_max_output};
    pub(super) use crate::agent::runtime::tests::{
        Requests, Script, Sent, SentPart, Step, answer, bounded, count, cut, delta, events,
        interrupt_on, models, provider_name, recoverable, response, retrying, set_root_turn,
        spawn_prompt, summary_json, test_builder, todo, until_queued, usage,
    };
    use crate::agent::runtime::{HarnessError, SessionHandle, Turn, TurnContext, state};
    use crate::provider::profile::StateMode;
    use crate::{
        agent::{CompactionFault, TodoStatus},
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

        pub(super) async fn compact(&self) -> Result<(), HarnessError> {
            let compacted = self.compact_for(CompactionTrigger::Overflow).await;
            compacted.map_err(|failed| failed.error)
        }

        /// Compact for `trigger` as a turn of the root's own, which an interrupt
        /// stops as it would any. The record of a failure that would end the turn
        /// comes back unjournaled, as the turn's end journals it.
        pub(super) async fn compact_for(
            &self,
            trigger: CompactionTrigger,
        ) -> Result<(), TurnError> {
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
            let cancellation = CancellationToken::new();
            let turn = TurnContext {
                agent,
                owner_job: None,
                cancellation: &cancellation,
                location: &location,
                capabilities: capabilities.clone(),
            };
            let mut provider = self.script.open_context(ContextId::from(agent))?;
            let (root, token) = (agent.clone(), cancellation.clone());
            runtime.store.enqueue(move |state| {
                let facts = state.agents.get_mut(&root).expect("the root runs");
                facts.turn_token = token;
                facts.set_turn(Turn::Busy);
            });
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

    /// An interrupt that lands before the summary's attempt starts fails the round
    /// as requested; one after it fails the round on that attempt, the last thing
    /// sent. The failure names only an attempt that committed.
    #[tokio::test]
    async fn an_interrupt_either_side_of_a_summary_attempt_start_fails_on_what_committed() {
        use crate::provider::protocol::HistoryLifetime;
        let summarizing: fn(&SessionEvent) -> bool = |event| {
            matches!(event, SessionEvent::ModelRequested { history_lifetime, .. }
                if *history_lifetime == HistoryLifetime::Detached)
        };
        for started in [false, true] {
            let overflow = ProviderErrorKind::ContextWindowExceeded.error("prompt is too long");
            let steps = [
                Step::stream(vec![Err(overflow)]),
                Step::new(summary(summary_json())).gated(),
            ];
            let fixture = Fixture::new(steps).await;
            fixture.add_history(20_000).await;
            let session = &fixture.session;
            let behind = match started {
                true => None,
                false => Some(interrupt_on(session, summarizing).await),
            };
            let prompt = spawn_prompt(session, "Continue.");
            let interrupted = match behind {
                Some(interrupted) => bounded(interrupted).await,
                None => {
                    fixture.script.request(2).await;
                    session.interrupt().await
                }
            };
            assert_eq!(interrupted, 1);
            let interrupted = bounded(prompt).await.unwrap();
            assert!(matches!(interrupted, Err(HarnessError::Interrupted)));
            let records = fixture.records();
            let failed = events!(&records, SessionEvent::CompactionFailed { failure, error }
                => (*failure, error.clone()));
            let [(failure, CompactionFault::Interrupted)] = failed[..] else {
                panic!("{started}: {failed:?}")
            };
            let attempts = events!(&records, SessionEvent::ModelAttemptStarted(attempt)
                if Some(attempt.request) == failure.request() => *attempt);
            assert_eq!(failure.attempt(), attempts.last().copied(), "{started}");
            // The failure settles the summary request on that attempt.
            let ledger = fixture.session.observe().snapshot.ledger;
            let request = failure.request().and_then(|request| ledger.get(request));
            let attempt = attempts.last().map(|attempt| attempt.attempt);
            assert!(
                matches!(request.unwrap().phase,
                    crate::session::RequestPhase::Interrupted { attempt: at, .. } if at == attempt),
                "{started}"
            );
            assert_eq!(attempts.len(), usize::from(started));
            assert_eq!(fixture.requests().len(), 2 + usize::from(started));
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// An interrupt that lands once the summary is in, before its checkpoint
    /// commits, installs nothing: the round fails interrupted.
    #[tokio::test]
    async fn an_interrupt_before_the_checkpoint_commits_installs_nothing() {
        let mut summarized = summary(summary_json());
        let end = summarized.pop().unwrap();
        summarized.extend([ResponseEvent::Usage(usage(1, 0, 1)), end]);
        let fixture = Fixture::new([Step::new(summarized)]).await;
        fixture.add_history(20_000).await;
        let used: fn(&SessionEvent) -> bool = |event| matches!(event, SessionEvent::Usage { .. });
        let interrupted = interrupt_on(&fixture.session, used).await;
        let (result, stopped) =
            bounded(async { tokio::join!(fixture.compact(), interrupted) }).await;
        assert_eq!(stopped, 1);
        assert!(
            matches!(result, Err(HarnessError::Interrupted)),
            "{result:?}"
        );
        let records = fixture.records();
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let failed =
            events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
        assert_eq!(failed, [CompactionFault::Interrupted]);
        fixture.session.shutdown().await.unwrap();
    }

    /// An interrupt that lands right behind a queued batch joining at the boundary
    /// an overflow sent the turn back to ends it before its compaction journals
    /// anything: no summary context, request or failure.
    #[tokio::test]
    async fn an_interrupt_behind_a_join_ends_the_turn_before_its_compaction_journals() {
        let overflow = ProviderErrorKind::ContextWindowExceeded.error("prompt is too long");
        let steps = [
            Step::fail(overflow).gated(),
            Step::new(answer("unrequested")),
        ];
        let fixture = Fixture::new(steps).await;
        let session = &fixture.session;
        let prompt = spawn_prompt(session, "Continue.");
        fixture.script.request(1).await;
        let queued = crate::agent::QueuedPrompt {
            text: "joined".into(),
            ..Default::default()
        };
        let receipts = session.enqueue_prompts(vec![queued]).await;
        until_queued(session, &session.root).await;
        let joined: fn(&SessionEvent) -> bool = |event| {
            matches!(
                event,
                SessionEvent::MessageCommitted {
                    message: Message::User(_)
                }
            )
        };
        let interrupted = interrupt_on(session, joined).await;
        let from = fixture.records().len();
        fixture.script.release(1);
        assert_eq!(bounded(interrupted).await, 1);
        let interrupted = bounded(prompt).await.unwrap();
        assert!(
            matches!(interrupted, Err(HarnessError::Interrupted)),
            "{interrupted:?}"
        );
        for receipt in receipts {
            bounded(receipt.into_future()).await.unwrap();
        }
        let records = fixture.records();
        let compacting = records[from..].iter().filter(|record| {
            matches!(
                &record.event,
                SessionEvent::ModelContext { .. }
                    | SessionEvent::ModelRequested { .. }
                    | SessionEvent::CompactionFailed { .. }
            )
        });
        assert_eq!(compacting.count(), 0, "{:#?}", &records[from..]);
        fixture.session.shutdown().await.unwrap();
    }

    /// The fault of the compaction failure that ends a turn, carried for its end.
    fn final_fault(
        compacted: Result<(), TurnError>,
        agent: &crate::identity::AgentId,
    ) -> CompactionFault {
        let Err(TurnError {
            record: Some(record),
            ..
        }) = compacted
        else {
            panic!("compaction did not fail for good");
        };
        let faults = record.events(agent).filter_map(|(_, event)| match event {
            SessionEvent::CompactionFailed { error, .. } => Some(error),
            _ => None,
        });
        let [fault] = faults.collect::<Vec<_>>().try_into().unwrap();
        fault
    }

    /// A summary that fails for good ends the turn on its compaction failure, which
    /// commits right ahead of the turn's end and is reported by its request, live
    /// and once reopened: whether the failure settles the request, as a refused
    /// summary's does, or the summary's own model failure settled it first.
    #[tokio::test]
    async fn a_summary_failing_for_good_ends_the_turn_on_its_compaction_failure() {
        let partial = vec![AssistantItem::text("text/0", 0, "partial")];
        let refused = Step::new(cut(partial, CutReason::Refusal));
        let rejected = Step::fail(ProviderErrorKind::InvalidRequest.error("summary rejected"));
        for failing in [refused, rejected] {
            let overflow = ProviderErrorKind::ContextWindowExceeded.error("prompt is too long");
            let fixture = Fixture::new([Step::stream(vec![Err(overflow)]), failing]).await;
            fixture.add_history(20_000).await;
            let error = fixture.session.prompt("Continue.").await.unwrap_err();
            assert!(!matches!(error, HarnessError::Interrupted), "{error}");
            let records = fixture.records();
            let compaction = records
                .iter()
                .rposition(|record| matches!(record.event, SessionEvent::CompactionFailed { .. }));
            let ended = &records[compaction.unwrap() + 1];
            assert!(
                matches!(ended.event, SessionEvent::AgentFailed { .. }),
                "{:?}",
                ended.event
            );
            let live = fixture.session.observe().snapshot.ledger;
            let reopened = crate::session::RequestLedger::of(&records);
            for ledger in [live, reopened] {
                assert!(ledger.failed_by(ended.sequence).is_some(), "{error}");
            }
            fixture.session.shutdown().await.unwrap();
        }
    }

    /// A cut-off summary, even one holding a whole answer, is retried as truncated,
    /// as are a tool call and an unusable continuation; a refused one fails the
    /// round at once, diagnosed as what it was. None of them changes history or
    /// todos, or executes a tool.
    #[tokio::test]
    async fn failed_summaries_preserve_history_and_never_execute_tools() {
        use crate::agent::FailureKind;
        let arguments = json!({"path":"must-not-exist", "content":"side effect"});
        let call = ToolCall::new("never-execute", "write", arguments).unwrap();
        let tool_call = response(vec![AssistantItem::tool_call("never-execute", 0, call)]);
        let reasoning = AssistantItem::reasoning("reasoning/0", 0, "thinking", None);
        let text = AssistantItem::text("text/1", 1, summary_json().to_string());
        let partial = || vec![AssistantItem::text("text/0", 0, "partial")];
        let mut blank_todo = summary_json();
        blank_todo["todos"] = json!([{"text":" \t", "status":"pending"}]);
        type Expected = fn(&CompactionFault) -> bool;
        let cases: [(_, usize, Expected); 5] = [
            (
                cut(vec![reasoning, text], CutReason::MaxTokens),
                3,
                |fault| matches!(fault, CompactionFault::Truncated),
            ),
            (cut(partial(), CutReason::Incomplete), 3, |fault| {
                matches!(fault, CompactionFault::Truncated)
            }),
            (tool_call, 3, |fault| {
                matches!(fault, CompactionFault::ToolCall)
            }),
            (summary(blank_todo), 3, |fault| {
                matches!(fault, CompactionFault::Continuation(_))
            }),
            (cut(partial(), CutReason::Refusal), 1, |fault| {
                matches!(fault, CompactionFault::Summary(FailureKind::Refused))
            }),
        ];
        for (events, attempts, expected) in cases {
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
            let compacted = fixture.compact_for(CompactionTrigger::Overflow);
            let fault = final_fault(retrying(&fixture.session, compacted).await, agent);
            assert!(expected(&fault), "{fault}");
            let records = fixture.records();
            let faults =
                events!(&records, SessionEvent::CompactionFailed { error, .. } => error.clone());
            assert_eq!(faults, vec![fault.clone(); attempts - 1]);
            let requests = fixture.requests().len() - 1;
            let unchanged = project_history(&records, agent) == before;
            let compactions = count!(&records, SessionEvent::Compaction { .. });
            let kept_todos = todos.inspect_todos(agent, None).unwrap() == old_todos;
            let outcome = (requests, unchanged, compactions, kept_todos);
            assert_eq!(outcome, (attempts, true, 0, true), "{fault}");
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
            let compacted = fixture.compact_for(CompactionTrigger::Overflow);
            let Err(TurnError { error, record }) = retrying(&fixture.session, compacted).await
            else {
                panic!("compaction succeeded")
            };
            let failed = matches!(
                error,
                HarnessError::Session(crate::session::SessionError::JournalFailed(_))
            );
            assert_eq!(failed, invariant, "{error}");
            assert!(record.is_some(), "{error}");
            let records = fixture.records();
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            let failed = count!(&records, SessionEvent::CompactionFailed { .. });
            assert_eq!(failed, if invariant { 0 } else { 2 }, "{error}");
            let shutdown = fixture.session.shutdown().await;
            assert_eq!(shutdown.is_ok(), !invariant);
        }
    }
}

//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::{
    identity::AgentId,
    provider::{
        ProviderContext,
        protocol::{Message, ModelRequest, Usage},
    },
    session::{EventRecord, ModelPurpose, SessionEvent},
};

/// Context/validation recovery is bounded independently of transient retries.
pub(super) const MAX_COMPACTION_ATTEMPTS: u8 = 3;

mod checkpoint;
mod retention;
mod summary;
use checkpoint::CompactionInput;

pub(super) async fn retry_delay(
    cancellation: &crate::job::CancellationToken,
    attempt: u8,
) -> Result<(), HarnessError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(HarnessError::Interrupted),
        () = tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt))) => Ok(()),
    }
}

/// Calibrate the next estimate against the last successful request with the same template.
#[derive(Default)]
pub(super) struct TokenMeter {
    baseline: Option<(u64, u64)>,
}

impl TokenMeter {
    pub(super) fn restore(
        records: &[EventRecord],
        agent: &AgentId,
        template: &ModelRequest,
    ) -> Self {
        let mut meter = Self::default();
        for record in records.iter().rev().filter(|record| &record.agent == agent) {
            if matches!(
                record.event,
                SessionEvent::Compaction { .. } | SessionEvent::ModelChanged { .. }
            ) {
                break;
            }
            let SessionEvent::Usage {
                request: Some(request),
                usage,
            } = &record.event
            else {
                continue;
            };
            let Some(request_event) = records.iter().find(|record| record.sequence == *request)
            else {
                continue;
            };
            let SessionEvent::ModelRequested {
                context,
                purpose: ModelPurpose::Agent,
                ..
            } = &request_event.event
            else {
                continue;
            };
            let same_template = records.iter().any(|record| record.sequence == *context && matches!(&record.event, SessionEvent::ModelContext { template: original, .. } if original == template));
            if same_template
                && let Ok((_, request)) =
                    crate::session::reconstruct_model_request(records, *request)
            {
                meter.observe(compaction::estimate_request(&request), *usage);
            }
            break;
        }
        meter
    }

    pub(super) fn estimate(&self, request: &ModelRequest) -> u64 {
        let estimate = compaction::estimate_request(request);
        self.baseline.map_or(estimate, |(old_estimate, actual)| {
            estimate.max(actual.saturating_add(estimate).saturating_sub(old_estimate))
        })
    }

    pub(super) fn observe(&mut self, estimate: u64, usage: Usage) {
        let actual = usage.input_tokens.saturating_add(usage.cached_input_tokens);
        if actual > 0 {
            self.baseline = Some((estimate, actual));
        }
    }
}

pub(super) fn context_sources(projected: &[(u64, Message)]) -> Vec<u64> {
    projected.iter().map(|(sequence, _)| *sequence).collect()
}

impl SessionRuntime {
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        context: u64,
        input: &ModelRequest,
        max_context: u64,
    ) -> Result<(), HarnessError> {
        let mut launches = self.jobs.active_launches(turn.agent).await;
        let mut model_attempt = 0;
        for attempt in 1..=MAX_COMPACTION_ATTEMPTS {
            let mut request_sequence = None;
            match self
                .compact_inner(
                    turn,
                    provider,
                    CompactionInput {
                        context,
                        request: input,
                        max_context,
                        model_attempt: &mut model_attempt,
                    },
                    &mut request_sequence,
                    &mut launches,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    self.store
                        .append(
                            turn.agent.clone(),
                            SessionEvent::CompactionFailed {
                                request: request_sequence,
                                error: format!(
                                    "attempt {attempt}/{MAX_COMPACTION_ATTEMPTS}: {error}"
                                ),
                            },
                        )
                        .await?;
                    let retryable =
                        request_sequence.is_some() && matches!(&error, HarnessError::Compaction(_));
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
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use futures_util::{StreamExt, stream};
    use serde_json::json;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    pub(super) use crate::agent::runtime::tests::{
        count, events, summary_json, test_builder, todo,
    };
    use crate::agent::runtime::{HarnessError, SessionHandle, TurnContext, compaction, prompt};
    use crate::{
        agent::TodoStatus,
        execution::ExecutionLocation,
        provider::{
            Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
            ResponseStream,
            protocol::{
                AssistantContent, Message, ModelRequest, ResponseChunk, StopReason, ToolCall,
                Usage, UserContent, events_for_content,
            },
        },
        session::{SessionEvent, project_history},
        tool::policy::CapabilitySet,
    };

    #[derive(Default)]
    pub(super) struct ControlledProvider {
        pub(super) opened: AtomicUsize,
        pub(super) requests: StdMutex<Vec<ModelRequest>>,
        pub(super) summary: StdMutex<String>,
        pub(super) overflow: AtomicBool,
        pub(super) truncate: AtomicBool,
        pub(super) block: AtomicBool,
        pub(super) agent_immediate_failures: AtomicUsize,
        pub(super) agent_stream_failures: AtomicUsize,
        pub(super) summary_immediate_failures: AtomicUsize,
        pub(super) summary_stream_failures: AtomicUsize,
        pub(super) summary_tools: AtomicBool,
        pub(super) observed_failure_usage: StdMutex<Option<Usage>>,
        pub(super) pause_stream_after_usage: AtomicBool,
        pub(super) started: Notify,
        pub(super) release: Notify,
    }

    impl Provider for Arc<ControlledProvider> {
        fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(self.clone()))
        }
    }

    fn side_effect(id: &str) -> Vec<AssistantContent> {
        let arguments = json!({"path":"must-not-exist", "content":"side effect"});
        let call = ToolCall::new(id, "write", arguments).unwrap();
        vec![AssistantContent::tool_call(id, 0, call)]
    }

    impl ProviderContext for Arc<ControlledProvider> {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            let provider = self.clone();
            Box::pin(async move {
                let summary = request.tail.last() == Some(&compaction::directive());
                provider.requests.lock().unwrap().push(request);
                let counters = [
                    (
                        &provider.agent_immediate_failures,
                        &provider.agent_stream_failures,
                    ),
                    (
                        &provider.summary_immediate_failures,
                        &provider.summary_stream_failures,
                    ),
                ];
                let (immediate, streaming) = counters[usize::from(summary)];
                let error = || ProviderError {
                    retry_after: None,
                    kind: ProviderErrorKind::Transport,
                    message: "deterministic transient failure".into(),
                };
                let usage = *provider.observed_failure_usage.lock().unwrap();
                let usage = usage.map(|usage| Ok(ResponseChunk::UsageUpdated { usage }));
                if provider.pause_stream_after_usage.load(Ordering::SeqCst) {
                    let mut chunks = vec![usage.unwrap()];
                    let call = events_for_content(&side_effect("interrupted-tool"));
                    chunks.extend(call.into_iter().map(Ok));
                    let tail = stream::once(async move {
                        provider.started.notify_one();
                        std::future::pending::<Result<ResponseChunk, ProviderError>>().await
                    });
                    return Ok(Box::pin(stream::iter(chunks).chain(tail)) as ResponseStream);
                }
                if consume_failure(immediate) {
                    return Err(error());
                }
                if consume_failure(streaming) {
                    let chunks = usage.into_iter().chain([Err(error())]);
                    return Ok(Box::pin(stream::iter(chunks)) as ResponseStream);
                }
                let chunks = if summary {
                    provider.started.notify_one();
                    if provider.block.load(Ordering::SeqCst) {
                        provider.release.notified().await;
                    }
                    if provider.summary_tools.load(Ordering::SeqCst) {
                        response_chunks(side_effect("never-execute"), StopReason::ToolUse)
                    } else {
                        let reasoning = "Reasoning before the answer is not JSON and must not enter the continuation.";
                        let text = provider.summary.lock().unwrap().clone();
                        let items = vec![
                            AssistantContent::reasoning("reasoning/0", 0, reasoning, None),
                            AssistantContent::text("text/1", 1, text),
                        ];
                        let truncate = usize::from(provider.truncate.load(Ordering::SeqCst));
                        let stop = [StopReason::EndTurn, StopReason::MaxTokens][truncate].clone();
                        response_chunks(items, stop)
                    }
                } else if provider.overflow.swap(false, Ordering::SeqCst) {
                    vec![Err(ProviderError {
                        retry_after: None,
                        kind: ProviderErrorKind::ContextWindowExceeded,
                        message: "prompt is too long".into(),
                    })]
                } else {
                    let done = vec![AssistantContent::text("text/0", 0, "done")];
                    response_chunks(done, StopReason::EndTurn)
                };
                Ok(Box::pin(stream::iter(chunks)) as ResponseStream)
            })
        }
    }

    fn response_chunks(
        items: Vec<AssistantContent>,
        stop_reason: StopReason,
    ) -> Vec<Result<ResponseChunk, ProviderError>> {
        let mut events = events_for_content(&items);
        events.push(ResponseChunk::ResponseEnded { stop_reason });
        events.into_iter().map(Ok).collect()
    }

    fn consume_failure(counter: &AtomicUsize) -> bool {
        let decrement = |remaining: usize| remaining.checked_sub(1);
        counter
            .try_update(Ordering::SeqCst, Ordering::SeqCst, decrement)
            .is_ok()
    }

    pub(super) fn usage(input_tokens: u64, cached_input_tokens: u64, output_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        }
    }

    pub(super) struct Fixture {
        pub(super) workspace: tempfile::TempDir,
        pub(super) session: SessionHandle,
        pub(super) provider: Arc<ControlledProvider>,
        pub(super) template: ModelRequest,
    }

    impl Fixture {
        pub(super) async fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let provider = Arc::new(ControlledProvider::default());
            let sessions = workspace.path().join("sessions");
            let factory = Arc::new(provider.clone());
            let builder = test_builder(workspace.path(), &sessions, factory, false);
            let session = builder.build().await.unwrap().new_session().await.unwrap();
            let research =
                session.prompt("Research the existing task and preserve its constraints.");
            research.await.unwrap();
            let mut template = provider.requests.lock().unwrap()[0].clone();
            (template.history, template.tail) = (Vec::new(), Vec::new());
            template.history_lifetime = Default::default();
            *provider.summary.lock().unwrap() = summary_json().to_string();
            Self {
                workspace,
                session,
                provider,
                template,
            }
        }

        pub(super) fn set_summary(&self, summary: serde_json::Value) {
            *self.provider.summary.lock().unwrap() = summary.to_string();
        }

        pub(super) async fn add_history(&self, tokens: usize) {
            let (runtime, root) = (&self.session.runtime, &self.session.root);
            let research = "research ".repeat(tokens * 4 / 9);
            let research = Message::Assistant(vec![AssistantContent::text("text/0", 0, research)]);
            runtime.commit(root, research).await.unwrap();
            let text = "Continue the existing task.".into();
            let next = Message::User(vec![UserContent::Text { text }]);
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
            let context = SessionEvent::ModelContext {
                provider: "test".into(),
                template: self.template.clone(),
            };
            let store = &runtime.store;
            let context = store.append(agent.clone(), context).await.unwrap().sequence;
            let mut input = self.template.clone();
            let history = project_history(&runtime.store.records().await, agent).unwrap();
            input.history = history.into_iter().map(|(_, message)| message).collect();
            let capabilities = CapabilitySet::default();
            let location = ExecutionLocation::root(self.workspace.path().to_path_buf());
            let (jobs, todos) = (&runtime.jobs, &runtime.todos);
            let state =
                prompt::runtime_state_content(jobs, todos, agent, &capabilities, &location).await;
            input.tail = vec![Message::User(vec![state])];
            let turn = TurnContext {
                agent,
                owner_job: None,
                cancellation,
                location: &location,
                capabilities: &capabilities,
            };
            let mut provider = self.provider.open_context(agent.to_string())?;
            runtime
                .compact_history(&turn, provider.as_mut(), context, &input, 128_000)
                .await
        }
    }

    #[tokio::test]
    async fn stream_overflow_below_threshold_compacts_once() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        fixture.provider.overflow.store(true, Ordering::SeqCst);
        assert_eq!(fixture.session.prompt("Continue.").await.unwrap(), "done");
        // Summarization and retry retain the agent's context.
        assert_eq!(fixture.provider.opened.load(Ordering::SeqCst), 1);
        let requests = fixture.provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 4); // initial response, rejected stream, summary, retry
        assert!(compaction::estimate_request(&requests[1]) < 128_000 * 4 / 5);
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_blocked_summarizer_does_not_activate_a_checkpoint() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        fixture.provider.block.store(true, Ordering::SeqCst);
        let todos = vec![todo("Keep on interruption", TodoStatus::InProgress)];
        runtime.todos.replace(agent, todos.clone()).await.unwrap();
        let before = project_history(&fixture.records().await, agent).unwrap();
        let cancellation = CancellationToken::new();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(fixture.compact(&cancellation), async {
                fixture.provider.started.notified().await;
                cancellation.cancel();
            })
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        let records = fixture.records().await;
        assert_eq!(project_history(&records, agent).unwrap(), before);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let found = runtime.todos.inspect(agent, None).await.unwrap().items;
        assert_eq!(found, todos);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_summary_failures_preserve_history_and_never_execute_tools() {
        // Parser field cases live in compaction.rs; this covers rollback and tool contracts.
        for failure in ["truncated", "tool_call", "blank_todo"] {
            let fixture = Fixture::new().await;
            let (todos, agent) = (&fixture.session.runtime.todos, &fixture.session.root);
            fixture.add_history(20_000).await;
            let old_todos = vec![todo("Preserve unfinished work", TodoStatus::InProgress)];
            todos.replace(agent, old_todos.clone()).await.unwrap();
            match failure {
                "truncated" => fixture.provider.truncate.store(true, Ordering::SeqCst),
                "tool_call" => fixture.provider.summary_tools.store(true, Ordering::SeqCst),
                _ => {
                    let mut invalid = summary_json();
                    invalid["todos"] = json!([{"text":" \t", "status":"pending"}]);
                    fixture.set_summary(invalid);
                }
            }
            let before = project_history(&fixture.records().await, agent).unwrap();
            let cancellation = CancellationToken::new();
            let error = fixture.compact(&cancellation).await.unwrap_err();
            let records = fixture.records().await;
            if failure == "truncated" {
                assert!(error.to_string().contains("truncated"));
                let failed = count!(&records, SessionEvent::CompactionFailed { request, .. } if request.is_some());
                assert_ne!(failed, 0);
            }
            let requests = fixture.provider.requests.lock().unwrap().len();
            let unchanged = project_history(&records, agent).unwrap() == before;
            let compactions = count!(&records, SessionEvent::Compaction { .. });
            let kept_todos = todos.inspect(agent, None).await.unwrap().items == old_todos;
            let outcome = (requests, unchanged, compactions, kept_todos);
            assert_eq!(outcome, (4, true, 0, true), "{failure}");
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_selected_job_retries_without_installing_compaction() {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let mut summary = summary_json();
        summary["jobs"] = json!([99999]);
        fixture.set_summary(summary);
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        assert_eq!(count!(&records, SessionEvent::CompactionFailed { .. }), 3);
        fixture.session.shutdown().await.unwrap();
    }
}

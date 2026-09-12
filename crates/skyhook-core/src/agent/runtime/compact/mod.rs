//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use super::{HarnessError, SessionRuntime, TurnContext, compaction};
use crate::{
    identity::AgentId,
    provider::{
        ProviderContext,
        protocol::{Message, ModelRequest, Usage},
    },
    session::{ContextMessage, EventRecord, ModelPurpose, SessionEvent},
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

pub(super) fn context_sources(projected: &[(u64, Message)]) -> Vec<ContextMessage> {
    projected
        .iter()
        .map(|(sequence, _)| ContextMessage::Source {
            sequence: *sequence,
        })
        .collect()
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
    use serde_json::{Value, json};
    use tokio::sync::{Notify, Semaphore};
    use tokio_util::sync::CancellationToken;

    use crate::agent::runtime::{
        HarnessBuilder, HarnessError, SessionHandle, TurnContext, compaction, prompt,
    };
    use crate::{
        agent::{TodoItem, TodoStatus},
        execution::ExecutionLocation,
        provider::{
            Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
            ResponseStream,
            profile::ModelProfile,
            protocol::{
                AssistantContent, Message, ModelRequest, ResponseChunk, StopReason, Usage,
                UserContent, events_for_content,
            },
        },
        session::{SessionEvent, project_history},
        tool::policy::CapabilitySet,
    };

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
        pub(super) release: Semaphore,
    }

    impl Default for ControlledProvider {
        fn default() -> Self {
            Self {
                opened: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                summary: StdMutex::new(String::new()),
                overflow: AtomicBool::new(false),
                truncate: AtomicBool::new(false),
                block: AtomicBool::new(false),
                agent_immediate_failures: AtomicUsize::new(0),
                agent_stream_failures: AtomicUsize::new(0),
                summary_immediate_failures: AtomicUsize::new(0),
                summary_stream_failures: AtomicUsize::new(0),
                summary_tools: AtomicBool::new(false),
                observed_failure_usage: StdMutex::new(None),
                pause_stream_after_usage: AtomicBool::new(false),
                started: Notify::new(),
                release: Semaphore::new(0),
            }
        }
    }

    impl Provider for Arc<ControlledProvider> {
        fn open_context(
            &self,
            _correlation: String,
        ) -> Result<Box<dyn ProviderContext>, ProviderError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(self.clone()))
        }
    }

    impl ProviderContext for Arc<ControlledProvider> {
        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            let provider = self.clone();
            Box::pin(async move {
                let summary = request.messages.last() == Some(&compaction::directive());
                provider.requests.lock().unwrap().push(request);
                let (immediate, streaming) = if summary {
                    (
                        &provider.summary_immediate_failures,
                        &provider.summary_stream_failures,
                    )
                } else {
                    (
                        &provider.agent_immediate_failures,
                        &provider.agent_stream_failures,
                    )
                };
                let error = || ProviderError {
                    retry_after: None,
                    kind: ProviderErrorKind::Transport,
                    message: "deterministic transient failure".into(),
                };
                if provider.pause_stream_after_usage.load(Ordering::SeqCst) {
                    let usage = provider.observed_failure_usage.lock().unwrap().unwrap();
                    let mut chunks = vec![Ok(ResponseChunk::UsageUpdated { usage })];
                    chunks.extend(
                        events_for_content(&[AssistantContent::tool_call(
                            "interrupted-tool",
                            0,
                            crate::provider::protocol::ToolCall {
                                id: "interrupted-tool".into(),
                                name: "write".into(),
                                arguments: json!({"path":"must-not-exist", "content":"side effect"}),
                            },
                        )])
                        .into_iter()
                        .map(Ok),
                    );
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
                    let mut chunks = Vec::new();
                    if let Some(usage) = *provider.observed_failure_usage.lock().unwrap() {
                        chunks.push(Ok(ResponseChunk::UsageUpdated { usage }));
                    }
                    chunks.push(Err(error()));
                    return Ok(Box::pin(stream::iter(chunks)) as ResponseStream);
                }
                let chunks = if summary {
                    provider.started.notify_one();
                    if provider.block.load(Ordering::SeqCst) {
                        provider.release.acquire().await.unwrap().forget();
                    }
                    if provider.summary_tools.load(Ordering::SeqCst) {
                        response_chunks(
                            vec![AssistantContent::tool_call(
                                "never-execute",
                                0,
                                crate::provider::protocol::ToolCall {
                                    id: "never-execute".into(),
                                    name: "write".into(),
                                    arguments: json!({"path":"must-not-exist", "content":"side effect"}),
                                },
                            )],
                            StopReason::ToolUse,
                        )
                    } else {
                        response_chunks(
                            vec![
                                AssistantContent::reasoning(
                                    "reasoning/0",
                                    0,
                                    "Reasoning before the answer is not JSON and must not enter the continuation.",
                                    None,
                                ),
                                AssistantContent::text(
                                    "text/1",
                                    1,
                                    provider.summary.lock().unwrap().clone(),
                                ),
                            ],
                            if provider.truncate.load(Ordering::SeqCst) {
                                StopReason::MaxTokens
                            } else {
                                StopReason::EndTurn
                            },
                        )
                    }
                } else if provider.overflow.swap(false, Ordering::SeqCst) {
                    vec![Err(ProviderError {
                        retry_after: None,
                        kind: ProviderErrorKind::ContextWindowExceeded,
                        message: "prompt is too long".into(),
                    })]
                } else {
                    response_chunks(
                        vec![AssistantContent::text("text/0", 0, "done")],
                        StopReason::EndTurn,
                    )
                };
                Ok(Box::pin(stream::iter(chunks)) as ResponseStream)
            })
        }
    }

    pub(super) fn response_chunks(
        items: Vec<AssistantContent>,
        stop_reason: StopReason,
    ) -> Vec<Result<ResponseChunk, ProviderError>> {
        let mut events = events_for_content(&items);
        events.push(ResponseChunk::ResponseEnded { stop_reason });
        events.into_iter().map(Ok).collect()
    }

    pub(super) fn consume_failure(counter: &AtomicUsize) -> bool {
        let mut remaining = counter.load(Ordering::SeqCst);
        while remaining > 0 {
            match counter.compare_exchange_weak(
                remaining,
                remaining - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => remaining = actual,
            }
        }
        false
    }

    pub(super) fn summary_value() -> Value {
        json!({
            "objective": "Continue the existing task and preserve its constraints.",
            "user_instructions": [],
            "session_rules": [],
            "plan": [],
            "resumption_point": "Research is complete.",
            "completed_work": [],
            "findings": [],
            "decisions": [],
            "open_issues": [],
            "next_actions": [],
            "running_work": [],
            "recovery_details": [],
            "jobs": [],
            "additional_context": [],
            "todo_reconciliation": [],
            "todos": []
        })
    }

    pub(super) fn profile() -> ModelProfile {
        ModelProfile {
            provider: "test".into(),
            model: "test".into(),
            reasoning: None,
            max_context: 128_000,
            max_output: 16_384,
            supports_images: false,
        }
    }

    pub(super) struct Fixture {
        pub(super) _workspace: tempfile::TempDir,
        pub(super) _sessions: tempfile::TempDir,
        pub(super) session: SessionHandle,
        pub(super) provider: Arc<ControlledProvider>,
        pub(super) template: ModelRequest,
    }

    impl Fixture {
        pub(super) async fn new() -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = tempfile::tempdir().unwrap();
            let provider = Arc::new(ControlledProvider::default());
            let harness = HarnessBuilder::new(workspace.path())
                .session_root(sessions.path())
                .provider("test", Arc::new(provider.clone()))
                .model_profile("test", profile())
                .default_model_profile("test")
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            session
                .prompt("Research the existing task and preserve its constraints.")
                .await
                .unwrap();
            let mut template = provider.requests.lock().unwrap()[0].clone();
            template.messages.clear();
            *provider.summary.lock().unwrap() = summary_value().to_string();
            Self {
                _workspace: workspace,
                _sessions: sessions,
                session,
                provider,
                template,
            }
        }

        pub(super) async fn add_history(&self, tokens: usize) {
            self.session
                .runtime
                .commit(
                    &self.session.root,
                    Message::Assistant(vec![AssistantContent::text(
                        "text/0",
                        0,
                        "research ".repeat(tokens * 4 / 9),
                    )]),
                )
                .await
                .unwrap();
            self.session
                .runtime
                .commit(
                    &self.session.root,
                    Message::User(vec![UserContent::Text {
                        text: "Continue the existing task.".into(),
                    }]),
                )
                .await
                .unwrap();
        }

        pub(super) async fn assert_no_tool_execution(&self) {
            let records = self.session.runtime.store.records().await;
            assert!(
                !records
                    .iter()
                    .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
            );
            assert!(!self._workspace.path().join("must-not-exist").exists());
        }

        pub(super) async fn compact(
            &self,
            cancellation: &CancellationToken,
        ) -> Result<(), HarnessError> {
            let runtime = &self.session.runtime;
            let agent = &self.session.root;
            let context = runtime
                .store
                .append(
                    agent.clone(),
                    SessionEvent::ModelContext {
                        provider: "test".into(),
                        template: self.template.clone(),
                    },
                )
                .await
                .unwrap()
                .sequence;
            let mut input = self.template.clone();
            input.messages = project_history(&runtime.store.records().await, agent)
                .unwrap()
                .into_iter()
                .map(|(_, message)| message)
                .collect();
            let capabilities = CapabilitySet::default();
            let location = ExecutionLocation::root(self._workspace.path().to_path_buf());
            input.messages.push(Message::User(vec![
                prompt::runtime_state_content(
                    &runtime.jobs,
                    &runtime.todos,
                    agent,
                    &capabilities,
                    &location,
                )
                .await,
            ]));
            runtime
                .compact_history(
                    &TurnContext {
                        agent,
                        owner_job: None,
                        cancellation,
                        location: &location,
                        capabilities: &capabilities,
                    },
                    self.provider.open_context(agent.to_string())?.as_mut(),
                    context,
                    &input,
                    profile().max_context,
                )
                .await
        }
    }

    #[tokio::test]
    async fn stream_overflow_below_threshold_compacts_once() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        fixture.add_history(20_000).await;
        fixture.provider.overflow.store(true, Ordering::SeqCst);
        assert_eq!(fixture.session.prompt("Continue.").await.unwrap(), "done");
        assert_eq!(
            fixture.provider.opened.load(Ordering::SeqCst),
            1,
            "summarization and retry retain the agent's context"
        );
        let records = runtime.store.records().await;
        let requests = fixture.provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 4); // initial response, rejected stream, summary, retry
        assert!(compaction::estimate_request(&requests[1]) < 128_000 * 4 / 5);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::Compaction { .. }))
                .count(),
            1
        );

        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_blocked_summarizer_does_not_activate_a_checkpoint() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;
        let agent = &fixture.session.root;
        fixture.add_history(20_000).await;
        fixture.provider.block.store(true, Ordering::SeqCst);
        let todos = vec![TodoItem {
            text: "Do not discard this task on interruption".into(),
            status: TodoStatus::InProgress,
        }];
        runtime.todos.replace(agent, todos.clone()).await.unwrap();
        let before = project_history(&runtime.store.records().await, agent).unwrap();
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
        let records = runtime.store.records().await;
        assert_eq!(project_history(&records, agent).unwrap(), before);
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        );
        assert_eq!(
            runtime.todos.inspect(agent, None).await.unwrap().items,
            todos
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn exhausted_summary_failures_preserve_history_and_never_execute_tools() {
        // Exhaustive malformed-field cases belong to the continuation parser tests.
        // Here retain provider, truncation, tool-execution and semantic rollback contracts.
        for failure in ["truncated", "tool_call", "blank_todo"] {
            let fixture = Fixture::new().await;
            let runtime = &fixture.session.runtime;
            let agent = &fixture.session.root;
            fixture.add_history(20_000).await;
            let old_todos = vec![TodoItem {
                text: "Preserve unfinished work".into(),
                status: TodoStatus::InProgress,
            }];
            runtime
                .todos
                .replace(agent, old_todos.clone())
                .await
                .unwrap();
            let mut invalid = summary_value();
            match failure {
                "truncated" => fixture.provider.truncate.store(true, Ordering::SeqCst),
                "tool_call" => fixture.provider.summary_tools.store(true, Ordering::SeqCst),
                "blank_todo" => invalid["todos"] = json!([{"text":" \t", "status":"pending"}]),
                _ => unreachable!(),
            }
            if failure == "blank_todo" {
                *fixture.provider.summary.lock().unwrap() = invalid.to_string();
            }
            let before = project_history(&runtime.store.records().await, agent).unwrap();
            let error = fixture
                .compact(&CancellationToken::new())
                .await
                .unwrap_err();
            let records = runtime.store.records().await;
            if failure == "truncated" {
                assert!(error.to_string().contains("truncated"));
                assert!(records.iter().any(|record| matches!(
                    record.event,
                    SessionEvent::CompactionFailed {
                        request: Some(_),
                        ..
                    }
                )));
            }
            assert_eq!(
                fixture.provider.requests.lock().unwrap().len(),
                4,
                "{failure}"
            );
            assert_eq!(
                project_history(&records, agent).unwrap(),
                before,
                "{failure}"
            );
            assert!(
                !records
                    .iter()
                    .any(|record| matches!(record.event, SessionEvent::Compaction { .. })),
                "{failure}"
            );
            assert_eq!(
                runtime.todos.inspect(agent, None).await.unwrap().items,
                old_todos,
                "{failure}"
            );
            fixture.assert_no_tool_execution().await;
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_selected_job_retries_without_installing_compaction() {
        let fixture = Fixture::new().await;
        let runtime = &fixture.session.runtime;

        fixture.add_history(20_000).await;
        let mut summary = summary_value();
        summary["jobs"] = json!([99999]);
        *fixture.provider.summary.lock().unwrap() = summary.to_string();
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        let records = runtime.store.records().await;
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::CompactionFailed { .. }))
                .count(),
            3
        );
        fixture.session.shutdown().await.unwrap();
    }
}

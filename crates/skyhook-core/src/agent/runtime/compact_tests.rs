//! Compaction lifecycle tests with deterministic local provider streams.

use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{HarnessBuilder, HarnessError, SessionHandle, TurnContext, compaction, prompt};
use crate::{
    agent::{TodoItem, TodoStatus},
    execution::ExecutionLocation,
    job::{JobOutcome, JobSpec, JobState},
    provider::{
        Provider, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
        ResponseStream,
        profile::ModelProfile,
        protocol::{
            AssistantContent, Message, ModelRequest, ResponseChunk, StopReason, Usage, UserContent,
            events_for_content,
        },
    },
    session::{ModelPurpose, SessionEvent, project_history, reconstruct_model_request},
    tool::{ToolOutput, policy::CapabilitySet},
};

struct ControlledProvider {
    opened: AtomicUsize,
    requests: StdMutex<Vec<ModelRequest>>,
    summary: StdMutex<String>,
    overflow: AtomicBool,
    truncate: AtomicBool,
    block: AtomicBool,
    agent_immediate_failures: AtomicUsize,
    agent_stream_failures: AtomicUsize,
    agent_failure_tool_blocks: AtomicBool,
    agent_empty_responses: AtomicUsize,
    summary_immediate_failures: AtomicUsize,
    summary_stream_failures: AtomicUsize,
    summary_tools: AtomicBool,
    observed_failure_usage: StdMutex<Option<Usage>>,
    pause_stream_after_usage: AtomicBool,
    started: Notify,
    release: Semaphore,
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
            agent_failure_tool_blocks: AtomicBool::new(false),
            agent_empty_responses: AtomicUsize::new(0),
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
                if !summary && provider.agent_failure_tool_blocks.load(Ordering::SeqCst) {
                    chunks.extend(events_for_content(&[AssistantContent::tool_call(
                        "failed-attempt-tool", 0,
                        crate::provider::protocol::ToolCall {
                            id: "failed-attempt-tool".into(),
                            name: "write".into(),
                            arguments: json!({"path":"must-not-exist", "content":"side effect"}),
                        },
                    )]).into_iter().map(Ok));
                }
                chunks.push(Err(error()));
                return Ok(Box::pin(stream::iter(chunks)) as ResponseStream);
            }
            if !summary && consume_failure(&provider.agent_empty_responses) {
                let chunks: Vec<_> = provider
                    .observed_failure_usage
                    .lock()
                    .unwrap()
                    .map(|usage| Ok(ResponseChunk::UsageUpdated { usage }))
                    .into_iter()
                    .collect();
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

fn response_chunks(
    items: Vec<AssistantContent>,
    stop_reason: StopReason,
) -> Vec<Result<ResponseChunk, ProviderError>> {
    let mut events = events_for_content(&items);
    events.push(ResponseChunk::ResponseEnded { stop_reason });
    events.into_iter().map(Ok).collect()
}

fn consume_failure(counter: &AtomicUsize) -> bool {
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

fn summary_value() -> Value {
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

fn profile() -> ModelProfile {
    ModelProfile {
        provider: "test".into(),
        model: "test".into(),
        reasoning: None,
        max_context: 128_000,
        max_output: 16_384,
        supports_images: false,
    }
}

struct Fixture {
    _workspace: tempfile::TempDir,
    _sessions: tempfile::TempDir,
    session: SessionHandle,
    provider: Arc<ControlledProvider>,
    template: ModelRequest,
}

impl Fixture {
    async fn new() -> Self {
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

    async fn add_history(&self, tokens: usize) {
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

    async fn assert_exact_requests(&self) {
        let records = self.session.runtime.store.records().await;
        let replay: Vec<_> = records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
            .map(|record| {
                reconstruct_model_request(&records, record.sequence)
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(replay, *self.provider.requests.lock().unwrap());
    }

    async fn assert_exact_requests_and_no_tool_execution(&self) {
        self.assert_exact_requests().await;
        let records = self.session.runtime.store.records().await;
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
        );
        assert!(!self._workspace.path().join("must-not-exist").exists());
    }

    async fn compact(&self, cancellation: &CancellationToken) -> Result<(), HarnessError> {
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
        input.messages.push(Message::User(vec![
            prompt::runtime_state_content(&runtime.jobs, &runtime.todos, agent, &capabilities)
                .await,
        ]));
        let location = ExecutionLocation::root(self._workspace.path().to_path_buf());
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
                Message::User(parts) => images.extend(parts.iter().filter_map(|part| match part {
                    UserContent::Image { image } => Some(image),
                    _ => None,
                })),
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
async fn stream_overflow_below_threshold_compacts_once_and_replays_every_request() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture.provider.overflow.store(true, Ordering::SeqCst);
    assert_eq!(fixture.session.prompt("Continue.").await.unwrap(), "done");
    assert_eq!(
        fixture.provider.opened.load(Ordering::SeqCst),
        1,
        "summarization and retry retain the agent's context"
    );
    let records = fixture.session.runtime.store.records().await;
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
    let replay: Vec<_> = records
        .iter()
        .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
        .map(|record| {
            reconstruct_model_request(&records, record.sequence)
                .unwrap()
                .1
        })
        .collect();
    assert_eq!(replay, requests);
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_blocked_summarizer_does_not_activate_a_checkpoint() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture.provider.block.store(true, Ordering::SeqCst);
    let todos = vec![TodoItem {
        text: "Do not discard this task on interruption".into(),
        status: TodoStatus::InProgress,
    }];
    fixture
        .session
        .runtime
        .todos
        .replace(&fixture.session.root, todos.clone())
        .await
        .unwrap();
    let before = project_history(
        &fixture.session.runtime.store.records().await,
        &fixture.session.root,
    )
    .unwrap();
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
    let records = fixture.session.runtime.store.records().await;
    assert_eq!(
        project_history(&records, &fixture.session.root).unwrap(),
        before
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    assert_eq!(
        fixture
            .session
            .runtime
            .todos
            .inspect(&fixture.session.root, None)
            .await
            .unwrap()
            .items,
        todos
    );
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn fresh_state_reflects_concurrent_todo_updates_without_claiming_job_notifications() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    // Use a separate owner so the idle root command loop cannot consume this
    // notification independently of the compaction under test.
    let owner = fixture.session.root.child(99);
    let job = fixture
        .session
        .runtime
        .jobs
        .create(JobSpec {
            background: true,
            ..JobSpec::test(owner.clone(), "background-research")
        })
        .await
        .unwrap();
    let root_job = fixture
        .session
        .runtime
        .jobs
        .create(JobSpec::test(fixture.session.root.clone(), "root-research"))
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
            fixture
                .session
                .runtime
                .todos
                .replace(&fixture.session.root, updated.clone())
                .await
                .unwrap();
            fixture
                .session
                .runtime
                .jobs
                .finish(job.id, JobOutcome::Completed(ToolOutput::default()))
                .await
                .unwrap();
            fixture
                .session
                .runtime
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
    let records = fixture.session.runtime.store.records().await;
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
        fixture
            .session
            .runtime
            .jobs
            .snapshot(job.id)
            .await
            .unwrap()
            .state,
        JobState::Completed
    );
    assert_eq!(
        fixture
            .session
            .runtime
            .jobs
            .take_pending(&owner)
            .await
            .unwrap()[0]
            .id,
        job.id
    );
    assert!(
        fixture
            .session
            .runtime
            .jobs
            .take_pending(&owner)
            .await
            .unwrap()
            .is_empty()
    );
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
        fixture
            .session
            .runtime
            .todos
            .inspect(&fixture.session.root, None)
            .await
            .unwrap()
            .items,
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
    fixture.assert_exact_requests().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_requests_do_not_replay_provider_failures() {
    let fixture = Fixture::new().await;
    fixture
        .provider
        .agent_immediate_failures
        .store(1, Ordering::SeqCst);
    fixture
        .provider
        .agent_stream_failures
        .store(1, Ordering::SeqCst);
    assert!(
        fixture
            .session
            .prompt("Do not replay this request.")
            .await
            .is_err()
    );
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 2);
    assert_eq!(
        fixture
            .provider
            .agent_stream_failures
            .load(Ordering::SeqCst),
        1
    );
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_provider_failures_stop_after_one_attempt() {
    for streaming in [false, true] {
        let fixture = Fixture::new().await;
        let counter = if streaming {
            &fixture.provider.agent_stream_failures
        } else {
            &fixture.provider.agent_immediate_failures
        };
        fixture
            .provider
            .agent_failure_tool_blocks
            .store(streaming, Ordering::SeqCst);
        counter.store(10, Ordering::SeqCst);
        assert!(
            fixture
                .session
                .prompt("This request will fail.")
                .await
                .is_err()
        );
        assert_eq!(fixture.provider.requests.lock().unwrap().len(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 9);
        fixture.assert_exact_requests_and_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn overflow_compaction_recovers_by_changing_request() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture.provider.overflow.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture
            .session
            .prompt("Retry, compact, then continue.")
            .await
            .unwrap(),
        "done"
    );
    let records = fixture.session.runtime.store.records().await;
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Agent,
                    ..
                }
            ))
            .count(),
        3
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::Compaction { .. }))
            .count(),
        1
    );
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn summarizer_does_not_replay_provider_failures() {
    for streaming in [false, true] {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let counter = if streaming {
            &fixture.provider.summary_stream_failures
        } else {
            &fixture.provider.summary_immediate_failures
        };
        counter.store(2, Ordering::SeqCst);
        assert!(fixture.compact(&CancellationToken::new()).await.is_err());
        assert_eq!(fixture.provider.requests.lock().unwrap().len(), 2);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let records = fixture.session.runtime.store.records().await;
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        );
        fixture.assert_exact_requests_and_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn permanent_summary_failures_preserve_history_without_provider_replays_or_tools() {
    // Exhaustive malformed-field cases belong to the continuation parser tests.
    // Here retain provider, truncation, tool-execution and semantic rollback contracts.
    for failure in ["stream", "truncated", "tool_call", "blank_todo"] {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let old_todos = vec![TodoItem {
            text: "Preserve unfinished work".into(),
            status: TodoStatus::InProgress,
        }];
        fixture
            .session
            .runtime
            .todos
            .replace(&fixture.session.root, old_todos.clone())
            .await
            .unwrap();
        let mut invalid = summary_value();
        match failure {
            "stream" => fixture
                .provider
                .summary_stream_failures
                .store(10, Ordering::SeqCst),
            "truncated" => fixture.provider.truncate.store(true, Ordering::SeqCst),
            "tool_call" => fixture.provider.summary_tools.store(true, Ordering::SeqCst),
            "blank_todo" => invalid["todos"] = json!([{"text":" \t", "status":"pending"}]),
            _ => unreachable!(),
        }
        if failure == "blank_todo" {
            *fixture.provider.summary.lock().unwrap() = invalid.to_string();
        }
        let before = project_history(
            &fixture.session.runtime.store.records().await,
            &fixture.session.root,
        )
        .unwrap();
        let error = fixture
            .compact(&CancellationToken::new())
            .await
            .unwrap_err();
        let records = fixture.session.runtime.store.records().await;
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
            if failure == "stream" { 2 } else { 4 },
            "{failure}"
        );
        assert_eq!(
            project_history(&records, &fixture.session.root).unwrap(),
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
            fixture
                .session
                .runtime
                .todos
                .inspect(&fixture.session.root, None)
                .await
                .unwrap()
                .items,
            old_todos,
            "{failure}"
        );
        fixture.assert_exact_requests_and_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn observed_usage_is_counted_once_for_each_failed_agent_and_summary_request() {
    let fixture = Fixture::new().await;
    let observed = Usage {
        input_tokens: 11,
        cached_input_tokens: 7,
        output_tokens: 3,
    };
    *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
    fixture
        .provider
        .agent_stream_failures
        .store(1, Ordering::SeqCst);
    fixture
        .provider
        .agent_empty_responses
        .store(1, Ordering::SeqCst);
    assert!(
        fixture
            .session
            .prompt("Preserve failed usage.")
            .await
            .is_err()
    );
    assert_eq!(fixture.session.usage().await, observed);

    fixture.add_history(20_000).await;
    fixture
        .provider
        .summary_stream_failures
        .store(2, Ordering::SeqCst);
    assert!(fixture.compact(&CancellationToken::new()).await.is_err());
    assert_eq!(
        fixture.session.usage().await,
        Usage {
            input_tokens: 22,
            cached_input_tokens: 14,
            output_tokens: 6
        }
    );
    let records = fixture.session.runtime.store.records().await;
    let failed: Vec<_> = records
        .iter()
        .filter_map(|record| match record.event {
            SessionEvent::ModelFailed { request, .. } => Some(request),
            SessionEvent::CompactionFailed {
                request: Some(request),
                ..
            } => Some(request),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 2);
    for request in failed {
        let recorded: Vec<_> = records
            .iter()
            .filter_map(|record| match record.event {
                SessionEvent::Usage {
                    request: Some(sequence),
                    usage,
                } if sequence == request => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(recorded, vec![observed]);
    }
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn structured_continuation_and_reconciled_todos_activate_together_and_survive_resume() {
    let mut fixture = Fixture::new().await;
    fixture.template.reasoning = Some("high".into());
    fixture.add_history(20_000).await;
    let markdown = "  ## Continue here\n\n- Preserve the user's deployment restriction.\n- We rejected caching because invalidation is unresolved.\n\n```text\nThis is a plan, not JSON: {unfinished\n```\n\nNext: inspect the queue.\n  ";
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
    fixture
        .session
        .runtime
        .todos
        .replace(&fixture.session.root, original)
        .await
        .unwrap();
    let child = fixture.session.root.child(1);
    let child_todos = vec![TodoItem {
        text: "Independent delegated work".into(),
        status: TodoStatus::InProgress,
    }];
    fixture
        .session
        .runtime
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
    let before_sequence = fixture
        .session
        .runtime
        .store
        .records()
        .await
        .last()
        .unwrap()
        .sequence;
    fixture.compact(&CancellationToken::new()).await.unwrap();
    let records = fixture.session.runtime.store.records().await;
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
    assert_eq!(checkpoint.schema_version, 2);
    assert_eq!(checkpoint.todos, reconciled);
    assert_eq!(
        fixture
            .session
            .runtime
            .todos
            .inspect(&fixture.session.root, None)
            .await
            .unwrap()
            .items,
        reconciled
    );
    assert_eq!(
        fixture
            .session
            .runtime
            .todos
            .inspect(&child, None)
            .await
            .unwrap()
            .items,
        child_todos
    );
    assert!(
        !records
            .iter()
            .any(|record| record.sequence > before_sequence
                && matches!(record.event, SessionEvent::TodosReplaced { .. }))
    );
    let summary_request = fixture
        .provider
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        summary_request.response_schema.as_ref().unwrap().schema,
        compaction::response_schema()
    );
    assert_eq!(summary_request.system, fixture.template.system);
    assert!(summary_request.tools.is_empty());
    assert_eq!(summary_request.reasoning, fixture.template.reasoning);
    fixture.assert_exact_requests_and_no_tool_execution().await;

    let harness = super::Harness {
        inner: fixture.session.runtime.harness.clone(),
    };
    let id = fixture.session.id();
    super::tests::shutdown_session(fixture.session).await;
    let resumed = harness.resume_session(id).await.unwrap();
    assert_eq!(
        resumed
            .runtime
            .todos
            .inspect(&resumed.root, None)
            .await
            .unwrap()
            .items,
        reconciled
    );
    assert_eq!(
        resumed
            .runtime
            .todos
            .inspect(&child, None)
            .await
            .unwrap()
            .items,
        child_todos
    );
    assert_eq!(
        resumed
            .prompt("Continue the preserved task.")
            .await
            .unwrap(),
        "done"
    );
    let captured = fixture
        .provider
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(captured.messages.first(), Some(&expected));
    assert_eq!(captured.system, fixture.template.system);
    assert_eq!(captured.tools, fixture.template.tools);
    assert!(captured.response_schema.is_none());
    let Some(Message::User(blocks)) = captured.messages.last() else {
        panic!("fresh runtime state")
    };
    let UserContent::Runtime { text } = &blocks[0] else {
        panic!("runtime block")
    };
    assert!(text.contains("Investigate the queue"));
    assert!(text.contains("completed"));
    assert!(text.contains("Verify the resulting fix"));
    assert!(text.contains("pending"));
    let records = resumed.runtime.store.records().await;
    let last = records
        .iter()
        .rev()
        .find(|record| {
            matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Agent,
                    ..
                }
            )
        })
        .unwrap();
    assert_eq!(
        reconstruct_model_request(&records, last.sequence)
            .unwrap()
            .1,
        captured
    );
    resumed.shutdown().await.unwrap();
}

#[tokio::test]
async fn selected_jobs_preserve_arguments_truncate_outputs_and_replay_without_claiming() {
    let fixture = Fixture::new().await;
    let runtime = &fixture.session.runtime;
    let arguments = json!({"path":"evidence.txt", "literal":null});
    let output = "evidence\n".repeat(400);
    let lease = runtime.jobs.create(JobSpec {
        arguments: arguments.clone(),
        output_schema: Some(json!({"type":"object","properties":{"content":{"type":"string","x-skyhook-truncatable":true}}})),
        background: true,
        ..JobSpec::test(fixture.session.root.child(99), "read")
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
    assert_eq!(view["id"], json!(lease.id));
    assert_eq!(view["tool"], "read");
    assert!(view["result"]["content"].as_str().unwrap().len() < output.len());
    assert_eq!(view["truncated"][0]["field"], "/result/content");
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
            .take_pending(&fixture.session.root.child(99))
            .await
            .unwrap()
            .iter()
            .any(|job| job.id == lease.id)
    );
    assert!(
        project_history(&records, &fixture.session.root)
            .unwrap()
            .iter()
            .any(|(_, message)| message == &checkpoint.message)
    );
    // Repeated compaction can select the same saved job again, without duplicating it.
    fixture.add_history(20_000).await;
    fixture.compact(&CancellationToken::new()).await.unwrap();
    fixture.assert_exact_requests().await;
}

#[tokio::test]
async fn invalid_selected_job_retries_without_installing_compaction() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    let mut summary = summary_value();
    summary["jobs"] = json!([99999]);
    *fixture.provider.summary.lock().unwrap() = summary.to_string();
    assert!(fixture.compact(&CancellationToken::new()).await.is_err());
    let records = fixture.session.runtime.store.records().await;
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
}

#[tokio::test]
async fn cancellation_journals_observed_usage_once_without_committing_or_executing_tools() {
    for summary in [false, true] {
        let fixture = Fixture::new().await;
        let observed = Usage {
            input_tokens: 11,
            cached_input_tokens: 7,
            output_tokens: 3,
        };
        *fixture.provider.observed_failure_usage.lock().unwrap() = Some(observed);
        fixture
            .provider
            .pause_stream_after_usage
            .store(true, Ordering::SeqCst);
        if summary {
            fixture.add_history(20_000).await;
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
        } else {
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(fixture.session.prompt("Interrupt this turn."), async {
                    fixture.provider.started.notified().await;
                    fixture.session.interrupt().await;
                })
            })
            .await
            .unwrap();
            assert!(result.is_err());
        }
        let records = fixture.session.runtime.store.records().await;
        let requested = records
            .iter()
            .rev()
            .find(|r| matches!(r.event, SessionEvent::ModelRequested { .. }))
            .unwrap()
            .sequence;
        let observed_events: Vec<_> = records
            .iter()
            .filter_map(|r| match r.event {
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } if request == requested => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(observed_events, vec![observed]);
        assert_eq!(fixture.session.usage().await, observed);
        assert!(!records.iter().any(|r| matches!(&r.event,
            SessionEvent::MessageCommitted { message: Message::Assistant(items) }
                if items.iter().any(|item| item.id == "interrupted-tool"))));
        fixture.assert_exact_requests_and_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn compaction_checkpoint_and_resumed_request_preserve_reasoning_tool_exchange() {
    use crate::provider::protocol::{ReplayEnvelope, ToolCall, ToolResult};
    for protocol in ["chat_completions", "responses", "anthropic"] {
        let fixture = Fixture::new().await;
        fixture.add_history(20_000).await;
        let assistant = Message::Assistant(vec![
            AssistantContent::reasoning(
                "retained-reasoning",
                0,
                "original reasoning",
                Some(ReplayEnvelope {
                    version: 1,
                    protocol: protocol.into(),
                    model: "native".into(),
                    scope: "original-scope".into(),
                    payload: json!({"encrypted_content":"opaque state", "signature":"original signature"}),
                }),
            ),
            AssistantContent::tool_call(
                "retained-tool",
                1,
                ToolCall {
                    id: "retained-call".into(),
                    name: "read".into(),
                    arguments: json!({"path":"evidence.txt"}),
                },
            ),
        ]);
        let result = Message::Tool(vec![ToolResult {
            call_id: "retained-call".into(),
            name: "read".into(),
            result: json!({"content":"retained evidence"}),
            images: vec![],
            is_error: false,
        }]);
        let assistant_id = fixture
            .session
            .runtime
            .commit(&fixture.session.root, assistant.clone())
            .await
            .unwrap();
        let result_id = fixture
            .session
            .runtime
            .commit(&fixture.session.root, result.clone())
            .await
            .unwrap();
        fixture.compact(&CancellationToken::new()).await.unwrap();
        let records = fixture.session.runtime.store.records().await;
        let history = project_history(&records, &fixture.session.root).unwrap();
        assert!(
            records
                .iter()
                .any(|r| matches!(&r.event, SessionEvent::Compaction { .. }))
        );
        assert!(history.contains(&(assistant_id, assistant.clone())));
        assert!(history.contains(&(result_id, result.clone())));
        fixture.assert_exact_requests_and_no_tool_execution().await;
        let harness = super::Harness {
            inner: fixture.session.runtime.harness.clone(),
        };
        let id = fixture.session.id();
        super::tests::shutdown_session(fixture.session).await;
        let resumed = harness.resume_session(id).await.unwrap();
        let reloaded = resumed.runtime.store.records().await;
        assert_eq!(project_history(&reloaded, &resumed.root).unwrap(), history);
        resumed
            .prompt("Continue from the preserved evidence.")
            .await
            .unwrap();
        let requests = fixture.provider.requests.lock().unwrap().clone();
        let request = requests.last().unwrap();
        let position = request
            .messages
            .iter()
            .position(|message| message == &assistant)
            .expect("original opaque assistant item is replayed");
        assert_eq!(request.messages.get(position + 1), Some(&result));
        assert!(
            !resumed
                .runtime
                .store
                .records()
                .await
                .iter()
                .any(|r| matches!(r.event, SessionEvent::JobCreated { .. }))
        );
        resumed.shutdown().await.unwrap();
    }
}

//! Compaction lifecycle tests with deterministic local provider streams.

use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use futures_util::stream;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    HarnessBuilder, HarnessError, SessionHandle, TurnContext, compact, compaction, prompt,
};
use crate::{
    agent::{TodoItem, TodoStatus},
    execution::ExecutionLocation,
    job::{JobOutcome, JobSpec, JobState},
    provider::{
        Provider, ProviderError, ProviderErrorKind, ProviderFuture, ResponseStream,
        profile::ModelProfile,
        protocol::{AssistantContent, Message, ModelRequest, ResponseChunk, Usage, UserContent},
    },
    session::{ModelPurpose, SessionEvent, project_history, reconstruct_model_request},
    tool::{ToolOutput, policy::CapabilitySet},
};

struct ControlledProvider {
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
    started: Notify,
    release: Semaphore,
}

impl Default for ControlledProvider {
    fn default() -> Self {
        Self {
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
            started: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}

impl Provider for Arc<ControlledProvider> {
    fn invoke(&self, request: ModelRequest) -> ProviderFuture {
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
            if consume_failure(immediate) {
                return Err(error());
            }
            if consume_failure(streaming) {
                let mut chunks = Vec::new();
                if let Some(usage) = *provider.observed_failure_usage.lock().unwrap() {
                    chunks.push(Ok(ResponseChunk::Usage { usage }));
                }
                if !summary && provider.agent_failure_tool_blocks.load(Ordering::SeqCst) {
                    chunks.push(Ok(ResponseChunk::Block {
                        block: AssistantContent::ToolCall(crate::provider::protocol::ToolCall {
                            id: "failed-attempt-tool".into(),
                            name: "write".into(),
                            arguments: json!({"path":"must-not-exist", "content":"side effect"}),
                        }),
                    }));
                }
                chunks.push(Err(error()));
                return Ok(Box::pin(stream::iter(chunks)) as ResponseStream);
            }
            if !summary && consume_failure(&provider.agent_empty_responses) {
                let chunks: Vec<_> = provider
                    .observed_failure_usage
                    .lock()
                    .unwrap()
                    .map(|usage| Ok(ResponseChunk::Usage { usage }))
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
                    vec![Ok(ResponseChunk::Block {
                        block: AssistantContent::ToolCall(crate::provider::protocol::ToolCall {
                            id: "never-execute".into(),
                            name: "write".into(),
                            arguments: json!({"path":"must-not-exist", "content":"side effect"}),
                        }),
                    })]
                } else {
                    vec![
                        Ok(ResponseChunk::ReasoningDelta {
                            text: "Reasoning before the answer is not JSON and must not enter the continuation.".into(),
                        }),
                        Ok(ResponseChunk::TextDelta {
                            text: provider.summary.lock().unwrap().clone(),
                        }),
                        Ok(ResponseChunk::Finished {
                            truncated: provider.truncate.load(Ordering::SeqCst),
                        }),
                    ]
                }
            } else if provider.overflow.swap(false, Ordering::SeqCst) {
                vec![Err(ProviderError {
                    kind: ProviderErrorKind::ContextWindowExceeded,
                    message: "prompt is too long".into(),
                })]
            } else {
                vec![Ok(ResponseChunk::TextDelta {
                    text: "done".into(),
                })]
            };
            Ok(Box::pin(stream::iter(chunks)) as ResponseStream)
        })
    }
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
                Message::Assistant(vec![AssistantContent::Text {
                    text: "research ".repeat(tokens * 4 / 9),
                }]),
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

    async fn add_history_to(&self, target: u64) {
        let mut current = self.template.clone();
        current.messages = project_history(
            &self.session.runtime.store.records().await,
            &self.session.root,
        )
        .unwrap()
        .into_iter()
        .map(|(_, message)| message)
        .collect();
        current.messages.push(Message::User(vec![
            prompt::runtime_state_content(
                &self.session.runtime.jobs,
                &self.session.runtime.todos,
                &self.session.root,
                &CapabilitySet::default(),
            )
            .await,
        ]));
        self.add_history(target.saturating_sub(compaction::estimate_request(&current)) as usize)
            .await;
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
                    profile: &profile(),
                    system: &input.system,
                    owner_job: None,
                    cancellation,
                    location: &location,
                    capabilities: &capabilities,
                },
                &self.provider,
                context,
                &input,
            )
            .await
    }
}

#[tokio::test]
async fn stream_overflow_below_threshold_compacts_once_and_replays_every_request() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture.provider.overflow.store(true, Ordering::SeqCst);
    assert_eq!(fixture.session.prompt("Continue.").await.unwrap(), "done");
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
async fn truncated_summary_keeps_the_original_projection() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture.provider.truncate.store(true, Ordering::SeqCst);
    let before = project_history(
        &fixture.session.runtime.store.records().await,
        &fixture.session.root,
    )
    .unwrap();
    let error = fixture
        .compact(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("truncated"));
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
    assert!(records.iter().any(|record| matches!(
        record.event,
        SessionEvent::CompactionFailed {
            request: Some(_),
            ..
        }
    )));
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
async fn token_meter_restores_actual_input_usage_for_the_same_agent_and_template() {
    let fixture = Fixture::new().await;
    let records = fixture.session.runtime.store.records().await;
    let request = records
        .iter()
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
    let captured = fixture.provider.requests.lock().unwrap()[0].clone();
    let estimated = compaction::estimate_request(&captured);
    fixture
        .session
        .runtime
        .store
        .append(
            fixture.session.root.clone(),
            SessionEvent::Usage {
                request: Some(request.sequence),
                usage: crate::provider::protocol::Usage {
                    input_tokens: estimated + 10_000,
                    cached_input_tokens: 2_000,
                    output_tokens: 3,
                },
            },
        )
        .await
        .unwrap();
    let records = fixture.session.runtime.store.records().await;
    let meter = compact::TokenMeter::restore(&records, &fixture.session.root, &fixture.template);
    assert_eq!(meter.estimate(&captured), estimated + 12_000);
    let other_agent =
        compact::TokenMeter::restore(&records, &fixture.session.root.child(1), &fixture.template);
    assert_eq!(other_agent.estimate(&captured), estimated);
    let mut changed = fixture.template.clone();
    changed.model = "another-model".into();
    let other_template = compact::TokenMeter::restore(&records, &fixture.session.root, &changed);
    assert_eq!(other_template.estimate(&captured), estimated);
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn large_plan_in_continuation_is_kept_without_a_post_compaction_token_cap() {
    let fixture = Fixture::new().await;
    let plan = format!(
        "## Implementation plan\n{}",
        "Preserve this original detail.\n".repeat(5_000)
    );
    fixture
        .session
        .runtime
        .commit(
            &fixture.session.root,
            Message::Assistant(vec![AssistantContent::Text { text: plan.clone() }]),
        )
        .await
        .unwrap();
    fixture.add_history(20_000).await;
    let mut summary = summary_value();
    summary["plan"] = json!([plan]);
    *fixture.provider.summary.lock().unwrap() = summary.to_string();
    fixture.compact(&CancellationToken::new()).await.unwrap();
    let records = fixture.session.runtime.store.records().await;
    let checkpoint = records
        .iter()
        .find_map(|record| match &record.event {
            SessionEvent::Compaction { checkpoint } => Some(checkpoint),
            _ => None,
        })
        .unwrap();
    assert!(checkpoint.after_tokens > 30_000);
    assert!(checkpoint.after_tokens < checkpoint.before_tokens);
    assert!(
        matches!(&checkpoint.message, Message::User(blocks) if blocks.iter().any(|block| matches!(block, UserContent::Compaction {text} if text.contains(&plan))))
    );
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn compaction_threshold_reserves_max_output_instead_of_using_eighty_percent() {
    let fixture = Fixture::new().await;
    fixture.add_history_to(104_000).await;
    fixture
        .session
        .prompt("Continue below the reserve threshold.")
        .await
        .unwrap();
    let requests = fixture.provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let estimated = compaction::estimate_request(&requests[1]);
    assert!(estimated >= profile().max_context * 4 / 5);
    assert!(estimated < profile().max_context - profile().max_output);
    assert!(
        !fixture
            .session
            .runtime
            .store
            .records()
            .await
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );

    fixture.add_history_to(112_000).await;
    fixture
        .session
        .prompt("Continue above the reserve threshold.")
        .await
        .unwrap();
    let records = fixture.session.runtime.store.records().await;
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::Compaction { .. }))
            .count(),
        1
    );
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_summary_estimates_reach_the_provider_and_can_compact_successfully() {
    let fixture = Fixture::new().await;
    fixture.add_history_to(150_000).await;
    assert_eq!(
        fixture
            .session
            .prompt("Continue despite the conservative estimate.")
            .await
            .unwrap(),
        "done"
    );
    let records = fixture.session.runtime.store.records().await;
    let summary = records
        .iter()
        .find(|record| {
            matches!(
                record.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Compaction,
                    ..
                }
            )
        })
        .unwrap();
    let (_, request) = reconstruct_model_request(&records, summary.sequence).unwrap();
    assert!(compaction::estimate_request(&request) > profile().max_context);
    assert!(
        records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_protected_content_still_reaches_the_working_provider() {
    let fixture = Fixture::new().await;
    let plan = format!(
        "## Plan\n{}",
        "Keep this exact requirement.\n".repeat(24_000)
    );
    fixture
        .session
        .runtime
        .commit(
            &fixture.session.root,
            Message::Assistant(vec![AssistantContent::Text { text: plan.clone() }]),
        )
        .await
        .unwrap();
    let mut summary = summary_value();
    summary["plan"] = json!([plan]);
    *fixture.provider.summary.lock().unwrap() = summary.to_string();
    assert_eq!(
        fixture
            .session
            .prompt("Keep the complete plan and continue.")
            .await
            .unwrap(),
        "done"
    );
    let requests = fixture.provider.requests.lock().unwrap().clone();
    let working = requests.last().unwrap();
    assert!(compaction::estimate_request(working) > profile().max_context);
    assert!(working.messages.iter().any(|message| {
        match message {
            Message::Assistant(blocks) => blocks
                .iter()
                .any(|block| matches!(block, AssistantContent::Text { text } if text == &plan)),
            Message::User(blocks) => blocks.iter().any(
                |block| matches!(block, UserContent::Compaction { text } if text.contains(&plan)),
            ),
            _ => false,
        }
    }));
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_requests_retry_immediate_and_stream_failures_then_succeed_on_attempt_three() {
    let fixture = Fixture::new().await;
    fixture
        .provider
        .agent_immediate_failures
        .store(1, Ordering::SeqCst);
    fixture
        .provider
        .agent_stream_failures
        .store(1, Ordering::SeqCst);
    assert_eq!(
        fixture.session.prompt("Retry this request.").await.unwrap(),
        "done"
    );
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
    let records = fixture.session.runtime.store.records().await;
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.event, SessionEvent::Compaction { .. }))
    );
    fixture.assert_exact_requests_and_no_tool_execution().await;
    fixture.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn ordinary_provider_failures_stop_after_three_total_attempts() {
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
        assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
        assert_eq!(counter.load(Ordering::SeqCst), 7);
        fixture.assert_exact_requests_and_no_tool_execution().await;
        fixture.session.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn overflow_compaction_does_not_reset_the_ordinary_attempt_budget() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture
        .provider
        .agent_immediate_failures
        .store(1, Ordering::SeqCst);
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
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 5);
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
        4
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
async fn summarizer_retries_both_failure_paths_and_succeeds_on_attempt_three() {
    let fixture = Fixture::new().await;
    fixture.add_history(20_000).await;
    fixture
        .provider
        .summary_immediate_failures
        .store(1, Ordering::SeqCst);
    fixture
        .provider
        .summary_stream_failures
        .store(1, Ordering::SeqCst);
    fixture.compact(&CancellationToken::new()).await.unwrap();
    let records = fixture.session.runtime.store.records().await;
    assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
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
async fn permanent_summary_failures_preserve_history_after_three_attempts_without_tools() {
    for failure in [
        "transport",
        "stream",
        "empty",
        "truncated",
        "tool_call",
        "markdown",
        "missing_field",
        "wrong_type",
        "unknown_field",
        "invalid_status",
        "blank_todo",
    ] {
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
            "transport" => fixture
                .provider
                .summary_immediate_failures
                .store(10, Ordering::SeqCst),
            "stream" => fixture
                .provider
                .summary_stream_failures
                .store(10, Ordering::SeqCst),
            "empty" => *fixture.provider.summary.lock().unwrap() = " \n\t ".into(),
            "truncated" => fixture.provider.truncate.store(true, Ordering::SeqCst),
            "tool_call" => fixture.provider.summary_tools.store(true, Ordering::SeqCst),
            "markdown" => *fixture.provider.summary.lock().unwrap() = "## Continue the task".into(),
            "missing_field" => {
                invalid.as_object_mut().unwrap().remove("plan");
            }
            "wrong_type" => invalid["findings"] = json!("old string format"),
            "unknown_field" => invalid["invented_field"] = json!("unexpected"),
            "invalid_status" => invalid["todos"] = json!([{"text":"Task", "status":"blocked"}]),
            "blank_todo" => invalid["todos"] = json!([{"text":" \t", "status":"pending"}]),
            _ => unreachable!(),
        }
        if matches!(
            failure,
            "missing_field" | "wrong_type" | "unknown_field" | "invalid_status" | "blank_todo"
        ) {
            *fixture.provider.summary.lock().unwrap() = invalid.to_string();
        }
        let before = project_history(
            &fixture.session.runtime.store.records().await,
            &fixture.session.root,
        )
        .unwrap();
        assert!(
            fixture.compact(&CancellationToken::new()).await.is_err(),
            "{failure}"
        );
        let records = fixture.session.runtime.store.records().await;
        assert_eq!(
            fixture.provider.requests.lock().unwrap().len(),
            4,
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
async fn empty_ordinary_responses_retry_twice_before_success_and_stop_after_three_failures() {
    for empty_responses in [2, 10] {
        let fixture = Fixture::new().await;
        fixture
            .provider
            .agent_empty_responses
            .store(empty_responses, Ordering::SeqCst);
        let result = fixture
            .session
            .prompt("Continue after an empty provider response.")
            .await;
        if empty_responses == 2 {
            assert_eq!(result.unwrap(), "done");
        } else {
            assert!(result.is_err());
            assert_eq!(
                fixture
                    .provider
                    .agent_empty_responses
                    .load(Ordering::SeqCst),
                7
            );
        }
        assert_eq!(fixture.provider.requests.lock().unwrap().len(), 4);
        let records = fixture.session.runtime.store.records().await;
        let failures: Vec<_> = records
            .iter()
            .filter_map(|record| match record.event {
                SessionEvent::ModelFailed { attempt, .. } => Some(attempt),
                _ => None,
            })
            .collect();
        assert_eq!(
            failures,
            if empty_responses == 2 {
                vec![1, 2]
            } else {
                vec![1, 2, 3]
            }
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
    assert_eq!(
        fixture
            .session
            .prompt("Retry while preserving usage accounting.")
            .await
            .unwrap(),
        "done"
    );
    assert_eq!(
        fixture.session.usage().await,
        Usage {
            input_tokens: 22,
            cached_input_tokens: 14,
            output_tokens: 6
        }
    );

    fixture.add_history(20_000).await;
    fixture
        .provider
        .summary_stream_failures
        .store(2, Ordering::SeqCst);
    fixture.compact(&CancellationToken::new()).await.unwrap();
    assert_eq!(
        fixture.session.usage().await,
        Usage {
            input_tokens: 44,
            cached_input_tokens: 28,
            output_tokens: 12
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
    assert_eq!(failed.len(), 4);
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
    assert_eq!(checkpoint.schema_version, 1);
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
    fixture.session.shutdown().await.unwrap();
    fixture.session.runtime.store.close().await.unwrap();
    let resumed = harness.resume_session(fixture.session.id()).await.unwrap();
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

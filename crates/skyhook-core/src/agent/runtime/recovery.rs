//! Transient model failure recovery and usage accounting.

use super::*;

/// Server hints take precedence over the exponential fallback, including hints
/// longer than its 30-second cap. `attempt` counts transient failures of the
/// frozen request, independently of context/validation attempts.
fn recovery_delay(error: &crate::provider::ProviderError, attempt: u64) -> std::time::Duration {
    error.retry_after.unwrap_or_else(|| {
        std::time::Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5))
            .min(std::time::Duration::from_secs(30))
    })
}

/// Audit attempts span a logical response; backoff counts only transient failures
/// of the currently frozen request. Rebuilding a request resets only its backoff.
pub(super) struct RecoveryAttempt {
    pub(super) model: u64,
    pub(super) transient: u64,
}

impl SessionRuntime {
    /// Transient recovery continues until success or cancellation and never restarts a child or
    /// re-executes committed tools. The caller has recorded the failed attempt and
    /// dropped the provider's failed stream before entering this wait.
    pub(super) async fn schedule_model_recovery(
        &self,
        turn: &TurnContext<'_>,
        request: u64,
        attempt: RecoveryAttempt,
        error: &crate::provider::ProviderError,
        provider: &mut dyn crate::provider::ProviderContext,
    ) -> Result<(), HarnessError> {
        if turn.cancellation.is_cancelled() {
            return Err(HarnessError::Interrupted);
        }
        provider.reset();
        let delay = recovery_delay(error, attempt.transient);
        let delay_millis = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.store
            .append(
                turn.agent.clone(),
                SessionEvent::ModelRecoveryScheduled {
                    request,
                    attempt: attempt.model.saturating_add(1),
                    max_attempts: None,
                    delay_millis,
                    error: error.to_string(),
                },
            )
            .await?;
        tokio::select! {
            () = turn.cancellation.cancelled() => Err(HarnessError::Interrupted),
            () = tokio::time::sleep(delay) => Ok(()),
        }
    }

    pub(super) async fn record_model_failure(
        &self,
        agent: &AgentId,
        request: u64,
        attempt: u64,
        usage: Usage,
        error: String,
    ) -> Result<(), HarnessError> {
        if usage != Usage::default() {
            self.record_model_usage(agent, request, usage).await?;
        }
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelFailed {
                    request,
                    attempt,
                    error,
                },
            )
            .await?;
        Ok(())
    }

    pub(super) async fn record_model_usage(
        &self,
        agent: &AgentId,
        request: u64,
        usage: Usage,
    ) -> Result<(), HarnessError> {
        self.store
            .append(
                agent.clone(),
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                },
            )
            .await?;
        self.usage.lock().await.accumulate(usage);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Exercise provider-neutral recovery through the real agent loop and durable journal.

    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use super::*;
    use crate::agent::runtime::tests::test_harness;
    use crate::provider::{
        CodexWebSocketError, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
        ResponseStream,
        protocol::{StopReason, events_for_content},
    };
    use crate::session::{ModelPurpose, project_history};

    // Startup and streaming failures deliberately use the same provider context. A
    // reset must release transport state, not open a new agent or replay its tools.
    enum Step {
        Startup(ProviderError),
        Stream(Vec<Result<ResponseChunk, ProviderError>>),
    }

    #[derive(Default)]
    struct Script {
        steps: StdMutex<VecDeque<Step>>,
        requests: StdMutex<Vec<ModelRequest>>,
        opened: AtomicUsize,
        resets: AtomicUsize,
    }

    struct Factory(Arc<Script>);
    struct Context(Arc<Script>);

    impl Provider for Factory {
        fn open_context(&self, _: String) -> Result<Box<dyn ProviderContext>, ProviderError> {
            self.0.opened.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Context(self.0.clone())))
        }
    }

    impl ProviderContext for Context {
        fn reset(&mut self) {
            self.0.resets.fetch_add(1, Ordering::SeqCst);
        }

        fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
            self.0.requests.lock().unwrap().push(request);
            let step = self.0.steps.lock().unwrap().pop_front();
            Box::pin(async move {
                match step.expect("unexpected extra model invocation") {
                    Step::Startup(error) => Err(error),
                    Step::Stream(chunks) => {
                        Ok(Box::pin(futures_util::stream::iter(chunks)) as ResponseStream)
                    }
                }
            })
        }
    }

    fn recoverable() -> ProviderError {
        ProviderError {
            retry_after: None,
            kind: ProviderErrorKind::CodexWebSocket(CodexWebSocketError::Read),
            message: "scripted websocket connection lost".into(),
        }
    }

    #[test]
    fn fallback_backoff_caps_without_overflow_and_server_hint_overrides_it() {
        let mut error = recoverable();
        for (attempt, seconds) in [
            (0, 1),
            (1, 1),
            (2, 2),
            (3, 4),
            (4, 8),
            (5, 16),
            (6, 30),
            (7, 30),
            (256, 30),
            (u64::MAX, 30),
        ] {
            assert_eq!(
                recovery_delay(&error, attempt),
                Duration::from_secs(seconds)
            );
        }
        for hint in [
            Duration::ZERO,
            Duration::from_millis(1234),
            Duration::from_secs(90),
        ] {
            error.retry_after = Some(hint);
            assert_eq!(recovery_delay(&error, 1), hint);
            assert_eq!(recovery_delay(&error, u64::MAX), hint);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn server_retry_after_above_fallback_cap_is_honored_and_journaled() {
        let mut error = recoverable();
        error.retry_after = Some(Duration::from_secs(90));
        let fixture = Fixture::new([Step::Startup(error), answer("recovered")]).await;
        let started = tokio::time::Instant::now();
        assert_eq!(
            fixture.session.prompt("recover").await.unwrap(),
            "recovered"
        );
        assert!(started.elapsed() >= Duration::from_secs(90));
        let records = fixture.records().await;
        let scheduled = recoveries(&records);
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].1, 2);
        assert_eq!(scheduled[0].2, None);
        assert_eq!(scheduled[0].3, 90_000);
        assert_eq!(scheduled[0].4, recoverable().to_string());
        fixture.session.shutdown().await.unwrap();
    }

    fn success(items: Vec<AssistantContent>, stop_reason: StopReason) -> Step {
        let mut chunks = events_for_content(&items);
        chunks.push(ResponseChunk::ResponseEnded { stop_reason });
        Step::Stream(chunks.into_iter().map(Ok).collect())
    }

    fn answer(text: &str) -> Step {
        success(
            vec![AssistantContent::text("answer", 0, text)],
            StopReason::EndTurn,
        )
    }

    fn write_call(id: &str, path: &str) -> AssistantContent {
        AssistantContent::tool_call(
            id,
            0,
            ToolCall {
                id: id.into(),
                name: "write".into(),
                arguments: json!({"path":path, "content":id}),
            },
        )
    }

    struct Fixture {
        workspace: tempfile::TempDir,
        _sessions: tempfile::TempDir,
        session: SessionHandle,
        script: Arc<Script>,
    }

    impl Fixture {
        async fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = tempfile::tempdir().unwrap();
            let script = Arc::new(Script {
                steps: StdMutex::new(steps.into_iter().collect()),
                ..Script::default()
            });
            let harness = HarnessBuilder::new(workspace.path())
                .session_root(sessions.path())
                .provider("test", Arc::new(Factory(script.clone())))
                .model_profile(
                    "test",
                    ModelProfile {
                        provider: "test".into(),
                        model: "test".into(),
                        reasoning: None,
                        max_context: 128_000,
                        max_output: 16_384,
                        supports_images: true,
                    },
                )
                .default_model_profile("test")
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            Self {
                workspace,
                _sessions: sessions,
                session,
                script,
            }
        }

        async fn records(&self) -> Vec<EventRecord> {
            self.session.runtime.store.records().await
        }

        fn requests(&self) -> Vec<ModelRequest> {
            self.script.requests.lock().unwrap().clone()
        }
    }

    fn recoveries(records: &[EventRecord]) -> Vec<(u64, u64, Option<u64>, u64, String)> {
        records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ModelRecoveryScheduled {
                    request,
                    attempt,
                    max_attempts,
                    delay_millis,
                    error,
                } => Some((
                    *request,
                    *attempt,
                    *max_attempts,
                    *delay_millis,
                    error.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    fn assert_schedule(records: &[EventRecord], expected_attempts: &[u64]) {
        let scheduled = recoveries(records);
        assert_eq!(
            scheduled.iter().map(|entry| entry.1).collect::<Vec<_>>(),
            expected_attempts
        );
        let mut transient_attempts = std::collections::HashMap::<u64, u64>::new();
        for (request, _attempt, maximum, delay, error) in scheduled {
            let transient = transient_attempts.entry(request).or_default();
            *transient += 1;
            assert_eq!(maximum, None);
            assert_eq!(
                delay,
                recovery_delay(&recoverable(), *transient).as_millis() as u64
            );
            assert_eq!(error, recoverable().to_string());
            assert!(records.iter().any(|record| record.sequence == request
                && matches!(
                    record.event,
                    SessionEvent::ModelRequested {
                        purpose: ModelPurpose::Agent,
                        ..
                    }
                )));
            assert!(records.iter().any(|record| matches!(&record.event,
                SessionEvent::ModelFailed { request: failed, error: failure, .. }
                    if *failed == request && failure == &error)));
        }
    }

    async fn next_recovery(events: &mut broadcast::Receiver<RuntimeEvent>) -> EventRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let RuntimeEvent::Record(record) = events.recv().await.unwrap()
                    && matches!(record.event, SessionEvent::ModelRecoveryScheduled { .. })
                {
                    return *record;
                }
            }
        })
        .await
        .expect("recovery should be scheduled promptly")
    }

    #[tokio::test(start_paused = true)]
    async fn startup_failure_retries_the_exact_request_without_committing_an_error() {
        let fixture = Fixture::new([Step::Startup(recoverable()), answer("recovered")]).await;
        let image = fixture.workspace.path().join("evidence.png");
        std::fs::write(&image, b"request image fixture").unwrap();
        assert_eq!(
            fixture
                .session
                .prompt_with_images("continue", &[image])
                .await
                .unwrap(),
            "recovered"
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0], requests[1],
            "retry must use the frozen hydrated request"
        );
        assert!(requests[0].messages.iter().any(|message| matches!(message,
            Message::User(blocks) if blocks.iter().any(|block| matches!(block,
                UserContent::Image { image } if image.data_base64.is_some())))));
        assert_eq!(fixture.script.opened.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.script.resets.load(Ordering::SeqCst), 1);
        let records = fixture.records().await;
        assert_schedule(&records, &[2]);
        let history = project_history(&records, &fixture.session.root).unwrap();
        let assistants: Vec<_> = history
            .iter()
            .filter(|(_, message)| matches!(message, Message::Assistant(_)))
            .collect();
        assert_eq!(assistants.len(), 1);
        assert!(
            !serde_json::to_string(&history)
                .unwrap()
                .contains("connection lost")
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_partial_text_and_complete_tool_block_are_discarded_before_retry() {
        let mut partial = events_for_content(&[
            AssistantContent::text("partial", 0, "DO NOT COMMIT"),
            AssistantContent::tool_call(
                "discarded",
                1,
                ToolCall {
                    id: "discarded".into(),
                    name: "write".into(),
                    arguments: json!({"path":"must-not-exist", "content":"bad"}),
                },
            ),
        ])
        .into_iter()
        .map(Ok)
        .collect::<Vec<_>>();
        let observed_usage = Usage {
            input_tokens: 71,
            cached_input_tokens: 13,
            output_tokens: 5,
        };
        partial.insert(
            0,
            Ok(ResponseChunk::UsageUpdated {
                usage: observed_usage,
            }),
        );
        partial.push(Err(recoverable()));
        let fixture = Fixture::new([
            Step::Stream(partial),
            success(
                vec![write_call("committed", "successful-tool")],
                StopReason::ToolUse,
            ),
            answer("done"),
        ])
        .await;
        assert_eq!(fixture.session.prompt("write once").await.unwrap(), "done");
        assert!(!fixture.workspace.path().join("must-not-exist").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.path().join("successful-tool")).unwrap(),
            "committed"
        );
        let records = fixture.records().await;
        let failed_request = recoveries(&records)[0].0;
        let failed_usage: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::Usage {
                    request: Some(request),
                    usage,
                } if *request == failed_request => Some(*usage),
                _ => None,
            })
            .collect();
        assert_eq!(
            failed_usage,
            [observed_usage, Usage::default()],
            "failed usage and successful usage must each be recorded exactly once"
        );
        assert_eq!(fixture.session.usage().await, observed_usage);
        let jobs: Vec<_> = records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
            .collect();
        assert_eq!(
            jobs.len(),
            1,
            "only the successful attempt may execute tools"
        );
        let history =
            serde_json::to_string(&project_history(&records, &fixture.session.root).unwrap())
                .unwrap();
        assert!(!history.contains("DO NOT COMMIT"));
        assert!(!history.contains("discarded"));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| matches!(message, Message::Tool(_)))
        );
        assert_schedule(&records, &[2]);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn successful_tool_results_survive_recovery_and_connection_budget_resets() {
        let fixture = Fixture::new([
            Step::Startup(recoverable()),
            Step::Startup(recoverable()),
            success(
                vec![write_call("prior-tool", "prior-tool")],
                StopReason::ToolUse,
            ),
            Step::Startup(recoverable()),
            Step::Startup(recoverable()),
            answer("done"),
        ])
        .await;
        assert_eq!(
            fixture.session.prompt("write and finish").await.unwrap(),
            "done"
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), 6);
        assert!(requests[..3].windows(2).all(|pair| pair[0] == pair[1]));
        assert!(requests[3..].windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(requests[2], requests[3]);
        for request in &requests[3..] {
            assert_eq!(
                request
                    .messages
                    .iter()
                    .filter(|message| matches!(message, Message::Tool(_)))
                    .count(),
                1
            );
        }
        let records = fixture.records().await;
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.path().join("prior-tool")).unwrap(),
            "prior-tool"
        );
        assert_schedule(&records, &[2, 3, 2, 3]);
        assert_eq!(fixture.script.resets.load(Ordering::SeqCst), 4);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn websocket_retries_past_u8_limit_then_succeeds() {
        let failures = 260;
        let fixture = Fixture::new(
            (0..failures)
                .map(|_| Step::Startup(recoverable()))
                .chain([answer("recovered")]),
        )
        .await;
        assert_eq!(
            fixture.session.prompt("recover").await.unwrap(),
            "recovered"
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), failures + 1);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(fixture.script.resets.load(Ordering::SeqCst), failures);
        let records = fixture.records().await;
        let requested: Vec<_> = records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
            .collect();
        assert_eq!(
            requested.len(),
            1,
            "one frozen logical request across retries"
        );
        let started: Vec<_> = records
            .iter()
            .filter_map(|record| match record.event {
                SessionEvent::ModelAttemptStarted { request, attempt } => Some((request, attempt)),
                _ => None,
            })
            .collect();
        assert_eq!(
            started,
            (1..=failures as u64 + 1)
                .map(|attempt| (requested[0].sequence, attempt))
                .collect::<Vec<_>>()
        );
        assert_schedule(&records, &(2..=failures as u64 + 1).collect::<Vec<_>>());
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                .count(),
            failures
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_during_backoff_prevents_the_next_invocation() {
        let fixture = Fixture::new(
            (0..4)
                .map(|_| Step::Startup(recoverable()))
                .chain([answer("must not run")]),
        )
        .await;
        let mut events = fixture.session.subscribe();
        let session = fixture.session.clone();
        let prompt = tokio::spawn(async move { session.prompt("recover then cancel").await });
        for _ in 0..4 {
            next_recovery(&mut events).await;
        }
        fixture.session.interrupt().await;
        let result = tokio::time::timeout(Duration::from_millis(300), prompt)
            .await
            .expect("cancellation must not wait for the recovery delay")
            .unwrap();
        assert!(result.is_err());
        assert_eq!(fixture.requests().len(), 4);
        assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
        fixture.session.shutdown().await.unwrap();
        assert_eq!(fixture.requests().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn all_transient_categories_retry_past_three_startup_or_stream_failures() {
        for kind in [
            ProviderErrorKind::Response,
            ProviderErrorKind::Transport,
            ProviderErrorKind::Timeout,
            ProviderErrorKind::RateLimited,
        ] {
            for streaming in [false, true] {
                let failures = 7;
                let fixture = Fixture::new(
                    (0..failures)
                        .map(|_| {
                            let error = ProviderError {
                                kind,
                                message: "provider stream error".into(),
                                retry_after: None,
                            };
                            if streaming {
                                let mut chunks: Vec<_> = events_for_content(&[
                                    AssistantContent::text(
                                        "partial",
                                        1,
                                        "discard failed partial answer",
                                    ),
                                    write_call("failed", "must-not-exist"),
                                ])
                                .into_iter()
                                .map(Ok)
                                .collect();
                                chunks.push(Err(error));
                                Step::Stream(chunks)
                            } else {
                                Step::Startup(error)
                            }
                        })
                        .chain([answer("recovered")]),
                )
                .await;
                assert_eq!(
                    fixture.session.prompt("keep retrying").await.unwrap(),
                    "recovered"
                );
                let requests = fixture.requests();
                assert_eq!(
                    requests.len(),
                    failures + 1,
                    "{kind:?}, streaming={streaming}"
                );
                assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
                assert_eq!(fixture.script.resets.load(Ordering::SeqCst), failures);
                assert!(!fixture.workspace.path().join("must-not-exist").exists());
                let records = fixture.records().await;
                let scheduled = recoveries(&records);
                assert_eq!(scheduled.len(), failures);
                assert!(scheduled.iter().all(|recovery| recovery.2.is_none()));
                assert_eq!(
                    records
                        .iter()
                        .filter(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                        .count(),
                    failures
                );
                let history = project_history(&records, &fixture.session.root).unwrap();
                let encoded = serde_json::to_string(&history).unwrap();
                for discarded in [
                    "provider stream error",
                    "discard failed partial answer",
                    "must-not-exist",
                ] {
                    assert!(!encoded.contains(discarded));
                }
                assert_eq!(
                    history
                        .iter()
                        .filter(|(_, message)| matches!(message, Message::Assistant(_)))
                        .count(),
                    1
                );
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_error_kinds_ignore_retry_sounding_messages() {
        for kind in [
            ProviderErrorKind::Authentication,
            ProviderErrorKind::Protocol,
            ProviderErrorKind::InvalidRequest,
        ] {
            for streaming in [false, true] {
                let error = ProviderError {
                    retry_after: None,
                    kind,
                    message:
                        "websocket connection lost; retry this request; previous_response_not_found"
                            .into(),
                };
                let failure = if streaming {
                    Step::Stream(vec![Err(error)])
                } else {
                    Step::Startup(error)
                };
                let fixture = Fixture::new([failure, answer("must not run")]).await;
                assert!(
                    fixture
                        .session
                        .prompt("fail without recovery")
                        .await
                        .is_err(),
                    "{kind:?}"
                );
                assert_eq!(fixture.requests().len(), 1, "{kind:?}");
                assert_eq!(fixture.script.resets.load(Ordering::SeqCst), 0, "{kind:?}");
                assert!(recoveries(&fixture.records().await).is_empty(), "{kind:?}");
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn child_recovery_keeps_the_same_owner_job_and_does_not_fail_the_agent() {
        let fixture = Fixture::new([Step::Startup(recoverable()), answer("child recovered")]).await;
        let session = &fixture.session;
        let child = session.root.child(1);
        let owner = session
            .runtime
            .jobs
            .create(crate::job::JobSpec::test(session.root.clone(), "agent"))
            .await
            .unwrap();
        session
            .runtime
            .jobs
            .transition(owner.id, crate::job::JobState::Running)
            .await
            .unwrap();
        let sender = session
            .runtime
            .spawn_agent(AgentLaunch {
                id: child.clone(),
                owner_job: Some(owner.id),
                model_profile: "test".into(),
                todos: None,
                available_depth: 0,
                location: crate::execution::ExecutionLocation::root(
                    fixture.workspace.path().to_owned(),
                ),
            })
            .await
            .unwrap();
        let mut events = session.subscribe();
        let (done, received) = oneshot::channel();
        sender
            .send(AgentCommand::Input {
                model: None,
                content: vec![UserContent::Text {
                    text: "recover".into(),
                }],
                done: Some(done),
            })
            .await
            .unwrap();
        let scheduled = next_recovery(&mut events).await;
        assert_eq!(scheduled.agent, child);
        assert_eq!(
            session.runtime.jobs.snapshot(owner.id).await.unwrap().state,
            crate::job::JobState::Running
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), received)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            "child recovered"
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, RuntimeEvent::Activity { agent, activity: AgentActivity::Failed(_) } if agent == child)
            );
        }
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].correlation, Some(child.to_string()));
        let records = fixture.records().await;
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
                .count(),
            1
        );
        assert!(!records.iter().any(|record| record.agent == child
            && matches!(record.event, SessionEvent::AgentInterrupted)));
        assert_eq!(
            fixture.script.opened.load(Ordering::SeqCst),
            2,
            "root plus the original child context"
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_input_is_deferred_until_the_frozen_request_recovers() {
        let fixture = Fixture::new([
            Step::Startup(recoverable()),
            answer("first response"),
            answer("queued response"),
        ])
        .await;
        let mut events = fixture.session.subscribe();
        let session = fixture.session.clone();
        let turn = tokio::spawn(async move { session.prompt("initial input").await });
        next_recovery(&mut events).await;
        let session = fixture.session.clone();
        let token = QueuedPromptToken::new();
        let queued_token = token.clone();
        let receipt = tokio::spawn(async move {
            session
                .enqueue_prompt_with_options(
                    "queued input",
                    &[],
                    PromptOptions::default(),
                    queued_token,
                )
                .await
        });
        tokio::time::timeout(Duration::from_millis(300), async {
            while fixture.session.root_tx.capacity() == AGENT_CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued input should reach the mailbox during backoff");
        assert!(!token.is_claimed());
        assert!(!receipt.is_finished());
        assert_eq!(fixture.requests().len(), 1);
        assert!(
            !serde_json::to_string(
                &project_history(&fixture.records().await, &fixture.session.root).unwrap()
            )
            .unwrap()
            .contains("queued input")
        );
        tokio::time::timeout(Duration::from_secs(5), receipt)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(token.is_claimed());
        let requests = fixture.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert!(
            serde_json::to_string(&requests[2])
                .unwrap()
                .contains("queued input")
        );
        assert!(
            !serde_json::to_string(&requests[1])
                .unwrap()
                .contains("queued input")
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_do_not_exhaust_context_compaction_budget() {
        fn overflow() -> Step {
            Step::Startup(ProviderError {
                retry_after: None,
                kind: ProviderErrorKind::ContextWindowExceeded,
                message: "scripted context overflow".into(),
            })
        }
        fn summary(objective: String) -> Step {
            answer(
                &json!({
                    "objective": objective,
                    "user_instructions": [], "session_rules": [], "plan": [],
                    "resumption_point": "Continue the user's task.", "completed_work": [],
                    "findings": [], "decisions": [], "open_issues": [], "next_actions": [],
                    "running_work": [], "recovery_details": [], "jobs": [],
                    "additional_context": [], "todo_reconciliation": [], "todos": []
                })
                .to_string(),
            )
        }
        let fixture = Fixture::new((0..4).map(|_| Step::Startup(recoverable())).chain([
            overflow(),
            summary("preserve context ".repeat(100)),
            Step::Startup(recoverable()),
            answer("recovered after compaction"),
        ]))
        .await;
        // Exceed compaction's 8,000-token verbatim tail so the old assistant work
        // is summarized rather than retained alongside the continuation.
        fixture
            .session
            .runtime
            .commit(
                &fixture.session.root,
                Message::Assistant(vec![AssistantContent::text(
                    "history",
                    0,
                    "original context ".repeat(5000),
                )]),
            )
            .await
            .unwrap();
        assert_eq!(
            fixture.session.prompt("continue").await.unwrap(),
            "recovered after compaction"
        );
        let requests = fixture.requests();
        assert_eq!(requests.len(), 8);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.response_schema.is_none())
                .count(),
            7
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.response_schema.is_some())
                .count(),
            1
        );
        assert!(requests[..5].windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(requests[4], requests[6]);
        assert_eq!(requests[6], requests[7]);
        assert!(fixture.script.steps.lock().unwrap().is_empty());
        let records = fixture.records().await;
        assert_schedule(&records, &[2, 3, 4, 5, 7]);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::Compaction { .. }))
                .count(),
            1
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_context_overflows_remain_bounded_to_three_failures() {
        for streaming in [false, true] {
            let overflow = || {
                let error = ProviderError {
                    kind: ProviderErrorKind::ContextWindowExceeded,
                    message: "scripted context overflow".into(),
                    retry_after: None,
                };
                if streaming {
                    Step::Stream(vec![Err(error)])
                } else {
                    Step::Startup(error)
                }
            };
            let summary = || {
                answer(
                    &json!({
                        "objective": "Continue the user's task.",
                        "user_instructions": [], "session_rules": [], "plan": [],
                        "resumption_point": "Continue the user's task.", "completed_work": [],
                        "findings": [], "decisions": [], "open_issues": [], "next_actions": [],
                        "running_work": [], "recovery_details": [], "jobs": [],
                        "additional_context": [], "todo_reconciliation": [], "todos": []
                    })
                    .to_string(),
                )
            };
            let fixture = Fixture::new([
                overflow(),
                summary(),
                overflow(),
                summary(),
                overflow(),
                answer("must not run"),
            ])
            .await;
            let error = fixture.session.prompt("continue").await.unwrap_err();
            assert!(error.to_string().contains("scripted context overflow"));
            assert_eq!(
                fixture.requests().len(),
                5,
                "three context failures and two summaries"
            );
            assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
            let records = fixture.records().await;
            assert!(recoveries(&records).is_empty());
            assert_eq!(
                records
                    .iter()
                    .filter(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                    .count(),
                3
            );
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_provider_call_is_journaled_before_invocation() {
        #[derive(Clone)]
        struct FailingProvider {
            session_root: PathBuf,
        }
        impl Provider for FailingProvider {
            fn open_context(
                &self,
                _correlation: String,
            ) -> Result<Box<dyn ProviderContext>, ProviderError> {
                Ok(Box::new(self.clone()))
            }
        }

        impl ProviderContext for FailingProvider {
            fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
                let session = request
                    .correlation
                    .as_ref()
                    .unwrap()
                    .split(':')
                    .next()
                    .unwrap();
                let path = self.session_root.join(session).join("events.jsonl");
                Box::pin(async move {
                    let journal = fs::read_to_string(path).await.unwrap();
                    let records = journal
                        .lines()
                        .map(|line| serde_json::from_str::<EventRecord>(line).unwrap())
                        .collect::<Vec<_>>();
                    let call = records.last().unwrap();
                    let SessionEvent::ModelAttemptStarted {
                        request: sequence,
                        attempt: 1,
                    } = call.event
                    else {
                        panic!("attempt start must be durable before invocation");
                    };
                    let (provider, restored) =
                        crate::session::reconstruct_model_request(&records, sequence).unwrap();
                    assert_eq!(provider, "test");
                    assert_eq!(restored, request);
                    Err(crate::provider::ProviderError::protocol(
                        "intentional provider failure",
                    ))
                })
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(FailingProvider {
                session_root: sessions.path().to_owned(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let error = session.prompt("test failure").await.unwrap_err();
        assert!(error.to_string().contains("intentional provider failure"));
    }
}

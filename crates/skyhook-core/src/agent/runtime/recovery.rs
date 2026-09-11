//! Transient model failure recovery and usage accounting.

use super::*;

impl SessionRuntime {
    /// Recovery is bounded per logical response and never restarts a child or
    /// re-executes committed tools. The caller has recorded the failed attempt and
    /// dropped the provider's failed stream before entering this wait.
    pub(super) async fn schedule_model_recovery(
        &self,
        turn: &TurnContext<'_>,
        request: u64,
        model_attempt: u8,
        connection_attempt: u8,
        error: &crate::provider::ProviderError,
        provider: &mut dyn crate::provider::ProviderContext,
    ) -> Result<(), HarnessError> {
        if turn.cancellation.is_cancelled() {
            return Err(HarnessError::Interrupted);
        }
        if connection_attempt >= MAX_CONNECTION_ATTEMPTS || model_attempt >= MAX_MODEL_ATTEMPTS {
            let mut exhausted = error.clone();
            exhausted.message = format!(
                "{}; connection recovery exhausted after {} attempts ({} total model attempts); completed tool results were preserved",
                error.message, connection_attempt, model_attempt,
            );
            return Err(exhausted.into());
        }
        provider.reset();
        // Bounded positive jitter avoids synchronized reconnects without making
        // randomness a prerequisite for recovery.
        let mut jitter = [0u8; 1];
        let _ = getrandom::fill(&mut jitter);
        let delay_millis =
            if connection_attempt == 1 { 500 } else { 1500 } + u64::from(jitter[0] % 101);
        self.store
            .append(
                turn.agent.clone(),
                SessionEvent::ModelRecoveryScheduled {
                    request,
                    attempt: connection_attempt + 1,
                    max_attempts: MAX_CONNECTION_ATTEMPTS,
                    delay_millis,
                    error: error.to_string(),
                },
            )
            .await?;
        tokio::select! {
            () = turn.cancellation.cancelled() => Err(HarnessError::Interrupted),
            () = tokio::time::sleep(std::time::Duration::from_millis(delay_millis)) => Ok(()),
        }
    }

    pub(super) async fn record_model_failure(
        &self,
        agent: &AgentId,
        request: u64,
        attempt: u8,
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
    //! Exercise connection recovery through the real agent loop and durable journal.

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
            kind: ProviderErrorKind::CodexWebSocket(CodexWebSocketError::Read),
            message: "scripted websocket connection lost".into(),
        }
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

    fn recoveries(records: &[EventRecord]) -> Vec<(u64, u8, u8, u64, String)> {
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

    fn assert_schedule(records: &[EventRecord], expected_attempts: &[u8]) {
        let scheduled = recoveries(records);
        assert_eq!(
            scheduled.iter().map(|entry| entry.1).collect::<Vec<_>>(),
            expected_attempts
        );
        for (request, attempt, maximum, delay, error) in scheduled {
            assert_eq!(maximum, 3);
            let base = match attempt {
                2 => 500,
                3 => 1500,
                _ => panic!("unexpected retry"),
            };
            assert!(
                (base..=base + 100).contains(&delay),
                "unbounded retry jitter: {delay}"
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

    #[tokio::test]
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

    #[tokio::test]
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
            [observed_usage],
            "known failed usage must be recorded exactly once"
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

    #[tokio::test]
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

    #[tokio::test]
    async fn connection_exhaustion_is_three_total_attempts_and_only_two_delays() {
        let fixture = Fixture::new((0..3).map(|_| Step::Startup(recoverable()))).await;
        let error = fixture.session.prompt("fail").await.unwrap_err();
        assert!(error.to_string().contains("connection lost"));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(fixture.script.resets.load(Ordering::SeqCst), 2);
        let records = fixture.records().await;
        assert_schedule(&records, &[2, 3]);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelFailed { .. }))
                .count(),
            3
        );
        assert!(!records.iter().any(|record| matches!(
            record.event,
            SessionEvent::MessageCommitted {
                message: Message::Assistant(_)
            }
        )));
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_during_backoff_prevents_the_next_invocation() {
        let fixture = Fixture::new([Step::Startup(recoverable()), answer("must not run")]).await;
        let mut events = fixture.session.subscribe();
        let session = fixture.session.clone();
        let prompt = tokio::spawn(async move { session.prompt("recover then cancel").await });
        next_recovery(&mut events).await;
        fixture.session.interrupt().await;
        let result = tokio::time::timeout(Duration::from_millis(300), prompt)
            .await
            .expect("cancellation must not wait for the recovery delay")
            .unwrap();
        assert!(result.is_err());
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
        fixture.session.shutdown().await.unwrap();
        assert_eq!(fixture.requests().len(), 1);
    }

    #[tokio::test]
    async fn ordinary_provider_failures_stop_after_one_attempt() {
        for streaming in [false, true] {
            let error = ProviderError {
                kind: ProviderErrorKind::Transport,
                message: "ordinary provider failure".into(),
            };
            let failure = if streaming {
                // Even a complete tool block must not execute if its response fails.
                let mut chunks: Vec<_> =
                    events_for_content(&[write_call("failed", "must-not-exist")])
                        .into_iter()
                        .map(Ok)
                        .collect();
                chunks.push(Err(error));
                Step::Stream(chunks)
            } else {
                Step::Startup(error)
            };
            let fixture = Fixture::new([failure, answer("must not retry")]).await;
            assert!(
                fixture
                    .session
                    .prompt("This request will fail.")
                    .await
                    .is_err()
            );
            assert_eq!(fixture.requests().len(), 1);
            assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
            assert_eq!(fixture.script.resets.load(Ordering::SeqCst), 0);
            assert!(!fixture.workspace.path().join("must-not-exist").exists());
            assert!(!fixture.records().await.iter().any(|record| matches!(
                record.event,
                SessionEvent::JobCreated { .. } | SessionEvent::ModelRecoveryScheduled { .. }
            )));
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn generic_error_kinds_and_retry_sounding_messages_are_terminal() {
        for kind in [
            ProviderErrorKind::Authentication,
            ProviderErrorKind::RateLimited,
            ProviderErrorKind::Timeout,
            ProviderErrorKind::Transport,
            ProviderErrorKind::Protocol,
            ProviderErrorKind::InvalidRequest,
            ProviderErrorKind::Response,
        ] {
            for streaming in [false, true] {
                let error = ProviderError {
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn mixed_connection_and_compaction_recovery_is_additive_not_nested() {
        fn overflow() -> Step {
            Step::Startup(ProviderError {
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
        let fixture = Fixture::new([
            Step::Startup(recoverable()),
            overflow(),
            summary("preserve context ".repeat(100)),
            Step::Startup(recoverable()),
            overflow(),
            summary("continue".into()),
            Step::Startup(recoverable()),
            answer("must not exceed five agent attempts"),
        ])
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
        let error = fixture.session.prompt("continue").await.unwrap_err();
        assert!(error.to_string().contains("5 total model attempts"));
        let requests = fixture.requests();
        // Summary invocations have their own schema and do not restart either
        // recovery allowance. Five agent attempts plus two compaction summaries.
        assert_eq!(requests.len(), 7);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.response_schema.is_none())
                .count(),
            5
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.response_schema.is_some())
                .count(),
            2
        );
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[3], requests[4]);
        assert_ne!(requests[1], requests[3]);
        assert_ne!(requests[4], requests[6]);
        assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
        let records = fixture.records().await;
        assert_schedule(&records, &[2, 3]);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::Compaction { .. }))
                .count(),
            2
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
        );
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
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
                    let (provider, restored) =
                        crate::session::reconstruct_model_request(&records, call.sequence).unwrap();
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

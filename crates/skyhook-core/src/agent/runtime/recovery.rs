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
        self.record_model_outcome(
            agent,
            request,
            attempt,
            usage,
            error,
            crate::session::ModelFailureKind::Error,
        )
        .await
    }

    /// Record a failed attempt with its classification. A refusal is journaled the
    /// same way as any other failure so hosts render and resume it identically,
    /// but its kind keeps it out of automatic recovery.
    pub(super) async fn record_model_outcome(
        &self,
        agent: &AgentId,
        request: u64,
        attempt: u64,
        usage: Usage,
        error: String,
        kind: crate::session::ModelFailureKind,
    ) -> Result<(), HarnessError> {
        // Observed usage and the attempt's outcome commit together.
        let mut events = Vec::new();
        if usage != Usage::default() {
            let request = Some(request);
            events.push((agent.clone(), SessionEvent::Usage { request, usage }));
        }
        let failed = SessionEvent::ModelFailed {
            request,
            attempt,
            error,
            kind,
        };
        events.push((agent.clone(), failed));
        self.store.append_all(events).await?;
        self.usage.lock().await.accumulate(usage);
        Ok(())
    }

    /// Close an attempt cancelled before it produced an outcome.
    pub(super) async fn record_attempt_interrupted(
        &self,
        agent: &AgentId,
        request: u64,
        attempt: u64,
    ) -> Result<(), HarnessError> {
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelAttemptInterrupted { request, attempt },
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
    use crate::agent::runtime::tests::{
        bounded, cloned_provider, count, enqueue_prompts, events, owner, summary_json,
        test_builder, test_harness, tool_call,
    };
    use crate::provider::{
        CodexWebSocketError, ProviderContext, ProviderError, ProviderErrorKind, ProviderFuture,
        ResponseStream,
        protocol::{StopReason, events_for_content},
    };
    use crate::session::{ModelPurpose, project_history};

    // Failures reuse one provider context: a reset must not reopen agents or replay tools.
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

    fn error(kind: ProviderErrorKind, message: &str) -> ProviderError {
        ProviderError {
            retry_after: None,
            kind,
            message: message.into(),
        }
    }

    fn recoverable() -> ProviderError {
        let kind = ProviderErrorKind::CodexWebSocket(CodexWebSocketError::Read);
        error(kind, "scripted websocket connection lost")
    }

    fn overflow(streaming: bool) -> Step {
        let error = error(
            ProviderErrorKind::ContextWindowExceeded,
            "scripted context overflow",
        );
        if streaming {
            Step::Stream(vec![Err(error)])
        } else {
            Step::Startup(error)
        }
    }

    fn success(items: Vec<AssistantContent>, stop_reason: StopReason) -> Step {
        let mut chunks = events_for_content(&items);
        chunks.push(ResponseChunk::ResponseEnded { stop_reason });
        Step::Stream(chunks.into_iter().map(Ok).collect())
    }

    fn answer(text: &str) -> Step {
        let items = vec![AssistantContent::text("answer", 0, text)];
        success(items, StopReason::EndTurn)
    }
    fn write_call(id: &str, path: &str) -> AssistantContent {
        tool_call(0, id, "write", json!({"path":path, "content":id}))
    }

    fn write_step(id: &str, path: &str) -> Step {
        success(vec![write_call(id, path)], StopReason::ToolUse)
    }

    struct Fixture {
        workspace: tempfile::TempDir,
        session: SessionHandle,
        script: Arc<Script>,
    }

    impl Fixture {
        async fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            let steps = StdMutex::new(steps.into_iter().collect());
            let script = Arc::new(Script {
                steps,
                ..Script::default()
            });
            let provider = Arc::new(Factory(script.clone()));
            let sessions = workspace.path().join("sessions");
            let builder = test_builder(workspace.path(), &sessions, provider, true);
            let session = builder.build().await.unwrap().new_session().await.unwrap();
            Self {
                workspace,
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

        async fn prompt(&self, text: &str) -> String {
            self.session.prompt(text).await.unwrap()
        }

        fn resets(&self) -> usize {
            self.script.resets.load(Ordering::SeqCst)
        }

        /// Root history as JSON, plus its number of assistant messages.
        fn history(&self, records: &[EventRecord]) -> (String, usize) {
            let history = project_history(records, &self.session.root).unwrap();
            let assistants = history
                .iter()
                .filter(|(_, message)| matches!(message, Message::Assistant(_)));
            let assistants = assistants.count();
            (serde_json::to_string(&history).unwrap(), assistants)
        }
    }

    fn recoveries(records: &[EventRecord]) -> Vec<(u64, u64, Option<u64>, u64, String)> {
        events!(records, SessionEvent::ModelRecoveryScheduled { request, attempt, max_attempts, delay_millis, error }
            => (*request, *attempt, *max_attempts, *delay_millis, error.clone()))
    }

    fn assert_schedule(records: &[EventRecord], expected_attempts: &[u64]) {
        let scheduled = recoveries(records);
        let attempts = scheduled.iter().map(|entry| entry.1).collect::<Vec<_>>();
        assert_eq!(attempts, expected_attempts);
        let mut transient_attempts = std::collections::HashMap::<u64, u64>::new();
        for (request, _attempt, maximum, delay, error) in scheduled {
            let transient = transient_attempts.entry(request).or_default();
            *transient += 1;
            let expected = recovery_delay(&recoverable(), *transient).as_millis() as u64;
            assert_eq!((maximum, delay), (None, expected));
            assert_eq!(error, recoverable().to_string());
            let requested = records.iter().filter(|record| record.sequence == request);
            assert_eq!(
                count!(
                    requested,
                    SessionEvent::ModelRequested {
                        purpose: ModelPurpose::Agent,
                        ..
                    }
                ),
                1
            );
            assert_ne!(
                count!(records, SessionEvent::ModelFailed { request: failed, error: failure, .. } if *failed == request && failure == &error),
                0
            );
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

    #[test]
    fn fallback_backoff_caps_without_overflow_and_server_hint_overrides_it() {
        let mut error = recoverable();
        let attempts = [0, 1, 2, 3, 4, 5, 6, 7, 256, u64::MAX];
        let seconds = [1, 1, 2, 4, 8, 16, 30, 30, 30, 30];
        for (attempt, seconds) in attempts.into_iter().zip(seconds) {
            let expected = Duration::from_secs(seconds);
            assert_eq!(recovery_delay(&error, attempt), expected);
        }
        for hint in [0, 1234, 90_000].map(Duration::from_millis) {
            error.retry_after = Some(hint);
            assert_eq!(recovery_delay(&error, 1), hint);
            assert_eq!(recovery_delay(&error, u64::MAX), hint);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn startup_failure_retries_the_frozen_request_and_honors_server_retry_after() {
        let mut error = recoverable();
        error.retry_after = Some(Duration::from_secs(90));
        let fixture = Fixture::new([Step::Startup(error), answer("recovered")]).await;
        let image = crate::media::Attachment::Image {
            file: Some(fixture.workspace.path().join("evidence.png")),
            image: crate::tests::png(b"request image fixture"),
        };
        let (images, options) = ([image], PromptOptions::default());
        let started = tokio::time::Instant::now();
        let prompt = fixture
            .session
            .prompt_with_options("continue", &images, options);
        assert_eq!(prompt.await.unwrap(), "recovered");
        // A server hint above the fallback cap is honored and journaled.
        assert!(started.elapsed() >= Duration::from_secs(90));
        let records = fixture.records().await;
        let scheduled = recoveries(&records).into_iter();
        let scheduled =
            scheduled.map(|(_, attempt, max, delay, error)| (attempt, max, delay, error));
        let expected = (2, None, 90_000, recoverable().to_string());
        assert_eq!(scheduled.collect::<Vec<_>>(), [expected]);
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        // The retry uses the frozen hydrated request on the same, reset context.
        assert_eq!(requests[0], requests[1]);
        assert!(requests[0].messages().any(|message| matches!(message,
            Message::User(blocks) if blocks.iter().any(|block| matches!(block,
                UserContent::Attachment { attachment: crate::media::AttachmentRef::Image(image) }
                    if requests[0].blobs.get(&image.blob).is_ok())))));
        assert_eq!(fixture.script.opened.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.resets(), 1);
        let (history, assistants) = fixture.history(&records);
        assert_eq!(assistants, 1);
        assert!(!history.contains("connection lost"));
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_partial_text_and_complete_tool_block_are_discarded_before_retry() {
        let observed_usage = Usage {
            input_tokens: 71,
            cached_input_tokens: 13,
            output_tokens: 5,
        };
        let mut partial = vec![Ok(ResponseChunk::UsageUpdated {
            usage: observed_usage,
        })];
        let discarded = json!({"path":"must-not-exist", "content":"bad"});
        let items = [
            AssistantContent::text("partial", 0, "DO NOT COMMIT"),
            tool_call(1, "discarded", "write", discarded),
        ];
        partial.extend(events_for_content(&items).into_iter().map(Ok));
        partial.push(Err(recoverable()));
        let committed = write_step("committed", "successful-tool");
        let fixture = Fixture::new([Step::Stream(partial), committed, answer("done")]).await;
        assert_eq!(fixture.prompt("write once").await, "done");
        let workspace = fixture.workspace.path();
        assert!(!workspace.join("must-not-exist").exists());
        let written = std::fs::read_to_string(workspace.join("successful-tool")).unwrap();
        assert_eq!(written, "committed");
        let records = fixture.records().await;
        let failed_request = recoveries(&records)[0].0;
        let failed_usage = events!(&records, SessionEvent::Usage { request: Some(request), usage } if *request == failed_request => *usage);
        // failed usage and successful usage must each be recorded exactly once
        assert_eq!(failed_usage, [observed_usage, Usage::default()]);
        assert_eq!(fixture.session.usage().await, observed_usage);
        let jobs = count!(&records, SessionEvent::JobCreated { .. });
        assert_eq!(jobs, 1, "only the successful attempt may execute tools");
        let (history, _) = fixture.history(&records);
        assert!(!history.contains("DO NOT COMMIT") && !history.contains("discarded"));
        let requests = fixture.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert!(
            requests[2]
                .messages()
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
            write_step("prior-tool", "prior-tool"),
            Step::Startup(recoverable()),
            Step::Startup(recoverable()),
            answer("done"),
        ])
        .await;
        assert_eq!(fixture.prompt("write and finish").await, "done");
        let requests = fixture.requests();
        assert_eq!(requests.len(), 6);
        assert!(requests[..3].windows(2).all(|pair| pair[0] == pair[1]));
        assert!(requests[3..].windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(requests[2], requests[3]);
        for request in &requests[3..] {
            let tool = |message: &&Message| matches!(message, Message::Tool(_));
            assert_eq!(request.messages().filter(tool).count(), 1);
        }
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 1);
        let written = std::fs::read_to_string(fixture.workspace.path().join("prior-tool"));
        assert_eq!(written.unwrap(), "prior-tool");
        assert_schedule(&records, &[2, 3, 2, 3]);
        assert_eq!(fixture.resets(), 4);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn websocket_retries_past_u8_limit_then_succeeds() {
        let failures = 260;
        let steps = (0..failures).map(|_| Step::Startup(recoverable()));
        let fixture = Fixture::new(steps.chain([answer("recovered")])).await;
        assert_eq!(fixture.prompt("recover").await, "recovered");
        let requests = fixture.requests();
        assert_eq!(requests.len(), failures + 1);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(fixture.resets(), failures);
        let records = fixture.records().await;
        let requested = count!(&records, SessionEvent::ModelRequested { .. });
        assert_eq!(requested, 1, "one frozen logical request across retries");
        let started = events!(&records, SessionEvent::ModelAttemptStarted { request, attempt } => (*request, *attempt));
        let request = started[0].0;
        let attempts = (1..=failures as u64 + 1).map(|attempt| (request, attempt));
        assert_eq!(started, attempts.collect::<Vec<_>>());
        let at_request = records.iter().filter(|record| record.sequence == request);
        assert_eq!(count!(at_request, SessionEvent::ModelRequested { .. }), 1);
        assert_schedule(&records, &(2..=failures as u64 + 1).collect::<Vec<_>>());
        assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), failures);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_during_backoff_prevents_the_next_invocation() {
        let steps = (0..4).map(|_| Step::Startup(recoverable()));
        let fixture = Fixture::new(steps.chain([answer("must not run")])).await;
        let mut events = fixture.session.subscribe();
        let session = fixture.session.clone();
        let prompt = tokio::spawn(async move { session.prompt("recover then cancel").await });
        for _ in 0..4 {
            next_recovery(&mut events).await;
        }
        fixture.session.interrupt().await;
        let result = tokio::time::timeout(Duration::from_millis(300), prompt)
            .await
            .expect("cancellation must not wait for the recovery delay");
        assert!(result.unwrap().is_err());
        assert_eq!(fixture.requests().len(), 4);
        assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
        fixture.session.shutdown().await.unwrap();
        assert_eq!(fixture.requests().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn all_transient_categories_retry_past_three_startup_or_stream_failures() {
        use ProviderErrorKind::{RateLimited, Response, Timeout, Transport};
        for kind in [Response, Transport, Timeout, RateLimited] {
            for streaming in [false, true] {
                let failures = 7;
                let failure = || {
                    let error = error(kind, "provider stream error");
                    if !streaming {
                        return Step::Startup(error);
                    }
                    let items = [
                        AssistantContent::text("partial", 1, "discard failed partial answer"),
                        write_call("failed", "must-not-exist"),
                    ];
                    let mut chunks: Vec<_> =
                        events_for_content(&items).into_iter().map(Ok).collect();
                    chunks.push(Err(error));
                    Step::Stream(chunks)
                };
                let steps = (0..failures)
                    .map(|_| failure())
                    .chain([answer("recovered")]);
                let fixture = Fixture::new(steps).await;
                assert_eq!(fixture.prompt("keep retrying").await, "recovered");
                let requests = fixture.requests();
                assert_eq!(
                    requests.len(),
                    failures + 1,
                    "{kind:?}, streaming={streaming}"
                );
                assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
                assert_eq!(fixture.resets(), failures);
                assert!(!fixture.workspace.path().join("must-not-exist").exists());
                let records = fixture.records().await;
                let scheduled = recoveries(&records);
                assert_eq!(scheduled.len(), failures);
                assert!(scheduled.iter().all(|recovery| recovery.2.is_none()));
                assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), failures);
                let (history, assistants) = fixture.history(&records);
                for discarded in [
                    "provider stream error",
                    "discard failed partial answer",
                    "must-not-exist",
                ] {
                    assert!(!history.contains(discarded));
                }
                assert_eq!(assistants, 1);
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn permanent_error_kinds_ignore_retry_sounding_messages() {
        use ProviderErrorKind::{Authentication, InvalidRequest, Protocol};
        for kind in [Authentication, Protocol, InvalidRequest] {
            for streaming in [false, true] {
                let message =
                    "websocket connection lost; retry this request; previous_response_not_found";
                let error = error(kind, message);
                let failure = if streaming {
                    Step::Stream(vec![Err(error)])
                } else {
                    Step::Startup(error)
                };
                let fixture = Fixture::new([failure, answer("must not run")]).await;
                let result = fixture.session.prompt("fail without recovery").await;
                assert!(result.is_err(), "{kind:?}");
                assert_eq!(fixture.requests().len(), 1, "{kind:?}");
                assert_eq!(fixture.resets(), 0, "{kind:?}");
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
        let owner = owner(session).await;
        let location =
            crate::execution::ExecutionLocation::root(fixture.workspace.path().to_owned());
        let launch = AgentLaunch {
            id: child.clone(),
            owner_job: Some(owner),
            model_profile: "test".into(),
            todos: None,
            available_depth: 0,
            location,
        };
        let sender = session.runtime.spawn_agent(launch).await.unwrap();
        let mut events = session.subscribe();
        let (done, received) = oneshot::channel();
        let content = vec![UserContent::Text {
            text: "recover".into(),
        }];
        let input = AgentCommand::Input {
            model: None,
            content,
            done: Some(done),
        };
        sender.send(input).await.unwrap();
        assert_eq!(next_recovery(&mut events).await.agent, child);
        let state = session.runtime.jobs.snapshot(owner).await.unwrap().state;
        assert_eq!(state, crate::job::JobState::Running);
        let received = tokio::time::timeout(Duration::from_secs(5), received).await;
        assert_eq!(received.unwrap().unwrap().unwrap(), "child recovered");
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
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 1);
        let child_records = records.iter().filter(|record| record.agent == child);
        assert_eq!(count!(child_records, SessionEvent::AgentInterrupted), 0);
        let opened = fixture.script.opened.load(Ordering::SeqCst);
        assert_eq!(opened, 2, "root plus the original child context");
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_input_is_deferred_until_the_frozen_request_recovers() {
        let steps = [
            Step::Startup(recoverable()),
            answer("first"),
            answer("queued"),
        ];
        let fixture = Fixture::new(steps).await;
        let mut events = fixture.session.subscribe();
        let session = fixture.session.clone();
        let turn = tokio::spawn(async move { session.prompt("initial input").await });
        next_recovery(&mut events).await;
        let session = fixture.session.clone();
        let token = QueuedPromptToken::new().unwrap();
        let token_cancel = token.cancellation_handle();
        let prompt = QueuedPrompt {
            text: "queued input".into(),
            attachments: vec![],
            options: PromptOptions::default(),
            token,
        };
        let receipt =
            tokio::spawn(
                async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() },
            );
        tokio::time::timeout(Duration::from_millis(300), async {
            while fixture.session.root_tx.capacity() == AGENT_CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("queued input should reach the mailbox during backoff");
        assert!(!token_cancel.is_claimed());
        assert!(!receipt.is_finished());
        assert_eq!(fixture.requests().len(), 1);
        let (history, _) = fixture.history(&fixture.records().await);
        assert!(!history.contains("queued input"));
        bounded(receipt).await.unwrap().unwrap();
        bounded(turn).await.unwrap().unwrap();
        assert!(token_cancel.is_claimed());
        let requests = fixture.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0], requests[1]);
        let queued = |index: usize| serde_json::to_string(&requests[index]).unwrap();
        assert!(queued(2).contains("queued input") && !queued(1).contains("queued input"));
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_do_not_exhaust_context_compaction_budget() {
        let mut summary = summary_json();
        summary["objective"] = json!("preserve context ".repeat(100));
        let fixture = Fixture::new((0..4).map(|_| Step::Startup(recoverable())).chain([
            overflow(false),
            answer(&summary.to_string()),
            Step::Startup(recoverable()),
            answer("recovered after compaction"),
        ]))
        .await;
        // Exceed the 8,000-token verbatim tail so old work is summarized, not retained.
        let history = "original context ".repeat(5000);
        let history = Message::Assistant(vec![AssistantContent::text("history", 0, history)]);
        let root = &fixture.session.root;
        fixture.session.runtime.commit(root, history).await.unwrap();
        let answer = fixture.prompt("continue").await;
        assert_eq!(answer, "recovered after compaction");
        let requests = fixture.requests();
        assert_eq!(requests.len(), 8);
        let summaries = requests
            .iter()
            .filter(|request| request.response_schema.is_some());
        assert_eq!(summaries.count(), 1);
        assert!(requests[..5].windows(2).all(|pair| pair[0] == pair[1]));
        assert_ne!(requests[4], requests[6]);
        assert_eq!(requests[6], requests[7]);
        assert!(fixture.script.steps.lock().unwrap().is_empty());
        let records = fixture.records().await;
        assert_schedule(&records, &[2, 3, 4, 5, 7]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_context_overflows_remain_bounded_to_three_failures() {
        for streaming in [false, true] {
            let summary = || answer(&summary_json().to_string());
            let fixture = Fixture::new([
                overflow(streaming),
                summary(),
                overflow(streaming),
                summary(),
                overflow(streaming),
                answer("must not run"),
            ])
            .await;
            let error = fixture.session.prompt("continue").await.unwrap_err();
            assert!(error.to_string().contains("scripted context overflow"));
            let requests = fixture.requests().len();
            assert_eq!(requests, 5, "three context failures and two summaries");
            assert_eq!(fixture.script.steps.lock().unwrap().len(), 1);
            let records = fixture.records().await;
            assert!(recoveries(&records).is_empty());
            assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 3);
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_provider_call_is_journaled_before_invocation() {
        #[derive(Clone)]
        struct FailingProvider {
            session_root: PathBuf,
        }
        cloned_provider!(FailingProvider);
        impl ProviderContext for FailingProvider {
            fn invoke(&mut self, request: ModelRequest) -> ProviderFuture {
                let correlation = request.correlation.as_ref().unwrap();
                let session = correlation.split(':').next().unwrap().parse().unwrap();
                let root = self.session_root.clone();
                Box::pin(async move {
                    // A separate reader sees only committed transactions.
                    let records = SessionStore::read_records(&root, session).await.unwrap();
                    let SessionEvent::ModelAttemptStarted {
                        request: sequence,
                        attempt: 1,
                    } = records.last().unwrap().event
                    else {
                        panic!("attempt start must be durable before invocation");
                    };
                    let reconstructed =
                        crate::session::reconstruct_model_request(&records, sequence);
                    assert_eq!(reconstructed.unwrap(), ("test".into(), request));
                    Err(ProviderError::protocol("intentional provider failure"))
                })
            }
        }
        let workspace = tempfile::tempdir().unwrap();
        let sessions = workspace.path().join("sessions");
        let provider = Arc::new(FailingProvider {
            session_root: sessions.clone(),
        });
        let harness = test_harness(workspace.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        let error = session.prompt("test failure").await.unwrap_err();
        assert!(error.to_string().contains("intentional provider failure"));
    }
}

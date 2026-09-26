//! Transient model failure recovery and usage accounting.

use super::*;

/// Server hints take precedence over the exponential fallback, including hints
/// longer than its 30-second cap. `attempt` counts transient failures of the
/// frozen request, independently of context/validation attempts.
fn recovery_delay(error: &crate::provider::ProviderError, attempt: u64) -> std::time::Duration {
    error.retry_after().unwrap_or_else(|| {
        std::time::Duration::from_secs(1u64 << attempt.saturating_sub(1).min(5))
            .min(std::time::Duration::from_secs(30))
    })
}

impl SessionRuntime {
    /// Journal a failed attempt and, when it is transient, back off before the
    /// caller retries its frozen request (`None`). A permanent error is handed back.
    /// Recovery continues until success or cancellation and never re-executes tools.
    ///
    /// `attempt` is the audit attempt of the logical response and the count of
    /// transient failures of the frozen request, which alone drives the backoff.
    pub(super) async fn recover_model_failure(
        &self,
        turn: &TurnContext<'_>,
        (attempt, transient): (crate::session::AttemptRef, &mut u64),
        usage: Usage,
        error: crate::provider::ProviderError,
    ) -> Result<Option<crate::provider::ProviderError>, HarnessError> {
        let kind = crate::session::ModelFailureKind::Error;
        let failure = self
            .record_model_failure(turn.agent, attempt, usage, error.to_string(), kind)
            .await?;
        if !error.is_retryable() {
            return Ok(Some(error));
        }
        if turn.cancellation.is_cancelled() {
            return Err(HarnessError::Interrupted);
        }
        *transient = transient.saturating_add(1);
        let delay = recovery_delay(&error, *transient);
        let scheduled = SessionEvent::ModelRecoveryScheduled {
            failure,
            delay_millis: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        };
        self.store.append(turn.agent.clone(), scheduled).await?;
        tokio::select! {
            () = turn.cancellation.cancelled() => Err(HarnessError::Interrupted),
            () = tokio::time::sleep(delay) => Ok(None),
        }
    }

    /// Journal a failed attempt with its observed usage; returns the failure's
    /// sequence. A refusal is journaled like any other failure; its kind keeps it
    /// out of automatic recovery.
    pub(super) async fn record_model_failure(
        &self,
        agent: &AgentId,
        attempt: crate::session::AttemptRef,
        usage: Usage,
        error: String,
        kind: crate::session::ModelFailureKind,
    ) -> Result<RecordSeq, HarnessError> {
        // Observed usage and the attempt's outcome commit together.
        let mut events = Vec::new();
        if usage != Usage::default() {
            let request = attempt.request;
            events.push((agent.clone(), SessionEvent::Usage { request, usage }));
        }
        let failed = SessionEvent::ModelFailed {
            attempt,
            error,
            kind,
        };
        events.push((agent.clone(), failed));
        let records = self.store.append_all(events).await?;
        self.usage.lock().await.accumulate(usage);
        Ok(records.last().expect("one record per event").sequence)
    }

    /// Close an attempt cancelled before it produced an outcome.
    pub(super) async fn record_attempt_interrupted(
        &self,
        agent: &AgentId,
        attempt: crate::session::AttemptRef,
    ) -> Result<(), HarnessError> {
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelAttemptInterrupted(attempt),
            )
            .await?;
        Ok(())
    }

    pub(super) async fn record_model_usage(
        &self,
        agent: &AgentId,
        request: RequestSeq,
        usage: Usage,
    ) -> Result<(), HarnessError> {
        self.store
            .append(agent.clone(), SessionEvent::Usage { request, usage })
            .await?;
        self.usage.lock().await.accumulate(usage);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Exercise provider-neutral recovery through the real agent loop and durable journal.

    use std::sync::{Arc, atomic::Ordering};
    use std::time::Duration;

    use super::*;
    use crate::agent::runtime::tests::{
        Requests, Script, Sent, SentPart, Served, Step, bounded, child_launch, count, delta,
        enqueue_prompts, events, model_ref, owner, poll, rendered, response, shutdown_session,
        stream, summary_json, test_builder, test_harness, tool_call, usage,
    };
    use crate::provider::{
        Provider, ProviderContext, ProviderError, ProviderErrorKind, ResponseStream,
        protocol::{AssistantItem, ContextId, ResponseEvent},
    };
    use crate::session::{ModelPurpose, project_history};
    use futures_util::TryStreamExt;

    fn error(kind: ProviderErrorKind, message: &str) -> ProviderError {
        ProviderError {
            kind,
            message: message.into(),
        }
    }

    fn recoverable() -> ProviderError {
        error(ProviderErrorKind::Transport, "scripted connection lost")
    }

    /// A context overflow at invocation, or after streaming text as a Messages stop does.
    fn overflow(streaming: bool) -> Step {
        let error = error(
            ProviderErrorKind::ContextWindowExceeded,
            "scripted context overflow",
        );
        if streaming {
            let mut events = partial(&[AssistantItem::text("cut", 0, "DO NOT COMMIT")]);
            events.push(Err(error));
            Step::stream(events)
        } else {
            Step::fail(error)
        }
    }

    fn success(items: Vec<AssistantItem>) -> Step {
        Step::stream(response(items).into_iter().map(Ok).collect())
    }

    fn answer(text: &str) -> Step {
        success(vec![AssistantItem::text("answer", 0, text)])
    }

    fn write_call(id: &str, path: &str) -> AssistantItem {
        tool_call(0, id, "write", json!({"path":path, "content":id}))
    }

    fn write_step(id: &str, path: &str) -> Step {
        success(vec![write_call(id, path)])
    }

    /// Provisional deltas for `items`, as a stream that fails before its end shows.
    fn partial(items: &[AssistantItem]) -> Vec<Result<ResponseEvent, ProviderError>> {
        items
            .iter()
            .map(|item| {
                let text = match item {
                    AssistantItem::Text { .. } => item.text_content().unwrap(),
                    AssistantItem::Reasoning { .. } => item.reasoning_text().unwrap(),
                    AssistantItem::ToolCall { call, .. } => {
                        serde_json::Value::Object(call.arguments().clone()).to_string()
                    }
                };
                let block = format!("{}:0", item.id());
                Ok(delta(item.id().as_str(), &block, item.kind(), &text))
            })
            .collect()
    }

    struct Fixture {
        workspace: tempfile::TempDir,
        session: SessionHandle,
        script: Arc<Script>,
    }

    impl Fixture {
        async fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            let workspace = tempfile::tempdir().unwrap();
            // One shared script: a retry must not reopen agents or replay tools.
            let script = Script::new(steps, &Requests::default());
            let provider = script.clone();
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
        fn requests(&self) -> Vec<Served> {
            self.script.requests.lock().unwrap().clone()
        }

        async fn prompt(&self, text: &str) -> String {
            self.session.prompt(text).await.unwrap()
        }

        /// Root history as JSON, plus its number of assistant messages.
        fn history(&self, records: &[EventRecord]) -> (String, usize) {
            let history = project_history(records, &self.session.root).messages;
            let assistants = history
                .iter()
                .filter(|(_, message)| matches!(message, Message::Assistant(_)));
            let assistants = assistants.count();
            (serde_json::to_string(&history).unwrap(), assistants)
        }
    }

    /// Every scheduled recovery as (request, next attempt, delay, error) of its failure.
    fn recoveries(records: &[EventRecord]) -> Vec<(RequestSeq, u64, u64, String)> {
        records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ModelRecoveryScheduled {
                    failure,
                    delay_millis,
                } => {
                    let failed = records.iter().find(|record| record.sequence == *failure)?;
                    let SessionEvent::ModelFailed { attempt, error, .. } = &failed.event else {
                        panic!("recovery {} names no failure", record.sequence);
                    };
                    let next = attempt.attempt + 1;
                    Some((attempt.request, next, *delay_millis, error.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn assert_schedule(records: &[EventRecord], expected_attempts: &[u64]) {
        let scheduled = recoveries(records);
        let attempts = scheduled.iter().map(|entry| entry.1).collect::<Vec<_>>();
        assert_eq!(attempts, expected_attempts);
        let mut transient_attempts = std::collections::HashMap::<RequestSeq, u64>::new();
        for (request, _attempt, delay, error) in scheduled {
            let transient = transient_attempts.entry(request).or_default();
            *transient += 1;
            let expected = recovery_delay(&recoverable(), *transient).as_millis() as u64;
            assert_eq!(delay, expected);
            assert_eq!(error, recoverable().to_string());
            let requested = crate::session::record_at(records, request.into()).unwrap();
            let context = crate::session::request_context(requested, |sequence| {
                crate::session::record_at(records, sequence)
            });
            let purpose = context.expect("recovery names no request").purpose;
            assert_eq!(purpose, ModelPurpose::Agent);
            assert_ne!(
                count!(records, SessionEvent::ModelFailed { attempt, error: failure, .. } if attempt.request == request && failure == &error),
                0
            );
        }
    }

    async fn next_recovery(
        events: &mut broadcast::Receiver<crate::agent::ObservedEvent>,
    ) -> EventRecord {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let RuntimeEvent::Record(record) = events.recv().await.unwrap().event
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
            error.kind = ProviderErrorKind::Unavailable {
                retry_after: Some(hint),
            };
            assert_eq!(recovery_delay(&error, 1), hint);
            assert_eq!(recovery_delay(&error, u64::MAX), hint);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn startup_failure_retries_the_frozen_request_and_honors_server_retry_after() {
        let error = error(
            ProviderErrorKind::RateLimited {
                retry_after: Some(Duration::from_secs(90)),
            },
            "scripted throttle",
        );
        let fixture = Fixture::new([Step::fail(error.clone()), answer("recovered")]).await;
        let image = crate::media::Attachment::Image {
            file: Some(fixture.workspace.path().join("evidence.png")),
            image: crate::tests::png(b"request image fixture"),
        };
        let (images, options) = ([image], Selection::default());
        let started = tokio::time::Instant::now();
        let prompt = fixture
            .session
            .prompt_with_options("continue", &images, options);
        assert_eq!(prompt.await.unwrap(), "recovered");
        // A server hint above the fallback cap is honored and journaled.
        assert!(started.elapsed() >= Duration::from_secs(90));
        let records = fixture.records().await;
        let scheduled = recoveries(&records).into_iter();
        let scheduled = scheduled.map(|(_, attempt, delay, error)| (attempt, delay, error));
        let expected = (2, 90_000, error.to_string());
        assert_eq!(scheduled.collect::<Vec<_>>(), [expected]);
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        // The retry uses the frozen hydrated request on the same context.
        assert_eq!(requests[0], requests[1]);
        assert!(requests[0].messages().any(|message| matches!(message,
            Sent::User(blocks) if blocks.iter().any(|block| matches!(block,
                SentPart::Attachment { attachment: crate::media::AttachmentRef::Image(image) }
                    if requests[0].blobs.get(&image.blob).is_ok())))));
        assert_eq!(fixture.script.opened.load(Ordering::SeqCst), 1);
        let (history, assistants) = fixture.history(&records);
        assert_eq!(assistants, 1);
        assert!(!history.contains("scripted throttle"));
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn failed_partial_text_and_complete_tool_block_are_discarded_before_retry() {
        let observed_usage = usage(71, 13, 5);
        let mut partial = vec![Ok(ResponseEvent::Usage(observed_usage))];
        let discarded = json!({"path":"must-not-exist", "content":"bad"});
        let items = [
            AssistantItem::text("partial", 0, "DO NOT COMMIT"),
            tool_call(1, "discarded", "write", discarded),
        ];
        partial.extend(self::partial(&items));
        partial.push(Err(recoverable()));
        let committed = write_step("committed", "successful-tool");
        let fixture = Fixture::new([Step::stream(partial), committed, answer("done")]).await;
        assert_eq!(fixture.prompt("write once").await, "done");
        let workspace = fixture.workspace.path();
        assert!(!workspace.join("must-not-exist").exists());
        let written = std::fs::read_to_string(workspace.join("successful-tool")).unwrap();
        assert_eq!(written, "committed");
        let records = fixture.records().await;
        let failed_request = recoveries(&records)[0].0;
        let failed_usage = events!(&records, SessionEvent::Usage { request, usage } if *request == failed_request => *usage);
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
                .any(|message| matches!(message, Sent::Tool(_)))
        );
        assert_schedule(&records, &[2]);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn successful_tool_results_survive_recovery_and_connection_budget_resets() {
        let fixture = Fixture::new([
            Step::fail(recoverable()),
            Step::fail(recoverable()),
            write_step("prior-tool", "prior-tool"),
            Step::fail(recoverable()),
            Step::fail(recoverable()),
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
            let tool = |message: &&Sent| matches!(message, Sent::Tool(_));
            assert_eq!(request.messages().filter(tool).count(), 1);
        }
        let records = fixture.records().await;
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 1);
        let written = std::fs::read_to_string(fixture.workspace.path().join("prior-tool"));
        assert_eq!(written.unwrap(), "prior-tool");
        assert_schedule(&records, &[2, 3, 2, 3]);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_number_attempts_of_one_frozen_request() {
        // Past a u8, so the counters and their journal columns cannot be narrow.
        let failures = 260;
        let steps = (0..failures).map(|_| Step::fail(recoverable()));
        let fixture = Fixture::new(steps.chain([answer("recovered")])).await;
        assert_eq!(fixture.prompt("recover").await, "recovered");
        let requests = fixture.requests();
        assert_eq!(requests.len(), failures + 1);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        let records = fixture.records().await;
        let requested = count!(&records, SessionEvent::ModelRequested { .. });
        assert_eq!(requested, 1, "one frozen logical request across retries");
        let started = events!(&records, SessionEvent::ModelAttemptStarted(attempt) => (attempt.request, attempt.attempt));
        let request = started[0].0;
        let attempts = (1..=failures as u64 + 1).map(|attempt| (request, attempt));
        assert_eq!(started, attempts.collect::<Vec<_>>());
        let at_request = records
            .iter()
            .filter(|record| record.sequence == RecordSeq::from(request));
        assert_eq!(count!(at_request, SessionEvent::ModelRequested { .. }), 1);
        assert_schedule(&records, &(2..=failures as u64 + 1).collect::<Vec<_>>());
        assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), failures);
        let (sessions, id) = (
            fixture.workspace.path().join("sessions"),
            fixture.session.id(),
        );
        shutdown_session(fixture.session).await;
        let reopened = SessionStore::read_records(&sessions, id).await.unwrap();
        assert_eq!(reopened[..records.len()], records);
    }

    #[tokio::test(start_paused = true)]
    async fn interrupt_during_backoff_prevents_the_next_invocation() {
        let steps = (0..4).map(|_| Step::fail(recoverable()));
        let fixture = Fixture::new(steps.chain([answer("must not run")])).await;
        let mut events = fixture.session.runtime.events.observe().updates;
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
        assert_eq!(fixture.script.remaining(), 1);
        fixture.session.shutdown().await.unwrap();
        assert_eq!(fixture.requests().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn all_transient_categories_retry_past_three_startup_or_stream_failures() {
        use ProviderErrorKind::{CredentialExpired, RateLimited, Timeout, Transport, Unavailable};
        for kind in [
            CredentialExpired,
            Unavailable { retry_after: None },
            Transport,
            Timeout,
            RateLimited { retry_after: None },
        ] {
            for streaming in [false, true] {
                let failures = 4;
                let failure = || {
                    let error = error(kind, "provider stream error");
                    if !streaming {
                        return Step::fail(error);
                    }
                    let items = [
                        AssistantItem::text("partial", 1, "discard failed partial answer"),
                        write_call("failed", "must-not-exist"),
                    ];
                    let mut events = partial(&items);
                    events.push(Err(error));
                    Step::stream(events)
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
                assert!(!fixture.workspace.path().join("must-not-exist").exists());
                let records = fixture.records().await;
                let scheduled = recoveries(&records);
                assert_eq!(scheduled.len(), failures);
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
                    Step::stream(vec![Err(error)])
                } else {
                    Step::fail(error)
                };
                let fixture = Fixture::new([failure, answer("must not run")]).await;
                let result = fixture.session.prompt("fail without recovery").await;
                assert!(result.is_err(), "{kind:?}");
                assert_eq!(fixture.requests().len(), 1, "{kind:?}");
                assert!(recoveries(&fixture.records().await).is_empty(), "{kind:?}");
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn child_recovery_keeps_the_same_owner_job_and_does_not_fail_the_agent() {
        let fixture = Fixture::new([Step::fail(recoverable()), answer("child recovered")]).await;
        let session = &fixture.session;
        let child = session.root.child(1);
        let owner = owner(session).await;
        let launch = child_launch(session, child.clone(), Some(owner));
        let sender = session.runtime.spawn_agent(launch).await.unwrap();
        let mut events = session.runtime.events.observe().updates;
        let (done, received) = oneshot::channel();
        let content = vec![UserPart::Text {
            text: "recover".into(),
        }];
        let input = AgentCommand::Input {
            options: Default::default(),
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
                !matches!(event.event, RuntimeEvent::Activity { agent, activity: AgentActivity::Stopped(_) } if agent == child)
            );
        }
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].context, ContextId::from(&child));
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
        let steps = [Step::fail(recoverable()), answer("first"), answer("queued")];
        let fixture = Fixture::new(steps).await;
        let mut events = fixture.session.runtime.events.observe().updates;
        let session = fixture.session.clone();
        let turn = tokio::spawn(async move { session.prompt("initial input").await });
        next_recovery(&mut events).await;
        let session = fixture.session.clone();
        let token_cancel = QueuedPromptCancellation::default();
        let prompt = QueuedPrompt {
            text: "queued input".into(),
            cancellation: token_cancel.clone(),
            ..Default::default()
        };
        let receipt =
            tokio::spawn(
                async move { enqueue_prompts(&session, vec![prompt]).await.pop().unwrap() },
            );
        // The queued input reaches the mailbox during backoff.
        bounded(async {
            while fixture.session.root_tx.capacity() == AGENT_CHANNEL_CAPACITY {
                poll().await;
            }
        })
        .await;
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
        let queued = |index: usize| serde_json::to_string(&requests[index].request).unwrap();
        assert!(queued(2).contains("queued input") && !queued(1).contains("queued input"));
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn transient_retries_do_not_exhaust_context_compaction_budget() {
        let mut summary = summary_json();
        summary["objective"] = json!("preserve context ".repeat(100));
        let fixture = Fixture::new((0..4).map(|_| Step::fail(recoverable())).chain([
            overflow(false),
            answer(&summary.to_string()),
            Step::fail(recoverable()),
            answer("recovered after compaction"),
        ]))
        .await;
        // Exceed the 8,000-token verbatim tail so old work is summarized, not retained.
        let history = "original context ".repeat(5000);
        let history = Message::Assistant(vec![AssistantItem::text("history", 0, history)]);
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
        assert_eq!(fixture.script.remaining(), 0);
        let records = fixture.records().await;
        assert_schedule(&records, &[2, 3, 4, 5, 7]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn context_overflows_compact_even_after_streamed_output_up_to_three_failures() {
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
            let requests = fixture.requests();
            assert_eq!(
                requests.len(),
                5,
                "three context failures and two summaries"
            );
            assert_eq!(fixture.script.remaining(), 1);
            let records = fixture.records().await;
            assert!(recoveries(&records).is_empty());
            assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 3);
            let summaries = requests
                .iter()
                .filter(|sent| sent.response_schema.is_some());
            assert_eq!(summaries.count(), 2);
            // Text streamed before an overflow is discarded with the attempt.
            assert!(
                requests
                    .iter()
                    .all(|sent| !rendered(sent).contains("DO NOT"))
            );
            assert_eq!(fixture.history(&records).1, 0);
            fixture.session.shutdown().await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_provider_call_is_journaled_before_invocation() {
        struct FailingProvider {
            session_root: PathBuf,
        }
        struct FailingContext {
            session_root: PathBuf,
            context: ContextId,
        }
        impl Provider for FailingProvider {
            fn open_context(
                &self,
                context: ContextId,
            ) -> Result<Box<dyn ProviderContext>, ProviderError> {
                Ok(Box::new(FailingContext {
                    session_root: self.session_root.clone(),
                    context,
                }))
            }
        }
        impl ProviderContext for FailingContext {
            fn invoke(&mut self, request: ModelRequest) -> ResponseStream {
                let session = self.context.as_str().split(':').next().unwrap();
                let session = session.parse().unwrap();
                let root = self.session_root.clone();
                let started = async move {
                    // A separate reader sees only committed transactions.
                    let records = SessionStore::read_records(&root, session).await.unwrap();
                    let SessionEvent::ModelAttemptStarted(crate::session::AttemptRef {
                        request: sequence,
                        attempt: 1,
                    }) = records.last().unwrap().event
                    else {
                        panic!("attempt start must be durable before invocation");
                    };
                    let reconstructed =
                        crate::session::reconstruct_model_request(&records, sequence);
                    assert_eq!(reconstructed.unwrap(), (model_ref("test"), request));
                    Err::<ResponseStream, _>(ProviderError::protocol(
                        "intentional provider failure",
                    ))
                };
                Box::pin(stream::once(started).try_flatten())
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

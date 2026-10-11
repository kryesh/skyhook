//! Transient model failure recovery and usage accounting.

use super::*;
use crate::session::AttemptFailure;

/// The fallback delay before a first retry, doubled per failure up to the cap.
const BACKOFF_BASE: std::time::Duration = std::time::Duration::from_secs(1);
const BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// Server hints take precedence over the exponential fallback, including hints
/// longer than its cap. `attempt` counts transient failures of the frozen
/// request, independently of context/validation attempts.
fn recovery_delay(error: &crate::provider::ProviderError, attempt: u64) -> std::time::Duration {
    error.retry_after().unwrap_or_else(|| {
        // Five doublings pass the cap; more would only overflow.
        (BACKOFF_BASE * (1 << attempt.saturating_sub(1).min(5))).min(BACKOFF_CAP)
    })
}

/// How an attempt ended without a response.
pub(super) enum AttemptEnd {
    Interrupted,
    Failed(AttemptFailure),
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
        if !error.is_retryable() {
            let failed = AttemptFailure::Final((&error).into());
            (self.record_attempt(turn.agent, attempt, usage, AttemptEnd::Failed(failed))).await?;
            return Ok(Some(error));
        }
        *transient = transient.saturating_add(1);
        let delay = recovery_delay(&error, *transient);
        // The delay runs from the failure, however long its record takes to commit.
        let retry = tokio::time::sleep(delay);
        let retrying = AttemptFailure::Retrying {
            message: error.message.clone(),
            kind: error.kind(),
            delay: delay.into(),
        };
        (self.record_attempt(turn.agent, attempt, usage, AttemptEnd::Failed(retrying))).await?;
        tokio::select! {
            biased;
            () = turn.cancellation.cancelled() => Err(HarnessError::Interrupted),
            () = retry => Ok(None),
        }
    }

    /// Journal how an attempt ended together with its observed usage, so a crash
    /// never separates them.
    pub(super) async fn record_attempt(
        &self,
        agent: &AgentId,
        attempt: crate::session::AttemptRef,
        usage: Usage,
        end: AttemptEnd,
    ) -> Result<(), HarnessError> {
        let mut events: Vec<_> = (SessionEvent::usage(attempt.request, usage).into_iter())
            .map(|event| (agent.clone(), event))
            .collect();
        let outcome = match end {
            AttemptEnd::Interrupted => SessionEvent::ModelAttemptInterrupted(attempt),
            AttemptEnd::Failed(failure) => SessionEvent::ModelFailed { attempt, failure },
        };
        events.push((agent.clone(), outcome));
        self.store.append_all(events).await?;
        Ok(())
    }

    pub(super) async fn record_model_usage(
        &self,
        agent: &AgentId,
        request: RequestSeq,
        usage: Usage,
    ) -> Result<(), HarnessError> {
        if let Some(event) = SessionEvent::usage(request, usage) {
            self.store.append(agent.clone(), event).await?;
        }
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
        enqueue_prompts, events, journaled_usage, model_ref, next_recovery, owner, recoverable,
        rendered, response, retrying, send, shutdown_session, spawn_prompt, stream, summary_json,
        test_builder, test_harness, tool_call, until_queued, usage,
    };
    use crate::provider::{
        Provider, ProviderContext, ProviderError, ProviderErrorKind, ResponseStream,
        protocol::{AssistantItem, ContextId, ResponseEvent},
    };
    use crate::session::{ModelPurpose, project_history};
    use futures_util::TryStreamExt;

    /// A context overflow at invocation, or after streaming text as a Messages stop does.
    fn overflow(streaming: bool) -> Step {
        let error = ProviderErrorKind::ContextWindowExceeded.error("scripted context overflow");
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
                Ok(delta(item.id().as_str(), item.kind(), &text))
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

        fn records(&self) -> Vec<EventRecord> {
            self.session.runtime.store.records()
        }
        fn requests(&self) -> Vec<Arc<Served>> {
            self.script.requests.lock().unwrap().clone()
        }

        async fn prompt(&self, text: &str) -> String {
            retrying(&self.session, self.session.prompt(text))
                .await
                .unwrap()
        }

        /// Root history as JSON, plus its number of assistant messages.
        fn history(&self, records: &[EventRecord]) -> (String, usize) {
            let projection = project_history(records, &self.session.root);
            let history: Vec<_> = projection.history().map(|(message, _)| message).collect();
            let assistants = history
                .iter()
                .filter(|message| matches!(message, Message::Assistant(_)));
            let assistants = assistants.count();
            (serde_json::to_string(&history).unwrap(), assistants)
        }
    }

    /// Every retried failure as (request, next attempt, delay, error).
    fn recoveries(records: &[EventRecord]) -> Vec<(RequestSeq, u64, u64, Failure)> {
        records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ModelFailed {
                    attempt,
                    failure: failure @ AttemptFailure::Retrying { delay, .. },
                } => Some((
                    attempt.request,
                    attempt.attempt + 1,
                    delay.millis(),
                    failure.failure(),
                )),
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
            assert_eq!(error, Failure::from(&recoverable()));
            let requested = crate::session::record_at(records, request.into()).unwrap();
            let context = crate::session::request_context(requested, |sequence| {
                crate::session::record_at(records, sequence)
            });
            let purpose = context.expect("recovery names no request").purpose;
            assert_eq!(purpose, ModelPurpose::Agent);
        }
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
            error = ProviderErrorKind::Unavailable
                .error("")
                .with_retry_after(Some(hint));
            assert_eq!(recovery_delay(&error, 1), hint);
            assert_eq!(recovery_delay(&error, u64::MAX), hint);
        }
    }

    #[tokio::test]
    async fn startup_failure_retries_the_frozen_request_and_honors_server_retry_after() {
        let error = ProviderErrorKind::RateLimited
            .error("scripted throttle")
            .with_retry_after(Some(Duration::from_secs(90)));
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
        let answer = retrying(&fixture.session, prompt).await;
        assert_eq!(answer.unwrap(), "recovered");
        // A server hint above the fallback cap is honored and journaled.
        assert!(started.elapsed() >= Duration::from_secs(90));
        let records = fixture.records();
        let scheduled = recoveries(&records).into_iter();
        let scheduled = scheduled.map(|(_, attempt, delay, error)| (attempt, delay, error));
        let expected = (
            2,
            90_000,
            Failure::Provider("scripted throttle".into(), ProviderErrorKind::RateLimited),
        );
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

    #[tokio::test]
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
        let records = fixture.records();
        let failed_request = recoveries(&records)[0].0;
        let failed_usage = events!(&records, SessionEvent::Usage { request, usage } if *request == failed_request => *usage);
        // The failed attempt's usage is recorded once; the retry reported none.
        assert_eq!(failed_usage, [observed_usage]);
        assert_eq!(journaled_usage(&fixture.session).await, observed_usage);
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

    #[tokio::test]
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
        let records = fixture.records();
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 1);
        let written = std::fs::read_to_string(fixture.workspace.path().join("prior-tool"));
        assert_eq!(written.unwrap(), "prior-tool");
        assert_schedule(&records, &[2, 3, 2, 3]);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn transient_retries_number_attempts_of_one_frozen_request() {
        // Past a u8, so the counters and their journal columns cannot be narrow.
        let failures = 260;
        let steps = (0..failures).map(|_| Step::fail(recoverable()));
        let fixture = Fixture::new(steps.chain([answer("recovered")])).await;
        assert_eq!(fixture.prompt("recover").await, "recovered");
        let requests = fixture.requests();
        assert_eq!(requests.len(), failures + 1);
        assert!(requests.windows(2).all(|pair| pair[0] == pair[1]));
        let records = fixture.records();
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

    #[tokio::test]
    async fn interrupt_during_backoff_prevents_the_next_invocation() {
        // The failure backs off for longer than any test runs.
        let hour = Some(Duration::from_secs(3600));
        let failure = ProviderErrorKind::Unavailable
            .error("down")
            .with_retry_after(hour);
        let fixture = Fixture::new([Step::fail(failure), answer("must not run")]).await;
        let mut events = fixture.session.runtime.store.observe().updates;
        let prompt = spawn_prompt(&fixture.session, "recover then cancel");
        let retrying = next_recovery(&mut events).await;
        fixture.session.interrupt().await;
        assert!(bounded(prompt).await.unwrap().is_err());
        // The interrupted turn's end closes the request it left retrying.
        let SessionEvent::ModelFailed { attempt, .. } = retrying.event else {
            unreachable!("a recovery is a retried failure")
        };
        let ledger = fixture.session.observe().snapshot.ledger;
        let phase = &ledger.get(attempt.request).unwrap().phase;
        assert!(matches!(
            phase,
            crate::session::RequestPhase::Interrupted {
                attempt: Some(1),
                ..
            }
        ));
        assert_eq!(fixture.requests().len(), 1);
        assert_eq!(fixture.script.remaining(), 1);
        fixture.session.shutdown().await.unwrap();
        assert_eq!(fixture.requests().len(), 1);
    }

    #[tokio::test]
    async fn all_transient_categories_retry_past_three_startup_or_stream_failures() {
        use ProviderErrorKind::{CredentialExpired, RateLimited, Timeout, Transport, Unavailable};
        for kind in [
            CredentialExpired,
            Unavailable,
            Transport,
            Timeout,
            RateLimited,
        ] {
            for streaming in [false, true] {
                let failures = 4;
                let failure = || {
                    let error = kind.error("provider stream error");
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
                let records = fixture.records();
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

    #[tokio::test]
    async fn permanent_error_kinds_ignore_retry_sounding_messages() {
        use ProviderErrorKind::{Authentication, InvalidRequest, Protocol};
        for kind in [Authentication, Protocol, InvalidRequest] {
            for streaming in [false, true] {
                let message =
                    "websocket connection lost; retry this request; previous_response_not_found";
                let error = kind.error(message);
                let failure = if streaming {
                    Step::stream(vec![Err(error)])
                } else {
                    Step::fail(error)
                };
                let fixture = Fixture::new([failure, answer("must not run")]).await;
                let result = fixture.session.prompt("fail without recovery").await;
                assert!(result.is_err(), "{kind:?}");
                assert_eq!(fixture.requests().len(), 1, "{kind:?}");
                assert!(recoveries(&fixture.records()).is_empty(), "{kind:?}");
                fixture.session.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn child_recovery_keeps_the_same_owner_job_and_does_not_fail_the_agent() {
        let fixture = Fixture::new([Step::fail(recoverable()), answer("child recovered")]).await;
        let session = &fixture.session;
        let child = session.root.child(1);
        let owner = owner(session, false).await;
        let launch = child_launch(session, None);
        let spawned = session
            .runtime
            .spawn_agent(child.clone(), Some(owner), launch);
        spawned.await.unwrap();
        let mut events = session.runtime.store.observe().updates;
        let (done, received) = oneshot::channel();
        let content = vec![UserPart::Text {
            text: "recover".into(),
        }];
        let input = AgentCommand::Input {
            options: Default::default(),
            content,
            done: Some(RequestCompletion::Child(done)),
        };
        let received = retrying(session, async {
            send(session, &child, input).await;
            assert_eq!(next_recovery(&mut events).await.agent, child);
            let state = session.runtime.jobs.snapshot(owner).unwrap().state;
            assert_eq!(state, crate::job::JobState::Running);
            bounded(received).await
        });
        assert_eq!(received.await.unwrap().unwrap(), "child recovered");
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.event, RuntimeEvent::Activity { agent, activity } if agent == child && activity.state.is_retryable())
            );
        }
        let requests = fixture.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].context_id, ContextId::from(&child));
        let records = fixture.records();
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 1);
        let child_records = records.iter().filter(|record| record.agent == child);
        assert_eq!(count!(child_records, SessionEvent::AgentInterrupted), 0);
        let opened = fixture.script.opened.load(Ordering::SeqCst);
        assert_eq!(opened, 2, "root plus the original child context");
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn queued_input_is_deferred_until_the_frozen_request_recovers() {
        let backoff = Duration::from_secs(3600);
        let failure = ProviderErrorKind::Unavailable.error("down");
        let failure = Step::fail(failure.with_retry_after(Some(backoff)));
        let fixture = Fixture::new([failure, answer("first"), answer("queued")]).await;
        let mut events = fixture.session.runtime.store.observe().updates;
        let turn = spawn_prompt(&fixture.session, "initial input");
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
        // The queued input reaches the inbox during backoff.
        until_queued(&fixture.session, &fixture.session.root).await;
        assert!(!token_cancel.is_claimed());
        assert!(!receipt.is_finished());
        assert_eq!(fixture.requests().len(), 1);
        let (history, _) = fixture.history(&fixture.records());
        assert!(!history.contains("queued input"));
        crate::tests::elapse(backoff).await;
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

    #[tokio::test]
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
        let records = fixture.records();
        assert_schedule(&records, &[2, 3, 4, 5, 7]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        fixture.session.shutdown().await.unwrap();
    }

    #[tokio::test]
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
            let records = fixture.records();
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

    #[tokio::test]
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
                    Err::<ResponseStream, _>(
                        ProviderErrorKind::Protocol.error("intentional provider failure"),
                    )
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

//! Provider turn execution and terminal response handling.

use super::*;
use crate::provider::{ProviderErrorKind, protocol::ResponseEvent};
use crate::session::RuntimeState;

impl SessionRuntime {
    pub(super) async fn run_turn(
        &self,
        mut turn: TurnContext<'_>,
        agent_context: &mut AgentContext,
        settings: &mut AgentSettings,
    ) -> Result<String, HarnessError> {
        let (agent, owner_job) = (turn.agent, turn.owner_job);
        let (cancellation, location) = (turn.cancellation, turn.location);
        let mut final_text = String::new();
        let mut provider_attempt = 0u64;
        let mut context_failures = 0u8;
        // The last request overflowed the context window.
        let mut overflowed = false;
        'requests: loop {
            self.flush_events(agent, cancellation).await?;
            // A held turn whose wait ended without `continue` ends here, its
            // results committed.
            if cancellation.is_cancelled() || !self.agents(|agents| agents.busy(agent)) {
                return Err(HarnessError::Interrupted);
            }
            if self
                .consume_queued_inputs(&mut turn, agent_context, settings)
                .await
            {
                provider_attempt = 0;
                context_failures = 0;
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let viewer = turn.diagnostic_viewer();
            self.deliver_events(agent, viewer, &[]).await?;
            agent_context.refresh(&self.store, agent)?;
            let profile = agent_context.profile.profile.clone();
            if !profile.supports_images && agent_context.contains_images() {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let context = agent_context.context_record(&self.store, agent).await?;
            let tail = agent_context.tail(|| self.runtime_state(&turn));
            // An overflow is the latest word on the context's size.
            let trigger = if overflowed {
                Some(crate::session::CompactionTrigger::Overflow)
            } else {
                agent_context.compaction_due()
            };
            if let Some(trigger) = trigger {
                overflowed = false;
                let request = agent_context.request(tail.as_ref());
                let provider = agent_context.provider.as_mut();
                let snapshot = &agent_context.profile;
                self.compact_history(&turn, provider, snapshot, &request, trigger)
                    .await?;
                // Consume input received during compaction before starting the
                // normal request, rather than delaying it by another request.
                continue 'requests;
            }
            // Persisted state joins the conversation ahead of the request, which later
            // requests then extend unchanged: signed reasoning is bound to it.
            let tail = match tail {
                Some(state)
                    if profile.state_mode == crate::provider::profile::StateMode::Persist =>
                {
                    self.commit(agent, state).await?;
                    agent_context.refresh(&self.store, agent)?;
                    None
                }
                tail => tail,
            };
            let mut request = agent_context.request(tail.as_ref());
            // The request is frozen across provider recovery; input and model changes
            // wait for the next request boundary.
            self.store
                .load_blobs(&mut request, &mut agent_context.blobs)
                .await?;
            let requested = SessionEvent::ModelRequested {
                context,
                checkpoint: agent_context.projected.checkpoint,
                through: agent_context.projected.through(),
                from: None,
                tail: tail.clone().into_iter().collect(),
                history_lifetime: request.history_lifetime,
            };
            let requested = self.request(agent, requested).await?;
            let mut transient_attempt = 0u64;
            let (requested, response) = 'attempts: loop {
                if cancellation.is_cancelled() {
                    return Err(HarnessError::Interrupted);
                }
                provider_attempt = provider_attempt.saturating_add(1);
                let attempt = crate::session::AttemptRef {
                    request: requested.sequence.request(),
                    attempt: provider_attempt,
                };
                self.store
                    .append(agent.clone(), SessionEvent::ModelAttemptStarted(attempt))
                    .await?;
                // The failed stream may own the connection, so it is dropped before
                // any recovery wait.
                let stream = agent_context.provider.invoke(request.clone());
                let streamed = self.consume_stream(&turn, attempt, stream, |event| {
                    self.store.post(RuntimeEvent::ResponseEvent {
                        agent: agent.clone(),
                        request: requested.sequence.request(),
                        event: event.clone(),
                    });
                });
                let streamed = streamed.await?;
                // Nothing from a failed attempt is committed or executed.
                let (completion, usage) = match streamed {
                    Ok(streamed) => streamed,
                    Err((error, usage)) => {
                        let attempt = (attempt, &mut transient_attempt);
                        let Some(error) = self
                            .recover_model_failure(&turn, attempt, usage, error)
                            .await?
                        else {
                            continue 'attempts;
                        };
                        // Streamed output is discarded with the attempt, so an overflow
                        // compacts wherever the stream stopped.
                        if error.kind() == crate::provider::ProviderErrorKind::ContextWindowExceeded
                        {
                            context_failures += 1;
                            if context_failures < compact::MAX_COMPACTION_ATTEMPTS {
                                overflowed = true;
                                continue 'requests;
                            }
                        }
                        return Err(error.into());
                    }
                };
                break ((requested, attempt), (completion, usage));
            };
            let (requested, attempt) = requested;
            let (completion, usage) = response;
            let outcome = completion.outcome();
            let text = visible_text(completion.items());
            let calls: Vec<ToolCall> = completion.calls().cloned().collect();
            // Empty text is not history, even beside an action: a service may add an
            // empty message only in its terminal output. Whitespace is text and is kept.
            let assistant = Message::Assistant(
                completion
                    .into_items()
                    .into_iter()
                    .filter(|item| {
                        !matches!(item, crate::provider::protocol::AssistantItem::Text { blocks, .. }
                            if blocks.iter().all(|block| block.text.is_empty()))
                    })
                    .collect(),
            );
            // A refusal is never history and never retried here: it is deterministic
            // for a given request. A content-free message would make every later
            // request unencodable. Either fails the turn with nothing committed.
            let completed = match crate::session::CompletedOutcome::try_from(outcome) {
                Err(crate::session::Refused) => Err(HarnessError::Refused(refusal_detail(&text))),
                Ok(_) if assistant.is_content_free() => {
                    Err(if outcome == Outcome::Cut(CutReason::MaxTokens) {
                        HarnessError::OutputLimit
                    } else {
                        HarnessError::EmptyResponse
                    })
                }
                Ok(completed) => Ok(completed),
            };
            let completed = match completed {
                Ok(completed) => completed,
                Err(error) => {
                    let failure = Failure::from(&error);
                    let failed = crate::session::AttemptFailure::Final(failure.clone());
                    let failed = recovery::AttemptEnd::Failed(failed);
                    self.record_attempt(agent, attempt, usage, failed).await?;
                    self.store.post(RuntimeEvent::ResponseSettled {
                        agent: agent.clone(),
                        request: requested.sequence.request(),
                        settlement: Settlement::Failed(failure.into()),
                    });
                    return Err(error);
                }
            };
            // The message, its usage and its outcome commit in one transaction, so a
            // crash never leaves a response without the attempt's outcome.
            let outcome = {
                let (agent, request) = (agent.clone(), requested.sequence.request());
                move |message: RecordSeq| {
                    let message = message.message();
                    let completed = SessionEvent::ResponseCompleted {
                        attempt,
                        message,
                        outcome: completed,
                    };
                    (SessionEvent::usage(request, usage).into_iter())
                        .chain([completed])
                        .map(|event| (agent.clone(), event))
                        .collect::<Vec<_>>()
                }
            };
            let ends = calls.is_empty();
            let (author, published) = (agent.clone(), text.clone());
            let commit = move |state: &CoreState| {
                // A response whose stream outlived an interrupt is discarded.
                if !state.agents.busy(&author) {
                    return Err(HarnessError::Interrupted);
                }
                let turn = author.clone();
                let plan = match owner_job {
                    Some(job) => {
                        let (child, text) = (author, &published);
                        crate::job::Jobs::child_message(
                            state, child, job, assistant, text, outcome,
                        )?
                    }
                    None => {
                        let message = SessionEvent::MessageCommitted { message: assistant };
                        let mut entries = vec![(author, message)];
                        entries.extend(outcome(state.next_sequence()));
                        Plan::new(entries, |_, records| records[0].sequence.message())
                    }
                };
                Ok(plan.then(move |state| {
                    if ends {
                        state.agents.answered(&turn);
                    }
                }))
            };
            let origin = match self.store.commit(Command::Journal, commit).await {
                Err(HarnessError::Interrupted) => {
                    let interrupted = recovery::AttemptEnd::Interrupted;
                    self.record_attempt(agent, attempt, usage, interrupted)
                        .await?;
                    return Err(HarnessError::Interrupted);
                }
                committed => committed?,
            };
            // A child's earlier replies are published at commit; only the eventual
            // answer is its job's result.
            if owner_job.is_none() || calls.is_empty() {
                final_text.push_str(&text);
            }
            self.store.post(RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request: requested.sequence.request(),
                settlement: Settlement::Committed(origin),
            });
            let next_tail = agent_context.tail(|| self.runtime_state(&turn));
            // Decided from the committed response's reported occupancy, which excludes
            // newly produced tool results and queued input. An answer ends the turn's
            // model work: what it would compact waits for the next request boundary.
            agent_context.refresh(&self.store, agent)?;
            if !ends && let Some(trigger) = agent_context.compaction_due() {
                let current = agent_context.request(next_tail.as_ref());
                let compacted = async {
                    let provider = agent_context.provider.as_mut();
                    let snapshot = &agent_context.profile;
                    self.compact_history(&turn, provider, snapshot, &current, trigger)
                        .await?;
                    agent_context.refresh(&self.store, agent)?;
                    if cancellation.is_cancelled() {
                        return Err(HarnessError::Interrupted);
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = &compacted {
                    // The response is durable, but none of its calls ran. Close the
                    // exchange so a later request never sends unanswered calls.
                    let error = format!("not executed: the turn ended during compaction: {error}");
                    let results = calls
                        .iter()
                        .map(|call| {
                            let (id, name) = (call.id().to_owned(), call.name().to_owned());
                            dispatch::unrun_tool_result(id, name, error.clone())
                        })
                        .collect();
                    self.commit(agent, Message::Tool(results)).await?;
                }
                compacted?;
            }
            if !calls.is_empty() {
                // Every call's job exists, in call order, before any runs: a `wait`
                // in this response must see its sibling calls as outstanding work.
                let mut created = Vec::with_capacity(calls.len());
                for call in &calls {
                    created.push(if agent_context.unavailable_tools.contains(call.name()) {
                        // A pinned tool the live registry no longer provides as journaled.
                        let error =
                            format!("tool `{}` is unavailable in this session", call.name());
                        let (id, name) = (call.id().to_owned(), call.name().to_owned());
                        CreatedCall::Settled(dispatch::unrun_tool_result(id, name, error))
                    } else {
                        self.create_call(
                            agent,
                            owner_job,
                            call,
                            origin,
                            location,
                            &turn.capabilities,
                        )
                        .await
                    });
                }
                // A call that ended without a job ends it for a `wait` beside it.
                if created
                    .iter()
                    .any(|call| matches!(call, CreatedCall::Settled(_)))
                {
                    self.jobs.settled_jobless(agent, origin);
                }
                // Each result commits as its call finishes, so a crash keeps every
                // completed result; history merges them back into call order.
                let mut calls: futures_util::stream::FuturesUnordered<_> =
                    created.into_iter().map(CreatedCall::run).collect();
                // Drain every call even after a failed commit; results that could not
                // commit are settled as interrupted when the session resumes.
                let mut committed = Ok(());
                while let Some((result, acknowledges)) =
                    futures_util::StreamExt::next(&mut calls).await
                {
                    if committed.is_ok() {
                        committed = self.commit_result(agent, result, acknowledges).await;
                    }
                }
                committed?;
            }
            provider_attempt = 0;
            context_failures = 0;
            // A response without tools is a request boundary too: what waits for
            // the turn then precedes a stale answer.
            if calls.is_empty() && self.rejoin(agent).await {
                // Events that arrived during the request precede the answer.
                let viewer = turn.diagnostic_viewer();
                let delivered = self.deliver_events(agent, viewer, &[]).await?.is_some();
                if delivered
                    || self
                        .consume_queued_inputs(&mut turn, agent_context, settings)
                        .await
                {
                    final_text.clear();
                    // The invocation continues, so the silent answer needs its wake now.
                    if let Some(job) = owner_job {
                        self.jobs.notify_owner(job).await?;
                    }
                    continue 'requests;
                }
            }
            if calls.is_empty() {
                self.store.post(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                });
                return Ok(final_text);
            }
        }
    }
}

/// How an attempt's stream ended by itself; a failure keeps the usage observed before it.
pub(super) type Streamed =
    Result<(crate::provider::protocol::Completion, Usage), (ProviderError, Usage)>;

impl SessionRuntime {
    fn runtime_state(&self, turn: &TurnContext<'_>) -> RuntimeState {
        let (agent, capabilities) = (turn.agent, &turn.capabilities);
        state::runtime_state_content(&self.jobs, &self.store, agent, capabilities, turn.location)
    }

    /// Stream one attempt to its end, handing each event to `observe`. Cancellation
    /// journals the observed usage with the attempt's interruption. An invalid usage
    /// snapshot ends the response before it is observed or accumulated; the attempt
    /// keeps the last valid one.
    pub(super) async fn consume_stream(
        &self,
        turn: &TurnContext<'_>,
        attempt: crate::session::AttemptRef,
        mut stream: crate::provider::ResponseStream,
        mut observe: impl FnMut(&ResponseEvent),
    ) -> Result<Streamed, HarnessError> {
        let mut live = LiveResponse::default();
        loop {
            // An interrupt that lands before a poll wins even over a ready stream.
            let event = tokio::select! {
                biased;
                () = turn.cancellation.cancelled() => {
                    self.record_attempt(turn.agent, attempt, live.usage(), recovery::AttemptEnd::Interrupted).await?;
                    return Err(HarnessError::Interrupted);
                },
                event = stream.next() => event,
            };
            let checked = |event: ResponseEvent| event.checked().map_err(ProviderError::from);
            let event = match event.map(|event| event.and_then(checked)) {
                Some(Ok(event)) => event,
                Some(Err(error)) => return Ok(Err((error, live.usage()))),
                None => {
                    let error =
                        ProviderErrorKind::Protocol.error("stream ended without completion");
                    return Ok(Err((error, live.usage())));
                }
            };
            observe(&event);
            match live.push(event) {
                LiveStep::Open(open) => live = open,
                LiveStep::Ended {
                    completion, usage, ..
                } => return Ok(Ok((completion, usage))),
            }
        }
    }
}

/// Bounded excerpt of any partial text a refused response produced.
const REFUSAL_EXCERPT_LIMIT: usize = 240;

/// Human-readable refusal detail for the journal and for a parent agent. Providers
/// rarely return refusal prose, so partial visible text is preserved when present.
pub(super) fn refusal_detail(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "content filter; the response contained no content".to_owned();
    }
    // This string is journaled twice and reaches a parent agent's context, so the
    // excerpt is bounded. The full partial text remains in the response events.
    let excerpt: String = text.chars().take(REFUSAL_EXCERPT_LIMIT).collect();
    if excerpt.chars().count() < text.chars().count() {
        format!("content filter; partial response: {excerpt}…")
    } else {
        format!("content filter; partial response: {excerpt}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;
    use crate::session::AttemptFailure;

    /// A decoded response that ends in a refusal, as a content filter produces.
    fn refusal(items: Vec<AssistantItem>) -> Vec<ResponseEvent> {
        cut(items, CutReason::Refusal)
    }

    #[tokio::test]
    async fn unusable_responses_fail_the_turn_without_committing_or_retrying() {
        // A stream that closes before its authoritative end.
        let incomplete = vec![delta("answer", ItemKind::Text, "must not persist")];
        let text = |text| AssistantItem::text("answer", 0, text);
        let cases = [
            // A refusing provider streams nothing, or only an unusable reasoning stub.
            (refusal(Vec::new()), "content filter", true),
            // The journal keeps the refusal's detail.
            (
                refusal(vec![AssistantItem::reasoning("thought", 0, "", None)]),
                "the response contained no content",
                true,
            ),
            // A cut keeps no tool calls, which can leave nothing at all.
            (
                cut(Vec::new(), CutReason::MaxTokens),
                "output limit was reached",
                false,
            ),
            // A turn stands on nonblank text or a tool call: nothing, empty or
            // whitespace text, or reasoning alone says nothing and takes no action.
            (response(Vec::new()), "no assistant content", false),
            (response(vec![text("")]), "no assistant content", false),
            (response(vec![text("   ")]), "no assistant content", false),
            (
                response(vec![AssistantItem::reasoning(
                    "thought", 0, "private", None,
                )]),
                "no assistant content",
                false,
            ),
            (incomplete, "stream ended without completion", false),
        ];
        for (events, expected, refused) in cases {
            // The refusal is durable: it must survive a reopen to be continued.
            let (root, requests) = (tempfile::tempdir().unwrap(), Requests::default());
            let provider = scripted_provider(&requests, [events, answer("must not be requested")]);
            let sessions = root.path().join("sessions");
            let harness = test_harness(root.path(), &sessions, provider).await;
            let durable = expected == "content filter";
            let session = match durable {
                true => harness.new_session().await.unwrap(),
                false => ephemeral_session(&harness).await,
            };
            let failure = session.prompt("hello").await.unwrap_err().to_string();
            assert!(failure.contains(expected), "unexpected failure: {failure}");
            assert_eq!(requests.lock().unwrap().len(), 1, "{expected}: retried");
            let records = session.runtime.store.records();
            // An error state, never conversation history or an executed tool.
            assert_eq!(assistant_commits(&records), 0);
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
            let failed = events!(&records, SessionEvent::ModelFailed {
                failure: AttemptFailure::Final(failure), ..
            } => failure.clone());
            assert_eq!(failed.len(), 1);
            assert_eq!(matches!(failed[0], Failure::Refused(_)), refused);
            assert!(failed[0].to_string().contains(expected), "{failed:?}");
            let agent = events!(&records, SessionEvent::AgentFailed { failure } => failure.clone());
            assert_eq!(agent, failed);
            assert_eq!(
                count!(&records, SessionEvent::ModelAttemptStarted { .. }),
                1
            );
            let id = session.id();
            shutdown_session(session).await;
            if durable {
                let reopened = SessionStore::read_records(&sessions, id).await.unwrap();
                assert_eq!(reopened[..records.len()], records);
            }
        }
    }

    #[tokio::test]
    async fn child_refusal_reports_a_failed_agent_to_its_parent() {
        let work = json!({"prompt":"work"});
        let (_root, _requests, session) = scripted_session([
            response(vec![tool_call(0, "delegate", "agent", work)]),
            // The child refuses; its owner job must fail with that reason.
            refusal(Vec::new()),
            answer("child could not proceed"),
        ])
        .await;
        assert_eq!(
            session.prompt("delegate").await.unwrap(),
            "child could not proceed"
        );
        let records = session.runtime.store.records();
        // The parent sees a failed agent carrying the refusal message.
        let results = events!(
            &records,
            SessionEvent::MessageCommitted {
                message: Message::Tool(results)
            } => results.clone()
        );
        let reported = results
            .into_iter()
            .flatten()
            .find(|result| result.name == "agent")
            .expect("the parent received an agent tool result");
        assert!(reported.is_error, "the agent tool result must be an error");
        let rendered = reported.result.to_string();
        assert!(
            rendered.contains("declined to respond") && rendered.contains("content filter"),
            "parent-visible result lost the refusal reason: {rendered}"
        );
        // The child is journaled as refused and failed, not merely idle.
        let child = records
            .iter()
            .map(|record| record.agent.clone())
            .find(|agent| !agent.path().is_empty())
            .expect("a child agent was started");
        let child_records: Vec<_> = records
            .iter()
            .filter(|record| record.agent == child)
            .cloned()
            .collect();
        assert_eq!(
            count!(
                &child_records,
                SessionEvent::ModelFailed {
                    failure: AttemptFailure::Final(Failure::Refused(_)),
                    ..
                }
            ),
            1
        );
        assert_eq!(count!(&child_records, SessionEvent::AgentFailed { .. }), 1);
        assert_eq!(assistant_commits(&child_records), 0);
    }

    #[tokio::test]
    async fn whitespace_text_is_answered_committed_and_replayed() {
        let items = vec![
            AssistantItem::text("first", 0, "alpha"),
            AssistantItem::text("space", 1, " "),
            AssistantItem::text("second", 2, "beta"),
        ];
        let (_root, requests, session) =
            scripted_session([response(items.clone()), answer("done")]).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "alpha beta");
        let records = session.runtime.store.records();
        let committed = events!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(content) } => content.clone());
        assert_eq!(committed, vec![items.clone()]);
        assert_eq!(session.prompt("again").await.unwrap(), "done");
        let captured = requests.lock().unwrap();
        assert!(captured[1].messages().any(|message| matches!(message,
            Sent::Assistant(sent) if *sent == items)));
    }

    #[tokio::test]
    async fn empty_text_beside_a_call_is_not_committed() {
        let call = tool_call(1, "call", "missing", serde_json::json!({}));
        let first = response(vec![AssistantItem::text("blank", 0, ""), call.clone()]);
        let (_root, requests, session) = scripted_session([first, answer("done")]).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "done");
        let captured = requests.lock().unwrap();
        assert!(captured[1].messages().any(|message| matches!(message,
            Sent::Assistant(items) if *items == vec![call.clone()])));
    }

    /// Turns under unchanged settings share one context record, which keeps their
    /// reported prompts comparable for retention.
    #[tokio::test]
    async fn turns_with_unchanged_settings_share_a_context_record() {
        let (_root, _requests, session) = scripted_session([answer("one"), answer("two")]).await;
        session.prompt("first").await.unwrap();
        session.prompt("second").await.unwrap();
        let records = session.runtime.store.records();
        let contexts = events!(&records, SessionEvent::ModelContext { .. } => ());
        let requests = events!(&records, SessionEvent::ModelRequested { context, .. } => *context);
        assert_eq!((contexts.len(), requests.len()), (1, 2));
        assert_eq!(requests[0], requests[1]);
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn successful_responses_journal_their_outcome() {
        let (_root, _requests, session) = scripted_session([answer("done")]).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "done");
        let records = session.runtime.store.records();
        let outcomes = events!(
            &records,
            SessionEvent::ResponseCompleted { outcome, .. } => *outcome
        );
        assert_eq!(outcomes, vec![crate::session::CompletedOutcome::Answer]);
    }

    // End-to-end automatic compaction uses completed provider usage.

    use crate::session::ModelPurpose;

    fn with_usage(mut events: Vec<ResponseEvent>, snapshots: &[Usage]) -> Vec<ResponseEvent> {
        let end = events.pop().expect("response has a terminal event");
        assert!(matches!(end, ResponseEvent::End(_)));
        for &usage in snapshots {
            events.push(ResponseEvent::Usage(usage));
        }
        events.push(end);
        events
    }

    // First whole token at 90% of (128,000 - 16,384). Each component is
    // necessary: neither input alone nor input plus cache reaches the threshold.
    const THRESHOLD: (u64, u64, u64) = (80_000, 20_000, 455);

    fn threshold_usage() -> Usage {
        usage(THRESHOLD.0, THRESHOLD.1, THRESHOLD.2)
    }

    async fn seed_history(session: &SessionHandle, repetitions: usize) {
        let research = AssistantItem::text("old-history", 0, "research ".repeat(repetitions));
        let research = Message::Assistant(vec![research]);
        session
            .runtime
            .commit(&session.root, research)
            .await
            .unwrap();
    }

    fn purposes(records: &[EventRecord]) -> Vec<ModelPurpose> {
        records
            .iter()
            .filter_map(|record| purpose(records, record))
            .collect()
    }

    /// The purpose of a request record, from its context.
    fn purpose(records: &[EventRecord], record: &EventRecord) -> Option<ModelPurpose> {
        let context = crate::session::request_context(record, |sequence| {
            crate::session::record_at(records, sequence)
        });
        context.map(|context| context.purpose)
    }

    fn shell_response() -> Vec<ResponseEvent> {
        let command = "printf 'executed\\n' >> executions; printf retained-result";
        let arguments = json!({ "command": command });
        response(vec![tool_call(0, "append-once", "exec", arguments)])
    }

    #[tokio::test]
    async fn high_usage_tool_response_compacts_before_tools_and_delivers_results_to_next_request() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let script = Script::new(
            [
                // The summary carries the triggering response as read-only evidence.
                // Its input and output leave 1,000 tokens of the context available.
                Step::new(with_usage(
                    shell_response(),
                    &[usage(100_000, 20_000, 7_000)],
                )),
                Step::new(answer(summary_json().to_string())).gated(),
                Step::new(answer("original final")),
            ],
            &requests,
        );
        let harness =
            test_harness(root.path(), &root.path().join("sessions"), script.clone()).await;
        let session = harness.new_session().await.unwrap();
        // Enough old material to remove, but nowhere near the automatic threshold.
        seed_history(&session, 6_000).await;
        let (found, ()) = bounded(async {
            tokio::join!(session.prompt("Run the tool."), async {
                script.request(1).await;
                let records = session.runtime.store.records();
                assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
                assert_eq!(
                    count!(
                        &records,
                        SessionEvent::MessageCommitted {
                            message: Message::Tool(_)
                        }
                    ),
                    0
                );
                assert!(!root.path().join("executions").exists());
                script.release(1);
            })
        })
        .await;
        let found = found.unwrap();
        assert_eq!(found, "original final");
        let executions = fs::read_to_string(root.path().join("executions")).await;
        // compaction must not re-execute the tool
        assert_eq!(executions.unwrap(), "executed\n");
        let records = session.runtime.store.records();
        use ModelPurpose::{Agent, Compaction};
        assert_eq!(purposes(&records), vec![Agent, Compaction, Agent]);
        let position = |matches: &dyn Fn(&SessionEvent) -> bool| {
            records.iter().position(|r| matches(&r.event)).unwrap()
        };
        let tool = position(&|event| {
            matches!(event, SessionEvent::MessageCommitted { message: Message::Tool(results) }
        if results.iter().any(|result| result.call_id == "append-once"))
        });
        let SessionEvent::MessageCommitted {
            message: tool_message,
        } = &records[tool].event
        else {
            unreachable!()
        };
        let Message::Tool(results) = tool_message else {
            unreachable!()
        };
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error);
        assert!(results[0].result.to_string().contains("retained-result"));
        let summary = records
            .iter()
            .position(|record| purpose(&records, record) == Some(Compaction))
            .unwrap();
        let checkpoint = position(&|event| matches!(event, SessionEvent::Compaction { .. }));
        let next = records
            .iter()
            .rposition(|record| purpose(&records, record) == Some(Agent));
        assert!(summary < checkpoint && checkpoint < tool && tool < next.unwrap());
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].tools.is_empty());
            assert_eq!(
                captured[1].max_output_tokens,
                std::num::NonZeroU64::new(1_000)
            );
            assert_eq!(
                captured[0].max_output_tokens,
                std::num::NonZeroU64::new(16_384)
            );
            assert_eq!(captured[2].max_output_tokens, captured[0].max_output_tokens);
            assert!(captured[2].response_schema.is_none());
            assert_eq!(captured[0].tools, captured[2].tools);
            let sent_tool_message = sent(tool_message);
            assert!(
                !captured[1]
                    .messages()
                    .any(|message| message == &sent_tool_message)
            );
            assert!(!captured[1].messages().any(|message| matches!(message, Sent::Assistant(items)
                if items.iter().filter_map(|item| item.call()).any(|call| call.id() == "append-once"))));
            let index = captured[2]
                .messages()
                .position(|message| message == &sent_tool_message)
                .expect("the first post-compaction call receives the actual tool result");
            assert!(
                matches!(captured[2].messages().nth(index - 1), Some(Sent::Assistant(items))
                if items.iter().filter_map(|item| item.call()).any(|call| call.id() == "append-once"))
            );
            // The persisted summary boundary and the post-compaction call both replay exactly.
            let requested = records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }));
            for (record, sent) in requested.zip(captured.iter()) {
                let (_, replayed) =
                    crate::session::reconstruct_model_request(&records, record.sequence.request())
                        .unwrap();
                assert_eq!(&replayed, &sent.request);
            }
        }
        shutdown_session(session).await;
    }

    /// Only a todo replacement the triggering response issued directly is superseded;
    /// reads, replacements from its scripts and every other call run after compaction.
    #[tokio::test]
    async fn compaction_supersedes_only_direct_pending_todo_replacements() {
        use crate::agent::TodoStatus;

        let direct = vec![todo("Direct replacement", TodoStatus::Completed)];
        let scripted = vec![todo("Script replacement", TodoStatus::Pending)];
        let reconciled = vec![todo("Reconciled task", TodoStatus::InProgress)];
        let source = format!(
            "const before = await tool.todo({{}}); await tool.todo({{items:{}}}); return {{before, after: await tool.todo({{}})}};",
            serde_json::to_string(&scripted).unwrap()
        );
        let calls = vec![
            tool_call(0, "direct-write", "todo", json!({"items": direct})),
            tool_call(1, "direct-read", "todo", json!({})),
            tool_call(2, "scripted", "script", json!({"source": source})),
            tool_call(
                3,
                "append-once",
                "exec",
                json!({"command": "printf 'executed\\n' >> executions"}),
            ),
        ];
        let mut summary = summary_json();
        summary["todos"] = json!(reconciled);
        let (root, requests, session) = scripted_session([
            with_usage(response(calls), &[threshold_usage()]),
            answer(summary.to_string()),
            answer("done"),
        ])
        .await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Continue the task.").await.unwrap(), "done");
        let records = session.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 1);
        assert_eq!(
            events!(&records, SessionEvent::TodosReplaced { items } => items.clone()),
            vec![scripted.clone()]
        );
        assert_eq!(
            fs::read_to_string(root.path().join("executions"))
                .await
                .unwrap(),
            "executed\n"
        );
        {
            let captured = requests.lock().unwrap();
            let results: Vec<_> = captured[2]
                .messages()
                .filter_map(|message| match message {
                    Sent::Tool(results) => Some(results),
                    _ => None,
                })
                .flatten()
                .collect();
            for id in ["direct-write", "direct-read", "scripted", "append-once"] {
                let result = results.iter().find(|result| result.call_id == id).unwrap();
                let superseded = id == "direct-write";
                assert_eq!(result.is_error, superseded, "{id}: {}", result.result);
                if superseded {
                    assert!(result.result.to_string().contains("superseded"));
                }
            }
            let script = results
                .iter()
                .find(|result| result.call_id == "scripted")
                .unwrap();
            let value = &script.result["result"]["value"];
            assert_eq!(value["before"]["result"]["items"], json!(reconciled));
            assert_eq!(value["after"]["result"]["items"], json!(scripted));
        }
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn failed_compaction_settles_unexecuted_calls_before_a_live_retry() {
        let (root, requests, session) = scripted_session([
            with_usage(shell_response(), &[threshold_usage()]),
            refusal(Vec::new()),
            answer("retry done"),
        ])
        .await;
        seed_history(&session, 6_000).await;
        assert!(matches!(
            session.prompt("Run the tool.").await,
            Err(HarnessError::Refused(_))
        ));
        assert_calls_settled_unexecuted(root.path(), &requests, session, 0).await;
    }

    #[tokio::test]
    async fn interruption_during_compaction_persistence_settles_unexecuted_calls() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        // The summary reports usage, which commits before its checkpoint.
        let summary = with_usage(answer(summary_json().to_string()), &[usage(1_000, 0, 100)]);
        let script = Script::new(
            [
                Step::new(with_usage(shell_response(), &[threshold_usage()])),
                Step::new(summary).gated(),
                Step::new(answer("retry done")),
            ],
            &requests,
        );
        let harness =
            test_harness(root.path(), &root.path().join("sessions"), script.clone()).await;
        let session = harness.new_session().await.unwrap();
        seed_history(&session, 6_000).await;
        let (result, ()) = bounded(async {
            tokio::join!(session.prompt("Run the tool."), async {
                script.request(1).await;
                let store = &session.runtime.store;
                let (usage, resume_usage) = store.pause_committed(|_| true).await;
                script.release(1);
                usage.await.unwrap();
                // Queue the next gate before releasing usage persistence: it must
                // catch the checkpoint, after the summary has completed.
                let persistence = store.pause_committed(|_| true);
                tokio::pin!(persistence);
                assert!(futures_util::poll!(&mut persistence).is_pending());
                resume_usage.send(()).unwrap();
                let (persisting, resume) = persistence.await;
                persisting.await.unwrap();
                // Queued behind the checkpoint's commit.
                assert_eq!(interrupt_behind(&session, resume).await, 1);
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_calls_settled_unexecuted(root.path(), &requests, session, 1).await;
    }

    /// A turn interrupted once its response commits makes none of its calls.
    #[tokio::test]
    async fn interruption_after_a_response_commits_settles_unexecuted_calls() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let steps = [
            Step::new(shell_response()).gated(),
            Step::new(answer("retry done")),
        ];
        let script = Script::new(steps, &requests);
        let harness =
            test_harness(root.path(), &root.path().join("sessions"), script.clone()).await;
        let session = harness.new_session().await.unwrap();
        let (result, ()) = bounded(async {
            tokio::join!(session.prompt("Run the tool."), async {
                script.request(0).await;
                let store = &session.runtime.store;
                let (committing, resume) = store.pause_committed(|_| true).await;
                script.release(0);
                committing.await.unwrap();
                // Queued behind the response's commit, ahead of its calls.
                assert_eq!(interrupt_behind(&session, resume).await, 1);
            })
        })
        .await;
        assert!(matches!(result, Err(HarnessError::Interrupted)));
        assert_calls_settled_unexecuted(root.path(), &requests, session, 0).await;
    }

    /// An interrupt takes effect at the response's commit. A turn it held while
    /// its response still streamed has that response discarded, however the
    /// stream ends, and its attempt journaled interrupted. Applied behind the
    /// answer's commit, it finds nothing to stop, and the answer stands.
    #[tokio::test]
    async fn an_interrupt_stops_a_turn_until_its_answer_commits() {
        let steps = [
            Step::new(answer("answer")).midstream(),
            Step::new(Vec::new()).midstream(),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let prompt = spawn_prompt(&session, "task");
        provider.held(0).await;
        // A host call's child holds the streaming root.
        let (executor, owner) = (session.runtime.executor.clone(), session.root.clone());
        let arguments = json!({"prompt": "child task"});
        let host =
            tokio::spawn(async move { executor.execute(owner, "agent", arguments, None).await });
        provider.held(1).await;
        assert_eq!(session.interrupt().await, 2);
        assert_eq!(turn(&session, &session.root), Turn::Held);
        provider.release(0);
        let result = bounded(prompt).await.unwrap();
        assert!(
            matches!(result, Err(HarnessError::Interrupted)),
            "{result:?}"
        );
        let records = session.runtime.store.records();
        let root_records = records.iter().filter(|record| record.agent == session.root);
        let interrupted = count!(root_records, SessionEvent::ModelAttemptInterrupted(_));
        assert_eq!((assistant_commits(&records), interrupted), (0, 1));
        session.shutdown().await.unwrap();
        assert!(bounded(host).await.unwrap().is_err());

        let (_root, _requests, session) = scripted_session([answer("answer")]).await;
        let store = session.runtime.store.clone();
        let answered: fn(&SessionEvent) -> bool = |event| {
            matches!(
                event,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            )
        };
        let (reached, resume) = store.pause_committed(answered).await;
        let prompt = spawn_prompt(&session, "task");
        bounded(reached).await.unwrap();
        assert_eq!(interrupt_behind(&session, resume).await, 0);
        assert_eq!(bounded(prompt).await.unwrap().unwrap(), "answer");
        session.shutdown().await.unwrap();
    }

    /// The response's call never ran, its exchange is closed with an error result, and
    /// the next turn sends that result.
    async fn assert_calls_settled_unexecuted(
        root: &Path,
        requests: &Requests,
        session: SessionHandle,
        compactions: usize,
    ) {
        assert!(!root.join("executions").exists());
        let records = session.runtime.store.records();
        assert_eq!(
            count!(&records, SessionEvent::Compaction { .. }),
            compactions
        );
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        let results = events!(&records, SessionEvent::MessageCommitted { message: Message::Tool(results) } => results.clone());
        let [results] = results.as_slice() else {
            panic!("one settled exchange: {results:?}")
        };
        assert!(
            matches!(results.as_slice(), [result] if result.is_error && result.call_id == "append-once")
        );
        assert_eq!(session.prompt("Continue.").await.unwrap(), "retry done");
        let expected = sent(&Message::Tool(results.clone()));
        assert!(
            (requests.lock().unwrap().last().unwrap())
                .messages()
                .any(|message| message == &expected)
        );
        shutdown_session(session).await;
    }

    /// Every mode leaves the next request extending the previous one unchanged: a
    /// persisted state is committed, and a dynamic one is resent superseded before the
    /// response it produced.
    #[tokio::test]
    async fn state_mode_persists_supersedes_or_omits_runtime_state() {
        use crate::provider::{
            profile::StateMode,
            protocol::{Entry, StateLifetime::*},
        };
        let lifetimes = |request: &ModelRequest| -> Vec<_> {
            request.states().map(|state| state.lifetime).collect()
        };
        let modes = [
            (StateMode::None, [&[][..], &[]]),
            (
                StateMode::Persist,
                [&[Persisted][..], &[Persisted, Persisted]],
            ),
            (StateMode::Dynamic, [&[Current][..], &[Superseded, Current]]),
        ];
        for (mode, expected) in modes {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let inspect = response(vec![tool_call(0, "first", "todo", json!({}))]);
            let provider = scripted_provider(&requests, [inspect, answer("done")]);
            let mut profile = crate::tests::profile("test", false);
            profile.state_mode = mode;
            let harness = serving(root.path(), provider, [("test", profile)])
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            assert_eq!(session.prompt("Run the tool.").await.unwrap(), "done");
            let captured = requests.lock().unwrap().clone();
            let [first, second] = captured.as_slice() else {
                panic!("unexpected requests: {captured:?}")
            };
            assert_eq!(
                [lifetimes(first), lifetimes(second)],
                expected.map(<[_]>::to_vec)
            );
            let mut extended = first.context.clone();
            for entry in &mut extended {
                if let Entry::State(state) = entry
                    && state.lifetime == Current
                {
                    state.lifetime = Superseded;
                }
            }
            assert!(second.context.starts_with(&extended), "{mode:?}");
            let inspected = second
                .messages()
                .flat_map(|message| match message {
                    Sent::Tool(results) => results.as_slice(),
                    _ => &[],
                })
                .find(|result| result.call_id == "first")
                .expect("todo result in the next request");
            assert_eq!(inspected.name, "todo");
            assert!(!inspected.is_error, "todo failed: {inspected:?}");
            assert_eq!(inspected.result["result"], json!({"items": []}));
            shutdown_session(session).await;
        }
    }

    /// An answered turn goes on only for work the session lets it take: one that
    /// runs on in a session an interrupt paused leaves what became pending
    /// during its answer for the next request.
    #[tokio::test]
    async fn a_turn_running_on_while_paused_leaves_deliveries_at_its_answer() {
        let child = json!({"prompt": "child task", "depth": 0});
        let steps = [
            Step::new(response(vec![tool_call(0, "agent-0", "agent", child)])),
            Step::new(Vec::new()).midstream(),
            Step::new(answer("done")).gated(),
            Step::new(answer("again")),
        ];
        let provider = Script::new(steps, &Requests::default());
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let parent = spawn_prompt(&session, "delegate");
        provider.request(1).await;
        root_waiting(&session).await;
        assert_eq!(session.interrupt().await, 2);
        // Cancelling the held child lets the root run on, the session still paused.
        let job = session.runtime.jobs.list(&session.root)[0].id;
        session.runtime.jobs.cancel(job).await.unwrap();
        provider.request(2).await;
        let arguments = json!({"source": "return 1", "bg": true});
        let executor = &session.runtime.executor;
        let script = executor.execute(session.root.clone(), "script", arguments, None);
        let script = script.await.unwrap().job;
        bounded(session.runtime.jobs.wait_settled(script))
            .await
            .unwrap();
        provider.release(2);
        assert_eq!(bounded(parent).await.unwrap().unwrap(), "done");
        assert!(pending(&session, &session.root));
        assert!(!provider.requested_from(3));
        session.shutdown().await.unwrap();
    }

    /// A call's result commits with the acknowledgement of the outcome it
    /// reports. One that never commits, as when the process dies between them,
    /// leaves the outcome for delivery once the session reopens; one that
    /// commits is not reported again.
    #[tokio::test]
    async fn a_result_acknowledges_the_outcome_it_reports_as_it_commits() {
        for commits in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let requests = Requests::default();
            let shell = tool_call(0, "shell", "exec", json!({"command": "true"}));
            let steps = [response(vec![shell]), answer("done")];
            let provider = scripted_provider(&requests, steps);
            let harness = test_harness(root.path(), &sessions, provider).await;
            let session = harness.new_session().await.unwrap();
            let store = &session.runtime.store;
            let refused = (!commits)
                .then(|| crate::session::tests::refuse(store, "BEFORE INSERT ON tool_result"));
            assert_eq!(session.prompt("run").await.is_ok(), commits);
            drop(refused);
            let id = session.id();
            // A refused result stops the session, which reports it.
            assert_eq!(bounded(session.shutdown()).await.is_ok(), commits);
            assert_eq!(session.close().await.is_ok(), commits);
            let resumed = harness.resume_session(id).await.unwrap();
            resumed.running().await.unwrap();
            let receipt = resumed.runtime.jobs.pending_delivery(&resumed.root);
            let receipt = receipt.await.unwrap();
            let pending = (receipt.envelopes().iter()).map(|job| (job.tool.as_str(), job.state));
            let expected = (!commits).then_some(("exec", crate::job::JobState::Completed));
            assert_eq!(pending.collect::<Vec<_>>(), Vec::from_iter(expected));
            drop(receipt);
            shutdown_session(resumed).await;
        }
    }

    /// A `jobs` read is acknowledged with its own finish. A crash before the
    /// model's result commits leaves that finish unread, so reopening delivers
    /// the read once, in its outcome, and never the outcome it read again.
    #[tokio::test]
    async fn a_read_cut_off_from_its_result_is_delivered_once_on_reopen() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let read = tool_call(0, "read", "jobs", json!({"job": 1}));
        let provider = Script::new([Step::new(response(vec![read])).gated()], &requests);
        let harness = test_harness(root.path(), &sessions, provider.clone()).await;
        let session = harness.new_session().await.unwrap();
        let prompt = spawn_prompt(&session, "read it");
        provider.request(0).await;
        let arguments = json!({"source": "return 1", "bg": true});
        let executor = &session.runtime.executor;
        let script = executor.execute(session.root.clone(), "script", arguments, None);
        let script = script.await.unwrap().job;
        bounded(session.runtime.jobs.wait_settled(script))
            .await
            .unwrap();
        let store = &session.runtime.store;
        let refused = crate::session::tests::refuse(store, "BEFORE INSERT ON tool_result");
        provider.release(0);
        assert!(bounded(prompt).await.unwrap().is_err());
        drop(refused);
        let id = session.id();
        assert!(bounded(session.shutdown()).await.is_err());
        assert!(session.close().await.is_err());
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        let receipt = resumed.runtime.jobs.pending_delivery(&resumed.root);
        let receipt = receipt.await.unwrap();
        let pending = (receipt.envelopes().iter()).map(|job| job.tool.as_str());
        assert_eq!(pending.collect::<Vec<_>>(), ["jobs"]);
        drop(receipt);
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn only_compaction_summaries_end_their_history() {
        use crate::provider::protocol::HistoryLifetime::*;
        let shell = |id| response(vec![tool_call(0, id, "exec", json!({"command": "true"}))]);
        let (_root, _requests, session) = scripted_session([
            shell("first"),
            with_usage(shell("second"), &[threshold_usage()]),
            answer(summary_json().to_string()),
            answer("done"),
        ])
        .await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Run both tools.").await.unwrap(), "done");
        let records = session.runtime.store.records();
        let requests: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ModelRequested {
                    tail,
                    history_lifetime,
                    ..
                } => Some((
                    purpose(&records, record).unwrap(),
                    tail.as_slice(),
                    *history_lifetime,
                )),
                _ => None,
            })
            .collect();
        let [first, second, summary, after] = requests.as_slice() else {
            panic!("unexpected requests: {requests:?}")
        };
        assert_eq!((summary.0, summary.2), (ModelPurpose::Compaction, Detached));
        // The directive precedes the summary's own state, which ends the request.
        let [.., directive, Message::State(_)] = summary.1 else {
            panic!("unexpected summary tail: {:?}", summary.1)
        };
        assert_eq!(directive, &compaction::directive());
        for (purpose, tail, lifetime) in [first, second, after] {
            assert_eq!((*purpose, *lifetime), (ModelPurpose::Agent, Extends));
            assert!(matches!(tail, [Message::State(_)]));
        }
        shutdown_session(session).await;
    }

    /// A final answer over the threshold leaves busy in its own commit, so an
    /// interrupt behind it finds nothing to stop. The compaction it triggers is due
    /// from the journal: after a reopen, the next turn compacts before its first
    /// request, and an interrupt during that compaction leaves it due for `continue`.
    #[tokio::test]
    async fn high_usage_final_answer_stands_and_the_next_turn_compacts_first() {
        let final_answer = with_usage(answer("original final"), &[threshold_usage()]);
        let summary = answer(summary_json().to_string());
        let summary = with_usage(summary, &[usage(1_000_000, 0, 1)]);
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [final_answer, summary, answer("next")]);
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = harness.new_session().await.unwrap();
        seed_history(&session, 6_000).await;
        let answered: fn(&SessionEvent) -> bool = |event| {
            matches!(
                event,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            )
        };
        let (reached, resume) = session.runtime.store.pause_committed(answered).await;
        let prompt = spawn_prompt(&session, "Finish.");
        bounded(reached).await.unwrap();
        assert_eq!(interrupt_behind(&session, resume).await, 0);
        assert_eq!(bounded(prompt).await.unwrap().unwrap(), "original final");
        let records = session.runtime.store.records();
        assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);

        let id = session.id();
        shutdown_session(session).await;
        let session = harness.resume_session(id).await.unwrap();
        let compacting: fn(&SessionEvent) -> bool = |event| {
            matches!(event, SessionEvent::ModelContext { context }
                if context.purpose == ModelPurpose::Compaction)
        };
        let (reached, resume) = session.runtime.store.pause_committed(compacting).await;
        let prompt = spawn_prompt(&session, "Next.");
        bounded(reached).await.unwrap();
        assert_eq!(interrupt_behind(&session, resume).await, 1);
        let interrupted = bounded(prompt).await.unwrap();
        assert!(matches!(interrupted, Err(HarnessError::Interrupted)));
        let records = session.runtime.store.records();
        assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);

        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "next");
        let records = session.runtime.store.records();
        let found = purposes(&records);
        let expected = [
            ModelPurpose::Agent,
            ModelPurpose::Compaction,
            ModelPurpose::Agent,
        ];
        assert_eq!(found, expected);
        let checkpoint = events!(&records, SessionEvent::Compaction { checkpoint } => checkpoint);
        let occupancy = threshold_usage().occupancy().unwrap();
        let trigger = crate::session::CompactionTrigger::Occupancy(occupancy);
        assert_eq!(checkpoint[0].trigger, trigger);
        {
            let captured = requests.lock().unwrap();
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].messages().any(|message| matches!(message,
            Sent::Assistant(items) if *items == vec![AssistantItem::text("answer", 0, "original final")])));
        }
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn oversized_history_or_tool_result_with_low_completed_usage_does_not_compact() {
        for tool_result in [false, true] {
            let low = with_usage(answer("done"), &[usage(10, 5, 1)]);
            let (_root, _requests, session) = scripted_session([low]).await;
            if tool_result {
                seed_history(&session, 6_000).await;
                // An oversized historical tool result must not substitute for actual usage.
                let read = json!({"path": "research.txt"});
                let call = Message::Assistant(vec![tool_call(0, "large-result", "read", read)]);
                let result = Message::Tool(vec![ToolResult {
                    call_id: "large-result".into(),
                    name: "read".into(),
                    result: json!({"content": "research ".repeat(100_000)}),
                    images: vec![],
                    is_error: false,
                }]);
                for message in [call, result] {
                    session
                        .runtime
                        .commit(&session.root, message)
                        .await
                        .unwrap();
                }
            } else {
                seed_history(&session, 100_000).await;
            }
            assert_eq!(session.prompt("Use the result.").await.unwrap(), "done");
            let records = session.runtime.store.records();
            assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn usage_with_cache_writes_above_input_fails_the_response_with_the_last_valid_usage() {
        // Cache writes may equal input, never exceed it.
        let valid = Usage::prompted(100, 0, 100, 5);
        let invalid = Usage::prompted(100, 0, 101, 5);
        let rejected = |events| with_usage(events, &[valid, invalid]);
        for compaction in [false, true] {
            let responses = if compaction {
                // The answer stands; the summary its usage asks for, before the
                // next turn's request, is rejected.
                let summary = rejected(answer(summary_json().to_string()));
                vec![with_usage(answer("final"), &[threshold_usage()]), summary]
            } else {
                vec![rejected(shell_response())]
            };
            let (root, requests, session) = scripted_session(responses).await;
            if compaction {
                seed_history(&session, 6_000).await;
                assert_eq!(session.prompt("Answer.").await.unwrap(), "final");
            }
            let mut observed = session.runtime.store.observe().updates;
            let error = session.prompt("Finish.").await.unwrap_err();
            let protocol = crate::provider::ProviderErrorKind::Protocol;
            assert!(
                matches!(&error, HarnessError::Provider(error) if error.kind() == protocol),
                "{error:?}"
            );
            assert_eq!(requests.lock().unwrap().len(), 1 + usize::from(compaction));
            let records = session.runtime.store.records();
            let expected = if compaction {
                vec![ModelPurpose::Agent, ModelPurpose::Compaction]
            } else {
                vec![ModelPurpose::Agent]
            };
            assert_eq!(purposes(&records), expected);
            let request = records
                .iter()
                .rfind(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
                .unwrap()
                .sequence
                .request();
            let journaled = events!(&records, SessionEvent::Usage { request: at, usage } if *at == request => *usage);
            assert_eq!(journaled, vec![valid]);
            let failed = count!(&records, SessionEvent::ModelFailed { attempt, .. }
                if attempt.request == request);
            assert_eq!(failed, 1);
            let completed = count!(&records, SessionEvent::ResponseCompleted { attempt, .. }
                if attempt.request == request);
            assert_eq!(completed, 0);
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
            assert!(!root.path().join("executions").exists());
            while let Ok(event) = observed.try_recv() {
                assert!(!matches!(event.event, RuntimeEvent::ResponseEvent {
                    event: ResponseEvent::Usage(usage), ..
                } if usage == invalid));
            }
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn cumulative_usage_snapshots_and_separate_responses_are_not_added_for_compaction() {
        let snapshots = [usage(50_000, 0, 0), usage(50_000, 5_000, 5_000)];
        let responses = [
            with_usage(shell_response(), &snapshots),
            with_usage(answer("done"), &snapshots),
        ];
        let (_root, _, session) = scripted_session(responses).await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Run then finish.").await.unwrap(), "done");
        let records = session.runtime.store.records();
        let found = purposes(&records);
        assert_eq!(found, vec![ModelPurpose::Agent, ModelPurpose::Agent]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let usage = events!(&records, SessionEvent::Usage { usage, .. } => *usage);
        assert_eq!(usage, vec![snapshots[1], snapshots[1]]);
        shutdown_session(session).await;
    }
}

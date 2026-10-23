//! Provider turn execution and terminal response handling.

use super::*;
use crate::provider::{
    ProviderContext, ProviderErrorKind,
    protocol::{Completion, ModelRequest, ResponseEvent},
};
use crate::session::RuntimeState;

impl SessionRuntime {
    pub(super) async fn run_turn(
        &self,
        mut turn: TurnContext<'_>,
        agent_context: &mut AgentContext,
        settings: &mut AgentSettings,
    ) -> Result<recovery::Answered, recovery::TurnError> {
        let (agent, owner_job) = (turn.agent, turn.owner_job);
        let (cancellation, location) = (turn.cancellation, turn.location);
        let mut final_text = String::new();
        let mut provider_attempt = 0u64;
        let mut context_failures = 0u8;
        let mut overflowed = false;
        'requests: loop {
            self.flush_events(agent, cancellation).await?;
            // A held turn whose wait ended without `continue` ends here, its
            // results committed.
            if !self.agents(|agents| agents.busy(agent)) {
                return Err(HarnessError::Interrupted.into());
            }
            if self
                .consume_queued_inputs(&mut turn, agent_context, settings)
                .await
                .map_err(recovery::TurnError::refused)?
            {
                provider_attempt = 0;
                context_failures = 0;
            }
            let viewer = turn.diagnostic_viewer();
            self.deliver_events(agent, viewer, &[]).await?;
            agent_context.refresh(&self.store, agent).await?;
            agent_context.admits_images()?;
            let profile = agent_context.model.profile.profile.clone();
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
                self.compact_history(&turn, agent_context, &request, trigger)
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
                    let state = SessionEvent::MessageCommitted { message: state };
                    self.journal_turn(agent, Command::Journal, move |_| vec![state])
                        .await?;
                    agent_context.refresh(&self.store, agent).await?;
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
            let (checkpoint, through) = (
                agent_context.projected.checkpoint,
                agent_context.projected.through(),
            );
            let (tail, history_lifetime) = (tail.into_iter().collect(), request.history_lifetime);
            let requested = move |context| SessionEvent::ModelRequested {
                context,
                checkpoint,
                through,
                from: None,
                tail,
                history_lifetime,
            };
            let model = agent_context.model.clone();
            let sequence = self.journal_request(agent, model, requested).await?;
            let observe = |event: &ResponseEvent| {
                self.store.post(RuntimeEvent::ResponseEvent {
                    agent: agent.clone(),
                    request: sequence,
                    event: event.clone(),
                });
            };
            let provider = agent_context.provider()?;
            let attempted = self.attempt(
                &turn,
                provider,
                &request,
                sequence,
                &mut provider_attempt,
                observe,
            );
            let (attempt, completion, usage) = match attempted.await? {
                Ok(response) => response,
                // Streamed output is discarded with the attempt, so an overflow
                // compacts wherever the stream stopped.
                Err((error, record)) => {
                    if error.kind() == ProviderErrorKind::ContextWindowExceeded {
                        context_failures += 1;
                        if context_failures < compact::MAX_COMPACTION_ATTEMPTS {
                            self.record_failure(agent, record).await?;
                            overflowed = true;
                            continue 'requests;
                        }
                    }
                    let (error, record) = (error.into(), Some(record));
                    return Err(recovery::TurnError { error, record });
                }
            };
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
            // request unencodable. Either fails the turn with no message committed.
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
                    let record = recovery::FailureRecord::attempt(attempt, usage, failure.clone());
                    self.store.post(RuntimeEvent::ResponseSettled {
                        agent: agent.clone(),
                        request: sequence,
                        settlement: Settlement::Failed(failure.into()),
                    });
                    let record = Some(record);
                    return Err(recovery::TurnError { error, record });
                }
            };
            // The message, its usage and its outcome commit in one transaction, so a
            // crash never leaves a response without the attempt's outcome.
            let outcome = {
                let (agent, request) = (agent.clone(), sequence);
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
                let owner = owner_job.map(|job| (job, published.as_str()));
                let answered = author.clone();
                Ok(crate::job::Jobs::message_plan(
                    state,
                    author,
                    assistant,
                    owner,
                    outcome,
                    move |state| {
                        if ends {
                            state.agents.answered(&answered);
                        }
                    },
                )?)
            };
            let origin = match self
                .store
                .commit_on(owner_job, Command::Journal, commit)
                .await
            {
                Err(HarnessError::Interrupted) => {
                    let interrupted = recovery::AttemptEnd::Interrupted;
                    self.record_attempt(agent, attempt, usage, interrupted)
                        .await?;
                    return Err(HarnessError::Interrupted.into());
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
                request: sequence,
                settlement: Settlement::Committed(origin),
            });
            let next_tail = agent_context.tail(|| self.runtime_state(&turn));
            // Decided from the committed response's reported occupancy, which excludes
            // newly produced tool results and queued input. An answer ends the turn's
            // model work: what it would compact waits for the next request boundary.
            agent_context.refresh(&self.store, agent).await?;
            if !ends && let Some(trigger) = agent_context.compaction_due() {
                let current = agent_context.request(next_tail.as_ref());
                self.compact_history(&turn, agent_context, &current, trigger)
                    .await?;
                agent_context.refresh(&self.store, agent).await?;
            }
            if !calls.is_empty() {
                let capabilities = &turn.capabilities;
                let created =
                    (self.create_calls(agent, owner_job, &calls, origin, location, capabilities))
                        .await;
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
                // commit are closed when the turn ends.
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
            // The answer stands however the boundary after it goes: what it
            // refuses is reported after the answer.
            let mut refused = None;
            if calls.is_empty() && self.rejoin(agent).await {
                // Events that arrived during the request precede the answer.
                let joined = async {
                    let viewer = turn.diagnostic_viewer();
                    let delivered = self.deliver_events(agent, viewer, &[]).await?;
                    Ok::<_, super::queue::Refusal>(
                        delivered.is_some()
                            || (self.consume_queued_inputs(&mut turn, agent_context, settings))
                                .await?,
                    )
                };
                match joined.await {
                    Ok(true) => {
                        final_text.clear();
                        // The invocation continues, so the silent answer needs its wake now.
                        if let Some(job) = owner_job {
                            self.jobs.notify_owner(job).await?;
                        }
                        continue 'requests;
                    }
                    Ok(false) => {}
                    Err(error) => refused = Some(error),
                }
            }
            if calls.is_empty() {
                self.store.post(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                });
                let text = final_text;
                return Ok(recovery::Answered { text, refused });
            }
        }
    }
}

/// How an attempt's stream ended by itself; a failure keeps the usage observed before it.
type Streamed = Result<(Completion, Usage), (ProviderError, Usage)>;

impl SessionRuntime {
    fn runtime_state(&self, turn: &TurnContext<'_>) -> RuntimeState {
        let (agent, capabilities) = (turn.agent, &turn.capabilities);
        state::runtime_state_content(&self.jobs, &self.store, agent, capabilities, turn.location)
    }

    /// A response to the frozen `request` journaled as `sequence`, retrying
    /// transient failures until one succeeds, or the permanent error with its
    /// unjournaled record. Each attempt starts as a busy-gated commit, so an
    /// interrupt landing first sends nothing; `attempts` counts those that
    /// committed, the audit numbering of the logical response. The failed stream
    /// is dropped before any recovery wait, as it may own the connection. Nothing
    /// from a failed attempt is committed or executed.
    pub(super) async fn attempt(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        request: &ModelRequest,
        sequence: crate::session::RequestSeq,
        attempts: &mut u64,
        mut observe: impl FnMut(&ResponseEvent),
    ) -> Result<
        Result<
            (crate::session::AttemptRef, Completion, Usage),
            (ProviderError, recovery::FailureRecord),
        >,
        HarnessError,
    > {
        let mut transient = 0u64;
        loop {
            let attempt = crate::session::AttemptRef {
                request: sequence,
                attempt: attempts.saturating_add(1),
            };
            let started = SessionEvent::ModelAttemptStarted(attempt);
            self.journal_turn(turn.agent, Command::Request, move |_| vec![started])
                .await?;
            *attempts = attempt.attempt;
            let stream = provider.invoke(request.clone());
            match (self.consume_stream(turn, attempt, stream, &mut observe)).await? {
                Ok((completion, usage)) => return Ok(Ok((attempt, completion, usage))),
                Err((error, usage)) => {
                    let recovered = (attempt, &mut transient);
                    let recovered = self.recover_model_failure(turn, recovered, usage, error);
                    if let Some(failed) = recovered.await? {
                        return Ok(Err(failed));
                    }
                }
            }
        }
    }

    /// Stream one attempt to its end, handing each event to `observe`. Cancellation
    /// journals the observed usage with the attempt's interruption. An invalid usage
    /// snapshot ends the response before it is observed or accumulated; the attempt
    /// keeps the last valid one.
    async fn consume_stream(
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
                message: Message::Tool(result)
            } => result.clone()
        );
        let reported = results
            .into_iter()
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
    async fn whitespace_text_is_committed_and_replayed_but_empty_text_is_dropped() {
        let items = vec![
            AssistantItem::text("first", 0, "alpha"),
            AssistantItem::text("space", 1, " "),
            AssistantItem::text("second", 2, "beta"),
        ];
        let call = tool_call(1, "call", "missing", serde_json::json!({}));
        let blank = response(vec![AssistantItem::text("blank", 0, ""), call.clone()]);
        let steps = [response(items.clone()), blank, answer("done")];
        let (_root, requests, session) = scripted_session(steps).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "alpha beta");
        let records = session.runtime.store.records();
        let committed = events!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(content) } => content.clone());
        assert_eq!(committed, vec![items.clone()]);
        assert_eq!(session.prompt("again").await.unwrap(), "done");
        let captured = requests.lock().unwrap();
        let replays = |request: &Served, sent: &[AssistantItem]| {
            (request.messages())
                .any(|message| matches!(message, Sent::Assistant(items) if items == sent))
        };
        assert!(replays(&captured[1], &items));
        assert!(
            replays(&captured[2], &[call]),
            "empty text beside a call is dropped"
        );
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
        let outcomes =
            events!(&records, SessionEvent::ResponseCompleted { outcome, .. } => *outcome);
        assert_eq!(outcomes, [crate::session::CompletedOutcome::Answer; 2]);
        shutdown_session(session).await;
    }

    // End-to-end automatic compaction uses completed provider usage.

    use crate::session::{ModelPurpose, tests::request_purpose};

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

    fn purposes(records: &[Arc<EventRecord>]) -> Vec<ModelPurpose> {
        records
            .iter()
            .filter_map(|record| request_purpose(records, record))
            .collect()
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
            matches!(event, SessionEvent::MessageCommitted { message: Message::Tool(result) }
        if result.call_id == "append-once")
        });
        let SessionEvent::MessageCommitted {
            message: tool_message @ Message::Tool(result),
        } = &records[tool].event
        else {
            unreachable!()
        };
        assert!(!result.is_error);
        assert!(result.result.to_string().contains("retained-result"));
        let summary = records
            .iter()
            .position(|record| request_purpose(&records, record) == Some(Compaction))
            .unwrap();
        let checkpoint = position(&|event| matches!(event, SessionEvent::Compaction { .. }));
        let next = records
            .iter()
            .rposition(|record| request_purpose(&records, record) == Some(Agent));
        assert!(summary < checkpoint && checkpoint < tool && tool < next.unwrap());
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert!(captured[1].tools.is_empty());
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
        }
        // The persisted summary boundary and the post-compaction call both replay exactly.
        let captured = requests.lock().unwrap().clone();
        let requested = records
            .iter()
            .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }));
        for (record, sent) in requested.zip(captured.iter()) {
            let store = &session.runtime.store;
            let replayed = store.reconstruct_model_request(record.sequence.request());
            assert_eq!(&replayed.await.unwrap().1, &sent.request);
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

    /// A failed pre-tool compaction closes each of its response's calls with its own
    /// result, in call order, ahead of the turn's failure. The session keeps running,
    /// and once reopened it sends both results and settles nothing again.
    #[tokio::test]
    async fn failed_compaction_closes_every_unexecuted_call_before_the_turn_fails() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let shell = |position, id| {
            let arguments = json!({"command": "printf 'executed\\n' >> executions"});
            tool_call(position, id, "exec", arguments)
        };
        let calls = response(vec![shell(0, "first"), shell(1, "second")]);
        let steps = [
            with_usage(calls, &[threshold_usage()]),
            refusal(Vec::new()),
            answer("retry done"),
        ];
        let provider = scripted_provider(&requests, steps);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        seed_history(&session, 6_000).await;
        assert!(matches!(
            session.prompt("Run the tools.").await,
            Err(HarnessError::Refused(_))
        ));
        let records = session.runtime.store.records();
        let closed = |event: &SessionEvent| match event {
            SessionEvent::MessageCommitted {
                message: Message::Tool(result),
            } => Some((result.call_id.clone(), result.is_error)),
            SessionEvent::AgentFailed { .. } => Some(("failed".into(), true)),
            _ => None,
        };
        let closed: Vec<_> = records
            .iter()
            .filter_map(|record| closed(&record.event))
            .collect();
        let order: Vec<_> = closed.iter().map(|(call, _)| call.as_str()).collect();
        assert_eq!(order, ["first", "second", "failed"]);
        assert!(closed[..2].iter().all(|(_, error)| *error));
        let id = session.id();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        let store = &resumed.runtime.store;
        assert_eq!(store.read(|state| state.requests.unanswered().count()), 0);
        let results = count!(
            &store.records(),
            SessionEvent::MessageCommitted {
                message: Message::Tool(_)
            }
        );
        assert_eq!(results, 2);
        assert_eq!(resumed.prompt("Continue.").await.unwrap(), "retry done");
        let request = requests.lock().unwrap().last().unwrap().clone();
        let sent = request.messages().find_map(|message| match message {
            Sent::Tool(results) => Some(
                results
                    .iter()
                    .map(|result| result.call_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        });
        assert_eq!(sent.unwrap(), ["first", "second"]);
        assert!(!root.path().join("executions").exists());
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn interruption_during_compaction_persistence_settles_unexecuted_calls() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let responses = [
            with_usage(shell_response(), &[threshold_usage()]),
            answer(summary_json().to_string()),
            answer("retry done"),
        ];
        let provider = scripted_provider(&requests, responses);
        let harness = test_harness(root.path(), &root.path().join("sessions"), provider).await;
        let session = harness.new_session().await.unwrap();
        seed_history(&session, 6_000).await;
        let compacted: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::Compaction { .. });
        // Queued behind the checkpoint's commit.
        let interrupted = interrupt_on(&session, &session.root, compacted).await;
        let (result, stopped) =
            bounded(async { tokio::join!(session.prompt("Run the tool."), interrupted) }).await;
        assert_eq!(stopped, 1);
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
                // Queued behind the response's commit, ahead of its calls.
                let interrupted = interrupt_on(&session, &session.root, |_| true).await;
                script.release(0);
                assert_eq!(interrupted.await, 1);
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
        let answered: fn(&SessionEvent) -> bool = |event| {
            matches!(
                event,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            )
        };
        let interrupted = interrupt_on(&session, &session.root, answered).await;
        let prompt = spawn_prompt(&session, "task");
        assert_eq!(bounded(interrupted).await, 0);
        assert_eq!(bounded(prompt).await.unwrap().unwrap(), "answer");
        session.shutdown().await.unwrap();
    }

    /// An interrupt, the user's or one giving up a child's tree, that lands before
    /// an attempt starts sends nothing; one that lands after ends that attempt
    /// interrupted. Either way nothing more is sent, and the request is closed.
    #[tokio::test]
    async fn an_interrupt_either_side_of_an_attempt_start_sends_nothing_after_it() {
        let requested: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::ModelRequested { .. });
        for (tree, started) in [(false, false), (false, true), (true, false), (true, true)] {
            let requests = Requests::default();
            let script = Script::new([Step::new(answer("unsent")).gated()], &requests);
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let harness = test_harness(root.path(), &sessions, script.clone()).await;
            let session = harness.new_session().await.unwrap();
            let store = session.runtime.store.clone();
            let agent = match tree {
                true => spawned_child(&session).await.0,
                false => session.root.clone(),
            };
            let interrupt = {
                let (session, agent) = (session.clone(), agent.clone());
                move || async move {
                    match tree {
                        true => session.runtime.interrupt_tree(&agent).await,
                        false => assert_eq!(session.interrupt().await, 1),
                    }
                }
            };
            let behind = match started {
                true => None,
                false => Some(on_commit(&store, &agent, requested, interrupt.clone()).await),
            };
            let ended: futures_util::future::BoxFuture<bool> = if tree {
                let (done, ended) = oneshot::channel();
                child_input(&session, &agent, Some(done)).await;
                Box::pin(async { ended.await.unwrap() == Err(TurnFailure::Interrupted) })
            } else {
                let prompt = spawn_prompt(&session, "task");
                Box::pin(async { matches!(prompt.await.unwrap(), Err(HarnessError::Interrupted)) })
            };
            match behind {
                Some(interrupted) => bounded(interrupted).await,
                None => {
                    script.request(0).await;
                    interrupt().await;
                }
            }
            assert!(bounded(ended).await, "{tree} {started}");
            let records = store.records();
            let own = || records.iter().filter(|record| record.agent == agent);
            let attempts = (
                count!(own(), SessionEvent::ModelAttemptStarted(_)),
                count!(own(), SessionEvent::ModelAttemptInterrupted(_)),
                requests.lock().unwrap().len(),
            );
            let once = usize::from(started);
            assert_eq!(attempts, (once, once, once), "{tree} {started}");
            let phase = store.read(|state| {
                let request = state.requests.latest(&agent);
                request
                    .and_then(|request| state.requests.get(request))
                    .map(|r| r.phase.clone())
            });
            assert!(
                matches!(phase, Some(crate::session::RequestPhase::Interrupted { attempt, .. })
                    if attempt == started.then_some(1)),
                "{tree} {started}: {phase:?}"
            );
            session.shutdown().await.unwrap();
        }
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
        let results = events!(&records, SessionEvent::MessageCommitted { message: Message::Tool(result) } => result.clone());
        let [result] = results.as_slice() else {
            panic!("one settled exchange: {results:?}")
        };
        assert!(result.is_error && result.call_id == "append-once");
        assert_eq!(session.prompt("Continue.").await.unwrap(), "retry done");
        let expected = sent(&Message::Tool(result.clone()));
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
        let job = session.runtime.jobs.list(&session.root).await.unwrap()[0].id;
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
                    request_purpose(&records, record).unwrap(),
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
        assert_eq!(directive, &super::compact::continuation::directive());
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
        let interrupted = interrupt_on(&session, &session.root, answered).await;
        let prompt = spawn_prompt(&session, "Finish.");
        assert_eq!(bounded(interrupted).await, 0);
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
        let interrupted = interrupt_on(&session, &session.root, compacting).await;
        let prompt = spawn_prompt(&session, "Next.");
        assert_eq!(bounded(interrupted).await, 1);
        let interrupted = bounded(prompt).await.unwrap();
        assert!(matches!(interrupted, Err(HarnessError::Interrupted)));
        // The summary's context commits with its request, which the interrupt
        // leaves unsent: `continue` compacts afresh.
        let records = session.runtime.store.records();
        let expected = [ModelPurpose::Agent, ModelPurpose::Compaction];
        assert_eq!(purposes(&records), expected);

        assert_eq!(bounded(session.continue_turn()).await.unwrap(), "next");
        let records = session.runtime.store.records();
        let found = purposes(&records);
        let expected = [
            ModelPurpose::Agent,
            ModelPurpose::Compaction,
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

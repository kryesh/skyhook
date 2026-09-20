//! Provider turn execution and terminal response handling.

use super::*;

/// The failure recorded when a provider aborts a response mid-stream.
const ABORTED: &str = "provider aborted response";

impl SessionRuntime {
    pub(super) async fn run_turn(
        &self,
        mut turn: TurnContext<'_>,
        agent_context: &mut AgentContext,
        settings: &mut AgentSettings,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> Result<String, HarnessError> {
        let (agent, owner_job) = (turn.agent, turn.owner_job);
        let (cancellation, location) = (turn.cancellation, turn.location);
        let mut context_sequence = None;
        let mut final_text = String::new();
        let mut force_compaction = false;
        let mut provider_attempt = 0u64;
        let mut context_failures = 0u8;
        'requests: loop {
            if let Some(sender) = self.agent_sender(agent) {
                sender.flush_events(cancellation).await?;
                sender.begin_request();
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            if self
                .consume_queued_inputs(&mut turn, agent_context, settings, rx, deferred)
                .await
            {
                // A mode or model change can replace both the template and its token meter.
                context_sequence = None;
                provider_attempt = 0;
                context_failures = 0;
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let (content, messages) = self
                .pending_event_content(agent, &turn.capabilities, location)
                .await?;
            if !content.is_empty() {
                messages.commit().await?;
            }
            let profile = agent_context.profile.clone();
            if !profile.supports_images && agent_context.contains_images() {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let template = agent_context.template.to_request();
            let context = match context_sequence {
                Some(sequence) => sequence,
                None => {
                    let context = crate::session::ModelContext {
                        purpose: crate::session::ModelPurpose::Agent,
                        profile: crate::session::ProfileSnapshot {
                            name: settings.model_profile.clone(),
                            profile: profile.clone(),
                        },
                        system: template.system.clone(),
                        tools: template.tools.clone(),
                        response_schema: template.response_schema.clone(),
                    };
                    let record = self
                        .store
                        .append(agent.clone(), SessionEvent::ModelContext { context })
                        .await?;
                    context_sequence = Some(record.sequence);
                    record.sequence
                }
            };
            agent_context.refresh(&self.store, agent).await?;
            let state = self.runtime_state(&turn).await;
            let mut request = agent_context.request(state);
            if force_compaction {
                let (provider, meter) = (agent_context.provider.as_mut(), &mut agent_context.meter);
                self.compact_history(
                    &turn,
                    provider,
                    meter,
                    context,
                    &request,
                    profile.max_context,
                )
                .await?;
                force_compaction = false;
                // Consume input received during compaction before starting the
                // normal request, rather than delaying it by another request.
                continue 'requests;
            }
            if profile.state_mode == crate::provider::profile::StateMode::Persist
                && let Some(state) = request.tail.pop()
            {
                // Later requests then extend an unchanged conversation, which signed
                // reasoning is bound to.
                let sequence = self.commit(agent, state.clone()).await?;
                agent_context.projected.push((sequence, state.clone()));
                request.history.push(state);
            }
            // The request is frozen across provider recovery; input and model changes
            // wait for the next request boundary.
            let input_estimate = compaction::estimate_request(&request);
            let context_tokens = agent_context.meter.estimate(&request);
            self.store.load_blobs(&mut request).await?;
            let requested = self
                .store
                .append(
                    agent.clone(),
                    SessionEvent::ModelRequested {
                        context,
                        history: compact::context_sources(&agent_context.projected),
                        tail: request.tail.clone(),
                        history_lifetime: request.history_lifetime,
                        purpose: crate::session::ModelPurpose::Agent,
                    },
                )
                .await?;
            let mut transient_attempt = 0u64;
            let (requested, response) = 'attempts: loop {
                if cancellation.is_cancelled() {
                    return Err(HarnessError::Interrupted);
                }
                self.activity(agent, AgentActivity::Working);
                self.events.send(RuntimeEvent::Context {
                    agent: agent.clone(),
                    tokens: context_tokens,
                    capacity: profile.max_context,
                });
                provider_attempt = provider_attempt.saturating_add(1);
                self.store
                    .append(
                        agent.clone(),
                        SessionEvent::ModelAttemptStarted {
                            request: requested.sequence,
                            attempt: provider_attempt,
                        },
                    )
                    .await?;
                let invoked = tokio::select! {
                    response = agent_context.provider.invoke(request.clone()) => response,
                    () = cancellation.cancelled() => {
                        self.record_attempt_interrupted(agent, requested.sequence, provider_attempt)
                            .await?;
                        return Err(HarnessError::Interrupted);
                    },
                };
                // The failed stream may own the connection, so it is dropped with this
                // block before any recovery wait.
                let streamed = 'stream: {
                    let mut response = match invoked {
                        Ok(response) => response,
                        Err(error) => break 'stream Err((error, Usage::default(), false)),
                    };
                    let mut assembler = ResponseAssembler::default();
                    let mut usage = Usage::default();
                    let mut saw_content = false;
                    loop {
                        let chunk = tokio::select! {
                            chunk = response.next() => chunk,
                            () = cancellation.cancelled() => {
                                self.record_model_usage(agent, requested.sequence, usage).await?;
                                self.record_attempt_interrupted(agent, requested.sequence, provider_attempt)
                                    .await?;
                                return Err(HarnessError::Interrupted);
                            },
                        };
                        let Some(chunk) = chunk else {
                            break 'stream Ok((assembler, usage));
                        };
                        let pushed = chunk.and_then(|chunk| {
                            assembler.push(&chunk)?;
                            Ok(chunk)
                        });
                        let chunk = match pushed {
                            Ok(chunk) => chunk,
                            Err(error) => break 'stream Err((error, usage, saw_content)),
                        };
                        saw_content |= !matches!(&chunk, ResponseChunk::UsageUpdated { .. });
                        if let ResponseChunk::UsageUpdated { usage: value } = &chunk {
                            usage = *value;
                        }
                        self.events.send(RuntimeEvent::ResponseEvent {
                            agent: agent.clone(),
                            request: requested.sequence,
                            event: chunk,
                        });
                    }
                };
                // Nothing from a failed attempt is committed or executed.
                let (assembler, usage) = match streamed {
                    Ok(streamed) => streamed,
                    Err((error, usage, saw_content)) => {
                        let attempt = (provider_attempt, &mut transient_attempt);
                        let Some(error) = self
                            .recover_model_failure(&turn, requested.sequence, attempt, usage, error)
                            .await?
                        else {
                            continue 'attempts;
                        };
                        if !saw_content
                            && error.kind
                                == crate::provider::ProviderErrorKind::ContextWindowExceeded
                        {
                            context_failures += 1;
                            if context_failures < compact::MAX_COMPACTION_ATTEMPTS {
                                force_compaction = true;
                                continue 'requests;
                            }
                        }
                        return Err(error.into());
                    }
                };
                let response = match finish_response(assembler, usage) {
                    Ok(response) => response,
                    Err(error) => {
                        let kind = crate::session::ModelFailureKind::Error;
                        let (request, message) = (requested.sequence, error.to_string());
                        self.record_model_failure(
                            agent,
                            request,
                            provider_attempt,
                            usage,
                            message,
                            kind,
                        )
                        .await?;
                        return Err(error);
                    }
                };
                break (requested, response);
            };
            use crate::{provider::protocol::StopReason, session::ModelFailureKind};
            let aborted = response.stop_reason == StopReason::Aborted;
            let assistant = Message::Assistant(response.blocks);
            // A refusal is never history and never retried here: it is deterministic
            // for a given request. A content-free message would make every later
            // request unencodable. Either fails the turn with nothing committed.
            let unusable = if response.stop_reason == StopReason::ContentFilter {
                let error = HarnessError::Refused(refusal_detail(&response.text));
                Some((error, ModelFailureKind::Refusal))
            } else if !assistant.is_content_free() {
                None
            } else if aborted {
                Some((HarnessError::ProviderAborted, ModelFailureKind::Error))
            } else {
                Some((HarnessError::EmptyResponse, ModelFailureKind::Error))
            };
            if let Some((error, kind)) = unusable {
                let (request, message) = (requested.sequence, error.to_string());
                self.record_model_failure(
                    agent,
                    request,
                    provider_attempt,
                    response.usage,
                    message,
                    kind,
                )
                .await?;
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request,
                    message: None,
                    error: Some(error.to_string()),
                });
                return Err(error);
            }
            // A working child's progress wakes its owner at once; a text-only answer is
            // published silently so it arrives with the invocation's resolution.
            let working = !response.calls.is_empty();
            // The message, its usage and its outcome commit in one transaction, so a
            // crash never leaves a response without the attempt's outcome.
            let outcome = {
                let (agent, request) = (agent.clone(), requested.sequence);
                let (usage, stop_reason) = (response.usage, response.stop_reason.clone());
                move |message: u64| {
                    let mut events = Vec::new();
                    if !aborted || usage != Usage::default() {
                        events.push(SessionEvent::Usage {
                            request: Some(request),
                            usage,
                        });
                    }
                    events.push(if aborted {
                        SessionEvent::ModelFailed {
                            request,
                            attempt: provider_attempt,
                            error: ABORTED.to_owned(),
                            kind: crate::session::ModelFailureKind::Error,
                        }
                    } else {
                        SessionEvent::ResponseCompleted {
                            request,
                            attempt: provider_attempt,
                            message: Some(message),
                            stop_reason,
                        }
                    });
                    events
                        .into_iter()
                        .map(|event| (agent.clone(), event))
                        .collect()
                }
            };
            let origin = if let Some(job) = owner_job {
                self.jobs
                    .commit_child_message(
                        agent,
                        job,
                        assistant.clone(),
                        response.text.clone(),
                        working,
                        outcome,
                    )
                    .await?
            } else {
                self.store
                    .append_then(
                        agent.clone(),
                        SessionEvent::MessageCommitted {
                            message: assistant.clone(),
                        },
                        outcome,
                    )
                    .await?[0]
                    .sequence
            };
            self.usage.lock().await.accumulate(response.usage);
            agent_context.projected.push((origin, assistant));
            if aborted {
                // Preserve completed visible/replay content, but never turn a
                // provider abort into a successful agent turn.
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: requested.sequence,
                    message: Some(origin),
                    error: Some(ABORTED.to_owned()),
                });
                return Err(HarnessError::ProviderAborted);
            }
            // Child replies are already independently published at commit, even
            // when queued input will make a text-only response nonterminal. Only
            // the eventual answer belongs in the saved final job result.
            if owner_job.is_none() || response.calls.is_empty() {
                final_text.push_str(&response.text);
            }
            self.events.send(RuntimeEvent::ResponseSettled {
                agent: agent.clone(),
                request: requested.sequence,
                message: Some(origin),
                error: None,
            });
            agent_context.meter.observe(input_estimate, response.usage);
            // Decide once from the successful completed response, never from an
            // estimate that includes newly produced tool results or queued input.
            let compact_completed_response = agent_context.needs_compaction(response.usage);
            let state = self.runtime_state(&turn).await;
            let current = agent_context.request(state);
            self.events.send(RuntimeEvent::Context {
                agent: agent.clone(),
                tokens: agent_context.meter.estimate(&current),
                capacity: profile.max_context,
            });
            if !response.calls.is_empty() {
                self.questions
                    .prepare_question_batch(agent, &response.calls, self.executor.registry())
                    .await;
                self.activity(agent, AgentActivity::Tools);
                // Each result commits as its call finishes, so a crash keeps every
                // completed result; history merges them back into call order.
                let mut calls: futures_util::stream::FuturesUnordered<_> = response
                    .calls
                    .iter()
                    .map(|call| {
                        if !agent_context.unavailable_tools.contains(call.name()) {
                            return self
                                .execute_call(agent, owner_job, call, origin, location, &turn.capabilities);
                        }
                        // A pinned tool the live registry no longer provides as journaled.
                        Box::pin(std::future::ready(ToolResult {
                            call_id: call.id().to_owned(),
                            name: call.name().to_owned(),
                            result: json!({"error": format!("tool `{}` is unavailable in this session", call.name())}),
                            images: Vec::new(),
                            is_error: true,
                        }))
                    })
                    .collect();
                // Drain every call even after a failed commit; results that could not
                // commit are settled as interrupted when the session resumes.
                let mut committed = Ok(());
                while let Some(result) = futures_util::StreamExt::next(&mut calls).await {
                    if committed.is_ok() {
                        let tools = Message::Tool(vec![result]);
                        committed = self
                            .commit(agent, tools.clone())
                            .await
                            .map(|sequence| agent_context.projected.push((sequence, tools)));
                    }
                }
                committed?;
            }
            if compact_completed_response {
                // The tool exchange is closed first: checkpoints retain whole exchanges.
                let (provider, meter) = (agent_context.provider.as_mut(), &mut agent_context.meter);
                let installed = self
                    .compact_history(
                        &turn,
                        provider,
                        meter,
                        context,
                        &current,
                        profile.max_context,
                    )
                    .await?;
                if !installed {
                    agent_context.compaction_skipped(response.usage);
                }
                agent_context.refresh(&self.store, agent).await?;
                context_sequence = None;
            }
            provider_attempt = 0;
            context_failures = 0;
            if response.calls.is_empty() {
                // Events that arrived during the request precede the answer.
                let (content, messages) = self
                    .pending_event_content(agent, &turn.capabilities, location)
                    .await?;
                if !content.is_empty() {
                    messages.commit().await?;
                    final_text.clear();
                    // The invocation continues, so the silent answer needs its wake now.
                    if let Some(job) = owner_job {
                        self.jobs.notify_owner(job).await;
                    }
                    continue 'requests;
                }
                // A response without tools is a request boundary too: consume prompts
                // received in flight before returning a stale answer.
                if self
                    .consume_queued_inputs(&mut turn, agent_context, settings, rx, deferred)
                    .await
                {
                    context_sequence = None;
                    final_text.clear();
                    if let Some(job) = owner_job {
                        self.jobs.notify_owner(job).await;
                    }
                    continue 'requests;
                }
                self.events.send(RuntimeEvent::TurnCompleted {
                    agent: agent.clone(),
                    text: final_text.clone(),
                });
                return Ok(final_text);
            }
        }
    }
}
impl SessionRuntime {
    async fn runtime_state(&self, turn: &TurnContext<'_>) -> UserContent {
        let (agent, capabilities) = (turn.agent, &turn.capabilities);
        state::runtime_state_content(&self.jobs, &self.todos, agent, capabilities, turn.location)
            .await
    }
}

/// Bounded excerpt of any partial text a refused response produced.
const REFUSAL_EXCERPT_LIMIT: usize = 240;

/// Human-readable refusal detail for the journal and for a parent agent. Providers
/// rarely return refusal prose, so partial visible text is preserved when present.
fn refusal_detail(text: &str) -> String {
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

struct FoldedResponse {
    stop_reason: crate::provider::protocol::StopReason,
    blocks: Vec<AssistantContent>,
    usage: Usage,
    calls: Vec<ToolCall>,
    text: String,
}

fn finish_response(
    assembler: ResponseAssembler,
    usage: Usage,
) -> Result<FoldedResponse, HarnessError> {
    let (mut blocks, final_usage, stop_reason) = assembler.finish()?;
    // Only a normal finish authorizes execution, even if a backend completed
    // valid arguments before learning the final stop reason.
    if !stop_reason.authorizes_tools() {
        blocks.retain(|item| {
            !item
                .blocks
                .iter()
                .any(|block| matches!(block.content, BlockContent::ToolCall(_)))
        });
    }
    debug_assert_eq!(usage, final_usage);
    // A whitespace-only response is no response: the blank text block stays in
    // `blocks` for replay, but it must not read as an answer to this turn.
    let text = crate::provider::protocol::visible_text(&blocks);
    let calls = blocks
        .iter()
        .flat_map(|item| &item.blocks)
        .filter_map(|block| match &block.content {
            BlockContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    Ok(FoldedResponse {
        stop_reason,
        blocks,
        usage,
        calls,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::runtime::tests::*;

    fn ended(items: &[AssistantContent], stop_reason: StopReason) -> Vec<ResponseChunk> {
        let mut events = events_for_content(items);
        events.push(ResponseChunk::ResponseEnded { stop_reason });
        events
    }

    /// A decoded response that ends in a refusal, as a content filter produces.
    fn refusal(items: Vec<AssistantContent>) -> Vec<ResponseChunk> {
        ended(&items, StopReason::ContentFilter)
    }

    #[tokio::test]
    async fn unusable_responses_fail_the_turn_without_committing_or_retrying() {
        use crate::session::ModelFailureKind::{Error, Refusal};
        let call = tool_call(0, "call", "shell", json!({"command":"true"}));
        let mut incomplete = answer("must not persist");
        incomplete.truncate(3); // item and block started, item never ended
        let cases = [
            // A refusing provider streams nothing, or only an unusable reasoning stub.
            (refusal(Vec::new()), "content filter", Refusal),
            (
                refusal(vec![AssistantContent::reasoning("thought", 0, "", None)]),
                "declined to respond",
                Refusal,
            ),
            // MaxTokens and Aborted discard tool calls, which can leave nothing at all.
            (
                ended(std::slice::from_ref(&call), StopReason::MaxTokens),
                "no assistant content",
                Error,
            ),
            (
                ended(&[call], StopReason::Aborted),
                "provider aborted",
                Error,
            ),
            (response(Vec::new()), "no assistant content", Error),
            (incomplete, "response ended before all items ended", Error),
        ];
        for (chunks, expected, kind) in cases {
            // The refusal is durable: it must survive a reopen to be continued.
            let (root, requests) = (tempfile::tempdir().unwrap(), Requests::default());
            let provider = scripted_provider(&requests, [chunks, answer("must not be requested")]);
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
            let records = session.runtime.store.records().await;
            // An error state, never conversation history or an executed tool.
            assert_eq!(assistant_commits(&records), 0);
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
            let failed = events!(&records, SessionEvent::ModelFailed { kind, error, .. } => (*kind, error.clone()));
            assert_eq!(failed.len(), 1);
            assert_eq!(failed[0].0, kind);
            assert!(failed[0].1.contains(expected), "{failed:?}");
            let agent = events!(&records, SessionEvent::AgentFailed { error } => error.clone());
            assert_eq!(agent.len(), 1);
            assert!(agent[0].contains(expected), "{agent:?}");
            assert_eq!(
                count!(&records, SessionEvent::ModelAttemptStarted { .. }),
                1
            );
            assert_eq!(
                count!(&records, SessionEvent::ModelRecoveryScheduled { .. }),
                0
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
        let records = session.runtime.store.records().await;
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
            count!(&child_records, SessionEvent::ModelFailed { kind, .. }
                if *kind == crate::session::ModelFailureKind::Refusal),
            1
        );
        assert_eq!(count!(&child_records, SessionEvent::AgentFailed { .. }), 1);
        assert_eq!(assistant_commits(&child_records), 0);
    }

    #[tokio::test]
    async fn blank_text_responses_are_committed_and_complete_the_turn() {
        // Some providers return empty or whitespace text on a non-final turn. That
        // is ordinary content: it encodes and must stay in history, so it must not be
        // treated as a failure. It is not an answer, though: the projection
        // (`provider::protocol::visible_text`) normalizes blank text away, so the turn
        // completes with no text rather than with the blank string itself.
        for text in ["", "   "] {
            let (_root, _requests, session) =
                scripted_session([response(vec![AssistantContent::text("answer", 0, text)])]).await;
            assert_eq!(session.prompt("hello").await.unwrap(), "");
            let records = session.runtime.store.records().await;
            assert_eq!(assistant_commits(&records), 1);
            assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 0);
            assert_eq!(count!(&records, SessionEvent::AgentFailed { .. }), 0);
        }
    }

    #[tokio::test]
    async fn successful_responses_journal_their_terminal_stop_reason() {
        let (_root, _requests, session) = scripted_session([answer("done")]).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let reasons = events!(
            &records,
            SessionEvent::ResponseCompleted { stop_reason, .. } => stop_reason.clone()
        );
        assert_eq!(reasons, vec![StopReason::EndTurn]);
    }

    #[test]
    fn abnormal_termination_never_executes_even_completed_tool_calls() {
        let reasons = [
            StopReason::MaxTokens,
            StopReason::ContentFilter,
            StopReason::Aborted,
            StopReason::Other("pause_turn".into()),
            StopReason::Other("unknown".into()),
        ];
        for stop_reason in reasons {
            let mut assembler = ResponseAssembler::default();
            let items = vec![
                AssistantContent::text("answer", 0, "Visible response"),
                tool_call(1, "call", "shell", json!({"command":"unsafe"})),
            ];
            for event in events_for_content(&items) {
                assembler.push(&event).unwrap();
            }
            let ended = ResponseChunk::ResponseEnded { stop_reason };
            assembler.push(&ended).unwrap();
            let folded = finish_response(assembler, Usage::default()).unwrap();
            assert_eq!(folded.text, "Visible response");
            assert!(folded.calls.is_empty());
            assert_eq!(folded.blocks.len(), 1);
        }
    }

    #[tokio::test]
    async fn aborted_response_preserves_visible_content_and_usage_but_fails() {
        let replay = ReplayEnvelope {
            version: 1,
            protocol: "responses".into(),
            model: "native".into(),
            scope: "reasoning".into(),
            payload: json!({"encrypted_content":"retained"}),
            conversation_bound: false,
        };
        let retained = vec![
            AssistantContent::reasoning("reason", 0, "completed reasoning", Some(replay)),
            AssistantContent::text("answer", 1, "partial visible answer"),
        ];
        let mut items = retained.clone();
        let unsafe_write = json!({"path":"must-not-exist", "content":"unsafe"});
        items.push(tool_call(2, "call", "write", unsafe_write));
        let observed = usage(11, 7, 3);
        let mut chunks = events_for_content(&items);
        chunks.push(ResponseChunk::UsageUpdated { usage: observed });
        chunks.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::Aborted,
        });
        let (root, requests, session) = scripted_session([chunks]).await;
        let mut events = session.runtime.events.observe().updates;
        let error = session.prompt("Abort this turn.").await.unwrap_err();
        assert!(
            matches!(error, HarnessError::Agent(error) if error == HarnessError::ProviderAborted.to_string())
        );
        assert!(matches!(
            session.runtime.events.observe().snapshot.activity.get(&session.root),
            Some(AgentActivity::Failed(error)) if error == "provider aborted response"
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(session.usage().await, observed);
        let records = session.runtime.store.records().await;
        let assistants = events!(&records, SessionEvent::MessageCommitted { message: Message::Assistant(items) } => items.clone());
        assert_eq!(assistants, vec![retained]);
        assert_eq!(count!(&records, SessionEvent::Usage { .. }), 1);
        assert_eq!(
            count!(&records, SessionEvent::ModelFailed { error, .. } if error == "provider aborted response"),
            1
        );
        assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        assert!(!root.path().join("must-not-exist").exists());
        let mut settled = false;
        while let Ok(event) = events.try_recv() {
            match event.event {
                RuntimeEvent::ResponseSettled { message, error, .. } => {
                    assert!(message.is_some());
                    assert_eq!(error.as_deref(), Some("provider aborted response"));
                    settled = true;
                }
                RuntimeEvent::TurnCompleted { .. } => {
                    panic!("aborted turn cannot complete successfully")
                }
                _ => {}
            }
        }
        assert!(settled);
        session.shutdown().await.unwrap();
    }

    // End-to-end automatic compaction uses completed provider usage, not request estimates.

    use crate::session::ModelPurpose;

    fn with_usage(mut chunks: Vec<ResponseChunk>, snapshots: &[Usage]) -> Vec<ResponseChunk> {
        let end = chunks.pop().expect("response has a terminal chunk");
        assert!(matches!(end, ResponseChunk::ResponseEnded { .. }));
        for &usage in snapshots {
            chunks.push(ResponseChunk::UsageUpdated { usage });
        }
        chunks.push(end);
        chunks
    }

    // Exactly 80% of the harness's 128,000-token context. Each component is
    // necessary: neither input alone nor input plus cache reaches the threshold.
    const THRESHOLD: (u64, u64, u64) = (80_000, 20_000, 2_400);

    fn threshold_usage() -> Usage {
        usage(THRESHOLD.0, THRESHOLD.1, THRESHOLD.2)
    }

    async fn seed_history(session: &SessionHandle, repetitions: usize) {
        let research = AssistantContent::text("old-history", 0, "research ".repeat(repetitions));
        let research = Message::Assistant(vec![research]);
        session
            .runtime
            .commit(&session.root, research)
            .await
            .unwrap();
    }

    fn purposes(records: &[EventRecord]) -> Vec<ModelPurpose> {
        events!(records, SessionEvent::ModelRequested { purpose, .. } => *purpose)
    }

    fn shell_response() -> Vec<ResponseChunk> {
        let command = "printf 'executed\\n' >> executions; printf retained-result";
        let arguments = json!({ "command": command });
        response(vec![tool_call(0, "append-once", "shell", arguments)])
    }

    async fn session_with_max_output(
        max_output: u64,
        responses: impl IntoIterator<Item = Vec<ResponseChunk>>,
    ) -> (tempfile::TempDir, Requests, SessionHandle) {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let provider = scripted_provider(&requests, responses);
        let profile = ModelProfile::new("test", "test", None, 128_000, max_output, false);
        let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
            .model_profile("test", profile)
            .build()
            .await
            .unwrap();
        (root, requests, harness.new_session().await.unwrap())
    }

    #[tokio::test]
    async fn high_usage_tool_response_closes_exchange_before_summary_and_next_normal_request() {
        let (root, requests, session) = scripted_session([
            with_usage(shell_response(), &[threshold_usage()]),
            answer(summary_json().to_string()),
            answer("original final"),
        ])
        .await;
        // Enough old material to remove, but nowhere near the automatic threshold.
        seed_history(&session, 6_000).await;
        let found = session.prompt("Run the tool.").await.unwrap();
        assert_eq!(found, "original final");
        let executions = fs::read_to_string(root.path().join("executions")).await;
        // compaction must not re-execute the tool
        assert_eq!(executions.unwrap(), "executed\n");
        let records = session.runtime.store.records().await;
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
        let summary = position(
            &|event| matches!(event, SessionEvent::ModelRequested { purpose, .. } if *purpose == Compaction),
        );
        let checkpoint = position(&|event| matches!(event, SessionEvent::Compaction { .. }));
        let next = records.iter().rposition(|record| {
        matches!(&record.event, SessionEvent::ModelRequested { purpose, .. } if *purpose == Agent)
    });
        assert!(tool < summary && summary < checkpoint && checkpoint < next.unwrap());
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].tools.is_empty());
            assert!(captured[2].response_schema.is_none());
            assert_eq!(captured[0].tools, captured[2].tools);
            for request in [&captured[1], &captured[2]] {
                let index = request
                    .messages()
                    .position(|message| message == tool_message)
                    .expect("summary and continuation retain the actual tool result");
                assert!(
                    matches!(&request.history[index - 1], Message::Assistant(items)
                if items.iter().flat_map(|item| &item.blocks).any(|block|
                    matches!(&block.content, BlockContent::ToolCall(call) if call.id() == "append-once")))
                );
            }
        }
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn state_mode_persists_or_omits_runtime_state() {
        use crate::provider::{profile::StateMode, protocol::UserContent};
        let state = |message: &&Message| {
            matches!(message, Message::User(blocks) if blocks.iter().any(|block|
            matches!(block, UserContent::Runtime { text } if text.starts_with("<skyhook_state>"))))
        };
        for mode in [StateMode::None, StateMode::Persist] {
            let root = tempfile::tempdir().unwrap();
            let requests = Requests::default();
            let shell = response(vec![tool_call(
                0,
                "first",
                "shell",
                json!({"command": "true"}),
            )]);
            let provider = scripted_provider(&requests, [shell, answer("done")]);
            let mut profile = ModelProfile::new("test", "test", None, 128_000, 4096, false);
            profile.state_mode = mode;
            let harness = test_builder(root.path(), &root.path().join("sessions"), provider, false)
                .model_profile("test", profile)
                .build()
                .await
                .unwrap();
            let session = harness.new_session().await.unwrap();
            assert_eq!(session.prompt("Run the tool.").await.unwrap(), "done");
            let captured = requests.lock().unwrap().clone();
            let [first, second] = captured.as_slice() else {
                panic!("unexpected requests: {captured:?}")
            };
            let persist = usize::from(mode == StateMode::Persist);
            for (request, states) in [(first, persist), (second, 2 * persist)] {
                assert!(request.tail.is_empty());
                assert_eq!(request.history.iter().filter(state).count(), states);
            }
            // Append-only: everything sent before is resent unchanged.
            let sent: Vec<_> = first.messages().collect();
            assert!(second.messages().take(sent.len()).eq(sent));
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn only_compaction_summaries_end_their_history() {
        use crate::provider::protocol::{HistoryLifetime::*, UserContent};
        let shell = |id| response(vec![tool_call(0, id, "shell", json!({"command": "true"}))]);
        let (_root, _requests, session) = scripted_session([
            shell("first"),
            with_usage(shell("second"), &[threshold_usage()]),
            answer(summary_json().to_string()),
            answer("done"),
        ])
        .await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Run both tools.").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let requests = events!(&records, SessionEvent::ModelRequested { tail, history_lifetime, purpose, .. }
        => (*purpose, tail.as_slice(), *history_lifetime));
        let [first, second, summary, after] = requests.as_slice() else {
            panic!("unexpected requests: {requests:?}")
        };
        assert_eq!((summary.0, summary.2), (ModelPurpose::Compaction, Detached));
        assert_eq!(summary.1.last(), Some(&compaction::directive()));
        for (purpose, tail, lifetime) in [first, second, after] {
            assert_eq!((*purpose, *lifetime), (ModelPurpose::Agent, Continuing));
            assert!(matches!(tail, [Message::User(blocks)]
            if matches!(blocks.as_slice(), [UserContent::Runtime { .. }])));
        }
        shutdown_session(session).await;
    }

    #[tokio::test]
    async fn high_usage_final_response_compacts_before_returning_original_text_regardless_of_max_output()
     {
        for max_output in [1, 120_000] {
            let final_answer = with_usage(answer("original final"), &[threshold_usage()]);
            let summary = answer(summary_json().to_string());
            let summary = with_usage(summary, &[usage(1_000_000, 0, 1)]);
            let (_root, requests, session) =
                session_with_max_output(max_output, [final_answer, summary]).await;
            seed_history(&session, 6_000).await;
            assert_eq!(session.prompt("Finish.").await.unwrap(), "original final");
            // Live state must already show the reduced context, without further catch-up.
            let observation = session.runtime.events.observe().snapshot;
            let records = session.runtime.store.records().await;
            let found = purposes(&records);
            assert_eq!(found, vec![ModelPurpose::Agent, ModelPurpose::Compaction]);
            let checkpoint =
                events!(&records, SessionEvent::Compaction { checkpoint } => checkpoint);
            let checkpoint = checkpoint[0];
            let context = observation.context.get(&session.root).unwrap();
            assert!(checkpoint.after_tokens < checkpoint.before_tokens);
            // final-response compaction must refresh live occupancy before returning
            assert_eq!(context.tokens, checkpoint.after_tokens);
            assert_eq!(context.capacity, 128_000);
            {
                let captured = requests.lock().unwrap();
                assert_eq!(captured.len(), 2);
                // The summary's reported prompt size calibrates both sides of the checkpoint.
                let estimate = super::super::compaction::estimate_request;
                let scaled = estimate(&captured[0]) * 1_000_000 / estimate(&captured[1]);
                assert!(checkpoint.before_tokens >= scaled);
                assert!(captured[0].response_schema.is_none());
                assert!(captured[1].response_schema.is_some());
                assert!(captured[1].messages().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("answer", 0, "original final")])));
            }
            shutdown_session(session).await;
        }
    }

    #[tokio::test]
    async fn oversized_history_or_tool_result_with_low_completed_usage_does_not_compact() {
        for tool_result in [false, true] {
            let low = with_usage(answer("done"), &[usage(10, 5, 1)]);
            let (_root, requests, session) = scripted_session([low]).await;
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
            let records = session.runtime.store.records().await;
            assert_eq!(purposes(&records), vec![ModelPurpose::Agent]);
            assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
            let estimate = super::super::compaction::estimate_request(&requests.lock().unwrap()[0]);
            assert!(estimate > 128_000);
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
        let (_root, _, session) = session_with_max_output(120_000, responses).await;
        seed_history(&session, 6_000).await;
        assert_eq!(session.prompt("Run then finish.").await.unwrap(), "done");
        let records = session.runtime.store.records().await;
        let found = purposes(&records);
        assert_eq!(found, vec![ModelPurpose::Agent, ModelPurpose::Agent]);
        assert_eq!(count!(&records, SessionEvent::Compaction { .. }), 0);
        let usage = events!(&records, SessionEvent::Usage { usage, .. } => *usage);
        assert_eq!(usage, vec![snapshots[1], snapshots[1]]);
        shutdown_session(session).await;
    }
}

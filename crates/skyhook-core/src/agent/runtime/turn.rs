//! Provider turn execution and terminal response handling.

use super::*;

impl SessionRuntime {
    pub(super) async fn run_turn(
        &self,
        turn: TurnContext<'_>,
        agent_context: &mut AgentContext,
        model_profile: &mut String,
        rx: &mut mpsc::Receiver<AgentCommand>,
        deferred: &mut VecDeque<AgentCommand>,
    ) -> Result<String, HarnessError> {
        let TurnContext {
            agent,
            owner_job,
            cancellation,
            location,
            capabilities,
        } = turn;
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
                .consume_queued_inputs(&turn, agent_context, model_profile, rx, deferred)
                .await
            {
                // A model change can replace both the template and its token meter.
                context_sequence = None;
                provider_attempt = 0;
                context_failures = 0;
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let (content, messages) = self
                .pending_event_content(agent, capabilities, location)
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
                    let record = self
                        .store
                        .append(
                            agent.clone(),
                            SessionEvent::ModelContext {
                                provider: profile.provider.clone(),
                                template: template.clone(),
                            },
                        )
                        .await?;
                    context_sequence = Some(record.sequence);
                    record.sequence
                }
            };
            agent_context.refresh(&self.store.records().await, agent)?;
            let runtime = prompt::runtime_state_content(
                &self.jobs,
                &self.todos,
                agent,
                capabilities,
                location,
            )
            .await;
            let mut request = agent_context.request(runtime);
            if force_compaction {
                self.compact_history(
                    &turn,
                    agent_context.provider.as_mut(),
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
            // Freeze the request across provider recovery. Input/notifications and
            // model changes remain queued until the next normal request boundary.
            // Partial streamed output is display-only: only a fully assembled response
            // below is committed and allowed to execute Skyhook tools.
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
                    () = cancellation.cancelled() => return Err(HarnessError::Interrupted),
                };
                let mut response = match invoked {
                    Ok(response) => response,
                    Err(error) => {
                        self.record_model_failure(
                            agent,
                            requested.sequence,
                            provider_attempt,
                            Usage::default(),
                            error.to_string(),
                        )
                        .await?;
                        // The runtime alone applies the unbounded transient retry policy to an
                        // eligible, uncommitted response; adapters do not own retry counts.
                        if error.recovery().is_some() {
                            transient_attempt = transient_attempt.saturating_add(1);
                            self.schedule_model_recovery(
                                &turn,
                                requested.sequence,
                                recovery::RecoveryAttempt {
                                    model: provider_attempt,
                                    transient: transient_attempt,
                                },
                                &error,
                                agent_context.provider.as_mut(),
                            )
                            .await?;
                            continue 'attempts;
                        }
                        if error.kind == crate::provider::ProviderErrorKind::ContextWindowExceeded {
                            context_failures += 1;
                            if context_failures >= compact::MAX_COMPACTION_ATTEMPTS {
                                return Err(error.into());
                            }
                            force_compaction = true;
                            continue 'requests;
                        }
                        return Err(error.into());
                    }
                };
                let mut assembler = ResponseAssembler::default();
                let mut usage = Usage::default();
                let mut saw_content = false;
                loop {
                    let chunk = tokio::select! {
                        chunk = response.next() => chunk,
                        () = cancellation.cancelled() => {
                            self.record_model_usage(agent, requested.sequence, usage).await?;
                            return Err(HarnessError::Interrupted);
                        },
                    };
                    let Some(chunk) = chunk else {
                        break;
                    };
                    let chunk = match chunk.and_then(|chunk| {
                        assembler.push(&chunk)?;
                        Ok(chunk)
                    }) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            self.record_model_failure(
                                agent,
                                requested.sequence,
                                provider_attempt,
                                usage,
                                error.to_string(),
                            )
                            .await?;
                            if error.recovery().is_some() {
                                transient_attempt = transient_attempt.saturating_add(1);
                                // The failed stream may own the connection mutex. Drop it
                                // before resetting or waiting. No assistant message or tools
                                // from this attempt have been committed/executed.
                                drop(response);
                                self.schedule_model_recovery(
                                    &turn,
                                    requested.sequence,
                                    recovery::RecoveryAttempt {
                                        model: provider_attempt,
                                        transient: transient_attempt,
                                    },
                                    &error,
                                    agent_context.provider.as_mut(),
                                )
                                .await?;
                                continue 'attempts;
                            }
                            if !saw_content
                                && error.kind
                                    == crate::provider::ProviderErrorKind::ContextWindowExceeded
                            {
                                context_failures += 1;
                                if context_failures >= compact::MAX_COMPACTION_ATTEMPTS {
                                    return Err(error.into());
                                }
                                force_compaction = true;
                                continue 'requests;
                            }
                            return Err(error.into());
                        }
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
                let response = match finish_response(assembler, usage) {
                    Ok(response) => response,
                    Err(error) => {
                        self.record_model_failure(
                            agent,
                            requested.sequence,
                            provider_attempt,
                            usage,
                            error.to_string(),
                        )
                        .await?;
                        return Err(error);
                    }
                };
                break (requested, response);
            };
            if response.stop_reason == crate::provider::protocol::StopReason::ContentFilter {
                // A refusal is a successful response that produced no usable turn.
                // It is an error state, never conversation history, so nothing is
                // committed. It is also never retried automatically: refusals are
                // deterministic for a given request, so the trigger must come from
                // outside the refused agent (its parent, or a human), optionally
                // after selecting another model.
                let error = HarnessError::Refused(refusal_detail(&response));
                self.record_model_outcome(
                    agent,
                    requested.sequence,
                    provider_attempt,
                    response.usage,
                    error.to_string(),
                    crate::session::ModelFailureKind::Refusal,
                )
                .await?;
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: requested.sequence,
                    message: None,
                    error: Some(error.to_string()),
                });
                return Err(error);
            }
            let assistant = Message::Assistant(response.blocks);
            if assistant.is_content_free() {
                // The journal is append-only and Anthropic rejects a content-free
                // assistant message on every subsequent request, so committing one
                // would make the session permanently unusable. Fail the turn with
                // nothing committed; an external retry can still continue it.
                let error =
                    if response.stop_reason == crate::provider::protocol::StopReason::Aborted {
                        HarnessError::ProviderAborted
                    } else {
                        HarnessError::EmptyResponse
                    };
                self.record_model_failure(
                    agent,
                    requested.sequence,
                    provider_attempt,
                    response.usage,
                    error.to_string(),
                )
                .await?;
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: requested.sequence,
                    message: None,
                    error: Some(error.to_string()),
                });
                return Err(error);
            }
            // A response with tool calls means the child keeps working, so its
            // progress wakes the owner immediately, as before. A text-only response
            // is this invocation's answer: publish it durably but silently, so the
            // wake comes from the invocation's resolution point instead (the owning
            // job's completion, or an explicit wake wherever the invocation
            // continues). The answer and the completion envelope then reach the
            // owner in one delivery batch, without relying on any wake timing.
            let working = !response.calls.is_empty();
            let origin = if let Some(job) = owner_job {
                self.jobs
                    .commit_child_message(
                        agent,
                        job,
                        assistant.clone(),
                        response.text.clone(),
                        working,
                    )
                    .await?
            } else {
                self.commit(agent, assistant.clone()).await?
            };
            agent_context.projected.push((origin, assistant));
            if response.stop_reason == crate::provider::protocol::StopReason::Aborted {
                // Preserve completed visible/replay content, but never turn a
                // provider abort into a successful agent turn. The failure
                // helper records observed usage exactly once before returning.
                let error = "provider aborted response".to_owned();
                self.record_model_failure(
                    agent,
                    requested.sequence,
                    provider_attempt,
                    response.usage,
                    error.clone(),
                )
                .await?;
                self.events.send(RuntimeEvent::ResponseSettled {
                    agent: agent.clone(),
                    request: requested.sequence,
                    message: Some(origin),
                    error: Some(error),
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
            self.record_model_usage(agent, requested.sequence, response.usage)
                .await?;
            self.record_response_completed(agent, requested.sequence, response.stop_reason.clone())
                .await?;
            agent_context.meter.observe(input_estimate, response.usage);
            // Decide once from the successful completed response, never from an
            // estimate that includes newly produced tool results or queued input.
            let compact_completed_response = agent_context.needs_compaction(response.usage);
            let runtime = prompt::runtime_state_content(
                &self.jobs,
                &self.todos,
                agent,
                capabilities,
                location,
            )
            .await;
            let current = agent_context.request(runtime);
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
                let results = join_all(response.calls.iter().map(|call| {
                    self.execute_call(agent, owner_job, call, origin, location, capabilities)
                }))
                .await;
                let tools = Message::Tool(results);
                let sequence = self.commit(agent, tools.clone()).await?;
                agent_context.projected.push((sequence, tools));
            }
            if compact_completed_response {
                // Checkpoints retain complete tool exchanges. Close the exchange
                // first, then compact before its results reach the next normal
                // model request. Text-only final responses compact here too.
                self.compact_history(
                    &turn,
                    agent_context.provider.as_mut(),
                    context,
                    &current,
                    profile.max_context,
                )
                .await?;
                agent_context.refresh(&self.store.records().await, agent)?;
                context_sequence = None;
            }
            provider_attempt = 0;
            context_failures = 0;
            if response.calls.is_empty() {
                // Notifications arriving during this provider request must be
                // processed before returning an answer based on earlier context.
                // Use the same snapshot/commit/ack boundary for child progress
                // and terminal/question events, including a shared completion batch.
                let (content, messages) = self
                    .pending_event_content(agent, capabilities, location)
                    .await?;
                if !content.is_empty() {
                    messages.commit().await?;
                    final_text.clear();
                    // This invocation continues instead of resolving, so the answer
                    // published silently above has no resolution point to ride on.
                    // Wake the owner here or it would stay pending until the owner's
                    // next unrelated boundary.
                    if let Some(job) = owner_job {
                        self.jobs.notify_owner(job).await;
                    }
                    continue 'requests;
                }
                // A response without tools is still a request boundary. Consume
                // prompts received in flight before completing a one-shot child
                // (or returning a stale final answer to the root caller).
                if self
                    .consume_queued_inputs(&turn, agent_context, model_profile, rx, deferred)
                    .await
                {
                    context_sequence = None;
                    final_text.clear();
                    // Same reasoning: queued input continues this invocation, so the
                    // silently published answer needs its wake now.
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
/// Bounded excerpt of any partial text a refused response produced.
const REFUSAL_EXCERPT_LIMIT: usize = 240;

/// Human-readable refusal detail for the journal and for a parent agent. Providers
/// rarely return refusal prose, so partial visible text is preserved when present.
fn refusal_detail(response: &FoldedResponse) -> String {
    let text = response.text.trim();
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
    // A terminal limit/filter/abort does not authorize execution, even if a
    // backend completed valid arguments before learning the final stop reason.
    if matches!(
        stop_reason,
        crate::provider::protocol::StopReason::MaxTokens
            | crate::provider::protocol::StopReason::ContentFilter
            | crate::provider::protocol::StopReason::Aborted
    ) {
        blocks.retain(|item| {
            !item
                .blocks
                .iter()
                .any(|block| matches!(block.content, BlockContent::ToolCall(_)))
        });
    }
    debug_assert_eq!(usage, final_usage);
    let text = blocks
        .iter()
        .flat_map(|item| &item.blocks)
        .filter_map(|block| match &block.content {
            BlockContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
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

    /// A decoded response that ends in a refusal, as a content filter produces.
    fn refusal(items: Vec<AssistantContent>) -> Vec<ResponseChunk> {
        let mut events = events_for_content(&items);
        events.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::ContentFilter,
        });
        events
    }

    #[tokio::test]
    async fn refusal_fails_the_turn_without_committing_history_or_retrying() {
        // The real shape observed from a refusing provider: a successful stream
        // whose content is empty, or only an unusable reasoning stub.
        for response in [
            refusal(Vec::new()),
            refusal(vec![AssistantContent::reasoning("thought", 0, "", None)]),
        ] {
            let (root, requests, session) = scripted_session([response]).await;
            let failure = session.prompt("Design feedback").await.unwrap_err();
            let message = failure.to_string();
            assert!(
                message.contains("declined to respond") && message.contains("content filter"),
                "unexpected failure: {message}"
            );
            // A refusal is deterministic, so the runtime must not retry it itself.
            assert_eq!(requests.lock().unwrap().len(), 1);
            let sessions = root.path().join("sessions");
            let id = session.id();
            session.shutdown().await.unwrap();
            let records = SessionStore::read_records(&sessions, id).await.unwrap();
            // An error state, never conversation history.
            assert_eq!(
                count!(
                    &records,
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(_)
                    }
                ),
                0
            );
            let refusals = events!(
                &records,
                SessionEvent::ModelFailed { kind, error, .. }
                    if *kind == crate::session::ModelFailureKind::Refusal => error.clone()
            );
            assert_eq!(refusals.len(), 1);
            assert!(refusals[0].contains("declined to respond"));
            // Journaled so a reopened session can still continue the turn.
            let failed = events!(&records, SessionEvent::AgentFailed { error } => error.clone());
            assert_eq!(failed.len(), 1);
            assert!(failed[0].contains("declined to respond"));
            assert_eq!(count!(&records, SessionEvent::ModelRequested { .. }), 1);
            assert_eq!(
                count!(&records, SessionEvent::ModelAttemptStarted { .. }),
                1
            );
            assert_eq!(
                count!(&records, SessionEvent::ModelRecoveryScheduled { .. }),
                0
            );
        }
    }

    #[tokio::test]
    async fn child_refusal_reports_a_failed_agent_to_its_parent() {
        let work = json!({"prompt":"work"});
        let (root, _requests, session) = scripted_session([
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
        let sessions = root.path().join("sessions");
        let id = session.id();
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(&sessions, id).await.unwrap();
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
        assert_eq!(
            count!(
                &child_records,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            ),
            0
        );
    }

    #[tokio::test]
    async fn abnormal_stops_that_strip_their_only_tool_call_fail_instead_of_committing() {
        // MaxTokens and Aborted discard tool calls, which can leave nothing at all.
        // Committing that emptiness was a second, independent brick path.
        for (stop_reason, expected) in [
            (StopReason::MaxTokens, "no assistant content"),
            (StopReason::Aborted, "provider aborted"),
        ] {
            let call = tool_call(0, "call", "shell", json!({"command":"true"}));
            let mut events = events_for_content(&[call]);
            events.push(ResponseChunk::ResponseEnded {
                stop_reason: stop_reason.clone(),
            });
            let (root, _requests, session) = scripted_session([events]).await;
            let failure = session.prompt("run it").await.unwrap_err();
            assert!(
                failure.to_string().contains(expected),
                "unexpected failure for {stop_reason:?}: {failure}"
            );
            let sessions = root.path().join("sessions");
            let id = session.id();
            session.shutdown().await.unwrap();
            let records = SessionStore::read_records(&sessions, id).await.unwrap();
            assert_eq!(
                count!(
                    &records,
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(_)
                    }
                ),
                0
            );
            // The stripped call must never have reached execution.
            assert_eq!(count!(&records, SessionEvent::JobCreated { .. }), 0);
        }
    }

    #[tokio::test]
    async fn blank_text_responses_are_committed_and_complete_the_turn() {
        // Some providers return empty or whitespace text on a non-final turn. That
        // is ordinary content: it encodes, so it must not be treated as a failure.
        for text in ["", "   "] {
            let (root, _requests, session) =
                scripted_session([response(vec![AssistantContent::text("answer", 0, text)])]).await;
            assert_eq!(session.prompt("hello").await.unwrap(), text);
            let sessions = root.path().join("sessions");
            let id = session.id();
            session.shutdown().await.unwrap();
            let records = SessionStore::read_records(&sessions, id).await.unwrap();
            assert_eq!(
                count!(
                    &records,
                    SessionEvent::MessageCommitted {
                        message: Message::Assistant(_)
                    }
                ),
                1
            );
            assert_eq!(count!(&records, SessionEvent::ModelFailed { .. }), 0);
            assert_eq!(count!(&records, SessionEvent::AgentFailed { .. }), 0);
        }
    }

    #[tokio::test]
    async fn content_free_response_fails_instead_of_committing_an_unencodable_message() {
        // Not a refusal, and not merely blank: a response carrying no blocks at all.
        // Committing it would make every later request unencodable, so the turn fails.
        let empty = response(Vec::new());
        let (root, _requests, session) = scripted_session([empty]).await;
        let failure = session.prompt("hello").await.unwrap_err();
        assert!(
            failure.to_string().contains("no assistant content"),
            "unexpected failure: {failure}"
        );
        let sessions = root.path().join("sessions");
        let id = session.id();
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(&sessions, id).await.unwrap();
        assert_eq!(
            count!(
                &records,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            ),
            0
        );
        // Classified as an ordinary error, not a refusal.
        assert_eq!(
            count!(&records, SessionEvent::ModelFailed { kind, .. }
                if *kind == crate::session::ModelFailureKind::Error),
            1
        );
    }

    #[tokio::test]
    async fn successful_responses_journal_their_terminal_stop_reason() {
        let (root, _requests, session) = scripted_session([answer("done")]).await;
        assert_eq!(session.prompt("hello").await.unwrap(), "done");
        let sessions = root.path().join("sessions");
        let id = session.id();
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(&sessions, id).await.unwrap();
        let reasons = events!(
            &records,
            SessionEvent::ResponseCompleted { stop_reason, .. } => stop_reason.clone()
        );
        assert_eq!(reasons, vec![StopReason::EndTurn]);
    }

    #[tokio::test]
    async fn incomplete_native_response_is_not_retried_or_committed() {
        let mut incomplete = answer("must not persist");
        incomplete.truncate(3); // item and block started, item never ended
        let (root, _, session) = scripted_session([incomplete, answer("Recovered")]).await;
        assert!(session.prompt("Question").await.is_err());
        session.shutdown().await.unwrap();
        let sessions = root.path().join("sessions");
        let records = SessionStore::read_records(&sessions, session.id())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(&record.event,
            SessionEvent::ModelFailed { error, .. } if error.contains("response ended before all items ended"))));
        assert_eq!(
            count!(
                &records,
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(_)
                }
            ),
            0
        );
        assert_eq!(count!(&records, SessionEvent::ModelRequested { .. }), 1);
    }

    #[test]
    fn abnormal_termination_never_executes_even_completed_tool_calls() {
        let reasons = [
            StopReason::MaxTokens,
            StopReason::ContentFilter,
            StopReason::Aborted,
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
        let observed = Usage {
            input_tokens: 11,
            cached_input_tokens: 7,
            output_tokens: 3,
        };
        let mut chunks = events_for_content(&items);
        chunks.push(ResponseChunk::UsageUpdated { usage: observed });
        chunks.push(ResponseChunk::ResponseEnded {
            stop_reason: StopReason::Aborted,
        });
        let (root, requests, session) = scripted_session([chunks]).await;
        let mut events = session.runtime.events.subscribe();
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
            match event {
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
}

#[cfg(test)]
mod compaction_tests;

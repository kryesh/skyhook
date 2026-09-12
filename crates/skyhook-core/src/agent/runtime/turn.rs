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
        let mut provider_attempt = 0u8;
        let mut connection_attempt = 1u8;
        let mut compaction_attempt = 1u8;
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
                connection_attempt = 1;
                compaction_attempt = 1;
            }
            if cancellation.is_cancelled() {
                return Err(HarnessError::Interrupted);
            }
            let (content, messages) = self
                .pending_event_content(agent, capabilities, location)
                .await?;
            if !content.is_empty() {
                messages.commit(self, agent, Message::User(content)).await?;
            }
            let profile = agent_context.profile.clone();
            if !profile.supports_images && agent_context.contains_images() {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let template = agent_context.template.clone();
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
            let mut messages = compact::context_sources(&agent_context.projected);
            messages.push(crate::session::ContextMessage::Inline {
                message: request
                    .messages
                    .last()
                    .expect("runtime state is present")
                    .clone(),
            });
            // Freeze the request across connection recovery. Input/notifications and
            // model changes remain queued until the next normal request boundary.
            // Partial streamed output is display-only: only a fully assembled response
            // below is committed and allowed to execute Skyhook tools.
            let input_estimate = compaction::estimate_request(&request);
            let context_tokens = agent_context.meter.estimate(&request);
            self.store.hydrate_model_request(&mut request).await?;
            let (requested, response) = 'attempts: loop {
                if cancellation.is_cancelled() {
                    return Err(HarnessError::Interrupted);
                }
                let requested = self
                    .store
                    .append(
                        agent.clone(),
                        SessionEvent::ModelRequested {
                            context,
                            messages: messages.clone(),
                            purpose: crate::session::ModelPurpose::Agent,
                        },
                    )
                    .await?;
                self.activity(agent, AgentActivity::Working);
                self.events.send(RuntimeEvent::Context {
                    agent: agent.clone(),
                    tokens: context_tokens,
                    capacity: profile.max_context,
                });
                provider_attempt += 1;
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
                        // Only explicitly eligible connection failures may replay an
                        // uncommitted response. Native HTTP startup retries stay transport-owned.
                        if error.recovery().is_some() {
                            self.schedule_model_recovery(
                                &turn,
                                requested.sequence,
                                provider_attempt,
                                connection_attempt,
                                &error,
                                agent_context.provider.as_mut(),
                            )
                            .await?;
                            connection_attempt += 1;
                            continue 'attempts;
                        }
                        if provider_attempt < MAX_MODEL_ATTEMPTS
                            && compaction_attempt < compact::MAX_PROVIDER_ATTEMPTS
                            && error.kind
                                == crate::provider::ProviderErrorKind::ContextWindowExceeded
                        {
                            force_compaction = true;
                            compaction_attempt += 1;
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
                                // The failed stream may own the connection mutex. Drop it
                                // before resetting or waiting. No assistant message or tools
                                // from this attempt have been committed/executed.
                                drop(response);
                                self.schedule_model_recovery(
                                    &turn,
                                    requested.sequence,
                                    provider_attempt,
                                    connection_attempt,
                                    &error,
                                    agent_context.provider.as_mut(),
                                )
                                .await?;
                                connection_attempt += 1;
                                continue 'attempts;
                            }
                            if !saw_content
                                && provider_attempt < MAX_MODEL_ATTEMPTS
                                && compaction_attempt < compact::MAX_PROVIDER_ATTEMPTS
                                && error.kind
                                    == crate::provider::ProviderErrorKind::ContextWindowExceeded
                            {
                                force_compaction = true;
                                compaction_attempt += 1;
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
            let assistant = Message::Assistant(response.blocks);
            let origin = if let Some(job) = owner_job {
                self.jobs
                    .commit_child_message(agent, job, assistant.clone(), response.text.clone())
                    .await?
            } else {
                self.commit(agent, assistant.clone()).await?
            };
            agent_context.projected.push((origin, assistant));
            if response.stop_reason == crate::provider::protocol::StopReason::Aborted {
                // Preserve completed visible/replay content, but never turn a
                // provider cancellation into a successful agent turn. The failure
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
                return Err(HarnessError::Interrupted);
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
                    .prepare_question_batch(agent, &response.calls)
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
            connection_attempt = 1;
            compaction_attempt = 1;
            if response.calls.is_empty() {
                // Notifications arriving during this provider request must be
                // processed before returning an answer based on earlier context.
                // Use the same snapshot/commit/ack boundary for child progress
                // and terminal/question events, including a shared completion batch.
                let (content, messages) = self
                    .pending_event_content(agent, capabilities, location)
                    .await?;
                if !content.is_empty() {
                    messages.commit(self, agent, Message::User(content)).await?;
                    final_text.clear();
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

    #[tokio::test]
    async fn incomplete_native_response_is_not_retried_or_committed() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            Arc::new(ScriptedProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                responses: StdMutex::new(VecDeque::from([
                    vec![
                        ResponseChunk::ItemStarted {
                            id: "answer".into(),
                            position: 0,
                            kind: ItemKind::Text,
                        },
                        ResponseChunk::BlockStarted {
                            item: "answer".into(),
                            id: "answer:0".into(),
                            position: 0,
                            kind: crate::provider::protocol::BlockKind::Text,
                        },
                        ResponseChunk::BlockDelta {
                            item: "answer".into(),
                            block: "answer:0".into(),
                            delta: ContentDelta::Text("must not persist".into()),
                        },
                    ],
                    answer("Recovered"),
                ]))
                .into(),
            }),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        assert!(session.prompt("Question").await.is_err());
        session.shutdown().await.unwrap();
        let records = SessionStore::read_records(sessions.path(), session.id())
            .await
            .unwrap();
        assert!(records.iter().any(|record| matches!(&record.event,
            SessionEvent::ModelFailed { error, .. } if error.contains("response ended before all items ended"))));
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(blocks),
                } => Some(blocks),
                _ => None,
            })
            .collect();
        assert!(assistants.is_empty());
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::ModelRequested { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn abnormal_termination_never_executes_even_completed_tool_calls() {
        for reason in [
            StopReason::MaxTokens,
            StopReason::ContentFilter,
            StopReason::Aborted,
        ] {
            let mut assembler = ResponseAssembler::default();
            let items = vec![
                AssistantContent::text("answer", 0, "Visible response"),
                AssistantContent::tool_call(
                    "tool",
                    1,
                    ToolCall {
                        id: "call".into(),
                        name: "shell".into(),
                        arguments: json!({"command":"unsafe"}),
                    },
                ),
            ];
            for event in crate::provider::protocol::events_for_content(&items) {
                assembler.push(&event).unwrap();
            }
            assembler
                .push(&ResponseChunk::ResponseEnded {
                    stop_reason: reason,
                })
                .unwrap();
            let folded = finish_response(assembler, Usage::default()).unwrap();
            assert_eq!(folded.text, "Visible response");
            assert!(folded.calls.is_empty());
            assert_eq!(folded.blocks.len(), 1);
        }
    }

    #[tokio::test]
    async fn aborted_response_preserves_visible_content_and_usage_but_settles_as_interrupted() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let retained = vec![
            AssistantContent::reasoning(
                "reason",
                0,
                "completed reasoning",
                Some(replay(json!({"encrypted_content":"retained"}))),
            ),
            AssistantContent::text("answer", 1, "partial visible answer"),
        ];
        let mut items = retained.clone();
        items.push(AssistantContent::tool_call(
            "tool",
            2,
            ToolCall {
                id: "call".into(),
                name: "write".into(),
                arguments: json!({"path":"must-not-exist", "content":"unsafe"}),
            },
        ));
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
        let harness = test_harness(
            workspace.path(),
            sessions.path(),
            scripted_provider(&requests, [chunks]),
        )
        .await;
        let session = harness.new_session().await.unwrap();
        let mut events = session.runtime.events.subscribe();
        let error = session.prompt("Abort this turn.").await.unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(session.usage().await, observed);
        let records = session.runtime.store.records().await;
        let assistants: Vec<_> = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted {
                    message: Message::Assistant(items),
                } => Some(items.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(assistants, vec![retained]);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.event, SessionEvent::Usage { .. }))
                .count(),
            1
        );
        assert!(records.iter().any(|record| matches!(&record.event, SessionEvent::ModelFailed { error, .. } if error == "provider aborted response")));
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.event, SessionEvent::JobCreated { .. }))
        );
        assert!(!workspace.path().join("must-not-exist").exists());
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

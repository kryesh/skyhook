//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use std::collections::BTreeSet;

use futures_util::StreamExt;

use super::{HarnessError, SessionRuntime, TurnContext, compaction, prompt};
use crate::{
    identity::{AgentId, JobId},
    provider::{
        Provider,
        protocol::{AssistantContent, Message, ModelRequest, ResponseChunk, ResponseSchema, Usage},
    },
    session::{
        CompactionCheckpoint, ContextMessage, EventRecord, ModelCallOrigin, ModelPurpose,
        SessionEvent, project_history,
    },
};

pub(super) const MAX_PROVIDER_ATTEMPTS: u8 = 3;

pub(super) async fn retry_delay(
    cancellation: &crate::job::CancellationToken,
    attempt: u8,
) -> Result<(), HarnessError> {
    tokio::select! {
        () = cancellation.cancelled() => Err(HarnessError::Interrupted),
        () = tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt))) => Ok(()),
    }
}

/// Calibrate the next estimate against the last successful request with the same template.
#[derive(Default)]
pub(super) struct TokenMeter {
    baseline: Option<(u64, u64)>,
}

impl TokenMeter {
    pub(super) fn restore(
        records: &[EventRecord],
        agent: &AgentId,
        template: &ModelRequest,
    ) -> Self {
        let mut meter = Self::default();
        for record in records.iter().rev().filter(|record| &record.agent == agent) {
            let SessionEvent::Usage {
                request: Some(request),
                usage,
            } = &record.event
            else {
                continue;
            };
            let Some(request_event) = records.iter().find(|record| record.sequence == *request)
            else {
                continue;
            };
            let SessionEvent::ModelRequested {
                context,
                purpose: ModelPurpose::Agent,
                ..
            } = &request_event.event
            else {
                continue;
            };
            let same_template = records.iter().any(|record| record.sequence == *context && matches!(&record.event, SessionEvent::ModelContext { template: original, .. } if original == template));
            if same_template
                && let Ok((_, request)) =
                    crate::session::reconstruct_model_request(records, *request)
            {
                meter.observe(compaction::estimate_request(&request), *usage);
            }
            break;
        }
        meter
    }

    pub(super) fn estimate(&self, request: &ModelRequest) -> u64 {
        let estimate = compaction::estimate_request(request);
        self.baseline.map_or(estimate, |(old_estimate, actual)| {
            estimate.max(actual.saturating_add(estimate).saturating_sub(old_estimate))
        })
    }

    pub(super) fn observe(&mut self, estimate: u64, usage: Usage) {
        let actual = usage.input_tokens.saturating_add(usage.cached_input_tokens);
        if actual > 0 {
            self.baseline = Some((estimate, actual));
        }
    }
}

pub(super) fn context_sources(projected: &[(u64, Message)]) -> Vec<ContextMessage> {
    projected
        .iter()
        .map(|(sequence, _)| ContextMessage::Source {
            sequence: *sequence,
        })
        .collect()
}

/// Select complete original exchanges, including creators no longer in the visible tail.
fn retained_sources(
    records: &[EventRecord],
    agent: &AgentId,
    projected: &[(u64, Message)],
    origins: &[ModelCallOrigin],
) -> Result<Vec<u64>, HarnessError> {
    let originals: Vec<_> = records
        .iter()
        .filter_map(|record| {
            if &record.agent != agent {
                return None;
            }
            match &record.event {
                SessionEvent::MessageCommitted { message } => Some((record.sequence, message)),
                _ => None,
            }
        })
        .collect();
    let original_ids: BTreeSet<_> = originals.iter().map(|(id, _)| *id).collect();
    let visible: Vec<_> = projected
        .iter()
        .filter(|(id, _)| original_ids.contains(id))
        .collect();
    let mut retained = BTreeSet::new();
    let mut total = 0u64;
    let mut start = visible.len();
    for (index, (_, message)) in visible.iter().enumerate().rev() {
        let tokens = compaction::estimate_message(message);
        if total > 0 && total.saturating_add(tokens) > 8_000 {
            break;
        }
        total = total.saturating_add(tokens);
        start = index;
    }
    if start < visible.len() && matches!(visible[start].1, Message::Tool(_)) {
        start = start.saturating_sub(1);
    }
    retained.extend(visible[start..].iter().map(|(sequence, _)| *sequence));
    for origin in origins {
        let index = originals
            .iter()
            .position(|(id, _)| *id == origin.message)
            .ok_or_else(|| {
                HarnessError::Compaction("active job creator message is missing".into())
            })?;
        let Message::Assistant(blocks) = originals[index].1 else {
            return Err(HarnessError::Compaction(
                "active job creator is not an assistant message".into(),
            ));
        };
        if !blocks.iter().any(
            |block| matches!(block, AssistantContent::ToolCall(call) if call.id == origin.call_id),
        ) {
            return Err(HarnessError::Compaction(
                "active job creator call is missing".into(),
            ));
        }
        let Some((result_sequence, Message::Tool(results))) = originals.get(index + 1) else {
            return Err(HarnessError::Compaction(
                "active job creator has no complete result exchange".into(),
            ));
        };
        for call in blocks.iter().filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        }) {
            if !results.iter().any(|result| result.call_id == call.id) {
                return Err(HarnessError::Compaction(
                    "active job exchange has an unmatched tool call".into(),
                ));
            }
        }
        retained.insert(origin.message);
        retained.insert(*result_sequence);
    }
    Ok(retained.into_iter().collect())
}

impl SessionRuntime {
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &dyn Provider,
        context: u64,
        input: &ModelRequest,
    ) -> Result<(), HarnessError> {
        let mut launches = self.jobs.active_launches(turn.agent).await;
        for attempt in 1..=MAX_PROVIDER_ATTEMPTS {
            let mut request_sequence = None;
            match self
                .compact_inner(
                    turn,
                    provider,
                    context,
                    input,
                    &mut request_sequence,
                    &mut launches,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    self.store
                        .append(
                            turn.agent.clone(),
                            SessionEvent::CompactionFailed {
                                request: request_sequence,
                                error: format!(
                                    "attempt {attempt}/{MAX_PROVIDER_ATTEMPTS}: {error}"
                                ),
                            },
                        )
                        .await?;
                    let retryable = request_sequence.is_some()
                        && matches!(
                            &error,
                            HarnessError::Provider(_) | HarnessError::Compaction(_)
                        );
                    if !retryable || attempt == MAX_PROVIDER_ATTEMPTS {
                        return Err(error);
                    }
                    retry_delay(turn.cancellation, attempt).await?;
                }
            }
        }
        unreachable!("bounded attempts return a result")
    }

    async fn compact_inner(
        &self,
        turn: &TurnContext<'_>,
        provider: &dyn Provider,
        context: u64,
        input: &ModelRequest,
        request_sequence: &mut Option<u64>,
        launches: &mut Vec<(JobId, Option<ModelCallOrigin>)>,
    ) -> Result<(), HarnessError> {
        let agent = turn.agent;
        let records = self.store.records().await;
        let frontier = records.last().map_or(0, |record| record.sequence);
        let projected = project_history(&records, agent)?;
        let previous = records.iter().rev().find_map(|record| {
            (&record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. }))
                .then_some(record.sequence)
        });
        for launch in self.jobs.active_launches(agent).await {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        // Retry against current history and runtime state, including any todo
        // changes that invalidated an earlier attempt's snapshot.
        let mut input = input.clone();
        input.messages = projected
            .iter()
            .map(|(_, message)| message.clone())
            .collect();
        input.messages.push(Message::User(vec![
            prompt::runtime_state_content(&self.jobs, &self.todos, agent, turn.capabilities).await,
        ]));
        let directive = compaction::directive();
        let mut summary_request = input.clone();
        // Summarization cannot execute tools. Keep their historical calls/results
        // as evidence, but advertise no callable tools on this request.
        summary_request.tools.clear();
        summary_request.response_schema = Some(ResponseSchema {
            name: "skyhook_compaction".into(),
            schema: compaction::response_schema(),
        });
        summary_request.messages.push(directive.clone());
        let mut messages = context_sources(&projected);
        // input ends in the exact transient runtime state used for this attempt.
        messages.push(ContextMessage::Inline {
            message: input
                .messages
                .last()
                .expect("runtime state is present")
                .clone(),
        });
        messages.push(ContextMessage::Inline { message: directive });
        let provider_name = records
            .iter()
            .find_map(|record| {
                if record.sequence == context
                    && &record.agent == agent
                    && let SessionEvent::ModelContext { provider, .. } = &record.event
                {
                    Some(provider.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| HarnessError::Compaction("model context is missing".into()))?;
        let mut template = summary_request.clone();
        template.messages.clear();
        let summary_context = self
            .store
            .append(
                agent.clone(),
                SessionEvent::ModelContext {
                    provider: provider_name,
                    template,
                },
            )
            .await?;
        let requested = self
            .store
            .append(
                agent.clone(),
                SessionEvent::ModelRequested {
                    context: summary_context.sequence,
                    messages,
                    purpose: ModelPurpose::Compaction,
                },
            )
            .await?;
        *request_sequence = Some(requested.sequence);
        self.store
            .hydrate_model_request(&mut summary_request)
            .await?;
        let mut stream = tokio::select! {
            result = provider.invoke(summary_request) => result?,
            () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
        };
        let mut blocks = Vec::new();
        let mut streamed = String::new();
        let mut usage = Usage::default();
        let mut truncated = false;
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    if usage != Usage::default() {
                        self.record_model_usage(agent, requested.sequence, usage)
                            .await?;
                    }
                    return Err(error.into());
                }
            };
            match chunk {
                ResponseChunk::TextDelta { text } => streamed.push_str(&text),
                ResponseChunk::Block { block } => blocks.push(block),
                ResponseChunk::Usage { usage: value } => usage = value,
                ResponseChunk::Finished { truncated: value } => truncated |= value,
                ResponseChunk::ReasoningDelta { .. } => {}
            }
        }
        self.record_model_usage(agent, requested.sequence, usage)
            .await?;
        if truncated {
            return Err(HarnessError::Compaction(
                "summarization was truncated; original history is retained".into(),
            ));
        }
        if blocks
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_)))
        {
            return Err(HarnessError::Compaction(
                "summarizer returned a tool call; no tools were executed".into(),
            ));
        }
        let text: String = blocks
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = if text.is_empty() { streamed } else { text };
        let continuation = compaction::continuation(&text).map_err(HarnessError::Compaction)?;
        let mut message = continuation.message;
        for launch in self.jobs.active_launches(agent).await {
            if !launches.contains(&launch) {
                launches.push(launch);
            }
        }
        let origins: Vec<_> = launches
            .iter()
            .filter_map(|(_, origin)| origin.clone())
            .collect();
        let retained = retained_sources(&records, agent, &projected, &origins)?;
        // Host API calls have no original assistant exchange. Preserve their actual launch facts.
        let host_jobs: BTreeSet<_> = launches
            .iter()
            .filter_map(|(job, origin)| origin.is_none().then_some(*job))
            .collect();
        if !host_jobs.is_empty() {
            let latest = self.store.records().await;
            let facts: Vec<_> = latest.iter().filter(|record| &record.agent == agent && matches!(&record.event, SessionEvent::JobCreated {job, ..} if host_jobs.contains(job)))
                .map(|record| serde_json::json!({"source_event": record.sequence, "launch": record.event})).collect();
            if let Message::User(blocks) = &mut message {
                blocks.push(crate::provider::protocol::UserContent::Compaction { text: format!("Previously started host work; these are launch facts, not requests to launch again. Current runtime state governs job status.\n{}", serde_json::to_string(&facts).map_err(|error| HarnessError::Compaction(error.to_string()))?) });
            }
        }
        let mut compacted = input.clone();
        compacted.messages = vec![message.clone()];
        for sequence in &retained {
            let message = records
                .iter()
                .find_map(|record| {
                    if record.sequence != *sequence {
                        return None;
                    }
                    match &record.event {
                        SessionEvent::MessageCommitted { message } => Some(message.clone()),
                        _ => None,
                    }
                })
                .expect("retention references validated original messages");
            compacted.messages.push(message);
        }
        let runtime = prompt::runtime_state_with_todos(
            &self.jobs,
            agent,
            turn.capabilities,
            continuation.todos.clone(),
        )
        .await;
        compacted.messages.push(Message::User(vec![runtime]));
        let before_tokens = compaction::estimate_request(&input);
        let after_tokens = compaction::estimate_request(&compacted);
        if after_tokens >= before_tokens {
            self.store.append(agent.clone(), SessionEvent::CompactionSkipped {
                request: requested.sequence,
                reason: "continuation and retained messages do not reduce context; continuing with original history".into(),
            }).await?;
            return Ok(());
        }
        if turn.cancellation.is_cancelled() {
            return Err(HarnessError::Interrupted);
        }
        if !self
            .todos
            .commit_compaction(
                agent,
                CompactionCheckpoint {
                    schema_version: compaction::SCHEMA_VERSION,
                    previous,
                    frontier,
                    message,
                    todos: continuation.todos,
                    retained,
                    request: requested.sequence,
                    before_tokens,
                    after_tokens,
                    max_context: turn.profile.max_context,
                },
            )
            .await?
        {
            return Err(HarnessError::Compaction(
                "todo state changed during summarization; retrying with current state".into(),
            ));
        }
        Ok(())
    }
}

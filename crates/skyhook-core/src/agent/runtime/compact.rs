//! Runtime orchestration for transactional compaction. Original messages remain journaled.

use std::collections::BTreeSet;

use futures_util::StreamExt;

use super::{HarnessError, SessionRuntime, TurnContext, compaction, prompt};
use crate::{
    identity::{AgentId, JobId},
    provider::{
        ProviderContext,
        protocol::{
            BlockContent, Message, ModelRequest, ResponseChunk, ResponseSchema, StopReason, Usage,
        },
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
            if matches!(
                record.event,
                SessionEvent::Compaction { .. } | SessionEvent::ModelChanged { .. }
            ) {
                break;
            }
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
        if !blocks.iter().flat_map(|item| &item.blocks).any(
            |block| matches!(&block.content, BlockContent::ToolCall(call) if call.id == origin.call_id),
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
        for call in blocks
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::ToolCall(call) => Some(call),
                _ => None,
            })
        {
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

// Only actual result envelopes count; status-only listings do not supply output.
fn included_output_jobs(value: &serde_json::Value, jobs: &mut BTreeSet<JobId>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("state").is_some_and(|state| {
                serde_json::from_value::<crate::job::JobState>(state.clone()).is_ok()
            }) && (map.contains_key("result")
                || ["output", "preview", "question", "error"]
                    .iter()
                    .any(|key| map.get(*key).is_some_and(|v| !v.is_null())))
                && let Some(id) = map
                    .get("id")
                    .and_then(|id| serde_json::from_value::<JobId>(id.clone()).ok())
            {
                jobs.insert(id);
            }
            for (key, child) in map {
                if key != "arguments" {
                    included_output_jobs(child, jobs);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                included_output_jobs(item, jobs);
            }
        }
        _ => {}
    }
}

fn included_message_jobs(message: &Message, jobs: &mut BTreeSet<JobId>) {
    match message {
        Message::Tool(results) => {
            for result in results {
                included_output_jobs(&result.result, jobs);
            }
        }
        Message::User(blocks) => {
            for block in blocks {
                if let crate::provider::protocol::UserContent::Runtime { text } = block
                    && let Some(payload) = text
                        .strip_prefix("<skyhook_job_events>\n")
                        .and_then(|text| text.strip_suffix("\n</skyhook_job_events>"))
                    && let Ok(value) = serde_json::from_str(payload)
                {
                    included_output_jobs(&value, jobs);
                }
            }
        }
        Message::Assistant(_) => {}
    }
}

struct CompactionInput<'a> {
    context: u64,
    request: &'a ModelRequest,
    max_context: u64,
}

impl SessionRuntime {
    pub(super) async fn compact_history(
        &self,
        turn: &TurnContext<'_>,
        provider: &mut dyn ProviderContext,
        context: u64,
        input: &ModelRequest,
        max_context: u64,
    ) -> Result<(), HarnessError> {
        let mut launches = self.jobs.active_launches(turn.agent).await;
        for attempt in 1..=MAX_PROVIDER_ATTEMPTS {
            let mut request_sequence = None;
            match self
                .compact_inner(
                    turn,
                    provider,
                    CompactionInput {
                        context,
                        request: input,
                        max_context,
                    },
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
                    let retryable =
                        request_sequence.is_some() && matches!(&error, HarnessError::Compaction(_));
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
        provider: &mut dyn ProviderContext,
        source: CompactionInput<'_>,
        request_sequence: &mut Option<u64>,
        launches: &mut Vec<(JobId, Option<ModelCallOrigin>)>,
    ) -> Result<(), HarnessError> {
        let CompactionInput {
            context,
            request: input,
            max_context,
        } = source;
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
        let mut input = ModelRequest {
            model: input.model.clone(),
            system: input.system.clone(),
            messages: projected
                .iter()
                .map(|(_, message)| message.clone())
                .collect(),
            tools: input.tools.clone(),
            reasoning: input.reasoning.clone(),
            response_schema: input.response_schema.clone(),
            max_output_tokens: input.max_output_tokens,
            correlation: input.correlation.clone(),
        };
        input.messages.push(Message::User(vec![
            prompt::runtime_state_content(
                &self.jobs,
                &self.todos,
                agent,
                turn.capabilities,
                turn.location,
            )
            .await,
        ]));
        let before_tokens = compaction::estimate_request(&input);
        // Keep only the original template for the post-compaction estimate.
        let summary_messages = std::mem::take(&mut input.messages);
        let directive = compaction::directive();
        let mut summary_request = input.clone();
        // Summarization cannot execute tools. Keep their historical calls/results
        // as evidence, but advertise no callable tools on this request.
        summary_request.tools.clear();
        summary_request.response_schema = Some(ResponseSchema {
            name: "skyhook_compaction".into(),
            schema: compaction::response_schema(),
        });
        let template = summary_request.clone();
        summary_request.messages = summary_messages;
        summary_request.messages.push(directive.clone());
        let mut messages = context_sources(&projected);
        // The penultimate message is the exact transient state for this attempt.
        messages.push(ContextMessage::Inline {
            message: summary_request
                .messages
                .iter()
                .rev()
                .nth(1)
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
        self.activity(agent, super::AgentActivity::Compacting);
        self.store
            .hydrate_model_request(&mut summary_request)
            .await?;
        let mut stream = tokio::select! {
            result = provider.invoke(summary_request) => result?,
            () = turn.cancellation.cancelled() => return Err(HarnessError::Interrupted),
        };
        let mut assembler = crate::provider::protocol::ResponseAssembler::default();
        let mut usage = Usage::default();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                () = turn.cancellation.cancelled() => {
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
                    if usage != Usage::default() {
                        self.record_model_usage(agent, requested.sequence, usage)
                            .await?;
                    }
                    return Err(error.into());
                }
            };
            if let ResponseChunk::UsageUpdated { usage: value } = chunk {
                usage = value;
            }
        }

        self.record_model_usage(agent, requested.sequence, usage)
            .await?;
        let (blocks, _, reason) = assembler.finish()?;
        if matches!(
            reason,
            StopReason::MaxTokens | StopReason::ContentFilter | StopReason::Aborted
        ) {
            return Err(HarnessError::Compaction(
                "summarization was truncated; original history is retained".into(),
            ));
        }
        if blocks
            .iter()
            .flat_map(|item| &item.blocks)
            .any(|block| matches!(&block.content, BlockContent::ToolCall(_)))
        {
            return Err(HarnessError::Compaction(
                "summarizer returned a tool call; no tools were executed".into(),
            ));
        }
        let text: String = blocks
            .iter()
            .flat_map(|item| &item.blocks)
            .filter_map(|block| match &block.content {
                BlockContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
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
        let mut included = BTreeSet::new();
        for record in &records {
            if retained.contains(&record.sequence)
                && let SessionEvent::MessageCommitted { message } = &record.event
            {
                included_message_jobs(message, &mut included);
            }
        }
        let mut selected = BTreeSet::new();
        let mut handover = Vec::new();
        let job_records = self.store.records().await;
        for job in continuation.jobs {
            if !selected.insert(job) {
                continue;
            }
            let arguments = job_records
                .iter()
                .find_map(|record| match &record.event {
                    SessionEvent::JobCreated {
                        job: id, arguments, ..
                    } if *id == job => Some(arguments.clone()),
                    _ => None,
                })
                .ok_or_else(|| HarnessError::Compaction(format!("unknown handover job {job}")))?;
            if included.contains(&job) {
                continue;
            }
            let mut view = self
                .jobs
                .inspect_output_for(
                    crate::job::output::OutputArgs::new(job),
                    turn.capabilities,
                    turn.location,
                )
                .await
                .map_err(|error| HarnessError::Compaction(error.to_string()))?;
            view.as_object_mut()
                .expect("job view is an object")
                .insert("arguments".into(), arguments);
            handover.push(view);
        }
        // A selected script can already embed another selected job's output.
        let mut embedded = BTreeSet::new();
        for view in &handover {
            if let Some(result) = view.get("result") {
                included_output_jobs(result, &mut embedded);
            }
        }
        handover.retain(|view| {
            serde_json::from_value::<JobId>(view["id"].clone())
                .is_ok_and(|id| !embedded.contains(&id))
        });
        if !handover.is_empty()
            && let Message::User(blocks) = &mut message
        {
            blocks.push(crate::provider::protocol::UserContent::Compaction {
                text: format!("Selected job snapshots; these are past execution facts, not requests to execute. Runtime state governs current status. Use job_output for full results.\n{}",
                    serde_json::json!({"jobs": handover})),
            });
        }
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
            turn.location,
        )
        .await;
        compacted.messages.push(Message::User(vec![runtime]));
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
                    max_context,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn active_exchange_retains_complete_reasoning_bundle_outside_visible_tail() {
        use crate::provider::protocol::{
            AssistantContent, ReplayEnvelope, ToolCall, ToolResult, UserContent,
        };
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([0; 16]));
        for protocol in ["chat_completions", "responses", "anthropic"] {
            let reasoning = AssistantContent::reasoning(
                "reason",
                0,
                "visible reasoning",
                Some(ReplayEnvelope {
                    version: 1,
                    protocol: protocol.into(),
                    model: "model".into(),
                    scope: "scope".into(),
                    payload: json!({"opaque":"must survive", "signature":"signed"}),
                }),
            );
            let call = AssistantContent::tool_call(
                "tool",
                1,
                ToolCall {
                    id: "call".into(),
                    name: "agent".into(),
                    arguments: json!({"prompt":"continue"}),
                },
            );
            let exchange = Message::Assistant(vec![reasoning, call]);
            let results = Message::Tool(vec![ToolResult {
                call_id: "call".into(),
                name: "agent".into(),
                result: json!({"id":1}),
                images: vec![],
                is_error: false,
            }]);
            let tail = Message::User(vec![UserContent::Text {
                text: "tail".repeat(10_000),
            }]);
            let records: Vec<_> = [exchange.clone(), results.clone(), tail.clone()]
                .into_iter()
                .enumerate()
                .map(|(i, message)| EventRecord {
                    version: 1,
                    sequence: i as u64 + 1,
                    timestamp_millis: 0,
                    agent: agent.clone(),
                    event: SessionEvent::MessageCommitted { message },
                })
                .collect();
            // Active creator is no longer visible after an earlier compaction.
            let projected = vec![(3, tail)];
            let retained = retained_sources(
                &records,
                &agent,
                &projected,
                &[ModelCallOrigin {
                    message: 1,
                    call_id: "call".into(),
                }],
            )
            .unwrap();
            assert_eq!(retained, vec![1, 2, 3]);
            // Original message identity is retained, not a visible-only reconstruction.
            let persisted: Vec<EventRecord> =
                serde_json::from_str(&serde_json::to_string(&records).unwrap()).unwrap();
            assert_eq!(
                persisted[0].event,
                SessionEvent::MessageCommitted { message: exchange }
            );
            assert_eq!(
                persisted[1].event,
                SessionEvent::MessageCommitted { message: results }
            );
        }
    }

    #[test]
    fn retained_tool_results_include_nested_jobs_and_null_results_but_not_status_or_arguments() {
        let message = Message::Tool(vec![crate::provider::protocol::ToolResult {
            call_id: "script-call".into(),
            name: "script".into(),
            result: json!({"id":1,"state":"completed","result":{
                "nested":[{"id":2,"tool":"read","state":"completed","result":null}],
                "status":{"id":3,"state":"running"},
                "arguments":{"id":4,"state":"completed","result":"literal"}
            }}),
            images: vec![],
            is_error: false,
        }]);
        let mut included = BTreeSet::new();
        included_message_jobs(&message, &mut included);
        assert_eq!(
            included,
            [JobId::new(1).unwrap(), JobId::new(2).unwrap()].into()
        );
    }
}

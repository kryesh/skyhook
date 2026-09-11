//! Preserve complete tool exchanges and identify outputs already carried by history.

use super::{HarnessError, compaction};
use crate::{
    identity::{AgentId, JobId},
    provider::protocol::{BlockContent, Message},
    session::{EventRecord, ModelCallOrigin, SessionEvent},
};
use std::collections::BTreeSet;

/// Select complete original exchanges, including creators no longer in the visible tail.
pub(super) fn retained_sources(
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
pub(super) fn included_output_jobs(value: &serde_json::Value, jobs: &mut BTreeSet<JobId>) {
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

pub(super) fn included_message_jobs(message: &Message, jobs: &mut BTreeSet<JobId>) {
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

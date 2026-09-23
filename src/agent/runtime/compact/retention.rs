//! Preserve complete tool exchanges and identify outputs already carried by history.

use super::{HarnessError, compaction};
use crate::{
    identity::{AgentId, JobId},
    provider::protocol::Message,
    session::{EventRecord, ModelCallOrigin, SessionEvent},
};
use std::collections::BTreeSet;

/// An original message selected from one immutable journal snapshot.
/// Only retention can construct the sequence/payload pair; no payload is cloned.
pub(super) struct RetainedSource<'a> {
    sequence: u64,
    message: &'a Message,
}

impl RetainedSource<'_> {
    pub(super) fn message(&self) -> &Message {
        self.message
    }

    pub(super) fn into_sequence(self) -> u64 {
        self.sequence
    }
}

/// The verbatim tail's share of the context, so a small window still shrinks.
pub(in crate::agent::runtime) fn retention_budget(max_context: u64) -> u64 {
    (max_context / 16).min(8_000)
}

/// Select complete original exchanges, including creators no longer in the visible tail.
/// The tail is what `model` would be sent of it after compaction, within `budget` tokens.
pub(super) fn retained_sources<'a>(
    records: &'a [EventRecord],
    agent: &AgentId,
    projected: &[(u64, Message)],
    origins: &[ModelCallOrigin],
    model: &str,
    budget: u64,
) -> Result<Vec<RetainedSource<'a>>, HarnessError> {
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
        let kept = message.clone().without_bound_reasoning();
        let tokens = kept.map_or(0, |kept| compaction::estimate_message(&kept, model));
        if total > 0 && total.saturating_add(tokens) > budget {
            break;
        }
        total = total.saturating_add(tokens);
        start = index;
    }
    // Per-call results follow their assistant call; keep the whole exchange.
    while start < visible.len() && start > 0 && matches!(visible[start].1, Message::Tool(_)) {
        start -= 1;
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
        if !blocks
            .iter()
            .filter_map(|item| item.call())
            .any(|call| call.id() == origin.call_id)
        {
            return Err(HarnessError::Compaction(
                "active job creator call is missing".into(),
            ));
        }
        let exchange: Vec<_> = originals[index + 1..]
            .iter()
            .take_while(|(_, message)| matches!(message, Message::Tool(_)))
            .collect();
        if exchange.is_empty() {
            return Err(HarnessError::Compaction(
                "active job creator has no complete result exchange".into(),
            ));
        }
        let results: Vec<_> = exchange
            .iter()
            .flat_map(|(_, message)| match message {
                Message::Tool(results) => results.as_slice(),
                _ => &[],
            })
            .collect();
        for call in blocks.iter().filter_map(|item| item.call()) {
            if !results.iter().any(|result| result.call_id == call.id()) {
                return Err(HarnessError::Compaction(
                    "active job exchange has an unmatched tool call".into(),
                ));
            }
        }
        retained.insert(origin.message);
        retained.extend(exchange.iter().map(|(sequence, _)| *sequence));
    }
    let mut sources: Vec<_> = originals
        .into_iter()
        .filter(|(sequence, _)| retained.contains(sequence))
        .map(|(sequence, message)| RetainedSource { sequence, message })
        .collect();
    sources.sort_unstable_by_key(|source| source.sequence);
    Ok(sources)
}

// Only actual result envelopes count; status-only listings do not supply output.
pub(super) fn included_output_jobs(value: &serde_json::Value, jobs: &mut BTreeSet<JobId>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("state").is_some_and(|state| {
                serde_json::from_value::<crate::job::JobState>(state.clone()).is_ok()
            }) && (map.get("has_result") == Some(&serde_json::Value::Bool(true))
                || map.get("error").is_some_and(|error| !error.is_null())
                || map.get("presentation").is_some_and(|presentation| {
                    ["preview", "question"]
                        .iter()
                        .any(|key| presentation.get(*key).is_some_and(|value| !value.is_null()))
                }))
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
    use crate::provider::protocol::{
        AssistantItem, Binding, Provenance, Replay, Scope, ToolCall, ToolResult, UserContent,
    };
    use serde_json::json;

    fn committed(agent: &AgentId, messages: impl IntoIterator<Item = Message>) -> Vec<EventRecord> {
        let records = messages
            .into_iter()
            .enumerate()
            .map(|(i, message)| EventRecord {
                id: crate::identity::EventId::generate().unwrap(),
                sequence: i as u64 + 1,
                timestamp_millis: 0,
                agent: agent.clone(),
                event: SessionEvent::MessageCommitted { message },
            });
        records.collect()
    }

    fn user(text: String) -> Message {
        Message::User(vec![UserContent::Text { text }])
    }

    #[test]
    fn active_exchange_retains_complete_reasoning_bundle_outside_visible_tail() {
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([0; 16]));
        for protocol in ["chat_completions", "responses", "anthropic"] {
            let replay = Replay {
                provenance: Provenance {
                    protocol: protocol.into(),
                    model: "model".into(),
                    scope: Scope::try_from("scope".to_owned()).unwrap(),
                },
                payload: json!({"opaque":"must survive", "signature":"signed"}),
                binding: Binding::Free,
            };
            let reasoning =
                AssistantItem::reasoning("reason", 0, "visible reasoning", Some(replay));
            let call = ToolCall::new("call", "agent", json!({"prompt":"continue"})).unwrap();
            let exchange =
                Message::Assistant(vec![reasoning, AssistantItem::tool_call("tool", 1, call)]);
            let results = Message::Tool(vec![ToolResult {
                call_id: "call".into(),
                name: "agent".into(),
                result: json!({"id":1}),
                images: vec![],
                is_error: false,
            }]);
            let tail = user("tail".repeat(10_000));
            let records = committed(&agent, [exchange.clone(), results.clone(), tail.clone()]);
            // Active creator is no longer visible after an earlier compaction.
            let origin = ModelCallOrigin {
                message: 1,
                call_id: "call".into(),
            };
            let retained =
                retained_sources(&records, &agent, &[(3, tail)], &[origin], "model", 8_000)
                    .unwrap();
            let sequences = retained
                .iter()
                .map(|source| source.sequence)
                .collect::<Vec<_>>();
            assert_eq!(sequences, vec![1, 2, 3]);
            // Original message identity is retained, not a visible-only reconstruction.
            assert_eq!(retained[0].message(), &exchange);
            assert_eq!(retained[1].message(), &results);
        }
    }

    #[test]
    fn retention_budget_keeps_original_tail_and_complete_tool_exchange() {
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([0; 16]));
        let messages = [
            user("old".into()),
            Message::Assistant(vec![]),
            Message::Tool(vec![]),
            // Together with the tool result this is exactly the 8,000-token budget.
            user("x".repeat(31_920)),
        ];
        let records = committed(&agent, messages.clone());
        let mut projected: Vec<_> = (1..).zip(messages).collect();
        let sequences = |projected: &[(u64, Message)], max_context| {
            let budget = retention_budget(max_context);
            let retained =
                retained_sources(&records, &agent, projected, &[], "model", budget).unwrap();
            retained
                .into_iter()
                .map(RetainedSource::into_sequence)
                .collect::<Vec<_>>()
        };
        assert_eq!(sequences(&projected, 128_000), vec![2, 3, 4]);
        // A larger window keeps the same tail; a smaller one keeps proportionally less.
        assert_eq!(sequences(&projected, 1_000_000), vec![2, 3, 4]);
        assert_eq!(sequences(&projected, 127_000), vec![4]);
        let sequences = |projected: &[(u64, Message)]| sequences(projected, 128_000);
        // Even an oversized final message survives; its payload is the original,
        // not the temporary projection used for token accounting.
        projected[3].1 = user("x".repeat(40_000));
        assert_eq!(sequences(&projected), vec![4]);
        // A synthetic projected continuation is not an original message source.
        projected.push((99, user("synthetic".into())));
        assert_eq!(sequences(&projected), vec![4]);
        assert!(sequences(&[]).is_empty());
    }

    #[test]
    fn retained_tool_results_include_nested_jobs_and_null_results_but_not_status_or_arguments() {
        let message = Message::Tool(vec![ToolResult {
            call_id: "script-call".into(),
            name: "script".into(),
            result: json!({"id":1,"state":"completed","has_result":true,"result":{
                "nested":[{"id":2,"state":"completed","has_result":true,"result":null}],
                "status":{"id":3,"state":"running"},
                "arguments":{"id":4,"state":"completed","has_result":true,"result":"literal"},
                "metadata":{"id":5,"state":"completed","has_result":false,"result":null,"error":null,"presentation":null}
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

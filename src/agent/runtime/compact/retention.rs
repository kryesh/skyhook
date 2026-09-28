//! Preserve whole available tool exchanges and identify outputs already carried by history.
use super::compaction;
use crate::{
    agent::CompactionError,
    identity::{AgentId, JobId},
    job::JobView,
    session::{
        CheckpointError, EventRecord, Message, MessageSeq, ModelCallOrigin, RecordSeq, SessionEvent,
    },
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// An original message selected from one immutable journal snapshot.
/// Only retention can construct the sequence/payload pair; no payload is cloned.
pub(super) struct RetainedSource<'a> {
    sequence: MessageSeq,
    message: &'a Message,
}

impl RetainedSource<'_> {
    pub(super) fn message(&self) -> &Message {
        self.message
    }

    pub(super) fn into_sequence(self) -> MessageSeq {
        self.sequence
    }
}

/// The verbatim tail's share of the context, so a small window still shrinks.
const RETAINED_SHARE: u64 = 16;
/// The verbatim tail's most tokens, however large the window.
const RETAINED_CAP: u64 = 8_000;

pub(in crate::agent::runtime) fn retention_budget(max_context: u64) -> u64 {
    (max_context / RETAINED_SHARE).min(RETAINED_CAP)
}

/// Select complete original exchanges, including creators no longer in the visible tail.
/// The tail is what `model` would be sent of it after compaction, within `budget` tokens.
pub(super) fn retained_sources<'a>(
    records: &'a [EventRecord],
    agent: &AgentId,
    projected: &[(RecordSeq, Message)],
    origins: &[ModelCallOrigin],
    model: &str,
    budget: u64,
) -> Result<Vec<RetainedSource<'a>>, CompactionError> {
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
        let tokens = compaction::estimate_message(&kept.render(), model);
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
    // The store rejects a checkpoint whose retained exchange is incomplete.
    for origin in origins {
        let index = originals
            .iter()
            .position(|(id, _)| *id == RecordSeq::from(origin.message))
            .ok_or(CheckpointError::RetainedSource)?;
        let exchange = originals[index + 1..]
            .iter()
            .take_while(|(_, message)| matches!(message, Message::Tool(_)));
        retained.insert(RecordSeq::from(origin.message));
        retained.extend(exchange.map(|(sequence, _)| *sequence));
    }
    let mut sources: Vec<_> = originals
        .into_iter()
        .filter(|(sequence, _)| retained.contains(sequence))
        .map(|(sequence, message)| RetainedSource {
            sequence: sequence.message(),
            message,
        })
        .collect();
    sources.sort_unstable_by_key(|source| source.sequence);
    Ok(sources)
}

/// A presented view supplies output when it carries a result, an error or a page;
/// its result and capture pages may embed further views.
fn included_view_jobs(view: &JobView, result: Option<&Value>, jobs: &mut BTreeSet<JobId>) {
    let presentation = view.presentation.as_ref();
    let paged = presentation.is_some_and(|presentation| {
        presentation.preview.is_some() || presentation.question.is_some()
    });
    if let Some(id) = view.id
        && (result.is_some() || view.error.is_some() || paged)
    {
        jobs.insert(id);
    }
    if let Some(result) = result {
        included_output_jobs(result, jobs);
    }
    for capture in presentation
        .into_iter()
        .flat_map(crate::job::Presentation::captures)
    {
        if let Some(page) = &capture.output {
            included_view_jobs(page, page.result.as_ref(), jobs);
        }
    }
}

/// Whether `value` is a view, adding the jobs it includes. Its result is read in
/// place rather than copied into the view, so nested views cost one pass.
fn included_value_jobs(value: &Value, jobs: &mut BTreeSet<JobId>) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    let fields = map.iter().filter(|(key, _)| *key != "result");
    let fields = fields.map(|(key, value)| (key.as_str(), value));
    let fields = serde::de::value::MapDeserializer::<_, serde_json::Error>::new(fields);
    let Ok(view) = JobView::deserialize(fields) else {
        return false;
    };
    included_view_jobs(&view, map.get("result"), jobs);
    true
}

/// Views a result embeds, as a script's return embeds those its calls returned.
/// Only an object naming a job and its state can be one, and arguments hold none.
pub(super) fn included_output_jobs(value: &Value, jobs: &mut BTreeSet<JobId>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("id")
                && map.contains_key("state")
                && included_value_jobs(value, jobs)
            {
                return;
            }
            for (key, child) in map {
                if key != "arguments" {
                    included_output_jobs(child, jobs);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                included_output_jobs(item, jobs);
            }
        }
        _ => {}
    }
}

/// A tool result is its call's view; job events carry theirs.
pub(super) fn included_message_jobs(message: &Message, jobs: &mut BTreeSet<JobId>) {
    match message {
        Message::Tool(results) => {
            for result in results {
                included_value_jobs(&result.result, jobs);
            }
        }
        Message::User(blocks) => {
            for block in blocks {
                if let crate::session::UserPart::JobEvents { events } = block {
                    for event in events {
                        if let crate::session::JobEvent::Job(view) = event {
                            included_view_jobs(view, view.result.as_ref(), jobs);
                        }
                    }
                }
            }
        }
        Message::Assistant(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::codec::common::tests::envelope;
    use crate::provider::protocol::{AssistantItem, Binding, ReplayFormat, ToolCall, ToolResult};
    use crate::session::UserPart;
    use serde_json::json;

    fn committed(agent: &AgentId, messages: impl IntoIterator<Item = Message>) -> Vec<EventRecord> {
        let committed = |(i, message)| {
            let event = SessionEvent::MessageCommitted { message };
            crate::session::tests::record(agent, i as u64 + 1, event)
        };
        messages.into_iter().enumerate().map(committed).collect()
    }

    fn user(text: String) -> Message {
        Message::User(vec![UserPart::Text { text }])
    }

    #[test]
    fn active_exchange_retains_complete_reasoning_bundle_outside_visible_tail() {
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([0; 16]));
        for format in ReplayFormat::ALL {
            let payload = json!({"opaque":"must survive", "signature":"signed"});
            let replay = envelope(format, "model", payload, Binding::Free);
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
                message: 1.into(),
                call_id: "call".into(),
            };
            let retained = retained_sources(
                &records,
                &agent,
                &[(3.into(), tail)],
                &[origin],
                "model",
                8_000,
            )
            .unwrap();
            let sequences = retained
                .iter()
                .map(|source| source.sequence.get())
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
        let mut projected: Vec<_> = (1u64..).map(RecordSeq::from).zip(messages).collect();
        let sequences = |projected: &[(RecordSeq, Message)], max_context| {
            let budget = retention_budget(max_context);
            let retained =
                retained_sources(&records, &agent, projected, &[], "model", budget).unwrap();
            retained
                .into_iter()
                .map(|source| source.into_sequence().get())
                .collect::<Vec<_>>()
        };
        assert_eq!(sequences(&projected, 128_000), vec![2, 3, 4]);
        // A larger window keeps the same tail; a smaller one keeps proportionally less.
        assert_eq!(sequences(&projected, 1_000_000), vec![2, 3, 4]);
        assert_eq!(sequences(&projected, 127_000), vec![4]);
        let sequences = |projected: &[(RecordSeq, Message)]| sequences(projected, 128_000);
        // Even an oversized final message survives; its payload is the original,
        // not the temporary projection used for token accounting.
        projected[3].1 = user("x".repeat(40_000));
        assert_eq!(sequences(&projected), vec![4]);
        // A synthetic projected continuation is not an original message source.
        projected.push((99.into(), user("synthetic".into())));
        assert_eq!(sequences(&projected), vec![4]);
        assert!(sequences(&[]).is_empty());
    }

    #[test]
    fn retained_tool_results_include_nested_jobs_and_null_results_but_not_status_or_arguments() {
        let message = Message::Tool(vec![ToolResult {
            call_id: "script-call".into(),
            name: "script".into(),
            result: json!({"id":1,"state":"completed","result":{
                "nested":[{"id":2,"state":"completed","result":null}],
                "status":{"id":3,"state":"running"},
                "arguments":{"id":4,"state":"completed","result":"literal"},
                "metadata":{"id":5,"state":"completed","meta":{"tool":"read"}}
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

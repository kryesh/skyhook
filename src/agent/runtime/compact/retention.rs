//! Preserve whole available tool exchanges and identify outputs already carried by history.
use crate::{
    agent::CompactionError,
    identity::{AgentId, JobId},
    job::JobView,
    session::{
        CheckpointError, CommittedUsage, EventRecord, Message, MessageSeq, ModelCallOrigin,
        Projected, RecordSeq, SessionEvent,
    },
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

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

/// A prompt size the provider reported for a request whose response committed.
struct Report {
    /// The last message the request's history held.
    through: Option<MessageSeq>,
    /// The model context it was sent under: requests under one share their system
    /// prompt, tools and model, so only their prompts are comparable.
    context: RecordSeq,
    prompt: u64,
}

/// The prompts the agent's requests since its latest checkpoint reported for the
/// responses they committed, in order. Only the agent's own requests complete responses.
fn reports(records: &[EventRecord], agent: &AgentId) -> Vec<Report> {
    let own: Vec<_> = records
        .iter()
        .filter(|record| &record.agent == agent)
        .collect();
    let start = own
        .iter()
        .rposition(|record| matches!(record.event, SessionEvent::Compaction { .. }))
        .map_or(0, |index| index + 1);
    let mut requests = HashMap::new();
    let mut committed = CommittedUsage::default();
    let mut reports = Vec::new();
    for record in &own[start..] {
        if let SessionEvent::ModelRequested {
            context, through, ..
        } = &record.event
        {
            requests.insert(record.sequence.request(), (*through, *context));
        }
        if let Some((request, usage)) = committed.fold(record)
            && let Some(&(through, context)) = requests.get(&request)
            && let Some(prompt) = usage.prompt_tokens()
        {
            reports.push(Report {
                through,
                context,
                prompt,
            });
        }
    }
    reports
}

/// How much of the visible history a checkpoint keeps beside the creators it must.
#[derive(Clone, Copy, Debug)]
pub(super) enum Retain {
    /// The messages after the last reported request, and older exchanges while what
    /// the reports size of them fits.
    Within(u64),
    /// Only the last exchange: a message and its tool results.
    LastExchange,
}

/// Select complete original exchanges, including creators no longer in the visible tail,
/// with sizes inferred from what the provider reported. Within a budget, the messages
/// after the last reported request are kept whole; older exchanges join them while the
/// growth between the prompts of the requests around them fits. The walk stops at the
/// first exchange the reports cannot size: one between requests of different model
/// contexts, or whose prompts did not grow, as when a smaller state replaced a larger one.
pub(super) fn retained_sources<'a>(
    records: &'a [EventRecord],
    agent: &AgentId,
    projected: &[Projected],
    origins: &[ModelCallOrigin],
    retain: Retain,
) -> Result<Vec<RetainedSource<'a>>, CompactionError> {
    let originals: Vec<_> = records
        .iter()
        .filter_map(|record| {
            if &record.agent != agent {
                return None;
            }
            // Retention keeps conversation: a committed state is stale once the next
            // request sends its own, and must not stand for the latest exchange.
            match &record.event {
                SessionEvent::MessageCommitted { message }
                    if !matches!(message, Message::State(_)) =>
                {
                    Some((record.sequence, message))
                }
                _ => None,
            }
        })
        .collect();
    let original_ids: BTreeSet<_> = originals.iter().map(|(id, _)| *id).collect();
    let visible: Vec<_> = projected
        .iter()
        .filter(|projected| original_ids.contains(&projected.sequence))
        .collect();
    // At least the last message stays, however large.
    let last = visible.len().saturating_sub(1);
    let mut start = match retain {
        Retain::Within(budget) => reported_start(records, agent, &visible, budget).min(last),
        Retain::LastExchange => last,
    };
    let mut retained = BTreeSet::new();
    // Per-call results follow their assistant call; keep the whole exchange.
    while start < visible.len() && start > 0 && matches!(visible[start].message, Message::Tool(_)) {
        start -= 1;
    }
    retained.extend(visible[start..].iter().map(|projected| projected.sequence));
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

/// Where the retained messages begin among `visible` within `budget`: after the last
/// reported request, or earlier while the reports size the exchanges before it.
fn reported_start(
    records: &[EventRecord],
    agent: &AgentId,
    visible: &[&Projected],
    budget: u64,
) -> usize {
    // Where each reported request's new messages begin among the visible ones.
    let boundaries: Vec<_> = reports(records, agent)
        .into_iter()
        .map(|report| {
            let through = report.through.map_or(RecordSeq::default(), RecordSeq::from);
            let start = visible.partition_point(|projected| projected.sequence <= through);
            (start, report)
        })
        .collect();
    let Some(&(mut start, _)) = boundaries.last() else {
        return visible.len();
    };
    let mut total = 0u64;
    for pair in boundaries.windows(2).rev() {
        let [(earlier, before), (_, after)] = pair else {
            unreachable!("windows of two")
        };
        if before.context != after.context || after.prompt <= before.prompt {
            break;
        }
        total = total.saturating_add(after.prompt - before.prompt);
        if total > budget {
            break;
        }
        start = *earlier;
    }
    start
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
        Message::Assistant(_) | Message::State(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::codec::common::tests::envelope;
    use crate::provider::protocol::{AssistantItem, Binding, ReplayFormat, ToolCall, ToolResult};
    use crate::session::{ModelPurpose, UserPart};
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

    /// A state committed before a request that then overflowed is not the latest
    /// exchange: the input before it is.
    #[test]
    fn committed_state_is_never_the_retained_exchange() {
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([1; 16]));
        let state = crate::session::RuntimeState {
            date: "2026-10-11".into(),
            jobs: vec![],
            todos: vec![],
            location: crate::execution::ExecutionLocation::root("/".into()),
        };
        let messages = [user("look".into()), Message::State(state)];
        let records = committed(&agent, messages.clone());
        let projected: Vec<_> = (1u64..)
            .zip(messages)
            .map(|(sequence, message)| Projected::unbound(sequence.into(), message))
            .collect();
        for retain in [Retain::LastExchange, Retain::Within(8_000)] {
            let retained = retained_sources(&records, &agent, &projected, &[], retain).unwrap();
            let retained: Vec<_> = (retained.into_iter())
                .map(|source| source.into_sequence().get())
                .collect();
            assert_eq!(retained, [1], "{retain:?}");
        }
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
                &[Projected::unbound(3.into(), tail)],
                &[origin],
                Retain::Within(8_000),
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

    /// The exchanges since the last reported request stay whole; older ones join while
    /// the growth between comparable reported prompts fits the budget, and the walk stops
    /// at an exchange the reports cannot size.
    #[test]
    fn retention_sizes_exchanges_by_reported_prompt_growth() {
        use crate::session::{AttemptRef, CompletedOutcome, ModelContext, tests};
        let agent = AgentId::root(crate::identity::SessionId::from_bytes([0; 16]));
        let call = |id: &str| {
            let call = ToolCall::new(id, "read", json!({})).unwrap();
            Message::Assistant(vec![AssistantItem::tool_call(id, 0, call)])
        };
        let result = |id: &str| {
            Message::Tool(vec![ToolResult {
                call_id: id.into(),
                name: "read".into(),
                result: json!("ok"),
                images: vec![],
                is_error: false,
            }])
        };
        let commit = |message| SessionEvent::MessageCommitted { message };
        let requested = |through: u64| SessionEvent::ModelRequested {
            context: 2.into(),
            checkpoint: None,
            through: Some(RecordSeq::from(through).message()),
            from: None,
            tail: Vec::new(),
            history_lifetime: Default::default(),
        };
        let attempt = |request: u64| AttemptRef {
            request: RecordSeq::from(request).request(),
            attempt: 1,
        };
        let usage = |request: u64, prompt| SessionEvent::Usage {
            request: RecordSeq::from(request).request(),
            usage: tests::usage(prompt, 0, 0),
        };
        let completed = |request: u64, message: u64| SessionEvent::ResponseCompleted {
            attempt: attempt(request),
            message: RecordSeq::from(message).message(),
            outcome: CompletedOutcome::Answer,
        };
        let context = ModelContext::test(ModelPurpose::Agent, tests::profile());
        let events = [
            commit(user("start".into())),
            SessionEvent::ModelContext { context },
            requested(1),
            SessionEvent::ModelAttemptStarted(attempt(3)),
            usage(3, 1_000),
            commit(call("a")),
            completed(3, 6),
            commit(result("a")),
            requested(8),
            SessionEvent::ModelAttemptStarted(attempt(9)),
            usage(9, 5_000),
            commit(call("b")),
            completed(9, 12),
            commit(result("b")),
            requested(14),
            SessionEvent::ModelAttemptStarted(attempt(15)),
            usage(15, 6_000),
            commit(user("answer".into())),
            completed(15, 18),
        ];
        let records: Vec<_> = (events.into_iter().enumerate())
            .map(|(index, event)| tests::record(&agent, index as u64 + 1, event))
            .collect();
        let mut projected: Vec<_> = (records.iter())
            .filter_map(|record| match &record.event {
                SessionEvent::MessageCommitted { message } => {
                    Some(Projected::unbound(record.sequence, message.clone()))
                }
                _ => None,
            })
            .collect();
        let sequences = |records: &[EventRecord], projected: &[Projected], budget| {
            let retain = Retain::Within(budget);
            let retained = retained_sources(records, &agent, projected, &[], retain).unwrap();
            (retained.into_iter())
                .map(|source| source.into_sequence().get())
                .collect::<Vec<_>>()
        };
        assert_eq!(sequences(&records, &projected, 8_000), [6, 8, 12, 14, 18]);
        assert_eq!(sequences(&records, &projected, 2_000), [12, 14, 18]);
        assert_eq!(sequences(&records, &projected, 500), [18]);
        let last = retained_sources(&records, &agent, &projected, &[], Retain::LastExchange);
        let last: Vec<_> = (last.unwrap().into_iter())
            .map(|source| source.into_sequence().get())
            .collect();
        assert_eq!(last, [18], "whatever the reports would size");
        // A synthetic projected continuation is not an original message source.
        projected.push(Projected::unbound(99.into(), user("synthetic".into())));
        assert_eq!(sequences(&records, &projected, 500), [18]);
        // Without reports only the last message stays, with its exchange.
        let unreported: Vec<_> = (records.iter())
            .filter(|record| !matches!(record.event, SessionEvent::Usage { .. }))
            .cloned()
            .collect();
        assert_eq!(sequences(&unreported, &projected, 8_000), [18]);
        assert!(sequences(&records, &[], 8_000).is_empty());
        // A prompt that did not grow sizes nothing: the walk stops there.
        let mut shrunk = records.clone();
        shrunk[10].event = usage(9, 500);
        assert_eq!(sequences(&shrunk, &projected, 8_000), [12, 14, 18]);
        // Prompts under different model contexts are not compared.
        let mut switched = records.clone();
        let other = ModelContext::test(ModelPurpose::Agent, tests::profile());
        switched[0].event = SessionEvent::ModelContext { context: other };
        let SessionEvent::ModelRequested { context, .. } = &mut switched[14].event else {
            unreachable!("the third request")
        };
        *context = 1.into();
        assert_eq!(sequences(&switched, &projected, 8_000), [18]);
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

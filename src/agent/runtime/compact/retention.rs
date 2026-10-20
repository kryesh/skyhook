//! Preserve whole available tool exchanges and identify outputs already carried by history.
use crate::{
    identity::JobId,
    job::JobView,
    session::{CompletedRequest, Message, MessageSeq, Projected, Projection, RecordSeq},
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// An original message the checkpoint retains, and its payload, borrowed from the
/// history it was selected from.
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

/// How much of the visible history a checkpoint keeps beside the creators it must.
#[derive(Clone, Copy, Debug)]
pub(super) enum Retain {
    /// The messages after the last reported request, and older exchanges while what
    /// the reports size of them fits.
    Within(u64),
    /// Only the last exchange: a message and its tool results.
    LastExchange,
}

/// Select complete original exchanges of `projected`, the agent's history, with sizes
/// inferred from what the provider reported for `requests`, those completed since its
/// checkpoint, beside `exchanges`, those of creators the checkpoint must keep, wherever
/// they are. Within a budget, the messages after the last reported request are kept
/// whole; older exchanges join them while the growth between the prompts of the
/// requests around them fits. The walk stops at the first exchange the reports cannot
/// size: one between requests of different model contexts, or whose prompts did not
/// grow, as when a smaller state replaced a larger one.
pub(super) fn retained_sources<'a>(
    projected: &'a Projection,
    requests: &[CompletedRequest],
    exchanges: &'a [(MessageSeq, Message)],
    retain: Retain,
) -> Vec<RetainedSource<'a>> {
    // Retention keeps conversation: a committed state is stale once the next request
    // sends its own, and must not stand for the latest exchange.
    let visible: Vec<_> = (projected.commits().iter())
        .filter(|projected| !matches!(projected.message, Message::State(_)))
        .collect();
    // At least the last message stays, however large.
    let last = visible.len().saturating_sub(1);
    let mut start = match retain {
        Retain::Within(budget) => reported_start(requests, &visible, budget).min(last),
        Retain::LastExchange => last,
    };
    // Per-call results follow their assistant call; keep the whole exchange.
    while start < visible.len() && start > 0 && matches!(visible[start].message, Message::Tool(_)) {
        start -= 1;
    }
    let mut sources: BTreeMap<_, _> = (visible[start..].iter())
        .map(|projected| (projected.sequence.message(), &projected.message))
        .collect();
    // The store rejects a checkpoint whose retained exchange is incomplete.
    sources.extend(
        exchanges
            .iter()
            .map(|(sequence, message)| (*sequence, message)),
    );
    (sources.into_iter())
        .map(|(sequence, message)| RetainedSource { sequence, message })
        .collect()
}

/// Where the retained messages begin among `visible` within `budget`: after the last
/// reported request, or earlier while the reports size the exchanges before it.
fn reported_start(requests: &[CompletedRequest], visible: &[&Projected], budget: u64) -> usize {
    // The prompts the agent's requests since its checkpoint reported for the responses
    // they committed, in order, each with where its new messages begin among the visible
    // ones and the model context it was sent under: requests under one share their
    // system prompt, tools and model, so only their prompts are comparable.
    let boundaries: Vec<_> = requests
        .iter()
        .filter_map(|requested| {
            let prompt = requested.usage?.prompt_tokens()?;
            let through = requested
                .through
                .map_or(RecordSeq::default(), RecordSeq::from);
            let start = visible.partition_point(|projected| projected.sequence <= through);
            Some((start, requested.context, prompt))
        })
        .collect();
    let Some(&(mut start, ..)) = boundaries.last() else {
        return visible.len();
    };
    let mut total = 0u64;
    for pair in boundaries.windows(2).rev() {
        let [(earlier, before, smaller), (_, after, larger)] = pair else {
            unreachable!("windows of two")
        };
        if before != after || larger <= smaller {
            break;
        }
        total = total.saturating_add(larger - smaller);
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
        Message::Tool(result) => {
            included_value_jobs(&result.result, jobs);
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
    use crate::session::{SessionEvent, UserPart};
    use serde_json::json;

    fn user(text: String) -> Message {
        Message::User(vec![UserPart::Text { text }])
    }

    /// History holding `messages`, committed at the sequences each names.
    fn history(messages: impl IntoIterator<Item = (u64, Message)>) -> Projection {
        let messages = messages.into_iter();
        Projection {
            checkpoint: None,
            messages: messages
                .map(|(sequence, message)| Projected::unbound(sequence.into(), message))
                .collect(),
        }
    }

    fn sequences(retained: Vec<RetainedSource<'_>>) -> Vec<u64> {
        (retained.into_iter())
            .map(|source| source.into_sequence().get())
            .collect()
    }

    fn call(id: &str, index: u32) -> AssistantItem {
        AssistantItem::tool_call(id, index, ToolCall::new(id, "read", json!({})).unwrap())
    }

    fn result(id: &str) -> Message {
        Message::Tool(ToolResult {
            call_id: id.into(),
            name: "read".into(),
            result: json!("ok"),
            images: vec![],
            is_error: false,
        })
    }

    /// A state committed before a request that then overflowed is not the latest
    /// exchange: the input before it is.
    #[test]
    fn committed_state_is_never_the_retained_exchange() {
        let state = crate::session::RuntimeState {
            date: "2026-10-11".into(),
            jobs: vec![],
            todos: vec![],
            location: crate::execution::ExecutionLocation::root("/".into()),
        };
        let projected = history([(1, user("look".into())), (2, Message::State(state))]);
        for retain in [Retain::LastExchange, Retain::Within(8_000)] {
            let retained = retained_sources(&projected, &[], &[], retain);
            assert_eq!(sequences(retained), [1], "{retain:?}");
        }
    }

    #[test]
    fn active_exchange_retains_complete_reasoning_bundle_outside_visible_tail() {
        for format in ReplayFormat::ALL {
            let payload = json!({"opaque":"must survive", "signature":"signed"});
            let replay = envelope(format, "model", payload, Binding::Free);
            let reasoning =
                AssistantItem::reasoning("reason", 0, "visible reasoning", Some(replay));
            let call = ToolCall::new("call", "agent", json!({"prompt":"continue"})).unwrap();
            let exchange =
                Message::Assistant(vec![reasoning, AssistantItem::tool_call("tool", 1, call)]);
            let results = Message::Tool(ToolResult {
                call_id: "call".into(),
                name: "agent".into(),
                result: json!({"id":1}),
                images: vec![],
                is_error: false,
            });
            // Active creator is no longer visible after an earlier compaction.
            let projected = history([(3, user("tail".repeat(10_000)))]);
            let exchanges = [(1.into(), exchange.clone()), (2.into(), results.clone())];
            let retained = retained_sources(&projected, &[], &exchanges, Retain::Within(8_000));
            // Original message identity is retained, not a visible-only reconstruction.
            assert_eq!(retained[0].message(), &exchange);
            assert_eq!(retained[1].message(), &results);
            assert_eq!(sequences(retained), [1, 2, 3]);
        }
    }

    /// The exchanges since the last reported request stay whole; older ones join while
    /// the growth between comparable reported prompts fits the budget, and the walk stops
    /// at an exchange the reports cannot size.
    #[test]
    fn retention_sizes_exchanges_by_reported_prompt_growth() {
        let projected = history([
            (1, user("start".into())),
            (6, Message::Assistant(vec![call("a", 0)])),
            (8, result("a")),
            (12, Message::Assistant(vec![call("b", 0)])),
            (14, result("b")),
            (18, user("answer".into())),
        ]);
        let reported = |through: u64, prompt| CompletedRequest {
            context: 2.into(),
            through: Some(RecordSeq::from(through).message()),
            usage: Some(crate::session::tests::usage(prompt, 0, 0)),
        };
        let requests = [reported(1, 1_000), reported(8, 5_000), reported(14, 6_000)];
        let kept = |requests: &[CompletedRequest], projected: &Projection, budget| {
            sequences(retained_sources(
                projected,
                requests,
                &[],
                Retain::Within(budget),
            ))
        };
        assert_eq!(kept(&requests, &projected, 8_000), [6, 8, 12, 14, 18]);
        assert_eq!(kept(&requests, &projected, 2_000), [12, 14, 18]);
        assert_eq!(kept(&requests, &projected, 500), [18]);
        let last = retained_sources(&projected, &requests, &[], Retain::LastExchange);
        assert_eq!(sequences(last), [18], "whatever the reports would size");
        // Without reports only the last message stays, with its exchange.
        let unreported = requests.clone().map(|request| CompletedRequest {
            usage: None,
            ..request
        });
        assert_eq!(kept(&unreported, &projected, 8_000), [18]);
        assert!(kept(&requests, &Projection::default(), 8_000).is_empty());
        // A prompt that did not grow sizes nothing: the walk stops there.
        let mut shrunk = requests.clone();
        shrunk[1] = reported(8, 500);
        assert_eq!(kept(&shrunk, &projected, 8_000), [12, 14, 18]);
        // Prompts under different model contexts are not compared.
        let mut switched = requests.clone();
        switched[2].context = 1.into();
        assert_eq!(kept(&switched, &projected, 8_000), [18]);
    }

    /// A retried request is sized by the attempt that answered it, not by what a
    /// failed attempt reported; per-call results committed out of call order stay
    /// with their exchange, whichever of them the retained tail starts at.
    #[tokio::test]
    async fn retention_sizes_a_retried_request_by_its_answer_and_keeps_reordered_results() {
        use crate::provider::ProviderErrorKind;
        use crate::session::{
            AttemptFailure, AttemptRef, CompletedOutcome, ModelContext, ModelPurpose, tests,
        };
        let runtime = crate::tests::TestRuntime::new().await;
        let (store, agent) = (&runtime.store, &runtime.agent);
        let append = async |event| store.append(agent.clone(), event).await.unwrap().sequence;
        let commit = |message| SessionEvent::MessageCommitted { message };
        let attempt = |request: RecordSeq, attempt| AttemptRef {
            request: request.request(),
            attempt,
        };
        let usage = |request: RecordSeq, prompt| SessionEvent::Usage {
            request: request.request(),
            usage: tests::usage(prompt, 0, 0),
        };
        let completed = |attempt, message: RecordSeq| SessionEvent::ResponseCompleted {
            attempt,
            message: message.message(),
            outcome: CompletedOutcome::Answer,
        };
        let context = ModelContext::test(ModelPurpose::Agent, tests::profile());
        let start = append(commit(user("start".into()))).await;
        let context = append(SessionEvent::ModelContext { context }).await;
        let requested = |through: RecordSeq| SessionEvent::ModelRequested {
            context,
            checkpoint: None,
            through: Some(through.message()),
            from: None,
            tail: Vec::new(),
            history_lifetime: Default::default(),
        };
        let first = append(requested(start)).await;
        append(SessionEvent::ModelAttemptStarted(attempt(first, 1))).await;
        append(usage(first, 1_000)).await;
        let calls = append(commit(Message::Assistant(vec![call("a", 0), call("b", 1)]))).await;
        append(completed(attempt(first, 1), calls)).await;
        let first = append(commit(result("b"))).await;
        let second = append(commit(result("a"))).await;
        let retried = append(requested(second)).await;
        append(SessionEvent::ModelAttemptStarted(attempt(retried, 1))).await;
        append(usage(retried, 9_000)).await;
        append(SessionEvent::ModelFailed {
            attempt: attempt(retried, 1),
            failure: AttemptFailure::Retrying {
                message: "dropped".into(),
                kind: ProviderErrorKind::Transport,
                delay: std::time::Duration::from_millis(1).into(),
            },
        })
        .await;
        append(SessionEvent::ModelAttemptStarted(attempt(retried, 2))).await;
        append(usage(retried, 5_000)).await;
        let answer = Message::Assistant(vec![AssistantItem::text("text", 0, "answer")]);
        let answer = append(commit(answer)).await;
        let last = append(completed(attempt(retried, 2), answer)).await;
        let projected = tests::projected(store, agent).await;
        let retention = store.retention(agent, RecordSeq::default(), last, Vec::new());
        let requests = retention.await.unwrap().unwrap().requests;
        let retained = |projected: &Projection, retain| {
            let retained = retained_sources(projected, &requests, &[], retain);
            (retained.into_iter())
                .map(|source| RecordSeq::from(source.into_sequence()))
                .collect::<Vec<_>>()
        };
        // The answering attempt grew the prompt by 4,000; the failed one by 8,000.
        let within = |budget| retained(&projected, Retain::Within(budget));
        assert_eq!(within(6_000), [calls, first, second, answer]);
        assert_eq!(within(3_000), [answer]);
        // Ending on the later-committed result, the tail starts at its call.
        let mut unanswered = projected.clone();
        unanswered.messages.pop();
        let last = retained(&unanswered, Retain::LastExchange);
        assert_eq!(last, [calls, first, second]);
    }

    #[test]
    fn retained_tool_results_include_nested_jobs_and_null_results_but_not_status_or_arguments() {
        let message = Message::Tool(ToolResult {
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
        });
        let mut included = BTreeSet::new();
        included_message_jobs(&message, &mut included);
        assert_eq!(
            included,
            [JobId::new(1).unwrap(), JobId::new(2).unwrap()].into()
        );
    }
}

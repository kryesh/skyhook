//! Reconstruct model-visible history and exact provider requests from durable events.
use crate::{
    identity::AgentId,
    provider::{profile::ModelRef, protocol::ModelRequest},
    session::Message,
};

use super::{EventRecord, ModelContext, ModelPurpose, SessionError, SessionEvent};
use crate::session::{MessageSeq, RecordSeq, RequestSeq};

/// The record journaled at `sequence`. Records must be in sequence order.
#[must_use]
pub fn record_at(records: &[EventRecord], sequence: RecordSeq) -> Option<&EventRecord> {
    let index = records
        .binary_search_by_key(&sequence, |record| record.sequence)
        .ok()?;
    Some(&records[index])
}

/// The context a `ModelRequested` record was issued under, with `lookup` finding
/// the record journaled at a sequence.
#[must_use]
pub fn request_context<'a>(
    record: &EventRecord,
    lookup: impl FnOnce(RecordSeq) -> Option<&'a EventRecord>,
) -> Option<&'a ModelContext> {
    let SessionEvent::ModelRequested { context, .. } = &record.event else {
        return None;
    };
    match &lookup(*context)?.event {
        SessionEvent::ModelContext { context } => Some(context),
        _ => None,
    }
}

/// One agent's model-visible history: its latest checkpoint's sequence, if any, and
/// the messages in order, the checkpoint's own message first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Projection {
    pub checkpoint: Option<RecordSeq>,
    /// Each message with the record that carries it: a commit, or the checkpoint.
    pub messages: Vec<(RecordSeq, Message)>,
}

impl Projection {
    /// The committed message sources after the checkpoint, as a request names them.
    #[must_use]
    pub fn sources(&self) -> Vec<MessageSeq> {
        let start = usize::from(self.checkpoint.is_some());
        self.messages[start..]
            .iter()
            .map(|(sequence, _)| sequence.message())
            .collect()
    }

    pub fn history(&self) -> impl Iterator<Item = Message> + '_ {
        self.messages.iter().map(|(_, message)| message.clone())
    }
}

/// Committed history as a provider receives it. Tool results are committed one
/// message per call as each call finishes; providers receive one tool message per
/// exchange, ordered like the calls that produced it.
#[must_use]
pub fn render_history(
    messages: impl IntoIterator<Item = Message>,
) -> Vec<crate::provider::protocol::Message> {
    use crate::provider::protocol::Message;
    let mut merged: Vec<Message> = Vec::new();
    let mut calls: Vec<String> = Vec::new();
    for message in messages {
        match message.render() {
            Message::Tool(results) => {
                if let Some(Message::Tool(previous)) = merged.last_mut() {
                    previous.extend(results);
                } else {
                    merged.push(Message::Tool(results));
                }
                if let Some(Message::Tool(results)) = merged.last_mut() {
                    results.sort_by_key(|result| {
                        calls
                            .iter()
                            .position(|call| call == &result.call_id)
                            .unwrap_or(usize::MAX)
                    });
                }
            }
            message => {
                if let Message::Assistant(items) = &message {
                    calls = items
                        .iter()
                        .filter_map(|item| item.call())
                        .map(|call| call.id().to_owned())
                        .collect();
                }
                merged.push(message);
            }
        }
    }
    merged
}

/// Validate a decoded journal's order-dependent rules, as append validates live
/// records, so what a store hands out always projects.
pub(super) fn admit_records(records: Vec<EventRecord>) -> Result<Vec<EventRecord>, SessionError> {
    for (index, record) in records.iter().enumerate() {
        validate_compaction(&records[..index], record)?;
    }
    Ok(records)
}

/// Project the latest committed compaction and subsequent messages for one agent.
/// Checkpoints were validated when appended or loaded, so a journal always projects.
#[must_use]
pub fn project_history(records: &[EventRecord], agent: &AgentId) -> Projection {
    let checkpoint = records.iter().rposition(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. })
    });
    let mut result = Projection::default();
    let frontier = if let Some(index) = checkpoint {
        let record = &records[index];
        let SessionEvent::Compaction { checkpoint } = &record.event else {
            unreachable!()
        };
        result.checkpoint = Some(record.sequence);
        result
            .messages
            .push((record.sequence, checkpoint.message.clone()));
        for sequence in &checkpoint.retained {
            let source = records[..index]
                .iter()
                .find(|source| source.sequence == RecordSeq::from(*sequence))
                .expect("retained sources were validated when the checkpoint was appended");
            let SessionEvent::MessageCommitted { message } = &source.event else {
                unreachable!()
            };
            let message = message.clone().without_bound_reasoning();
            result.messages.push(((*sequence).into(), message));
        }
        checkpoint.frontier
    } else {
        RecordSeq::default()
    };
    let switched = mode_boundary(records, agent);
    result.messages.extend(records.iter().filter_map(|record| {
        if &record.agent == agent
            && record.sequence > frontier
            && let SessionEvent::MessageCommitted { message } = &record.event
        {
            let message = message.clone();
            let message = if record.sequence < switched {
                message.without_bound_reasoning()
            } else {
                message
            };
            return Some((record.sequence, message));
        }
        None
    }));
    result
}

/// A mode switch replaces the system prompt and tools, which invalidates bound
/// reasoning before it exactly as compaction does.
fn mode_boundary(records: &[EventRecord], agent: &AgentId) -> RecordSeq {
    let switched = records.iter().rev().find(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::ModeChanged { .. })
    });
    switched.map_or(RecordSeq::default(), |record| record.sequence)
}

pub(super) fn validate_compaction(
    preceding: &[EventRecord],
    record: &EventRecord,
) -> Result<(), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay {
        sequence: record.sequence.get(),
        reason,
    };
    let SessionEvent::Compaction { checkpoint } = &record.event else {
        return Ok(());
    };
    if checkpoint
        .todos
        .iter()
        .any(|item| item.text.trim().is_empty())
    {
        return Err(invalid("compaction todo text cannot be blank"));
    }
    // Live commits hold the todo-store lock and check its revision first. This
    // check enforces the same invariant for journal replay and direct appends.
    if preceding.iter().any(|source| {
        source.agent == record.agent
            && source.sequence > checkpoint.frontier
            && matches!(
                source.event,
                SessionEvent::TodosReplaced { .. } | SessionEvent::Compaction { .. }
            )
    }) {
        return Err(invalid("compaction cannot overwrite newer todo state"));
    }
    if !matches!(checkpoint.message, Message::User(_)) {
        return Err(invalid("compaction message must use the user role"));
    }
    if checkpoint.frontier >= record.sequence
        || checkpoint.frontier
            > preceding
                .last()
                .map_or(RecordSeq::default(), |r| r.sequence)
    {
        return Err(invalid("compaction frontier must precede its event"));
    }
    let summary = preceding
        .iter()
        .find(|source| {
            source.sequence == RecordSeq::from(checkpoint.attempt.request)
                && source.agent == record.agent
                && checkpoint.frontier < source.sequence
        })
        .and_then(|source| request_context(source, |sequence| record_at(preceding, sequence)));
    if summary.is_none_or(|context| context.purpose != ModelPurpose::Compaction) {
        return Err(invalid(
            "compaction request must be a summary request that follows its frontier, precedes its event and belongs to the same agent",
        ));
    }
    let mut last = MessageSeq::default();
    for sequence in &checkpoint.retained {
        if *sequence <= last || RecordSeq::from(*sequence) > checkpoint.frontier {
            return Err(invalid(
                "retained messages must be unique, chronological and covered by the frontier",
            ));
        }
        if !preceding.iter().any(|source| {
            source.sequence == RecordSeq::from(*sequence)
                && source.agent == record.agent
                && matches!(source.event, SessionEvent::MessageCommitted { .. })
        }) {
            return Err(invalid(
                "retained source must be an original message of the same agent",
            ));
        }
        last = *sequence;
    }
    // References preserve whole messages, but a checkpoint must also preserve the
    // original assistant call and its complete run of per-call results.
    let originals: Vec<_> = preceding
        .iter()
        .filter(|source| {
            source.agent == record.agent
                && source.sequence <= checkpoint.frontier
                && matches!(source.event, SessionEvent::MessageCommitted { .. })
        })
        .collect();
    let message = |index: usize| match &originals[index].event {
        SessionEvent::MessageCommitted { message } => message,
        _ => unreachable!(),
    };
    let retained = |index: usize| {
        checkpoint
            .retained
            .binary_search(&originals[index].sequence.message())
            .is_ok()
    };
    for index in 0..originals.len() {
        if !retained(index) {
            continue;
        }
        let assistant = match message(index) {
            Message::Tool(_) => {
                let Some(assistant) = (0..index)
                    .rev()
                    .find(|&index| !matches!(message(index), Message::Tool(_)))
                else {
                    return Err(invalid("retained tool exchange is incomplete"));
                };
                assistant
            }
            Message::Assistant(content) if content.iter().any(|item| item.call().is_some()) => {
                index
            }
            _ => continue,
        };
        let results: Vec<_> = (assistant + 1..originals.len())
            .take_while(|&index| matches!(message(index), Message::Tool(_)))
            .collect();
        if !retained(assistant) || results.is_empty() || !results.iter().all(|&i| retained(i)) {
            return Err(invalid("retained tool exchange is incomplete"));
        }
        let tools: Vec<_> = results.into_iter().map(message).collect();
        if !valid_tool_pair(message(assistant), &tools) {
            return Err(invalid("retained tool call and results do not match"));
        }
    }
    Ok(())
}

fn valid_tool_pair(assistant: &Message, tools: &[&Message]) -> bool {
    let Message::Assistant(content) = assistant else {
        return false;
    };
    let calls: Vec<_> = content
        .iter()
        .filter_map(|item| item.call())
        .map(|call| (call.id(), call.name()))
        .collect();
    let results: Vec<_> = tools
        .iter()
        .flat_map(|tool| match tool {
            Message::Tool(results) => results.as_slice(),
            _ => &[],
        })
        .map(|result| (result.call_id.as_str(), result.name.as_str()))
        .collect();
    let call_set: std::collections::HashSet<_> = calls.iter().copied().collect();
    let result_set: std::collections::HashSet<_> = results.iter().copied().collect();
    !calls.is_empty()
        && calls.len() == call_set.len()
        && results.len() == result_set.len()
        && call_set == result_set
}

/// Return the model the request was issued under and the exact request at a
/// `ModelRequested` event.
/// Records must be ordered by sequence, as returned by `SessionStore`.
/// Attachment and image references keep their blob digests; use
/// `SessionStore::load_blobs` to load their contents for provider encoding.
pub fn reconstruct_model_request(
    records: &[EventRecord],
    sequence: RequestSeq,
) -> Result<(ModelRef, ModelRequest), SessionError> {
    let index = records
        .binary_search_by_key(&sequence.into(), |record| record.sequence)
        .map_err(|_| SessionError::ModelRequestReplay {
            sequence: sequence.get(),
            reason: "event not found",
        })?;
    let call = &records[index];
    let invalid = |reason| SessionError::ModelRequestReplay {
        sequence: sequence.get(),
        reason,
    };
    let SessionEvent::ModelRequested {
        checkpoint,
        history: sources,
        tail,
        history_lifetime,
        ..
    } = &call.event
    else {
        return Err(invalid("event is not a model request"));
    };
    let lookup = |source: RecordSeq| record_at(&records[..index], source);
    let context = request_context(call, lookup)
        .ok_or_else(|| invalid("context must be a model context that precedes the call"))?;
    let frontier = match checkpoint.map(lookup) {
        None => None,
        Some(Some(EventRecord {
            event: SessionEvent::Compaction { checkpoint },
            agent,
            ..
        })) if *agent == call.agent => Some(checkpoint.frontier),
        Some(_) => {
            return Err(invalid(
                "checkpoint must be a compaction of the same agent that precedes the call",
            ));
        }
    };
    let switched = mode_boundary(&records[..index], &call.agent);
    let mut history = Vec::new();
    let sources = sources.iter().map(|source| RecordSeq::from(*source));
    for source in checkpoint.iter().copied().chain(sources) {
        let record = lookup(source)
            .filter(|record| record.agent == call.agent)
            .ok_or_else(|| {
                invalid("message source must precede the call and belong to the same agent")
            })?;
        let message = match &record.event {
            SessionEvent::MessageCommitted { message } => message,
            SessionEvent::Compaction { checkpoint } => &checkpoint.message,
            _ => {
                return Err(invalid(
                    "referenced event does not contain a conversation message",
                ));
            }
        };
        // As in projection and summaries, bound reasoning never enters a changed conversation.
        let changed = context.purpose == ModelPurpose::Compaction
            || source < switched
            || frontier.is_some_and(|last| source <= last);
        let message = message.clone();
        history.push(if changed {
            message.without_bound_reasoning()
        } else {
            message
        });
    }
    let request = ModelRequest {
        history: render_history(history),
        tail: tail.iter().map(Message::render).collect(),
        history_lifetime: *history_lifetime,
        ..context.template()
    };
    Ok((context.profile.name.clone(), request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{TodoItem, TodoStatus},
        identity::AgentId,
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, ReplayFormat, SystemSegment, ToolCall,
            ToolDefinition, ToolResult,
        },
        session::{AttemptRef, CompactionCheckpoint, SessionStore, UserPart, tests::MemorySession},
    };
    use serde_json::json;

    fn text_message(text: &str) -> Message {
        Message::User(vec![UserPart::Text { text: text.into() }])
    }

    fn committed(message: Message) -> SessionEvent {
        SessionEvent::MessageCommitted { message }
    }

    fn requested(
        context: RecordSeq,
        checkpoint: Option<RecordSeq>,
        history: &[RecordSeq],
        tail: &str,
        history_lifetime: HistoryLifetime,
    ) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            checkpoint,
            history: history.iter().map(|source| source.message()).collect(),
            tail: vec![text_message(tail)],
            history_lifetime,
        }
    }

    fn context(purpose: ModelPurpose, provider: &str, model: &str, system: &str) -> SessionEvent {
        let mut profile = crate::session::tests::profile();
        profile.name.provider = provider.parse().unwrap();
        profile.profile.model = model.into();
        profile.profile.reasoning = Some("high".into());
        SessionEvent::ModelContext {
            context: crate::session::ModelContext {
                purpose,
                profile,
                system: vec![SystemSegment {
                    text: system.into(),
                    cache: true,
                }],
                tools: vec![ToolDefinition {
                    name: "read".into(),
                    description: "original description".into(),
                    input_schema: json!({"type":"object"}),
                }],
                response_schema: Some(crate::provider::protocol::ResponseSchema {
                    name: "answer".into(),
                    schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
                }),
            },
        }
    }

    fn todo(text: &str) -> TodoItem {
        TodoItem {
            text: text.into(),
            status: TodoStatus::Pending,
        }
    }

    #[tokio::test]
    async fn replay_preserves_context_boundaries_and_image_payloads() {
        let (directory, store, agent) = crate::session::tests::on_disk().await;
        let child = agent.child(1);
        let child_start = crate::session::tests::agent_started(directory.path());
        store.append(child.clone(), child_start).await.unwrap();
        let png = crate::tests::png(b"image payload");
        let image = store
            .store_image(Some("test.png".into()), &png)
            .await
            .unwrap();
        let text = crate::media::Attachment::Text {
            file: Some("notes.txt".into()),
            content: "notes".into(),
        };
        let notes = store.store_attachment(&text).await.unwrap();
        let user = Message::User(vec![
            UserPart::Text {
                text: "inspect".into(),
            },
            UserPart::Attachment {
                attachment: crate::media::AttachmentRef::Image(image.clone()),
            },
            UserPart::Attachment {
                attachment: notes.clone(),
            },
        ]);
        // Opaque to the journal: nesting, key order, numbers and escapes must survive.
        let payload =
            json!({"signature":"pre\"serve\u{e9}","a":[1.5,{"z":null,"b":-0.0}],"n":1e300});
        let replay = envelope(
            ReplayFormat::Messages,
            "original-model",
            payload,
            Binding::Free,
        );
        let reasoning = AssistantItem::reasoning("reasoning-item", 0, "reasoning", Some(replay));
        let read = crate::provider::protocol::ToolCall::new("read-1", "read", json!({})).unwrap();
        let assistant = Message::Assistant(vec![
            reasoning,
            AssistantItem::tool_call("read-item", 1, read),
        ]);
        let tool = Message::Tool(vec![ToolResult {
            call_id: "read-1".into(),
            name: "read".into(),
            result: json!({"ok":true}),
            images: vec![image.clone()],
            is_error: false,
        }]);
        let append =
            async |agent: &AgentId, event| store.append(agent.clone(), event).await.unwrap();
        let mut sources = Vec::new();
        for message in [&user, &assistant, &tool] {
            sources.push(append(&agent, committed(message.clone())).await.sequence);
        }
        let original = context(
            ModelPurpose::Agent,
            "original-provider",
            "original-model",
            "original instructions",
        );
        let SessionEvent::ModelContext { context: template } = &original else {
            unreachable!()
        };
        let template = template.template();
        let context = append(&agent, original.clone()).await;
        let child_context = append(&child, original).await;
        append(&child, committed(text_message("child only"))).await;
        let history_lifetime = HistoryLifetime::Detached;
        let request = requested(
            context.sequence,
            None,
            &sources,
            "exact call-time state",
            history_lifetime,
        );
        let call = append(&agent, request).await;
        append(&agent, context_event_changed()).await;
        append(&agent, committed(text_message("future message"))).await;
        // A request names only its own agent's context.
        let foreign = requested(child_context.sequence, None, &[], "state", history_lifetime);
        assert!(store.append(agent.clone(), foreign).await.is_err());
        let id = store.id();
        drop(store);
        let (store, records) = SessionStore::open(directory.path(), id).await.unwrap();
        let (model, mut restored) =
            reconstruct_model_request(&records, call.sequence.request()).unwrap();
        assert_eq!(model.to_string(), "original-provider/test");
        let mut expected = template;
        (expected.history, expected.history_lifetime) = (
            vec![user.render(), assistant.render(), tool.render()],
            history_lifetime,
        );
        expected.tail = vec![text_message("exact call-time state").render()];
        assert_eq!(restored, expected);
        // Value equality ignores key order and the sign of zero; the wire bytes must not.
        let wire = |request: &ModelRequest| serde_json::to_string(&request.history[1]).unwrap();
        assert_eq!(wire(&restored), wire(&expected));
        store.load_blobs(&mut restored).await.unwrap();
        store.load_blobs(&mut expected).await.unwrap();
        assert_eq!(restored, expected);
        assert_eq!(restored.blobs.get(&image.blob).unwrap(), png.bytes());
        let crate::media::AttachmentRef::Text(notes) = notes else {
            panic!("text attachment")
        };
        assert_eq!(restored.blobs.text(&notes).unwrap(), "notes");

        // A history source must be a message; a context record is not one.
        let index = records
            .iter()
            .position(|r| r.sequence == call.sequence)
            .unwrap();
        let mut invalid = records.clone();
        let SessionEvent::ModelRequested { history, .. } = &mut invalid[index].event else {
            unreachable!()
        };
        history[0] = child_context.sequence.message();
        assert!(reconstruct_model_request(&invalid, call.sequence.request()).is_err());
        assert!(reconstruct_model_request(&records, 0.into()).is_err());
        assert!(reconstruct_model_request(&records, child_context.sequence.request()).is_err());
    }

    fn context_event_changed() -> SessionEvent {
        context(
            ModelPurpose::Agent,
            "new-provider",
            "new-model",
            "new instructions",
        )
    }

    fn projection_fixture() -> (AgentId, Vec<EventRecord>) {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let checkpoint = CompactionCheckpoint {
            todos: Vec::new(),
            frontier: 2.into(),
            message: text_message("first summary"),
            retained: vec![1.into()],
            attempt: AttemptRef {
                request: 5.into(),
                attempt: 1,
            },
            before_tokens: 100_000,
            after_tokens: 10_000,
        };
        let summary = |checkpoint: Option<u64>, history: &[u64], tail| {
            let history: Vec<RecordSeq> = history.iter().copied().map(Into::into).collect();
            let checkpoint = checkpoint.map(Into::into);
            requested(
                4.into(),
                checkpoint,
                &history,
                tail,
                HistoryLifetime::Detached,
            )
        };
        let events = vec![
            committed(text_message("verbatim plan")),
            committed(text_message("research")),
            context(ModelPurpose::Agent, "provider", "model", "system"),
            context(ModelPurpose::Compaction, "provider", "model", "system"),
            summary(None, &[1, 2], "summarize"),
            committed(text_message("concurrent steering")),
            SessionEvent::Compaction {
                checkpoint: checkpoint.clone(),
            },
            committed(text_message("continued work")),
            summary(Some(7), &[1, 6, 8], "state at request time"),
            SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier: 8.into(),
                    message: text_message("second summary with inherited plan"),
                    retained: vec![1.into(), 6.into()],
                    attempt: AttemptRef {
                        request: 9.into(),
                        attempt: 1,
                    },
                    ..checkpoint
                },
            },
        ];
        let records = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| EventRecord {
                id: crate::identity::EventId::generate().unwrap(),
                sequence: (index as u64 + 1).into(),
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })
            .collect();
        (agent, records)
    }

    fn sequences(projection: &Projection) -> Vec<u64> {
        projection
            .messages
            .iter()
            .map(|(sequence, _)| sequence.get())
            .collect()
    }

    fn messages(sources: &[MessageSeq]) -> Vec<u64> {
        sources.iter().map(|source| source.get()).collect()
    }

    #[test]
    fn projection_preserves_concurrent_messages_and_repeated_compaction() {
        let (agent, records) = projection_fixture();
        let early = project_history(&records[..6], &agent);
        assert_eq!((early.checkpoint, sequences(&early)), (None, vec![1, 2, 6]));
        assert_eq!(messages(&early.sources()), [1, 2, 6]);
        let first = project_history(&records[..8], &agent);
        assert_eq!(
            (first.checkpoint, sequences(&first)),
            (Some(7.into()), vec![7, 1, 6, 8])
        );
        assert_eq!(messages(&first.sources()), [1, 6, 8]);
        assert_eq!(
            (&first.messages[1].1, &first.messages[2].1),
            (
                &text_message("verbatim plan"),
                &text_message("concurrent steering")
            )
        );
        let second = project_history(&records, &agent);
        assert_eq!(
            (second.checkpoint, sequences(&second)),
            (Some(10.into()), vec![10, 1, 6])
        );
        assert_eq!(second.messages[1].1, text_message("verbatim plan"));
        assert!(
            project_history(&records, &agent.child(1))
                .messages
                .is_empty()
        );
        // Reconstruction resolves the same references and rejects broken ones.
        for (checkpoint, source) in [(Some(3), 1u64), (Some(9), 1), (Some(7), 4), (Some(7), 9)] {
            let mut invalid = records.clone();
            let SessionEvent::ModelRequested {
                checkpoint: named,
                history,
                ..
            } = &mut invalid[8].event
            else {
                unreachable!()
            };
            (*named, history[0]) = (
                checkpoint.map(Into::into),
                RecordSeq::from(source).message(),
            );
            assert!(reconstruct_model_request(&invalid, 9.into()).is_err());
        }
    }

    /// Checkpoints are validated when appended, so a journal always projects.
    #[tokio::test]
    async fn invalid_compactions_are_rejected_at_append() {
        let session = MemorySession::new().await;
        let (store, agent) = (&session.store, &session.agent);
        let append = async |event| store.append(agent.clone(), event).await.map(|r| r.sequence);
        let call = |id: &str, position| {
            let call = ToolCall::new(id, "exec", json!({})).unwrap();
            AssistantItem::tool_call(format!("item-{id}"), position, call)
        };
        let result = |id: &str| {
            Message::Tool(vec![ToolResult {
                call_id: id.into(),
                name: "exec".into(),
                result: json!({}),
                images: vec![],
                is_error: false,
            }])
        };
        let calls = append(committed(Message::Assistant(vec![
            call("a", 0),
            call("b", 1),
        ])))
        .await
        .unwrap();
        let first = append(committed(result("b"))).await.unwrap();
        let second = append(committed(result("a"))).await.unwrap();
        let research = append(committed(text_message("research"))).await.unwrap();
        let todos = append(SessionEvent::TodosReplaced {
            items: vec![todo("Keep")],
        })
        .await
        .unwrap();
        let agent_context = append(context(ModelPurpose::Agent, "p", "m", "s"))
            .await
            .unwrap();
        let summary_context = append(context(ModelPurpose::Compaction, "p", "m", "s"))
            .await
            .unwrap();
        let sources = [calls, first, second, research];
        let extends = HistoryLifetime::Extends;
        let request = append(requested(agent_context, None, &sources, "state", extends))
            .await
            .unwrap();
        let attempt = |request: RecordSeq| {
            SessionEvent::ModelAttemptStarted(AttemptRef {
                request: request.request(),
                attempt: 1,
            })
        };
        append(attempt(request)).await.unwrap();
        let detached = HistoryLifetime::Detached;
        let summary = append(requested(summary_context, None, &sources, "sum", detached))
            .await
            .unwrap();
        append(attempt(summary)).await.unwrap();
        let checkpoint = |frontier: RecordSeq, retained: Vec<RecordSeq>, request: RecordSeq| {
            CompactionCheckpoint {
                frontier,
                message: text_message("summary"),
                todos: vec![todo("Keep")],
                retained: retained.iter().map(|source| source.message()).collect(),
                attempt: AttemptRef {
                    request: request.request(),
                    attempt: 1,
                },
                before_tokens: 100,
                after_tokens: 10,
            }
        };
        let valid = checkpoint(todos, vec![calls, first, second], summary);
        let invalid = |retained| CompactionCheckpoint {
            retained,
            ..valid.clone()
        };
        let messages =
            |sources: &[RecordSeq]| sources.iter().map(|source| source.message()).collect();
        for (reason, checkpoint) in [
            ("duplicate retained", invalid(messages(&[calls, calls]))),
            ("unordered retained", invalid(messages(&[first, calls]))),
            (
                "retained after the frontier",
                invalid(messages(&[agent_context])),
            ),
            ("call without its results", invalid(messages(&[calls]))),
            (
                "results without their call",
                invalid(messages(&[first, second])),
            ),
            (
                "frontier after the request",
                checkpoint(summary, vec![], summary),
            ),
            (
                "frontier in the future",
                checkpoint((summary.get() + 5).into(), vec![], summary),
            ),
            ("agent request", checkpoint(todos, vec![], request)),
            (
                "assistant message",
                CompactionCheckpoint {
                    message: Message::Assistant(vec![]),
                    ..valid.clone()
                },
            ),
            (
                "blank todo",
                CompactionCheckpoint {
                    todos: vec![todo(" ")],
                    ..valid.clone()
                },
            ),
            ("stale todos", checkpoint(research, vec![], summary)),
        ] {
            let rejected = append(SessionEvent::Compaction { checkpoint }).await;
            assert!(
                matches!(rejected, Err(SessionError::ModelRequestReplay { .. })),
                "{reason}: {rejected:?}"
            );
        }
        // A loaded journal is admitted by the same rules, so a checkpoint append
        // would refuse never reaches projection.
        let mut corrupt = store.records().await;
        let mut record = corrupt.last().unwrap().clone();
        record.sequence = record.sequence.next();
        record.event = SessionEvent::Compaction {
            checkpoint: invalid(messages(&[calls, calls])),
        };
        corrupt.push(record);
        assert!(matches!(
            admit_records(corrupt),
            Err(SessionError::ModelRequestReplay { .. })
        ));
        let installed = append(SessionEvent::Compaction { checkpoint: valid })
            .await
            .unwrap();
        let records = admit_records(store.records().await).unwrap();
        let projected = project_history(&records, agent);
        assert_eq!(projected.checkpoint, Some(installed));
        assert_eq!(
            projected.sources(),
            [calls, first, second].map(|source| source.message())
        );
    }

    #[test]
    fn bound_reasoning_does_not_survive_compaction() {
        let (agent, mut records) = projection_fixture();
        let replay = |binding| {
            envelope(
                ReplayFormat::Messages,
                "model",
                json!({"signature":"opaque"}),
                binding,
            )
        };
        let (portable, bound) = (replay(Binding::Free), replay(Binding::Conversation));
        let message = |replay| {
            Message::Assistant(vec![
                AssistantItem::reasoning("bound", 0, "visible", replay),
                AssistantItem::reasoning("portable", 1, "kept", Some(portable.clone())),
                AssistantItem::text("said", 2, "answer"),
            ])
        };
        let (signed, stripped) = (message(Some(bound)), message(None));
        records[0].event = committed(signed.clone());
        // Retained after each checkpoint, and sent without it by both summary requests.
        assert_eq!(project_history(&records, &agent).messages[1].1, stripped);
        for (request, index) in [(5, 0), (9, 1)] {
            let (_, request) = reconstruct_model_request(&records, request.into()).unwrap();
            assert_eq!(request.history[index], stripped.render());
        }
        // Bound reasoning after the latest checkpoint is kept; replay matches projection.
        let append = |records: &mut Vec<EventRecord>, sequence: u64, event: SessionEvent| {
            let mut record = records[8].clone();
            (record.sequence, record.event) = (sequence.into(), event);
            records.push(record);
        };
        append(&mut records, 11, committed(signed.clone()));
        let lifetime = HistoryLifetime::Extends;
        let state = |sources: &[u64]| {
            let sources: Vec<RecordSeq> = sources.iter().copied().map(Into::into).collect();
            requested(3.into(), Some(10.into()), &sources, "state", lifetime)
        };
        append(&mut records, 12, state(&[1, 6, 11]));
        let projected = project_history(&records, &agent);
        assert_eq!(projected.messages[3].1, signed);
        let replays_projection = |records: &[EventRecord], request: u64, projected: &Projection| {
            let (_, request) = reconstruct_model_request(records, request.into()).unwrap();
            let rendered: Vec<_> = projected
                .history()
                .map(|message| message.render())
                .collect();
            assert_eq!(request.history, rendered);
        };
        replays_projection(&records, 12, &projected);
        // A mode switch changes the conversation too: bound reasoning before it is
        // dropped from later requests, while the earlier request replays as sent.
        let mode = crate::session::ModeSelection {
            name: "plan".into(),
            definition: None,
        };
        let capabilities = Vec::new();
        append(
            &mut records,
            13,
            SessionEvent::ModeChanged { mode, capabilities },
        );
        append(&mut records, 14, committed(signed.clone()));
        append(&mut records, 15, state(&[1, 6, 11, 14]));
        let switched = project_history(&records, &agent);
        assert_eq!(
            (&switched.messages[3].1, &switched.messages[4].1),
            (&stripped, &signed)
        );
        replays_projection(&records, 15, &switched);
        replays_projection(&records, 12, &projected);
    }
}

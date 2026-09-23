//! Reconstruct model-visible history and exact provider requests from durable events.

use crate::{
    identity::AgentId,
    provider::protocol::{HistoryLifetime, Message, ModelRequest},
};

use super::{EventRecord, ModelPurpose, SessionError, SessionEvent};

/// Tool results are committed one message per call as each call finishes. Providers
/// receive one tool message per exchange, ordered like the calls that produced it.
#[must_use]
pub fn merge_tool_results(messages: impl IntoIterator<Item = Message>) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::new();
    let mut calls: Vec<String> = Vec::new();
    for message in messages {
        match message {
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

/// Project the latest committed compaction and subsequent messages for one agent.
/// Sequence IDs refer to original message events or the compaction event itself.
pub fn project_history(
    records: &[EventRecord],
    agent: &AgentId,
) -> Result<Vec<(u64, Message)>, SessionError> {
    let checkpoint = records.iter().rposition(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::Compaction { .. })
    });
    let mut result = Vec::new();
    let frontier = if let Some(index) = checkpoint {
        let record = &records[index];
        validate_compaction(&records[..index], record)?;
        let SessionEvent::Compaction { checkpoint } = &record.event else {
            unreachable!()
        };
        result.push((record.sequence, checkpoint.message.clone()));
        for sequence in &checkpoint.retained {
            let source = records[..index]
                .iter()
                .find(|source| source.sequence == *sequence)
                .unwrap();
            let SessionEvent::MessageCommitted { message } = &source.event else {
                unreachable!()
            };
            if let Some(message) = message.clone().without_bound_reasoning() {
                result.push((*sequence, message));
            }
        }
        checkpoint.frontier
    } else {
        0
    };
    let switched = mode_boundary(records, agent);
    result.extend(records.iter().filter_map(|record| {
        if &record.agent == agent
            && record.sequence > frontier
            && let SessionEvent::MessageCommitted { message } = &record.event
        {
            let message = message.clone();
            return if record.sequence < switched {
                message.without_bound_reasoning()
            } else {
                Some(message)
            }
            .map(|message| (record.sequence, message));
        }
        None
    }));
    Ok(result)
}

/// A mode switch replaces the system prompt and tools, which invalidates bound
/// reasoning before it exactly as compaction does.
fn mode_boundary(records: &[EventRecord], agent: &AgentId) -> u64 {
    let switched = records.iter().rev().find(|record| {
        &record.agent == agent && matches!(record.event, SessionEvent::ModeChanged { .. })
    });
    switched.map_or(0, |record| record.sequence)
}

pub(super) fn validate_compaction(
    preceding: &[EventRecord],
    record: &EventRecord,
) -> Result<(), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay {
        sequence: record.sequence,
        reason,
    };
    let SessionEvent::Compaction { checkpoint } = &record.event else {
        return Ok(());
    };
    if checkpoint.schema_version != 2 {
        return Err(invalid("unsupported compaction schema version"));
    }
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
        || checkpoint.frontier > preceding.last().map_or(0, |r| r.sequence)
    {
        return Err(invalid("compaction frontier must precede its event"));
    }
    let previous = preceding.iter().rev().find(|source| {
        source.agent == record.agent && matches!(source.event, SessionEvent::Compaction { .. })
    });
    if checkpoint.previous != previous.map(|source| source.sequence) {
        return Err(invalid(
            "compaction must reference the preceding checkpoint",
        ));
    }
    if let Some(EventRecord {
        event: SessionEvent::Compaction { checkpoint: old },
        ..
    }) = previous
        && checkpoint.frontier < old.frontier
    {
        return Err(invalid("compaction frontier must not move backwards"));
    }
    if !preceding.iter().any(|source| {
        source.sequence == checkpoint.request
            && source.agent == record.agent
            && checkpoint.frontier < source.sequence
            && matches!(
                source.event,
                SessionEvent::ModelRequested {
                    purpose: ModelPurpose::Compaction,
                    ..
                }
            )
    }) {
        return Err(invalid(
            "compaction request must follow its frontier, precede its event and belong to the same agent",
        ));
    }
    let mut last = 0;
    for sequence in &checkpoint.retained {
        if *sequence <= last || *sequence > checkpoint.frontier {
            return Err(invalid(
                "retained messages must be unique, chronological and covered by the frontier",
            ));
        }
        if !preceding.iter().any(|source| {
            source.sequence == *sequence
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
            .binary_search(&originals[index].sequence)
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

/// Return the configured provider name and exact request at a `ModelRequested` event.
/// Records must be ordered by sequence, as returned by `SessionStore`.
/// Attachment and image references keep their blob digests; use
/// `SessionStore::load_blobs` to load their contents for provider encoding.
pub fn reconstruct_model_request(
    records: &[EventRecord],
    sequence: u64,
) -> Result<(String, ModelRequest), SessionError> {
    let index = records
        .binary_search_by_key(&sequence, |record| record.sequence)
        .map_err(|_| SessionError::ModelRequestReplay {
            sequence,
            reason: "event not found",
        })?;
    let mut history = Vec::new();
    let (provider, template, tail, history_lifetime) = visit_request(
        &records[index],
        |source| {
            records[..index]
                .binary_search_by_key(&source, |record| record.sequence)
                .ok()
                .map(|position| &records[position])
        },
        |message| history.push(message.clone()),
    )?;
    let SessionEvent::ModelRequested {
        history: sources,
        purpose,
        ..
    } = &records[index].event
    else {
        unreachable!("visit_request accepts only model requests")
    };
    // As in projection and summaries, bound reasoning never enters a changed conversation.
    let frontier = sources.first().and_then(|source| {
        let position = records[..index]
            .binary_search_by_key(source, |record| record.sequence)
            .ok()?;
        match &records[position].event {
            SessionEvent::Compaction { checkpoint } => Some(checkpoint.frontier),
            _ => None,
        }
    });
    let switched = mode_boundary(&records[..index], &records[index].agent);
    // Unencodable history drops out of the pairing itself, which depends on
    // `sources` and `history` being index-aligned.
    let history = sources.iter().zip(history).filter_map(|(source, message)| {
        if *purpose == ModelPurpose::Compaction
            || *source < switched
            || frontier.is_some_and(|last| *source <= last)
        {
            message.without_bound_reasoning()
        } else {
            (!message.is_content_free()).then_some(message)
        }
    });
    let request = ModelRequest {
        history: merge_tool_results(history),
        tail: tail.to_vec(),
        history_lifetime,
        ..template
    };
    Ok((provider, request))
}

fn visit_request<'a>(
    call: &'a EventRecord,
    mut lookup: impl FnMut(u64) -> Option<&'a EventRecord>,
    mut visit: impl FnMut(&'a Message),
) -> Result<(String, ModelRequest, &'a [Message], HistoryLifetime), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay {
        sequence: call.sequence,
        reason,
    };
    let SessionEvent::ModelRequested {
        context,
        history,
        tail,
        history_lifetime,
        ..
    } = &call.event
    else {
        return Err(invalid("event is not a model request"));
    };
    let context = lookup(*context)
        .filter(|record| record.agent == call.agent)
        .ok_or_else(|| invalid("context must precede the call and belong to the same agent"))?;
    let SessionEvent::ModelContext { context } = &context.event else {
        return Err(invalid("referenced event is not a model context"));
    };
    for sequence in history {
        let source = lookup(*sequence)
            .filter(|record| record.agent == call.agent)
            .ok_or_else(|| {
                invalid("message source must precede the call and belong to the same agent")
            })?;
        visit(match &source.event {
            SessionEvent::MessageCommitted { message } => message,
            SessionEvent::Compaction { checkpoint } => &checkpoint.message,
            _ => {
                return Err(invalid(
                    "referenced event does not contain a conversation message",
                ));
            }
        });
    }
    Ok((
        context.profile.profile.provider.clone(),
        context.template(),
        tail,
        *history_lifetime,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        provider::protocol::{
            AssistantItem, Binding, Provenance, Replay, Scope, SystemSegment, ToolDefinition,
            ToolResult, UserContent,
        },
        session::SessionStore,
    };
    use serde_json::json;

    fn text_message(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    fn committed(message: Message) -> SessionEvent {
        SessionEvent::MessageCommitted { message }
    }

    fn requested(
        context: u64,
        purpose: ModelPurpose,
        history: &[u64],
        tail: &str,
        history_lifetime: HistoryLifetime,
    ) -> SessionEvent {
        SessionEvent::ModelRequested {
            context,
            history: history.to_vec(),
            tail: vec![text_message(tail)],
            history_lifetime,
            purpose,
        }
    }

    fn context(provider: &str, model: &str, system: &str) -> SessionEvent {
        let mut profile = crate::session::fixture::profile();
        profile.profile.provider = provider.into();
        profile.profile.model = model.into();
        profile.profile.reasoning = Some("high".into());
        SessionEvent::ModelContext {
            context: crate::session::ModelContext {
                purpose: ModelPurpose::Agent,
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

    #[tokio::test]
    async fn replay_preserves_context_boundaries_and_image_payloads() {
        let (directory, store, agent) = crate::session::fixture::on_disk().await;
        let child = agent.child(1);
        let child_start =
            crate::session::fixture::agent_started(Some(agent.clone()), directory.path());
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
            UserContent::Text {
                text: "inspect".into(),
            },
            UserContent::Attachment {
                attachment: crate::media::AttachmentRef::Image(image.clone()),
            },
            UserContent::Attachment {
                attachment: notes.clone(),
            },
        ]);
        let envelope = Replay {
            provenance: Provenance {
                protocol: "test".into(),
                model: "original-model".into(),
                scope: Scope::try_from("reasoning".to_owned()).unwrap(),
            },
            // Opaque to the journal: nesting, key order, numbers and escapes must survive.
            payload: json!({"signature":"pre\"serve\u{e9}","a":[1.5,{"z":null,"b":-0.0}],"n":1e300}),
            binding: Binding::Free,
        };
        let reasoning = AssistantItem::reasoning("reasoning-item", 0, "reasoning", Some(envelope));
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
            ModelPurpose::Agent,
            &sources,
            "exact call-time state",
            history_lifetime,
        );
        let call = append(&agent, request).await;
        append(&agent, context_event_changed()).await;
        append(&agent, committed(text_message("future message"))).await;
        let id = store.id();
        drop(store);
        let (store, records) = SessionStore::open(directory.path(), id).await.unwrap();
        let (provider, mut restored) = reconstruct_model_request(&records, call.sequence).unwrap();
        assert_eq!(provider, "original-provider");
        let mut expected = template;
        (expected.history, expected.history_lifetime) =
            (vec![user, assistant, tool], history_lifetime);
        expected.tail = vec![text_message("exact call-time state")];
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

        let index = records
            .iter()
            .position(|r| r.sequence == call.sequence)
            .unwrap();
        for mutation in 0..2 {
            let mut invalid = records.clone();
            let SessionEvent::ModelRequested {
                context, history, ..
            } = &mut invalid[index].event
            else {
                unreachable!()
            };
            match mutation {
                0 => history[0] = child_context.sequence,
                _ => *context = child_context.sequence,
            }
            assert!(reconstruct_model_request(&invalid, call.sequence).is_err());
        }
        assert!(reconstruct_model_request(&records, 0).is_err());
        assert!(reconstruct_model_request(&records, child_context.sequence).is_err());
    }

    fn context_event_changed() -> SessionEvent {
        context("new-provider", "new-model", "new instructions")
    }

    fn projection_fixture() -> (AgentId, Vec<EventRecord>) {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let checkpoint = super::super::CompactionCheckpoint {
            schema_version: 2,
            todos: Vec::new(),
            previous: None,
            frontier: 2,
            message: text_message("first summary"),
            retained: vec![1],
            request: 4,
            attempt: 1,
            before_tokens: 100_000,
            after_tokens: 10_000,
        };
        let compaction = |history: &[u64], tail| {
            requested(
                3,
                ModelPurpose::Compaction,
                history,
                tail,
                HistoryLifetime::Detached,
            )
        };
        let events = vec![
            committed(text_message("verbatim plan")),
            committed(text_message("research")),
            context("provider", "model", "system"),
            compaction(&[1, 2], "summarize"),
            committed(text_message("concurrent steering")),
            SessionEvent::Compaction {
                checkpoint: checkpoint.clone(),
            },
            committed(text_message("continued work")),
            compaction(&[6, 1, 5, 7], "state at request time"),
            SessionEvent::Compaction {
                checkpoint: super::super::CompactionCheckpoint {
                    previous: Some(6),
                    frontier: 7,
                    message: text_message("second summary with inherited plan"),
                    retained: vec![1, 5],
                    request: 8,
                    ..checkpoint
                },
            },
        ];
        let records = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| EventRecord {
                id: crate::identity::EventId::generate().unwrap(),
                sequence: index as u64 + 1,
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })
            .collect();
        (agent, records)
    }

    /// A copy of `records` whose checkpoint at `index` has been edited.
    fn with_checkpoint(
        records: &[EventRecord],
        index: usize,
        edit: impl FnOnce(&mut super::super::CompactionCheckpoint),
    ) -> Vec<EventRecord> {
        let mut records = records.to_vec();
        let SessionEvent::Compaction { checkpoint } = &mut records[index].event else {
            unreachable!()
        };
        edit(checkpoint);
        records
    }

    fn sequences(projection: &[(u64, Message)]) -> Vec<u64> {
        projection.iter().map(|(sequence, _)| *sequence).collect()
    }

    #[test]
    fn projection_preserves_concurrent_messages_and_repeated_compaction() {
        let (agent, records) = projection_fixture();
        assert_eq!(
            sequences(&project_history(&records[..5], &agent).unwrap()),
            [1, 2, 5]
        );
        let first = project_history(&records[..8], &agent).unwrap();
        assert_eq!(sequences(&first), [6, 1, 5, 7]);
        assert_eq!(
            (&first[1].1, &first[2].1),
            (
                &text_message("verbatim plan"),
                &text_message("concurrent steering")
            )
        );
        let second = project_history(&records, &agent).unwrap();
        assert_eq!(sequences(&second), [9, 1, 5]);
        assert_eq!(second[1].1, text_message("verbatim plan"));
        assert!(
            project_history(&records, &agent.child(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn invalid_compaction_and_request_references_are_rejected() {
        let (agent, records) = projection_fixture();
        for retained in [vec![1, 1], vec![5, 1], vec![8], vec![3], vec![6]] {
            let invalid = with_checkpoint(&records, 8, |checkpoint| checkpoint.retained = retained);
            assert!(project_history(&invalid, &agent).is_err());
        }
        let mut invalid = records.clone();
        invalid[0].agent = agent.child(1);
        assert!(project_history(&invalid, &agent).is_err());
        let mut invalid = records.clone();
        let SessionEvent::ModelRequested { purpose, .. } = &mut invalid[7].event else {
            unreachable!()
        };
        *purpose = ModelPurpose::Agent;
        assert!(project_history(&invalid, &agent).is_err());
        for (previous, frontier, request, message) in [
            (None, 7, 8, text_message("summary")),
            (Some(6), 9, 8, text_message("summary")),
            (Some(6), 1, 8, text_message("summary")),
            (Some(6), 7, 7, text_message("summary")),
            (Some(6), 7, 8, Message::Assistant(vec![])),
        ] {
            let invalid = with_checkpoint(&records, 8, |checkpoint| {
                (checkpoint.previous, checkpoint.frontier) = (previous, frontier);
                (checkpoint.request, checkpoint.message) = (request, message);
            });
            assert!(project_history(&invalid, &agent).is_err());
        }
        for sequence in [0, 3, 8, 9] {
            let mut invalid = records.clone();
            let SessionEvent::ModelRequested { history, .. } = &mut invalid[7].event else {
                unreachable!()
            };
            history[0] = sequence;
            assert!(reconstruct_model_request(&invalid, 8).is_err());
        }
    }

    #[test]
    fn checkpoint_cannot_cut_or_mismatch_parallel_tool_exchanges() {
        let (agent, records) = projection_fixture();
        let calls = ["a", "b"];
        let call = |(position, id): (u32, &&str)| {
            let call = crate::provider::protocol::ToolCall::new(*id, "shell", json!({})).unwrap();
            AssistantItem::tool_call(format!("item-{id}"), position, call)
        };
        let result = |id: &&str| ToolResult {
            call_id: (*id).into(),
            name: "shell".into(),
            result: json!({}),
            images: vec![],
            is_error: false,
        };
        let mut records =
            with_checkpoint(&records, 5, |checkpoint| checkpoint.retained = vec![1, 2]);
        records[0].event = committed(Message::Assistant((0..).zip(&calls).map(call).collect()));
        records[1].event = committed(Message::Tool(calls.iter().rev().map(result).collect()));
        let records = &records[..6];
        assert!(project_history(records, &agent).is_ok());
        for retained in [vec![1], vec![2]] {
            let invalid = with_checkpoint(records, 5, |checkpoint| checkpoint.retained = retained);
            assert!(project_history(&invalid, &agent).is_err());
        }
        for mutation in 0..3 {
            let mut invalid = records.to_vec();
            if mutation == 2 {
                invalid[1].agent = agent.child(1);
            } else {
                let SessionEvent::MessageCommitted {
                    message: Message::Tool(results),
                } = &mut invalid[1].event
                else {
                    unreachable!()
                };
                if mutation == 0 {
                    results.pop();
                } else {
                    results[0].call_id = "unrelated".into();
                }
            }
            assert!(
                project_history(&invalid, &agent).is_err(),
                "mutation {mutation}"
            );
        }
    }

    #[test]
    fn bound_reasoning_does_not_survive_compaction() {
        let (agent, mut records) = projection_fixture();
        let portable = Replay {
            provenance: Provenance {
                protocol: "test".into(),
                model: "model".into(),
                scope: Scope::try_from("scope".to_owned()).unwrap(),
            },
            payload: json!({"signature":"opaque"}),
            binding: Binding::Free,
        };
        let bound = Replay {
            binding: Binding::Conversation,
            ..portable.clone()
        };
        let message = |replay| {
            Message::Assistant(vec![
                AssistantItem::reasoning("bound", 0, "visible", replay),
                AssistantItem::reasoning("portable", 1, "kept", Some(portable.clone())),
            ])
        };
        let (signed, stripped) = (message(Some(bound)), message(None));
        records[0].event = committed(signed.clone());
        // Retained after each checkpoint, and sent without it by both summary requests.
        assert_eq!(project_history(&records, &agent).unwrap()[1].1, stripped);
        for (request, index) in [(4, 0), (8, 1)] {
            let (_, request) = reconstruct_model_request(&records, request).unwrap();
            assert_eq!(request.history[index], stripped);
        }
        // Bound reasoning after the latest checkpoint is kept; replay matches projection.
        let append = |records: &mut Vec<EventRecord>, sequence: u64, event: SessionEvent| {
            let mut record = records[8].clone();
            (record.sequence, record.event) = (sequence, event);
            records.push(record);
        };
        append(&mut records, 10, committed(signed.clone()));
        let lifetime = HistoryLifetime::Extends;
        append(
            &mut records,
            11,
            requested(3, ModelPurpose::Agent, &[9, 1, 5, 10], "state", lifetime),
        );
        let projected = project_history(&records, &agent).unwrap();
        assert_eq!(projected[3].1, signed);
        let replays_projection =
            |records: &[EventRecord], request, projected: &[(u64, Message)]| {
                let (_, request) = reconstruct_model_request(records, request).unwrap();
                let projected = projected.iter().map(|(_, message)| message);
                assert!(request.history.iter().eq(projected));
            };
        replays_projection(&records, 11, &projected);
        // A mode switch changes the conversation too: bound reasoning before it is
        // dropped from later requests, while the earlier request replays as sent.
        let mode = crate::session::ModeSelection {
            name: "plan".into(),
            definition: None,
        };
        let capabilities = Vec::new();
        append(
            &mut records,
            12,
            SessionEvent::ModeChanged { mode, capabilities },
        );
        append(&mut records, 13, committed(signed.clone()));
        let sources = &[9, 1, 5, 10, 13];
        append(
            &mut records,
            14,
            requested(3, ModelPurpose::Agent, sources, "state", lifetime),
        );
        let switched = project_history(&records, &agent).unwrap();
        assert_eq!((&switched[3].1, &switched[4].1), (&stripped, &signed));
        replays_projection(&records, 14, &switched);
        replays_projection(&records, 11, &projected);
    }
}

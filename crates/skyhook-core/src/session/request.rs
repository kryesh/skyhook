//! Reconstruct model-visible history and exact provider requests from durable events.

use crate::{
    identity::AgentId,
    provider::protocol::{AssistantContent, Message, ModelRequest},
};

use super::{ContextMessage, EventRecord, ModelPurpose, SessionError, SessionEvent};

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
            result.push((*sequence, message.clone()));
        }
        checkpoint.frontier
    } else {
        0
    };
    result.extend(records.iter().filter_map(|record| {
        if &record.agent == agent
            && record.sequence > frontier
            && let SessionEvent::MessageCommitted { message } = &record.event
        {
            return Some((record.sequence, message.clone()));
        }
        None
    }));
    Ok(result)
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
    if checkpoint.schema_version != 1 {
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
    // original adjacent assistant/result exchange rather than severing a tool call.
    let originals: Vec<_> = preceding
        .iter()
        .filter(|source| {
            source.agent == record.agent
                && source.sequence <= checkpoint.frontier
                && matches!(source.event, SessionEvent::MessageCommitted { .. })
        })
        .collect();
    for (index, source) in originals.iter().enumerate() {
        if checkpoint.retained.binary_search(&source.sequence).is_err() {
            continue;
        }
        let SessionEvent::MessageCommitted { message } = &source.event else {
            unreachable!()
        };
        let companion = match message {
            Message::Assistant(content)
                if content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_))) =>
            {
                index.checked_add(1)
            }
            Message::Tool(_) => index.checked_sub(1),
            _ => continue,
        }
        .and_then(|index| originals.get(index));
        let Some(companion) = companion else {
            return Err(invalid("retained tool exchange is incomplete"));
        };
        if checkpoint
            .retained
            .binary_search(&companion.sequence)
            .is_err()
        {
            return Err(invalid("retained tool exchange is incomplete"));
        }
        let SessionEvent::MessageCommitted { message: other } = &companion.event else {
            unreachable!()
        };
        let (assistant, tool) = if matches!(message, Message::Tool(_)) {
            (other, message)
        } else {
            (message, other)
        };
        if !valid_tool_pair(assistant, tool) {
            return Err(invalid("retained tool call and results do not match"));
        }
    }
    Ok(())
}

fn valid_tool_pair(assistant: &Message, tool: &Message) -> bool {
    let (Message::Assistant(content), Message::Tool(results)) = (assistant, tool) else {
        return false;
    };
    let calls: Vec<_> = content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some((&call.id, &call.name)),
            _ => None,
        })
        .collect();
    let call_set: std::collections::HashSet<_> = calls.iter().copied().collect();
    let result_set: std::collections::HashSet<_> = results
        .iter()
        .map(|result| (&result.call_id, &result.name))
        .collect();
    !calls.is_empty()
        && calls.len() == call_set.len()
        && results.len() == result_set.len()
        && call_set == result_set
}

/// Return the configured provider name and exact request at a `ModelRequested` event.
/// Image references retain their journaled blob metadata; use
/// `SessionStore::hydrate_model_request` to restore request-local payloads.
pub fn reconstruct_model_request(
    records: &[EventRecord],
    sequence: u64,
) -> Result<(String, ModelRequest), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay { sequence, reason };
    let index = records
        .iter()
        .position(|record| record.sequence == sequence)
        .ok_or_else(|| invalid("event not found"))?;
    reconstruct_from_prefix(&records[..index], &records[index])
}

pub(super) fn reconstruct_from_prefix(
    records: &[EventRecord],
    call: &EventRecord,
) -> Result<(String, ModelRequest), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay {
        sequence: call.sequence,
        reason,
    };
    let SessionEvent::ModelRequested {
        context, messages, ..
    } = &call.event
    else {
        return Err(invalid("event is not a model request"));
    };
    let context = records
        .iter()
        .find(|record| record.sequence == *context && record.agent == call.agent)
        .ok_or_else(|| invalid("context must precede the call and belong to the same agent"))?;
    let SessionEvent::ModelContext { provider, template } = &context.event else {
        return Err(invalid("referenced event is not a model context"));
    };
    if !template.messages.is_empty() {
        return Err(invalid(
            "model context must not duplicate conversation history",
        ));
    }
    let mut request = template.clone();
    for message in messages {
        request.messages.push(match message {
            ContextMessage::Inline { message } => message.clone(),
            ContextMessage::Source { sequence } => {
                let source = records
                    .iter()
                    .find(|record| record.sequence == *sequence && record.agent == call.agent)
                    .ok_or_else(|| {
                        invalid("message source must precede the call and belong to the same agent")
                    })?;
                match &source.event {
                    SessionEvent::MessageCommitted { message } => message.clone(),
                    SessionEvent::Compaction { checkpoint } => checkpoint.message.clone(),
                    _ => {
                        return Err(invalid(
                            "referenced event does not contain a conversation message",
                        ));
                    }
                }
            }
        });
    }
    Ok((provider.clone(), request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        provider::protocol::{
            AssistantContent, SystemSegment, ToolDefinition, ToolResult, UserContent,
        },
        session::SessionStore,
    };
    use serde_json::json;

    #[tokio::test]
    async fn replay_preserves_context_boundaries_and_image_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let child = agent.child(1);
        let image = store
            .import_blob(b"image payload", "test.png".into(), "image/png".into())
            .await
            .unwrap();
        let user = Message::User(vec![
            UserContent::Text {
                text: "inspect".into(),
            },
            UserContent::Image {
                image: image.clone(),
            },
        ]);
        let assistant = Message::Assistant(vec![AssistantContent::Reasoning {
            text: "reasoning".into(),
            opaque: Some(json!({"signature":"preserve"})),
        }]);
        let tool = Message::Tool(vec![ToolResult {
            call_id: "read-1".into(),
            name: "read".into(),
            result: json!({"ok":true}),
            console_output: "console output".into(),
            images: vec![image],
            is_error: false,
        }]);
        for message in [&user, &assistant, &tool] {
            store
                .append(
                    agent.clone(),
                    SessionEvent::MessageCommitted {
                        message: message.clone(),
                    },
                )
                .await
                .unwrap();
        }
        let template = ModelRequest {
            model: "original-model".into(),
            system: vec![SystemSegment {
                text: "original instructions".into(),
                cache: true,
            }],
            messages: vec![],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "original description".into(),
                input_schema: json!({"type":"object"}),
            }],
            reasoning: Some("high".into()),
            response_schema: Some(crate::provider::protocol::ResponseSchema {
                name: "answer".into(),
                schema: json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}),
            }),
            max_output_tokens: Some(4096),
            correlation: Some(agent.to_string()),
        };
        let context = store
            .append(
                agent.clone(),
                SessionEvent::ModelContext {
                    provider: "original-provider".into(),
                    template: template.clone(),
                },
            )
            .await
            .unwrap();
        let child_context = store
            .append(
                child.clone(),
                SessionEvent::ModelContext {
                    provider: "child-provider".into(),
                    template: template.clone(),
                },
            )
            .await
            .unwrap();
        store
            .append(
                child,
                SessionEvent::MessageCommitted {
                    message: Message::User(vec![UserContent::Text {
                        text: "child only".into(),
                    }]),
                },
            )
            .await
            .unwrap();
        let runtime = UserContent::Runtime {
            text: "exact call-time state".into(),
        };
        let call = store
            .append(
                agent.clone(),
                SessionEvent::ModelRequested {
                    context: context.sequence,
                    purpose: super::super::ModelPurpose::Agent,
                    messages: vec![
                        ContextMessage::Source { sequence: 1 },
                        ContextMessage::Source { sequence: 2 },
                        ContextMessage::Source { sequence: 3 },
                        ContextMessage::Inline {
                            message: Message::User(vec![runtime.clone()]),
                        },
                    ],
                },
            )
            .await
            .unwrap();
        let mut changed = template.clone();
        changed.model = "new-model".into();
        changed.system[0].text = "new instructions".into();
        store
            .append(
                agent.clone(),
                SessionEvent::ModelContext {
                    provider: "new-provider".into(),
                    template: changed,
                },
            )
            .await
            .unwrap();
        store
            .append(
                agent,
                SessionEvent::MessageCommitted {
                    message: Message::User(vec![UserContent::Text {
                        text: "future message".into(),
                    }]),
                },
            )
            .await
            .unwrap();
        let id = store.id();
        store.close().await.unwrap();
        let (store, records) = SessionStore::open(directory.path(), id).await.unwrap();
        let (provider, mut restored) = reconstruct_model_request(&records, call.sequence).unwrap();
        assert_eq!(provider, "original-provider");
        let mut expected = template;
        expected.messages = vec![user, assistant, tool, Message::User(vec![runtime])];
        assert_eq!(restored, expected);
        store.hydrate_model_request(&mut restored).await.unwrap();
        store.hydrate_model_request(&mut expected).await.unwrap();
        assert_eq!(restored, expected);
        let Message::User(content) = &restored.messages[0] else {
            panic!("user message")
        };
        let UserContent::Image { image } = &content[1] else {
            panic!("image")
        };
        assert!(image.data_base64.is_some());

        let index = records
            .iter()
            .position(|r| r.sequence == call.sequence)
            .unwrap();
        let mut invalid = records.clone();
        let SessionEvent::ModelRequested { context, .. } = &mut invalid[index].event else {
            unreachable!()
        };
        *context = child_context.sequence;
        assert!(reconstruct_model_request(&invalid, call.sequence).is_err());
        invalid = records.clone();
        let SessionEvent::ModelRequested { messages, .. } = &mut invalid[index].event else {
            unreachable!()
        };
        messages[0] = ContextMessage::Source {
            sequence: child_context.sequence,
        };
        assert!(reconstruct_model_request(&invalid, call.sequence).is_err());
        assert!(reconstruct_model_request(&records, 0).is_err());
        assert!(reconstruct_model_request(&records, child_context.sequence).is_err());
    }

    fn text_message(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    fn projection_fixture() -> (AgentId, Vec<EventRecord>) {
        let id = crate::identity::SessionId::generate().unwrap();
        let agent = AgentId::root(id);
        let checkpoint = super::super::CompactionCheckpoint {
            schema_version: 1,
            todos: Vec::new(),
            previous: None,
            frontier: 2,
            message: text_message("first summary"),
            retained: vec![1],
            request: 4,
            max_context: 128_000,
            before_tokens: 100_000,
            after_tokens: 10_000,
        };
        let events = vec![
            SessionEvent::MessageCommitted {
                message: text_message("verbatim plan"),
            },
            SessionEvent::MessageCommitted {
                message: text_message("research"),
            },
            SessionEvent::ModelContext {
                provider: "provider".into(),
                template: ModelRequest {
                    model: "model".into(),
                    system: vec![SystemSegment {
                        text: "system".into(),
                        cache: false,
                    }],
                    tools: vec![],
                    messages: vec![],
                    reasoning: None,
                    response_schema: None,
                    max_output_tokens: Some(4096),
                    correlation: None,
                },
            },
            SessionEvent::ModelRequested {
                context: 3,
                purpose: super::super::ModelPurpose::Compaction,
                messages: vec![
                    ContextMessage::Source { sequence: 1 },
                    ContextMessage::Source { sequence: 2 },
                    ContextMessage::Inline {
                        message: text_message("summarize"),
                    },
                ],
            },
            SessionEvent::MessageCommitted {
                message: text_message("concurrent steering"),
            },
            SessionEvent::Compaction {
                checkpoint: checkpoint.clone(),
            },
            SessionEvent::MessageCommitted {
                message: text_message("continued work"),
            },
            SessionEvent::ModelRequested {
                context: 3,
                purpose: super::super::ModelPurpose::Compaction,
                messages: vec![
                    ContextMessage::Source { sequence: 6 },
                    ContextMessage::Source { sequence: 1 },
                    ContextMessage::Source { sequence: 5 },
                    ContextMessage::Source { sequence: 7 },
                    ContextMessage::Inline {
                        message: text_message("state at request time"),
                    },
                ],
            },
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
                version: super::super::SESSION_FORMAT_VERSION,
                sequence: index as u64 + 1,
                timestamp_millis: 0,
                agent: agent.clone(),
                event,
            })
            .collect();
        (agent, records)
    }

    #[test]
    fn projection_preserves_concurrent_messages_and_repeated_compaction() {
        let (agent, records) = projection_fixture();
        let before = project_history(&records[..5], &agent).unwrap();
        assert_eq!(
            before.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
        let first = project_history(&records[..8], &agent).unwrap();
        assert_eq!(
            first.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![6, 1, 5, 7]
        );
        assert_eq!(first[1].1, text_message("verbatim plan"));
        assert_eq!(first[2].1, text_message("concurrent steering"));
        let second = project_history(&records, &agent).unwrap();
        assert_eq!(
            second.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![9, 1, 5]
        );
        assert_eq!(second[1].1, text_message("verbatim plan"));
        assert!(
            project_history(&records, &agent.child(1))
                .unwrap()
                .is_empty()
        );
        super::super::event::validate_records(&records, agent.session()).unwrap();
    }

    #[test]
    fn exact_replay_uses_explicit_order_and_compaction_sources() {
        let (_, records) = projection_fixture();
        let (_, before) = reconstruct_model_request(&records, 4).unwrap();
        assert_eq!(
            before.messages,
            vec![
                text_message("verbatim plan"),
                text_message("research"),
                text_message("summarize")
            ]
        );
        let (provider, after) = reconstruct_model_request(&records, 8).unwrap();
        assert_eq!(provider, "provider");
        assert_eq!(after.system[0].text, "system");
        assert_eq!(
            after.messages,
            vec![
                text_message("first summary"),
                text_message("verbatim plan"),
                text_message("concurrent steering"),
                text_message("continued work"),
                text_message("state at request time")
            ]
        );
        assert_eq!(
            reconstruct_model_request(&records[..8], 8).unwrap().1,
            after
        );
    }

    #[test]
    fn invalid_compaction_and_request_references_are_rejected() {
        let (agent, records) = projection_fixture();
        for retained in [vec![1, 1], vec![5, 1], vec![8], vec![3], vec![6]] {
            let mut invalid = records.clone();
            let SessionEvent::Compaction { checkpoint } = &mut invalid[8].event else {
                unreachable!()
            };
            checkpoint.retained = retained;
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
            let mut invalid = records.clone();
            let SessionEvent::Compaction { checkpoint } = &mut invalid[8].event else {
                unreachable!()
            };
            checkpoint.previous = previous;
            checkpoint.frontier = frontier;
            checkpoint.request = request;
            checkpoint.message = message;
            assert!(project_history(&invalid, &agent).is_err());
        }
        for sequence in [0, 3, 8, 9] {
            let mut invalid = records.clone();
            let SessionEvent::ModelRequested { messages, .. } = &mut invalid[7].event else {
                unreachable!()
            };
            messages[0] = ContextMessage::Source { sequence };
            assert!(reconstruct_model_request(&invalid, 8).is_err());
        }
    }

    #[test]
    fn checkpoint_cannot_cut_or_mismatch_parallel_tool_exchanges() {
        let (agent, mut records) = projection_fixture();
        let calls = ["a", "b"];
        records[0].event = SessionEvent::MessageCommitted {
            message: Message::Assistant(
                calls
                    .iter()
                    .map(|id| {
                        AssistantContent::ToolCall(crate::provider::protocol::ToolCall {
                            id: (*id).into(),
                            name: "shell".into(),
                            arguments: json!({}),
                        })
                    })
                    .collect(),
            ),
        };
        records[1].event = SessionEvent::MessageCommitted {
            message: Message::Tool(
                calls
                    .iter()
                    .rev()
                    .map(|id| ToolResult {
                        call_id: (*id).into(),
                        name: "shell".into(),
                        result: json!({}),
                        console_output: String::new(),
                        images: vec![],
                        is_error: false,
                    })
                    .collect(),
            ),
        };
        let SessionEvent::Compaction { checkpoint } = &mut records[5].event else {
            unreachable!()
        };
        checkpoint.retained = vec![1, 2];
        assert!(project_history(&records[..6], &agent).is_ok());
        for retained in [vec![1], vec![2]] {
            let mut invalid = records.clone();
            let SessionEvent::Compaction { checkpoint } = &mut invalid[5].event else {
                unreachable!()
            };
            checkpoint.retained = retained;
            assert!(project_history(&invalid[..6], &agent).is_err());
        }
        let mut invalid = records.clone();
        let SessionEvent::MessageCommitted {
            message: Message::Tool(results),
        } = &mut invalid[1].event
        else {
            unreachable!()
        };
        results.pop();
        assert!(project_history(&invalid[..6], &agent).is_err());
        let mut invalid = records.clone();
        let SessionEvent::MessageCommitted {
            message: Message::Tool(results),
        } = &mut invalid[1].event
        else {
            unreachable!()
        };
        results[0].call_id = "unrelated".into();
        assert!(project_history(&invalid[..6], &agent).is_err());
        let mut invalid = records.clone();
        invalid[1].agent = agent.child(1);
        assert!(project_history(&invalid[..6], &agent).is_err());
    }

    #[tokio::test]
    async fn compaction_commit_survives_reopen_and_invalid_commit_keeps_old_projection() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let (_, records) = projection_fixture();
        let agent = AgentId::root(store.id());
        for record in records {
            store.append(agent.clone(), record.event).await.unwrap();
        }
        let snapshot = store.records().await;
        let expected = project_history(&snapshot, &agent).unwrap();
        let mut invalid = snapshot.last().unwrap().event.clone();
        let SessionEvent::Compaction { checkpoint } = &mut invalid else {
            unreachable!()
        };
        checkpoint.retained = vec![999];
        assert!(store.append(agent.clone(), invalid).await.is_err());
        assert_eq!(store.records().await, snapshot);
        store.close().await.unwrap();
        let (reopened, records) = SessionStore::open(directory.path(), store.id())
            .await
            .unwrap();
        assert_eq!(project_history(&records, &agent).unwrap(), expected);
        assert_eq!(reopened.records().await, snapshot);
    }
}

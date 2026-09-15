//! Reconstruct model-visible history and exact provider requests from durable events.

use crate::{
    identity::AgentId,
    provider::protocol::{BlockContent, HistoryLifetime, Message, ModelRequest},
};

use super::{EventRecord, ModelPurpose, SessionError, SessionEvent};

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
            let mut message = message.clone();
            message.strip_bound_reasoning();
            result.push((*sequence, message));
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
    if !matches!(checkpoint.schema_version, 1 | 2) {
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
                    .flat_map(|item| &item.blocks)
                    .any(|block| matches!(&block.content, BlockContent::ToolCall(_))) =>
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
        .flat_map(|item| &item.blocks)
        .filter_map(|block| match &block.content {
            BlockContent::ToolCall(call) => Some((call.id(), call.name())),
            _ => None,
        })
        .collect();
    let call_set: std::collections::HashSet<_> = calls.iter().copied().collect();
    let result_set: std::collections::HashSet<_> = results
        .iter()
        .map(|result| (result.call_id.as_str(), result.name.as_str()))
        .collect();
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
    for (source, message) in sources.iter().zip(&mut history) {
        if *purpose == ModelPurpose::Compaction || frontier.is_some_and(|last| *source <= last) {
            message.strip_bound_reasoning();
        }
    }
    let request = ModelRequest {
        history,
        tail: tail.to_vec(),
        history_lifetime,
        ..super::ModelRequestTemplate::try_from(template.clone())?.into_request()
    };
    Ok((provider.to_owned(), request))
}

/// Validate against the contiguous, sequence-checked journal prefix without
/// cloning a request that the caller will discard.
pub(super) fn validate_request(
    records: &[EventRecord],
    call: &EventRecord,
) -> Result<(), SessionError> {
    visit_request(
        call,
        |sequence| records.get(usize::try_from(sequence.checked_sub(1)?).ok()?),
        |_| {},
    )?;
    Ok(())
}

fn visit_request<'a>(
    call: &'a EventRecord,
    mut lookup: impl FnMut(u64) -> Option<&'a EventRecord>,
    mut visit: impl FnMut(&'a Message),
) -> Result<(&'a str, &'a ModelRequest, &'a [Message], HistoryLifetime), SessionError> {
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
    let SessionEvent::ModelContext { provider, template } = &context.event else {
        return Err(invalid("referenced event is not a model context"));
    };
    if !template.history.is_empty() || !template.tail.is_empty() {
        return Err(invalid(
            "model context must not duplicate conversation history",
        ));
    }
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
    Ok((provider, template, tail, *history_lifetime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::AgentId,
        provider::protocol::{
            AssistantItem, ReplayEnvelope, SystemSegment, ToolDefinition, ToolResult, UserContent,
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

    #[tokio::test]
    async fn replay_preserves_context_boundaries_and_image_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::create(directory.path()).await.unwrap();
        let agent = AgentId::root(store.id());
        let child = agent.child(1);
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
        let envelope = ReplayEnvelope {
            version: 1,
            protocol: "test".into(),
            model: "original-model".into(),
            scope: "reasoning".into(),
            payload: json!({"signature":"preserve"}),
            conversation_bound: false,
        };
        let reasoning = AssistantItem::reasoning("reasoning-item", 0, "reasoning", Some(envelope));
        let assistant = Message::Assistant(vec![reasoning]);
        let tool = Message::Tool(vec![ToolResult {
            call_id: "read-1".into(),
            name: "read".into(),
            result: json!({"ok":true}),
            images: vec![image.clone()],
            is_error: false,
        }]);
        let append =
            async |agent: &AgentId, event| store.append(agent.clone(), event).await.unwrap();
        for message in [&user, &assistant, &tool] {
            append(&agent, committed(message.clone())).await;
        }
        let template = ModelRequest {
            model: "original-model".into(),
            system: vec![SystemSegment {
                text: "original instructions".into(),
                cache: true,
            }],
            history: vec![],
            tail: vec![],
            history_lifetime: HistoryLifetime::Continuing,
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
            blobs: Default::default(),
        };
        let context_event = |provider: &str, template: &ModelRequest| SessionEvent::ModelContext {
            provider: provider.into(),
            template: template.clone(),
        };
        let context = append(&agent, context_event("original-provider", &template)).await;
        let child_context = append(&child, context_event("child-provider", &template)).await;
        append(&child, committed(text_message("child only"))).await;
        let history_lifetime = HistoryLifetime::Ending;
        let request = requested(
            context.sequence,
            ModelPurpose::Agent,
            &[1, 2, 3],
            "exact call-time state",
            history_lifetime,
        );
        let call = append(&agent, request).await;
        let mut changed = template.clone();
        changed.model = "new-model".into();
        changed.system[0].text = "new instructions".into();
        append(&agent, context_event("new-provider", &changed)).await;
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
            assert!(validate_request(&invalid[..index], &invalid[index]).is_err());
        }
        assert!(reconstruct_model_request(&records, 0).is_err());
        assert!(reconstruct_model_request(&records, child_context.sequence).is_err());
    }

    fn projection_fixture() -> (AgentId, Vec<EventRecord>) {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
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
        let compaction = |history: &[u64], tail| {
            requested(
                3,
                ModelPurpose::Compaction,
                history,
                tail,
                HistoryLifetime::Ending,
            )
        };
        let template = ModelRequest {
            model: "model".into(),
            system: vec![SystemSegment {
                text: "system".into(),
                cache: false,
            }],
            tools: vec![],
            history: vec![],
            tail: vec![],
            history_lifetime: HistoryLifetime::Continuing,
            reasoning: None,
            response_schema: None,
            max_output_tokens: Some(4096),
            correlation: None,
            blobs: Default::default(),
        };
        let events = vec![
            committed(text_message("verbatim plan")),
            committed(text_message("research")),
            SessionEvent::ModelContext {
                provider: "provider".into(),
                template,
            },
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
                queue_attempt: None,
                version: super::super::SESSION_FORMAT_VERSION,
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
        super::super::event::validate_records(&records, agent.session()).unwrap();
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
            assert!(validate_request(&invalid[..7], &invalid[7]).is_err());
        }
    }

    #[test]
    fn checkpoint_cannot_cut_or_mismatch_parallel_tool_exchanges() {
        let (agent, records) = projection_fixture();
        let calls = ["a", "b"];
        let call = |(position, id): (usize, &&str)| {
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
        records[0].event = committed(Message::Assistant(
            calls.iter().enumerate().map(call).collect(),
        ));
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
        let portable = ReplayEnvelope {
            version: 1,
            protocol: "test".into(),
            model: "model".into(),
            scope: "scope".into(),
            payload: json!({"signature":"opaque"}),
            conversation_bound: false,
        };
        let bound = ReplayEnvelope {
            conversation_bound: true,
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
        let mut append = |sequence: u64, event: SessionEvent| {
            let mut record = records[8].clone();
            (record.sequence, record.event) = (sequence, event);
            records.push(record);
        };
        append(10, committed(signed.clone()));
        let lifetime = HistoryLifetime::Continuing;
        append(
            11,
            requested(3, ModelPurpose::Agent, &[9, 1, 5, 10], "state", lifetime),
        );
        let projected = project_history(&records, &agent).unwrap();
        assert_eq!(projected[3].1, signed);
        let (_, request) = reconstruct_model_request(&records, 11).unwrap();
        assert!(
            request
                .history
                .iter()
                .eq(projected.iter().map(|(_, message)| message))
        );
    }
}

//! Validate the journal's history rules and reconstruct exact provider requests from it.
use crate::{
    identity::AgentId,
    provider::{profile::ModelRef, protocol::ModelRequest},
    session::Message,
};

use super::projection::project_from;
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

/// Validate a decoded journal's order-dependent rules, as append validates live
/// records, so what a store hands out always projects.
pub(super) fn admit_records(records: Vec<EventRecord>) -> Result<Vec<EventRecord>, SessionError> {
    for (index, record) in records.iter().enumerate() {
        validate(&records[..index], &[], record)?;
    }
    Ok(records)
}

crate::named_enum::named_enum! {
    /// Why a compaction checkpoint cannot replace its agent's history.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize)]
    pub error enum CheckpointError {
        #[error("compaction cannot overwrite newer todo state")]
        StaleTodos = "stale_todos",
        #[error("compaction message must use the user role")]
        MessageRole = "message_role",
        #[error("compaction frontier must precede its event")]
        Frontier = "frontier",
        #[error(
            "compaction request must be a summary request that follows its frontier, precedes its event and belongs to the same agent"
        )]
        SummaryRequest = "summary_request",
        #[error("retained messages must be unique, chronological and covered by the frontier")]
        RetainedOrder = "retained_order",
        #[error("retained source must be an original message of the same agent")]
        RetainedSource = "retained_source",
        #[error("retained tool exchange is incomplete")]
        IncompleteExchange = "incomplete_exchange",
        #[error("retained tool call and results do not match")]
        MismatchedExchange = "mismatched_exchange",
    }
}

/// Whether `agent`'s todos changed after `frontier`, which makes the todos a
/// summary up to it reconciled stale.
pub(crate) fn stale_todos<'a>(
    preceding: impl IntoIterator<Item = &'a EventRecord>,
    agent: &AgentId,
    frontier: RecordSeq,
) -> bool {
    preceding.into_iter().any(|source| {
        &source.agent == agent
            && source.sequence > frontier
            && matches!(
                source.event,
                SessionEvent::TodosReplaced { .. } | SessionEvent::Compaction { .. }
            )
    })
}

/// Validate a request's or checkpoint's references against the records before it:
/// `committed`, then `batch`.
pub(super) fn validate(
    committed: &[EventRecord],
    batch: &[EventRecord],
    record: &EventRecord,
) -> Result<(), SessionError> {
    let at = |sequence| record_at(batch, sequence).or_else(|| record_at(committed, sequence));
    let checkpoint = match &record.event {
        SessionEvent::Compaction { checkpoint } => checkpoint,
        SessionEvent::ModelRequested { .. } => {
            return request_history(record, at).map(drop).map_err(|reason| {
                SessionError::ModelRequestReplay {
                    sequence: record.sequence.request(),
                    reason,
                }
            });
        }
        _ => return Ok(()),
    };
    let invalid = |reason| SessionError::InvalidCompaction {
        sequence: record.sequence,
        reason,
    };
    let preceding = || committed.iter().chain(batch);
    if stale_todos(preceding(), &record.agent, checkpoint.frontier) {
        return Err(invalid(CheckpointError::StaleTodos));
    }
    if !matches!(checkpoint.message, Message::User(_)) {
        return Err(invalid(CheckpointError::MessageRole));
    }
    if checkpoint.frontier >= record.sequence
        || checkpoint.frontier
            > batch
                .last()
                .or(committed.last())
                .map_or(RecordSeq::default(), |r| r.sequence)
    {
        return Err(invalid(CheckpointError::Frontier));
    }
    let summary = at(checkpoint.attempt.request.into())
        .filter(|source| source.agent == record.agent && checkpoint.frontier < source.sequence)
        .and_then(|source| request_context(source, at));
    if summary.is_none_or(|context| context.purpose != ModelPurpose::Compaction) {
        return Err(invalid(CheckpointError::SummaryRequest));
    }
    let mut last = MessageSeq::default();
    for sequence in &checkpoint.retained {
        if *sequence <= last || RecordSeq::from(*sequence) > checkpoint.frontier {
            return Err(invalid(CheckpointError::RetainedOrder));
        }
        if !at((*sequence).into()).is_some_and(|source| {
            source.agent == record.agent
                && matches!(source.event, SessionEvent::MessageCommitted { .. })
        }) {
            return Err(invalid(CheckpointError::RetainedSource));
        }
        last = *sequence;
    }
    // References preserve whole messages and every existing result of a retained
    // exchange. Only the original trailing exchange may still be awaiting results.
    let originals: Vec<_> = preceding()
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
                    return Err(invalid(CheckpointError::IncompleteExchange));
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
        if !retained(assistant) || !results.iter().all(|&i| retained(i)) {
            return Err(invalid(CheckpointError::IncompleteExchange));
        }
        let trailing = assistant + 1 + results.len() == originals.len();
        let tools: Vec<_> = results.into_iter().map(message).collect();
        match tool_exchange(message(assistant), &tools) {
            None => return Err(invalid(CheckpointError::MismatchedExchange)),
            Some(ToolExchange::Pending) if !trailing => {
                return Err(invalid(CheckpointError::IncompleteExchange));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

enum ToolExchange {
    Complete,
    Pending,
}

fn tool_exchange(assistant: &Message, tools: &[&Message]) -> Option<ToolExchange> {
    let Message::Assistant(content) = assistant else {
        return None;
    };
    let calls: Vec<_> = content
        .iter()
        .filter_map(|item| item.call())
        .map(|call| (call.id(), call.name()))
        .collect();
    let results: Vec<_> = tools
        .iter()
        .filter_map(|tool| match tool {
            Message::Tool(result) => Some(result),
            _ => None,
        })
        .map(|result| (result.call_id.as_str(), result.name.as_str()))
        .collect();
    let call_map: std::collections::HashMap<_, _> = calls.iter().copied().collect();
    let result_map: std::collections::HashMap<_, _> = results.iter().copied().collect();
    if calls.is_empty()
        || calls.len() != call_map.len()
        || results.len() != result_map.len()
        || !result_map
            .iter()
            .all(|(id, name)| call_map.get(id) == Some(name))
    {
        return None;
    }
    Some(if call_map == result_map {
        ToolExchange::Complete
    } else {
        ToolExchange::Pending
    })
}

/// Why a `ModelRequested` record cannot be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("event not found")]
    NotFound,
    #[error("event is not a model request")]
    NotRequest,
    #[error("context must be a model context that precedes the call")]
    Context,
    #[error("checkpoint must be a compaction of the same agent that precedes the call")]
    Checkpoint,
    #[error("history must end at a message of the same agent's history under the checkpoint")]
    Through,
    #[error(
        "only a summary may drop older history, resuming after its opening exchange and never at tool results"
    )]
    From,
}

/// The history a `ModelRequested` record names: its context, then its checkpoint's
/// retained sources.
struct RequestHistory<'a> {
    context: &'a ModelContext,
    retained: &'a [MessageSeq],
}

/// Resolve a request's references with `at`, which finds only records before it: its
/// context and checkpoint belong to its agent, and `through` is a message the agent's
/// history under that checkpoint holds.
fn request_history<'a>(
    record: &EventRecord,
    at: impl Fn(RecordSeq) -> Option<&'a EventRecord>,
) -> Result<RequestHistory<'a>, ReplayError> {
    let SessionEvent::ModelRequested {
        context,
        checkpoint,
        through,
        from,
        ..
    } = &record.event
    else {
        return Err(ReplayError::NotRequest);
    };
    let own = |source| at(source).filter(|found| found.agent == record.agent);
    let Some(SessionEvent::ModelContext { context }) = own(*context).map(|found| &found.event)
    else {
        return Err(ReplayError::Context);
    };
    let (frontier, retained) = match checkpoint.map(own) {
        None => (None, &[][..]),
        Some(Some(EventRecord {
            event: SessionEvent::Compaction { checkpoint },
            ..
        })) => (Some(checkpoint.frontier), &checkpoint.retained[..]),
        Some(_) => return Err(ReplayError::Checkpoint),
    };
    // The history's end is a message it projects.
    if let Some(through) = *through {
        let projected = retained.contains(&through)
            || frontier.is_none_or(|frontier| RecordSeq::from(through) > frontier);
        let committed = matches!(
            own(through.into()).map(|found| &found.event),
            Some(SessionEvent::MessageCommitted { .. })
        );
        if !(projected && committed) {
            return Err(ReplayError::Through);
        }
    }
    // Only a summary drops older history, which invalidates no reasoning it replays;
    // reconstruction checks that it resumes at a cut `Projection::trim` could make.
    if from.is_some() && context.purpose != ModelPurpose::Compaction {
        return Err(ReplayError::From);
    }
    Ok(RequestHistory { context, retained })
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
    let invalid = |reason| SessionError::ModelRequestReplay { sequence, reason };
    let index = records
        .binary_search_by_key(&sequence.into(), |record| record.sequence)
        .map_err(|_| invalid(ReplayError::NotFound))?;
    let call = &records[index];
    let SessionEvent::ModelRequested {
        checkpoint,
        through,
        from,
        tail,
        history_lifetime,
        ..
    } = &call.event
    else {
        return Err(invalid(ReplayError::NotRequest));
    };
    let earlier = &records[..index];
    let RequestHistory { context, retained } =
        request_history(call, |source| record_at(earlier, source)).map_err(invalid)?;
    let checkpoint =
        checkpoint.map(|checkpoint| earlier.partition_point(|record| record.sequence < checkpoint));
    let mut history = project_from(earlier, &call.agent, checkpoint);
    // The history ends at `through`, after its checkpoint's opening.
    let opening = checkpoint.map_or(0, |_| 1 + retained.len());
    let through = through.map(RecordSeq::from);
    let kept = history.messages[opening..]
        .partition_point(|projected| through.is_some_and(|through| projected.sequence <= through));
    history.messages.truncate(opening + kept);
    // A summary drops tools, which changes the conversation for all of it.
    if context.purpose == ModelPurpose::Compaction {
        history.unbind();
    }
    if let Some(from) = *from
        && !history.trim_from(from)
    {
        return Err(invalid(ReplayError::From));
    }
    let request = ModelRequest {
        context: history.context(tail),
        history_lifetime: *history_lifetime,
        ..context.template()
    };
    Ok((context.profile.name.clone(), request))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::StateLifetime;
    use crate::session::CompactionTrigger;
    use crate::session::{
        project_history,
        tests::{committed, context, requested_history, sequences, text_message},
    };
    use crate::{
        agent::{TodoItem, TodoStatus},
        identity::AgentId,
        provider::codec::common::tests::envelope,
        provider::protocol::{
            AssistantItem, Binding, HistoryLifetime, ReplayFormat, ToolCall, ToolResult,
        },
        session::{AttemptRef, CompactionCheckpoint, SessionStore, UserPart},
    };
    use serde_json::json;

    fn todo(text: &str) -> TodoItem {
        TodoItem {
            text: text.parse().unwrap(),
            status: TodoStatus::Pending,
        }
    }

    #[tokio::test]
    async fn replay_preserves_context_boundaries_and_image_payloads() {
        let crate::tests::TestRuntime {
            root: directory,
            store,
            agent,
            ..
        } = crate::tests::TestRuntime::on_disk().await;
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
        let tool = Message::Tool(ToolResult {
            call_id: "read-1".into(),
            name: "read".into(),
            result: json!({"ok":true}),
            images: vec![image.clone()],
            is_error: false,
        });
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
        let child_message = append(&child, committed(text_message("child only"))).await;
        let history_lifetime = HistoryLifetime::Detached;
        let request = requested_history(
            context.sequence,
            None,
            &sources,
            "exact call-time state",
            history_lifetime,
        );
        let call = append(&agent, request).await;
        append(&agent, context_event_changed()).await;
        append(&agent, committed(text_message("future message"))).await;
        // A request names only its own agent's context and history.
        for (context, history, expected) in [
            (child_context.sequence, &[][..], ReplayError::Context),
            (
                context.sequence,
                &[child_message.sequence],
                ReplayError::Through,
            ),
            (context.sequence, &[context.sequence], ReplayError::Through),
        ] {
            let request = requested_history(context, None, history, "state", history_lifetime);
            let rejected = refused(&store, &agent, request);
            assert!(
                matches!(rejected, Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected),
                "{expected}: {rejected:?}"
            );
        }
        let id = store.id();
        drop(store);
        let sessions = directory.path().join("sessions");
        let (store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let (model, mut restored) =
            reconstruct_model_request(&records, call.sequence.request()).unwrap();
        assert_eq!(model.to_string(), "original-provider/test");
        let mut expected = template;
        let messages = [
            &user,
            &assistant,
            &tool,
            &text_message("exact call-time state"),
        ];
        expected.context = (messages.into_iter())
            .map(|message| message.entry(StateLifetime::Persisted))
            .collect();
        expected.history_lifetime = history_lifetime;
        assert_eq!(restored, expected);
        // Value equality ignores key order and the sign of zero; the wire bytes must not.
        let wire = |request: &ModelRequest| serde_json::to_string(&request.context[1]).unwrap();
        assert_eq!(wire(&restored), wire(&expected));
        let mut cache = crate::media::LoadedBlobs::default();
        store.load_blobs(&mut restored, &mut cache).await.unwrap();
        store.load_blobs(&mut expected, &mut cache).await.unwrap();
        assert_eq!(restored, expected);
        assert_eq!(restored.blobs.get(&image.blob).unwrap(), png.bytes());
        let crate::media::AttachmentRef::Text(notes) = notes else {
            panic!("text attachment")
        };
        assert_eq!(restored.blobs.text(&notes).unwrap(), "notes");

        for (sequence, expected) in [
            (0.into(), ReplayError::NotFound),
            (child_context.sequence.request(), ReplayError::NotRequest),
        ] {
            assert!(matches!(
                reconstruct_model_request(&records, sequence),
                Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected
            ));
        }
    }

    /// Whether the journal takes `event` as `agent`'s next record.
    fn refused(
        store: &crate::session::SessionStore,
        agent: &AgentId,
        event: SessionEvent,
    ) -> Result<(), SessionError> {
        let committed = store.records();
        let sequence = committed.len() as u64 + 1;
        let record = crate::session::tests::record(agent, sequence, event);
        validate(&committed, &[], &record)
    }

    fn context_event_changed() -> SessionEvent {
        context(
            ModelPurpose::Agent,
            "new-provider",
            "new-model",
            "new instructions",
        )
    }

    /// Checkpoints are validated when appended, so a journal always projects.
    #[tokio::test]
    async fn invalid_compactions_are_refused() {
        let session = crate::tests::TestRuntime::new().await;
        let (store, agent) = (&session.store, &session.agent);
        let append = async |event| store.append(agent.clone(), event).await.map(|r| r.sequence);
        let call = |id: &str, position| {
            let call = ToolCall::new(id, "exec", json!({})).unwrap();
            AssistantItem::tool_call(format!("item-{id}"), position, call)
        };
        let result = |id: &str| {
            Message::Tool(ToolResult {
                call_id: id.into(),
                name: "exec".into(),
                result: json!({}),
                images: vec![],
                is_error: false,
            })
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
        let request = append(requested_history(
            agent_context,
            None,
            &sources,
            "state",
            extends,
        ))
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
        let summary = append(requested_history(
            summary_context,
            None,
            &sources,
            "sum",
            detached,
        ))
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
                trigger: CompactionTrigger::Occupancy(100),
            }
        };
        let valid = checkpoint(todos, vec![calls, first, second], summary);
        let invalid = |retained| CompactionCheckpoint {
            retained,
            ..valid.clone()
        };
        let messages =
            |sources: &[RecordSeq]| sources.iter().map(|source| source.message()).collect();
        use CheckpointError::*;
        for (expected, checkpoint) in [
            (RetainedOrder, invalid(messages(&[calls, calls]))),
            (RetainedOrder, invalid(messages(&[first, calls]))),
            (RetainedOrder, invalid(messages(&[agent_context]))),
            (IncompleteExchange, invalid(messages(&[calls]))),
            (IncompleteExchange, invalid(messages(&[calls, first]))),
            (IncompleteExchange, invalid(messages(&[first, second]))),
            (SummaryRequest, checkpoint(summary, vec![], summary)),
            (
                Frontier,
                checkpoint((summary.get() + 5).into(), vec![], summary),
            ),
            (SummaryRequest, checkpoint(todos, vec![], request)),
            (
                MessageRole,
                CompactionCheckpoint {
                    message: Message::Assistant(vec![]),
                    ..valid.clone()
                },
            ),
            (StaleTodos, checkpoint(research, vec![], summary)),
        ] {
            let rejected = refused(store, agent, SessionEvent::Compaction { checkpoint });
            assert!(
                matches!(rejected, Err(SessionError::InvalidCompaction { reason, .. }) if reason == expected),
                "{expected}: {rejected:?}"
            );
        }
        // A loaded journal is admitted by the same rules, so a checkpoint append
        // would refuse never reaches projection.
        let mut corrupt = store.records();
        let mut record = corrupt.last().unwrap().clone();
        record.sequence = record.sequence.next();
        record.event = SessionEvent::Compaction {
            checkpoint: invalid(messages(&[calls, calls])),
        };
        corrupt.push(record);
        assert!(matches!(
            admit_records(corrupt),
            Err(SessionError::InvalidCompaction { .. })
        ));
        // A checkpoint may name a summary request committed in its own batch.
        let owner = agent.clone();
        let summary = requested_history(summary_context, None, &sources, "sum", detached);
        let batch = store.append_then(agent.clone(), summary, move |request| {
            let checkpoint = checkpoint(todos, vec![calls, first, second], request);
            let compaction = SessionEvent::Compaction { checkpoint };
            vec![(owner.clone(), attempt(request)), (owner, compaction)]
        });
        let installed = batch.await.unwrap().last().unwrap().sequence;
        let records = admit_records(store.records()).unwrap();
        let projected = project_history(&records, agent);
        assert_eq!(projected.checkpoint, Some(installed));
        assert_eq!(
            sequences(&projected),
            [installed, calls, first, second].map(RecordSeq::get)
        );
    }

    #[test]
    fn checkpoints_admit_a_trailing_open_exchange_but_not_partial_retention() {
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let call = |id: &str, position| {
            AssistantItem::tool_call(id, position, ToolCall::new(id, "exec", json!({})).unwrap())
        };
        let result = |id: &str| {
            Message::Tool(ToolResult {
                call_id: id.into(),
                name: "exec".into(),
                result: json!({}),
                images: vec![],
                is_error: false,
            })
        };
        use CheckpointError::{IncompleteExchange, MismatchedExchange};
        for (results, trailing, retained, expected) in [
            (vec![], true, vec![1], None),
            (vec!["b"], true, vec![1, 2], None),
            (vec!["b", "a"], true, vec![1, 2, 3], None),
            (vec![], false, vec![1], Some(IncompleteExchange)),
            (vec!["b"], false, vec![1, 2], Some(IncompleteExchange)),
            (vec!["b"], true, vec![1], Some(IncompleteExchange)),
            (vec!["b"], true, vec![2], Some(IncompleteExchange)),
            (vec!["b", "a"], true, vec![1, 2], Some(IncompleteExchange)),
            (vec!["unknown"], true, vec![1, 2], Some(MismatchedExchange)),
            (
                vec!["b", "b"],
                true,
                vec![1, 2, 3],
                Some(MismatchedExchange),
            ),
        ] {
            let mut events = vec![committed(Message::Assistant(vec![
                call("a", 0),
                call("b", 1),
            ]))];
            events.extend(results.iter().map(|id| committed(result(id))));
            if !trailing {
                events.push(committed(text_message("later input")));
            }
            let frontier = RecordSeq::from(events.len() as u64);
            let context_sequence = frontier.next();
            events.push(context(ModelPurpose::Compaction, "p", "m", "s"));
            let summary = context_sequence.next();
            events.push(requested_history(
                context_sequence,
                None,
                &[],
                "sum",
                HistoryLifetime::Detached,
            ));
            events.push(SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier,
                    message: text_message("summary"),
                    todos: Vec::new(),
                    retained: retained.into_iter().map(MessageSeq::from).collect(),
                    attempt: AttemptRef {
                        request: summary.request(),
                        attempt: 1,
                    },
                    trigger: CompactionTrigger::Occupancy(100),
                },
            });
            let records = events
                .into_iter()
                .enumerate()
                .map(|(index, event)| {
                    crate::session::tests::record(&agent, index as u64 + 1, event)
                })
                .collect();
            let admitted = admit_records(records).map(drop);
            assert!(
                match expected {
                    None => admitted.is_ok(),
                    Some(expected) => matches!(admitted,
                        Err(SessionError::InvalidCompaction { reason, .. }) if reason == expected),
                },
                "{results:?}, trailing={trailing}: {admitted:?}"
            );
        }
    }
}

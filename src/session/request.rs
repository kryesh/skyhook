//! Validate the journal's history rules and reconstruct exact provider requests from it.
use crate::{
    provider::{profile::ModelRef, protocol::ModelRequest},
    session::Message,
};
use std::{collections::HashMap, sync::Arc};

use super::db::{self, Db, corrupt, rejected};
use super::{EventRecord, ModelPurpose, SessionError, SessionEvent};
use crate::identity::SessionId;
use crate::session::{MessageSeq, RecordSeq, RequestSeq};

/// Validate a decoded journal's order-dependent rules against `db`, which holds it,
/// as append validates live records, so what a store hands out always projects.
pub(super) fn admit_records(
    db: &Db,
    records: Vec<Arc<EventRecord>>,
) -> Result<Vec<Arc<EventRecord>>, SessionError> {
    for record in &records {
        validate(db, record)?;
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

/// What an entry a record references is, as validating the reference needs it.
pub(super) enum Referenced {
    Context(ModelPurpose),
    Request {
        context: RecordSeq,
    },
    Checkpoint {
        frontier: RecordSeq,
        retained: Vec<MessageSeq>,
    },
    Commit,
    Other,
}

/// The journal ahead of one agent's record, read by index: inside the record's own
/// transaction, which holds every record ahead of it in its batch.
#[derive(Clone, Copy)]
struct Preceding<'a> {
    db: &'a Db,
    /// The agent's row.
    row: i64,
    /// The record: only what precedes it can be referenced.
    before: RecordSeq,
}

impl<'a> Preceding<'a> {
    fn of(db: &'a Db, record: &EventRecord) -> Result<Self, SessionError> {
        let row = db::agent_row(db, &record.agent)?;
        let row = row.ok_or_else(|| rejected(format!("agent {} has not started", record.agent)))?;
        Ok(Self {
            db,
            row,
            before: record.sequence,
        })
    }

    /// What the agent's entry at `sequence` is; None unless the agent journaled it.
    fn own(&self, sequence: RecordSeq) -> Result<Option<Referenced>, SessionError> {
        if sequence >= self.before {
            return Ok(None);
        }
        Ok(db::history::referenced(self.db, self.row, sequence)?)
    }

    fn committed(&self, sequence: RecordSeq) -> Result<bool, SessionError> {
        Ok(matches!(self.own(sequence)?, Some(Referenced::Commit)))
    }
}

/// Validate a request's or checkpoint's references against the journal `db` holds
/// ahead of it.
pub(super) fn validate(db: &Db, record: &EventRecord) -> Result<(), SessionError> {
    let checkpoint = match &record.event {
        SessionEvent::Compaction { checkpoint } => checkpoint,
        event @ SessionEvent::ModelRequested { .. } => {
            let preceding = Preceding::of(db, record)?;
            return request_history(&preceding, record.sequence.request(), event).map(drop);
        }
        _ => return Ok(()),
    };
    let preceding = Preceding::of(db, record)?;
    let invalid = |reason| SessionError::InvalidCompaction {
        sequence: record.sequence,
        reason,
    };
    // Todos changed after the frontier make those the summary reconciled stale.
    let row = preceding.row;
    if db::history::todos_changed(db, row, checkpoint.frontier, record.sequence)? {
        return Err(invalid(CheckpointError::StaleTodos));
    }
    if !matches!(checkpoint.message, Message::User(_)) {
        return Err(invalid(CheckpointError::MessageRole));
    }
    if checkpoint.frontier >= record.sequence {
        return Err(invalid(CheckpointError::Frontier));
    }
    let summary = RecordSeq::from(checkpoint.attempt.request);
    let summarized = match preceding.own(summary)? {
        Some(Referenced::Request { context }) if checkpoint.frontier < summary => {
            preceding.own(context)?
        }
        _ => None,
    };
    if !matches!(
        summarized,
        Some(Referenced::Context(ModelPurpose::Compaction))
    ) {
        return Err(invalid(CheckpointError::SummaryRequest));
    }
    let mut last = MessageSeq::default();
    for sequence in &checkpoint.retained {
        if *sequence <= last || RecordSeq::from(*sequence) > checkpoint.frontier {
            return Err(invalid(CheckpointError::RetainedOrder));
        }
        if !preceding.committed((*sequence).into())? {
            return Err(invalid(CheckpointError::RetainedSource));
        }
        last = *sequence;
    }
    // References preserve whole messages and every existing result of a retained
    // exchange. Only the original trailing exchange may still be awaiting results.
    // Each exchange is read around its retained members, by index.
    let messages = db::history::committed(db, &checkpoint.retained)?;
    if messages.len() != checkpoint.retained.len() {
        return Err(corrupt("a retained source is not a committed message").into());
    }
    let message = |sequence: RecordSeq| &messages[&sequence.message()];
    let retained = |sequence: RecordSeq| messages.contains_key(&sequence.message());
    let mut checked = None;
    for sequence in checkpoint
        .retained
        .iter()
        .map(|sequence| RecordSeq::from(*sequence))
    {
        let call = match message(sequence) {
            Message::Tool(_) => db::history::opener(db, row, sequence)?
                .ok_or_else(|| invalid(CheckpointError::IncompleteExchange))?,
            Message::Assistant(content) if content.iter().any(|item| item.call().is_some()) => {
                sequence
            }
            _ => continue,
        };
        if checked.replace(call) == Some(call) {
            continue;
        }
        let (results, end) = db::history::results(db, row, call, checkpoint.frontier)?;
        let results = results.into_iter().map(RecordSeq::from);
        if !retained(call) || !results.clone().all(retained) {
            return Err(invalid(CheckpointError::IncompleteExchange));
        }
        let tools: Vec<_> = results.map(message).collect();
        match tool_exchange(message(call), &tools) {
            None => return Err(invalid(CheckpointError::MismatchedExchange)),
            Some(ToolExchange::Pending) if end.is_some() => {
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
    let call_map: HashMap<_, _> = calls.iter().copied().collect();
    let result_map: HashMap<_, _> = results.iter().copied().collect();
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
    #[error("context must be the agent's newest model context of its purpose ahead of the call")]
    Context,
    #[error("an agent request must follow a result for every call its agent made")]
    Unanswered,
    #[error("checkpoint must be a compaction of the same agent that precedes the call")]
    Checkpoint,
    #[error("history must end at a message of the same agent's history under the checkpoint")]
    Through,
    #[error(
        "only a summary may drop older history, resuming after its opening exchange and never at tool results"
    )]
    From,
}

/// Resolve the references of the request `event` journaled at `sequence`: its context
/// and checkpoint belong to its agent, and `through` is a message the agent's history
/// under that checkpoint holds. Returns the checkpoint's retained sources.
fn request_history(
    preceding: &Preceding,
    sequence: RequestSeq,
    event: &SessionEvent,
) -> Result<Vec<MessageSeq>, SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay { sequence, reason };
    let SessionEvent::ModelRequested {
        context,
        checkpoint,
        through,
        from,
        ..
    } = event
    else {
        return Err(invalid(ReplayError::NotRequest));
    };
    let Some(Referenced::Context(purpose)) = preceding.own(*context)? else {
        return Err(invalid(ReplayError::Context));
    };
    let Preceding { db, row, before } = *preceding;
    // Only each agent's newest context of a purpose stays resident to name.
    if db::history::context_replaced(db, row, purpose, *context, before)? {
        return Err(invalid(ReplayError::Context));
    }
    // An open session holds only the calls its agents made since their latest
    // agent request, as each agent request follows a result for every earlier call.
    if purpose == ModelPurpose::Agent && !db::history::unanswered(db, row, before)?.is_empty() {
        return Err(invalid(ReplayError::Unanswered));
    }
    let (frontier, retained) = match checkpoint {
        None => (None, Vec::new()),
        Some(checkpoint) => match preceding.own(*checkpoint)? {
            Some(Referenced::Checkpoint { frontier, retained }) => (Some(frontier), retained),
            _ => return Err(invalid(ReplayError::Checkpoint)),
        },
    };
    // The history's end is a message it projects.
    if let Some(through) = *through {
        let projected = retained.contains(&through)
            || frontier.is_none_or(|frontier| RecordSeq::from(through) > frontier);
        if !(projected && preceding.committed(through.into())?) {
            return Err(invalid(ReplayError::Through));
        }
    }
    // Only a summary drops older history, which invalidates no reasoning it replays;
    // reconstruction checks that it resumes at a cut `Projection::trim` could make.
    if from.is_some() && purpose != ModelPurpose::Compaction {
        return Err(invalid(ReplayError::From));
    }
    Ok(retained)
}

/// The model the request journaled at `sequence` was issued under, and the exact
/// request, as `db` holds them.
fn reconstruct(
    db: &Db,
    session: SessionId,
    sequence: RequestSeq,
) -> Result<(ModelRef, ModelRequest), SessionError> {
    let invalid = |reason| SessionError::ModelRequestReplay { sequence, reason };
    let entry = RecordSeq::from(sequence);
    let row = db::history::author(db, entry)?.ok_or(invalid(ReplayError::NotFound))?;
    let Some(Referenced::Request { context }) = db::history::referenced(db, row, entry)? else {
        return Err(invalid(ReplayError::NotRequest));
    };
    let records = db::decode_records(db, session, db::Entries::Only(&[context, entry]))?;
    let missing = || SessionError::from(corrupt("a request's context is missing"));
    let [context, request] = records.as_slice() else {
        return Err(missing());
    };
    let (
        SessionEvent::ModelContext { context },
        event @ SessionEvent::ModelRequested {
            checkpoint,
            through,
            from,
            tail,
            history_lifetime,
            ..
        },
    ) = (&context.event, &request.event)
    else {
        return Err(missing());
    };
    let preceding = Preceding {
        db,
        row,
        before: entry,
    };
    let retained = request_history(&preceding, sequence, event)?;
    let opening = db::history::Opening::Named(*checkpoint);
    let before = entry.preceding().unwrap_or_default();
    let mut history = db::history::project(db, session, row, opening, before)?;
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

impl super::SessionStore {
    /// The model a `model_requested` record's request was issued under, and the exact
    /// request, read from the journal. Attachment and image references keep their blob
    /// digests; `load_blobs` loads their contents for provider encoding.
    pub async fn reconstruct_model_request(
        &self,
        sequence: RequestSeq,
    ) -> Result<(ModelRef, ModelRequest), SessionError> {
        let session = self.id();
        self.snapshot(move |db| reconstruct(db, session, sequence))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::StateLifetime;
    use crate::session::CompactionTrigger;
    use crate::session::tests::{
        admitted, committed, context, projected, requested_history, sequences, text_message,
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
        let changed = append(&agent, context_event_changed()).await;
        append(&agent, committed(text_message("future message"))).await;
        // A request names only its own agent's newest context and its history.
        for (context, history, expected) in [
            (child_context.sequence, &[][..], ReplayError::Context),
            (context.sequence, &[][..], ReplayError::Context),
            (
                changed.sequence,
                &[child_message.sequence],
                ReplayError::Through,
            ),
            (changed.sequence, &[changed.sequence], ReplayError::Through),
        ] {
            let request = requested_history(context, None, history, "state", history_lifetime);
            let rejected = admitted(&store, &agent, request);
            assert!(
                matches!(rejected, Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected),
                "{expected}: {rejected:?}"
            );
        }
        let id = store.id();
        drop(store);
        let sessions = directory.path().join("sessions");
        let store = SessionStore::open(&sessions, id).await.unwrap();
        let restored = store.reconstruct_model_request(call.sequence.request());
        let (model, mut restored) = restored.await.unwrap();
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
                store.reconstruct_model_request(sequence).await,
                Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected
            ));
        }
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
            let rejected = admitted(store, agent, SessionEvent::Compaction { checkpoint });
            assert!(
                matches!(rejected, Err(SessionError::InvalidCompaction { reason, .. }) if reason == expected),
                "{expected}: {rejected:?}"
            );
        }
        // A checkpoint may name a summary request committed in its own batch.
        let owner = agent.clone();
        let summary = requested_history(summary_context, None, &sources, "sum", detached);
        let batch = store.append_then(agent.clone(), summary, move |request| {
            let checkpoint = checkpoint(todos, vec![calls, first, second], request);
            let compaction = SessionEvent::Compaction { checkpoint };
            vec![(owner.clone(), attempt(request)), (owner, compaction)]
        });
        let installed = batch.await.unwrap().last().unwrap().sequence;
        let projected = projected(store, agent).await;
        assert_eq!(projected.checkpoint, Some(installed));
        assert_eq!(
            sequences(&projected),
            [installed, calls, first, second].map(RecordSeq::get)
        );
    }

    #[tokio::test]
    async fn checkpoints_admit_a_trailing_open_exchange_but_not_partial_retention() {
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
            (vec![], true, vec![0], None),
            (vec!["b"], true, vec![0, 1], None),
            (vec!["b", "a"], true, vec![0, 1, 2], None),
            (vec![], false, vec![0], Some(IncompleteExchange)),
            (vec!["b"], false, vec![0, 1], Some(IncompleteExchange)),
            (vec!["b"], true, vec![0], Some(IncompleteExchange)),
            (vec!["b"], true, vec![1], Some(IncompleteExchange)),
            (vec!["b", "a"], true, vec![0, 1], Some(IncompleteExchange)),
            (vec!["unknown"], true, vec![0, 1], Some(MismatchedExchange)),
            (
                vec!["b", "b"],
                true,
                vec![0, 1, 2],
                Some(MismatchedExchange),
            ),
        ] {
            let session = crate::tests::TestRuntime::new().await;
            let (store, agent) = (&session.store, &session.agent);
            let mut events = vec![committed(Message::Assistant(vec![
                call("a", 0),
                call("b", 1),
            ]))];
            events.extend(results.iter().map(|id| committed(result(id))));
            if !trailing {
                events.push(committed(text_message("later input")));
            }
            let mut messages = Vec::new();
            for event in events {
                let appended = store.append(agent.clone(), event).await;
                let Ok(appended) = appended else {
                    // The journal binds no result to a call it does not answer.
                    assert_eq!(expected, Some(MismatchedExchange), "{results:?}");
                    break;
                };
                messages.push(appended.sequence);
            }
            if messages.len() < 1 + results.len() + usize::from(!trailing) {
                continue;
            }
            let frontier = *messages.last().unwrap();
            let summarizes = context(ModelPurpose::Compaction, "p", "m", "s");
            let summarizes = store
                .append(agent.clone(), summarizes)
                .await
                .unwrap()
                .sequence;
            let detached = HistoryLifetime::Detached;
            let summary = requested_history(summarizes, None, &[], "sum", detached);
            let summary = store.append(agent.clone(), summary).await.unwrap().sequence;
            let attempt = AttemptRef {
                request: summary.request(),
                attempt: 1,
            };
            let started = SessionEvent::ModelAttemptStarted(attempt);
            store.append(agent.clone(), started).await.unwrap();
            let compaction = SessionEvent::Compaction {
                checkpoint: CompactionCheckpoint {
                    frontier,
                    message: text_message("summary"),
                    todos: Vec::new(),
                    retained: (retained.iter())
                        .map(|index: &usize| messages[*index].message())
                        .collect(),
                    attempt,
                    trigger: CompactionTrigger::Occupancy(100),
                },
            };
            let admitted = admitted(store, agent, compaction);
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

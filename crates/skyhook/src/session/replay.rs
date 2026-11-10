//! Reconstruct exact provider requests from the journal.
use crate::provider::{model::ModelRef, protocol::ModelRequest};

use super::db::{self, corrupt};
use super::{ModelPurpose, SessionError, SessionEvent};
use crate::db::Db;
use crate::identity::SessionId;
use crate::session::{RecordSeq, RequestSeq};
use skyhook_base::enums::NamedEnum;

/// Why a compaction checkpoint cannot replace its agent's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointError {
    #[error("compaction cannot overwrite newer todo state")]
    StaleTodos,
    #[error("retained source must be an original message of the same agent")]
    RetainedSource,
}

/// Why a `ModelRequested` record cannot be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("event not found")]
    NotFound,
    #[error("event is not a model request")]
    NotRequest,
    #[error(
        "only a summary may drop older history, resuming after its opening exchange and never at tool results"
    )]
    From,
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
    let context =
        db::history::request_context(db, entry)?.ok_or(invalid(ReplayError::NotRequest))?;
    let records = db::decode_records(db, session, db::Entries::Only(&[context, entry]))?;
    let missing = || SessionError::from(corrupt("a request's context is missing"));
    let [context, request] = records.as_slice() else {
        return Err(missing());
    };
    let (
        SessionEvent::ModelContext { context },
        SessionEvent::ModelRequested {
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
    let opening = db::history::Opening::Named(*checkpoint);
    let before = entry.preceding().unwrap_or_default();
    let mut history = db::history::project(db, session, row, opening, before)?;
    // The history ends at `through`, after its checkpoint's opening.
    let opening = match checkpoint {
        Some(checkpoint) => (db::history::compaction(db, *checkpoint)?)
            .map_or(0, |(.., retained)| 1 + retained.len()),
        None => 0,
    };
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
    use crate::identity::AgentId;
    use crate::provider::protocol::StateLifetime;
    use crate::session::tests::{committed, context, requested_history, text_message};
    use crate::{
        provider::{
            codec::common::tests::envelope,
            protocol::{AssistantItem, Binding, HistoryLifetime, ReplayFormat, ToolResult},
        },
        session::{Message, SessionStore, UserPart},
    };
    use serde_json::json;
    use std::assert_matches;

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
        let png = crate::tests::png(b"image payload").await;
        let image = store
            .store_image(Some("test.png".into()), &png)
            .await
            .unwrap();
        let text = skyhook_media::Attachment::Text {
            file: Some("notes.txt".into()),
            content: "notes".into(),
        };
        let notes = store.store_attachment(&text).await.unwrap();
        let user = Message::User(vec![
            UserPart::Text {
                text: "inspect".into(),
            },
            UserPart::Attachment {
                attachment: skyhook_media::AttachmentRef::Image(image.clone()),
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
        append(&child, committed(text_message("child only"))).await;
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
        let id = store.id();
        drop(store);
        let sessions = directory.path().join("sessions");
        let store = SessionStore::open(&sessions, id, None).await.unwrap();
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
        let mut cache = skyhook_media::LoadedBlobs::default();
        store.load_blobs(&mut restored, &mut cache).await.unwrap();
        store.load_blobs(&mut expected, &mut cache).await.unwrap();
        assert_eq!(restored, expected);
        assert_eq!(restored.blobs.get(&image.blob).unwrap(), png.bytes());
        let skyhook_media::AttachmentRef::Text(notes) = notes else {
            panic!("text attachment")
        };
        assert_eq!(restored.blobs.text(&notes).unwrap(), "notes");

        for (sequence, expected) in [
            (0.into(), ReplayError::NotFound),
            (child_context.sequence.request(), ReplayError::NotRequest),
        ] {
            assert_matches!(
                store.reconstruct_model_request(sequence).await,
                Err(SessionError::ModelRequestReplay { reason, .. }) if reason == expected
            );
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
}

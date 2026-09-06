//! Reconstruct provider-neutral requests from their shared context and committed conversation.

use crate::provider::protocol::{Message, ModelRequest};

use super::{EventRecord, SessionError, SessionEvent};

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
    let call = &records[index];
    let SessionEvent::ModelRequested {
        context,
        history_len,
        runtime,
    } = &call.event
    else {
        return Err(invalid("event is not a model request"));
    };
    let context = records[..index]
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
    request.messages = records[..index]
        .iter()
        .filter_map(|record| {
            if record.agent != call.agent {
                return None;
            }
            match &record.event {
                SessionEvent::MessageCommitted { message } => Some(message.clone()),
                _ => None,
            }
        })
        .collect();
    if request.messages.len() != *history_len {
        return Err(invalid("committed history length does not match the call"));
    }
    request.messages.push(Message::User(vec![runtime.clone()]));
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
                    history_len: 3,
                    runtime: runtime.clone(),
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
        let SessionEvent::ModelRequested { history_len, .. } = &mut invalid[index].event else {
            unreachable!()
        };
        *history_len = 2;
        assert!(reconstruct_model_request(&invalid, call.sequence).is_err());
        assert!(reconstruct_model_request(&records, 0).is_err());
        assert!(reconstruct_model_request(&records, child_context.sequence).is_err());
    }
}

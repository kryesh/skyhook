//! Internal history-free request settings; persisted templates remain raw DTOs.

use crate::provider::protocol::{HistoryLifetime, ModelRequest, SystemSegment};

use super::SessionError;

/// A request template cannot carry conversation history. Constructing it from a
/// persisted/raw request validates rather than silently discarding messages.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelRequestTemplate(ModelRequest);

impl TryFrom<ModelRequest> for ModelRequestTemplate {
    type Error = SessionError;

    fn try_from(mut request: ModelRequest) -> Result<Self, Self::Error> {
        if !request.history.is_empty() || !request.tail.is_empty() {
            return Err(SessionError::TemplateHistory);
        }
        request.history_lifetime = HistoryLifetime::default();
        request.blobs = Default::default();
        Ok(Self(request))
    }
}

impl ModelRequestTemplate {
    pub(crate) fn system(&self) -> &[SystemSegment] {
        &self.0.system
    }

    /// The shared settings as a request with empty history/tail and the default lifetime;
    /// callers fill in the conversation with struct update syntax.
    pub(crate) fn to_request(&self) -> ModelRequest {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::protocol::{Message, ToolDefinition, UserContent};

    fn request() -> ModelRequest {
        ModelRequest {
            model: "test".into(),
            system: vec![SystemSegment {
                text: "system".into(),
                cache: true,
            }],
            history: Vec::new(),
            tail: Vec::new(),
            history_lifetime: HistoryLifetime::Extends,
            tools: vec![ToolDefinition {
                name: "test".into(),
                description: "description".into(),
                input_schema: serde_json::json!({"type":"object"}),
            }],
            response_schema: None,
            reasoning: Some("medium".into()),
            max_output_tokens: Some(64),
            blobs: Default::default(),
        }
    }

    #[test]
    fn raw_history_is_rejected_not_silently_discarded() {
        let message = Message::User(vec![UserContent::Text {
            text: "must not disappear".into(),
        }]);
        for tail in [false, true] {
            let mut raw = request();
            if tail {
                &mut raw.tail
            } else {
                &mut raw.history
            }
            .push(message.clone());
            assert!(matches!(
                ModelRequestTemplate::try_from(raw),
                Err(SessionError::TemplateHistory)
            ));
        }
        let mut raw = request();
        raw.history_lifetime = HistoryLifetime::Detached;
        let template = ModelRequestTemplate::try_from(raw).unwrap();
        assert_eq!(template.to_request(), request());
    }
}

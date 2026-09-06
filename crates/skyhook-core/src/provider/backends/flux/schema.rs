//! Preserve Flux's codecs while adding final-answer schema fields they do not expose.

use flux_provider::{ByteStream, ChunkStream, Request, WireCodec};
use serde_json::{Value, json};

use crate::provider::protocol::ResponseSchema;

pub(super) const METADATA_KEY: &str = "skyhook_response_schema";

pub(super) enum SchemaFormat {
    Chat,
    Responses,
    Anthropic,
}

pub(super) struct SchemaCodec<C> {
    pub inner: C,
    pub format: SchemaFormat,
}

impl<C: WireCodec> WireCodec for SchemaCodec<C> {
    fn build_body(&self, request: &Request) -> flux_core::Result<Value> {
        let Some(schema) = request.metadata.get(METADATA_KEY) else {
            return self.inner.build_body(request);
        };
        let schema: ResponseSchema = serde_json::from_value(schema.clone())?;
        // The private bridge field must never become provider metadata or a wire field.
        let mut request = request.clone();
        request.metadata.remove(METADATA_KEY);
        let mut body = self.inner.build_body(&request)?;
        match self.format {
            SchemaFormat::Chat => {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": {"name": schema.name, "schema": schema.schema, "strict": true}
                });
            }
            SchemaFormat::Responses => {
                if !body["text"].is_object() {
                    body["text"] = json!({});
                }
                body["text"]["format"] = json!({
                    "type": "json_schema", "name": schema.name,
                    "schema": schema.schema, "strict": true
                });
            }
            SchemaFormat::Anthropic => {
                if !body["output_config"].is_object() {
                    body["output_config"] = json!({});
                }
                body["output_config"]["format"] = json!({
                    "type": "json_schema", "schema": schema.schema
                });
            }
        }
        if request.tools.is_empty() {
            // A schema request without available tools must produce only its final
            // answer. Explicit selection also avoids implicit `auto` tool grammars.
            body.as_object_mut()
                .expect("provider request body")
                .remove("tools");
            body["tool_choice"] = match self.format {
                SchemaFormat::Chat | SchemaFormat::Responses => json!("none"),
                SchemaFormat::Anthropic => json!({"type": "none"}),
            };
        }
        Ok(body)
    }

    fn map_stream(&self, bytes: ByteStream) -> ChunkStream {
        self.inner.map_stream(bytes)
    }

    fn wire_headers(&self) -> Vec<(&'static str, String)> {
        self.inner.wire_headers()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        backends::flux::conversion::convert_request,
        protocol::{Message, ModelRequest, SystemSegment, ToolDefinition, UserContent},
    };
    use flux_providers::{
        anthropic::AnthropicMessages,
        openai::{OpenAiChat, OpenAiResponses},
    };

    fn request(schema: bool) -> ModelRequest {
        ModelRequest {
            model: "claude-sonnet-4-6".to_owned(),
            system: vec![SystemSegment {
                text: "Keep working".to_owned(),
                cache: true,
            }],
            messages: vec![Message::User(vec![UserContent::Text {
                text: "Summarize".to_owned(),
            }])],
            tools: vec![ToolDefinition {
                name: "read".to_owned(),
                description: "Read a file".to_owned(),
                input_schema: json!({"type":"object", "properties":{}}),
            }],
            response_schema: schema.then(|| ResponseSchema {
                name: "continuation".to_owned(),
                schema: json!({"type":"object", "properties":{
                    "state":{"type":"string"}, "evidence":{"type":"string"}},
                    "required":["state", "evidence"], "additionalProperties":false}),
            }),
            reasoning: Some("high".to_owned()),
            max_output_tokens: Some(16000),
            correlation: Some("test-session".to_owned()),
        }
    }

    #[test]
    fn schema_survives_request_serialization_and_keeps_reasoning_independent() {
        let request = request(true);
        let saved = serde_json::to_value(&request).unwrap();
        assert_eq!(saved["response_schema"]["name"], "continuation");
        assert_eq!(
            serde_json::from_value::<ModelRequest>(saved).unwrap(),
            request
        );
        let restored =
            serde_json::from_str::<ModelRequest>(&serde_json::to_string(&request).unwrap())
                .unwrap();
        assert_eq!(
            restored.response_schema.as_ref().unwrap().schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["state", "evidence"]
        );
        let converted = convert_request(restored).unwrap();
        assert!(converted.thinking);
        assert_eq!(converted.effort, Some(flux_provider::Effort::High));
    }

    #[test]
    fn schema_codecs_preserve_existing_body_fields_and_reasoning() {
        let schema_request = convert_request(request(true)).unwrap();
        let plain_request = convert_request(request(false)).unwrap();
        let schema = &schema_request.metadata[METADATA_KEY]["schema"];
        for (inner, format, container, expected) in [
            (
                Box::new(OpenAiChat) as Box<dyn WireCodec>,
                SchemaFormat::Chat,
                "response_format",
                json!({"type":"json_schema", "json_schema":{
                    "name":"continuation", "schema":schema, "strict":true}}),
            ),
            (
                Box::new(OpenAiResponses { codex: false }),
                SchemaFormat::Responses,
                "text",
                json!({"format":{"type":"json_schema", "name":"continuation",
                    "schema":schema, "strict":true}}),
            ),
            (
                Box::new(OpenAiResponses { codex: true }),
                SchemaFormat::Responses,
                "text",
                json!({"format":{"type":"json_schema", "name":"continuation",
                    "schema":schema, "strict":true}}),
            ),
            (
                Box::new(AnthropicMessages::direct()),
                SchemaFormat::Anthropic,
                "output_config",
                json!({"format":{"type":"json_schema", "schema":schema}}),
            ),
        ] {
            // Keep dispatch local so the wrapper itself needs no boxed-codec implementation.
            struct BorrowedCodec<'a>(&'a dyn WireCodec);
            impl WireCodec for BorrowedCodec<'_> {
                fn build_body(&self, request: &Request) -> flux_core::Result<Value> {
                    self.0.build_body(request)
                }
                fn map_stream(&self, bytes: ByteStream) -> ChunkStream {
                    self.0.map_stream(bytes)
                }
                fn wire_headers(&self) -> Vec<(&'static str, String)> {
                    self.0.wire_headers()
                }
            }
            let original = inner.build_body(&plain_request).unwrap();
            let codec = SchemaCodec {
                inner: BorrowedCodec(inner.as_ref()),
                format,
            };
            assert_eq!(codec.build_body(&plain_request).unwrap(), original);
            assert_eq!(codec.wire_headers(), inner.wire_headers());
            let mut structured = codec.build_body(&schema_request).unwrap();
            for (key, value) in expected.as_object().unwrap() {
                assert_eq!(&structured[container][key], value);
                // Value equality ignores object order, but constrained decoders
                // can use serialized property order to choose generation order.
                assert_eq!(structured[container][key].to_string(), value.to_string());
            }
            assert!(!structured.to_string().contains(METADATA_KEY));
            if let Some(value) = original.get(container) {
                for (key, original_value) in value.as_object().unwrap() {
                    assert_eq!(&structured[container][key], original_value);
                }
                structured[container] = value.clone();
            } else {
                structured.as_object_mut().unwrap().remove(container);
            }
            assert_eq!(
                structured, original,
                "schema must not change other request fields"
            );
        }
    }

    #[test]
    fn schemas_without_tools_explicitly_disable_tool_calls_and_keep_reasoning() {
        fn check<C: WireCodec>(codec: SchemaCodec<C>, choice: Value) {
            let mut model_request = request(true);
            model_request.tools.clear();
            let structured = convert_request(model_request.clone()).unwrap();
            model_request.response_schema = None;
            let plain = convert_request(model_request).unwrap();
            let original = codec.inner.build_body(&plain).unwrap();
            assert_eq!(codec.build_body(&plain).unwrap(), original);

            let body = codec.build_body(&structured).unwrap();
            assert_eq!(body["tool_choice"], choice);
            assert!(body.get("tools").is_none());
            for field in ["reasoning", "reasoning_effort", "thinking"] {
                assert_eq!(body.get(field), original.get(field));
            }
            assert_eq!(
                body["output_config"]["effort"],
                original["output_config"]["effort"]
            );
            assert!(structured.thinking);
            assert_eq!(structured.effort, Some(flux_provider::Effort::High));
        }

        check(
            SchemaCodec {
                inner: OpenAiChat,
                format: SchemaFormat::Chat,
            },
            json!("none"),
        );
        for codex in [false, true] {
            check(
                SchemaCodec {
                    inner: OpenAiResponses { codex },
                    format: SchemaFormat::Responses,
                },
                json!("none"),
            );
        }
        check(
            SchemaCodec {
                inner: AnthropicMessages::direct(),
                format: SchemaFormat::Anthropic,
            },
            json!({"type": "none"}),
        );
    }

    #[tokio::test]
    async fn wrapped_chat_stream_keeps_reasoning_separate_from_json_answer() {
        use futures_util::StreamExt;
        let codec = SchemaCodec {
            inner: OpenAiChat,
            format: SchemaFormat::Chat,
        };
        let data = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Consider the state\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"state\\\":\\\"done\\\"}\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let bytes: ByteStream = Box::pin(futures_util::stream::iter(vec![Ok(data
            .as_bytes()
            .to_vec()
            .into())]));
        let chunks = codec.map_stream(bytes).collect::<Vec<_>>().await;
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            Ok(flux_core::Chunk::ThinkingDelta(text)) if text == "Consider the state")));
        assert!(chunks.iter().any(|chunk| matches!(chunk,
            Ok(flux_core::Chunk::TextDelta(text)) if text == "{\"state\":\"done\"}")));
    }

    #[tokio::test]
    async fn schemas_use_supported_route_and_opaque_providers_fail_explicitly() {
        use crate::provider::{Provider, ProviderErrorKind, backends::flux::FluxProvider};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Recorder(Arc<AtomicUsize>);
        #[async_trait::async_trait]
        impl flux_provider::Provider for Recorder {
            fn name(&self) -> &str {
                "recorder"
            }
            async fn stream(&self, _: Request) -> flux_core::Result<ChunkStream> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Box::pin(futures_util::stream::empty()))
            }
        }
        let ordinary_calls = Arc::new(AtomicUsize::new(0));
        let schema_calls = Arc::new(AtomicUsize::new(0));
        let mut provider = FluxProvider::new(Recorder(ordinary_calls.clone()));
        let error = provider
            .invoke(request(true))
            .await
            .err()
            .expect("unsupported schema");
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert_eq!(ordinary_calls.load(Ordering::SeqCst), 0);
        provider.schema_inner = Some(Arc::new(Recorder(schema_calls.clone())));
        drop(provider.invoke(request(true)).await.unwrap());
        drop(provider.invoke(request(false)).await.unwrap());
        assert_eq!(schema_calls.load(Ordering::SeqCst), 1);
        assert_eq!(ordinary_calls.load(Ordering::SeqCst), 1);
    }
}

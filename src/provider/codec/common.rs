//! Shared model-facing input conversion, native field access, finish settlement
//! and opaque reasoning provenance.
use std::fmt;

use crate::{
    media::{AttachmentRef, ImageRef, MediaError, TextRef},
    named_enum::{NamedEnum, named_enum},
    provider::{
        ProviderError, ProviderErrorKind,
        codec::{CodecName, StateRole, ToolNames},
        protocol::{
            AssistantItem, Binding, BlockRef, Completion, CutReason, ItemKind, Message,
            ModelRequest, Position, Provenance, Replay, ReplayFormat, ResponseEvent, Scope, State,
            ToolDefinition, ToolResult, UserContent,
        },
    },
};
use serde_json::{Map, Value, json};

/// What every family requires of a tool definition: a name the endpoint
/// accepts and an object schema. A codec adds its own constraints.
pub(crate) fn check_tool(
    tool: &ToolDefinition,
    names: ToolNames,
    codec: CodecName,
) -> Result<(), ProviderError> {
    if names.accepts(&tool.name) && tool.input_schema.is_object() {
        return Ok(());
    }
    Err(ProviderErrorKind::InvalidRequest.error(format!(
        "{codec} tools require {} and an object JSON Schema",
        names.rule()
    )))
}

/// Tool-argument text as a JSON object: empty or null is `{}`, and a
/// double-encoded object is unwrapped. Anything else is `None`.
pub(crate) fn parse_tool_arguments(raw: &str) -> Option<Map<String, Value>> {
    if raw.trim().is_empty() {
        return Some(Map::new());
    }
    match serde_json::from_str::<Value>(raw).ok()? {
        Value::Object(object) => Some(object),
        Value::String(inner) => parse_tool_arguments(&inner),
        Value::Null => Some(Map::new()),
        _ => None,
    }
}

/// A tool-arguments field: JSON text, or the decoded object some servers send.
/// Missing or null is `{}`; any other type is `None`.
pub(crate) fn arguments_field(value: Option<&Value>) -> Option<Map<String, Value>> {
    match value {
        None | Some(Value::Null) => Some(Map::new()),
        Some(Value::String(text)) => parse_tool_arguments(text),
        Some(Value::Object(object)) => Some(object.clone()),
        Some(_) => None,
    }
}

/// A field that does not parse as `T` reads as absent: servers vary the types
/// of fields they do not document.
pub(crate) fn lenient<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    use serde::Deserialize;
    Ok(T::deserialize(Value::deserialize(deserializer)?).ok())
}

/// A counter field read by [`lenient_u64`]; anything else reads as absent.
pub(crate) fn lenient_count<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    use serde::Deserialize;
    Ok(lenient_u64(&Value::deserialize(deserializer)?))
}

/// Read a non-negative counter that some servers encode as a float or string.
pub(crate) fn lenient_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|float| float.fract() == 0.0 && *float >= 0.0 && *float <= u64::MAX as f64)
                .map(|float| float as u64)
        }),
        Value::String(text) => lenient_u64(&serde_json::from_str(text.trim()).ok()?),
        _ => None,
    }
}

pub(crate) fn blob_error(error: MediaError) -> ProviderError {
    ProviderErrorKind::InvalidRequest.error(format!("attachment: {error}"))
}

pub(crate) fn image_url(request: &ModelRequest, image: &ImageRef) -> Result<String, ProviderError> {
    let data = request.blobs.base64(&image.blob).map_err(blob_error)?;
    Ok(format!("data:{};base64,{data}", image.format.as_str()))
}

/// Attached text as the model sees it: its source file, if any, then the content.
pub(crate) fn attachment_text(
    request: &ModelRequest,
    text: &TextRef,
) -> Result<String, ProviderError> {
    let content = request.blobs.text(text).map_err(blob_error)?;
    Ok(match &text.file {
        Some(file) => format!("File: {file}\n{content}"),
        None => content.to_owned(),
    })
}

/// User content parts, with `image` encoding the protocol's image part.
pub(crate) fn user_parts(
    request: &ModelRequest,
    parts: &[UserContent],
    text_type: &str,
    image: impl Fn(&ImageRef) -> Result<Value, ProviderError>,
) -> Result<Vec<Value>, ProviderError> {
    let part = |text: &str| json!({"type":text_type, "text":text});
    parts
        .iter()
        .map(|content| match content.text() {
            Ok(text) => Ok(part(&text)),
            Err(AttachmentRef::Image(reference)) => image(reference),
            Err(AttachmentRef::Text(file)) => Ok(part(&attachment_text(request, file)?)),
        })
        .collect()
}

/// How an OpenAI protocol spells a state message.
pub(crate) struct StateWire {
    /// The role a system-role state takes.
    pub system: &'static str,
    /// The type of a text part.
    pub text: &'static str,
    /// A message of a role holding content parts.
    pub message: fn(&str, Value) -> Value,
}

/// Push a state the model reads, in `role`: a message of the wire's system role, or user
/// content joining the user message before it. The OpenAI protocols cannot hide a
/// superseded state, so it is omitted. Returns the message and part index of its text.
pub(crate) fn push_state(
    role: StateRole,
    wire: &StateWire,
    messages: &mut Vec<Value>,
    state: &State,
) -> Option<(usize, usize)> {
    if !state.is_read() {
        return None;
    }
    let part = json!({"type": wire.text, "text": state.text});
    if role == StateRole::User
        && let Some(last) = messages.len().checked_sub(1)
        && messages[last]["role"] == "user"
    {
        let content = messages[last]["content"]
            .as_array_mut()
            .expect("constructed array");
        content.push(part);
        return Some((last, content.len() - 1));
    }
    let role = match role {
        StateRole::System => wire.system,
        StateRole::User => "user",
    };
    messages.push((wire.message)(role, Value::Array(vec![part])));
    Some((messages.len() - 1, 0))
}

/// System segments as the single instruction text of the OpenAI protocols.
pub(crate) fn system_text(request: &ModelRequest) -> Option<String> {
    let segments: Vec<_> = request.system.iter().map(|s| s.text.as_str()).collect();
    (!segments.is_empty()).then(|| segments.join("\n\n"))
}

pub(crate) fn tool_text(tool: &ToolResult) -> String {
    // Keep the result and failure status distinguishable; a JSON result
    // that happens to contain similarly named keys must not overwrite metadata.
    json!({"result":tool.result,"is_error":tool.is_error}).to_string()
}

pub(crate) fn position(index: usize) -> Result<Position, ProviderError> {
    Position::try_from(index).map_err(|error| ProviderErrorKind::Protocol.error(error.to_string()))
}

/// A family's own native JSON, whose missing or mistyped fields are that
/// family's protocol errors.
#[derive(Clone, Copy)]
pub(crate) struct Native(pub CodecName);

impl Native {
    pub(crate) fn error(self, message: impl fmt::Display) -> ProviderError {
        ProviderErrorKind::Protocol.error(format!("{}: {message}", self.0))
    }

    pub(crate) fn string<'a>(self, value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| self.error(format_args!("missing or invalid {key}")))
    }

    pub(crate) fn index(self, value: &Value, key: &str) -> Result<usize, ProviderError> {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| self.error(format_args!("missing or invalid {key}")))
    }
}

/// A native object's `type`, as the family's enum names it.
pub(crate) fn tagged<T: NamedEnum>(value: &Value) -> Option<T> {
    value.get("type").and_then(Value::as_str).and_then(T::parse)
}

/// A delta for the one block of an index-addressed item.
pub(crate) fn delta(index: usize, kind: ItemKind, text: impl Into<String>) -> ResponseEvent {
    ResponseEvent::Delta {
        block: BlockRef::single(index),
        kind,
        text: text.into(),
    }
}

named_enum! {
    /// A finish reason as the families spell it, in any case.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) parsed enum StopReason {
        Normal = "end_turn" | "stop" | "eos" | "stop_sequence" | "tool_use" | "tool_calls",
        MaxTokens = "max_tokens" | "length" | "max_output_tokens" | "model_length",
        ContextWindow = "model_context_window_exceeded",
        Aborted = "aborted" | "abort" | "cancelled" | "canceled",
        Refusal = "refusal" | "content_filter" | "safety",
    }
}

impl StopReason {
    pub(crate) fn read(reason: &str) -> Option<Self> {
        Self::parse(&reason.to_ascii_lowercase())
    }
}

/// How a response's finish settled: normally, with its tool calls executable, or
/// cut, after which the decoder keeps text and reasoning but no calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    Normal,
    Cut(CutReason),
    /// A finish the service reports as a condition, not an outcome; streamed
    /// items are discarded as after an error frame.
    Error(ProviderErrorKind),
}

impl Finish {
    /// The finish a reason names. An unknown reason, like Messages'
    /// `pause_turn` whose calls are not final, ends without executing tools.
    pub(crate) fn of(reason: Option<StopReason>) -> Self {
        match reason {
            Some(StopReason::Normal) => Self::Normal,
            Some(StopReason::MaxTokens) => Self::Cut(CutReason::MaxTokens),
            // The service gave up on the response; retrying the request is standard.
            Some(StopReason::Aborted) => Self::Error(ProviderErrorKind::Unavailable),
            Some(StopReason::Refusal) => Self::Cut(CutReason::Refusal),
            // An overflow fails the attempt: its output is discarded and the context compacted.
            Some(StopReason::ContextWindow) => {
                Self::Error(ProviderErrorKind::ContextWindowExceeded)
            }
            None => Self::Cut(CutReason::Incomplete),
        }
    }

    pub(crate) fn complete(self, items: Vec<AssistantItem>) -> Result<Completion, ProviderError> {
        match self {
            Self::Normal => Ok(Completion::finished(items)?),
            Self::Cut(reason) => Ok(Completion::cut(items, reason)?),
            Self::Error(kind) => {
                Err(kind.error(format!("provider finished the response with {kind}")))
            }
        }
    }
}

/// Where a streamed response stands: producing output, settled on its finish
/// while late frames may still refine usage, or ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settlement {
    Open,
    Settled(Finish),
    Ended,
}

/// The finish flow of a stream whose finish reason may precede its end: the
/// first reason settles the response, later ones only revise it, and the end
/// marker or EOF completes it.
pub(crate) trait Settle {
    fn settlement(&mut self) -> &mut Settlement;
    fn has_tools(&self) -> bool;
    /// Reject output that `finish` cannot complete; an error finish wins unadmitted.
    fn admit(&self, finish: Finish) -> Result<(), ProviderError>;
    fn completion(&mut self, finish: Finish) -> Result<Completion, ProviderError>;

    /// A later abnormal reason retracts tool calls as a cut; nothing revives
    /// them, and a response without calls keeps its settled finish. A late error
    /// reason does not fail a response that already settled normally.
    fn reason(&mut self, finish: Finish) -> Result<(), ProviderError> {
        let settled = match (*self.settlement(), finish) {
            (Settlement::Open, Finish::Error(_)) => finish,
            (Settlement::Open, _) => {
                self.admit(finish)?;
                finish
            }
            (Settlement::Settled(Finish::Normal), Finish::Cut(reason)) if self.has_tools() => {
                Finish::Cut(reason)
            }
            (Settlement::Settled(Finish::Normal), Finish::Error(_)) if self.has_tools() => {
                Finish::Cut(CutReason::Incomplete)
            }
            _ => return Ok(()),
        };
        *self.settlement() = Settlement::Settled(settled);
        Ok(())
    }

    /// The end marker completes the response, settled or not.
    fn close(&mut self) -> Result<ResponseEvent, ProviderError> {
        let finish = match *self.settlement() {
            Settlement::Settled(finish) => finish,
            _ => {
                // Without a finish reason, tool calls are not provably complete.
                let finish = if self.has_tools() {
                    Finish::Cut(CutReason::Incomplete)
                } else {
                    Finish::Normal
                };
                self.admit(finish)?;
                finish
            }
        };
        self.complete(finish)
    }

    /// EOF completes a settled response; an open one fails with `unsettled`.
    fn eof(
        &mut self,
        unsettled: impl FnOnce() -> ProviderError,
    ) -> Result<Vec<ResponseEvent>, ProviderError> {
        match *self.settlement() {
            Settlement::Ended => Ok(Vec::new()),
            Settlement::Settled(finish) => Ok(vec![self.complete(finish)?]),
            Settlement::Open => Err(unsettled()),
        }
    }

    fn complete(&mut self, finish: Finish) -> Result<ResponseEvent, ProviderError> {
        *self.settlement() = Settlement::Ended;
        Ok(ResponseEvent::End(self.completion(finish)?))
    }
}

/// The replay `format` and `model` issued, if `replay` is one.
pub(crate) fn own_replay<'a>(
    replay: Option<&'a Replay>,
    format: ReplayFormat,
    model: &str,
) -> Option<&'a Replay> {
    replay.filter(|replay| replay.provenance.format == format && replay.provenance.model == model)
}

/// Whether `model` has sealed its `format` reasoning in this context. From then
/// on only sealed reasoning goes back; before that, unsigned reasoning is
/// replayed as well.
pub(crate) fn signed_context(request: &ModelRequest, format: ReplayFormat) -> bool {
    request
        .messages()
        .filter_map(|message| match message {
            Message::Assistant(items) => Some(items),
            _ => None,
        })
        .flatten()
        .filter_map(|item| own_replay(item.replay(), format, request.model.as_str()))
        .any(is_sealed)
}

/// Whether a service bound native reasoning to the conversation before it: a
/// nonempty signature, or nonempty opaque data (redacted or encrypted reasoning).
pub(crate) fn is_signed(signature: Option<&str>, data: Option<&str>) -> bool {
    [signature, data]
        .into_iter()
        .flatten()
        .any(|seal| !seal.is_empty())
}

/// Whether only the issuing service can read `replay` back: it carries a signature
/// or encrypted state. Such replay is bound to its conversation, except encrypted
/// Responses reasoning, which any history may carry.
pub(crate) fn is_sealed(replay: &Replay) -> bool {
    replay.binding == Binding::Conversation
        || (replay.provenance.format == ReplayFormat::Responses
            && super::responses::is_sealed(&replay.payload))
}

pub(crate) fn replay(
    format: ReplayFormat,
    model: &str,
    scope: &Scope,
    payload: Value,
    binding: Binding,
) -> Replay {
    Replay {
        provenance: Provenance {
            format,
            model: model.into(),
            scope: scope.clone(),
        },
        payload,
        binding,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::media::{BlobRef, ImageFormat};
    use crate::provider::protocol::{
        AssistantItem, Completion, LiveBlock, LiveResponse, Message, ModelRequest, Step, Usage,
    };

    /// The scope test decoders issue replay under.
    pub(crate) fn scope() -> Scope {
        Scope::try_from("scope".to_owned()).unwrap()
    }

    /// What a consumer sees of a decoded event sequence that reached its end.
    pub(crate) struct Reduced {
        pub(crate) blocks: Vec<LiveBlock>,
        pub(crate) usage: Usage,
        pub(crate) completion: Completion,
    }

    impl Reduced {
        pub(crate) fn items(&self) -> &[AssistantItem] {
            self.completion.items()
        }

        /// Provisional text per block, in arrival order.
        pub(crate) fn streamed(&self, kind: ItemKind) -> Vec<&str> {
            self.blocks
                .iter()
                .filter(|block| block.kind == kind)
                .map(|block| block.text.as_str())
                .collect()
        }
    }

    /// Decode `frames` through the stream's end and reduce them; a decoder error
    /// fails the test.
    pub(crate) fn drain(
        mut decoder: crate::provider::codec::Decoder,
        frames: impl IntoIterator<Item = crate::provider::http::transport::SseEvent>,
    ) -> Reduced {
        let mut events = Vec::new();
        for frame in frames {
            events.extend(decoder.decode(&frame).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        reduce(events)
    }

    /// Reduce events as the runtime does; a missing or non-final `End` is a decoder bug.
    pub(crate) fn reduce(events: impl IntoIterator<Item = ResponseEvent>) -> Reduced {
        let mut live = LiveResponse::default();
        let mut ended = None;
        for event in events {
            assert!(ended.is_none(), "event after End: {event:?}");
            // Keep what streamed, not the authoritative view the end supplies.
            let streamed = live.blocks().to_vec();
            match std::mem::take(&mut live).push(event) {
                Step::Open(open) => live = open,
                Step::Ended {
                    completion, usage, ..
                } => {
                    ended = Some(Reduced {
                        blocks: streamed,
                        usage,
                        completion,
                    });
                }
            }
        }
        ended.expect("response ended")
    }

    #[test]
    fn tool_arguments_tolerate_empty_null_and_double_encoded_objects() {
        for raw in ["", "  ", "null", "\"\"", "{}"] {
            assert_eq!(parse_tool_arguments(raw), Some(Map::new()), "{raw:?}");
        }
        let object = json!({"cmd":"ls","n":[1,{"x":null}]});
        assert_eq!(
            parse_tool_arguments(&object.to_string()).map(Value::Object),
            Some(object.clone())
        );
        let double = Value::String(object.to_string()).to_string();
        assert_eq!(
            parse_tool_arguments(&double).map(Value::Object),
            Some(object)
        );
        for raw in ["{\"cmd\":", "[]", "1", "\"text\"", "\"[1]\""] {
            assert_eq!(parse_tool_arguments(raw), None, "{raw:?}");
        }
    }

    /// Settles as the codecs do, recording only its settlement.
    struct Settling {
        settlement: Settlement,
        tools: bool,
    }

    impl Settle for Settling {
        fn settlement(&mut self) -> &mut Settlement {
            &mut self.settlement
        }

        fn has_tools(&self) -> bool {
            self.tools
        }

        fn admit(&self, _: Finish) -> Result<(), ProviderError> {
            Ok(())
        }

        fn completion(&mut self, finish: Finish) -> Result<Completion, ProviderError> {
            finish.complete(Vec::new())
        }
    }

    #[test]
    fn the_first_reason_settles_and_later_ones_only_retract_tools() {
        use Finish::{Cut, Error, Normal};
        use Settlement::{Open, Settled};
        let cut = Cut(CutReason::MaxTokens);
        let overflow = Error(ProviderErrorKind::ContextWindowExceeded);
        for (settlement, finish, tools, expected) in [
            (Open, Normal, true, Normal),
            (Open, overflow, true, overflow),
            (Settled(Normal), cut, true, cut),
            (Settled(Normal), overflow, true, Cut(CutReason::Incomplete)),
            (Settled(Normal), cut, false, Normal),
            (Settled(Normal), overflow, false, Normal),
            (Settled(cut), Normal, true, cut),
        ] {
            let mut settling = Settling { settlement, tools };
            settling.reason(finish).unwrap();
            let settled = Settled(expected);
            assert_eq!(
                settling.settlement, settled,
                "{settlement:?} then {finish:?}"
            );
        }
        for (reason, finish) in [
            ("STOP", Normal),
            ("tool_calls", Normal),
            ("eos", Normal),
            ("stop_sequence", Normal),
            ("length", cut),
            ("SAFETY", Cut(CutReason::Refusal)),
            ("canceled", Error(ProviderErrorKind::Unavailable)),
            ("model_context_window_exceeded", overflow),
            ("pause_turn", Cut(CutReason::Incomplete)),
        ] {
            assert_eq!(Finish::of(StopReason::read(reason)), finish, "{reason}");
        }
    }

    #[test]
    fn counters_accept_integral_floats_and_numeric_strings() {
        for (value, expected) in [
            (json!(7), Some(7)),
            (json!(7.0), Some(7)),
            (json!("7"), Some(7)),
            (json!(" 12 "), Some(12)),
            (json!(7.5), None),
            (json!(-1), None),
            (json!("-1"), None),
            (json!("seven"), None),
            (json!(null), None),
        ] {
            assert_eq!(lenient_u64(&value), expected, "{value}");
        }
    }

    /// A replay envelope under the shared test scope.
    pub(crate) fn envelope(
        format: ReplayFormat,
        model: &str,
        payload: Value,
        binding: Binding,
    ) -> Replay {
        replay(format, model, &scope(), payload, binding)
    }

    #[test]
    fn own_replay_requires_the_issuing_format_and_model() {
        let own = envelope(ReplayFormat::ChatText, "model", json!({}), Binding::Free);
        assert_eq!(
            own_replay(Some(&own), ReplayFormat::ChatText, "model"),
            Some(&own)
        );
        assert!(own_replay(Some(&own), ReplayFormat::Responses, "model").is_none());
        assert!(own_replay(Some(&own), ReplayFormat::ChatText, "other").is_none());
        assert!(own_replay(None, ReplayFormat::ChatText, "model").is_none());
    }

    pub(crate) fn image() -> ImageRef {
        ImageRef {
            file: Some("image.png".into()),
            format: ImageFormat::Png,
            blob: BlobRef::of(b"\x01\x02\x03"),
        }
    }

    pub(crate) fn notes() -> TextRef {
        TextRef {
            file: Some("notes.txt".into()),
            blob: BlobRef::of(b"notes"),
        }
    }

    /// A native Responses reasoning item with summaries and opaque private state.
    pub(crate) fn reasoning_item() -> Value {
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque+/=",
            "summary":[{"type":"summary_text", "text":"first"}, {"type":"summary_text", "text":"second"}],
            "future_native":{"state":"keep"}})
    }

    pub(crate) fn user(text: &str) -> Message {
        Message::User(vec![UserContent::Text { text: text.into() }])
    }

    pub(crate) fn tool_result(
        call_id: &str,
        name: &str,
        result: Value,
        images: Vec<ImageRef>,
        is_error: bool,
    ) -> Message {
        Message::Tool(vec![ToolResult {
            call_id: call_id.into(),
            name: name.into(),
            result,
            images,
            is_error,
        }])
    }

    /// Minimal request shared by codec/provider tests; cases override only the
    /// inputs relevant to the behavior under test.
    pub(crate) fn request(model: &str) -> ModelRequest {
        ModelRequest {
            context: crate::provider::protocol::entries(vec![user("hello")]),
            max_output_tokens: std::num::NonZeroU64::new(8192),
            ..ModelRequest::test(model)
        }
    }

    #[test]
    fn direct_provider_wire_parity_requires_a_loaded_blob() {
        let (image, text) = (image(), notes());
        let mut request = request("model");
        let missing = image_url(&request, &image).unwrap_err();
        assert_eq!(missing.kind(), ProviderErrorKind::InvalidRequest);
        assert!(attachment_text(&request, &text).is_err());
        request.blobs.insert(image.blob, b"\x01\x02\x03".to_vec());
        request.blobs.insert(text.blob, b"notes".to_vec());
        assert_eq!(
            image_url(&request, &image).unwrap(),
            "data:image/png;base64,AQID"
        );
        assert_eq!(
            attachment_text(&request, &text).unwrap(),
            "File: notes.txt\nnotes"
        );
        let pasted = TextRef { file: None, ..text };
        assert_eq!(attachment_text(&request, &pasted).unwrap(), "notes");
    }
}

//! Inference API families. Each codec encodes canonical requests and decodes native
//! streams under a typed dialect naming the wire conventions that vary between
//! servers of that family. Correctness rules live in the codec; a dialect only says
//! which convention applies.

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};

use crate::provider::{
    ProviderError,
    ProviderErrorKind::InvalidRequest,
    http::{
        Headers,
        errors::{self, ErrorSignals},
        transport::{Rejection, SseEvent},
    },
    protocol::{ContextId, ModelRequest, ResponseEvent, ResponseSchema, Scope},
};
use crate::tool::registry::{MAX_TOOL_NAME_BYTES, is_valid_tool_name};
use common::PreparedImages;

pub mod chat_completions;
pub(crate) mod common;
mod locations;
pub(crate) mod messages;
mod openai;
mod placement;
pub(crate) mod responses;
pub(crate) mod usage;

pub(crate) use locations::Locations;
pub(crate) use locations::set_each;
pub use locations::{LocationsError, NonEmpty};
pub use placement::{BodyPath, CacheKey, CacheTtl};
pub(crate) use placement::{
    BreakpointBudget, Cache, CacheControl, Identity, header, overlaps, path,
};
pub use responses::{CyberAccessProgram, ReasoningSummary};

crate::named_enum::named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum CodecName {
        ChatCompletions = "chat_completions",
        Responses = "responses",
        Messages = "messages",
    }
}

impl CodecName {
    /// The path the family serves under an API root.
    pub(crate) fn path_suffix(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }

    /// The `reasoning` profile values the family accepts.
    pub(crate) fn effort_levels(self) -> EffortLevels {
        match self {
            Self::ChatCompletions | Self::Responses => OPENAI_EFFORT,
            Self::Messages => MESSAGES_EFFORT,
        }
    }

    /// Write `effort` at each of `paths`. A journaled profile resumes on the
    /// live entry, which may speak another family since the profile was
    /// admitted, so the level is checked again here.
    pub(crate) fn place_effort(
        self,
        paths: &NonEmpty<BodyPath>,
        root: &mut serde_json::Map<String, Value>,
        effort: &str,
    ) -> Result<(), ProviderError> {
        if !self.effort_levels().accepts(effort) {
            return Err(InvalidRequest.error(format!("Unsupported reasoning effort: {effort}")));
        }
        set_each(paths.iter(), root, Value::from(effort))
    }
}

/// A family with its selected conventions; a Chat provider cannot carry Messages options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    ChatCompletions(chat_completions::Dialect),
    Responses(responses::Dialect),
    Messages(messages::Dialect),
}

crate::named_enum::named_enum! {
    /// The role runtime state takes on the wire.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    pub enum StateRole {
        /// The protocol's harness role: a system message on Messages, turn-scoped where
        /// its dialect says; a system or developer message on Chat; a developer message
        /// on Responses.
        System = "system",
        /// User content, for servers without a harness role mid-conversation: it joins
        /// the user turn before it, or is a user turn of its own.
        User = "user",
    }
}

/// An encoded request: the body and the headers the dialect places values in.
pub(crate) struct Encoded {
    pub body: serde_json::Map<String, Value>,
    pub headers: HeaderMap,
}

impl Encoded {
    /// Place the context identity where the dialect keeps it.
    fn place_identity(
        root: &mut serde_json::Map<String, Value>,
        headers: &mut HeaderMap,
        identity: &Identity,
        context: &ContextId,
    ) -> Result<(), ProviderError> {
        let identity_value = || Value::String(context.as_str().to_owned());
        for key in identity.cache_key.iter() {
            match key {
                CacheKey::Body(path) => path.set(root, identity_value())?,
                CacheKey::Header(name) => {
                    let value = HeaderValue::from_str(context.as_str())
                        .map_err(|_| InvalidRequest.error("invalid context identity header"))?;
                    headers.insert(name.clone(), value);
                }
            }
        }
        set_each(identity.user_id.iter(), root, identity_value())?;
        Ok(())
    }
}

impl Codec {
    pub(crate) fn name(&self) -> CodecName {
        match self {
            Self::ChatCompletions(_) => CodecName::ChatCompletions,
            Self::Responses(_) => CodecName::Responses,
            Self::Messages(_) => CodecName::Messages,
        }
    }

    /// Encode `request`, sending each image as `images` prepared it.
    pub(crate) fn encode(
        &self,
        request: &ModelRequest,
        images: PreparedImages<'_>,
        context: &ContextId,
    ) -> Result<Encoded, ProviderError> {
        let mut body = match self {
            Self::ChatCompletions(dialect) => chat_completions::encode(request, images, dialect)?,
            Self::Responses(dialect) => responses::encode(request, images, dialect)?,
            Self::Messages(dialect) => messages::encode(request, images, dialect)?,
        };
        let mut headers = HeaderMap::new();
        Encoded::place_identity(&mut body, &mut headers, self.identity(), context)?;
        Ok(Encoded { body, headers })
    }

    /// Headers every request of this family carries under these conventions.
    pub(crate) fn headers(&self) -> Headers {
        match self {
            Self::Messages(dialect) => dialect.headers(),
            Self::ChatCompletions(_) | Self::Responses(_) => Headers::default(),
        }
    }

    pub(crate) fn identity(&self) -> &Identity {
        match self {
            Self::ChatCompletions(dialect) => &dialect.identity,
            Self::Responses(dialect) => &dialect.identity,
            Self::Messages(dialect) => &dialect.identity,
        }
    }

    pub(crate) fn effort_mut(&mut self) -> &mut NonEmpty<BodyPath> {
        match self {
            Self::ChatCompletions(dialect) => &mut dialect.effort,
            Self::Responses(dialect) => &mut dialect.effort,
            Self::Messages(dialect) => &mut dialect.effort,
        }
    }

    pub(crate) fn state_role_mut(&mut self) -> &mut StateRole {
        match self {
            Self::ChatCompletions(dialect) => &mut dialect.state_role,
            Self::Responses(dialect) => &mut dialect.state_role,
            Self::Messages(dialect) => &mut dialect.state_role,
        }
    }

    pub(crate) fn identity_mut(&mut self) -> &mut Identity {
        match self {
            Self::ChatCompletions(dialect) => &mut dialect.identity,
            Self::Responses(dialect) => &mut dialect.identity,
            Self::Messages(dialect) => &mut dialect.identity,
        }
    }

    pub(crate) fn tool_names_mut(&mut self) -> &mut ToolNames {
        match self {
            Self::ChatCompletions(dialect) => &mut dialect.tool_names,
            Self::Responses(dialect) => &mut dialect.tool_names,
            Self::Messages(dialect) => &mut dialect.tool_names,
        }
    }

    /// How a response schema is transmitted, where the family has a choice.
    pub(crate) fn schema_mut(&mut self) -> Option<&mut SchemaConstraint> {
        match self {
            Self::ChatCompletions(dialect) => Some(&mut dialect.schema),
            Self::Responses(dialect) => Some(&mut dialect.schema),
            Self::Messages(_) => None,
        }
    }

    /// The prompt-cache mode, where the family has a choice.
    pub(crate) fn cache_mut(&mut self) -> Option<&mut Cache> {
        match self {
            Self::ChatCompletions(dialect) => Some(&mut dialect.cache),
            Self::Responses(dialect) => Some(&mut dialect.cache),
            Self::Messages(_) => None,
        }
    }

    /// The prompt-cache breakpoint lifetime, where these conventions mark breakpoints.
    pub(crate) fn cache_ttl_mut(&mut self) -> Option<&mut Option<CacheTtl>> {
        match self {
            Self::ChatCompletions(dialect) => dialect.cache_control.ttl_mut(),
            Self::Responses(dialect) => dialect.cache_control.ttl_mut(),
            Self::Messages(dialect) => Some(&mut dialect.cache_ttl),
        }
    }

    /// A rejected request's error, read with this family's vocabulary.
    pub(crate) fn error(&self, rejection: &Rejection, signals: ErrorSignals) -> ProviderError {
        let reading = match self {
            Self::ChatCompletions(_) | Self::Responses(_) => {
                openai::read::<openai::Code>(&rejection.body)
            }
            Self::Messages(_) => openai::read::<messages::Code>(&rejection.body),
        };
        errors::classify(
            Some(rejection.status),
            &rejection.body,
            reading,
            signals,
            rejection.retry_after,
        )
    }

    /// The decoder for a response to the serialized `body`, issuing replay
    /// under `scope` and reading errors with the vendor's evidence.
    pub(crate) fn decoder(
        &self,
        model: String,
        scope: Scope,
        body: &[u8],
        errors: ErrorSignals,
    ) -> Decoder {
        match self {
            Self::ChatCompletions(dialect) => {
                let format = dialect.reasoning_replay.format();
                let request = crate::media::BlobDigest::of(body);
                let decoder = chat_completions::Decoder::new(model, scope, format, errors, request);
                Decoder::ChatCompletions(decoder)
            }
            Self::Responses(dialect) => {
                Decoder::Responses(responses::Decoder::new(model, scope, dialect, errors))
            }
            Self::Messages(_) => Decoder::Messages(messages::Decoder::new(model, scope, errors)),
        }
    }
}

pub(crate) enum Decoder {
    ChatCompletions(chat_completions::Decoder),
    Responses(responses::Decoder),
    Messages(messages::Decoder),
}

impl Decoder {
    pub(crate) fn decode(&mut self, event: &SseEvent) -> Result<Vec<ResponseEvent>, ProviderError> {
        match self {
            Self::ChatCompletions(d) => d.decode(event),
            Self::Responses(d) => d.decode(event),
            Self::Messages(d) => d.decode(event),
        }
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<ResponseEvent>, ProviderError> {
        match self {
            Self::ChatCompletions(d) => d.finish(),
            Self::Responses(d) => d.finish(),
            Self::Messages(d) => d.finish(),
        }
    }
}

/// The `reasoning` profile values an endpoint accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffortLevels(pub(crate) &'static [&'static str]);

const OPENAI_EFFORT: EffortLevels =
    EffortLevels(&["none", "minimal", "low", "medium", "high", "xhigh", "max"]);

/// Messages thinking settings: `off` and `adaptive` select a thinking mode;
/// the rest are `output_config.effort` levels under adaptive thinking.
const MESSAGES_EFFORT: EffortLevels =
    EffortLevels(&["off", "adaptive", "low", "medium", "high", "xhigh", "max"]);

impl EffortLevels {
    pub(crate) fn accepts(&self, effort: &str) -> bool {
        self.0.contains(&effort)
    }
}

/// Whether a server takes back reasoning its service did not sign or encrypt, while
/// the context holds no signed reasoning; signed reasoning goes back either way.
/// `T` is where it goes, where the codec leaves that to the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnsignedReasoning<T = ()> {
    Replayed(T),
    /// The service accepts only reasoning it sealed.
    Dropped,
}

impl<T> UnsignedReasoning<T> {
    pub(crate) fn replayed(&self) -> Option<&T> {
        match self {
            Self::Replayed(at) => Some(at),
            Self::Dropped => None,
        }
    }
}

/// How a `json_schema` response format is sent; the schema always goes as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchemaConstraint {
    /// Flagged `strict`, for servers that enforce a schema only on request.
    Strict,
    /// Unflagged: the server compiles any schema into a grammar.
    Grammar,
}

impl SchemaConstraint {
    /// The `name`, `schema` and, where flagged, `strict` fields of a `json_schema` format.
    pub(crate) fn format(self, schema: &ResponseSchema) -> serde_json::Map<String, Value> {
        let mut format = serde_json::Map::from_iter([("name".into(), json!(schema.name()))]);
        if self == Self::Strict {
            format.insert("strict".into(), json!(true));
        }
        format.insert("schema".into(), schema.schema().clone());
        format
    }
}

const ANTHROPIC_MAX_TOOL_NAME_BYTES: usize = 128;

/// Which tool names an endpoint accepts: any nonblank name, or a bounded
/// run of ASCII letters, digits, underscores or hyphens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolNames {
    Any,
    OpenAi,
    Anthropic,
}

impl ToolNames {
    fn limit(self) -> Option<usize> {
        match self {
            Self::Any => None,
            Self::OpenAi => Some(MAX_TOOL_NAME_BYTES),
            Self::Anthropic => Some(ANTHROPIC_MAX_TOOL_NAME_BYTES),
        }
    }

    pub(crate) fn accepts(self, name: &str) -> bool {
        match self.limit() {
            None => !name.trim().is_empty(),
            Some(limit) => is_valid_tool_name(name, limit),
        }
    }

    pub(crate) fn rule(self) -> String {
        match self.limit() {
            None => "a nonblank name".into(),
            Some(limit) => format!("1–{limit} ASCII letters, digits, underscores or hyphens"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_placed_at_every_selected_header_and_field() {
        let request = common::tests::request("m");
        let context = "session".parse().unwrap();
        let keyed = Codec::Messages(messages::Dialect {
            identity: Identity {
                cache_key: Locations::one(header("x-session-id"))
                    .with(CacheKey::Body(path("metadata.session"))),
                user_id: Locations::one(path("metadata.user")),
            },
            ..messages::Dialect::standard()
        });
        let encoded = keyed
            .encode(&request, Default::default(), &context)
            .unwrap();
        assert_eq!(encoded.headers.len(), 1);
        assert_eq!(encoded.headers["x-session-id"], "session");
        assert_eq!(
            encoded.body["metadata"],
            serde_json::json!({"session": "session", "user": "session"})
        );
        let plain = Codec::Messages(messages::Dialect::standard());
        assert!(
            plain
                .encode(&request, Default::default(), &context)
                .unwrap()
                .headers
                .is_empty()
        );
    }

    /// Admission keeps placements clear of the listed fields, so every path an
    /// encoder writes outside its placements must be listed, down to nested
    /// fields; a placement beside a nested listed field keeps its siblings. Each
    /// listed field is written by some dialect.
    #[test]
    fn listed_fields_cover_every_body_path_an_encoder_writes() {
        use crate::provider::protocol::{ResponseSchema, SystemSegment, ToolDefinition};
        use serde_json::{Map, json};
        /// The paths under `prefix` that are neither listed nor lead to one.
        fn unlisted(prefix: &str, object: &Map<String, Value>, listed: &[&str]) -> Vec<String> {
            let mut found = Vec::new();
            for (key, value) in object {
                let path = match prefix {
                    "" => key.clone(),
                    _ => format!("{prefix}.{key}"),
                };
                if listed.contains(&path.as_str()) {
                    continue;
                }
                let leads = listed.iter().any(|field| overlaps(field, &path));
                match value.as_object() {
                    Some(inner) if leads => found.extend(unlisted(&path, inner, listed)),
                    _ => found.push(path),
                }
            }
            found
        }
        let mut request = common::tests::request("m");
        request.system = vec![SystemSegment {
            text: "s".into(),
            cache: true,
        }];
        request.tools = vec![ToolDefinition {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({"type":"object"}),
        }];
        request.response_schema = Some(
            ResponseSchema::new(
                "r",
                json!({"type":"object", "properties":{}, "additionalProperties":false}),
            )
            .unwrap(),
        );
        request.reasoning = Some("high".into());
        // Whether some row's body holds each listed field.
        let mut written = std::collections::BTreeMap::<_, bool>::new();
        let nested = messages::Dialect {
            output_limit: NonEmpty::one(path("output_config.max_tokens")),
            ..messages::Dialect::standard()
        };
        for (codec, fields, placed) in [
            (
                Codec::ChatCompletions(chat_completions::Dialect {
                    cache: Cache::Explicit,
                    ..chat_completions::Dialect::compatible()
                }),
                chat_completions::BODY_FIELDS,
                &["max_completion_tokens", "reasoning_effort"][..],
            ),
            (
                Codec::Responses(responses::Dialect {
                    reasoning_summary: Some(responses::ReasoningSummary::Auto),
                    cyber_access_program: Some(responses::CyberAccessProgram::DaybreakBlue),
                    cache: Cache::Explicit,
                    ..responses::Dialect::stateless()
                }),
                responses::BODY_FIELDS,
                &["max_output_tokens", "reasoning.effort", "prompt_cache_key"],
            ),
            // Top-level `cache_control` asks for automatic caching, so explicit mode omits it.
            (
                Codec::Responses(responses::Dialect {
                    cache_control: CacheControl::Sent { ttl: None },
                    ..responses::Dialect::stateless()
                }),
                responses::BODY_FIELDS,
                &["max_output_tokens", "reasoning.effort", "prompt_cache_key"],
            ),
            (
                Codec::Messages(messages::Dialect::standard()),
                messages::BODY_FIELDS,
                &["max_tokens", "output_config.effort"],
            ),
            (
                Codec::Messages(nested),
                messages::BODY_FIELDS,
                &["output_config.max_tokens", "output_config.effort"],
            ),
        ] {
            let body = Value::Object(
                codec
                    .encode(&request, Default::default(), &"c".parse().unwrap())
                    .unwrap()
                    .body,
            );
            let listed: Vec<_> = fields.iter().chain(placed).copied().collect();
            let name = codec.name();
            let found = unlisted("", body.as_object().unwrap(), &listed);
            assert!(found.is_empty(), "{name} writes unlisted {found:?}");
            for field in listed {
                let held = field
                    .split('.')
                    .try_fold(&body, |value, key| value.get(key));
                *written.entry((name.to_string(), field)).or_default() |= held.is_some();
            }
        }
        let lost: Vec<_> = written.into_iter().filter(|(_, held)| !held).collect();
        assert!(lost.is_empty(), "listed but never written: {lost:?}");
    }

    #[test]
    fn tool_name_rules() {
        assert!(ToolNames::Any.accepts("vendor.tool/雪"));
        assert!(!ToolNames::Any.accepts(" "));
        assert!(ToolNames::OpenAi.accepts(&"a".repeat(MAX_TOOL_NAME_BYTES)));
        assert!(!ToolNames::OpenAi.accepts(&"a".repeat(MAX_TOOL_NAME_BYTES + 1)));
        assert!(ToolNames::Anthropic.accepts(&"a".repeat(ANTHROPIC_MAX_TOOL_NAME_BYTES)));
        assert!(!ToolNames::Anthropic.accepts("vendor.tool"));
    }
}

//! Inference API families. Each codec encodes canonical requests and decodes native
//! streams under a typed dialect naming the wire conventions that vary between
//! servers of that family. Correctness rules live in the codec; a dialect only says
//! which convention applies.

use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::provider::{
    ProviderError,
    http::{
        Headers,
        errors::{self, ErrorSignals},
        transport::{Rejection, SseEvent},
    },
    protocol::{ContextId, ModelRequest, ResponseEvent, Scope},
};

pub mod chat_completions;
pub(crate) mod common;
pub(crate) mod messages;
mod openai;
mod placement;
pub(crate) mod responses;
pub(crate) mod usage;

pub use placement::{BodyPath, CacheKey, CacheTtl};
pub(crate) use placement::{Identity, header, overlaps, path};

crate::named_enum::named_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
}

/// A family with its selected conventions; a Chat provider cannot carry Messages options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    ChatCompletions(chat_completions::Dialect),
    Responses(responses::Dialect),
    Messages(messages::Dialect),
}

/// An encoded request: the body and the headers the dialect places values in.
pub(crate) struct Encoded {
    pub body: Value,
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
        match &identity.cache_key {
            CacheKey::Omitted => {}
            CacheKey::Body(path) => path.set(root, Value::String(context.as_str().to_owned()))?,
            CacheKey::Header(name) => {
                let value = HeaderValue::from_str(context.as_str())
                    .map_err(|_| common::invalid("invalid context identity header"))?;
                headers.insert(name.clone(), value);
            }
        }
        if let Some(path) = &identity.user_id {
            path.set(root, Value::String(context.as_str().to_owned()))?;
        }
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

    pub(crate) fn encode(
        &self,
        request: &ModelRequest,
        context: &ContextId,
    ) -> Result<Encoded, ProviderError> {
        let mut body = match self {
            Self::ChatCompletions(dialect) => chat_completions::encode(request, dialect)?,
            Self::Responses(dialect) => responses::encode(request, dialect)?,
            Self::Messages(dialect) => messages::encode(request, dialect)?,
        };
        let mut headers = HeaderMap::new();
        Encoded::place_identity(&mut body, &mut headers, self.identity(), context)?;
        Ok(Encoded {
            body: Value::Object(body),
            headers,
        })
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

    pub(crate) fn effort(&self) -> &Effort {
        match self {
            Self::ChatCompletions(dialect) => &dialect.effort,
            Self::Responses(dialect) => &dialect.effort,
            Self::Messages(dialect) => &dialect.effort,
        }
    }

    pub(crate) fn identity_mut(&mut self) -> &mut Identity {
        match self {
            Self::ChatCompletions(dialect) => &mut dialect.identity,
            Self::Responses(dialect) => &mut dialect.identity,
            Self::Messages(dialect) => &mut dialect.identity,
        }
    }

    /// A rejected request's error, read with this family's vocabulary.
    pub(crate) fn error(&self, rejection: &Rejection, signals: ErrorSignals) -> ProviderError {
        let reading = match self {
            Self::ChatCompletions(_) => chat_completions::read_error(&rejection.body),
            Self::Responses(_) => responses::read_error(&rejection.body),
            Self::Messages(_) => messages::read_error(&rejection.body),
        };
        errors::classify(
            Some(rejection.status),
            &rejection.body,
            reading,
            signals,
            rejection.retry_after,
        )
    }

    /// The decoder for a response to `body`, issuing replay under `scope` and
    /// reading errors with the vendor's evidence.
    pub(crate) fn decoder(
        &self,
        model: String,
        scope: Scope,
        body: &Value,
        errors: ErrorSignals,
    ) -> Decoder {
        match self {
            Self::ChatCompletions(dialect) => {
                let format = dialect.reasoning_replay.format();
                Decoder::ChatCompletions(
                    chat_completions::Decoder::new(model, scope, format, errors).for_request(body),
                )
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

pub(crate) const OPENAI_EFFORT: EffortLevels =
    EffortLevels(&["none", "minimal", "low", "medium", "high", "xhigh", "max"]);

impl EffortLevels {
    pub(crate) fn accepts(&self, effort: &str) -> bool {
        self.0.contains(&effort)
    }
}

/// Where the profile's `reasoning` goes, and the values accepted there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Effort {
    pub path: BodyPath,
    pub levels: EffortLevels,
}

impl Effort {
    /// Write `effort` at the path. A journaled profile resumes on the live
    /// entry, which may speak another family since the profile was admitted,
    /// so the level is checked again here.
    pub(crate) fn place(
        &self,
        root: &mut serde_json::Map<String, Value>,
        effort: &str,
    ) -> Result<(), ProviderError> {
        if !self.levels.accepts(effort) {
            return Err(common::invalid(format!(
                "Unsupported reasoning effort: {effort}"
            )));
        }
        self.path.set(root, Value::from(effort))
    }
}

/// Which tool names an endpoint accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolNames {
    /// Any nonblank name.
    Any,
    /// 1–64 ASCII letters, digits, underscores or hyphens.
    OpenAi,
    /// 1–128 ASCII letters, digits, underscores or hyphens.
    Anthropic,
}

impl ToolNames {
    pub(crate) fn accepts(self, name: &str) -> bool {
        let limit = match self {
            Self::Any => return !name.trim().is_empty(),
            Self::OpenAi => 64,
            Self::Anthropic => 128,
        };
        !name.is_empty()
            && name.len() <= limit
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }

    pub(crate) fn rule(self) -> &'static str {
        match self {
            Self::Any => "a nonblank name",
            Self::OpenAi => "1–64 ASCII letters, digits, underscores or hyphens",
            Self::Anthropic => "1–128 ASCII letters, digits, underscores or hyphens",
        }
    }
}

/// How a response schema is transmitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchemaConstraint {
    /// OpenAI strict mode: the schema is checked against its subset and sent with `strict`.
    OpenAiStrict,
    /// The server compiles the schema into a grammar; it is sent verbatim.
    Grammar,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_identity_is_placed_on_the_request() {
        let request = common::tests::request("m");
        let context = "session".parse().unwrap();
        let keyed = Codec::Messages(messages::Dialect {
            identity: Identity {
                cache_key: header("x-session-id"),
                user_id: None,
            },
            ..messages::Dialect::anthropic()
        });
        let encoded = keyed.encode(&request, &context).unwrap();
        assert_eq!(encoded.headers.len(), 1);
        assert_eq!(encoded.headers["x-session-id"], "session");
        let plain = Codec::Messages(messages::Dialect::anthropic());
        assert!(plain.encode(&request, &context).unwrap().headers.is_empty());
    }

    /// Admission keeps placements clear of the listed fields, so every path an
    /// encoder writes outside its placements must be listed, down to nested
    /// fields; a placement beside a nested listed field keeps its siblings.
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
        request.response_schema = Some(ResponseSchema {
            name: "r".into(),
            schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
        });
        request.reasoning = Some("high".into());
        let routed = chat_completions::Dialect {
            routing: Some(chat_completions::Routing {
                provider: chat_completions::ProviderPreferences {
                    zdr: Some(true),
                    ..Default::default()
                },
                fallback_models: vec!["fallback".into()],
            }),
            ..chat_completions::Dialect::compatible()
        };
        let nested = messages::Dialect {
            output_limit: path("output_config.max_tokens"),
            ..messages::Dialect::anthropic()
        };
        for (codec, fields, placed) in [
            (
                Codec::ChatCompletions(routed),
                chat_completions::BODY_FIELDS,
                &["max_completion_tokens", "reasoning_effort"][..],
            ),
            (
                Codec::Responses(responses::Dialect::stateless()),
                responses::BODY_FIELDS,
                &["max_output_tokens", "reasoning.effort", "prompt_cache_key"],
            ),
            (
                Codec::Messages(messages::Dialect::anthropic()),
                messages::BODY_FIELDS,
                &["max_tokens", "output_config.effort"],
            ),
            (
                Codec::Messages(nested),
                messages::BODY_FIELDS,
                &["output_config.max_tokens", "output_config.effort"],
            ),
        ] {
            let body = codec.encode(&request, &"c".parse().unwrap()).unwrap().body;
            let listed: Vec<_> = fields.iter().chain(placed).copied().collect();
            let name = codec.name();
            let found = unlisted("", body.as_object().unwrap(), &listed);
            assert!(found.is_empty(), "{name} writes unlisted {found:?}");
            for field in listed {
                let held = field
                    .split('.')
                    .try_fold(&body, |value, key| value.get(key));
                assert!(held.is_some(), "{name} lost `{field}`");
            }
        }
    }

    #[test]
    fn tool_name_rules_and_effort_levels() {
        assert!(ToolNames::Any.accepts("vendor.tool/雪"));
        assert!(!ToolNames::Any.accepts(" "));
        assert!(ToolNames::OpenAi.accepts(&"a".repeat(64)));
        assert!(!ToolNames::OpenAi.accepts(&"a".repeat(65)));
        assert!(ToolNames::Anthropic.accepts(&"a".repeat(128)));
        assert!(!ToolNames::Anthropic.accepts("vendor.tool"));
        assert!(OPENAI_EFFORT.accepts("xhigh"));
        assert!(!OPENAI_EFFORT.accepts("adaptive"));
    }
}

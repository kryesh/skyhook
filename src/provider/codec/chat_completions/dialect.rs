//! The conventions that vary between Chat Completions servers.
use serde::{Deserialize, Serialize};

use crate::provider::{
    codec::{
        BodyPath, CacheTtl, Effort, Identity, OPENAI_EFFORT, SchemaConstraint, ToolNames, path,
    },
    protocol::ReplayFormat,
};

/// The role carrying the system prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SystemRole {
    System,
    /// OpenAI reasoning models; compatible servers may reject it.
    Developer,
}

/// How an assistant turn's reasoning is replayed. Standard Chat has no
/// portable field, so the spelling is the server's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningReplay {
    /// Reasoning stays local; the server rejects unknown message keys.
    Unsupported,
    /// The reasoning text under this key of the assistant message.
    Text(BodyPath),
    /// The service's signed thinking blocks, kept whole and replayed under this
    /// key of the assistant message. Like Messages thinking, they are bound to
    /// the exact conversation before them.
    ThinkingBlocks(BodyPath),
    /// A router's `reasoning_details` objects (text, encrypted, or summary),
    /// kept whole and replayed verbatim in order under this key of the
    /// assistant message; bound to the conversation.
    Details(BodyPath),
}

impl ReasoningReplay {
    /// Where the assistant message carries the replay.
    pub(crate) fn path(&self) -> Option<&BodyPath> {
        match self {
            Self::Unsupported => None,
            Self::Text(path) | Self::ThinkingBlocks(path) | Self::Details(path) => Some(path),
        }
    }

    /// The same shape under another key; text where nothing was replayed.
    pub(crate) fn at(&self, path: BodyPath) -> Self {
        match self {
            Self::Unsupported | Self::Text(_) => Self::Text(path),
            Self::ThinkingBlocks(_) => Self::ThinkingBlocks(path),
            Self::Details(_) => Self::Details(path),
        }
    }

    /// The replay this convention sends back; decoded reasoning is text otherwise.
    pub(crate) fn format(&self) -> ReplayFormat {
        match self {
            Self::Unsupported | Self::Text(_) => ReplayFormat::ChatText,
            Self::ThinkingBlocks(_) => ReplayFormat::ChatThinkingBlock,
            Self::Details(_) => ReplayFormat::ChatReasoningDetail,
        }
    }
}

/// How an assistant turn without text spells `content`. Canonical history has
/// no text item for such a turn; the wire must still say so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmptyContent {
    /// The standard API's `null`.
    Null,
    /// `""`, for chat templates that reject a null or missing `content`.
    EmptyString,
}

/// Prompt-cache breakpoints, which standard Chat has no field for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cache {
    Unsupported,
    /// `cache_control: {type: ephemeral}` on marked system segments and on the
    /// last text part of history, which forces the parts form on those messages.
    ContentPartBreakpoints {
        /// The breakpoint lifetime, where the server takes one.
        ttl: Option<CacheTtl>,
    },
}

/// How usage is requested on a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageRequest {
    StreamOptions,
    /// The server always reports usage and may reject `stream_options`.
    Implicit,
}

/// OpenRouter's routing: the `provider` preferences object and the `models`
/// fallback list.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    #[serde(flatten)]
    pub provider: ProviderPreferences,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
}

/// The `provider` object. `require_parameters` is forced on whenever a response
/// schema is sent, so no endpoint that ignores it is routed to.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderPreferences {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quantizations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DataCollection {
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub system: SystemRole,
    /// Where `max_output` goes; None when the endpoint manages the limit.
    pub output_limit: Option<BodyPath>,
    pub effort: Effort,
    pub reasoning_replay: ReasoningReplay,
    pub empty_content: EmptyContent,
    pub usage_request: UsageRequest,
    pub tool_names: ToolNames,
    /// A flag some servers need before streaming tool arguments.
    pub tool_stream: Option<BodyPath>,
    pub schema: SchemaConstraint,
    pub identity: Identity,
    pub cache: Cache,
    /// Routing preferences a router takes in the body.
    pub routing: Option<Routing>,
}

impl Dialect {
    /// What standards-following servers accept. Schemas are grammar-enforced,
    /// and reasoning is replayed as `reasoning_content`.
    pub(crate) fn compatible() -> Self {
        Self {
            system: SystemRole::System,
            output_limit: Some(const { path("max_completion_tokens") }),
            effort: Effort {
                path: const { path("reasoning_effort") },
                levels: OPENAI_EFFORT,
            },
            reasoning_replay: ReasoningReplay::Text(const { path("reasoning_content") }),
            empty_content: EmptyContent::EmptyString,
            usage_request: UsageRequest::StreamOptions,
            tool_names: ToolNames::Any,
            tool_stream: None,
            schema: SchemaConstraint::Grammar,
            identity: Identity::OMITTED,
            cache: Cache::Unsupported,
            routing: None,
        }
    }
}

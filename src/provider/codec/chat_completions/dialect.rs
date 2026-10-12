//! The conventions that vary between Chat Completions servers.
use serde::{Deserialize, Serialize};

use crate::provider::{
    codec::{
        BodyPath, CacheTtl, Effort, Identity, Locations, NonEmpty, OPENAI_EFFORT, SchemaConstraint,
        StateRole, ToolNames, path,
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
    /// Reasoning stays local.
    Unsupported,
    /// The reasoning text under this key of the assistant message.
    Text(NonEmpty<BodyPath>),
    /// The service's signed thinking blocks, kept whole and replayed under this
    /// key of the assistant message. Like Messages thinking, they are bound to
    /// the exact conversation before them.
    ThinkingBlocks(NonEmpty<BodyPath>),
    /// A router's `reasoning_details` objects (text, encrypted, or summary),
    /// kept whole and replayed verbatim in order under this key of the
    /// assistant message; bound to the conversation.
    Details(NonEmpty<BodyPath>),
}

impl ReasoningReplay {
    /// Where the assistant message carries the replay.
    pub(crate) fn paths(&self) -> Option<&NonEmpty<BodyPath>> {
        match self {
            Self::Unsupported => None,
            Self::Text(path) | Self::ThinkingBlocks(path) | Self::Details(path) => Some(path),
        }
    }

    /// The same shape under other keys; text where nothing was replayed.
    pub(crate) fn at(&self, path: NonEmpty<BodyPath>) -> Self {
        match self {
            Self::Unsupported | Self::Text(_) => Self::Text(path),
            Self::ThinkingBlocks(_) => Self::ThinkingBlocks(path),
            Self::Details(_) => Self::Details(path),
        }
    }

    /// The reasoning this convention sends back; decoded reasoning is text otherwise.
    pub(crate) fn format(&self) -> ReasoningFormat {
        match self {
            Self::Unsupported | Self::Text(_) => ReasoningFormat::Text,
            Self::ThinkingBlocks(_) => ReasoningFormat::ThinkingBlocks,
            Self::Details(_) => ReasoningFormat::Details,
        }
    }
}

/// Which reasoning a decoder keeps for replay: the text, or one of the native
/// fields whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningFormat {
    Text,
    ThinkingBlocks,
    Details,
}

impl ReasoningFormat {
    pub(crate) fn replay(self) -> ReplayFormat {
        match self {
            Self::Text => ReplayFormat::ChatText,
            Self::ThinkingBlocks => ReplayFormat::ChatThinkingBlock,
            Self::Details => ReplayFormat::ChatReasoningDetail,
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

crate::provider::settings::settings! {
    /// OpenRouter's routing: the `provider` preferences object and the `models`
    /// fallback list.
    #[derive(Default)]
    pub struct Routing => RoutingPatch {
        @flatten { pub provider: ProviderPreferences }
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub fallback_models: Vec<String> => default,
    }
}

crate::provider::settings::settings! {
    /// The `provider` object. `require_parameters` is forced on whenever a response
    /// schema is sent, so no endpoint that ignores it is routed to.
    #[derive(Default)]
    pub struct ProviderPreferences => ProviderPreferencesPatch {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub order: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub allow_fallbacks: Option<bool> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub require_parameters: Option<bool> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub data_collection: Option<DataCollection> => default,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub quantizations: Vec<String> => default,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub zdr: Option<bool> => default,
    }
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
    /// Where `max_output` goes; none when the endpoint manages the limit.
    pub output_limit: Locations<BodyPath>,
    pub effort: Effort,
    pub reasoning_replay: ReasoningReplay,
    pub empty_content: EmptyContent,
    pub usage_request: UsageRequest,
    pub tool_names: ToolNames,
    /// A flag some servers need before streaming tool arguments.
    pub tool_stream: Locations<BodyPath>,
    pub schema: SchemaConstraint,
    pub identity: Identity,
    pub cache: Cache,
    /// Routing preferences a router takes in the body.
    pub routing: Option<Routing>,
    pub state_role: StateRole,
}

impl Dialect {
    /// What standards-following servers accept. Schemas are grammar-enforced,
    /// and reasoning is replayed as `reasoning_content`.
    pub(crate) fn compatible() -> Self {
        Self {
            system: SystemRole::System,
            output_limit: Locations::one(const { path("max_completion_tokens") }),
            effort: Effort {
                path: NonEmpty::one(const { path("reasoning_effort") }),
                levels: OPENAI_EFFORT,
            },
            reasoning_replay: ReasoningReplay::Text(NonEmpty::one(
                const { path("reasoning_content") },
            )),
            empty_content: EmptyContent::EmptyString,
            usage_request: UsageRequest::StreamOptions,
            tool_names: ToolNames::Any,
            tool_stream: Locations::NONE,
            schema: SchemaConstraint::Grammar,
            identity: Identity::NONE,
            cache: Cache::Unsupported,
            routing: None,
            state_role: StateRole::User,
        }
    }
}

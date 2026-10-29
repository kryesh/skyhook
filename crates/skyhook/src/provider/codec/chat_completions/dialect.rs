//! The conventions that vary between Chat Completions servers.
use crate::provider::{
    codec::{
        BodyPath, BreakpointBudget, Cache, CacheControl, Identity, Locations, NonEmpty,
        SchemaConstraint, StateRole, ToolNames, UnsignedReasoning, path,
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
    /// The service's signed thinking blocks, kept whole. Like Messages
    /// thinking, they are bound to the exact conversation before them.
    ThinkingBlocks(NativeReplay),
    /// A router's `reasoning_details` objects (text, encrypted, or summary),
    /// kept whole and replayed verbatim in order; bound to the conversation.
    Details(NativeReplay),
}

/// Where native reasoning objects go back on the assistant message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeReplay {
    /// The turn's objects, in order.
    pub objects: NonEmpty<BodyPath>,
    /// Unsigned objects go back with reasoning that arrived as plain text, under
    /// these keys; none where the server takes only objects.
    pub unsigned: UnsignedReasoning<Locations<BodyPath>>,
}

impl ReasoningReplay {
    /// Where the assistant message carries the replay.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &BodyPath> {
        let (primary, text) = match self {
            Self::Unsupported => (None, None),
            Self::Text(path) => (Some(path), None),
            Self::ThinkingBlocks(native) | Self::Details(native) => {
                (Some(&native.objects), native.unsigned.replayed())
            }
        };
        (primary.into_iter().flat_map(NonEmpty::iter))
            .chain(text.into_iter().flat_map(Locations::iter))
    }

    /// The same shape under other keys; text where nothing was replayed.
    pub(crate) fn at(&self, path: NonEmpty<BodyPath>) -> Self {
        let native = |native: &NativeReplay| NativeReplay {
            objects: path.clone(),
            ..native.clone()
        };
        match self {
            Self::Unsupported | Self::Text(_) => Self::Text(path),
            Self::ThinkingBlocks(blocks) => Self::ThinkingBlocks(native(blocks)),
            Self::Details(details) => Self::Details(native(details)),
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

/// How usage is requested on a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageRequest {
    StreamOptions,
    /// The server always reports usage and may reject `stream_options`.
    Implicit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub system: SystemRole,
    /// Where `max_output` goes; none when the endpoint manages the limit.
    pub output_limit: Locations<BodyPath>,
    /// Where the profile's `reasoning` goes.
    pub effort: NonEmpty<BodyPath>,
    pub reasoning_replay: ReasoningReplay,
    pub empty_content: EmptyContent,
    pub usage_request: UsageRequest,
    pub tool_names: ToolNames,
    /// A flag some servers need before streaming tool arguments.
    pub tool_stream: Locations<BodyPath>,
    pub schema: SchemaConstraint,
    pub identity: Identity,
    /// Breakpoints go on marked system segments and history boundaries, within
    /// `breakpoints`, which forces the parts form on those messages: as
    /// `prompt_cache_breakpoint` in explicit mode, or as `cache_control` in any.
    pub cache: Cache,
    pub cache_control: CacheControl,
    pub breakpoints: BreakpointBudget,
    pub state_role: StateRole,
}

impl Dialect {
    /// What standards-following servers accept. Schemas are grammar-enforced,
    /// and reasoning is replayed as `reasoning_content`.
    pub(crate) fn compatible() -> Self {
        Self {
            system: SystemRole::System,
            output_limit: Locations::one(const { path("max_completion_tokens") }),
            effort: NonEmpty::one(const { path("reasoning_effort") }),
            reasoning_replay: ReasoningReplay::Text(NonEmpty::one(
                const { path("reasoning_content") },
            )),
            empty_content: EmptyContent::EmptyString,
            usage_request: UsageRequest::StreamOptions,
            tool_names: ToolNames::Any,
            tool_stream: Locations::NONE,
            schema: SchemaConstraint::Grammar,
            identity: Identity::NONE,
            cache: Cache::Automatic,
            cache_control: CacheControl::Omitted,
            // A request makes at most four explicit cache writes.
            breakpoints: BreakpointBudget::new(4),
            state_role: StateRole::User,
        }
    }
}

//! The conventions that vary between Responses servers.
use crate::provider::codec::{
    BodyPath, CacheKey, Effort, Identity, OPENAI_EFFORT, ToolNames, path,
};

/// Whether `instructions` is sent without system segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Instructions {
    WhenPresent,
    /// The service rejects requests without the field.
    RequiredEvenWhenEmpty,
}

/// Whether reasoning summaries are requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReasoningSummary {
    Requested,
    /// The service rejects `reasoning.summary`.
    Unsupported,
}

/// What the terminal event says about streamed output items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalOutput {
    /// Every streamed item is restated in the terminal `output`.
    Restated,
    /// The terminal `output` may be empty; streamed items are authoritative.
    /// Premature EOF still fails.
    StreamedOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub instructions: Instructions,
    /// Where `max_output` goes; None when the endpoint manages the limit.
    pub output_limit: Option<BodyPath>,
    pub reasoning_summary: ReasoningSummary,
    pub effort: Effort,
    pub identity: Identity,
    pub terminal_output: TerminalOutput,
    /// Event types that are transport metadata, not output. Unknown semantic
    /// events still fail.
    pub metadata_events: &'static [&'static str],
    pub tool_names: ToolNames,
}

impl Dialect {
    /// Stateless use of the standard API: nothing stored server-side, encrypted
    /// reasoning replayed by the client.
    pub(crate) fn stateless() -> Self {
        Self {
            instructions: Instructions::WhenPresent,
            output_limit: Some(const { path("max_output_tokens") }),
            reasoning_summary: ReasoningSummary::Requested,
            effort: Effort {
                path: const { path("reasoning.effort") },
                levels: OPENAI_EFFORT,
            },
            identity: Identity {
                cache_key: CacheKey::Body(const { path("prompt_cache_key") }),
                user_id: None,
            },
            terminal_output: TerminalOutput::Restated,
            metadata_events: &[],
            tool_names: ToolNames::Any,
        }
    }
}

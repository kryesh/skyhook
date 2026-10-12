//! The conventions that vary between Responses servers.
use crate::provider::codec::{
    BodyPath, CacheKey, Effort, Identity, Locations, NonEmpty, OPENAI_EFFORT, StateRole, ToolNames,
    path,
};

/// Whether `instructions` is sent without system segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Instructions {
    WhenPresent,
    /// The service rejects requests without the field.
    RequiredEvenWhenEmpty,
}

crate::named_enum::named_enum! {
    /// The detail of the reasoning summaries requested, as `reasoning.summary` spells it.
    #[derive(Clone, Copy, Debug, Default, serde::Serialize, PartialEq, Eq)]
    pub enum ReasoningSummary {
        #[default]
        Auto = "auto",
        Concise = "concise",
        Detailed = "detailed",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub instructions: Instructions,
    /// Where `max_output` goes; none when the endpoint manages the limit.
    pub output_limit: Locations<BodyPath>,
    /// None for a service that rejects `reasoning.summary`.
    pub reasoning_summary: Option<ReasoningSummary>,
    pub effort: Effort,
    pub identity: Identity,
    /// Event types that are transport metadata, not output. Unknown semantic
    /// events still fail.
    pub metadata_events: &'static [&'static str],
    pub tool_names: ToolNames,
    pub state_role: StateRole,
}

impl Dialect {
    /// Stateless use of the standard API: nothing stored server-side, encrypted
    /// reasoning replayed by the client.
    pub(crate) fn stateless() -> Self {
        Self {
            instructions: Instructions::WhenPresent,
            output_limit: Locations::one(const { path("max_output_tokens") }),
            reasoning_summary: Some(ReasoningSummary::Auto),
            effort: Effort {
                path: NonEmpty::one(const { path("reasoning.effort") }),
                levels: OPENAI_EFFORT,
            },
            identity: Identity {
                cache_key: Locations::one(CacheKey::Body(const { path("prompt_cache_key") })),
                user_id: Locations::NONE,
            },
            metadata_events: &[],
            tool_names: ToolNames::Any,
            state_role: StateRole::User,
        }
    }
}

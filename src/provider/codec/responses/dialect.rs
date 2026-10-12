//! The conventions that vary between Responses servers.
use crate::{
    newtype::{Blank, nonblank, string_newtype},
    provider::codec::{
        BodyPath, CacheKey, Effort, Identity, Locations, NonEmpty, OPENAI_EFFORT, StateRole,
        ToolNames, path,
    },
};

/// Whether `instructions` is sent without system segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Instructions {
    WhenPresent,
    /// The service rejects requests without the field.
    RequiredEvenWhenEmpty,
}

/// Explicit-only caching is opt-in; otherwise the backend keeps its defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Cache {
    #[default]
    Automatic,
    Explicit,
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

string_newtype! {
    /// A service tier's wire ID. Catalogs may advertise tiers unknown to this client.
    pub struct ServiceTier(Blank) = |value| nonblank("service_tier", value);
}

crate::named_enum::named_enum! {
    /// The cybersecurity access program requested, not the account's approval level.
    #[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq)]
    pub enum CyberAccessProgram {
        Standard = "standard",
        DaybreakBlue = "daybreak_blue",
        DaybreakRed = "daybreak_red",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub instructions: Instructions,
    /// Where `max_output` goes; none when the endpoint manages the limit.
    pub output_limit: Locations<BodyPath>,
    /// None for a service that rejects `reasoning.summary`.
    pub reasoning_summary: Option<ReasoningSummary>,
    pub service_tier: Option<ServiceTier>,
    pub cyber_access_program: Option<CyberAccessProgram>,
    pub cache: Cache,
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
            service_tier: None,
            cyber_access_program: None,
            cache: Cache::Automatic,
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

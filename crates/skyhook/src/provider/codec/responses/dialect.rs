//! The conventions that vary between Responses servers.
use crate::provider::codec::{
    BodyPath, Cache, CacheControl, CacheKey, Identity, Locations, NonEmpty, SchemaConstraint,
    StateRole, UnsignedReasoning, path,
};
use skyhook_base::enums::NamedEnum;

/// Whether `instructions` is sent without system segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Instructions {
    WhenPresent,
    /// The service rejects requests without the field.
    RequiredEvenWhenEmpty,
}

/// The detail of the reasoning summaries requested, as `reasoning.summary` spells it.
#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
}

/// The cybersecurity access program requested, not the account's approval level.
#[derive(Clone, Copy, Debug, serde::Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum CyberAccessProgram {
    Standard,
    DaybreakBlue,
    DaybreakRed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Dialect {
    pub instructions: Instructions,
    /// Where `max_output` goes; none when the endpoint manages the limit.
    pub output_limit: Locations<BodyPath>,
    /// The dialect's default, which configuration may replace or withhold.
    pub reasoning_summary: Option<ReasoningSummary>,
    pub cyber_access_program: Option<CyberAccessProgram>,
    pub cache: Cache,
    /// Sent top level, asking the server to place breakpoints itself.
    pub cache_control: CacheControl,
    /// Where the profile's `reasoning` goes.
    pub effort: NonEmpty<BodyPath>,
    pub identity: Identity,
    /// Event types that are transport metadata, not output; skipped wherever
    /// they arrive, even after the terminal response.
    pub metadata_events: &'static [&'static str],
    pub schema: SchemaConstraint,
    pub state_role: StateRole,
    /// Unencrypted reasoning items.
    pub unsigned: UnsignedReasoning,
}

impl Dialect {
    /// Stateless use of the standard API: nothing stored server-side, encrypted
    /// reasoning replayed by the client, schemas sent as written, and no summary
    /// requested.
    pub(crate) fn stateless() -> Self {
        Self {
            instructions: Instructions::WhenPresent,
            output_limit: Locations::one(const { path("max_output_tokens") }),
            reasoning_summary: None,
            cyber_access_program: None,
            cache: Cache::Automatic,
            cache_control: CacheControl::Omitted,
            effort: NonEmpty::one(const { path("reasoning.effort") }),
            identity: Identity {
                cache_key: Locations::one(CacheKey::Body(const { path("prompt_cache_key") })),
                user_id: Locations::NONE,
            },
            metadata_events: &[],
            schema: SchemaConstraint::Grammar,
            state_role: StateRole::User,
            unsigned: UnsignedReasoning::Replayed(()),
        }
    }
}

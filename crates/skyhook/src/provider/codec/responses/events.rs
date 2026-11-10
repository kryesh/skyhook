//! The native event types.

use super::*;
use skyhook_base::enums::NamedEnum;

/// The native event types this codec reads; others carry nothing it uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub(super) enum Event {
    #[serde(rename = "response.output_item.added")]
    ItemAdded,
    #[serde(rename = "response.output_item.done")]
    ItemDone,
    #[serde(rename = "response.output_text.delta")]
    TextDelta,
    #[serde(rename = "response.output_text.done")]
    TextDone,
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta,
    #[serde(rename = "response.refusal.done")]
    RefusalDone,
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningDelta,
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningDone,
    #[serde(rename = "response.reasoning_summary_text.delta")]
    SummaryDelta,
    #[serde(rename = "response.reasoning_summary_text.done")]
    SummaryDone,
    #[serde(rename = "response.function_call_arguments.delta")]
    ArgumentsDelta,
    #[serde(rename = "response.function_call_arguments.done")]
    ArgumentsDone,
    #[serde(rename = "response.content_part.added")]
    PartAdded,
    #[serde(rename = "response.content_part.done")]
    PartDone,
    #[serde(rename = "response.reasoning_part.added")]
    ReasoningPartAdded,
    #[serde(rename = "response.reasoning_part.done")]
    ReasoningPartDone,
    #[serde(rename = "response.reasoning_summary_part.added")]
    SummaryPartAdded,
    #[serde(rename = "response.reasoning_summary_part.done")]
    SummaryPartDone,
    #[serde(rename = "response.completed")]
    Completed,
    #[serde(rename = "response.incomplete")]
    Incomplete,
    #[serde(rename = "response.failed")]
    Failed,
    #[serde(alias = "response.error")]
    Error,
    #[serde(
        rename = "response.aborted",
        alias = "response.cancelled",
        alias = "response.canceled",
        alias = "response.interrupted"
    )]
    Aborted,
}

impl Event {
    /// The kind of item an event on a live item addresses; `None` for the
    /// generic content-part events, whose item decides.
    pub(super) fn item_kind(self) -> Option<ItemKind> {
        match self {
            Self::TextDelta | Self::TextDone | Self::RefusalDelta | Self::RefusalDone => {
                Some(ItemKind::Text)
            }
            Self::ReasoningDelta
            | Self::ReasoningDone
            | Self::SummaryDelta
            | Self::SummaryDone
            | Self::ReasoningPartAdded
            | Self::ReasoningPartDone
            | Self::SummaryPartAdded
            | Self::SummaryPartDone => Some(ItemKind::Reasoning),
            Self::ArgumentsDelta | Self::ArgumentsDone => Some(ItemKind::ToolCall),
            _ => None,
        }
    }

    /// Whether the event addresses the summary namespace of a reasoning item.
    pub(super) fn is_summary(self) -> bool {
        matches!(
            self,
            Self::SummaryDelta | Self::SummaryDone | Self::SummaryPartAdded | Self::SummaryPartDone
        )
    }

    /// Whether a content-part event ends its part rather than announcing it.
    pub(super) fn is_done(self) -> bool {
        matches!(
            self,
            Self::PartDone | Self::ReasoningPartDone | Self::SummaryPartDone
        )
    }
}

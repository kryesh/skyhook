//! The native event types.

use super::*;
use crate::named_enum::named_enum;

named_enum! {
    /// The native event types this codec reads; others carry nothing it uses.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) parsed enum Event {
        ItemAdded = "response.output_item.added",
        ItemDone = "response.output_item.done",
        TextDelta = "response.output_text.delta",
        TextDone = "response.output_text.done",
        RefusalDelta = "response.refusal.delta",
        RefusalDone = "response.refusal.done",
        ReasoningDelta = "response.reasoning_text.delta",
        ReasoningDone = "response.reasoning_text.done",
        SummaryDelta = "response.reasoning_summary_text.delta",
        SummaryDone = "response.reasoning_summary_text.done",
        ArgumentsDelta = "response.function_call_arguments.delta",
        ArgumentsDone = "response.function_call_arguments.done",
        PartAdded = "response.content_part.added",
        PartDone = "response.content_part.done",
        ReasoningPartAdded = "response.reasoning_part.added",
        ReasoningPartDone = "response.reasoning_part.done",
        SummaryPartAdded = "response.reasoning_summary_part.added",
        SummaryPartDone = "response.reasoning_summary_part.done",
        Completed = "response.completed",
        Incomplete = "response.incomplete",
        Failed = "response.failed",
        Error = "error" | "response.error",
        Aborted = "response.aborted" | "response.cancelled" | "response.canceled"
            | "response.interrupted",
    }
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

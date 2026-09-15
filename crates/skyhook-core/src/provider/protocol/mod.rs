//! Provider-neutral request, response, and conversation protocol.

mod message;
mod request;
mod response;

pub use message::{
    AssistantBlock, AssistantContent, AssistantItem, BlockContent, BlockKind, ItemKind, Message,
    ReplayEnvelope, ToolCall, ToolCallError, ToolResult, UserContent,
};
pub use request::{HistoryLifetime, ModelRequest, ResponseSchema, SystemSegment, ToolDefinition};
#[cfg(test)]
pub use response::events_for_content;
pub use response::{
    BlockSnapshot, ContentDelta, ItemSnapshot, ResponseAssembler, ResponseChunk, ResponseEvent,
    ResponseSnapshot, StopReason, Usage,
};

/// Opaque replay fixture shared by protocol tests.
#[cfg(test)]
pub(crate) fn replay() -> ReplayEnvelope {
    ReplayEnvelope {
        version: 1,
        protocol: "responses".into(),
        model: "m".into(),
        scope: "s".into(),
        payload: serde_json::json!({"opaque":{"nested":[null, 42]}}),
    }
}

//! Provider-neutral request, response, and conversation protocol.

mod message;
mod request;
mod response;

pub use message::{
    AssistantBlock, AssistantContent, AssistantItem, BlockContent, BlockKind, ItemKind, Message,
    ReplayEnvelope, ToolCall, ToolResult, UserContent,
};
pub use request::{ModelRequest, ResponseSchema, SystemSegment, ToolDefinition};
#[cfg(test)]
pub use response::events_for_content;
pub use response::{
    BlockSnapshot, ContentDelta, ItemSnapshot, ResponseAssembler, ResponseChunk, ResponseEvent,
    ResponseSnapshot, StopReason, Usage,
};

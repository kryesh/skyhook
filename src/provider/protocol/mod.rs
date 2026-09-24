//! Provider-neutral request, response, and conversation protocol.

mod message;
mod request;
mod response;

pub use message::{
    AssistantItem, Binding, BlockId, ItemId, ItemKind, Message, Position, PositionOverflow,
    Provenance, Replay, Scope, TextBlock, ToolCall, ToolCallError, ToolResult, UserContent,
    visible_text,
};
pub use request::{
    ContextId, HistoryLifetime, ModelRequest, ResponseSchema, SystemSegment, ToolDefinition,
};
pub use response::{
    BlockRef, Completion, CompletionError, CutReason, LiveBlock, LiveResponse, Outcome,
    ResponseEvent, Step, Usage,
};

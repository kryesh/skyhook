//! Provider-neutral request, response, and conversation protocol.

mod context;
mod message;
mod request;
mod response;

pub use context::{Entry, State, StateLifetime, sent};
#[cfg(test)]
pub(crate) use context::{entries, state};
pub use message::{
    AssistantItem, Binding, BlockId, ItemId, ItemKind, Message, Position, PositionOverflow,
    Provenance, Replay, ReplayFormat, Scope, TextBlock, ToolCall, ToolCallError, ToolResult,
    UserContent, visible_text,
};
pub use request::{
    ContextId, HistoryLifetime, InvalidResponseSchema, ModelRequest, ResponseSchema, SystemSegment,
    ToolDefinition, WireModel,
};
pub use response::{
    BlockRef, Completion, CompletionError, CutReason, LiveBlock, LiveResponse, Outcome,
    ResponseEvent, Step, Usage, UsageError, UsageTotals,
};

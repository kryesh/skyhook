//! Provider-neutral request, response, and conversation protocol.

mod message;
mod request;
mod response;

pub use message::{AssistantContent, Message, ToolCall, ToolResult, UserContent};
pub use request::{ModelRequest, SystemSegment, ToolDefinition};
pub use response::{ResponseChunk, StopReason, Usage};

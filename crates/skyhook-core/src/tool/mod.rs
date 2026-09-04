//! Tool registration, authorization, execution, and built-in capabilities.

pub(crate) mod authorization;
mod context;
pub mod executor;
mod registry;

pub mod builtins;
pub mod javascript;
pub mod policy;

pub use context::{ProgressFuture, ProgressSink, ToolContext, ToolError, ToolOutput};
pub(crate) use registry::PathKind;
pub use registry::{
    RegisteredTool, RegistryError, ScriptBinding, ToolDefinition, ToolExecution, ToolExposure,
    ToolOptions, ToolPlacement, ToolRegistry, ToolRegistryBuilder, ToolSpec, ToolSurface,
};

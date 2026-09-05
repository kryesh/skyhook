//! Tool registration, authorization, execution, and built-in capabilities.

pub(crate) mod authorization;
mod context;
pub mod executor;
mod registry;

pub mod builtins;
pub mod javascript;
pub mod policy;

pub use context::{
    Denial, DenialCode, ProgressFuture, ProgressSink, ToolContext, ToolError, ToolOutput,
};
pub(crate) use registry::{PathKind, job_envelope_type};
pub use registry::{
    RegisteredTool, RegistryError, ScriptBinding, ToolDefinition, ToolExecution, ToolExposure,
    ToolOptions, ToolPlacement, ToolRegistry, ToolRegistryBuilder, ToolSpec, ToolSurface,
};

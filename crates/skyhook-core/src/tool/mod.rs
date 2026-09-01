//! Tool registration, authorization, execution, and built-in capabilities.

mod context;
pub mod executor;
mod registry;

pub mod builtins;
pub mod javascript;
pub mod policy;

pub use context::{ProgressFuture, ProgressSink, ToolContext, ToolError, ToolOutput};
pub(crate) use registry::PathKind;
pub use registry::{
    RegisteredTool, RegistryError, ScriptBinding, ToolExposure, ToolOptions, ToolRegistry,
    ToolRegistryBuilder, ToolVisibilityContext,
};

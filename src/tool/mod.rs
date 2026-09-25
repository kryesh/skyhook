//! Tool registration, authorization, execution, and built-in capabilities.

pub(crate) mod authorization;
mod coerce;
mod context;
pub mod diagnostic;
pub(crate) use context::StreamEnd;
pub mod executor;
pub(crate) mod invocation;
pub(crate) mod output;
pub(crate) mod registry;
pub(crate) mod source;

pub mod builtins;
pub mod javascript;
pub mod policy;

pub use context::{DenialCode, ToolContext, ToolError, ToolOutput};
pub use invocation::AdmissionError;
pub use registry::{
    PathArgument, PathKind, RegisteredTool, RegistryError, ScriptBinding, ToolExposure,
    ToolOptions, ToolPlacement, ToolRegistry, ToolRegistryBuilder, ToolSpec, ToolSurface,
};
pub(crate) use registry::{ToolResultPolicy, job_view_type};

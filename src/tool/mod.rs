//! Tool registration, authorization, execution, and built-in capabilities.

pub(crate) mod authorization;
mod coerce;
mod context;
pub mod diagnostic;
pub(crate) use context::StreamEnd;
pub mod executor;
pub(crate) mod invocation;
pub(crate) mod output;
pub(crate) mod path;
pub(crate) mod registry;
pub(crate) mod source;

pub mod builtins;
pub mod javascript;
pub mod policy;

pub use context::{DenialCode, ToolContext, ToolError, ToolOutput};
pub use invocation::AdmissionError;
pub(crate) use registry::{JobLocation, ToolResultPolicy, job_view_type};
pub use registry::{
    JobViewResult, PathArgument, PathKind, RegisteredTool, RegistryError, ResultSchema,
    ScriptBinding, TARGET, ToolExposure, ToolOptions, ToolPlacement, ToolRegistry,
    ToolRegistryBuilder, ToolSpec, ToolSurface,
};

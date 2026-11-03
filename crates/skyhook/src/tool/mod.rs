//! Tool registration, authorization, execution, and capabilities.

pub mod arguments;
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
pub(crate) mod javascript;
pub mod policy;

pub use crate::job::output::REGEX_SIZE_LIMIT;
pub use builtins::search::excerpt;
pub use context::{DenialCode, ToolContext, ToolError, ToolOutput};
pub use invocation::AdmissionError;
pub use output::complete;
pub(crate) use registry::{JobLocation, ToolResultPolicy, job_view_type};
pub use registry::{
    PathArgument, PathKind, RegisteredTool, RegistryError, ScriptBinding, TARGET, ToolExposure,
    ToolOptions, ToolPlacement, ToolRegistry, ToolRegistryBuilder, ToolSpec, ToolSurface,
    input_schema_for, result_schema,
};

//! Sandboxed JavaScript orchestration over the shared tool registry.

mod bridge;
mod console;
mod outcome;
mod result;
mod runtime;

pub(crate) use result::ScriptResult;
pub(crate) use runtime::evaluate_captured;

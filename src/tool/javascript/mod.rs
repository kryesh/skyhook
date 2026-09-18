//! Sandboxed JavaScript orchestration over the shared tool registry.

mod bridge;
mod console;
mod outcome;
mod result;
mod runtime;

pub use runtime::JsError;

pub(crate) use result::{ScriptResult, script_output};
pub(crate) use runtime::{CapturedJsError, evaluate_captured};

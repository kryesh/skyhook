//! Sandboxed JavaScript orchestration over the shared tool registry.

mod console;
mod runtime;

pub use runtime::{JsError, evaluate};

pub(crate) use runtime::evaluate_captured;

//! Conversion from engine and script failures to the tool result contract.

use serde_json::Value;
use thiserror::Error;

use crate::tool::{
    ToolError,
    diagnostic::{Effects, Operation, Subject},
};

use super::{
    bridge::{Failure, Returned},
    source::MAX_SOURCE_BYTES,
};

#[derive(Debug, Error)]
pub(super) enum JsError {
    #[error("JavaScript source exceeds the {MAX_SOURCE_BYTES}-byte limit")]
    SourceTooLarge,
    #[error("{0}")]
    Syntax(String),
    #[error("QuickJS initialization failed: {0}")]
    Initialization(String),
    #[error("QuickJS execution failed: {0}")]
    Execution(String),
    #[error("QuickJS execution was cancelled")]
    Cancelled,
    /// With what the script returned, when only its conversion failed.
    #[error("{}", failure.message)]
    Failure {
        failure: Failure,
        returned: Box<Returned>,
    },
    #[error("JavaScript returned invalid JSON: {0}")]
    InvalidOutput(String),
}

pub(super) fn javascript_error(error: JsError) -> ToolError {
    let (operation, subject, effects) = match &error {
        JsError::SourceTooLarge | JsError::Syntax(_) => (
            Operation::Validate,
            "JavaScript source",
            Effects::NotStarted,
        ),
        JsError::Initialization(_) => (
            Operation::Prepare,
            "JavaScript runtime",
            Effects::NotStarted,
        ),
        JsError::Execution(_) | JsError::Cancelled => {
            (Operation::Execute, "JavaScript source", Effects::Unknown)
        }
        JsError::Failure { .. } => (Operation::Execute, "JavaScript source", Effects::Unknown),
        JsError::InvalidOutput(_) => (
            Operation::Deserialize,
            "JavaScript result",
            Effects::OutputIncomplete,
        ),
    };
    let error = match error {
        JsError::Cancelled => ToolError::cancelled(),
        JsError::Failure { failure, returned } => ToolError::with_output(
            failure.message,
            super::result::script_output(returned.value, failure.details, None)
                .with_presented(returned.presented.into_iter().collect()),
        ),
        error => ToolError::with_output(
            error.to_string(),
            super::result::script_output(Value::Null, None, None),
        ),
    };
    error
        .operation(operation, Subject::Label(subject.into()))
        .effects(effects)
}

pub(super) fn bridge_error(error: &impl ToString) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host request", "JavaScript promise", error.to_string())
}

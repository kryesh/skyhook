//! The private envelopes of the JavaScript host boundary: the runtime's
//! configuration, host requests and responses, and the script's final outcome.
//! Tool payloads remain arbitrary JSON.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use std::collections::BTreeMap;

use crate::job::JobState;
use crate::tool::catalog::{ScriptBinding, ToolSurface};
use skyhook_tools::output::{FieldPointer, FieldPresentation};

#[derive(Serialize)]
pub(super) struct Config {
    pub(super) builders: Vec<String>,
    /// The states a job ends in, which a call's view unwraps to its result or error.
    pub(super) terminal: Vec<JobState>,
    /// The module name of the submitted body's frames.
    pub(super) filename: &'static str,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum HostRequest {
    Call {
        name: String,
        arguments: Map<String, Value>,
    },
    Receive,
}

/// Declared presentations of a value's fields; the script's own are rooted at
/// `/result/value`.
pub(super) type Presented = BTreeMap<FieldPointer, FieldPresentation>;

/// Only a receive fails: tool failures travel in `Ok.value` as public JobViews.
#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum HostResponse {
    Ok { value: Value, presented: Presented },
    Failed { error: String },
}

/// The script's outcome: what it returned, null when it threw, and why it failed
/// if it did. A returned value that does not convert keeps what did.
#[derive(Deserialize)]
pub(super) struct Finished {
    #[serde(flatten)]
    pub(super) returned: Returned,
    pub(super) failure: Option<Failure>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct Returned {
    pub(super) value: Value,
    pub(super) presented: Presented,
}

/// An error's text with its other properties as details, or another thrown
/// value's JSON.
#[derive(Debug, Deserialize)]
pub(super) struct Failure {
    pub(super) message: String,
    pub(super) details: Option<Map<String, Value>>,
}

/// The `tool.<name>` calls a script on `surface` may make.
pub(super) fn script_calls(surface: &ToolSurface) -> Vec<String> {
    (surface.specs())
        .filter(|tool| tool.script_binding != ScriptBinding::Unavailable)
        .map(|tool| tool.name.clone())
        .collect()
}

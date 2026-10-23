//! The private envelopes of the JavaScript host boundary: the runtime's
//! configuration, host requests and responses, and the script's final outcome.
//! Tool payloads remain arbitrary JSON.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use std::collections::BTreeMap;

use super::source::Layout;
use crate::{job::FieldPointer, tool::output::FieldPresentation, tool::registry::ScriptManifest};

#[derive(Serialize)]
pub(super) struct Config<'a> {
    pub(super) builders: Vec<ScriptManifest>,
    pub(super) source: &'a Layout,
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

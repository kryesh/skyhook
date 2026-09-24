//! Decoding of the private JavaScript wrapper envelope produced by `runtime.js`.

use serde::Deserialize;
use serde_json::Value;

use crate::job::output::ScriptPresentation;

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum Envelope {
    Ok {
        value: Value,
        presentation: ScriptPresentation,
    },
    /// What the script threw, as `__describeError` spells it: any JSON value.
    Failed { error: Value },
}

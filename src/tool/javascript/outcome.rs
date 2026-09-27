//! Decoding of the private JavaScript wrapper envelope produced by `runtime.js`.

use serde::Deserialize;
use serde_json::Value;

use std::collections::BTreeSet;

use crate::job::FieldPointer;

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum Envelope {
    Ok {
        value: Value,
        /// Complete fields of the returned value, rooted at `/result/value`.
        complete: BTreeSet<FieldPointer>,
    },
    /// What the script threw, as `__describeError` spells it: any JSON value.
    Failed { error: Value },
}

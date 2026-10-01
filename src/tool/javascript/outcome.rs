//! Decoding of the private JavaScript wrapper envelope produced by `runtime.js`.

use serde::Deserialize;
use serde_json::Value;

use std::collections::BTreeMap;

use crate::{job::FieldPointer, tool::output::FieldPresentation};

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum Envelope {
    Ok {
        value: Value,
        /// Declared presentations of the returned value's fields, rooted at
        /// `/result/value`.
        presented: BTreeMap<FieldPointer, FieldPresentation>,
    },
    /// What the script threw, as `__describeError` spells it: any JSON value.
    Failed { error: Value },
}

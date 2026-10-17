//! The private envelope of the JavaScript host boundary, in both directions:
//! host responses to `__skyhookHostCall` and the wrapper's final outcome. Tool
//! payloads remain arbitrary JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::BTreeMap;

use crate::{job::FieldPointer, tool::output::FieldPresentation};

#[derive(Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(super) enum Outcome<E> {
    Ok {
        value: Value,
        /// Declared presentations of the value's fields; the wrapper's are rooted
        /// at `/result/value`.
        presented: BTreeMap<FieldPointer, FieldPresentation>,
    },
    /// A host response fails only on receive, with a message: tool failures
    /// travel in `Ok.value` as public JobViews. The wrapper fails with what the
    /// script threw, as `__describeError` spells it: any JSON value.
    Failed { error: E },
}

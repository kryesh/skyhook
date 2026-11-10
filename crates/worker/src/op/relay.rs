//! The origin role: connect from this target to a further one and relay the stream's
//! standard input and output.
use serde::{Deserialize, Serialize};

use super::Input;
use crate::origin::Leg;

/// Open `leg` and run `command` at its destination; `input` is its standard input,
/// and its standard output comes back as events.
#[derive(Debug, Deserialize, Serialize)]
pub struct Open {
    pub leg: Leg,
    pub command: String,
    #[serde(with = "super::input")]
    pub input: Input,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct StreamOutput(#[serde(with = "serde_bytes")] pub Vec<u8>);

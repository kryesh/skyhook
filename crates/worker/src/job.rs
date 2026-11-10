//! The identifier of a job a tool call runs as.

use std::{fmt, num::NonZeroU64};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Clone, Copy, Debug, Hash, Deserialize, JsonSchema, Serialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(transparent)]
pub struct JobId(#[schemars(with = "u64", range(min = 1))] NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("job identifiers must be non-zero")]
pub struct ZeroJobId;

impl JobId {
    pub fn new(value: u64) -> Result<Self, ZeroJobId> {
        NonZeroU64::new(value).map(Self).ok_or(ZeroJobId)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

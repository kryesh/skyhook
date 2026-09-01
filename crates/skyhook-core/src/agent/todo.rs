//! Agent-owned todo state exposed to tools and persisted in session events.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

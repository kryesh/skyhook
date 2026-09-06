use serde::{Deserialize, Serialize};

use super::AssistantContent;

#[derive(Clone, Debug, PartialEq)]
pub enum ResponseChunk {
    TextDelta { text: String },
    ReasoningDelta { text: String },
    Block { block: AssistantContent },
    Usage { usage: Usage },
    Finished { truncated: bool },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub(crate) fn accumulate(&mut self, usage: Self) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
    }
}

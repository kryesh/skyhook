//! Workspace filesystem tools.

mod mutations;
mod read;

pub use mutations::{RemoveEntry, ReplaceText, WriteFile};
pub use read::Read;

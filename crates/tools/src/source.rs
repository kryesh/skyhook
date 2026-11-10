//! Source files: a path on any target whose contents a tool consumes. The executor
//! opens it where it lives before the call runs; its bytes then stream to where
//! the call writes them, or never leave a target that holds both.
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::CallError;
use skyhook_worker::op::{Content, Input, TargetPath};

/// An opened source.
#[derive(Clone, Debug)]
pub enum Source {
    /// Its bytes, read once from wherever they live.
    Stream(Arc<Mutex<Option<Input>>>),
    /// A regular file on the target the consuming call runs on.
    Copy(TargetPath),
}

impl Source {
    /// A source whose bytes `input` streams.
    pub fn stream(input: Input) -> Self {
        Self::Stream(Arc::new(Mutex::new(Some(input))))
    }

    /// What a write of the source's bytes sends its target.
    pub(crate) fn content(&self) -> Result<Content, CallError> {
        match self {
            Self::Copy(path) => Ok(Content::Copy(path.clone())),
            Self::Stream(input) => {
                let input = input.lock().unwrap_or_else(PoisonError::into_inner).take();
                input
                    .map(Content::Streamed)
                    .ok_or_else(|| CallError::failed("source was already read"))
            }
        }
    }
}

//! Streamed or offloaded bytes at one output pointer, including those left behind by
//! unfinished tools.
//!
//! Each pointer is one `job_capture` row in the job's current generation, reserved
//! atomically, so concurrent producers cannot take over another capture.
use super::*;

mod stream;
pub(crate) use stream::{
    CaptureCollector, CaptureWriter, CompletedCapture, HostOutput, PendingCapture, TextCaptureField,
};

pub use crate::tool::output::CaptureKind;

impl CaptureKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn parse(kind: &str) -> Self {
        match kind {
            "text" => Self::Text,
            "json" => Self::Json,
            _ => Self::Unknown,
        }
    }
}

use crate::tool::output::validate_field as validate_capture_field;

/// Descriptors describe raw captures, not a replacement structured result. In
/// particular, incomplete JSON is only safe to read through explicit byte paging.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct CaptureDescriptor {
    pub(crate) field: String,
    pub(crate) kind: CaptureKind,
    pub(crate) complete: bool,
    pub(crate) output: Option<Box<crate::job::JobView>>,
}

pub(crate) fn available_captures(saved: &Saved, terminal: bool) -> Vec<CaptureDescriptor> {
    // Merely having a value at a registered pointer does not mean that value
    // references the capture (for example, a producer may abandon it and return
    // null). Only the saved document's field references establish completion.
    let complete = terminal
        && saved
            .document
            .as_ref()
            .is_some_and(|document| document["capture_complete"].as_bool() == Some(true));
    saved
        .captures
        .values()
        .map(|capture| CaptureDescriptor {
            output: None,
            field: capture.pointer.clone(),
            kind: CaptureKind::parse(&capture.kind),
            complete: complete && capture.referenced,
        })
        .collect()
}

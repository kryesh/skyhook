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

/// Descriptors describe raw captures, not a replacement structured result. In
/// particular, incomplete JSON is only safe to read through explicit byte paging.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct CaptureDescriptor {
    pub(crate) field: FieldPointer,
    pub(crate) complete: bool,
    #[schemars(with = "crate::job::JobView")]
    pub(crate) output: Option<Box<crate::job::JobView>>,
}

impl CaptureDescriptor {
    pub fn field(&self) -> &FieldPointer {
        &self.field
    }

    /// The capture's page, when the host hydrated one.
    pub fn output(&self) -> Option<&crate::job::JobView> {
        self.output.as_deref()
    }
}

pub(crate) fn available_captures(saved: &Saved, terminal: bool) -> Vec<CaptureDescriptor> {
    // Merely having a value at a registered pointer does not mean that value
    // references the capture (for example, a producer may abandon it and return
    // null). Only the saved document's field references establish completion.
    let complete = terminal
        && saved
            .product
            .as_ref()
            .is_some_and(|product| product.captures_complete);
    saved
        .captures
        .values()
        .map(|capture| CaptureDescriptor {
            output: None,
            field: capture.pointer.clone(),
            complete: complete && capture.referenced,
        })
        .collect()
}

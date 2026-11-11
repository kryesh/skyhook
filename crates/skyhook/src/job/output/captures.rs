//! Streamed or offloaded bytes at one output pointer, including those left behind by
//! unfinished tools.
//!
//! Each pointer is one `job_capture` row in the job's current generation, reserved
//! atomically, so concurrent producers cannot take over another capture.
use super::*;

mod stream;
#[cfg(test)]
pub(crate) use stream::create_capture;
pub(crate) use stream::{CompletedCapture, HostOutput};

use skyhook_tools::output::CaptureKind;
use skyhook_tools::target::{ExecutionLocation, TargetRef};

/// Descriptors describe raw captures, not a replacement structured result. In
/// particular, incomplete JSON is only safe to read through explicit byte paging
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct CaptureDescriptor {
    pub field: FieldPointer,
    pub(crate) complete: bool,
    /// The capture's page, when the host hydrated one
    pub output: Option<Box<crate::job::JobView>>,
}

/// A file that holds a field's bytes in place of the session's database, as host
/// inspection shows it to the session's user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldFile {
    pub field: FieldPointer,
    /// The machine holding the file: the session host for what the session moved
    /// there, the job's target for what it wrote or was asked to read.
    pub target: TargetRef,
    pub path: skyhook_worker::op::TargetPath,
    /// The bytes the file held of the field when it was saved.
    pub bytes: u64,
    /// Why the session host can no longer read the file it kept.
    pub unreadable: Option<String>,
}

/// The files holding the fields of a job that ran at `location`.
pub(super) fn field_files(saved: &Saved, location: &ExecutionLocation) -> Vec<FieldFile> {
    use crate::session::{CaptureBytes, TargetFile};
    let held = |capture: &CaptureRow| {
        let (target, path, bytes) = match &capture.bytes {
            CaptureBytes::Rows => return None,
            CaptureBytes::Spilled { path, extent } => (
                TargetRef::Root,
                path.to_string_lossy().into_owned().into(),
                extent.bytes,
            ),
            CaptureBytes::Target(TargetFile { file, .. }) => (
                location.target.clone(),
                file.path().clone(),
                file.extent().bytes,
            ),
        };
        Some(FieldFile {
            field: capture.pointer.clone(),
            target,
            path,
            bytes,
            unreadable: (saved.unreadable.get(&capture.pointer)).map(ToString::to_string),
        })
    };
    saved.captures.values().filter_map(held).collect()
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
    // A finished job's capture that took nothing and was never referenced leaves
    // nothing to read.
    let unused = |capture: &&CaptureRow| terminal && capture.empty && !capture.referenced;
    saved
        .captures
        .values()
        .filter(|capture| !unused(capture))
        .map(|capture| CaptureDescriptor {
            output: None,
            field: capture.pointer.clone(),
            // A field whose saved bytes are gone is no complete one: it shows empty,
            // and reading it says so.
            complete: complete
                && capture.referenced
                && !saved.unreadable.contains_key(&capture.pointer),
        })
        .collect()
}

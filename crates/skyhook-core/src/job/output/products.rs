//! Canonical output query and presentation products. Wire JSON is an adapter,
//! not an admission path for a presentation or its associated attachments.
use super::{OutputArgs, Value};
use crate::{job::JobState, media::ImageRef};
use serde::Serialize;

/// Attachment intent depends on presence, never on equality to default values:
/// any explicit field/page selector, including explicitly supplied defaults,
/// yields a text page without images.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSelection {
    /// The whole result with its images: no selector at all.
    WholeWithImages,
    /// The whole result without images: only `context` was given.
    Whole,
    Explicit,
}

impl OutputArgs {
    pub fn selection(&self) -> OutputSelection {
        if self.field.is_some()
            || self.start.is_some()
            || self.limit.is_some()
            || self.pattern.is_some()
            || self.offset.is_some()
        {
            OutputSelection::Explicit
        } else if self.context.is_some() {
            OutputSelection::Whole
        } else {
            OutputSelection::WholeWithImages
        }
    }
}

/// A source page and continuation constructed by the saved-output reader.
/// A continuation always owns its line and optional byte offset together.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OutputPreview {
    pub(crate) field: String,
    pub(crate) lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_lines: Option<usize>,
    #[serde(flatten)]
    pub(crate) next: Option<OutputContinuation>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OutputContinuation {
    #[serde(rename = "next_start")]
    pub(crate) start: usize,
    #[serde(rename = "next_offset", skip_serializing_if = "is_zero")]
    pub(crate) offset: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// A truncated structured field and its source continuation.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct OutputTruncation {
    pub(crate) field: String,
    pub(crate) total_lines: usize,
    #[serde(flatten)]
    pub(crate) next: OutputContinuation,
}

/// A manager-produced projection, with attachments from the same job snapshot
/// as its state and presentation metadata. Saved files remain append-only/live
/// reads; this product does not claim an atomic filesystem snapshot.
#[derive(Clone, Debug)]
pub struct PresentedOutput {
    pub state: JobState,
    pub(crate) view: Value,
    pub images: Vec<ImageRef>,
    /// Captures absent from a whole presentation, as `(captures index, field)`
    /// in the descriptor's stable order. Hydration is opt-in through
    /// `inspect_output_with_captures`.
    pub(super) capture_targets: Vec<(usize, String)>,
}

impl PresentedOutput {
    /// Only the output owner constructs targets, in the descriptor's original
    /// order. Concurrent completion never changes that stable order.
    pub(super) fn attach_capture(
        &mut self,
        index: usize,
        page: Result<PresentedOutput, crate::tool::ToolError>,
    ) {
        self.view["captures"][index]["output"] = match page {
            Ok(page) => page.view,
            Err(error) => serde_json::json!({"error": error.to_string()}),
        };
    }
    pub fn view(&self) -> &Value {
        &self.view
    }
    pub fn into_parts(self) -> (JobState, Value, Vec<ImageRef>) {
        (self.state, self.view, self.images)
    }
    pub fn into_view(self) -> Value {
        self.view
    }
}

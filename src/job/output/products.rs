//! Canonical output query and presentation products. Wire JSON is an adapter,
//! not an admission path for a presentation or its associated attachments.
use super::{OutputArgs, Value};
use crate::{job::JobState, media::ImageRef};
use schemars::JsonSchema;
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

/// A source page with explicit continuation defaults at end of output.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct OutputPreview {
    pub(crate) field: String,
    pub(crate) lines: Vec<String>,
    pub(crate) total_lines: Option<usize>,
    #[schemars(range(min = 1))]
    pub(crate) next_start: Option<usize>,
    pub(crate) next_offset: usize,
}

/// A truncated structured field and its source continuation.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
pub(crate) struct OutputTruncation {
    pub(crate) field: String,
    pub(crate) total_lines: usize,
    #[schemars(range(min = 1))]
    pub(crate) next_start: usize,
    pub(crate) next_offset: usize,
}

/// A manager-produced projection, with attachments from the same job snapshot
/// as its state and presentation metadata. Captures remain append-only live
/// reads; this product does not claim an atomic snapshot of them.
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
        self.view["presentation"]["captures"][index]["output"] = match page {
            Ok(page) => page.view,
            Err(error) => crate::job::JobView::failure(
                error.to_string(),
                None,
                false,
                crate::job::JobMetadata::default(),
            )
            .into_value(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pagination_fields_are_stable_at_end_and_at_zero_offset() {
        let end = serde_json::to_value(OutputPreview {
            field: "/result".into(),
            lines: vec!["done".into()],
            total_lines: None,
            next_start: None,
            next_offset: 0,
        })
        .unwrap();
        assert_eq!(
            end,
            json!({
                "field":"/result", "lines":["done"], "total_lines":null,
                "next_start":null, "next_offset":0
            })
        );

        let truncated = serde_json::to_value(OutputTruncation {
            field: "/result/log".into(),
            total_lines: 101,
            next_start: 101,
            next_offset: 0,
        })
        .unwrap();
        assert_eq!(truncated["next_offset"], 0);
    }
}

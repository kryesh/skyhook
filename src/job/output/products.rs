//! Canonical output query and presentation products. Wire JSON is an adapter,
//! not an admission path for a presentation or its associated attachments.
use super::{FieldPointer, Value};
use crate::{
    job::{JobState, JobView},
    media::ImageRef,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a query selects, by which selectors are present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputSelection {
    /// The whole result with its images: no selector at all.
    WholeWithImages,
    /// The whole result without images: only `context` was given.
    Whole,
    Explicit,
}

/// A source page. At end of output `next_start` is absent; a model page omits
/// `field` when it is the field that was requested.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OutputPreview {
    #[schemars(with = "FieldPointer")]
    pub(crate) field: Option<FieldPointer>,
    pub(crate) lines: PageLines,
    #[schemars(with = "usize")]
    pub(crate) total_lines: Option<usize>,
    #[schemars(with = "usize", range(min = 1))]
    pub(crate) next_start: Option<usize>,
    /// Bytes into the `next_start` line, when a long line was cut.
    #[schemars(with = "usize", range(min = 1))]
    pub(crate) next_offset: Option<usize>,
}

/// A page's source lines: consecutive for a plain read, or each with its
/// one-based line number for a pattern search.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PageLines {
    Text(Vec<String>),
    Numbered(Vec<NumberedLine>),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct NumberedLine {
    pub(crate) line: usize,
    pub(crate) text: String,
}

impl OutputPreview {
    /// None when the page is of the field that was requested.
    pub fn field(&self) -> Option<&FieldPointer> {
        self.field.as_ref()
    }

    /// The page's text, without search line numbers.
    pub fn lines(&self) -> Vec<&str> {
        match &self.lines {
            PageLines::Text(lines) => lines.iter().map(String::as_str).collect(),
            PageLines::Numbered(lines) => lines.iter().map(|line| line.text.as_str()).collect(),
        }
    }

    /// The next page's `(field, start line, byte offset)`; none at end of output.
    pub fn continuation(&self) -> Option<(Option<&FieldPointer>, usize, Option<usize>)> {
        Some((self.field.as_ref(), self.next_start?, self.next_offset))
    }
}

/// A truncated structured field and its source continuation.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OutputTruncation {
    pub(crate) field: FieldPointer,
    pub(crate) total_lines: usize,
    #[schemars(range(min = 1))]
    pub(crate) next_start: usize,
    /// Bytes into the `next_start` line, when a long line was cut.
    #[schemars(with = "usize", range(min = 1))]
    pub(crate) next_offset: Option<usize>,
}

/// A manager-produced projection, with attachments from the same job snapshot
/// as its state and presentation metadata. Captures remain append-only live
/// reads; this product does not claim an atomic snapshot of them.
#[derive(Clone, Debug)]
pub struct PresentedOutput {
    pub state: JobState,
    pub(crate) view: JobView,
    pub images: Vec<ImageRef>,
    /// Captures absent from a whole presentation, as `(captures index, field)`
    /// in the descriptor's stable order. Hydration is opt-in through
    /// `inspect_output_with_captures`.
    pub(super) capture_targets: Vec<(usize, FieldPointer)>,
}

impl PresentedOutput {
    /// Only the output owner constructs targets, in the descriptor's original
    /// order. Concurrent completion never changes that stable order.
    pub(super) fn attach_capture(
        &mut self,
        index: usize,
        page: Result<PresentedOutput, crate::tool::ToolError>,
    ) {
        let Some(capture) = self
            .view
            .presentation
            .as_mut()
            .and_then(|presentation| presentation.captures.as_mut()?.get_mut(index))
        else {
            return;
        };
        capture.output = Some(Box::new(match page {
            Ok(page) => page.view,
            Err(error) => JobView::failure(error.to_string(), None, false),
        }));
    }
    pub fn into_parts(self) -> (JobState, Value, Vec<ImageRef>) {
        (self.state, self.view.into_value(), self.images)
    }
    pub fn into_view(self) -> Value {
        self.view.into_value()
    }
    pub fn into_job_view(self) -> JobView {
        self.view
    }
}

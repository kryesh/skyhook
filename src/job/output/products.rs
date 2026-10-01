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

/// A page of a saved field: text lines, or a JSON container's elements or
/// members. A model page omits `field` when it is the field that was requested.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum OutputPreview {
    Lines(LinePage),
    Elements(ElementPage),
    Members(MemberPage),
    Matches(MatchPage),
}

/// Consecutive source lines. At end of output `next_start` is absent.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct LinePage {
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

/// Saved-output pointers one level below a field, from some position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputFields {
    pub fields: Vec<FieldPointer>,
    /// The index of the first child after those listed, when there is one.
    pub next_index: Option<usize>,
}

/// Whole consecutive array elements; one too large for a page is sampled, with
/// its shape and cuts at their source pointers. `next_index` is absent at the end.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ElementPage {
    #[schemars(with = "FieldPointer")]
    pub(crate) field: Option<FieldPointer>,
    pub(crate) elements: Vec<Value>,
    pub(crate) total_elements: usize,
    #[schemars(with = "usize")]
    pub(crate) next_index: Option<usize>,
    pub(crate) shape: Option<Value>,
    #[schemars(with = "Vec<OutputTruncation>")]
    pub(crate) truncated: Option<Vec<OutputTruncation>>,
}

/// Whole consecutive object members in source order, like an `ElementPage`.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MemberPage {
    #[schemars(with = "FieldPointer")]
    pub(crate) field: Option<FieldPointer>,
    pub(crate) members: serde_json::Map<String, Value>,
    pub(crate) total_members: usize,
    #[schemars(with = "usize")]
    pub(crate) next_index: Option<usize>,
    pub(crate) shape: Option<Value>,
    #[schemars(with = "Vec<OutputTruncation>")]
    pub(crate) truncated: Option<Vec<OutputTruncation>>,
}

/// A JSONPath query's matches in document order, duplicates included, paged
/// like an `ElementPage`. Each `at` is the match's pointer in the saved output.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MatchPage {
    #[schemars(with = "FieldPointer")]
    pub(crate) field: Option<FieldPointer>,
    pub(crate) matches: Vec<Match>,
    pub(crate) total_matches: usize,
    #[schemars(with = "usize")]
    pub(crate) next_index: Option<usize>,
    pub(crate) shape: Option<Value>,
    #[schemars(with = "Vec<OutputTruncation>")]
    pub(crate) truncated: Option<Vec<OutputTruncation>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct Match {
    pub(crate) at: FieldPointer,
    pub(crate) value: Value,
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

/// Where a field's next page starts, in its own unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuation<'a> {
    Lines {
        field: Option<&'a FieldPointer>,
        start: usize,
        offset: Option<usize>,
    },
    Index {
        field: Option<&'a FieldPointer>,
        index: usize,
    },
}

impl Continuation<'_> {
    /// None when the page is of the field that was requested.
    pub fn field(&self) -> Option<&FieldPointer> {
        match self {
            Self::Lines { field, .. } | Self::Index { field, .. } => *field,
        }
    }
}

impl OutputPreview {
    /// None when the page is of the field that was requested.
    pub fn field(&self) -> Option<&FieldPointer> {
        match self {
            Self::Lines(page) => page.field.as_ref(),
            Self::Elements(page) => page.field.as_ref(),
            Self::Members(page) => page.field.as_ref(),
            Self::Matches(page) => page.field.as_ref(),
        }
    }

    pub(crate) fn clear_field(&mut self) {
        match self {
            Self::Lines(page) => page.field = None,
            Self::Elements(page) => page.field = None,
            Self::Members(page) => page.field = None,
            Self::Matches(page) => page.field = None,
        }
    }

    /// The shape of a value the page shows sampled.
    pub fn shape(&self) -> Option<&Value> {
        match self {
            Self::Lines(_) => None,
            Self::Elements(page) => page.shape.as_ref(),
            Self::Members(page) => page.shape.as_ref(),
            Self::Matches(page) => page.shape.as_ref(),
        }
    }

    /// What the page left out of the values it shows.
    pub fn truncated(&self) -> &[OutputTruncation] {
        let truncated = match self {
            Self::Lines(_) => None,
            Self::Elements(page) => page.truncated.as_deref(),
            Self::Members(page) => page.truncated.as_deref(),
            Self::Matches(page) => page.truncated.as_deref(),
        };
        truncated.unwrap_or_default()
    }

    /// The next page's position; none at end of output.
    pub fn continuation(&self) -> Option<Continuation<'_>> {
        let field = self.field();
        match self {
            Self::Lines(page) => Some(Continuation::Lines {
                field,
                start: page.next_start?,
                offset: page.next_offset,
            }),
            Self::Elements(ElementPage { next_index, .. })
            | Self::Members(MemberPage { next_index, .. })
            | Self::Matches(MatchPage { next_index, .. }) => Some(Continuation::Index {
                field,
                index: (*next_index)?,
            }),
        }
    }
}

impl LinePage {
    /// The field's lines so far; none when its output is unavailable.
    pub fn total_lines(&self) -> Option<usize> {
        self.total_lines
    }

    /// The page's text, without search line numbers.
    pub fn lines(&self) -> Vec<&str> {
        match &self.lines {
            PageLines::Text(lines) => lines.iter().map(String::as_str).collect(),
            PageLines::Numbered(lines) => lines.iter().map(|line| line.text.as_str()).collect(),
        }
    }
}

impl ElementPage {
    pub fn elements(&self) -> &[Value] {
        &self.elements
    }
}

impl MemberPage {
    pub fn members(&self) -> &serde_json::Map<String, Value> {
        &self.members
    }
}

impl MatchPage {
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }
}

impl Match {
    pub fn at(&self) -> &FieldPointer {
        &self.at
    }

    pub fn value(&self) -> &Value {
        &self.value
    }
}

/// What an automatic preview left out of one field, in that field's own unit.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum OutputTruncation {
    /// A string showing its first and last lines around a marker line: the lines
    /// from `next_start` up to, but not including, `tail_start` are left out.
    TextGap {
        field: FieldPointer,
        total_lines: usize,
        #[schemars(range(min = 1))]
        next_start: usize,
        #[schemars(range(min = 2))]
        tail_start: usize,
    },
    /// A string, continued by line position.
    Text {
        field: FieldPointer,
        total_lines: usize,
        #[schemars(range(min = 1))]
        next_start: usize,
        /// Bytes into the `next_start` line, when a long line was cut.
        #[schemars(with = "usize", range(min = 1))]
        next_offset: Option<usize>,
    },
    /// An array's leading elements, plus any `kept` ranges holding complete fields.
    Elements {
        field: FieldPointer,
        shown: usize,
        total_elements: usize,
        #[schemars(with = "Vec<[usize; 2]>")]
        kept: Option<Vec<[usize; 2]>>,
    },
    /// An object's shown members: a collection's leading ones in source order, or
    /// none of a record emptied to fit.
    Members {
        field: FieldPointer,
        shown: usize,
        total_members: usize,
        #[schemars(with = "Vec<[usize; 2]>")]
        kept: Option<Vec<[usize; 2]>>,
    },
    /// Further cuts within `field`, too many to list.
    Summary { field: FieldPointer, cuts: usize },
}

impl OutputTruncation {
    pub fn field(&self) -> &FieldPointer {
        match self {
            Self::TextGap { field, .. }
            | Self::Text { field, .. }
            | Self::Elements { field, .. }
            | Self::Members { field, .. }
            | Self::Summary { field, .. } => field,
        }
    }

    /// Where reading the cut field continues: its next line, or its first element
    /// or member not shown. None for a summary of cuts.
    pub fn continuation(&self) -> Option<Continuation<'_>> {
        let first_missing = |shown: usize, kept: &Option<Vec<[usize; 2]>>| match kept.as_deref() {
            Some([[0, last], ..]) => last + 1,
            Some(_) => 0,
            None => shown,
        };
        match self {
            Self::TextGap {
                field, next_start, ..
            } => Some(Continuation::Lines {
                field: Some(field),
                start: *next_start,
                offset: None,
            }),
            Self::Text {
                field,
                next_start,
                next_offset,
                ..
            } => Some(Continuation::Lines {
                field: Some(field),
                start: *next_start,
                offset: *next_offset,
            }),
            Self::Elements {
                field, shown, kept, ..
            }
            | Self::Members {
                field, shown, kept, ..
            } => Some(Continuation::Index {
                field: Some(field),
                index: first_missing(*shown, kept),
            }),
            Self::Summary { .. } => None,
        }
    }
}

/// A manager-produced projection, with attachments from the same job snapshot
/// as its state and presentation metadata. Captures remain append-only live
/// reads; this product does not claim an atomic snapshot of them.
#[derive(Clone, Debug)]
pub struct PresentedOutput {
    pub state: JobState,
    pub(crate) view: JobView,
    pub images: Vec<ImageRef>,
    /// Captures absent from a whole presentation, in the descriptor's stable
    /// order. Hydration is opt-in through `inspect_output_with_captures`.
    pub(super) capture_targets: Vec<HydrationTarget>,
}

/// A capture host inspection pages into its descriptor.
#[derive(Clone, Debug)]
pub(super) struct HydrationTarget {
    /// The descriptor's index among the presentation's captures.
    pub(super) index: usize,
    pub(super) field: FieldPointer,
    pub(super) capture: i64,
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

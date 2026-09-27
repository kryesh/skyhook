//! Output queries: what a page selects, and the bounds every query is admitted within.
use super::*;
use crate::tool::diagnostic::{Operation, Subject};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use std::sync::Arc;

/// Lines a page returns unless the query sets `limit`.
pub(crate) const DEFAULT_LIMIT: usize = 100;
/// The most lines one page returns.
pub(crate) const MAX_LIMIT: usize = 1000;
/// The most context lines a match carries.
pub(crate) const MAX_CONTEXT: usize = 20;
/// Bounds what compiling an untrusted pattern may allocate.
const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct OutputArgs {
    pub job: JobId,
    pub field: Option<FieldPointer>,
    pub start: Option<usize>,
    pub limit: Option<usize>,
    pub pattern: Option<String>,
    pub context: Option<usize>,
    pub offset: Option<usize>,
}

impl OutputArgs {
    pub fn new(job: JobId) -> Self {
        Self {
            job,
            field: None,
            start: None,
            limit: None,
            pattern: None,
            context: None,
            offset: None,
        }
    }

    /// Attachment intent depends on presence, never on equality to default values:
    /// any explicit field/page selector, including explicitly supplied defaults,
    /// yields a text page without images.
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

    /// The page this query selects, validated even when no output exists yet.
    pub(super) fn admit(&self) -> Result<Selection, ToolError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        let context = self.context.unwrap_or(0);
        if !(1..=MAX_LIMIT).contains(&limit) || self.start == Some(0) || context > MAX_CONTEXT {
            return Err(ToolError::invalid_arguments(format!(
                "limit must be 1-{MAX_LIMIT}, start positive, and context 0-{MAX_CONTEXT}"
            )));
        }
        if context > 0 && self.pattern.is_none() {
            return Err(ToolError::invalid_arguments("context requires pattern"));
        }
        Ok(Selection {
            field: self.field.clone().unwrap_or_else(FieldPointer::result),
            matcher: self.pattern.as_deref().map(pattern_matcher).transpose()?,
            context,
            start: self.start.unwrap_or(1),
            offset: self.offset.unwrap_or(0),
            limit,
        })
    }
}

#[derive(Clone)]
pub(super) struct Selection {
    pub(super) field: FieldPointer,
    pub(super) matcher: Option<Arc<RegexMatcher>>,
    pub(super) context: usize,
    pub(super) start: usize,
    pub(super) offset: usize,
    pub(super) limit: usize,
}

/// The line-oriented regex builder output search shares with the `search` tool.
pub(crate) fn line_matcher() -> RegexMatcherBuilder {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .line_terminator(Some(b'\n'))
        .ban_byte(Some(b'\0'))
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT);
    builder
}

pub(super) fn pattern_matcher(pattern: &str) -> Result<Arc<RegexMatcher>, ToolError> {
    let matcher = line_matcher().build(pattern).map_err(|_| {
        ToolError::invalid_arguments("regular expression could not be compiled")
            .operation(Operation::Validate, Subject::argument(["pattern"]))
    })?;
    Ok(Arc::new(matcher))
}

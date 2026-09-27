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
    /// Zero-based first element or member of a JSON field, or first match.
    pub index: Option<usize>,
    /// JSONPath (RFC 9535) query over a JSON field, rooted at it.
    pub query: Option<String>,
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
            index: None,
            query: None,
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
            || self.index.is_some()
            || self.query.is_some()
        {
            OutputSelection::Explicit
        } else if self.context.is_some() {
            OutputSelection::Whole
        } else {
            OutputSelection::WholeWithImages
        }
    }

    /// The page this query selects, validated even when no output exists yet.
    pub(super) fn admit(&self) -> Result<Query, ToolError> {
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
        let lines = Lines {
            matcher: self.pattern.as_deref().map(pattern_matcher).transpose()?,
            context,
            start: self.start.unwrap_or(1),
            offset: self.offset.unwrap_or(0),
        };
        // Selectors restating their defaults select nothing, so a model that fills
        // in schema defaults can still page either kind of field.
        let text = lines.matcher.is_some() || lines.start != 1 || lines.offset != 0;
        let index = self
            .index
            .filter(|&index| index > 0 || self.query.is_some());
        let position = match (&self.query, index) {
            (Some(_), _) | (None, Some(_)) if text => {
                return Err(ToolError::invalid_arguments(
                    "index and query select JSON; start, offset, pattern and context page text",
                ));
            }
            (Some(query), index) => {
                let path = serde_json_path::JsonPath::parse(query).map_err(|_| {
                    ToolError::invalid_arguments("query is not a JSONPath (RFC 9535) query")
                        .operation(Operation::Validate, Subject::argument(["query"]))
                })?;
                Position::Query {
                    path: Arc::new(path),
                    index: index.unwrap_or(0),
                }
            }
            (None, Some(index)) => Position::Index(index),
            (None, None) if text => Position::Lines(lines),
            (None, None) => Position::Unspecified,
        };
        Ok(Query {
            field: self.field.clone().unwrap_or_else(FieldPointer::result),
            limit,
            position,
        })
    }
}

/// An admitted page query. Its position is read in the selected field's unit.
#[derive(Clone)]
pub(super) struct Query {
    pub(super) field: FieldPointer,
    pub(super) limit: usize,
    pub(super) position: Position,
}

#[derive(Clone)]
pub(super) enum Position {
    /// The field's first page, in whichever unit it has.
    Unspecified,
    Lines(Lines),
    Index(usize),
    /// A page of a JSONPath query's matches.
    Query {
        path: Arc<serde_json_path::JsonPath>,
        index: usize,
    },
}

/// A position within text: a line and byte offset, optionally searching.
#[derive(Clone)]
pub(super) struct Lines {
    pub(super) matcher: Option<Arc<RegexMatcher>>,
    pub(super) context: usize,
    pub(super) start: usize,
    pub(super) offset: usize,
}

impl Default for Lines {
    fn default() -> Self {
        Self {
            matcher: None,
            context: 0,
            start: 1,
            offset: 0,
        }
    }
}

impl Query {
    /// The text page selected, unless the query selects JSON by index or query.
    pub(super) fn lines(&self) -> Option<Selection> {
        match &self.position {
            Position::Index(_) | Position::Query { .. } => None,
            Position::Lines(lines) => Some(self.selection(lines)),
            Position::Unspecified => Some(self.text()),
        }
    }

    /// The first text page, for fields that have no page of either unit yet.
    pub(super) fn text(&self) -> Selection {
        match &self.position {
            Position::Lines(lines) => self.selection(lines),
            _ => self.selection(&Lines::default()),
        }
    }

    fn selection(&self, lines: &Lines) -> Selection {
        Selection {
            field: self.field.clone(),
            matcher: lines.matcher.clone(),
            context: lines.context,
            start: lines.start,
            offset: lines.offset,
            limit: self.limit,
        }
    }

    /// The first element or member selected, unless the query selects text or matches.
    pub(super) fn index(&self) -> Option<usize> {
        match self.position {
            Position::Index(index) => Some(index),
            Position::Unspecified => Some(0),
            Position::Lines(_) | Position::Query { .. } => None,
        }
    }
}

/// A text page: `field` and `limit` repeat the query's.
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

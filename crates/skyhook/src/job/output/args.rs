//! Output queries: what a page selects, and the bounds every query is admitted within.
use super::*;
use grep_regex::RegexMatcher;
use skyhook_base::bounded::Bounded;
use skyhook_worker::diagnostic::{Operation, Subject};
use std::sync::Arc;

/// What a page returns unless the query sets `limit`.
pub(crate) const DEFAULT_LIMIT: PageLimit = PageLimit::new::<100>();
/// The most lines, values or matches one page returns.
pub(crate) const MAX_LIMIT: usize = 1000;
/// The most context lines a match carries.
pub(crate) const MAX_CONTEXT: usize = 20;
/// Why a query did not parse, with the filter syntax models most often mistake.
const QUERY_SYNTAX: &str = "query is not a JSONPath (RFC 9535) query. Filters are written \
    $[?search(@.title, '[Ee]ncoder')]: match() tests a whole string and search() any part of it, \
    and comparisons read @.count > 2 or @.state == 'open', joined with && and ||.";
/// How many lines, values, matches or listed jobs one page returns.
pub type PageLimit = Bounded<1, { MAX_LIMIT as u64 }>;

/// How many `limit` is.
pub(crate) fn count(limit: PageLimit) -> usize {
    // Lossless: the bound is a `usize`.
    u64::from(limit) as usize
}

/// Paging selectors as a query sets them.
#[derive(Clone, Copy)]
pub(crate) struct Selectors {
    pub(crate) start: Option<usize>,
    pub(crate) offset: Option<usize>,
    pub(crate) context: Option<usize>,
}

impl Selectors {
    pub(crate) fn is_set(self) -> bool {
        let Paging {
            start,
            offset,
            context,
        } = self.into();
        start.or(offset).or(context).is_some()
    }
}

/// The selectors that differ from their defaults. One restating its default
/// selects nothing, so a model that fills in schema defaults can query JSON, or
/// list jobs.
struct Paging {
    start: Option<usize>,
    offset: Option<usize>,
    context: Option<usize>,
}

impl From<Selectors> for Paging {
    fn from(selectors: Selectors) -> Self {
        let chosen = |value: Option<usize>, default| value.filter(|&value| value != default);
        Self {
            start: chosen(selectors.start, 1),
            offset: chosen(selectors.offset, 0),
            context: chosen(selectors.context, 0),
        }
    }
}

/// What `start` counts; copy it from the page that returned next_start
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PageUnit {
    Lines,
    Elements,
    Members,
    Matches,
}

impl std::fmt::Display for PageUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Lines => "lines",
            Self::Elements => "elements",
            Self::Members => "members",
            Self::Matches => "matches",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputArgs {
    pub job: JobId,
    pub field: Option<FieldPointer>,
    pub start: Option<usize>,
    /// The unit of the page `start` continues; JSON pages past the first need it.
    pub unit: Option<PageUnit>,
    pub limit: Option<PageLimit>,
    pub pattern: Option<String>,
    pub context: Option<usize>,
    pub offset: Option<usize>,
    /// JSONPath (RFC 9535) query over a JSON field, rooted at it.
    pub query: Option<String>,
}

impl OutputArgs {
    pub fn new(job: JobId) -> Self {
        Self {
            job,
            field: None,
            start: None,
            unit: None,
            limit: None,
            pattern: None,
            context: None,
            offset: None,
            query: None,
        }
    }

    /// Attachment intent depends on presence, never on equality to default values:
    /// any explicit field/page selector, including explicitly supplied defaults,
    /// yields a text page without images.
    pub fn selection(&self) -> OutputSelection {
        if self.field.is_some()
            || self.start.is_some()
            || self.unit.is_some()
            || self.limit.is_some()
            || self.pattern.is_some()
            || self.offset.is_some()
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
        let paging = Paging::from(Selectors {
            start: self.start,
            offset: self.offset,
            context: self.context,
        });
        if paging.start == Some(0) || paging.context.is_some_and(|context| context > MAX_CONTEXT) {
            return Err(ToolError::invalid_arguments(format!(
                "start must be positive and context 0-{MAX_CONTEXT}"
            )));
        }
        if paging.context.is_some() && self.pattern.is_none() {
            return Err(ToolError::invalid_arguments("context requires pattern"));
        }
        let start = paging.start.unwrap_or(1);
        let position = match &self.query {
            Some(_)
                if self.pattern.is_some()
                    || paging.offset.is_some()
                    || paging.context.is_some() =>
            {
                return Err(ToolError::invalid_arguments(
                    "query selects JSON matches: page them with start and limit, not pattern, offset or context",
                ));
            }
            Some(query) => {
                let path = serde_json_path::JsonPath::parse(query).map_err(|_| {
                    ToolError::invalid_arguments(QUERY_SYNTAX)
                        .operation(Operation::Validate, Subject::argument(["query"]))
                })?;
                Position::Query {
                    path: Arc::new(path),
                    start,
                }
            }
            None => Position::Page(Page {
                matcher: self.pattern.as_deref().map(pattern_matcher).transpose()?,
                context: paging.context.unwrap_or(0),
                start,
                offset: paging.offset.unwrap_or(0),
            }),
        };
        Ok(Query {
            field: self.field.clone().unwrap_or_else(FieldPointer::result),
            limit: count(self.limit.unwrap_or(DEFAULT_LIMIT)),
            position,
            unit: self.unit,
        })
    }
}

/// An admitted page query. Its position is read in the selected field's unit.
#[derive(Clone)]
pub(super) struct Query {
    pub(super) field: FieldPointer,
    pub(super) limit: usize,
    pub(super) position: Position,
    unit: Option<PageUnit>,
}

#[derive(Clone)]
pub(super) enum Position {
    Page(Page),
    /// A page of a JSONPath query's matches, from the one-based `start`.
    Query {
        path: Arc<serde_json_path::JsonPath>,
        start: usize,
    },
}

/// A one-based first line of text, or element or member of JSON, optionally
/// filtered by `matcher`. `context` and `offset` apply to text only.
#[derive(Clone)]
pub(super) struct Page {
    pub(super) matcher: Option<Arc<RegexMatcher>>,
    pub(super) context: usize,
    pub(super) start: usize,
    pub(super) offset: usize,
}

impl Default for Page {
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
    /// Whether a page counting `unit` may start where the query asks. A position
    /// past the first continues a page, and holds only in that page's unit: a
    /// field read as lines while its job ran can be JSON once it finished, so a
    /// JSON page needs the unit named, and a named unit must match.
    pub(super) fn starts_in(&self, unit: PageUnit) -> Result<(), ToolError> {
        let start = match &self.position {
            Position::Page(page) => page.start,
            Position::Query { start, .. } => *start,
        };
        let restart = "a field read as lines while its job ran can be JSON once it finished, \
            and no line position applies to it: read it from start 1";
        match self.unit {
            _ if start == 1 => Ok(()),
            Some(given) if given == unit => Ok(()),
            None if unit == PageUnit::Lines => Ok(()),
            Some(given) => Err(ToolError::invalid_arguments(format!(
                "start continues {given}, but this field pages {unit}: {restart}"
            ))),
            None => Err(ToolError::invalid_arguments(format!(
                "start past 1 on a JSON field continues a page: pass the unit it named ({unit} here); {restart}"
            ))),
        }
    }

    /// The text page selected: a JSONPath query's reads text from its first line.
    pub(super) fn text(&self) -> Selection {
        let first = Page::default();
        let page = match &self.position {
            Position::Page(page) => page,
            Position::Query { .. } => &first,
        };
        Selection {
            field: self.field.clone(),
            matcher: page.matcher.clone(),
            context: page.context,
            start: page.start,
            offset: page.offset,
            limit: self.limit,
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

pub(super) fn pattern_matcher(pattern: &str) -> Result<Arc<RegexMatcher>, ToolError> {
    let matcher = skyhook_worker::op::line_matcher()
        .build(pattern)
        .map_err(|_| {
            ToolError::invalid_arguments("regular expression could not be compiled")
                .operation(Operation::Validate, Subject::argument(["pattern"]))
        })?;
    Ok(Arc::new(matcher))
}

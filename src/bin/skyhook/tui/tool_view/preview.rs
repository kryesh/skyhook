//! Saved-output presentation boundary. Pages are validated once from their wire
//! shape and dropped when malformed.
use serde_json::Value;
use skyhook::job::FieldPointer;
use std::num::NonZeroUsize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pagination {
    End,
    More { start: usize, offset: usize },
}

#[derive(Clone)]
pub(super) struct PreviewView {
    field: FieldPointer,
    source: String,
    pagination: Pagination,
    line_count: usize,
    total_lines: Option<usize>,
    /// An EOF page is not necessarily an entire document or a complete capture.
    /// Only the constructors establish this proof for error deduplication.
    complete_document: Option<Value>,
}

impl PreviewView {
    pub(super) fn wire(preview: Option<&Value>) -> Option<Self> {
        let preview = preview.filter(|preview| preview.is_object())?;
        let field = preview.get("field")?.as_str()?.parse().ok()?;
        let lines = preview.get("lines")?.as_array()?;
        let strings = lines
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?;
        let mut page = Self {
            field,
            source: strings.join("\n"),
            pagination: wire_pagination(preview)?,
            line_count: lines.len(),
            total_lines: optional_count(preview, "total_lines")?,
            complete_document: None,
        };
        page.complete_document = page.validate_complete_document();
        Some(page)
    }

    pub(super) fn field(&self) -> &FieldPointer {
        &self.field
    }
    pub(super) fn source(&self) -> &str {
        &self.source
    }
    pub(super) fn pagination(&self) -> Pagination {
        self.pagination
    }

    pub(super) fn continuation(&self) -> Option<(FieldPointer, usize, usize)> {
        match self.pagination {
            Pagination::End => None,
            Pagination::More { start, offset } => Some((self.field.clone(), start, offset)),
        }
    }

    pub(super) fn empty_whole(&self) -> bool {
        self.field.is_root() && self.line_count == 0
    }

    pub(super) fn complete_document(&self) -> Option<&Value> {
        self.complete_document.as_ref()
    }

    fn validate_complete_document(&self) -> Option<Value> {
        if !self.field.is_root()
            || self.pagination != Pagination::End
            || self.total_lines != Some(self.line_count)
        {
            return None;
        }
        let document: Value = serde_json::from_str(&self.source).ok()?;
        document.is_object().then_some(document)
    }
}

/// Absent/null is `Some(None)`; a present non-count is malformed (`None`).
fn optional_count(value: &Value, key: &str) -> Option<Option<usize>> {
    match value.get(key) {
        None | Some(Value::Null) => Some(None),
        Some(value) => Some(Some(usize::try_from(value.as_u64()?).ok()?)),
    }
}

/// The one source cursor parser; `None` when the cursor fields are malformed.
fn wire_pagination(value: &Value) -> Option<Pagination> {
    let offset = usize::try_from(value.get("next_offset")?.as_u64()?).ok()?;
    Some(match optional_count(value, "next_start")? {
        None if offset == 0 => Pagination::End,
        None => return None,
        Some(start) => Pagination::More {
            start: NonZeroUsize::new(start)?.get(),
            offset,
        },
    })
}

/// A source cursor `(field, start, offset)` is independent of displayed text.
/// Historical truncation entries have no page lines, so admit only their
/// cursor fields.
pub(super) fn wire_continuation(value: &Value) -> Option<(FieldPointer, usize, usize)> {
    let field = value.get("field")?.as_str()?.parse().ok()?;
    match wire_pagination(value)? {
        Pagination::End => None,
        Pagination::More { start, offset } => Some((field, start, offset)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Anything the core page schema forbids falls back to raw envelope
    /// rendering rather than a guessed page.
    #[test]
    fn malformed_pages_are_not_admitted() {
        let page =
            json!({"field": "/result", "lines": ["one"], "next_start": 2, "next_offset": 10});
        assert!(PreviewView::wire(Some(&page)).is_some());
        for (key, value) in [
            ("lines", json!(["one", 2])),
            ("field", Value::Null),
            ("next_start", json!(0)),
            ("next_start", Value::Null),
        ] {
            let mut malformed = page.clone();
            malformed[key] = value.clone();
            assert!(
                PreviewView::wire(Some(&malformed)).is_none(),
                "{key}: {value}"
            );
        }
    }
}

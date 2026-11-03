//! JSONPath queries and pattern filters over a saved JSON field. The field's
//! value is read into memory within a size cap and its matches are paged like
//! elements. Query evaluation itself is bounded only by that input: it cannot be
//! interrupted, and duplicate matches, which RFC 9535 keeps, can multiply.
use grep_matcher::Matcher as _;
use grep_regex::RegexMatcher;
use serde_json::Value;
use serde_json_path::JsonPath;
use struson::{
    reader::JsonReader,
    writer::{JsonStreamWriter, JsonWriter},
};

use super::{
    FieldPointer, Match, MatchPage, OutputPreview, ToolError, check_cancelled,
    elements::Budget,
    json::{self, saved_json},
    preview::{self, Accounting, json_bytes},
    projection::Presented,
    render::{Clipped, JsonField},
    shape::Shape,
};

/// What selects a match page's values from a JSON field.
#[derive(Clone, Copy)]
pub(super) enum Filter<'a> {
    /// A JSONPath query's matches, paged by match.
    Path(&'a JsonPath),
    /// The field's elements or members whose compact JSON text matches, paged by
    /// source position. A member's text is its name, a colon and its value.
    Pattern(&'a RegexMatcher),
}

/// Up to `limit` values `filter` selects in the JSON value `json` is positioned
/// at, the value at `field`, from position `first`. Without matches, the page
/// carries the value's shape, which shows the members a filter can name.
pub(super) fn page(
    json: &mut JsonField,
    field: &FieldPointer,
    filter: Filter<'_>,
    first: usize,
    limit: usize,
    cancellation: &crate::job::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let value = read(json, crate::limits::WHOLE_VALUE_BYTES)?;
    check_cancelled(cancellation)?;
    type Nodes<'v> =
        Box<dyn Iterator<Item = Result<(usize, FieldPointer, &'v Value), ToolError>> + 'v>;
    let (total, nodes): (usize, Nodes<'_>) = match filter {
        Filter::Path(path) => {
            let located = path.query_located(&value);
            let total = located.len();
            let nodes = located.into_iter().enumerate().skip(first);
            let nodes = nodes.map(|(position, node)| {
                let pointer = format!("{}{}", field.as_str(), node.location().to_json_pointer());
                let at = (pointer.parse())
                    .map_err(|_| ToolError::failed("a query match has no JSON Pointer"))?;
                Ok((position, at, node.node()))
            });
            (total, Box::new(nodes))
        }
        Filter::Pattern(matcher) => {
            let members = value.as_object().into_iter().flatten();
            let elements = value.as_array().into_iter().flatten();
            let children = (members.map(|(name, value)| (Some(name), value)))
                .chain(elements.map(|value| (None, value)));
            let (mut matched, mut text) = (Vec::new(), Vec::new());
            for (position, (name, child)) in children.enumerate() {
                check_cancelled(cancellation)?;
                text.clear();
                if let Some(name) = name {
                    serde_json::to_writer(&mut text, name)?;
                    text.push(b':');
                }
                serde_json::to_writer(&mut text, child)?;
                if matcher.is_match(&text).map_err(ToolError::failed)? {
                    matched.push((position, name, child));
                }
            }
            let total = matched.len();
            let nodes = matched
                .into_iter()
                .filter(move |(position, ..)| *position >= first);
            let nodes = nodes.map(|(position, name, child)| {
                let at = name.map_or_else(|| field.index(position), |name| field.property(name));
                Ok((position, at, child))
            });
            (total, Box::new(nodes))
        }
    };
    let (mut matches, mut next, mut budget) = (Vec::new(), None, Budget::default());
    for node in nodes {
        let (position, at, node) = node?;
        if budget.shown == limit || cancellation.is_cancelled() {
            next = Some(position);
            break;
        }
        let bytes = serde_json::to_vec(node)?;
        let pooled = preview::pool(
            &mut json::reader(bytes.as_slice()),
            &at,
            &Presented::default(),
            &Clipped::new(),
            cancellation,
            Accounting::Page,
        )?;
        // The match's pointer and `{"at":…,"value":…}` count toward the page.
        let framing = json_bytes(&Match {
            at: at.clone(),
            value: Value::Null,
        }) - json_bytes(&Value::Null);
        let Some(value) = budget.admit(&pooled, &at, framing) else {
            next = Some(position);
            break;
        };
        matches.push(Match { at, value });
    }
    check_cancelled(cancellation)?;
    let (shape, truncated) = budget.finish();
    let shape = shape
        .or_else(|| (total == 0).then(|| Shape::of(&value).render(Accounting::Page.shape_bytes())));
    Ok(OutputPreview::Matches(MatchPage {
        field: Some(field.clone()),
        matches,
        total_matches: total,
        next_start: next.map(|position| position + 1),
        shape,
        truncated,
    }))
}

/// The next value, read whole unless its JSON text exceeds `cap` bytes. The
/// meter refuses input past the cap, before an oversized name or string is
/// buffered; bytes the reader held before it was armed are counted by position.
fn read(json: &mut JsonField, cap: usize) -> Result<Value, ToolError> {
    let start = position(json);
    json.meter.arm(cap as u64);
    let mut bytes = Vec::new();
    let copied = {
        let mut writer = JsonStreamWriter::new(&mut bytes);
        match json.reader.transfer_to(&mut writer) {
            Ok(()) => writer
                .finish_document()
                .map(drop)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    };
    if json.meter.refused() || position(json) - start > cap as u64 {
        return Err(ToolError::failed(format!(
            "pattern or query input exceeds {} MiB of JSON; select a field within it",
            cap >> 20
        )));
    }
    copied.map_err(saved_json)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Bytes of JSON text the reader has consumed.
fn position(json: &JsonField) -> u64 {
    (json.reader.current_position(false).data_pos).expect("stream readers track their position")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read;

    fn filter(value: &Value, filter: Filter, first: usize, limit: usize) -> MatchPage {
        let bytes = serde_json::to_vec(value).unwrap();
        let field = FieldPointer::result().property("items");
        let mut json = JsonField::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        match page(&mut json, &field, filter, first, limit, &Default::default()).unwrap() {
            OutputPreview::Matches(page) => page,
            page => panic!("{page:?}"),
        }
    }

    fn run(value: &Value, query: &str, first: usize, limit: usize) -> MatchPage {
        let path = JsonPath::parse(query).unwrap();
        filter(value, Filter::Path(&path), first, limit)
    }

    fn at(page: &MatchPage) -> Vec<&str> {
        page.matches.iter().map(|found| found.at.as_str()).collect()
    }

    #[test]
    fn matches_carry_saved_pointers_in_document_order_and_page_by_start() {
        let items = json!([
            {"name": "api-1", "phase": "Failed"},
            {"name": "db-1", "phase": "Running"},
            {"name": "api-2", "phase": "Failed"},
        ]);
        let page = run(
            &items,
            "$[?search(@.name, '^api') && @.phase == 'Failed'].name",
            0,
            100,
        );
        assert_eq!(at(&page), ["/result/items/0/name", "/result/items/2/name"]);
        assert_eq!(page.matches[1].value, "api-2");
        assert_eq!((page.total_matches, page.next_start), (2, None));
        // Node lists keep duplicates, and pages continue by match position.
        let page = run(&items, "$[0,0,1]", 1, 1);
        assert_eq!(at(&page), ["/result/items/0"]);
        assert_eq!((page.total_matches, page.next_start), (3, Some(3)));
        // A pattern keeps the elements whose JSON text matches, continuing by source position.
        let failed = super::super::args::pattern_matcher("\"Failed\"").unwrap();
        let page = filter(&items, Filter::Pattern(&failed), 0, 1);
        assert_eq!(at(&page), ["/result/items/0"]);
        assert_eq!((page.total_matches, page.next_start), (2, Some(3)));
        let page = filter(&items, Filter::Pattern(&failed), 2, 1);
        assert_eq!(at(&page), ["/result/items/2"]);
        assert_eq!(page.next_start, None);
        // A member's text includes its name.
        let named = super::super::args::pattern_matcher("^\"db").unwrap();
        let members = json!({"api": {"phase": "Failed"}, "db": {"phase": "Running"}});
        let page = filter(&members, Filter::Pattern(&named), 0, 100);
        assert_eq!(at(&page), ["/result/items/db"]);
    }

    #[test]
    fn match_pages_count_their_framing() {
        let items: Vec<_> = (0..3000).collect();
        let page = run(&json!(items), "$[*]", 0, super::super::args::MAX_LIMIT);
        assert!(json_bytes(&page.matches) <= Accounting::Page.budget());
        assert_eq!(page.next_start, Some(page.matches.len() + 1));
    }

    #[test]
    fn an_oversized_match_is_sampled_at_its_own_pointer() {
        let rows: Vec<_> = (0..5000).map(|index| json!({"n": index})).collect();
        let page = run(&json!({"rows": rows}), "$.rows", 0, 100);
        assert_eq!(at(&page), ["/result/items/rows"]);
        assert_eq!(page.shape, Some(json!([5000, {"n": "integer"}])));
        let cut = &page.truncated.unwrap()[0];
        assert_eq!(cut.field().as_str(), "/result/items/rows");
    }

    #[test]
    fn input_is_capped_exactly_including_bytes_read_before_the_query() {
        const CAP: usize = 1 << 20;
        let string = |bytes: usize| format!("\"{}\"", "s".repeat(bytes - 2));
        let within = |text: String| {
            let mut json = JsonField::new(Box::new(std::io::Cursor::new(text)), Default::default());
            read(&mut json, CAP).is_ok()
        };
        assert!(within(string(CAP)));
        assert!(!within(string(CAP + 1)));
        // Positioned inside a document, the reader already holds part of the value.
        let text = format!("[{}]", string(CAP + 1));
        let mut json = JsonField::new(Box::new(std::io::Cursor::new(text)), Default::default());
        json.reader.begin_array().unwrap();
        assert!(read(&mut json, CAP).is_err());
        // One name far longer than the cap, streamed rather than built.
        let input = std::io::Cursor::new("{\"")
            .chain(std::io::repeat(b'k').take(1 << 40))
            .chain(std::io::Cursor::new("\":1}"));
        let mut json = JsonField::new(Box::new(input), Default::default());
        let error = read(&mut json, CAP).unwrap_err();
        assert!(error.to_string().contains("1 MiB"), "{error}");
    }

    #[test]
    fn a_query_without_matches_shows_the_members_it_could_name() {
        let packages = json!({"a": {"version": "1"}, "b": {"version": "2", "name": "b"}});
        let page = run(
            &json!({"packages": packages}),
            "$.packages[?@.version == '3']",
            0,
            10,
        );
        assert_eq!(page.total_matches, 0);
        assert_eq!(
            page.shape,
            Some(
                json!({"packages": {"a": {"version": "string"}, "b": {"version": "string", "name": "string"}}})
            )
        );
    }
}

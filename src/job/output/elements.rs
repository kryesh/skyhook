//! Pages of a JSON field's elements or members: whole consecutive values within
//! the page budget. A first value that is not whole within it is sampled; any
//! later one starts the next page.
use std::{collections::BTreeSet, io::Read};

use serde_json::{Map, Value};
use struson::reader::{JsonReader, JsonStreamReader};

use super::{
    ElementPage, FieldPointer, MemberPage, OutputFields, OutputPreview, OutputTruncation,
    ToolError,
    json::saved_json,
    preview::{self, Accounting, Pooled},
    projection::Presented,
    render::JsonField,
};

/// Up to `limit` values of the container `json` is positioned at, from `index`.
pub(super) fn page(
    json: &mut JsonField,
    container: Container,
    field: &FieldPointer,
    index: usize,
    limit: usize,
    cancellation: &crate::job::CancellationToken,
) -> Result<OutputPreview, ToolError> {
    let JsonField {
        reader, clipped, ..
    } = json;
    let object = match container {
        Container::Object => reader.begin_object().map(|()| true),
        Container::Array => reader.begin_array().map(|()| false),
    }
    .map_err(saved_json)?;
    let (mut position, mut next, mut budget) = (0, None, Budget::default());
    let mut elements = Vec::new();
    let mut members = Map::new();
    while reader.has_next().map_err(saved_json)? {
        if cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        if position < index || next.is_some() || budget.shown == limit {
            if position >= index {
                next.get_or_insert(position);
            }
            if object {
                reader.skip_name().map_err(saved_json)?;
            }
            reader.skip_value().map_err(saved_json)?;
            position += 1;
            continue;
        }
        let key = object
            .then(|| reader.next_name_owned())
            .transpose()
            .map_err(saved_json)?;
        let child = key
            .as_ref()
            .map_or_else(|| field.index(position), |key| field.property(key));
        let pooled = preview::pool(
            reader,
            &child,
            &Presented::default(),
            clipped,
            cancellation,
            Accounting::Page,
        )?;
        // A member's name and colon count toward the page as well as its value.
        let name = key.as_ref().map_or(0, |key| preview::json_bytes(key) + 1);
        match (budget.admit(&pooled, &child, name), key) {
            (None, _) => next = Some(position),
            (Some(value), Some(key)) => {
                members.insert(key, value);
            }
            (Some(value), None) => elements.push(value),
        }
        position += 1;
    }
    let (shape, truncated) = budget.finish();
    Ok(if object {
        OutputPreview::Members(MemberPage {
            field: Some(field.clone()),
            members,
            total_members: position,
            next_index: next,
            shape,
            truncated,
        })
    } else {
        OutputPreview::Elements(ElementPage {
            field: Some(field.clone()),
            elements,
            total_elements: position,
            next_index: next,
            shape,
            truncated,
        })
    })
}

/// What one page holds so far: values shown whole within the page budget,
/// except a first one that is not whole, which is sampled to fit it.
pub(super) struct Budget {
    used: usize,
    pub(super) shown: usize,
    shape: Option<Value>,
    truncated: Vec<OutputTruncation>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            // The brackets or braces around the page's values.
            used: 2,
            shown: 0,
            shape: None,
            truncated: Vec::new(),
        }
    }
}

impl Budget {
    /// What the page shows of `pooled`, the value at `field` with `extra` bytes
    /// of framing besides the separator before it; none when it starts the next
    /// page instead.
    pub(super) fn admit(
        &mut self,
        pooled: &Pooled,
        field: &FieldPointer,
        extra: usize,
    ) -> Option<Value> {
        let budget = Accounting::Page.budget();
        let extra = extra + usize::from(self.shown > 0);
        // A page counts everything it holds, text fields and separators included.
        let whole = (pooled.whole(field))
            .filter(|whole| whole.truncated.is_empty())
            .map(|whole| (preview::json_bytes(&whole.value) + extra, whole))
            .filter(|(bytes, _)| self.used + bytes <= budget);
        let shown = match whole {
            Some((bytes, whole)) => {
                self.used += bytes;
                whole
            }
            None if self.shown > 0 => return None,
            None => {
                let room = budget.saturating_sub(self.used + extra);
                self.used = budget;
                pooled.fitted(field, &BTreeSet::new(), room)
            }
        };
        self.shape = self.shape.take().or(shown.shape);
        self.truncated.extend(shown.truncated);
        self.shown += 1;
        Some(shown.value)
    }

    /// The sampled value's shape and every cut, when any.
    pub(super) fn finish(self) -> (Option<Value>, Option<Vec<OutputTruncation>>) {
        (
            self.shape,
            (!self.truncated.is_empty()).then_some(self.truncated),
        )
    }
}

/// Children listed for host field discovery before the rest are left to paging.
const LISTED_CHILDREN: usize = 100;

/// Pointers of up to `LISTED_CHILDREN` members or elements of the container
/// `reader` is at, from `index`, and where the rest continue.
pub(super) fn children<R: Read>(
    reader: &mut JsonStreamReader<R>,
    container: Container,
    field: &FieldPointer,
    index: usize,
    cancellation: &crate::job::CancellationToken,
) -> Result<OutputFields, ToolError> {
    match container {
        Container::Object => reader.begin_object(),
        Container::Array => reader.begin_array(),
    }
    .map_err(saved_json)?;
    let (mut fields, mut position) = (Vec::new(), 0);
    while reader.has_next().map_err(saved_json)? {
        if cancellation.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        if fields.len() == LISTED_CHILDREN {
            let next_index = Some(position);
            return Ok(OutputFields { fields, next_index });
        }
        let child = match container {
            Container::Object => {
                let name = reader.next_name().map_err(saved_json)?;
                (position >= index).then(|| field.property(name))
            }
            Container::Array => (position >= index).then(|| field.index(position)),
        };
        fields.extend(child);
        reader.skip_value().map_err(saved_json)?;
        position += 1;
    }
    Ok(OutputFields {
        fields,
        next_index: None,
    })
}

/// The container a JSON field is, which names the unit it pages in.
#[derive(Clone, Copy)]
pub(super) enum Container {
    Object,
    Array,
}

impl Container {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
        }
    }

    pub(super) fn unit(self) -> &'static str {
        match self {
            Self::Object => "members",
            Self::Array => "elements",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn read(value: &Value, index: usize, limit: usize) -> OutputPreview {
        let bytes = serde_json::to_vec(value).unwrap();
        let mut json = JsonField::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        let field = FieldPointer::result().property("items");
        let container = if value.is_object() {
            Container::Object
        } else {
            Container::Array
        };
        page(
            &mut json,
            container,
            &field,
            index,
            limit,
            &Default::default(),
        )
        .unwrap()
    }

    fn elements(page: &OutputPreview) -> &ElementPage {
        match page {
            OutputPreview::Elements(page) => page,
            page => panic!("{page:?}"),
        }
    }

    #[test]
    fn pages_continue_by_source_position_until_the_end() {
        let items: Vec<_> = (0..250).map(|index| json!({"id": index})).collect();
        let mut index = 0;
        let mut seen = Vec::new();
        loop {
            let page = read(&Value::Array(items.clone()), index, 100);
            let page = elements(&page);
            assert_eq!(page.total_elements, 250);
            seen.extend(page.elements.iter().cloned());
            match page.next_index {
                Some(next) => index = next,
                None => break,
            }
        }
        assert_eq!(seen, items);
        // Members keep source order and page the same way.
        let members: serde_json::Map<_, _> = (0..5)
            .map(|index| (format!("z{index}"), json!(index)))
            .collect();
        let OutputPreview::Members(page) = read(&Value::Object(members.clone()), 3, 100) else {
            panic!("a member page")
        };
        assert_eq!(
            page.members,
            members
                .into_iter()
                .skip(3)
                .collect::<serde_json::Map<_, _>>()
        );
        assert_eq!((page.total_members, page.next_index), (5, None));
    }

    #[test]
    fn pages_count_everything_they_hold_and_do_not_presample_small_values() {
        // A small value is whole however deeply its arrays nest.
        let deep: Vec<_> = (0..40).map(|_| json!(1)).collect();
        let page = read(&json!([[[deep]]]), 0, 100);
        assert_eq!(elements(&page).elements, [json!([[deep]])]);
        let long = json!(vec![1; 1001]);
        let wide: serde_json::Map<_, _> = (0..101)
            .map(|index| (format!("k{index}"), json!(1)))
            .collect();
        let page = read(&json!([long, wide]), 0, 100);
        assert_eq!(elements(&page).elements, [long, json!(wide)]);
        // Numbers count as shown, not as the source spelled them.
        let spelled = format!("1.{}", "0".repeat(1000));
        let text = format!("[[{}]]", vec![spelled; 40].join(","));
        let mut json = JsonField::new(Box::new(std::io::Cursor::new(text)), Default::default());
        let field = FieldPointer::result();
        let numbers = super::page(
            &mut json,
            Container::Array,
            &field,
            0,
            100,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(elements(&numbers).elements, [json!(vec![1.0; 40])]);
        // Text fields and member names count toward a page like anything else.
        let wide = json!({"a": "x".repeat(20_000), "b": "y".repeat(20_000)});
        let page = read(&json!([wide]), 0, 100);
        let page = elements(&page);
        let bytes = preview::json_bytes(&page.elements[0])
            + preview::json_bytes(&page.shape)
            + preview::json_bytes(&page.truncated);
        assert!(bytes <= Accounting::Page.budget(), "{bytes}");
        assert!(!page.truncated.as_deref().unwrap_or_default().is_empty());
    }

    #[test]
    fn pages_hold_whole_values_within_the_budget_and_sample_only_an_oversized_first() {
        let medium: Vec<_> = (0..10)
            .map(|_| json!("m".repeat(Accounting::Page.budget() / 4)))
            .collect();
        let page = read(&Value::Array(medium), 0, 100);
        let page = elements(&page);
        assert_eq!(page.elements.len(), 3);
        assert_eq!(page.next_index, Some(3));
        assert!(page.truncated.is_none() && page.shape.is_none());
        let huge: Vec<_> = (0..5000).map(|index| json!({"n": index})).collect();
        let page = read(&json!([huge, 1]), 0, 100);
        let page = elements(&page);
        assert_eq!(page.elements.len(), 1);
        assert_eq!(page.next_index, Some(1));
        assert_eq!(page.shape, Some(json!([5000, {"n": "integer"}])));
        let cut = page.truncated.as_ref().unwrap();
        assert_eq!(cut[0].field().as_str(), "/result/items/0");
    }

    #[test]
    fn children_are_listed_from_an_index_up_to_the_listing_limit() {
        let list = |value: &Value, container, index| {
            let bytes = serde_json::to_vec(value).unwrap();
            let mut json =
                JsonField::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
            let field = FieldPointer::result();
            children(
                &mut json.reader,
                container,
                &field,
                index,
                &Default::default(),
            )
            .unwrap()
        };
        let items = json!(vec![0; LISTED_CHILDREN + 1]);
        let first = list(&items, Container::Array, 0);
        assert_eq!(first.fields.len(), LISTED_CHILDREN);
        assert_eq!(first.next_index, Some(LISTED_CHILDREN));
        let rest = list(&items, Container::Array, LISTED_CHILDREN);
        assert_eq!(rest.fields, [FieldPointer::result().index(LISTED_CHILDREN)]);
        assert_eq!(rest.next_index, None);
        let members = list(&json!({"a/b": 1, "c": 2}), Container::Object, 1);
        assert_eq!(members.fields, [FieldPointer::result().property("c")]);
    }
}

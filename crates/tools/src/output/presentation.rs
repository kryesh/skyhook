//! How presentation shows a result's fields, as their schemas and producers declare.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;
use skyhook_base::enums::NamedEnum;

use super::FieldPointer;

/// The schema keyword declaring how presentation shows a field.
pub const PREVIEW: &str = "x-skyhook-preview";

/// How a preview shows a field whose schema declares it. Other fields are
/// shortened from their end, or sampled.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
#[serde(rename_all = "snake_case")]
pub enum FieldPresentation {
    /// Never shortened.
    Complete,
    /// Text shortened in its middle, keeping its first and last lines.
    Ends,
}

impl FieldPresentation {
    fn mark(self, schema: &mut schemars::Schema) {
        schema.insert(PREVIEW.into(), self.as_str().into());
    }
}

/// Mark `schema` complete: `#[schemars(transform = complete)]` on a field or type.
pub fn complete(schema: &mut schemars::Schema) {
    FieldPresentation::Complete.mark(schema);
}

/// Mark `schema` as text keeping both ends: `#[schemars(transform = ends)]` on a field.
pub fn ends(schema: &mut schemars::Schema) {
    FieldPresentation::Ends.mark(schema);
}

/// Schema of text a finished job reads as the JSON it holds, when it holds one
/// object or array, or a whitespace-separated sequence of them.
pub struct JsonText;

impl JsonText {
    pub fn schema() -> Value {
        serde_json::json!({"anyOf": [
            {"type": "string", "contentMediaType": "application/json"},
            {"type": "object"},
            {"type": "array"}
        ]})
    }
}

impl schemars::JsonSchema for JsonText {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JsonText".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::Schema::try_from(Self::schema()).expect("an object schema")
    }
}

/// Fields of a result whose presentation is declared, each presented one way.
#[derive(Clone, Debug, Default)]
pub struct Presented {
    complete: BTreeSet<FieldPointer>,
    ends: BTreeSet<FieldPointer>,
}

impl Presented {
    pub const fn new() -> Self {
        Self {
            complete: BTreeSet::new(),
            ends: BTreeSet::new(),
        }
    }

    /// Fields never shortened.
    pub const fn complete(&self) -> &BTreeSet<FieldPointer> {
        &self.complete
    }

    /// Text fields keeping their first and last lines.
    pub const fn ends(&self) -> &BTreeSet<FieldPointer> {
        &self.ends
    }

    /// A field declared both ways is complete.
    pub(crate) fn insert(&mut self, field: FieldPointer, presentation: FieldPresentation) {
        match presentation {
            FieldPresentation::Complete => {
                self.ends.remove(&field);
                self.complete.insert(field);
            }
            FieldPresentation::Ends if !self.complete.contains(&field) => {
                self.ends.insert(field);
            }
            FieldPresentation::Ends => {}
        }
    }

    pub fn fields(&self) -> impl Iterator<Item = (&FieldPointer, FieldPresentation)> {
        let of = |presentation| move |field| (field, presentation);
        (self.complete.iter().map(of(FieldPresentation::Complete)))
            .chain(self.ends.iter().map(of(FieldPresentation::Ends)))
    }

    /// The value at `field` is a presentation already: presenting it again
    /// leaves it whole.
    pub fn presentation(field: FieldPointer) -> Self {
        Self::from_iter([(field, FieldPresentation::Complete)])
    }

    /// These presentations, of a value placed at `field`.
    pub fn at(&self, field: &FieldPointer) -> Self {
        let placed = |(inner, presentation)| (field.join(inner), presentation);
        self.fields().map(placed).collect()
    }

    /// The presentations of the value at `field`, addressed from it.
    pub fn within(&self, field: &FieldPointer) -> Self {
        let inside = |(inner, presentation): (&FieldPointer, _)| {
            Some((inner.relative_to(field)?, presentation))
        };
        self.fields().filter_map(inside).collect()
    }
}

impl Extend<(FieldPointer, FieldPresentation)> for Presented {
    fn extend<I: IntoIterator<Item = (FieldPointer, FieldPresentation)>>(&mut self, fields: I) {
        for (field, presentation) in fields {
            self.insert(field, presentation);
        }
    }
}

impl FromIterator<(FieldPointer, FieldPresentation)> for Presented {
    fn from_iter<I: IntoIterator<Item = (FieldPointer, FieldPresentation)>>(fields: I) -> Self {
        let mut presented = Self::default();
        presented.extend(fields);
        presented
    }
}

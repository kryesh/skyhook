//! The inferred type of a JSON value, merged across every value seen at each
//! position, and its compact presentation.
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

/// Objects with more members than this are collections: their members are
/// sampled like array elements and their shape merges every member's value.
pub(crate) const COLLECTION_MEMBERS: usize = 100;

crate::named_enum::named_enum! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) parsed enum Scalar {
        Null = "null",
        Boolean = "boolean",
        Integer = "integer",
        Number = "number",
        String = "string",
    }
}

impl Scalar {
    /// A JSON number token's scalar type.
    pub(crate) fn number(token: &str) -> Self {
        if token.contains(['.', 'e', 'E']) {
            Self::Number
        } else {
            Self::Integer
        }
    }
}

/// The smallest and largest count seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Range {
    min: usize,
    max: usize,
}

impl Range {
    fn of(count: usize) -> Self {
        Self {
            min: count,
            max: count,
        }
    }

    fn merge(&mut self, other: Self) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    fn render(self) -> Value {
        if self.min == self.max {
            self.min.into()
        } else {
            format!("{}..{}", self.min, self.max).into()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Shape {
    scalars: Vec<Scalar>,
    object: Option<Box<ObjectShape>>,
    array: Option<Box<ArrayShape>>,
}

#[derive(Clone, Debug, PartialEq)]
struct ObjectShape {
    /// How many objects were merged, and their member counts.
    objects: usize,
    counts: Range,
    members: Members,
}

#[derive(Clone, Debug, PartialEq)]
enum Members {
    /// Each member with the number of merged objects that had it.
    Record(IndexMap<String, (usize, Shape)>),
    Collection(Shape),
}

#[derive(Clone, Debug, PartialEq)]
struct ArrayShape {
    lengths: Range,
    element: Shape,
}

impl Shape {
    pub(crate) fn scalar(scalar: Scalar) -> Self {
        Self {
            scalars: vec![scalar],
            ..Self::default()
        }
    }

    /// The shape of an in-memory value.
    pub(crate) fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::scalar(Scalar::Null),
            Value::Bool(_) => Self::scalar(Scalar::Boolean),
            Value::Number(number) => Self::scalar(Scalar::number(&number.to_string())),
            Value::String(_) => Self::scalar(Scalar::String),
            Value::Array(items) => {
                let mut element = Self::default();
                for item in items {
                    element.merge(Self::of(item));
                }
                Self::array(items.len(), element)
            }
            Value::Object(map) => {
                let mut object = ObjectBuilder::default();
                for (key, value) in map {
                    object.member(key, Self::of(value));
                }
                object.finish()
            }
        }
    }

    /// An array shape from its length and its elements' merged shape.
    pub(crate) fn array(length: usize, element: Shape) -> Self {
        Self {
            array: Some(Box::new(ArrayShape {
                lengths: Range::of(length),
                element,
            })),
            ..Self::default()
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        for scalar in other.scalars {
            if !self.scalars.contains(&scalar) {
                self.scalars.push(scalar);
            }
        }
        match (&mut self.object, other.object) {
            (Some(object), Some(other)) => object.merge(*other),
            (slot @ None, other) => *slot = other,
            (Some(_), None) => {}
        }
        match (&mut self.array, other.array) {
            (Some(array), Some(other)) => {
                array.lengths.merge(other.lengths);
                array.element.merge(other.element);
            }
            (slot @ None, other) => *slot = other,
            (Some(_), None) => {}
        }
    }

    /// The compact notation within `bytes`, collapsing the deepest levels first;
    /// a collapsed object reads `"{…}"` and a collapsed array `[n, "…"]`.
    pub(crate) fn render(&self, bytes: usize) -> Value {
        let mut depth = self.depth();
        loop {
            let rendered = self.render_to(depth);
            if depth == 0 || serde_json::to_vec(&rendered).map_or(0, |json| json.len()) <= bytes {
                return rendered;
            }
            depth -= 1;
        }
    }

    fn depth(&self) -> usize {
        let object = self
            .object
            .as_ref()
            .map_or(0, |object| match &object.members {
                Members::Record(members) => members
                    .values()
                    .map(|(_, shape)| shape.depth() + 1)
                    .max()
                    .unwrap_or(1),
                Members::Collection(value) => value.depth() + 1,
            });
        let array = (self.array.as_ref()).map_or(0, |array| array.element.depth() + 1);
        object.max(array)
    }

    fn render_to(&self, depth: usize) -> Value {
        union(self.alternatives(depth))
    }

    /// A member only some merged objects have: `absent` joins its types, after
    /// the scalar ones, so keys stay exactly as written.
    fn render_member(&self, depth: usize, absent: bool) -> Value {
        let mut alternatives = self.alternatives(depth);
        if absent {
            let scalars = alternatives
                .iter()
                .take_while(|value| value.is_string())
                .count();
            alternatives.insert(scalars, "absent".into());
        }
        union(alternatives)
    }

    fn alternatives(&self, depth: usize) -> Vec<Value> {
        let mut alternatives: Vec<Value> = Scalar::ALL
            .into_iter()
            .filter(|scalar| self.scalars.contains(scalar))
            .map(|scalar| scalar.as_str().into())
            .collect();
        if let Some(object) = &self.object {
            alternatives.push(match depth {
                0 => "{…}".into(),
                depth => object.render(depth - 1),
            });
        }
        if let Some(array) = &self.array {
            alternatives.push(match (array.lengths.max, depth) {
                (0, _) => json!([0]),
                (_, 0) => json!([array.lengths.render(), "…"]),
                (_, depth) => json!([array.lengths.render(), array.element.render_to(depth - 1)]),
            });
        }
        alternatives
    }
}

/// Alternatives read as one type: joined names, or `{"|": [...]}` with a container.
fn union(mut alternatives: Vec<Value>) -> Value {
    match alternatives.len() {
        // Only elements of always-empty arrays have no values.
        0 => "never".into(),
        1 => alternatives.pop().expect("one alternative"),
        _ if alternatives.iter().all(Value::is_string) => alternatives
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|")
            .into(),
        _ => json!({"|": alternatives}),
    }
}

/// Builds one object's shape member by member, holding at most
/// `COLLECTION_MEMBERS` member shapes.
#[derive(Default)]
pub(crate) struct ObjectBuilder {
    count: usize,
    members: Option<IndexMap<String, (usize, Shape)>>,
    collection: Shape,
}

impl ObjectBuilder {
    pub(crate) fn member(&mut self, key: &str, shape: Shape) {
        self.count += 1;
        if self.count > COLLECTION_MEMBERS {
            if let Some(record) = self.members.take() {
                self.collection = collect(record);
            }
            self.collection.merge(shape);
        } else {
            self.members
                .get_or_insert_default()
                .insert(key.to_owned(), (1, shape));
        }
    }

    pub(crate) fn finish(self) -> Shape {
        let members = match self.members {
            Some(record) if self.count <= COLLECTION_MEMBERS => Members::Record(record),
            None if self.count == 0 => Members::Record(IndexMap::new()),
            _ => Members::Collection(self.collection),
        };
        Shape {
            object: Some(Box::new(ObjectShape {
                objects: 1,
                counts: Range::of(self.count),
                members,
            })),
            ..Shape::default()
        }
    }
}

impl ObjectShape {
    fn merge(&mut self, other: Self) {
        self.objects += other.objects;
        self.counts.merge(other.counts);
        let members = std::mem::replace(&mut self.members, Members::Collection(Shape::default()));
        self.members = match (members, other.members) {
            (Members::Record(mut mine), Members::Record(theirs)) => {
                for (key, (present, shape)) in theirs {
                    let entry = mine.entry(key).or_insert_with(|| (0, Shape::default()));
                    entry.0 += present;
                    entry.1.merge(shape);
                }
                if mine.len() > COLLECTION_MEMBERS {
                    Members::Collection(collect(mine))
                } else {
                    Members::Record(mine)
                }
            }
            (Members::Collection(mut value), Members::Record(record))
            | (Members::Record(record), Members::Collection(mut value)) => {
                value.merge(collect(record));
                Members::Collection(value)
            }
            (Members::Collection(mut value), Members::Collection(other)) => {
                value.merge(other);
                Members::Collection(value)
            }
        };
    }

    fn render(&self, depth: usize) -> Value {
        match &self.members {
            Members::Record(members) => Value::Object(
                members
                    .iter()
                    .map(|(key, (present, shape))| {
                        (
                            key.clone(),
                            shape.render_member(depth, *present < self.objects),
                        )
                    })
                    .collect::<Map<_, _>>(),
            ),
            Members::Collection(value) => {
                json!({"*": value.render_to(depth), "#": self.counts.render()})
            }
        }
    }
}

fn collect(record: IndexMap<String, (usize, Shape)>) -> Shape {
    let mut value = Shape::default();
    for (_, (_, shape)) in record {
        value.merge(shape);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_shapes_mark_optional_members_unions_and_length_ranges() {
        let value = json!({"results": [
            {"id": 1, "license": null, "tags": ["a"], "owner": {"login": "x"}, "archived": true},
            {"id": 2, "license": "mit", "tags": [], "owner": null},
            {"id": 3.5, "license": "mit", "tags": ["a", "b", "c"], "owner": {"login": "y"}},
        ], "forks": [[], []]});
        assert_eq!(
            Shape::of(&value).render(usize::MAX),
            json!({
                "results": [3, {
                    "id": "integer|number",
                    "license": "null|string",
                    "tags": ["0..3", "string"],
                    "owner": {"|": ["null", {"login": "string"}]},
                    "archived": "boolean|absent",
                }],
                "forks": [2, [0]],
            })
        );
    }

    #[test]
    fn keys_stay_as_written_when_members_are_optional() {
        let value = json!([{"x": 1, "x?": "a", "owner": {"login": "y"}}, {"x?": "b"}]);
        assert_eq!(
            Shape::of(&value).render(usize::MAX),
            json!([2, {
                "x": "integer|absent",
                "x?": "string",
                "owner": {"|": ["absent", {"login": "string"}]},
            }])
        );
    }

    #[test]
    fn wide_objects_become_collections_and_deep_levels_collapse_within_the_limit() {
        let labels: Map<String, Value> = (0..=COLLECTION_MEMBERS)
            .map(|index| (format!("label-{index}"), json!("v")))
            .collect();
        assert_eq!(
            Shape::of(&json!({"labels": labels})).render(usize::MAX),
            json!({"labels": {"*": "string", "#": COLLECTION_MEMBERS + 1}})
        );
        // Distinct keys across records also collapse once there are too many.
        let records: Vec<Value> = (0..=COLLECTION_MEMBERS)
            .map(|index| json!({ format!("key-{index}"): index }))
            .collect();
        assert_eq!(
            Shape::of(&Value::Array(records)).render(usize::MAX),
            json!([COLLECTION_MEMBERS + 1, {"*": "integer", "#": 1}])
        );
        let wide: Map<String, Value> = (0..COLLECTION_MEMBERS)
            .map(|index| (format!("{index:0>80}"), json!({"nested": [1]})))
            .collect();
        let rendered = Shape::of(&json!({"outer": wide})).render(8 * 1024);
        assert!(serde_json::to_vec(&rendered).unwrap().len() <= 8 * 1024);
        assert_eq!(rendered, json!({"outer": "{…}"}));
    }
}

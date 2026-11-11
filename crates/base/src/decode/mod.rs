//! Decoding configuration: one typed walk over a document's [`Node`]s that refuses each
//! bad value where it is written and goes on, so a run reports every refusal at once.
//!
//! A struct derives [`Decode`] and [`Fields`] with [`derive@Decode`]. A type spelled as a
//! string decodes through its `FromStr` with [`decode_from_str!`]; `NamedEnum` and
//! `string_newtype!` types decode that way already.

use std::fmt;

mod node;
mod std_impls;

pub use node::{Entry, Map, Node, Number, Source, Value};
pub use skyhook_macros::Decode;

/// One step from a value to one within it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Segment {
    Key(String),
    Index(usize),
}

/// The steps from a document's root to a value.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path(pub Vec<Segment>);

/// `providers.local.models[0]`, or `.` at the root.
impl fmt::Display for Path {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return formatter.write_str(".");
        }
        for (index, segment) in self.0.iter().enumerate() {
            match segment {
                Segment::Key(key) if index == 0 => formatter.write_str(key)?,
                Segment::Key(key) => write!(formatter, ".{key}")?,
                Segment::Index(item) => write!(formatter, "[{item}]")?,
            }
        }
        Ok(())
    }
}

/// A refused value: where it is written, its path, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub at: Source,
    pub path: Path,
    pub message: String,
}

impl Diagnostic {
    /// `` `path`: message ``, the path left out at the root.
    pub fn located(&self) -> String {
        match self.path.0.is_empty() {
            true => self.message.clone(),
            false => format!("`{}`: {}", self.path, self.message),
        }
    }
}

/// `` file:line:column: `path`: message ``.
impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.at, self.located())
    }
}

/// Every refusal of a run, by file and then position.
#[derive(Clone, Debug, Default, PartialEq, Eq, thiserror::Error)]
pub struct Diagnostics(pub Vec<Diagnostic>);

/// One refusal per line.
impl fmt::Display for Diagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            diagnostic.fmt(formatter)?;
        }
        Ok(())
    }
}

/// The walk's place in the document and what it has refused.
#[derive(Debug, Default)]
pub struct Context {
    path: Vec<Segment>,
    diagnostics: Vec<Diagnostic>,
}

impl Context {
    /// Refuse `node` at the walk's place.
    pub fn error(&mut self, node: &Node, message: impl fmt::Display) {
        self.report(&node.at, Vec::new(), message);
    }

    /// Refuse the value `path` leads to from `node`, at the nearest value written.
    pub fn error_within(&mut self, node: &Node, path: Vec<Segment>, message: impl fmt::Display) {
        let at = node.locate(&path).at.clone();
        self.report(&at, path, message);
    }

    fn report(&mut self, at: &Source, within: Vec<Segment>, message: impl fmt::Display) {
        let mut path = self.path.clone();
        path.extend(within);
        self.diagnostics.push(Diagnostic {
            at: at.clone(),
            path: Path(path),
            message: message.to_string(),
        });
    }

    /// Refuse `node` as not the kind of value `expected` names.
    pub fn expected(&mut self, node: &Node, expected: &str) {
        self.error(
            node,
            format_args!("expected {expected}, found {}", node.kind()),
        );
    }

    /// Run `decode` one step further into the document.
    pub fn within<R>(&mut self, segment: Segment, decode: impl FnOnce(&mut Self) -> R) -> R {
        self.path.push(segment);
        let decoded = decode(self);
        self.path.pop();
        decoded
    }

    /// How many refusals the walk has made, to tell whether a value decoded cleanly.
    pub fn refusals(&self) -> usize {
        self.diagnostics.len()
    }

    /// Whether anything refused lies on `path` or within it: what `path` names did not
    /// wholly decode, so a check of it would only repeat that.
    pub fn refused_at(&self, path: &[Segment]) -> bool {
        let related = |refused: &[Segment]| refused.starts_with(path) || path.starts_with(refused);
        self.diagnostics
            .iter()
            .any(|diagnostic| related(&diagnostic.path.0))
    }

    /// `value`, unless anything was refused on the way to it.
    pub fn finish<T>(mut self, value: Option<T>) -> Result<T, Diagnostics> {
        match value {
            Some(value) if self.diagnostics.is_empty() => Ok(value),
            _ => {
                self.diagnostics.sort_by(|first, second| {
                    let place = |at: &Source| (at.file.clone(), at.line, at.column);
                    place(&first.at).cmp(&place(&second.at))
                });
                Err(Diagnostics(self.diagnostics))
            }
        }
    }
}

/// A value decoded from a node of a configuration document.
pub trait Decode: Sized {
    /// The value, or `None` once the context holds why not.
    fn decode(node: &Node, cx: &mut Context) -> Option<Self>;

    /// What a field left out decodes as; `None` requires the field.
    fn absent() -> Option<Self> {
        None
    }
}

/// A type read from some of a mapping's keys, which a containing type may share.
pub trait Fields: Sized {
    /// The keys `map` may hold for this type; a tag among them can choose the rest.
    fn keys(map: &Map, keys: &mut Vec<&'static str>);

    /// The value from `map`, the mapping `node` holds. A key this type requires and
    /// `map` lacks is added to `missing`, for the containing mapping to report.
    fn decode_fields(
        node: &Node,
        map: &Map,
        cx: &mut Context,
        missing: &mut Vec<&'static str>,
    ) -> Option<Self>;
}

/// A flattened optional group is present when any of its keys is.
impl<T: Fields> Fields for Option<T> {
    fn keys(map: &Map, keys: &mut Vec<&'static str>) {
        T::keys(map, keys);
    }

    fn decode_fields(
        node: &Node,
        map: &Map,
        cx: &mut Context,
        missing: &mut Vec<&'static str>,
    ) -> Option<Self> {
        let mut keys = Vec::new();
        T::keys(map, &mut keys);
        match keys.iter().any(|key| map.contains(key)) {
            true => T::decode_fields(node, map, cx, missing).map(Some),
            false => Some(None),
        }
    }
}

/// A mapping decoded as `T`: every key it does not take and every key it lacks refused
/// together.
pub fn decode_mapping<T: Fields>(node: &Node, cx: &mut Context) -> Option<T> {
    let Some(map) = node.as_map() else {
        cx.expected(node, "a mapping");
        return None;
    };
    let mut keys = Vec::new();
    T::keys(map, &mut keys);
    let mut seen = Vec::new();
    keys.retain(|key| {
        !seen.contains(key) && {
            seen.push(*key);
            true
        }
    });
    for entry in map
        .iter()
        .filter(|entry| !keys.contains(&entry.key.as_str()))
    {
        let message = format!(
            "unknown field `{}`, expected one of {}",
            entry.key,
            one_of(&keys)
        );
        cx.within(Segment::Key(entry.key.clone()), |cx| {
            cx.report(&entry.at, Vec::new(), message)
        });
    }
    let mut missing = Vec::new();
    let value = T::decode_fields(node, map, cx, &mut missing);
    match missing[..] {
        [] => {}
        [field] => cx.error(node, format_args!("missing required field `{field}`")),
        _ => cx.error(
            node,
            format_args!("missing required fields {}", one_of(&missing)),
        ),
    }
    value.filter(|_| missing.is_empty())
}

/// The entry `key` of `map` decoded as `T`, `T`'s absence when it is left out, or else
/// added to `missing`.
pub fn field<T: Decode>(
    map: &Map,
    key: &'static str,
    cx: &mut Context,
    missing: &mut Vec<&'static str>,
) -> Option<T> {
    field_with(map, key, cx, missing, T::decode)
}

/// [`field`], the entry decoded by `decode` in place of `T`'s own decoding.
pub fn field_with<T: Decode>(
    map: &Map,
    key: &'static str,
    cx: &mut Context,
    missing: &mut Vec<&'static str>,
    decode: impl FnOnce(&Node, &mut Context) -> Option<T>,
) -> Option<T> {
    match map.get(key) {
        Some(node) => cx.within(Segment::Key(key.to_owned()), |cx| decode(node, cx)),
        None => T::absent().or_else(|| {
            missing.push(key);
            None
        }),
    }
}

/// A string `node` parsed as `T`, its refusal reported at the node.
pub fn parse<T: std::str::FromStr<Err: fmt::Display>>(node: &Node, cx: &mut Context) -> Option<T> {
    let Some(text) = node.as_str() else {
        cx.expected(node, "a string");
        return None;
    };
    text.parse().map_err(|error| cx.error(node, error)).ok()
}

/// `node` decoded as the `W` that `T` converts from, a refused conversion reported at it.
pub fn via<W: Decode, T: TryFrom<W, Error: fmt::Display>>(
    node: &Node,
    cx: &mut Context,
) -> Option<T> {
    let written = W::decode(node, cx)?;
    T::try_from(written)
        .map_err(|error| cx.error(node, error))
        .ok()
}

/// A mapping whose only key names which of `names` it holds, the value decoded by
/// `decode` at that key.
pub fn variant<T>(
    node: &Node,
    cx: &mut Context,
    names: &[&str],
    decode: impl FnOnce(&str, &Node, &mut Context) -> Option<T>,
) -> Option<T> {
    let entry = match node.as_map() {
        Some(map) if map.len() == 1 => map.iter().next(),
        _ => None,
    };
    let Some(entry) = entry else {
        let found = match node.as_map() {
            Some(map) => format!("{} keys", map.len()),
            None => node.kind().to_owned(),
        };
        let names = one_of(names);
        cx.error(
            node,
            format_args!("expected a mapping with exactly one of {names}, found {found}"),
        );
        return None;
    };
    cx.within(Segment::Key(entry.key.clone()), |cx| {
        if names.contains(&entry.key.as_str()) {
            return decode(&entry.key, &entry.value, cx);
        }
        let message = format!(
            "unknown variant `{}`, expected one of {}",
            entry.key,
            one_of(names)
        );
        cx.report(&entry.at, Vec::new(), message);
        None
    })
}

/// Names as a refusal lists what it would accept: `` `a`, `b`, `c` ``.
pub fn one_of<T: fmt::Display>(names: impl IntoIterator<Item = T>) -> String {
    let names = names.into_iter().map(|name| format!("`{name}`"));
    names.collect::<Vec<_>>().join(", ")
}

/// What a refused name could have been instead, as its refusal ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected<T>(pub Vec<T>);

impl<T: fmt::Display> fmt::Display for Expected<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.is_empty() {
            true => formatter.write_str("and none is configured"),
            false => write!(formatter, "expected one of {}", one_of(&self.0)),
        }
    }
}

/// Decode each type from a string node through its `FromStr`.
#[macro_export]
macro_rules! decode_from_str {
    ($($type:ty),+ $(,)?) => {$(
        impl $crate::decode::Decode for $type {
            fn decode(
                node: &$crate::decode::Node,
                cx: &mut $crate::decode::Context,
            ) -> ::std::option::Option<Self> {
                $crate::decode::parse(node, cx)
            }
        }
    )+};
}

/// `node` as the JSON value it writes.
pub fn json(node: &Node) -> serde_json::Value {
    match &node.value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(value) => (*value).into(),
        Value::Number(Number::Unsigned(value)) => (*value).into(),
        Value::Number(Number::Signed(value)) => (*value).into(),
        Value::Number(Number::Float(value)) => (*value).into(),
        Value::Str(text) => text.as_str().into(),
        Value::Seq(items) => items.iter().map(json).collect(),
        Value::Map(map) => {
            let entries = map
                .iter()
                .map(|entry| (entry.key.clone(), json(&entry.value)));
            serde_json::Value::Object(entries.collect())
        }
    }
}

/// Decode `$type` as the `$written` it converts from, fallibly or not.
#[macro_export]
macro_rules! decode_via {
    ($type:ty => $written:ty) => {
        impl $crate::decode::Decode for $type {
            fn decode(
                node: &$crate::decode::Node,
                cx: &mut $crate::decode::Context,
            ) -> ::std::option::Option<Self> {
                $crate::decode::via::<$written, Self>(node, cx)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use super::*;

    /// `value` as a document of `file`, each value on the next line in document order.
    fn node(file: &str, value: serde_json::Value) -> Node {
        fn build(value: serde_json::Value, file: &Arc<Path>, line: &mut u64) -> Node {
            *line += 1;
            let at = Source {
                file: Some(file.clone()),
                line: *line,
                column: 1,
            };
            let value = match value {
                serde_json::Value::Null => Value::Null,
                serde_json::Value::Bool(value) => Value::Bool(value),
                serde_json::Value::Number(number) => {
                    Value::Number(match (number.as_u64(), number.as_i64(), number.as_f64()) {
                        (Some(value), ..) => Number::Unsigned(value),
                        (_, Some(value), _) => Number::Signed(value),
                        (.., value) => Number::Float(value.unwrap_or_default()),
                    })
                }
                serde_json::Value::String(text) => Value::Str(text),
                serde_json::Value::Array(items) => Value::Seq(Vec::from_iter(
                    items.into_iter().map(|item| build(item, file, line)),
                )),
                serde_json::Value::Object(entries) => {
                    let mut map = Map::default();
                    for (key, value) in entries {
                        let value = build(value, file, line);
                        let at = value.at.clone();
                        map.insert(Entry { key, at, value });
                    }
                    Value::Map(map)
                }
            };
            Node { value, at }
        }
        build(value, &Path::new(file).into(), &mut 0)
    }

    #[derive(Debug, PartialEq, Decode)]
    struct Limits {
        max_context: std::num::NonZeroU64,
        max_output: std::num::NonZeroU64,
    }

    #[derive(Debug, PartialEq, Decode)]
    #[decode(check = "Model::check")]
    struct Model {
        model: String,
        #[serde(flatten)]
        limits: Limits,
        #[serde(default)]
        tags: Vec<String>,
        hint: Option<String>,
        #[decode(source)]
        at: Source,
    }

    impl Model {
        fn check(&mut self, node: &Node, cx: &mut Context) {
            if self.limits.max_output >= self.limits.max_context {
                let path = vec![Segment::Key("max_output".into())];
                cx.error_within(node, path, "max_output must be smaller than max_context");
            }
        }
    }

    #[derive(Debug, PartialEq, Decode)]
    #[decode(tag = "kind")]
    #[serde(rename_all = "snake_case")]
    enum Shape {
        Square(Side),
        Circle { radius: u8 },
    }

    #[derive(Debug, PartialEq, Decode)]
    struct Side {
        side: u8,
    }

    fn decode<T: Decode>(value: serde_json::Value) -> Result<T, Vec<String>> {
        let mut cx = Context::default();
        let decoded = T::decode(&node("f.yaml", value), &mut cx);
        cx.finish(decoded)
            .map_err(|diagnostics| diagnostics.0.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn mappings_take_their_fields_flattened_groups_defaults_and_where_they_are() {
        let model = decode::<Model>(serde_json::json!({
            "model": "m", "max_context": 8, "max_output": 2,
        }))
        .unwrap();
        assert_eq!(model.limits.max_output.get(), 2);
        assert_eq!((model.tags.len(), model.hint), (0, None));
        assert_eq!(model.at.to_string(), "f.yaml:1:1");
        let shapes = decode::<Vec<Shape>>(serde_json::json!([
            {"kind": "square", "side": 2}, {"kind": "circle", "radius": 3},
        ]));
        assert_eq!(
            shapes.unwrap(),
            [Shape::Square(Side { side: 2 }), Shape::Circle { radius: 3 }]
        );
    }

    /// One run refuses every bad value, each where it is written, in document order.
    #[test]
    fn every_refusal_is_reported_at_its_value() {
        let refused = decode::<Vec<Model>>(serde_json::json!([
            {"model": "m", "max_context": 2, "max_output": 4, "tagz": []},
            {"model": 1, "hint": "h"},
            {"max_context": 4, "max_output": 2, "model": "m", "tags": ["a", 2]},
        ]))
        .unwrap_err();
        let keys = one_of(["model", "max_context", "max_output", "tags", "hint"]);
        assert_eq!(
            refused,
            [
                "f.yaml:5:1: `[0].max_output`: max_output must be smaller than max_context".into(),
                format!("f.yaml:6:1: `[0].tagz`: unknown field `tagz`, expected one of {keys}"),
                "f.yaml:7:1: `[1]`: missing required fields `max_context`, `max_output`".into(),
                "f.yaml:8:1: `[1].model`: expected a string, found a number".into(),
                "f.yaml:16:1: `[2].tags[1]`: expected a string, found a number".into(),
            ]
        );
        let limits = decode::<Model>(serde_json::json!({
            "model": "m", "max_context": 2, "max_output": 4,
        }));
        assert_eq!(
            limits.unwrap_err(),
            ["f.yaml:4:1: `max_output`: max_output must be smaller than max_context"]
        );
    }

    #[test]
    fn a_tag_chooses_the_keys_its_variant_takes() {
        assert_eq!(
            decode::<Shape>(serde_json::json!({"kind": "circle", "side": 1})).unwrap_err(),
            [
                "f.yaml:1:1: missing required field `radius`",
                "f.yaml:3:1: `side`: unknown field `side`, expected one of `kind`, `radius`",
            ]
        );
        assert_eq!(
            decode::<Shape>(serde_json::json!({"kind": "oval"})).unwrap_err(),
            ["f.yaml:2:1: `kind`: unknown variant `oval`, expected one of `square`, `circle`"]
        );
        assert_eq!(
            decode::<Shape>(serde_json::json!({"side": 1})).unwrap_err(),
            ["f.yaml:1:1: missing required field `kind`"]
        );
    }
}

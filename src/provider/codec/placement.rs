//! Where a dialect places a value: dot paths into the body, the one header
//! placement (cache affinity), and the values placed there.

use std::{borrow::Cow, fmt};

use reqwest::header::HeaderName;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::provider::{ProviderError, codec::common};

/// A dot path into a request object, naming where a dialect places a value.
/// Segments are nonempty, without whitespace, and never contain a dot. Presets use [`path`];
/// configuration parses through serde.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BodyPath(Cow<'static, str>);

impl Serialize for BodyPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BodyPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for BodyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A preset path. Evaluated in a `const` block, an invalid path fails the build.
pub(crate) const fn path(path: &'static str) -> BodyPath {
    assert!(
        valid(path),
        "a body path is dot-separated segments without whitespace"
    );
    BodyPath(Cow::Borrowed(path))
}

const fn valid(path: &str) -> bool {
    let bytes = path.as_bytes();
    let (mut index, mut empty) = (0, true);
    while index < bytes.len() {
        match bytes[index] {
            b'.' if empty => return false,
            b'.' => empty = true,
            byte if byte.is_ascii_whitespace() || byte.is_ascii_control() => return false,
            _ => empty = false,
        }
        index += 1;
    }
    !empty
}

/// Whether two paths write into the same place: equal, or one inside the other.
pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long.strip_prefix(short)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a body path is dot-separated segments without whitespace")]
pub(crate) struct PathError;

impl BodyPath {
    pub(crate) fn new(path: impl Into<String>) -> Result<Self, PathError> {
        let path = path.into();
        if !valid(&path) || path.chars().any(char::is_whitespace) {
            return Err(PathError);
        }
        Ok(Self(Cow::Owned(path)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }

    /// Write `value` at this path under `root`, creating missing objects. An
    /// intermediate that is not an object is a request the codec cannot build.
    pub(crate) fn set(
        &self,
        root: &mut Map<String, Value>,
        value: Value,
    ) -> Result<(), ProviderError> {
        let mut segments = self.segments().peekable();
        let mut object = root;
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                object.insert(segment.to_owned(), value);
                return Ok(());
            }
            object = object
                .entry(segment)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| {
                    common::invalid(format!("request field `{}` is not an object", self.0))
                })?;
        }
        Ok(())
    }
}

/// Where the stable context identity travels: the one dimension that may be a
/// header, since some services derive cache affinity from one. Configuration
/// spells it `omitted`, `{header: name}` or `{body: path}`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheKey {
    Omitted,
    Header(#[serde(with = "header_name")] HeaderName),
    Body(BodyPath),
}

/// Where the context identity travels: cache affinity, and user isolation on
/// services that key caches per user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub cache_key: CacheKey,
    pub user_id: Option<BodyPath>,
}

impl Identity {
    pub(crate) const OMITTED: Self = Self {
        cache_key: CacheKey::Omitted,
        user_id: None,
    };
}

/// A header placement.
pub(crate) const fn header(name: &'static str) -> CacheKey {
    CacheKey::Header(HeaderName::from_static(name))
}

mod header_name {
    use reqwest::header::HeaderName;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(name: &HeaderName, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(name.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<HeaderName, D::Error> {
        let name = String::deserialize(deserializer)?;
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| serde::de::Error::custom(format!("`{name}` is not a valid header name")))
    }
}

/// Cache breakpoint lifetime; the hour costs more to write.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// The `cache_control` value marking a prompt-cache breakpoint.
pub(crate) fn breakpoint(ttl: Option<CacheTtl>) -> Value {
    let mut control = json!({"type": "ephemeral"});
    if let Some(ttl) = ttl {
        control["ttl"] = json!(ttl);
    }
    control
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_reject_empty_or_spaced_segments_overlap_by_prefix_and_create_intermediate_objects() {
        for invalid in ["", ".", "a..b", "a.", " .b", "a b", "a.\u{3000}", "a\n"] {
            assert!(BodyPath::new(invalid).is_err(), "{invalid}");
        }
        assert!(overlaps("reasoning", "reasoning.effort"));
        assert!(overlaps("a.b", "a.b"));
        assert!(!overlaps("reasoning.effort", "reasoning.summary"));
        assert!(!overlaps("max_tokens", "max_tokens_x"));
        let mut root = Map::new();
        path("a").set(&mut root, json!(1)).unwrap();
        BodyPath::new("b.c.d")
            .unwrap()
            .set(&mut root, json!("x"))
            .unwrap();
        path("b.c.e").set(&mut root, json!(true)).unwrap();
        assert_eq!(
            Value::Object(root.clone()),
            json!({"a":1, "b":{"c":{"d":"x", "e":true}}})
        );
        let error = path("a.child").set(&mut root, json!(2)).unwrap_err();
        assert_eq!(
            error.kind,
            crate::provider::ProviderErrorKind::InvalidRequest
        );
        assert_eq!(serde_json::to_value(path("x.y")).unwrap(), "x.y");
        assert!(serde_json::from_value::<BodyPath>(json!("x..y")).is_err());
    }
}

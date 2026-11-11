//! Where a dialect places a value: dot paths into the body, the one dimension that
//! may also be a header (cache affinity), and the values placed there.

use std::{borrow::Cow, fmt};

use reqwest::header::HeaderName;
use serde::Serialize;
use serde_json::{Map, Value, json};
use skyhook_base::{
    decode::{Context, Decode, Node},
    enums::NamedEnum,
};

use super::Locations;
use crate::provider::{
    ProviderError,
    ProviderErrorKind::InvalidRequest,
    protocol::{Entry, HistoryLifetime, Message, ModelRequest, sent},
};

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

impl Decode for BodyPath {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        let path = String::decode(node, cx)?;
        Self::new(path).map_err(|error| cx.error(node, error)).ok()
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
                    InvalidRequest.error(format!("request field `{}` is not an object", self.0))
                })?;
        }
        Ok(())
    }
}

/// A place the stable context identity travels: the one dimension that may be a
/// header, since some services derive cache affinity from one. Configuration
/// spells it `{header: name}` or `{body: path}`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheKey {
    Header(#[serde(serialize_with = "header_name::serialize")] HeaderName),
    Body(BodyPath),
}

/// A mapping naming `header` or `body`, and nothing else.
impl Decode for CacheKey {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        let decode = |key: &str, value: &Node, cx: &mut Context| match key {
            "header" => {
                let name = String::decode(value, cx)?;
                let refused =
                    |_| cx.error(value, format_args!("`{name}` is not a valid header name"));
                HeaderName::from_bytes(name.as_bytes())
                    .map(Self::Header)
                    .map_err(refused)
                    .ok()
            }
            _ => BodyPath::decode(value, cx).map(Self::Body),
        };
        skyhook_base::decode::variant(node, cx, &["header", "body"], decode)
    }
}

/// Where the context identity travels: cache affinity, and user isolation on
/// services that key caches per user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub cache_key: Locations<CacheKey>,
    pub user_id: Locations<BodyPath>,
}

impl Identity {
    pub(crate) const NONE: Self = Self {
        cache_key: Locations::NONE,
        user_id: Locations::NONE,
    };
}

/// A header placement.
pub(crate) const fn header(name: &'static str) -> CacheKey {
    CacheKey::Header(HeaderName::from_static(name))
}

mod header_name {
    use reqwest::header::HeaderName;
    use serde::Serializer;

    pub fn serialize<S: Serializer>(name: &HeaderName, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(name.as_str())
    }
}

/// Cache breakpoint lifetime; the hour costs more to write.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, NamedEnum)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

/// Whether the server places a prompt-cache breakpoint of its own besides the
/// request's, or caches only at the request's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cache {
    Automatic,
    /// Sent as `prompt_cache_options`; requests with no breakpoint are not cached.
    Explicit,
    /// The server alone places breakpoints and refuses the request's.
    Server,
}

impl Cache {
    /// The request-wide `prompt_cache_options`, where they depart from the server's default.
    pub(crate) fn options(self) -> Option<Value> {
        match self {
            Self::Automatic | Self::Server => None,
            Self::Explicit => Some(json!({"mode": "explicit"})),
        }
    }
}

/// The `prompt_cache_breakpoint` value marking the end of a reusable prefix.
pub(crate) fn explicit_breakpoint() -> Value {
    json!({"mode": "explicit"})
}

/// The most prompt-cache breakpoints one request may mark, its system prompt's
/// included: the cap each API documents.
const BREAKPOINTS: usize = 4;

/// Push the sent prefix of the request's context and then the rest, marking cache
/// breakpoints at the prefix's end and before each earlier response in it: each
/// earlier request's prefix ended there, and a cache is read back only at a marked
/// boundary. `locate` names the unmarked block a breakpoint would go on, the last
/// text pushed so far, and `mark` marks it. Where [`BREAKPOINTS`] cannot hold every
/// distinct block beside the `system` breakpoints, the oldest go unmarked. A detached
/// history is never reused, so it is not marked, and unless `marked` the server places
/// its own breakpoints.
pub(crate) fn push_history<T, L: PartialEq>(
    target: &mut T,
    request: &ModelRequest,
    marked: bool,
    system: usize,
    mut push: impl FnMut(&mut T, &Entry) -> Result<(), ProviderError>,
    locate: impl Fn(&T) -> Option<L>,
    mut mark: impl FnMut(&mut T, L),
) -> Result<(), ProviderError> {
    let (prefix, rest) = sent(&request.context);
    let marked = marked && request.history_lifetime == HistoryLifetime::Extends;
    let mut seams = Vec::new();
    let mut seam = |target: &T| {
        if let Some(block) = locate(target).filter(|_| marked)
            && !seams.contains(&block)
        {
            seams.push(block);
        }
    };
    for entry in prefix {
        if matches!(entry, Entry::Message(Message::Assistant(_))) {
            seam(target);
        }
        push(target, entry)?;
    }
    seam(target);
    let slots = BREAKPOINTS.saturating_sub(system);
    for block in seams.into_iter().rev().take(slots) {
        mark(target, block);
    }
    rest.into_iter().try_for_each(|entry| push(target, entry))
}

/// Anthropic-style `cache_control`, which some servers take in place of
/// `prompt_cache_breakpoint`; each codec places it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheControl {
    Omitted,
    Sent {
        /// The breakpoint lifetime, where the server takes one.
        ttl: Option<CacheTtl>,
    },
}

impl CacheControl {
    pub(crate) fn ttl_mut(&mut self) -> Option<&mut Option<CacheTtl>> {
        match self {
            Self::Sent { ttl } => Some(ttl),
            Self::Omitted => None,
        }
    }
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
            error.kind(),
            crate::provider::ProviderErrorKind::InvalidRequest
        );
        assert_eq!(serde_json::to_value(path("x.y")).unwrap(), "x.y");
        assert!(crate::tests::decode::<BodyPath>(json!("x..y")).is_err());
    }
}

//! One value written at several places: one location or a list of distinct ones, and
//! where the value may be withheld, none.
//!
//! Configuration spells locations as one location or a list; a single location is
//! written back as itself.

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use skyhook_base::decode::{Context, Decode, Node, Value as DecodedValue};

use super::BodyPath;
use crate::provider::ProviderError;

/// Where a value the request needs is written: one or more distinct locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonEmpty<T> {
    first: T,
    rest: Vec<T>,
}

/// Where a value the request may go without is written: distinct locations, none
/// withholding it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locations<T>(Vec<T>);

impl<T> NonEmpty<T> {
    pub const fn one(location: T) -> Self {
        Self {
            first: location,
            rest: Vec::new(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.first).chain(&self.rest)
    }
}

impl<T: PartialEq> NonEmpty<T> {
    /// `locations` as listed, unless the list is empty or names one twice.
    pub fn new(locations: Vec<T>) -> Result<Self, LocationsError> {
        let repeated = (locations.iter().enumerate())
            .any(|(index, location)| locations[..index].contains(location));
        if repeated {
            return Err(LocationsError::Repeated);
        }
        let mut locations = locations.into_iter();
        let first = locations.next().ok_or(LocationsError::Empty)?;
        Ok(Self {
            first,
            rest: locations.collect(),
        })
    }
}

impl<T> Locations<T> {
    pub(crate) const NONE: Self = Self(Vec::new());

    pub(crate) fn one(location: T) -> Self {
        Self(vec![location])
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// These locations and `location`.
    pub(crate) fn with(mut self, location: T) -> Self {
        self.0.push(location);
        self
    }
}

impl<T> From<NonEmpty<T>> for Locations<T> {
    fn from(locations: NonEmpty<T>) -> Self {
        Self(
            std::iter::once(locations.first)
                .chain(locations.rest)
                .collect(),
        )
    }
}

/// Write `value` at every path; nothing when there are none.
pub(crate) fn set_each<'a>(
    paths: impl IntoIterator<Item = &'a BodyPath>,
    root: &mut Map<String, Value>,
    value: Value,
) -> Result<(), ProviderError> {
    paths
        .into_iter()
        .try_for_each(|path| path.set(root, value.clone()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LocationsError {
    #[error("a list names at least one location")]
    Empty,
    #[error("a location is listed twice")]
    Repeated,
}

impl<T: Serialize> Serialize for NonEmpty<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.rest.is_empty() {
            self.first.serialize(serializer)
        } else {
            serializer.collect_seq(self.iter())
        }
    }
}

/// One location in whatever form the location takes, or a list of distinct ones.
impl<T: Decode + PartialEq> Decode for NonEmpty<T> {
    fn decode(node: &Node, cx: &mut Context) -> Option<Self> {
        match node.value {
            DecodedValue::Seq(_) => {
                let locations = Vec::decode(node, cx)?;
                Self::new(locations)
                    .map_err(|error| cx.error(node, error))
                    .ok()
            }
            _ => T::decode(node, cx).map(Self::one),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::codec::path;

    /// One location or a list of distinct ones, written back as read: a single location
    /// as itself.
    #[test]
    fn locations_read_and_write_one_location_or_a_list() {
        let read = |text: &str| crate::yaml::parse::<NonEmpty<BodyPath>>(text);
        assert_eq!(read("a.b").unwrap(), NonEmpty::one(path("a.b")));
        let both = read("[a, b.c]").unwrap();
        assert_eq!(
            Locations::from(both.clone()),
            Locations::one(path("a")).with(path("b.c"))
        );
        for (locations, text) in [(NonEmpty::one(path("a")), "a"), (both, "- a\n- b.c")] {
            assert_eq!(crate::yaml::to_string(&locations).unwrap().trim(), text);
        }
        for refused in ["[]", "[a, a]", "a b", "null"] {
            assert!(read(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_value_is_written_at_every_selected_path() {
        let mut root = Map::new();
        let paths = Locations::one(path("max_tokens")).with(path("limits.output"));
        set_each(paths.iter(), &mut root, Value::from(16)).unwrap();
        set_each(Locations::NONE.iter(), &mut root, Value::from(true)).unwrap();
        assert_eq!(
            Value::Object(root),
            serde_json::json!({"max_tokens": 16, "limits": {"output": 16}})
        );
    }
}

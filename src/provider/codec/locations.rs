//! One value written at several places: one location or a list of distinct ones, and
//! where the value may be withheld, none.
//!
//! Configuration spells locations as one location or a list; a single location is
//! written back as itself.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, IntoDeserializer, value},
};
use serde_json::{Map, Value};

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

/// `one` and `map` skip the distinctness check: one location cannot repeat, and every
/// `map` caller maps distinct locations to distinct ones.
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

    pub(crate) fn map<U>(self, mut f: impl FnMut(T) -> U) -> NonEmpty<U> {
        NonEmpty {
            first: f(self.first),
            rest: self.rest.into_iter().map(f).collect(),
        }
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
}

impl<T: PartialEq> Locations<T> {
    /// These locations and `location`, unless it is already among them.
    pub(crate) fn with(mut self, location: T) -> Self {
        if !self.0.contains(&location) {
            self.0.push(location);
        }
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

impl<'de, T: Deserialize<'de> + PartialEq> Deserialize<'de> for NonEmpty<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Spelling(std::marker::PhantomData))
    }
}

/// Reads one location in whatever form the location takes, or a list of them. Each
/// location reads through its own type, so its errors stand.
struct Spelling<T>(std::marker::PhantomData<T>);

impl<'de, T: Deserialize<'de> + PartialEq> de::Visitor<'de> for Spelling<T> {
    type Value = NonEmpty<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a location or a list of distinct locations")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        T::deserialize(text.into_deserializer()).map(NonEmpty::one)
    }

    fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        T::deserialize(value::MapAccessDeserializer::new(map)).map(NonEmpty::one)
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        let locations = Vec::<T>::deserialize(value::SeqAccessDeserializer::new(seq))?;
        NonEmpty::new(locations).map_err(de::Error::custom)
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

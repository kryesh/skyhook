//! Integers checked against a range their schema carries.

/// An integer argument from `MIN` through `MAX`, a range its schema carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Bounded<const MIN: u64, const MAX: u64>(u64);

/// A value outside a [`Bounded`] range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("must be {min} through {max}")]
pub struct OutOfRange {
    pub min: u64,
    pub max: u64,
}

impl<const MIN: u64, const MAX: u64> Bounded<MIN, MAX> {
    /// A constant within the range, checked at compile time.
    pub const fn new<const VALUE: u64>() -> Self {
        const { assert!(MIN <= VALUE && VALUE <= MAX) };
        Self(VALUE)
    }
}

impl<const MIN: u64, const MAX: u64> TryFrom<u64> for Bounded<MIN, MAX> {
    type Error = OutOfRange;

    fn try_from(value: u64) -> Result<Self, OutOfRange> {
        (MIN..=MAX)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(OutOfRange { min: MIN, max: MAX })
    }
}

impl<const MIN: u64, const MAX: u64> From<Bounded<MIN, MAX>> for u64 {
    fn from(value: Bounded<MIN, MAX>) -> Self {
        value.0
    }
}

impl<const MIN: u64, const MAX: u64> schemars::JsonSchema for Bounded<MIN, MAX> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Bounded".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "format": "uint64",
            "minimum": MIN,
            "maximum": MAX,
        })
    }
}

/// A timeout in whole seconds, from one second through a day.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct TimeoutSecs(Bounded<1, 86_400>);

impl TimeoutSecs {
    /// A constant within the range, checked at compile time.
    pub const fn new<const SECS: u64>() -> Self {
        Self(Bounded::new::<SECS>())
    }
}

impl From<TimeoutSecs> for std::time::Duration {
    fn from(timeout: TimeoutSecs) -> Self {
        Self::from_secs(timeout.0.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A timeout is a whole number of seconds from one through a day, as its schema says.
    #[test]
    fn timeouts_are_bounded_seconds() {
        let parse = |value| serde_json::from_value::<TimeoutSecs>(serde_json::json!(value));
        assert_eq!(
            std::time::Duration::from(parse(86_400).unwrap()),
            std::time::Duration::from_secs(86_400)
        );
        assert_eq!(parse(1).unwrap(), TimeoutSecs::new::<1>());
        for refused in [0, 86_401] {
            let error = parse(refused).unwrap_err().to_string();
            assert_eq!(error, "must be 1 through 86400");
        }
        let schema = schemars::schema_for!(TimeoutSecs);
        assert_eq!(
            (schema.get("minimum"), schema.get("maximum")),
            (Some(&1.into()), Some(&86_400.into()))
        );
    }
}

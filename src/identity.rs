use std::{fmt, num::NonZeroU64, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// Exactly `N` bytes of hexadecimal in either case.
pub(crate) fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    // Radix parsing alone would accept a leading `+`.
    if value.len() != N * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; N];
    for (slot, pair) in bytes.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

pub(crate) fn write_hex(bytes: &[u8], formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}

/// A 16-byte random identifier displayed and serialized as 32 lowercase hex digits.
macro_rules! random_id {
    ($(#[$meta:meta])* $name:ident, $error:expr) => {
        #[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
        pub struct $name([u8; 16]);

        impl $name {
            $(#[$meta])*
            pub fn generate() -> Result<Self, getrandom::Error> {
                let mut bytes = [0; 16];
                getrandom::fill(&mut bytes)?;
                Ok(Self(bytes))
            }

            #[must_use]
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn to_bytes(self) -> [u8; 16] {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(&self.0, formatter)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode_hex(value).map(Self).ok_or($error)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(de::Error::custom)
            }
        }
    };
}

random_id!(
    /// Creates a cryptographically random session identifier.
    SessionId,
    IdentityError::InvalidSessionId
);
random_id!(
    /// Creates a cryptographically random durable event identifier.
    EventId,
    IdentityError::InvalidEventId
);

#[derive(
    Clone, Debug, Hash, Deserialize, JsonSchema, Serialize, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct AgentId {
    #[schemars(with = "String")]
    session: SessionId,
    path: Vec<u32>,
}

impl AgentId {
    #[must_use]
    pub const fn root(session: SessionId) -> Self {
        Self {
            session,
            path: Vec::new(),
        }
    }

    #[must_use]
    pub fn child(&self, segment: u32) -> Self {
        let mut path = self.path.clone();
        path.push(segment);
        Self {
            session: self.session,
            path,
        }
    }

    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let mut path = self.path.clone();
        path.pop()?;
        Some(Self {
            session: self.session,
            path,
        })
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.path.len()
    }

    #[must_use]
    pub fn path(&self) -> &[u32] {
        &self.path
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.session)?;
        if self.path.is_empty() {
            formatter.write_str(":root")?;
        } else {
            for segment in &self.path {
                write!(formatter, ":{segment}")?;
            }
        }
        Ok(())
    }
}

#[derive(
    Clone, Copy, Debug, Hash, Deserialize, JsonSchema, Serialize, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(transparent)]
pub struct JobId(#[schemars(with = "u64", range(min = 1))] NonZeroU64);

impl JobId {
    pub fn new(value: u64) -> Result<Self, IdentityError> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(IdentityError::ZeroJobId)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("invalid session identifier")]
    InvalidSessionId,
    #[error("invalid event identifier")]
    InvalidEventId,
    #[error("job identifiers must be non-zero")]
    ZeroJobId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_event_ids_are_random_and_round_trip() {
        let event = EventId::generate().unwrap();
        assert_ne!(event, EventId::generate().unwrap());
        assert_eq!(event.to_string().parse::<EventId>().unwrap(), event);
        assert_eq!(
            serde_json::from_str::<EventId>(&serde_json::to_string(&event).unwrap()).unwrap(),
            event
        );
        assert!("0".parse::<EventId>().is_err());
        assert!("z".repeat(32).parse::<EventId>().is_err());
    }

    #[test]
    fn session_id_round_trips() {
        let id = SessionId::from_bytes([0xab; 16]);
        assert_eq!(id.to_string().parse::<SessionId>().unwrap(), id);
        assert_eq!(
            serde_json::from_str::<SessionId>(&serde_json::to_string(&id).unwrap()).unwrap(),
            id
        );
    }
}

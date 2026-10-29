use std::{fmt, num::NonZeroU64, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};
use thiserror::Error;

crate::newtype::string_newtype! {
    /// The application that created a session, independent of its title or mode.
    #[derive(PartialOrd, Ord)]
    pub struct HostName(crate::Blank) = |name| crate::newtype::nonblank("host name", name);
}

/// Exactly `N` bytes of hexadecimal in either case.
pub fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
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

pub fn write_hex(bytes: &[u8], formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    bytes
        .iter()
        .try_for_each(|byte| write!(formatter, "{byte:02x}"))
}

/// A 16-byte random identifier displayed and serialized as 32 lowercase hex digits.
macro_rules! random_id {
    ($(#[$meta:meta])* $name:ident, $error:expr) => {
        #[derive(
            Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
        )]
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

/// An agent of a session, written as its text everywhere: `<session>:root` for the root,
/// `<session>:<segment>:…` below it.
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct AgentId {
    session: SessionId,
    path: Vec<u32>,
}

impl JsonSchema for AgentId {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AgentId".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({"type": "string"})
    }
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

    pub fn parent(&self) -> Option<Self> {
        let mut path = self.path.clone();
        path.pop()?;
        Some(Self {
            session: self.session,
            path,
        })
    }

    pub const fn session(&self) -> SessionId {
        self.session
    }

    pub fn depth(&self) -> usize {
        self.path.len()
    }

    pub fn path(&self) -> &[u32] {
        &self.path
    }

    /// Whether this agent is `root` or one of its descendants.
    pub fn is_within(&self, root: &Self) -> bool {
        self.session == root.session && self.path.starts_with(&root.path)
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

impl FromStr for AgentId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || IdentityError::InvalidAgentId;
        let (session, path) = value.split_once(':').ok_or_else(invalid)?;
        let session = session.parse().map_err(|_| invalid())?;
        let path = match path {
            "root" => Vec::new(),
            path => path
                .split(':')
                .map(|segment| segment.parse().map_err(|_| invalid()))
                .collect::<Result<_, _>>()?,
        };
        Ok(Self { session, path })
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
    #[error("invalid agent identifier")]
    InvalidAgentId,
    #[error("job identifiers must be non-zero")]
    ZeroJobId,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Session and event identifiers share one generator and one text form.
    #[test]
    fn random_ids_are_random_and_round_trip() {
        let event = EventId::generate().unwrap();
        assert_ne!(event, EventId::generate().unwrap());
        let session = SessionId::from_bytes([0xab; 16]);
        assert_eq!(event.to_string().parse::<EventId>().unwrap(), event);
        assert_eq!(
            serde_json::from_str::<SessionId>(&serde_json::to_string(&session).unwrap()).unwrap(),
            session
        );
        assert!("0".parse::<EventId>().is_err());
        assert!("z".repeat(32).parse::<SessionId>().is_err());
        for agent in [
            AgentId::root(session),
            AgentId::root(session).child(1).child(2),
        ] {
            assert_eq!(agent.to_string().parse::<AgentId>().unwrap(), agent);
        }
        assert!(format!("{session}").parse::<AgentId>().is_err());
        assert!(format!("{session}:x").parse::<AgentId>().is_err());
    }

    #[test]
    fn subtrees_include_their_root_and_stay_in_one_session() {
        let root = AgentId::root(SessionId::from_bytes([1; 16]));
        let child = root.child(1);
        let grandchild = child.child(2);
        assert!(root.is_within(&root));
        assert!(grandchild.is_within(&root) && grandchild.is_within(&child));
        assert!(!child.is_within(&grandchild));
        assert!(!root.child(2).is_within(&child));
        assert!(
            !AgentId::root(SessionId::from_bytes([2; 16]))
                .child(1)
                .is_within(&root)
        );
    }
}

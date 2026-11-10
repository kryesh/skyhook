use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde_with::{DeserializeFromStr, SerializeDisplay};
use skyhook_base::hex::Digest;
use thiserror::Error;

skyhook_base::string_newtype! {
    /// The application that created a session, independent of its title or mode.
    #[derive(PartialOrd, Ord)]
    pub struct HostName(skyhook_base::newtype::Blank) = |name| skyhook_base::newtype::nonblank("host name", name);
}

/// A session's 16-byte random identifier, displayed and serialized as 32 lowercase hex digits.
#[derive(
    Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, SerializeDisplay, DeserializeFromStr,
)]
pub struct SessionId(Digest<16>);

impl SessionId {
    /// Creates a cryptographically random session identifier.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self::from_bytes(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Digest::from_bytes(bytes))
    }

    pub const fn to_bytes(self) -> [u8; 16] {
        *self.0.as_bytes()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

impl FromStr for SessionId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Digest::from_hex(value)
            .map(Self)
            .ok_or(IdentityError::InvalidSessionId)
    }
}

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

    /// `root`, or its child indexes such as `1:2`.
    #[must_use]
    pub fn label(&self) -> String {
        if self.path.is_empty() {
            return "root".into();
        }
        let path: Vec<_> = self.path.iter().map(u32::to_string).collect();
        path.join(":")
    }

    /// Its name when no job named it.
    #[must_use]
    pub fn default_name(&self) -> String {
        if self.path.is_empty() {
            return "root".into();
        }
        format!("agent {}", self.label())
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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("invalid session identifier")]
    InvalidSessionId,
    #[error("invalid agent identifier")]
    InvalidAgentId,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_round_trip() {
        let session = SessionId::from_bytes([0xab; 16]);
        assert_eq!(
            serde_json::from_str::<SessionId>(&serde_json::to_string(&session).unwrap()).unwrap(),
            session
        );
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

//! Typed execution targets, configuration, and routing.

mod config;
mod registry;
mod router;

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::newtype::string_newtype;

pub use config::{SshAuth, SshOptions, TargetAuth, TargetConfig, TargetsConfig, Transport};
pub use registry::{
    TargetDefinition, TargetEdge, TargetError, TargetRecord, TargetRegistry, TargetSource,
};
pub(crate) use router::{ResolvedRoute, RouteIdentity, TargetRouter};

string_newtype! {
    /// The name of a configured or session-added target: 1 to 128 ASCII letters,
    /// digits, underscores, hyphens, or periods, and never the reserved `root`.
    #[derive(PartialOrd, Ord)]
    pub struct TargetName(TargetError) = |name| {
        if name == TargetRef::ROOT {
            return Err(TargetError::ReservedName);
        }
        let valid = !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        valid
            .then_some(())
            .ok_or_else(|| TargetError::InvalidName(name.to_owned()))
    };
}

/// An execution target: the session host running Skyhook, or a named target.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
pub enum TargetRef {
    Root,
    Named(TargetName),
}

impl TargetRef {
    /// The spelling of the session host wherever targets are named by text.
    const ROOT: &'static str = "root";

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Root => Self::ROOT,
            Self::Named(name) => name.as_str(),
        }
    }

    #[must_use]
    pub fn name(&self) -> Option<&TargetName> {
        match self {
            Self::Root => None,
            Self::Named(name) => Some(name),
        }
    }
}

impl From<TargetName> for TargetRef {
    fn from(name: TargetName) -> Self {
        Self::Named(name)
    }
}

impl TryFrom<String> for TargetRef {
    type Error = TargetError;

    fn try_from(target: String) -> Result<Self, TargetError> {
        if target == Self::ROOT {
            Ok(Self::Root)
        } else {
            TargetName::try_from(target).map(Self::Named)
        }
    }
}

impl FromStr for TargetRef {
    type Err = TargetError;

    fn from_str(target: &str) -> Result<Self, TargetError> {
        Self::try_from(target.to_owned())
    }
}

impl From<TargetRef> for String {
    fn from(target: TargetRef) -> Self {
        match target {
            TargetRef::Root => TargetRef::ROOT.to_owned(),
            TargetRef::Named(name) => name.into(),
        }
    }
}

impl fmt::Display for TargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated_and_root_is_reserved() {
        assert_eq!("root".parse::<TargetRef>().unwrap(), TargetRef::Root);
        let named = "build.host-1_a".parse::<TargetRef>().unwrap();
        assert_eq!(named.as_str(), "build.host-1_a");
        assert!(matches!(
            "root".parse::<TargetName>(),
            Err(TargetError::ReservedName)
        ));
        for invalid in ["", "has space", "ünïcode", &"x".repeat(129)] {
            assert!(
                matches!(
                    invalid.parse::<TargetName>(),
                    Err(TargetError::InvalidName(_))
                ),
                "{invalid:?}"
            );
            assert!(invalid.parse::<TargetRef>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            serde_json::to_value([TargetRef::Root, named.clone()]).unwrap(),
            serde_json::json!(["root", "build.host-1_a"])
        );
        assert!(serde_json::from_value::<TargetName>(serde_json::json!("root")).is_err());
        assert_eq!(
            serde_json::from_value::<TargetRef>(serde_json::json!("root")).unwrap(),
            TargetRef::Root
        );
    }
}

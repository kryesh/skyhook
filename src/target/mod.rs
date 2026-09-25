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
pub(crate) use router::{ResolvedRoute, RouteIdentity, TargetRouter, select_location};

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

/// A path on an execution target, as tool arguments and results name one. An
/// omitted target is the caller's own location; a named one resolves with the
/// same rules as a tool's `target` argument. The `target` property is offered
/// only with target selection, so schemas add it where that capability applies.
#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetPath {
    /// File path.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub target: Option<TargetRef>,
}

impl TargetPath {
    /// Where schemas define this type.
    pub(crate) const SCHEMA: &'static str = "/$defs/TargetPath";

    /// The `target` property schemas add where the caller can select targets.
    /// It names the host holding a path, which need not be where the operation
    /// runs, so its wording differs from [`TargetRef::schema`].
    pub(crate) fn target_schema() -> serde_json::Value {
        serde_json::json!({
            "type": ["string", "null"],
            "description": "Target holding the path; omitted means your own."
        })
    }
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

    /// The `target` property through which a caller that can select targets
    /// chooses where an operation runs; omitted, it runs where the caller does.
    pub(crate) fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": ["string", "null"],
            "description": "Execution target."
        })
    }

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

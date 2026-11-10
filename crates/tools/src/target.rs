//! The targets a call names: where it runs, and the paths its arguments and results
//! name on them.

use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skyhook_base::string_newtype;
use skyhook_worker::op::TargetPath;

pub const MAX_TARGET_NAME_BYTES: usize = 128;

string_newtype! {
    /// The name of a configured or session-added target
    #[derive(PartialOrd, Ord)]
    pub struct TargetName(TargetNameError) = |name| {
        if name == TargetName::RESERVED {
            return Err(TargetNameError::Reserved);
        }
        let valid = !name.is_empty()
            && name.len() <= MAX_TARGET_NAME_BYTES
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        valid
            .then_some(())
            .ok_or(TargetNameError::Invalid)
    }, schema = schemars::json_schema!({
        "type": "string",
        "pattern": format!("^[A-Za-z0-9_.-]{{1,{MAX_TARGET_NAME_BYTES}}}$"),
    });
}

impl TargetName {
    /// The spelling of the session host wherever targets are named by text, which
    /// no target takes.
    pub const RESERVED: &'static str = "root";
}

/// Why text names no target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TargetNameError {
    #[error("target name `root` is reserved for the Skyhook session host; choose a different name")]
    Reserved,
    #[error(
        "target name must contain 1 to {MAX_TARGET_NAME_BYTES} ASCII letters, digits, underscores, hyphens, or periods"
    )]
    Invalid,
}

// An execution target: the session host running Skyhook, or a named target. A doc
// comment would describe it, where its schema is exactly a string's.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub enum TargetRef {
    Root,
    Named(TargetName),
}

impl TargetRef {
    /// The `target` property through which a caller that can select targets
    /// chooses where an operation runs; omitted, it runs where the caller does.
    pub fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": ["string", "null"],
            "description": "Execution target"
        })
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Root => TargetName::RESERVED,
            Self::Named(name) => name.as_str(),
        }
    }

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
    type Error = TargetNameError;

    fn try_from(target: String) -> Result<Self, TargetNameError> {
        if target == TargetName::RESERVED {
            Ok(Self::Root)
        } else {
            TargetName::try_from(target).map(Self::Named)
        }
    }
}

impl FromStr for TargetRef {
    type Err = TargetNameError;

    fn from_str(target: &str) -> Result<Self, TargetNameError> {
        Self::try_from(target.to_owned())
    }
}

impl From<TargetRef> for String {
    fn from(target: TargetRef) -> Self {
        match target {
            TargetRef::Root => TargetName::RESERVED.to_owned(),
            TargetRef::Named(name) => name.into(),
        }
    }
}

impl fmt::Display for TargetRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical execution target and workspace.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct ExecutionLocation {
    pub target: TargetRef,
    pub workspace: TargetPath,
}

impl ExecutionLocation {
    #[must_use]
    pub fn root(workspace: TargetPath) -> Self {
        Self {
            target: TargetRef::Root,
            workspace,
        }
    }

    #[must_use]
    pub fn named(target: TargetName, workspace: TargetPath) -> Self {
        Self {
            target: TargetRef::Named(target),
            workspace,
        }
    }

    pub fn is_root(&self) -> bool {
        self.target == TargetRef::Root
    }
}

// A path on an execution target, as tool arguments and results name one. An
// omitted target is the caller's own location; a named one resolves with the
// same rules as a tool's `target` argument. The `target` property is offered
// only with target selection, so schemas add it where that capability applies.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TargetPath")]
pub struct TargetedPath {
    /// File path
    pub path: TargetPath,
    #[serde(default)]
    #[schemars(skip)]
    pub target: Option<TargetRef>,
}

impl TargetedPath {
    /// Where schemas define this type.
    pub const SCHEMA: &'static str = "/$defs/TargetPath";

    /// The `target` property schemas add where the caller can select targets.
    /// It names the host holding a path, which need not be where the operation
    /// runs, so its wording differs from [`TargetRef::schema`]. Arguments also
    /// accept null for an omitted target; results omit it.
    pub fn target_schema(contract: &schemars::generate::Contract) -> serde_json::Value {
        let types = if contract.is_deserialize() {
            serde_json::json!(["string", "null"])
        } else {
            serde_json::json!("string")
        };
        serde_json::json!({
            "type": types,
            "description": "Target holding the path; omitted means your own"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_reserved_and_names_round_trip() {
        assert_eq!("root".parse::<TargetRef>().unwrap(), TargetRef::Root);
        let named = "build.host-1_a".parse::<TargetRef>().unwrap();
        assert_eq!(named.as_str(), "build.host-1_a");
        assert_eq!("root".parse::<TargetName>(), Err(TargetNameError::Reserved));
        for invalid in [
            "",
            "has space",
            "ünïcode",
            &"x".repeat(MAX_TARGET_NAME_BYTES + 1),
        ] {
            assert_eq!(
                invalid.parse::<TargetRef>(),
                Err(TargetNameError::Invalid),
                "{invalid:?}"
            );
        }
        assert!(serde_json::from_value::<TargetName>(serde_json::json!("root")).is_err());
        assert_eq!(
            serde_json::to_value([TargetRef::Root, named]).unwrap(),
            serde_json::json!(["root", "build.host-1_a"])
        );
        assert_eq!(
            serde_json::from_value::<TargetRef>(serde_json::json!("root")).unwrap(),
            TargetRef::Root
        );
    }
}

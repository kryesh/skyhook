//! Shared execution-location identity.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::target::{ROOT_TARGET, TargetDefinition};

/// Canonical execution target and workspace.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq, Hash)]
pub struct ExecutionLocation {
    pub target: String,
    #[serde(with = "native_path")]
    #[schemars(with = "String")]
    pub workspace: PathBuf,
}

/// A Unicode workspace keeps its plain string spelling. A Unix workspace that is
/// not Unicode is journaled as its native bytes instead of failing the record;
/// text boundaries (permissions, remote frames) still reject such a path.
mod native_path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{
        borrow::Cow,
        path::{Path, PathBuf},
    };

    #[derive(Deserialize, Serialize)]
    #[serde(untagged)]
    enum Spelling<'a> {
        Text(Cow<'a, str>),
        Native { native_bytes: Vec<u8> },
    }

    pub(super) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => Spelling::Text(Cow::Borrowed(text)),
            #[cfg(unix)]
            None => Spelling::Native {
                native_bytes: std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec(),
            },
            #[cfg(not(unix))]
            None => {
                return Err(serde::ser::Error::custom(
                    "path contains invalid UTF-8 characters",
                ));
            }
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        match Spelling::deserialize(deserializer)? {
            Spelling::Text(text) => Ok(PathBuf::from(text.into_owned())),
            #[cfg(unix)]
            Spelling::Native { native_bytes } => Ok(PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(native_bytes),
            )),
            #[cfg(not(unix))]
            Spelling::Native { .. } => Err(serde::de::Error::custom(
                "native Unix path bytes are unsupported on this platform",
            )),
        }
    }
}

/// A location choice whose named target and workspace come from one resolved definition.
///
/// Definition loading remains permissive; callers resolve/validate target existence before
/// choosing `Other`. This is not evidence of continued registry freshness.
pub(crate) enum LocationSelection<'a> {
    Inherit,
    Root,
    Other(&'a TargetDefinition),
}

impl ExecutionLocation {
    pub(crate) fn select(
        caller: &Self,
        root_workspace: &Path,
        selection: LocationSelection<'_>,
    ) -> Self {
        match selection {
            LocationSelection::Inherit => caller.clone(),
            LocationSelection::Root => Self::root(root_workspace.to_owned()),
            LocationSelection::Other(target) if target.name == caller.target => caller.clone(),
            LocationSelection::Other(target) => Self::named(&target.name, target.workspace.clone()),
        }
    }

    #[must_use]
    pub fn root(workspace: PathBuf) -> Self {
        Self {
            target: ROOT_TARGET.to_owned(),
            workspace,
        }
    }

    #[must_use]
    pub fn named(target: impl Into<String>, workspace: PathBuf) -> Self {
        Self {
            target: target.into(),
            workspace,
        }
    }

    pub fn is_root(&self) -> bool {
        self.target == ROOT_TARGET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_resolves_inherit_root_and_other_targets() {
        let root = Path::new("/root");
        let build = ExecutionLocation::named("build", "/caller-override".into());
        let overridden = ExecutionLocation::root("/override".into());
        let current = TargetDefinition::test("build", "/configured", None);
        let other = TargetDefinition::test("other", "relative directory/../project", None);
        for (caller, selection, expected) in [
            (&build, LocationSelection::Inherit, build.clone()),
            (&overridden, LocationSelection::Inherit, overridden.clone()),
            (
                &build,
                LocationSelection::Root,
                ExecutionLocation::root("/root".into()),
            ),
            (
                &overridden,
                LocationSelection::Root,
                ExecutionLocation::root("/root".into()),
            ),
            // An explicit current target keeps the caller's workspace override.
            (&build, LocationSelection::Other(&current), build.clone()),
            (
                &build,
                LocationSelection::Other(&other),
                ExecutionLocation::named("other", "relative directory/../project".into()),
            ),
        ] {
            assert_eq!(ExecutionLocation::select(caller, root, selection), expected);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let workspace =
                PathBuf::from(std::ffi::OsString::from_vec(b"/workspace/\xff".to_vec()));
            let target = TargetDefinition::test("other", workspace.clone(), None);
            let selected =
                ExecutionLocation::select(&overridden, root, LocationSelection::Other(&target));
            assert_eq!(selected.workspace, workspace);
            let journaled = serde_json::to_value(&selected).unwrap();
            assert_eq!(
                journaled["workspace"],
                serde_json::json!({"native_bytes": b"/workspace/\xff"})
            );
            let restored: ExecutionLocation = serde_json::from_value(journaled).unwrap();
            assert_eq!(restored, selected);
        }
        let unicode = serde_json::to_value(&build).unwrap();
        assert_eq!(
            unicode,
            serde_json::json!({"target":"build","workspace":"/caller-override"})
        );
        assert_eq!(
            serde_json::from_value::<ExecutionLocation>(unicode).unwrap(),
            build
        );
    }
}

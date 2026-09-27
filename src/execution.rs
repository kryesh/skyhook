//! Shared execution-location identity.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::target::{TargetName, TargetRef};

/// Canonical execution target and workspace.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq, Eq, Hash)]
pub struct ExecutionLocation {
    #[schemars(with = "String")]
    pub target: TargetRef,
    #[serde(with = "native_path")]
    #[schemars(with = "String")]
    pub workspace: PathBuf,
}

/// A path's native bytes, for storage that is not text.
#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec()
}

#[cfg(not(unix))]
pub(crate) fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// The path spelled by native `bytes`, the inverse of [`path_bytes`].
#[cfg(unix)]
pub(crate) fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(<std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes))
}

#[cfg(not(unix))]
pub(crate) fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

/// A Unicode workspace keeps its plain string spelling. A Unix workspace that is
/// not Unicode is journaled as its native bytes instead of failing the record;
/// text boundaries (permissions, remote frames) still reject such a path.
pub(crate) mod native_path {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::{
        borrow::Cow,
        path::{Path, PathBuf},
    };

    #[derive(Deserialize, Serialize)]
    #[serde(untagged)]
    enum Spelling<'a> {
        Text(Cow<'a, str>),
        Native {
            #[serde(with = "serde_bytes")]
            native_bytes: Vec<u8>,
        },
    }

    pub(crate) fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => Spelling::Text(Cow::Borrowed(text)),
            #[cfg(unix)]
            None => Spelling::Native {
                native_bytes: super::path_bytes(path),
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

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        match Spelling::deserialize(deserializer)? {
            Spelling::Text(text) => Ok(PathBuf::from(text.into_owned())),
            #[cfg(unix)]
            Spelling::Native { native_bytes } => Ok(super::path_from_bytes(native_bytes)),
            #[cfg(not(unix))]
            Spelling::Native { .. } => Err(serde::de::Error::custom(
                "native Unix path bytes are unsupported on this platform",
            )),
        }
    }
}

impl ExecutionLocation {
    #[must_use]
    pub fn root(workspace: PathBuf) -> Self {
        Self {
            target: TargetRef::Root,
            workspace,
        }
    }

    #[must_use]
    pub fn named(target: TargetName, workspace: PathBuf) -> Self {
        Self {
            target: TargetRef::Named(target),
            workspace,
        }
    }

    pub fn is_root(&self) -> bool {
        self.target == TargetRef::Root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_journal_as_text_or_native_bytes() {
        let build = ExecutionLocation::named("build".parse().unwrap(), "/caller-override".into());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let workspace =
                PathBuf::from(std::ffi::OsString::from_vec(b"/workspace/\xff".to_vec()));
            let selected = ExecutionLocation::named("other".parse().unwrap(), workspace);
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

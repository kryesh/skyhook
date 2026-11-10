//! Path resolution: every path a call names, resolved where it runs before the
//! call is authorized. The host treats a target's paths as opaque text whose
//! components only the target supplies.
use serde::{Deserialize, Serialize};

use crate::diagnostic::Failure;

/// A path as its target spells it: text the host never parses, whose components
/// only the target supplies.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct TargetPath(String);

impl TargetPath {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `os` reads this path from its root rather than a working directory: on
    /// unix from `/`; on Windows from a drive's root (`C:\`, `C:/`) or two separators
    /// (UNC and device paths), so neither `C:pw` nor `\pw` is.
    pub fn is_absolute(&self, os: crate::target::Os) -> bool {
        let separator = |byte: &u8| matches!(byte, b'/' | b'\\');
        match (os, self.0.as_bytes()) {
            (crate::target::Os::Linux, path) => path.starts_with(b"/"),
            (_, [drive, b':', root, ..]) => drive.is_ascii_alphabetic() && separator(root),
            (_, [first, second, ..]) => separator(first) && separator(second),
            _ => false,
        }
    }

    /// What follows the leading `~` of a path `os` reads from the home directory, from
    /// its separator on: `/` on unix, and `/` or `\` on Windows. It is appended to the
    /// home directory's own text, so nothing in it replaces that directory with a root.
    pub(crate) fn home_relative(&self, os: crate::target::Os) -> Option<&str> {
        let separators: &[char] = match os {
            crate::target::Os::Linux => &['/'],
            crate::target::Os::Windows => &['/', '\\'],
        };
        let rest = self.0.strip_prefix('~')?;
        rest.starts_with(separators).then_some(rest)
    }

    pub(crate) fn native(&self) -> &std::path::Path {
        std::path::Path::new(&self.0)
    }
}

impl From<TargetPath> for String {
    fn from(path: TargetPath) -> Self {
        path.0
    }
}

impl From<&str> for TargetPath {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<&TargetPath> for TargetPath {
    fn from(path: &TargetPath) -> Self {
        path.clone()
    }
}

impl From<String> for TargetPath {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl std::fmt::Display for TargetPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Schemas describe a target path as the string it is written as.
impl schemars::JsonSchema for TargetPath {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        String::schema_name()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(generator)
    }
}

/// How a call uses a path, which decides how it resolves.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PathKind {
    Existing,
    /// A file to write, whose missing parent directories the write creates.
    Writable,
    Removable,
    /// An existing directory a process runs in.
    WorkingDirectory,
    /// A directory a job runs in, which inside the authorization root need not
    /// exist yet: a write may create it.
    Workspace,
}

/// The workspace a call's `segments` name on the target, joined in order there, a
/// later absolute segment replacing those before it, as the target resolves it: a
/// relative one is in the home directory, where a login starts.
#[derive(Debug, Deserialize, Serialize)]
pub struct Workspace {
    pub segments: Vec<TargetPath>,
}

/// Resolve `paths` in `workspace`, each as its kind is used.
#[derive(Debug, Deserialize, Serialize)]
pub struct Resolve {
    pub workspace: TargetPath,
    pub paths: Vec<(TargetPath, PathKind)>,
}

/// One requested path: as the request spells it made absolute, `..` kept and no
/// link followed, and what resolving it found.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Resolved {
    pub spelled: TargetPath,
    pub resolution: Result<Resolution, Failure>,
}

impl Resolved {
    /// Put each of `resolved`'s paths in the slot naming it, the path it resolved to
    /// as the call then names it; one that failed leaves its slot.
    pub(crate) fn place<'a>(
        resolved: &[Self],
        slots: impl IntoIterator<Item = &'a mut TargetPath>,
    ) {
        for (slot, resolved) in slots.into_iter().zip(resolved) {
            if let Ok(
                Resolution::Resolved(path)
                | Resolution::MissingWorkspace(path)
                | Resolution::Unresolved(path, _),
            ) = &resolved.resolution
            {
                path.path.clone_into(slot);
            }
        }
    }
}

/// What resolving a path finds.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Resolution {
    Resolved(ResolvedPath),
    /// A job's workspace missing inside the root, which its call creates.
    MissingWorkspace(ResolvedPath),
    /// A path outside the root refused as the path it would be: its failure is
    /// reported only once the call is authorized without asking anyone, since
    /// asking would reveal what lies outside.
    Unresolved(ResolvedPath, Failure),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: TargetPath,
    /// `path`'s components as the target splits it.
    pub components: Vec<String>,
    pub directory: bool,
    /// Whether resolving it left the authorization root.
    pub outside: bool,
}

/// What no removal may destroy.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Protected {
    AuthorizationRoot,
    Workspace,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Os;

    /// A path is absolute, or home-relative, by the rules of the machine that reads it,
    /// whatever the host runs.
    #[test]
    fn paths_are_rooted_by_their_readers_rules() {
        for (path, linux, windows) in [
            ("/etc/pw", true, false),
            ("pw", false, false),
            (r"C:\pw", false, true),
            ("c:/pw", false, true),
            (r"\\server\share\pw", false, true),
            ("//server/share/pw", true, true),
            (r"\\?\C:\pw", false, true),
            ("C:pw", false, false),
            (r"\pw", false, false),
            ("C:", false, false),
        ] {
            let path = TargetPath::new(path);
            assert_eq!(path.is_absolute(Os::Linux), linux, "{path} on linux");
            assert_eq!(path.is_absolute(Os::Windows), windows, "{path} on windows");
        }
        for (path, linux, windows) in [
            ("~/pw", Some("/pw"), Some("/pw")),
            (r"~\pw", None, Some(r"\pw")),
            ("~pw", None, None),
            ("~", None, None),
        ] {
            let path = TargetPath::new(path);
            assert_eq!(path.home_relative(Os::Linux), linux, "{path} on linux");
            assert_eq!(
                path.home_relative(Os::Windows),
                windows,
                "{path} on windows"
            );
        }
    }
}

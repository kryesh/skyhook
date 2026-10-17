//! Path admission: resolve a requested path where the call runs, before authorization.
use std::{
    ffi::OsStr,
    io,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use crate::fs::RegularFileError;
use crate::tool::diagnostic::{Effects, Operation, PartialContext, PathRole, Subject};
use crate::tool::invocation::AdmissionError;
use crate::tool::policy::{Capability, PathText, PermissionUse, ResourceId};
use crate::tool::registry::{PathKind, PathScope};

#[derive(Clone, Debug)]
pub(crate) struct ResolvedWorkspacePath {
    pub path: PathText,
    pub directory: bool,
    /// Whether resolving it left the authorization root.
    outside: bool,
}

impl ResolvedWorkspacePath {
    /// The permission this path needs beyond the authorization root, which its
    /// caller has already authorized as a whole, unless its `scope` always
    /// needs one. It proposes a grant over a directory's descendants or the
    /// exact file.
    pub(crate) fn permission(
        &self,
        capability: Capability,
        scope: PathScope,
        target: &crate::target::TargetRef,
    ) -> Option<PermissionUse> {
        if !self.outside && scope == PathScope::Workspace {
            return None;
        }
        let resource = ResourceId::path(target, &self.path);
        Some(if self.directory {
            PermissionUse::descendants(capability, resource)
        } else {
            PermissionUse::exact(capability, resource)
        })
    }
}

/// What admission makes of a requested path.
pub(crate) enum Resolution {
    Resolved(ResolvedWorkspacePath),
    /// A path outside the root that admission refuses, as the path it would
    /// be. Its failure is reported only once the call is authorized without
    /// asking anyone: asking would reveal what lies outside.
    Unresolved(ResolvedWorkspacePath, AdmissionError),
}

/// Attribute a failure to open or read a requested source file.
pub(crate) fn source_file(path: &Path) -> impl FnOnce(RegularFileError) -> AdmissionError + use<> {
    let subject = Subject::path(path);
    move |error| AdmissionError::from(error).operation(Operation::Read, subject)
}

/// Resolve `input` in `workspace` as the OS does, as the tool will use it: an
/// existing entry, a write target, a job's workspace, or a removal, whose own
/// final link is not followed. A relative input resolves through the
/// workspace, which need not exist yet.
pub(crate) async fn resolve_for_authorization(
    workspace: &Path,
    root: &Path,
    input: &str,
    kind: PathKind,
) -> Result<Resolution, AdmissionError> {
    let workspace =
        std::path::absolute(workspace).map_err(AdmissionError::annotated(PartialContext::new(
            Operation::Canonicalize,
            Subject::working_directory(workspace),
        )))?;
    let joined = lexical_path(&workspace, input)?;
    // A removal names an entry, whose own final link it does not follow,
    // unless the input only names the workspace or a directory holding it.
    let names_entry = kind == PathKind::Removable
        && matches!(
            Path::new(input).components().next_back(),
            Some(Component::Normal(_))
        );
    let root = root.to_owned();
    let resolve = move || resolve(&workspace, &root, &joined, kind, names_entry);
    tokio::task::spawn_blocking(resolve)
        .await
        .map_err(io::Error::other)?
}

fn resolve(
    workspace: &Path,
    root: &Path,
    joined: &Path,
    kind: PathKind,
    names_entry: bool,
) -> Result<Resolution, AdmissionError> {
    let entry = joined
        .parent()
        .zip(joined.file_name())
        .filter(|_| names_entry);
    let trace = trace(root, joined, entry.is_none());
    let resolved = match (kind, entry) {
        (PathKind::Removable, Some((parent, name))) => {
            removable(parent, name, trace.requires_directory)
        }
        (PathKind::Existing | PathKind::WorkingDirectory | PathKind::Removable, _) => {
            existing(joined)
        }
        (PathKind::Writable | PathKind::WritableWithParents | PathKind::Workspace, _) => {
            let creatable = match kind {
                PathKind::Writable => trace.missing == Some(0),
                // Whether a workspace outside the root exists is not disclosed.
                PathKind::Workspace => trace.missing.is_some() && !trace.outside,
                _ => trace.missing.is_some(),
            };
            let mut spelled = joined.to_owned();
            if kind == PathKind::Workspace {
                // A trailing separator: the OS requires a directory.
                spelled.push("");
            }
            match existing(&spelled) {
                Err(error) if error.kind() == io::ErrorKind::NotFound && creatable => {
                    // A write creates a file, which a trailing separator cannot name.
                    if trace.requires_directory && kind != PathKind::Workspace {
                        Err(io::Error::from_raw_os_error(libc::EISDIR))
                    } else {
                        Ok((trace.reached.clone(), false))
                    }
                }
                found => found,
            }
        }
    };
    let (path, directory) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            let subject = match joined.parent() {
                Some(parent) if std::fs::canonicalize(parent).is_err() => {
                    Subject::ParentDirectory(parent.into())
                }
                _ => Subject::path(joined),
            };
            let error = AdmissionError::io(error).operation(Operation::Canonicalize, subject);
            return refuse(&trace.reached, trace.outside, error);
        }
    };
    if kind == PathKind::Removable
        && let Some(protected) = protected(&path, root, workspace)
    {
        let error = AdmissionError::failed(format!(
            "cannot remove {protected} or a directory containing it"
        ));
        let error =
            (error.operation(Operation::Remove, Subject::path(&path))).effects(Effects::Unchanged);
        return refuse(&path, trace.outside, error);
    }
    let outside = trace.outside || !path.starts_with(root);
    match PathText::new(&path) {
        Ok(path) => Ok(Resolution::Resolved(ResolvedWorkspacePath {
            path,
            directory,
            outside,
        })),
        Err(error) => {
            let error = error.or(PartialContext::default().path(PathRole::Resolved, &path));
            refuse(&path, outside, error)
        }
    }
}

fn existing(path: &Path) -> io::Result<(PathBuf, bool)> {
    let path = std::fs::canonicalize(path)?;
    let directory = std::fs::metadata(&path)?.is_dir();
    Ok((path, directory))
}

/// The entry a removal names: its parent resolved, its own final link not.
fn removable(parent: &Path, name: &OsStr, requires_directory: bool) -> io::Result<(PathBuf, bool)> {
    let entry = std::fs::canonicalize(parent)?.join(name);
    let directory = std::fs::symlink_metadata(&entry)?.is_dir();
    if requires_directory && !directory {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    Ok((entry, directory))
}

/// Whether `path` ends in a separator or `.`, which only a directory satisfies.
fn names_directory(path: &Path) -> bool {
    let spelled = path.as_os_str().as_bytes();
    spelled.ends_with(b"/") || spelled.ends_with(b"/.")
}

/// What removing `path` would destroy that no removal may: the authorization
/// root or the workspace, once it exists, whether itself or a directory
/// holding it.
fn protected(path: &Path, root: &Path, workspace: &Path) -> Option<&'static str> {
    if root.starts_with(path) {
        return Some("the authorization root");
    }
    let workspace = std::fs::canonicalize(workspace).ok()?;
    workspace.starts_with(path).then_some("the workspace")
}

/// Fail admission of `path` with `error`, unless resolving it left the root:
/// then defer `error`, authorizing `path` as itself or, when it is not text,
/// as the nearest ancestor that is, whose descendants include it.
fn refuse(path: &Path, outside: bool, error: AdmissionError) -> Result<Resolution, AdmissionError> {
    if !outside {
        return Err(error);
    }
    let spelled = path
        .ancestors()
        .find(|ancestor| ancestor.to_str().is_some());
    let spelled = spelled.unwrap_or(Path::new("/"));
    let would_be = ResolvedWorkspacePath {
        path: PathText::new(spelled)?,
        directory: spelled != path,
        outside: true,
    };
    Ok(Resolution::Unresolved(would_be, error))
}

fn lexical_path(workspace: &Path, relative: &str) -> Result<PathBuf, AdmissionError> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty() {
        return Err(AdmissionError::failed("path cannot be empty")
            .operation(Operation::Validate, Subject::argument(["path"])));
    }
    Ok(workspace.join(path))
}

/// The kernel's limit on symbolic links followed in one resolution.
const MAX_LINKS: u32 = 40;

/// Which entries resolving a path looks up, as the OS reaches them.
struct Trace {
    /// The location reached or, from an entry that failed on, where the names
    /// after it would lead, as far as they only descend.
    reached: PathBuf,
    /// How many names follow the entry that failed, when nothing else does:
    /// a write may create them once the OS finds that entry missing.
    missing: Option<usize>,
    /// Whether any entry looked up lies outside the root and is not one of
    /// its ancestors. The OS's answer about such a path discloses what lies
    /// outside.
    outside: bool,
    /// Whether the final name is spelled, in the path or in the target of a
    /// link it names last, so that only a directory satisfies it.
    requires_directory: bool,
}

/// Follow absolute `path` entry by entry, through every symbolic link (the
/// final one only when `follow_last`), until an entry is missing or fails.
fn trace(root: &Path, path: &Path, follow_last: bool) -> Trace {
    let (mut path, mut outside, mut links) = (path.to_owned(), false, 0);
    let mut requires_directory = names_directory(&path);
    'links: loop {
        let (mut reached, mut directory) = (PathBuf::from("/"), true);
        let mut components = path.components();
        while let Some(component) = components.next() {
            let name = match component {
                Component::Normal(name) => name,
                Component::ParentDir if directory => {
                    reached.pop();
                    continue;
                }
                Component::ParentDir => break,
                _ => continue,
            };
            let entry = reached.join(name);
            outside |= !entry.starts_with(root) && !root.starts_with(&entry);
            let rest = components.as_path();
            let last = rest.components().next().is_none();
            match std::fs::symlink_metadata(&entry) {
                Ok(found) if !found.is_symlink() || (last && !follow_last) => {
                    (reached, directory) = (entry, found.is_dir());
                    continue;
                }
                Ok(_) if links < MAX_LINKS => {
                    if let Ok(target) = std::fs::read_link(&entry) {
                        links += 1;
                        requires_directory |= last && names_directory(&target);
                        path = reached.join(target).join(rest);
                        continue 'links;
                    }
                }
                _ => {}
            }
            // As in the OS, nothing resolves under an entry that fails.
            let (mut reached, mut missing) = (entry, Some(0));
            for component in rest.components() {
                let Component::Normal(name) = component else {
                    missing = None;
                    break;
                };
                reached.push(name);
                missing = missing.map(|names| names + 1);
            }
            return Trace {
                reached,
                missing,
                outside,
                requires_directory,
            };
        }
        return Trace {
            reached,
            missing: None,
            outside,
            requires_directory,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn resolved(workspace: &Path, input: &str, kind: PathKind) -> PathBuf {
        match resolve_for_authorization(workspace, workspace, input, kind).await {
            Ok(Resolution::Resolved(resolved)) => resolved.path.into(),
            _ => panic!("{input} did not resolve"),
        }
    }

    #[tokio::test]
    async fn writable_admission_names_the_attempted_parent_not_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let resolution = resolve_for_authorization(
            root.path(),
            root.path(),
            "missing/file.txt",
            PathKind::Writable,
        );
        let Err(error) = resolution.await else {
            panic!("a missing parent inside the root fails");
        };
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.context.operation, Operation::Canonicalize);
        assert_eq!(
            diagnostic.context.subject,
            Subject::ParentDirectory(root.path().join("missing"))
        );
        assert!(matches!(
            diagnostic.cause,
            crate::tool::diagnostic::Cause::Io {
                kind: crate::tool::diagnostic::IoKind::NotFound,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn traversal_is_consistent_and_workspace_root_stays_protected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();
        std::fs::write(root.path().join("outside"), "data").unwrap();
        let outside = std::fs::canonicalize(root.path().join("outside")).unwrap();
        assert_eq!(
            resolved(&workspace, "../outside", PathKind::Existing).await,
            outside
        );
        assert_eq!(
            resolved(&workspace, outside.to_str().unwrap(), PathKind::Existing).await,
            outside
        );
        assert_eq!(
            resolved(&workspace, "../new", PathKind::Writable).await,
            root.path().join("new")
        );
        assert_eq!(
            resolved(&workspace, "nested/..", PathKind::Existing).await,
            std::fs::canonicalize(&workspace).unwrap()
        );
        std::os::unix::fs::symlink("b", workspace.join("a")).unwrap();
        std::os::unix::fs::symlink("a", workspace.join("b")).unwrap();
        let cycle = resolve_for_authorization(&workspace, &workspace, "a", PathKind::Existing);
        let Err(error) = crate::tests::bounded(cycle).await else {
            panic!("a symbolic link cycle resolves");
        };
        assert!(matches!(
            error.diagnostic().cause,
            crate::tool::diagnostic::Cause::Io {
                code: Some(libc::ELOOP),
                ..
            }
        ));
        // The root and a child workspace in it, and every directory holding
        // either, refuse removal; reached by way of an entry outside the root,
        // only once authorized.
        let inner = workspace.join("nested/inner");
        std::fs::create_dir(&inner).unwrap();
        let detour = root.path().join("detour");
        std::os::unix::fs::symlink(&inner, &detour).unwrap();
        for (from, path, protected) in [
            (&workspace, ".", "authorization root"),
            (&workspace, "nested/..", "authorization root"),
            (&workspace, "../workspace", "authorization root"),
            (&workspace, "/", "authorization root"),
            (&inner, "../..", "authorization root"),
            (&inner, workspace.to_str().unwrap(), "authorization root"),
            (&inner, "..", "workspace"),
            (&inner, ".", "workspace"),
        ] {
            let resolution = resolve_for_authorization(from, &workspace, path, PathKind::Removable);
            let Err(error) = resolution.await else {
                panic!("{path} removes the {protected}");
            };
            assert!(error.to_string().contains(protected), "{path}: {error}");
        }
        for path in [detour.join("../.."), detour.join("../inner")] {
            let input = path.to_str().unwrap();
            let resolution =
                resolve_for_authorization(&inner, &workspace, input, PathKind::Removable);
            assert!(matches!(resolution.await, Ok(Resolution::Unresolved(..))));
        }
        // A relative workspace resolves from where the process runs.
        let crate_root = std::fs::canonicalize("src").unwrap();
        let resolution =
            resolve_for_authorization(Path::new("src"), &crate_root, "lib.rs", PathKind::Existing);
        let Ok(Resolution::Resolved(lib)) = resolution.await else {
            panic!("a relative workspace does not resolve");
        };
        assert_eq!(PathBuf::from(lib.path), crate_root.join("lib.rs"));
    }

    /// Nothing resolves under a missing entry, so a `..` there cannot step
    /// past a link unread; and an outside path whose spelling is not text is
    /// authorized as its nearest ancestor that is, before its failure shows.
    #[tokio::test]
    async fn missing_entries_and_untextual_outside_paths_disclose_nothing() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let untextual = |name: &[u8]| Path::new(std::ffi::OsStr::from_bytes(name)).to_owned();
        let temporary = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temporary.path()).unwrap();
        let (workspace, outside) = (base.join("ws"), base.join("outside"));
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::create_dir(outside.join(untextual(b"dir-\xff"))).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        symlink(&outside, workspace.join("link")).unwrap();
        symlink(untextual(b"\xff/b"), outside.join("nonutf")).unwrap();
        symlink(outside.join("sub"), outside.join("hop")).unwrap();
        symlink(untextual(b"\xff"), outside.join("sub/deep")).unwrap();
        symlink(untextual(b"dir-\xff"), outside.join("named")).unwrap();
        symlink("new-dir/", outside.join("pending")).unwrap();
        for (input, kind, authorized_as) in [
            ("missing/../link/pwned", PathKind::WritableWithParents, None),
            (
                "missing/../../ws/link/pwned",
                PathKind::WritableWithParents,
                None,
            ),
            (
                "missing/../../outside/pwned",
                PathKind::WritableWithParents,
                None,
            ),
            (
                "link/missing/../pwned",
                PathKind::WritableWithParents,
                Some((outside.join("missing"), false)),
            ),
            (
                "link/pending",
                PathKind::WritableWithParents,
                Some((outside.join("new-dir"), false)),
            ),
            (
                "link/nonutf",
                PathKind::Existing,
                Some((outside.clone(), true)),
            ),
            (
                "link/hop/deep",
                PathKind::Existing,
                Some((outside.join("sub"), true)),
            ),
            (
                "link/named",
                PathKind::Existing,
                Some((outside.clone(), true)),
            ),
        ] {
            let resolution = resolve_for_authorization(&workspace, &workspace, input, kind);
            match (resolution.await, authorized_as) {
                (Err(error), None) => assert!(
                    matches!(
                        error.diagnostic().cause,
                        crate::tool::diagnostic::Cause::Io {
                            kind: crate::tool::diagnostic::IoKind::NotFound,
                            ..
                        }
                    ),
                    "{input}: {error:?}"
                ),
                (Ok(Resolution::Unresolved(would_be, _)), Some((path, directory))) => {
                    assert_eq!(
                        (would_be.path.as_path(), would_be.directory),
                        (&*path, directory)
                    );
                }
                (Ok(Resolution::Resolved(resolved)), _) => {
                    panic!("{input} resolved to {:?}", resolved.path);
                }
                (resolution, _) => panic!("{input}: {:?}", resolution.err()),
            }
        }
    }

    /// A trailing separator or `.` requires a directory, as in the kernel,
    /// including one inside a link's target; a write target never is one,
    /// and a workspace always is.
    #[tokio::test]
    async fn trailing_separators_require_a_directory() {
        use std::os::unix::fs::symlink;
        let workspace = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(workspace.path()).unwrap();
        std::fs::write(workspace.join("important"), "data").unwrap();
        std::fs::create_dir(workspace.join("dir")).unwrap();
        symlink("important/", workspace.join("slashed")).unwrap();
        symlink("dir", workspace.join("link")).unwrap();
        symlink("new-dir/", workspace.join("pending")).unwrap();
        symlink("new-dir/.", workspace.join("dotted")).unwrap();
        symlink("pending", workspace.join("chained")).unwrap();
        for (input, kind, code) in [
            ("important/", PathKind::Writable, libc::ENOTDIR),
            ("important/.", PathKind::Existing, libc::ENOTDIR),
            ("slashed", PathKind::Existing, libc::ENOTDIR),
            ("new/", PathKind::Writable, libc::EISDIR),
            ("new/.", PathKind::WritableWithParents, libc::EISDIR),
            ("pending", PathKind::Writable, libc::EISDIR),
            ("pending", PathKind::WritableWithParents, libc::EISDIR),
            ("dotted", PathKind::WritableWithParents, libc::EISDIR),
            ("chained", PathKind::WritableWithParents, libc::EISDIR),
            ("link/", PathKind::Removable, libc::ENOTDIR),
            ("important", PathKind::Workspace, libc::ENOTDIR),
        ] {
            let resolution = resolve_for_authorization(&workspace, &workspace, input, kind);
            let Err(error) = resolution.await else {
                panic!("{input} resolved");
            };
            assert!(
                matches!(
                    error.diagnostic().cause,
                    crate::tool::diagnostic::Cause::Io { code: Some(found), .. } if found == code
                ),
                "{input}: {error:?}"
            );
        }
        assert_eq!(
            resolved(&workspace, "link/.", PathKind::Existing).await,
            workspace.join("dir")
        );
        assert_eq!(
            resolved(&workspace, "new/", PathKind::Workspace).await,
            workspace.join("new")
        );
        assert_eq!(
            resolved(&workspace, "pending", PathKind::Workspace).await,
            workspace.join("new-dir")
        );
    }

    /// A workspace that does not exist yet fails only relative inputs that
    /// need it, and a write that creates its parents creates it.
    #[tokio::test]
    async fn a_missing_workspace_resolves_absolute_paths_and_can_be_created() {
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        let workspace = base.join("new-dir");
        std::fs::write(base.join("existing"), "data").unwrap();
        let existing = base.join("existing");
        let resolution = resolve_for_authorization(
            &workspace,
            &base,
            existing.to_str().unwrap(),
            PathKind::Existing,
        );
        let Ok(Resolution::Resolved(found)) = resolution.await else {
            panic!("an absolute path needs the workspace");
        };
        assert_eq!(PathBuf::from(found.path), existing);
        let resolution =
            resolve_for_authorization(&workspace, &base, "file", PathKind::WritableWithParents);
        let Ok(Resolution::Resolved(created)) = resolution.await else {
            panic!("a write cannot create the workspace");
        };
        assert_eq!(PathBuf::from(created.path), workspace.join("file"));
        let resolution = resolve_for_authorization(&workspace, &base, "file", PathKind::Existing);
        let Err(error) = resolution.await else {
            panic!("a path in a missing workspace resolved");
        };
        assert!(matches!(
            error.diagnostic().cause,
            crate::tool::diagnostic::Cause::Io {
                kind: crate::tool::diagnostic::IoKind::NotFound,
                ..
            }
        ));
    }
}

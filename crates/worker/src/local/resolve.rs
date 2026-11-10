//! Resolve requested paths where the call runs, as the OS would, before the call is
//! authorized.
use std::{
    ffi::OsStr,
    io,
    path::{Component, Path, PathBuf},
};

use crate::diagnostic::{Effects, Failure, Fault, Operation, Subject};
use crate::op::{
    Events, Never, PathKind, Protected, Resolution, Resolve, Resolved, ResolvedPath, TargetPath,
};

impl crate::op::Operation for Resolve {
    type Event = Never;
    type End = Vec<Resolved>;
    type Error = Never;

    async fn perform(self, local: &super::Local, _: Events<Never>) -> Result<Vec<Resolved>, Never> {
        Ok(self.resolve(local).await)
    }
}

impl Resolve {
    /// Resolve each path in the workspace, as its kind is used.
    pub(crate) async fn resolve(self, local: &super::Local) -> Vec<Resolved> {
        let root = local.authorization_root().to_owned();
        skyhook_base::blocking(move || {
            let Resolve { workspace, paths } = self;
            (paths.into_iter())
                .map(|(input, kind)| {
                    let joined = workspace.native().join(input.native());
                    Resolved {
                        spelled: text(&std::path::absolute(&joined).unwrap_or(joined)),
                        resolution: resolve_one(workspace.native(), &root, input.as_str(), kind),
                    }
                })
                .collect()
        })
        .await
    }
}

/// Resolve `input` in `workspace` as the OS does, as the tool will use it: an
/// existing entry, a write target, a job's workspace, or a removal, whose own
/// final link is not followed. A relative input resolves through the
/// workspace, which need not exist yet.
fn resolve_one(
    workspace: &Path,
    root: &Path,
    input: &str,
    kind: PathKind,
) -> Result<Resolution, Failure> {
    let workspace = std::path::absolute(workspace).map_err(|error| {
        let subject = Subject::WorkingDirectory(text(workspace));
        Failure::at(Operation::Canonicalize, subject, error)
    })?;
    if input.is_empty() {
        return Err(Fault::EmptyPath.into());
    }
    let unspelled = |native| unspelled(input, native);
    let joined = crate::fs::plain(workspace.join(input));
    // A removal names an entry, whose own final link it does not follow,
    // unless the input only names the workspace or a directory holding it.
    let names_entry = kind == PathKind::Removable
        && matches!(
            Path::new(input).components().next_back(),
            Some(Component::Normal(_))
        );
    Ok(
        match resolve(&workspace, root, &joined, kind, names_entry, input)? {
            Found::Path(path, directory, outside) => {
                Resolution::Resolved(resolved_path(path, directory, outside).map_err(unspelled)?)
            }
            Found::MissingWorkspace(path) => {
                Resolution::MissingWorkspace(resolved_path(path, false, false).map_err(unspelled)?)
            }
            Found::Refused(path, directory, error) => Resolution::Unresolved(
                resolved_path(path, directory, true).map_err(unspelled)?,
                error,
            ),
        },
    )
}

/// `input` resolved to a path that is not text. The bytes are in the target's
/// encoding, which the host never decodes.
fn unspelled(input: &str, native: Vec<u8>) -> Failure {
    let fault = Fault::Unspelled { native };
    Failure::at(Operation::Canonicalize, Subject::path(input), fault)
}

/// `path` resolved, or its native bytes when it is not text.
fn resolved_path(path: PathBuf, directory: bool, outside: bool) -> Result<ResolvedPath, Vec<u8>> {
    let components = (path.components())
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let path =
        (path.into_os_string().into_string()).map_err(|path| native_bytes(Path::new(&path)))?;
    Ok(ResolvedPath {
        path: TargetPath::new(path),
        components,
        directory,
        outside,
    })
}

/// A path's native bytes: on Windows its UTF-16 code units, little-endian.
fn native_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    return std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
    #[cfg(windows)]
    return (std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()))
        .flat_map(u16::to_le_bytes)
        .collect();
}

/// A path's display; resolved paths that are not text never leave as text.
fn text(path: &Path) -> TargetPath {
    TargetPath::new(path.to_string_lossy())
}

enum Found {
    /// The resolved path, whether it is a directory and whether resolving it
    /// left the authorization root.
    Path(PathBuf, bool, bool),
    MissingWorkspace(PathBuf),
    /// Refused outside the root, as the path it would be and whether that is a
    /// directory (an ancestor standing in for a path that is not text).
    Refused(PathBuf, bool, Failure),
}

fn resolve(
    workspace: &Path,
    root: &Path,
    joined: &Path,
    kind: PathKind,
    names_entry: bool,
    input: &str,
) -> Result<Found, Failure> {
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
        (PathKind::Writable | PathKind::Workspace, _) => {
            let creatable = match kind {
                // Whether a workspace outside the root exists is not disclosed.
                PathKind::Workspace => trace.creatable && !trace.outside,
                _ => trace.creatable,
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
                        Err(io::Error::from_raw_os_error(IS_A_DIRECTORY))
                    } else {
                        Ok((trace.reached.clone(), Entry::Missing))
                    }
                }
                found => found,
            }
        }
    };
    let (path, entry) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            let subject = match joined.parent() {
                Some(parent) if std::fs::canonicalize(parent).is_err() => {
                    Subject::ParentDirectory(text(parent))
                }
                _ => Subject::Path(text(joined)),
            };
            let error = Failure::at(Operation::Canonicalize, subject, error);
            return refuse(&trace.reached, trace.outside, error);
        }
    };
    if kind == PathKind::Removable
        && let Some(protected) = protected(&path, root, workspace)
    {
        let error = Failure::at(
            Operation::Remove,
            Subject::Path(text(&path)),
            Fault::Protected(protected),
        )
        .effects(Effects::Unchanged);
        return refuse(&path, trace.outside, error);
    }
    let outside = trace.outside || !path.starts_with(root);
    if path.to_str().is_none() {
        let native = native_bytes(&path);
        return refuse(&path, outside, unspelled(input, native));
    }
    Ok(match (kind, entry, outside) {
        (PathKind::Workspace, Entry::Missing, false) => Found::MissingWorkspace(path),
        _ => Found::Path(path, entry == Entry::Directory, outside),
    })
}

/// What a resolved path names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entry {
    Directory,
    Other,
    /// Nothing yet: a write creates it.
    Missing,
}

impl Entry {
    const fn found(directory: bool) -> Self {
        if directory {
            Self::Directory
        } else {
            Self::Other
        }
    }
}

fn existing(path: &Path) -> io::Result<(PathBuf, Entry)> {
    let path = crate::fs::canonical(path)?;
    let directory = std::fs::metadata(&path)?.is_dir();
    Ok((path, Entry::found(directory)))
}

/// The entry a removal names: its parent resolved, its own final link not.
fn removable(
    parent: &Path,
    name: &OsStr,
    requires_directory: bool,
) -> io::Result<(PathBuf, Entry)> {
    let entry = crate::fs::canonical(parent)?.join(name);
    let directory = std::fs::symlink_metadata(&entry)?.is_dir();
    if requires_directory && !directory {
        return Err(io::Error::from_raw_os_error(NOT_A_DIRECTORY));
    }
    Ok((entry, Entry::found(directory)))
}

/// Whether `path` ends in a separator or `.`, which only a directory satisfies.
fn names_directory(path: &Path) -> bool {
    let spelled = path.as_os_str().as_encoded_bytes();
    let spelled = spelled.strip_suffix(b".").unwrap_or(spelled);
    (spelled.last()).is_some_and(|&byte| std::path::is_separator(byte.into()))
}

/// The OS's own errors for a file where only a directory will do, and the reverse.
#[cfg(unix)]
const NOT_A_DIRECTORY: i32 = libc::ENOTDIR;
#[cfg(unix)]
const IS_A_DIRECTORY: i32 = libc::EISDIR;
#[cfg(windows)]
const NOT_A_DIRECTORY: i32 = windows_sys::Win32::Foundation::ERROR_DIRECTORY as i32;
#[cfg(windows)]
const IS_A_DIRECTORY: i32 = windows_sys::Win32::Foundation::ERROR_DIRECTORY_NOT_SUPPORTED as i32;

/// What removing `path` would destroy that no removal may: the authorization
/// root or the workspace, once it exists, whether itself or a directory
/// holding it.
fn protected(path: &Path, root: &Path, workspace: &Path) -> Option<Protected> {
    if root.starts_with(path) {
        return Some(Protected::AuthorizationRoot);
    }
    let workspace = crate::fs::canonical(workspace).ok()?;
    workspace.starts_with(path).then_some(Protected::Workspace)
}

/// Fail resolving `path` with `error`, unless resolving it left the root: then
/// defer `error`, the path standing as itself or, when it is not text, as the
/// nearest ancestor that is, whose descendants include it.
fn refuse(path: &Path, outside: bool, error: Failure) -> Result<Found, Failure> {
    if !outside {
        return Err(error);
    }
    let spelled = path
        .ancestors()
        .find(|ancestor| ancestor.to_str().is_some());
    let spelled = spelled.unwrap_or(Path::new("/"));
    Ok(Found::Refused(spelled.to_owned(), spelled != path, error))
}

/// The kernel's limit on symbolic links followed in one resolution.
const MAX_LINKS: u32 = 40;

/// Which entries resolving a path looks up, as the OS reaches them.
struct Trace {
    /// The location reached or, from an entry that failed on, where the names
    /// after it would lead, as far as they only descend.
    reached: PathBuf,
    /// Whether only names follow the entry that failed: a write may create
    /// them once the OS finds that entry missing.
    creatable: bool,
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
        let (mut reached, mut directory) = (PathBuf::new(), true);
        let mut components = path.components();
        while let Some(component) = components.next() {
            let name = match component {
                Component::Normal(name) => name,
                Component::ParentDir if directory => {
                    reached.pop();
                    continue;
                }
                Component::ParentDir => break,
                Component::Prefix(_) | Component::RootDir => {
                    reached.push(component);
                    continue;
                }
                Component::CurDir => continue,
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
                Ok(_)
                    if links < MAX_LINKS
                        && let Ok(target) = std::fs::read_link(&entry) =>
                {
                    let target = crate::fs::plain(target);
                    links += 1;
                    requires_directory |= last && names_directory(&target);
                    path = reached.join(target).join(rest);
                    continue 'links;
                }
                _ => {}
            }
            // As in the OS, nothing resolves under an entry that fails.
            let (mut reached, mut creatable) = (entry, true);
            for component in rest.components() {
                let Component::Normal(name) = component else {
                    creatable = false;
                    break;
                };
                reached.push(name);
            }
            return Trace {
                reached,
                creatable,
                outside,
                requires_directory,
            };
        }
        return Trace {
            reached,
            creatable: false,
            outside,
            requires_directory,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::{IoKind, OsError};

    /// The OS error a failed lookup found.
    fn lookup(result: &Result<Resolution, Failure>) -> Option<&OsError> {
        match result {
            Err(Failure {
                operation: Some((Operation::Canonicalize, _)),
                fault: Fault::Os(error),
                ..
            }) => Some(error),
            _ => None,
        }
    }

    /// What resolving `input` in `workspace`, under `root`, finds.
    async fn resolution(
        workspace: &Path,
        root: &Path,
        input: &str,
        kind: PathKind,
    ) -> Result<Resolution, Failure> {
        let local = super::super::Local::new(root.to_owned());
        let request = Resolve {
            workspace: text(workspace),
            paths: vec![(input.into(), kind)],
        };
        use crate::target::Target as _;
        let Ok(Ok(mut resolved)) = crate::tests::bounded(local.call(request)).await;
        resolved.remove(0).resolution
    }

    async fn resolved(workspace: &Path, input: &str, kind: PathKind) -> PathBuf {
        match resolution(workspace, workspace, input, kind).await {
            Ok(Resolution::Resolved(resolved) | Resolution::MissingWorkspace(resolved)) => {
                PathBuf::from(resolved.path.as_str())
            }
            _ => panic!("{input} did not resolve"),
        }
    }

    #[tokio::test]
    async fn traversal_is_consistent_and_workspace_root_stays_protected() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("nested")).unwrap();
        std::fs::write(root.path().join("outside"), "data").unwrap();
        let outside = crate::fs::canonical(&root.path().join("outside")).unwrap();
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
            crate::fs::canonical(&workspace).unwrap()
        );
        std::os::unix::fs::symlink("b", workspace.join("a")).unwrap();
        std::os::unix::fs::symlink("a", workspace.join("b")).unwrap();
        let cycle = resolution(&workspace, &workspace, "a", PathKind::Existing).await;
        assert_eq!(lookup(&cycle).unwrap().code, Some(libc::ELOOP));
        // The root and a child workspace in it, and every directory holding
        // either, refuse removal; reached by way of an entry outside the root,
        // only once authorized.
        let inner = workspace.join("nested/inner");
        std::fs::create_dir(&inner).unwrap();
        let detour = root.path().join("detour");
        std::os::unix::fs::symlink(&inner, &detour).unwrap();
        for (from, path, expected) in [
            (&workspace, ".", Protected::AuthorizationRoot),
            (&workspace, "nested/..", Protected::AuthorizationRoot),
            (&workspace, "../workspace", Protected::AuthorizationRoot),
            (&workspace, "/", Protected::AuthorizationRoot),
            (&inner, "../..", Protected::AuthorizationRoot),
            (
                &inner,
                workspace.to_str().unwrap(),
                Protected::AuthorizationRoot,
            ),
            (&inner, "..", Protected::Workspace),
            (&inner, ".", Protected::Workspace),
        ] {
            let refused = resolution(from, &workspace, path, PathKind::Removable).await;
            assert!(
                matches!(&refused, Err(Failure { fault: Fault::Protected(protected), .. }) if *protected == expected),
                "{path}: {refused:?}"
            );
        }
        for path in [detour.join("../.."), detour.join("../inner")] {
            let input = path.to_str().unwrap();
            let refused = resolution(&inner, &workspace, input, PathKind::Removable).await;
            assert!(matches!(refused, Ok(Resolution::Unresolved(..))));
        }
        // A relative workspace resolves from where the process runs.
        let crate_root = crate::fs::canonical(Path::new("src")).unwrap();
        let lib = resolution(Path::new("src"), &crate_root, "lib.rs", PathKind::Existing).await;
        let Ok(Resolution::Resolved(lib)) = lib else {
            panic!("a relative workspace does not resolve");
        };
        assert_eq!(PathBuf::from(lib.path.as_str()), crate_root.join("lib.rs"));
    }

    /// Nothing resolves under a missing entry, so a `..` there cannot step
    /// past a link unread; and an outside path whose spelling is not text is
    /// authorized as its nearest ancestor that is, before its failure shows.
    #[tokio::test]
    async fn missing_entries_and_untextual_outside_paths_disclose_nothing() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let untextual = |name: &[u8]| Path::new(std::ffi::OsStr::from_bytes(name)).to_owned();
        let temporary = tempfile::tempdir().unwrap();
        let base = crate::fs::canonical(temporary.path()).unwrap();
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
            ("missing/../link/pwned", PathKind::Writable, None),
            ("missing/../../ws/link/pwned", PathKind::Writable, None),
            ("missing/../../outside/pwned", PathKind::Writable, None),
            (
                "link/missing/../pwned",
                PathKind::Writable,
                Some((outside.join("missing"), false)),
            ),
            (
                "link/pending",
                PathKind::Writable,
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
            match (
                resolution(&workspace, &workspace, input, kind).await,
                authorized_as,
            ) {
                (error @ Err(_), None) => assert!(
                    lookup(&error).is_some_and(|error| error.kind == IoKind::NotFound),
                    "{input}: {error:?}"
                ),
                (Ok(Resolution::Unresolved(would_be, _)), Some((path, directory))) => {
                    assert_eq!(
                        (Path::new(would_be.path.as_str()), would_be.directory),
                        (&*path, directory)
                    );
                }
                (Ok(Resolution::Resolved(resolved)), _) => {
                    panic!("{input} resolved to {:?}", resolved.path);
                }
                (resolution, _) => panic!("{input}: {:?}", resolution.err()),
            }
        }
        // A write names the parent it attempted, not the destination.
        let parent = resolution(
            &workspace,
            &workspace,
            "missing/../file.txt",
            PathKind::Writable,
        );
        let expected = Subject::ParentDirectory(text(&workspace.join("missing/..")));
        let Err(Failure { operation, .. }) = parent.await else {
            panic!("a missing parent resolves");
        };
        assert_eq!(operation, Some((Operation::Canonicalize, expected)));
    }

    /// A trailing separator or `.` requires a directory, as in the kernel,
    /// including one inside a link's target; a write target never is one,
    /// and a workspace always is.
    #[tokio::test]
    async fn trailing_separators_require_a_directory() {
        use std::os::unix::fs::symlink;
        let workspace = tempfile::tempdir().unwrap();
        let workspace = crate::fs::canonical(workspace.path()).unwrap();
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
            ("new/.", PathKind::Writable, libc::EISDIR),
            ("pending", PathKind::Writable, libc::EISDIR),
            ("dotted", PathKind::Writable, libc::EISDIR),
            ("chained", PathKind::Writable, libc::EISDIR),
            ("link/", PathKind::Removable, libc::ENOTDIR),
            ("important", PathKind::Workspace, libc::ENOTDIR),
        ] {
            let refused = resolution(&workspace, &workspace, input, kind).await;
            assert_eq!(
                lookup(&refused).and_then(|error| error.code),
                Some(code),
                "{input}: {refused:?}"
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
    /// need it, and a write creates it.
    #[tokio::test]
    async fn a_missing_workspace_resolves_absolute_paths_and_can_be_created() {
        let base = tempfile::tempdir().unwrap();
        let base = crate::fs::canonical(base.path()).unwrap();
        let workspace = base.join("new-dir");
        std::fs::write(base.join("existing"), "data").unwrap();
        let existing = base.join("existing");
        let found = resolution(
            &workspace,
            &base,
            existing.to_str().unwrap(),
            PathKind::Existing,
        );
        let Ok(Resolution::Resolved(found)) = found.await else {
            panic!("an absolute path needs the workspace");
        };
        assert_eq!(PathBuf::from(found.path.as_str()), existing);
        let created = resolution(&workspace, &base, "file", PathKind::Writable).await;
        let Ok(Resolution::Resolved(created)) = created else {
            panic!("a write cannot create the workspace");
        };
        assert_eq!(PathBuf::from(created.path.as_str()), workspace.join("file"));
        let missing = resolution(&workspace, &base, "file", PathKind::Existing).await;
        assert_eq!(lookup(&missing).unwrap().kind, IoKind::NotFound);
    }
}

//! Path admission: what a target's resolution of a requested path means for the
//! call, its permissions, and how a refused one fails.
use crate::diagnostic::FailureSite;
use crate::error::AdmissionError;
use crate::target::ExecutionLocation;
use skyhook_worker::diagnostic::{Effects, Failure, PathFact, PathRole, Subject};
use skyhook_worker::op::{PathKind, Resolution, Resolve, Resolved, ResolvedPath, TargetPath};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathAccess {
    Read,
    Write,
}

/// Whether a path inside the authorization root needs its own permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathScope {
    /// The tool's workspace scope covers it; outside the root it needs its own.
    Workspace,
    /// It always needs its own, as the tool's capabilities do not cover its access.
    Exact,
}

/// A path the admitted input names. It is resolved where the call runs, and the
/// handler receives its resolved spelling.
pub struct PathArgument<'a> {
    pub(crate) path: &'a mut TargetPath,
    pub(crate) access: PathAccess,
    pub(crate) kind: PathKind,
    pub(crate) scope: PathScope,
}

impl<'a> PathArgument<'a> {
    #[must_use]
    pub fn new(path: &'a mut TargetPath, access: PathAccess, kind: PathKind) -> Self {
        Self {
            path,
            access,
            kind,
            scope: PathScope::Workspace,
        }
    }

    /// The path needs its own permission even inside the authorization root.
    #[must_use]
    pub const fn exact(mut self) -> Self {
        self.scope = PathScope::Exact;
        self
    }
}

/// Whether a job's local workspace exists, as admitting its call found it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JobWorkspace {
    #[default]
    Exists,
    /// Missing inside the root: its call creates it.
    Missing,
}

/// What a path argument asks of the path its target resolved.
pub struct PathNeed {
    pub access: PathAccess,
    pub scope: PathScope,
    pub path: ResolvedPath,
}

/// A call's path arguments as their target resolved them, each spelling replaced
/// by the resolved one.
#[derive(Default)]
pub struct AdmittedPaths {
    /// Whether one names a job's missing workspace.
    pub found: JobWorkspace,
    /// Each path as requested, then as resolved.
    pub facts: Vec<PathFact>,
    pub needs: Vec<PathNeed>,
    /// How the first path outside the root that does not resolve fails, once the
    /// call is authorized without asking anyone.
    pub unresolved: Option<AdmissionError>,
}

/// The request that resolves `arguments` where the invocation runs, unless there
/// are none.
pub fn request(arguments: &[PathArgument<'_>], location: &ExecutionLocation) -> Option<Resolve> {
    (!arguments.is_empty()).then(|| Resolve {
        workspace: location.workspace.clone(),
        paths: (arguments.iter())
            .map(|argument| (argument.path.clone(), argument.kind))
            .collect(),
    })
}

/// Whether `arguments` need no permission of their own once they resolve inside the
/// authorization root, each covered there by its tool's workspace scope.
pub fn workspace_scoped(arguments: &[PathArgument<'_>]) -> bool {
    (arguments.iter()).all(|argument| argument.scope == PathScope::Workspace)
}

/// Apply `resolved`, their target's answer to [`request`], to `arguments`.
pub fn admit(
    arguments: Vec<PathArgument<'_>>,
    resolved: Vec<Resolved>,
    location: &ExecutionLocation,
) -> Result<AdmittedPaths, AdmissionError> {
    let mut admitted = AdmittedPaths::default();
    for (argument, resolved) in arguments.into_iter().zip(resolved) {
        let input = argument.path.clone();
        // Preflight adds what it knows to the resolver's facts; nothing is resolved
        // here, so the tool's own fallback still fills the rest.
        let annotate = |error: Failure| {
            let error = AdmissionError::from(error);
            let mut facts = (error.facts().context.clone())
                .at(FailureSite::Execution(location.clone()))
                .path(PathRole::Requested, &input);
            if matches!(
                argument.kind,
                PathKind::WorkingDirectory | PathKind::Workspace
            ) {
                facts = facts
                    .subject(Subject::WorkingDirectory(resolved.spelled.clone()))
                    .effects(Effects::NotStarted);
            }
            error.context(facts)
        };
        let path = match resolved.resolution.clone().map_err(annotate)? {
            Resolution::Resolved(path) => path,
            Resolution::MissingWorkspace(path) => {
                admitted.found = JobWorkspace::Missing;
                path
            }
            // Later paths resolve as they would had this one existed.
            Resolution::Unresolved(path, error) => {
                admitted.unresolved.get_or_insert_with(|| annotate(error));
                path
            }
        };
        path.path.clone_into(argument.path);
        admitted.facts.extend([
            PathFact {
                role: PathRole::Requested,
                path: input,
            },
            PathFact {
                role: PathRole::Resolved,
                path: path.path.clone(),
            },
        ]);
        admitted.needs.push(PathNeed {
            access: argument.access,
            scope: argument.scope,
            path,
        });
    }
    Ok(admitted)
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, sync::Arc};

    use super::*;
    use crate::diagnostic::{Cause, FailureSite};
    use crate::output::OutputContext;
    use crate::spec::tests::{CapturedOutput, call, run_named};
    use crate::target::ExecutionLocation;
    use skyhook_worker::diagnostic::{Effects, Operation, PathRole, Subject};

    /// A path that does not resolve fails in preflight, before any approval.
    #[tokio::test]
    async fn local_admission_preserves_preflight_facts() {
        let root = tempfile::tempdir().unwrap();
        let output = OutputContext::new(Arc::new(CapturedOutput::default()));
        let local = skyhook_worker::local::Local::new(root.path().to_owned());
        let call = call(root.path(), output.clone(), Default::default());
        let arguments = serde_json::json!({"path":"missing"});
        let missing = run_named(&local, "read", arguments, call)
            .await
            .unwrap_err()
            .diagnostic();
        assert_matches!(
            missing.cause,
            Cause::Io(skyhook_worker::op::OsError {
                kind: skyhook_worker::op::IoKind::NotFound,
                ..
            })
        );
        // Preflight adds only what it knows: the site and the requested path. The
        // resolver's own facts stand, and nothing claims effects it cannot see.
        let facts = missing.context;
        assert_eq!(facts.operation, Operation::Canonicalize);
        let missing = crate::tests::text(root.path().join("missing"));
        assert_eq!(facts.subject, Subject::Path(missing));
        assert_eq!(
            facts.site,
            FailureSite::Execution(ExecutionLocation::root(crate::tests::text(root.path())))
        );
        assert_eq!(facts.effects, Effects::Unknown);
        assert!(facts.paths.iter().any(|fact| {
            fact.role == PathRole::Requested && fact.path == TargetPath::from("missing")
        }));
        output.settle().await.unwrap();
    }

    /// A path that resolves to one that is not text keeps its target's own bytes:
    /// a Windows target's UTF-16 reaches the diagnostic as it sent them, shown
    /// escaped, never decoded with this host's rules.
    #[test]
    fn unspelled_resolutions_keep_their_targets_bytes() {
        let native: Vec<u8> = (r"C:\w\".encode_utf16().chain([0xd800]))
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut path = TargetPath::from("alias");
        let argument = PathArgument::new(&mut path, PathAccess::Read, PathKind::Existing);
        let fault = skyhook_worker::diagnostic::Fault::Unspelled {
            native: native.clone(),
        };
        let resolved = Resolved {
            spelled: r"C:\w\alias".into(),
            resolution: Err(Failure::at(
                Operation::Canonicalize,
                Subject::path("alias"),
                fault,
            )),
        };
        let location = ExecutionLocation::named("win".parse().unwrap(), r"C:\w".into());
        let Err(error) = admit(vec![argument], vec![resolved], &location) else {
            panic!("a path that is not text is refused");
        };
        let diagnostic = error.diagnostic();
        assert_eq!(diagnostic.cause, Cause::Unspelled { native });
        assert_eq!(diagnostic.context.subject, Subject::path("alias"));
        let text = diagnostic.render(crate::diagnostic::TargetView::Hidden);
        assert!(
            text.contains(r"`C\x00:\x00\\\x00w\x00\\\x00\x00\xd8`"),
            "{text}"
        );
    }
}

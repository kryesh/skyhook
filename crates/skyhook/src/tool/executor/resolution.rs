//! Where a call's paths resolve, and what the call then needs approved: those on the
//! session host resolve as the call is planned, the rest once its connections open.

use super::*;
use crate::job::output::{HeldFile, Reach};
use crate::remote::{Landed, PreparedConnection};
use crate::target::Route;
use crate::tool::admission::{
    PathPreflight, admit_paths, assemble_permissions, preflight, scope_capabilities,
};
use crate::tool::target_tools::{ExecutionTarget, read_file};
use skyhook_tools::{
    diagnostic::RequestEffects,
    error::AdmissionError,
    path::{JobWorkspace, PathAccess, PathArgument},
    source::Source,
    target::TargetName,
};
use skyhook_worker::diagnostic::{Effects, PathFact};
use skyhook_worker::{
    op::{self, PathKind, TargetPath},
    target::Target as _,
};

/// Where a call runs, which resolves the paths its input names: on the session host,
/// which resolved them as the call was planned, or where a connection reaches.
pub(super) enum CallSite {
    Root(PathPreflight),
    Connection(usize),
}

/// A connection a call opens: its route, and the workspace it runs in there, the
/// target joining these segments.
pub(super) struct PlannedConnection {
    pub(super) route: Route,
    pub(super) workspace: Vec<TargetPath>,
}

/// When a call's paths resolve, and so how it is approved.
pub(super) enum Admission {
    /// All on the session host, as the call was planned: it is approved once.
    Resolved(Resolution),
    /// Some where its connections reach: their routes are approved before any
    /// opens, and the rest of the call once they resolved its paths.
    Routed {
        connections: Vec<PlannedConnection>,
        pending: Pending,
    },
}

/// What a call's resolution starts from: where each of its paths resolves, those on
/// the session host resolved already, and the permissions its input derives.
pub(super) struct Pending {
    pub(super) call: CallSite,
    pub(super) source: Option<PlannedSource>,
    pub(super) job: Option<PlacedJob>,
    pub(super) derived: Vec<PermissionUse>,
}

/// A source argument's read, where the file lives.
pub(super) enum PlannedSource {
    /// On the session host, resolved as the call was planned.
    Root(PendingSource, PathPreflight),
    /// Where the connection at its index reaches, resolved once it opens.
    Connection(PendingSource<usize>),
}

/// A job its call's input places in a workspace of its own.
pub(super) enum PlacedJob {
    /// On the session host, its workspace resolved as the call was planned.
    Root(ResolvedJob, PathPreflight),
    /// On the target a connection reaches, in the workspace it resolved as it opened.
    Connection(usize),
}

/// A call's paths as the machines holding them resolved them, and what it needs
/// approved.
pub(super) struct Resolution {
    /// The machine the call runs on.
    pub(super) target: ExecutionTarget,
    pub(super) job: ResolvedJob,
    pub(super) source: Option<PendingSource>,
    pub(super) permissions: Vec<PermissionUse>,
    pub(super) path_facts: Vec<PathFact>,
    /// How the call fails once authorized, which may not ask anyone, when a
    /// path it names outside the root does not resolve.
    pub(super) unresolved: Option<ToolError>,
    /// Approved before its operation flew in the flight that resolved its paths, as it
    /// performs there; none when it did not fly or is held there.
    pub(super) performing: Option<Preapproved>,
}

/// A routed call approved in advance for every path it names resolving inside its
/// target's authorization root, so its operation starts in the flight that resolves
/// them.
#[derive(Clone, Copy)]
pub(super) struct Preapproved {
    /// Whether it may only read, so stopping its operation leaves nothing changed.
    reads_only: bool,
    /// The job its operation runs as.
    job: JobId,
}

/// How a call fails once stopped, whose operation may be performing when `flight`
/// is some.
pub(super) fn stopped(flight: Option<Preapproved>, error: ToolError) -> ToolError {
    match flight {
        Some(Preapproved {
            reads_only: false, ..
        }) => (error.effects(Effects::Unknown)).request(RequestEffects::MayHaveExecuted),
        Some(Preapproved {
            reads_only: true, ..
        })
        | None => error,
    }
}

/// Where a call's job runs.
pub(super) struct ResolvedJob {
    pub(super) target: ExecutionTarget,
    pub(super) runs_in: ExecutionLocation,
    /// Whether its call creates the workspace it runs in.
    pub(super) creates: JobWorkspace,
}

/// The machines a call's paths resolve on: the session host and the targets its
/// connections reach.
pub(super) struct Targets<'a> {
    pub(super) root: &'a ExecutionTarget,
    pub(super) connections: &'a [PreparedConnection],
}

impl Targets<'_> {
    fn connection(&self, index: usize) -> ExecutionTarget {
        ExecutionTarget::Remote(self.connections[index].clone())
    }

    /// The workspace connection `index`'s target resolved as it opened.
    fn workspace(&self, index: usize) -> &TargetPath {
        self.connections[index].workspace()
    }

    /// Place what `pending` runs or reads over a connection in the workspace its
    /// target resolved, in place of the one its definition spells: paths resolve,
    /// are scoped and are shown against the directory the target runs them in.
    pub(super) fn place(&self, call: &mut PlannedCall, pending: &mut Pending) {
        if let CallSite::Connection(index) = pending.call {
            call.execution_location.workspace = self.workspace(index).clone();
        }
        if let Some(PlannedSource::Connection(source)) = &mut pending.source {
            source.location.workspace = self.workspace(source.target).clone();
        }
    }

    /// The revision of each target on the routes the call connected along.
    pub(super) fn admitted(&self) -> Vec<(TargetName, u64)> {
        (self.connections.iter())
            .flat_map(|connection| connection.admitted().iter().cloned())
            .collect()
    }
}

impl ToolExecutor {
    /// Approve `call`, connected to `targets`, for `subject` in advance of its paths
    /// resolving inside its target's authorization root: it names some, none needing
    /// a permission of its own there, it reads no source and places no job, and the
    /// request it makes once they do is allowed without asking anyone. That request is
    /// the call's own when they do.
    pub(super) async fn preapprove(
        &self,
        call: &mut PlannedCall,
        pending: &Pending,
        targets: &Targets<'_>,
        subject: &AuthorizationSubject,
    ) -> Option<Preapproved> {
        let (CallSite::Connection(_), None, None) = (&pending.call, &pending.source, &pending.job)
        else {
            return None;
        };
        let paths = call.admitted.paths();
        if paths.is_empty() || !skyhook_tools::path::workspace_scoped(&paths) {
            return None;
        }
        drop(paths);
        let location = &call.execution_location;
        let scope = (&location.target, &location.workspace);
        let (inside, derived) = (PathPreflight::default(), pending.derived.clone());
        let permissions =
            assemble_permissions(&call.tool, scope, &self.capabilities, derived, &inside).ok()?;
        let reads_only = (permissions.iter()).all(|use_| use_.capability == Capability::Read);
        let (arguments, admitted) = (
            (&*call.tool, &call.authorization_arguments),
            targets.admitted(),
        );
        let approved = self.approve(subject, arguments, permissions, Asking::Never, &admitted);
        approved.await.ok()?;
        let job = subject.job;
        Some(Preapproved { reads_only, job })
    }

    /// Resolve each of `call`'s paths still to resolve on the machine holding it, its
    /// input's in place, then assemble the permissions the call needs: those of its
    /// tool, of its input, and of the paths, which need their own beyond the root. A
    /// call `preapproved` starts its operation in the flight that resolves its own.
    pub(super) async fn resolve(
        &self,
        call: &mut PlannedCall,
        pending: Pending,
        targets: Targets<'_>,
        preapproved: Option<Preapproved>,
    ) -> Result<Resolution, ToolError> {
        let Pending {
            call: site,
            source,
            job,
            derived,
        } = pending;
        let (tool, location) = (&call.tool, &call.execution_location);
        let invalid = invalid(tool, location);
        let (target, mut path, performing) = match site {
            CallSite::Root(path) => (targets.root.clone(), path, None),
            CallSite::Connection(index) => {
                let target = targets.connection(index);
                let connection = &targets.connections[index];
                let flight = preapproved.and_then(|flown| {
                    let spills = self.shared.jobs.spills(flown.job);
                    call.admitted.fly(connection, location, spills)
                });
                let (path, performing) = match flight {
                    None => (
                        admit_paths(&target, call.admitted.paths(), location).await,
                        None,
                    ),
                    Some(flight) => {
                        let (resolved, performing) = match flight.await.map_err(&invalid)? {
                            Landed::Performing(resolved) => (resolved, preapproved),
                            Landed::Held(resolved) => (resolved, None),
                        };
                        let path = preflight(call.admitted.paths(), resolved, location);
                        (path, performing)
                    }
                };
                (target, path.map_err(&invalid)?, performing)
            }
        };
        let (job, placed) = match job {
            Some(PlacedJob::Root(job, placed)) => (job, placed),
            None => (
                ResolvedJob::existing(target.clone(), call.job_location.clone()),
                PathPreflight::default(),
            ),
            Some(PlacedJob::Connection(index)) => {
                let runs_in = ExecutionLocation {
                    target: call.job_location.target.clone(),
                    workspace: targets.workspace(index).clone(),
                };
                let job = ResolvedJob::existing(targets.connection(index), runs_in);
                (job, PathPreflight::default())
            }
        };
        path.permissions.extend(placed.permissions);
        let scope = (&location.target, &location.workspace);
        let mut permissions = assemble_permissions(tool, scope, &self.capabilities, derived, &path)
            .map_err(&invalid)?;
        let mut unresolved = (path.unresolved.or(placed.unresolved)).map(&invalid);
        let source = match source {
            None => None,
            Some(source) => {
                let (source, preflight) = match source {
                    PlannedSource::Root(source, preflight) => (source, preflight),
                    PlannedSource::Connection(PendingSource {
                        target: index,
                        location,
                        path,
                    }) => PendingSource::resolve(targets.connection(index), location, path).await?,
                };
                permissions.extend(preflight.permissions);
                unresolved = unresolved.or(preflight.unresolved.map(ToolError::from));
                Some(source)
            }
        };
        Ok(Resolution {
            target,
            job,
            source,
            permissions,
            path_facts: path.paths,
            unresolved,
            performing,
        })
    }

    /// Resolve a job's workspace on the session host, `requested` from
    /// `location`'s: a missing one inside the root is a write the caller must
    /// hold, which the job's call makes. The job shows the workspace as its target
    /// spelled it: `..` kept, as resolving would disclose where a link leads before
    /// approval, and collapsing would claim a location it never means.
    pub(super) async fn resolve_job(
        &self,
        location: ExecutionLocation,
        mut requested: TargetPath,
    ) -> Result<(TargetPath, ResolvedJob, PathPreflight), ToolError> {
        let root = &self.shared.root;
        let resolve = op::Resolve {
            workspace: location.workspace.clone(),
            paths: vec![(requested.clone(), PathKind::Workspace)],
        };
        let Ok(resolved) = root.call(resolve).await?;
        let spelled = resolved.first().map(|resolved| resolved.spelled.clone());
        let spelled = spelled.expect("a path resolves to one resolution");
        let argument = PathArgument::new(&mut requested, PathAccess::Read, PathKind::Workspace);
        let mut preflight = preflight(vec![argument], resolved, &location)?;
        let creates = preflight.found;
        let runs_in = ExecutionLocation {
            target: location.target,
            workspace: requested,
        };
        if creates == JobWorkspace::Missing {
            if !self.capabilities.contains(&Capability::Write) {
                return Err(refused_workspace(
                    "does not exist, and creating it needs write",
                ));
            }
            // A write to the workspace it creates, as resolved.
            let (target, workspace) = (&runs_in.target, &runs_in.workspace);
            let write = scope_capabilities(vec![Capability::Write], target, workspace, None);
            preflight.permissions.extend(write);
        }
        let job = ResolvedJob {
            target: root.clone(),
            runs_in,
            creates,
        };
        Ok((spelled, job, preflight))
    }
}

impl ResolvedJob {
    /// A job that runs `runs_in`, on `target`, a workspace that exists.
    fn existing(target: ExecutionTarget, runs_in: ExecutionLocation) -> Self {
        let creates = JobWorkspace::Exists;
        Self {
            target,
            runs_in,
            creates,
        }
    }
}

/// How a call fails to be admitted: a validation of its tool, where the tool runs.
pub(super) fn invalid(
    tool: &crate::tool::RegisteredTool,
    location: &ExecutionLocation,
) -> impl Fn(AdmissionError) -> ToolError + use<> {
    let context = PartialContext::new(Operation::Validate, Subject::Tool(tool.name().to_owned()))
        .at(tool.placement().site(location));
    move |error| ToolError::from(error).or(context.clone())
}

/// A job's workspace refused as its argument names it.
pub(super) fn refused_workspace(reason: &str) -> ToolError {
    ToolError::invalid_arguments(reason)
        .operation(Operation::Validate, Subject::argument(["workspace"]))
        .effects(Effects::NotStarted)
}

/// A source argument's read on `target`, where it lives: until its connection opens,
/// that connection's index. Resolved there, it opens as its call starts.
pub(super) struct PendingSource<T = ExecutionTarget> {
    pub(super) target: T,
    pub(super) path: TargetPath,
    pub(super) location: ExecutionLocation,
}

impl PendingSource {
    /// Resolve source argument `path` on `target`, where `location` reads it, its read
    /// scoped to the workspace there.
    pub(super) async fn resolve(
        target: ExecutionTarget,
        location: ExecutionLocation,
        mut path: TargetPath,
    ) -> Result<(Self, PathPreflight), ToolError> {
        let read = vec![Capability::Read];
        let mut permissions = scope_capabilities(read, &location.target, &location.workspace, None);
        let argument = PathArgument::new(&mut path, PathAccess::Read, PathKind::Existing);
        let mut preflight = admit_paths(&target, vec![argument], &location).await?;
        permissions.append(&mut preflight.permissions);
        preflight.permissions = permissions;
        let source = Self {
            target,
            path,
            location,
        };
        Ok((source, preflight))
    }

    /// The source opened for `context`'s call: a file on the machine the call
    /// writes to is copied there, and any other streams to it once.
    pub(super) async fn open(self, context: &ToolContext) -> Result<Source, ToolError> {
        let path = self.path.clone();
        if self.location.target == context.execution_location().target {
            return Ok(Source::Copy(path));
        }
        let read = op::Read {
            path,
            workspace: self.location.workspace.clone(),
            mode: op::ReadMode::File,
            from: op::ReadFrom::Start,
        };
        let (_, _, bytes) = read_file(&self.target, read, &self.location).await?;
        Ok(Source::stream(op::Input::new(bytes)))
    }
}

/// A call's reach to what a job's target holds of its saved output: the machine is
/// reached as the call's own would be, its route approved before it connects, and
/// only along the route the file was kept on. A stream that target wrote is the
/// session's own saved output, read back as its database is; a file a read named is
/// resolved and authorized again, as a read of it by the caller is.
pub(super) struct CallReach {
    pub(super) executor: ToolExecutor,
    pub(super) tool: Arc<crate::tool::RegisteredTool>,
    pub(super) arguments: skyhook_tools::arguments::Arguments,
    pub(super) subject: AuthorizationSubject,
}

impl Reach for CallReach {
    fn reach<'a>(
        &'a self,
        ran: &'a ExecutionLocation,
        file: crate::session::TargetFile,
    ) -> futures_util::future::BoxFuture<'a, Result<HeldFile, ToolError>> {
        Box::pin(self.reached(ran, file))
    }
}

impl CallReach {
    async fn reached(
        &self,
        ran: &ExecutionLocation,
        crate::session::TargetFile { file, route: kept }: crate::session::TargetFile,
    ) -> Result<HeldFile, ToolError> {
        use crate::tool::authorization::{AuthorizationArguments, SourceArguments};
        use skyhook_tools::{builtins::Read, output::CaptureFile, spec::TargetTool as _};
        let (executor, subject) = (&self.executor, &self.subject);
        let (services, held) = (&executor.shared, &executor.capabilities);
        let explicit = (ran.target != executor.caller_location.target).then(|| ran.target.clone());
        let read = match &file {
            CaptureFile::Written(_) => None,
            CaptureFile::Named { .. } => {
                let reads = services.registry.get(Read::NAME);
                let selects = explicit.is_none() || held.contains(&Capability::Targets);
                let unavailable = || ToolError::unavailable(Read::NAME);
                Some(reads.filter(|_| selects).ok_or_else(unavailable)?)
            }
        };
        let planning::SelectedLocation { location, route } = executor.locate(explicit)?;
        let refused = |error| dispatch::refusal(self.tool.name(), error);
        // The file is on the machine its run reached. A route since redefined reaches
        // another, whose file at the same path is not the one kept.
        if route.as_ref().map_or_else(Vec::new, Route::identity) != kept {
            return Err(refused(AuthorizationError::TargetRedefined));
        }
        let mut arguments = AuthorizationArguments {
            tool: self.arguments.clone(),
            route: route.as_ref().map(Route::authorization_arguments),
            source: None,
        };
        let (target, location, admitted) = match route {
            None => (services.root.clone(), location, Vec::new()),
            Some(route) => {
                let request = (&*self.tool, &arguments);
                let routes = route.permissions();
                let approved = executor.approve(subject, request, routes, Asking::Allowed, &[]);
                approved.await.map_err(refused)?;
                let workspace = vec![location.workspace.clone()];
                let connection = services.router.prepare(route, workspace, subject).await?;
                let workspace = connection.workspace().clone();
                let (target, admitted) = (location.target, connection.admitted().clone());
                let location = ExecutionLocation { target, workspace };
                (ExecutionTarget::Remote(connection), location, admitted)
            }
        };
        let mut path = file.path().clone();
        if let Some(read) = read {
            let argument = PathArgument::new(&mut path, PathAccess::Read, PathKind::Existing);
            let preflight = admit_paths(&target, vec![argument], &location).await?;
            let scope = (&location.target, &location.workspace);
            let permissions = assemble_permissions(&read, scope, held, Vec::new(), &preflight)?;
            arguments.source = Some(SourceArguments {
                path: file.path().clone(),
                target: location.target.clone(),
                route: arguments.route.clone(),
            });
            let (request, asking) = (
                (&*self.tool, &arguments),
                Asking::for_unresolved(&preflight.unresolved),
            );
            let approved = executor.approve(subject, request, permissions, asking, &admitted);
            approved.await.map_err(refused)?;
            if let Some(unresolved) = preflight.unresolved {
                return Err(unresolved.into());
            }
        }
        Ok(HeldFile {
            target,
            location,
            path,
            file,
        })
    }
}

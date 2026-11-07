//! Tool invocation entry points and shared execution services.

mod dispatch;
mod planning;
mod results;

pub(crate) use dispatch::cancellation_result;
pub use results::ExecutionResult;
pub(crate) use results::failure_response;
pub(crate) use results::{Reported, StartedExecution};

use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

use crate::{
    execution::ExecutionLocation,
    identity::{AgentId, JobId},
    job::{JobManager, JobSpec, JobState},
    remote::RemoteError,
    target::{Route, TargetRef, TargetRouter},
    tool::{
        ToolPlacement,
        authorization::{
            Asking, AuthorizationCoordinator, AuthorizationError, AuthorizationSubject,
        },
        diagnostic::{
            Cause, Effects, FailureSite, Operation, PartialContext, PartialDiagnostic, PathFact,
            Subject,
        },
        policy::{Capability, CapabilitySet, PermissionUse, Policy},
        registry::{Arguments, ExecutionEnvelope, JobLaunch},
    },
};

use super::{ScriptBinding, ToolContext, ToolError, ToolOutput, ToolRegistry};

#[derive(Clone, Copy)]
pub(crate) enum InvocationKind {
    Host,
    Model,
    Script,
}

struct InvocationPlan {
    origin: Option<crate::session::ModelCallOrigin>,
    agent: AgentId,
    tool: Arc<crate::tool::RegisteredTool>,
    original_arguments: Arguments,
    authorization_arguments: crate::tool::authorization::AuthorizationArguments,
    caller_location: ExecutionLocation,
    execution_location: ExecutionLocation,
    /// Where the job reports it acts: the execution location, unless the input
    /// places it elsewhere, with its workspace as written.
    job_location: ExecutionLocation,
    /// Where it runs: `job_location`, with a local workspace resolved.
    job_runs_in: ExecutionLocation,
    /// Whether its call creates the local workspace it runs in.
    creates: crate::tool::invocation::JobWorkspace,
    permissions: Vec<PermissionUse>,
    path_facts: Vec<PathFact>,
    parent: Option<JobId>,
    launch: JobLaunch,
    /// Admitted on the host even when a remote destination runs the call.
    result_policy: super::ToolResultPolicy,
    /// How much of a finished result the call's reads present: a script holds results whole.
    fit: crate::job::output::Fit,
    /// The schema the job's stored result follows.
    output_schema: Option<Value>,
    dispatch: InvocationDispatch<PlannedRemote>,
    source: Option<SourcePlan<PlannedRemote>>,
    /// The remote a host call's input places its job on, approved with the call and
    /// connected before it runs.
    job_route: Option<PlannedRemote>,
    /// How the call fails once authorized, which may not ask anyone, when a
    /// path it names outside the root does not resolve.
    unresolved: Option<ToolError>,
}

/// The read of a tool's source argument, planned and authorized with the call it
/// feeds. A remote path is resolved and checked by the worker that reads it.
enum SourcePlan<R> {
    Local {
        path: PathBuf,
        location: ExecutionLocation,
    },
    Remote {
        remote: R,
        workspace: PathBuf,
        path: String,
    },
}

/// The remote payload is a planned route before authorization and a prepared
/// connection afterwards; local dispatch is unchanged by that transition.
enum InvocationDispatch<R> {
    Local(super::registry::AdmittedInvocation),
    Remote { remote: R, arguments: Arguments },
}

struct PlannedRemote {
    route: Route,
    router: TargetRouter,
}

/// A call planned for its job, before the job exists.
pub(crate) struct PlannedInvocation {
    kind: InvocationKind,
    plan: InvocationPlan,
    spec: JobSpec,
}

/// A planned invocation whose job is published: the caller's outstanding work
/// from here on, before anything runs. Dropping it cancels the job.
pub(crate) struct CreatedInvocation {
    kind: InvocationKind,
    plan: InvocationPlan,
    lease: crate::job::JobLease,
}

impl CreatedInvocation {
    #[cfg(feature = "script")]
    pub(crate) fn result_policy(&self) -> super::ToolResultPolicy {
        self.plan.result_policy
    }

    pub(crate) fn job(&self) -> JobId {
        self.lease.id()
    }

    /// Whether its call blocks on a job its turn may move to the background.
    pub(crate) fn movable(&self) -> bool {
        movable(&self.plan)
    }
}

#[derive(Clone)]
pub struct ToolExecutor {
    model_origin: Option<crate::session::ModelCallOrigin>,
    shared: Arc<ExecutorServices>,
    caller_location: ExecutionLocation,
    capabilities: CapabilitySet,
}

#[derive(Clone)]
struct ExecutorServices {
    registry: ToolRegistry,
    authorization: AuthorizationCoordinator,
    jobs: JobManager,
    root_location: ExecutionLocation,
    router: Option<TargetRouter>,
}

impl ToolExecutor {
    #[must_use]
    pub fn new(
        registry: ToolRegistry,
        policy: Arc<dyn Policy>,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        Self::with_authorization(
            registry,
            AuthorizationCoordinator::new(policy),
            jobs,
            workspace,
        )
    }

    #[must_use]
    pub(crate) fn with_authorization(
        registry: ToolRegistry,
        authorization: AuthorizationCoordinator,
        jobs: JobManager,
        workspace: PathBuf,
    ) -> Self {
        let caller_location = ExecutionLocation::root(workspace.clone());
        Self {
            model_origin: None,
            shared: Arc::new(ExecutorServices {
                registry,
                authorization,
                jobs,
                root_location: ExecutionLocation::root(workspace),
                router: None,
            }),
            caller_location,
            capabilities: CapabilitySet::default(),
        }
    }

    #[must_use]
    pub(crate) fn with_model_origin(mut self, origin: crate::session::ModelCallOrigin) -> Self {
        self.model_origin = Some(origin);
        self
    }

    #[must_use]
    pub(crate) fn with_location(mut self, location: ExecutionLocation) -> Self {
        self.caller_location = location;
        self
    }

    #[must_use]
    pub(crate) fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Generate model and JavaScript tools for the receiving agent.
    #[must_use]
    pub fn surface_for_agent(&self, agent: &AgentId) -> Arc<crate::tool::ToolSurface> {
        self.shared
            .registry
            .surface_for_agent(&self.capabilities, agent)
    }

    #[must_use]
    pub(crate) fn with_target_router(mut self, router: TargetRouter) -> Self {
        Arc::make_mut(&mut self.shared).router = Some(router);
        self
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.shared.registry
    }

    pub(crate) fn diagnostic_viewer(&self) -> super::diagnostic::DiagnosticViewer<'_> {
        super::diagnostic::DiagnosticViewer::new(&self.capabilities, &self.caller_location)
    }

    pub fn jobs(&self) -> &JobManager {
        &self.shared.jobs
    }

    pub async fn execute(
        &self,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ToolError> {
        self.execute_as(InvocationKind::Host, agent, name, arguments, parent)
            .await
    }

    pub(crate) async fn execute_as(
        &self,
        kind: InvocationKind,
        agent: AgentId,
        name: &str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> Result<ExecutionResult, ToolError> {
        let created = self.create(kind, agent, name, arguments, parent).await?;
        self.run(created).await
    }

    /// Plan a call and publish its job without running it.
    pub(crate) fn create<'a>(
        &'a self,
        kind: InvocationKind,
        agent: AgentId,
        name: &'a str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> futures_util::future::BoxFuture<'a, Result<CreatedInvocation, ToolError>> {
        Box::pin(async move {
            let planned = self.plan(kind, agent, name, arguments, parent).await;
            let mut created = self.publish(vec![planned]).await;
            created.pop().expect("a creation per plan")
        })
    }

    /// Plan a call for its job without publishing it. Plans share nothing, so
    /// the calls of one response plan concurrently.
    ///
    /// Boxed, as [`publish`](Self::publish) is, so agent loops prove these
    /// futures `Send` at this boundary rather than through every type planning
    /// and publishing hold.
    pub(crate) fn plan<'a>(
        &'a self,
        kind: InvocationKind,
        agent: AgentId,
        name: &'a str,
        arguments: Value,
        parent: Option<JobId>,
    ) -> futures_util::future::BoxFuture<'a, Result<PlannedInvocation, ToolError>> {
        Box::pin(async move {
            let plan = self
                .plan_registered(kind, agent, name, arguments, parent)
                .await
                .map_err(|error| {
                    let placement = (self.shared.registry.get(name))
                        .map_or(ToolPlacement::Host, |tool| tool.placement());
                    error.or(PartialContext::new(
                        Operation::Validate,
                        Subject::Tool(name.to_owned()),
                    )
                    .at(FailureSite::bound(&self.caller_location, placement)))
                })?;
            let spec = self.job_spec(&plan);
            Ok(PlannedInvocation { kind, plan, spec })
        })
    }

    /// Publish the jobs of `planned`'s plans, in order, in one commit, so the
    /// calls of one response are all outstanding before any of them runs. A
    /// failed plan passes through as its call's result; a refusal refuses each
    /// planned call as it would have refused it alone.
    pub(crate) fn publish(
        &self,
        planned: Vec<Result<PlannedInvocation, ToolError>>,
    ) -> futures_util::future::BoxFuture<'_, Vec<Result<CreatedInvocation, ToolError>>> {
        Box::pin(async move {
            let mut specs = Vec::with_capacity(planned.len());
            let calls: Vec<_> = (planned.into_iter())
                .map(|planned| {
                    planned.map(|PlannedInvocation { kind, plan, spec }| {
                        specs.push(spec);
                        (kind, plan)
                    })
                })
                .collect();
            let leases = match specs.is_empty() {
                true => Ok(Vec::new()),
                false => self.shared.jobs.create_all(specs).await,
            };
            match leases {
                Ok(leases) => {
                    let mut leases = leases.into_iter();
                    (calls.into_iter())
                        .map(|call| {
                            call.map(|(kind, plan)| CreatedInvocation {
                                kind,
                                plan,
                                lease: leases.next().expect("a lease per planned call"),
                            })
                        })
                        .collect()
                }
                Err(error) => (calls.into_iter())
                    .map(|call| {
                        call.and_then(
                            |(_, plan)| Err(job_error(&error).or(prepare_fallback(&plan))),
                        )
                    })
                    .collect(),
            }
        })
    }

    /// Authorize, launch and collect a created invocation for the host,
    /// acknowledging what its result reports as it returns: the host journals
    /// no result to acknowledge it with.
    pub(crate) async fn run(
        &self,
        created: CreatedInvocation,
    ) -> Result<ExecutionResult, ToolError> {
        let Reported {
            outcome,
            acknowledges,
        } = self.run_reported(created, None).await;
        let acknowledged = self.shared.jobs.acknowledge(acknowledges);
        acknowledged.await.map_err(job_error)?;
        outcome
    }

    /// Authorize, launch and collect a created invocation, with the delivery
    /// its result reports for the caller to acknowledge with its record, held
    /// for job `reader` as it is collected when a job reads it.
    pub(crate) fn run_reported(
        &self,
        created: CreatedInvocation,
        reader: Option<JobId>,
    ) -> futures_util::future::BoxFuture<'_, Reported> {
        Box::pin(async move {
            let CreatedInvocation { kind, plan, lease } = created;
            let result_policy = plan.result_policy;
            let fallback = prepare_fallback(&plan);
            let purpose = match kind {
                InvocationKind::Model if result_policy != super::ToolResultPolicy::JobView => {
                    return self.run_model_call(plan, lease, fallback).await;
                }
                InvocationKind::Model | InvocationKind::Script => {
                    results::CollectionPurpose::Public(result_policy)
                }
                InvocationKind::Host => results::CollectionPurpose::Native,
            };
            let started = match self.start(plan, lease).await {
                Ok(started) => started,
                Err(error) => return Err(error.or(fallback)).into(),
            };
            self.collect_result(started, purpose, reader).await
        })
    }

    /// Start and collect the model's call. Its turn may move a movable call to the
    /// background while it is still starting, connecting to its target included: the
    /// start then goes on under the supervisor, which owns its lease, and the call
    /// returns the moved job at once.
    async fn run_model_call(
        &self,
        plan: InvocationPlan,
        lease: crate::job::JobLease,
        fallback: PartialContext,
    ) -> Reported {
        let started = match movable(&plan) {
            false => self.start(plan, lease).await,
            true => {
                let (job, executor) = (lease.id(), self.clone());
                let jobs = &self.shared.jobs;
                let starting = async move { executor.start(plan, lease).await };
                let mut starting = jobs.supervisor().spawn(starting);
                tokio::select! {
                    biased;
                    started = &mut starting => match started {
                        Ok(Some(started)) => started,
                        Ok(None) => Err(ToolError::cancelled_before_start()),
                        Err(panicked) => std::panic::resume_unwind(panicked.into_panic()),
                    },
                    moved = jobs.until_moved(job) => return match moved {
                        Ok(envelope) => Ok(results::released_call(job, &envelope)).into(),
                        Err(error) => Err(job_error(error)).into(),
                    },
                }
            }
        };
        match started {
            Ok(started) => self.collect_model_started(started).await,
            Err(error) => Err(error.or(fallback)).into(),
        }
    }
}

/// Whether `plan`'s call blocks on a job its turn may move to the background: a
/// foreground call of a tool that can run there, but never a question for a person.
fn movable(plan: &InvocationPlan) -> bool {
    !plan.launch.background
        && plan.tool.supports_background()
        && plan.tool.job_role() != crate::job::JobRole::Question
}

/// Job state lives on the session host; the caller names the failed stage.
fn job_error(error: impl std::borrow::Borrow<crate::job::JobError>) -> ToolError {
    ToolError::from(error.borrow()).or(PartialContext::default().at(FailureSite::Host))
}

fn prepare_fallback(plan: &InvocationPlan) -> PartialContext {
    PartialContext::new(
        Operation::Prepare,
        Subject::Tool(plan.tool.name().to_owned()),
    )
    .at(FailureSite::bound(
        &plan.execution_location,
        plan.tool.placement(),
    ))
}

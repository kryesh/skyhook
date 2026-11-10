//! Creating a response's tool calls as jobs, and the results of calls that never ran.

use super::SessionRuntime;
use crate::identity::AgentId;
use crate::provider::protocol::{ToolCall, ToolResult};
use crate::session::{Message, MessageSeq, SessionEvent};
use crate::tool::executor::ToolExecutor;
use crate::tool::policy::CapabilitySet;
use skyhook_worker::job::JobId;

/// A model call between its job's publication and its result.
pub(super) enum CreatedCall {
    Created {
        executor: ToolExecutor,
        call: ToolCall,
        created: Box<crate::tool::executor::CreatedInvocation>,
    },
    /// Settled without a job: planning failed or the tool is unavailable.
    Settled(ToolResult),
}

impl CreatedCall {
    /// The job the call blocks on that its turn may move to the background.
    pub(super) fn movable(&self) -> Option<skyhook_worker::job::JobId> {
        match self {
            Self::Created { created, .. } => created.movable().then(|| created.job()),
            Self::Settled(_) => None,
        }
    }

    /// The call's result, with the delivery it reports for its commit to
    /// acknowledge. Tool dispatch owns its execution inputs. Erasing this future
    /// separates the driver's Send proof from the nested supervised executor graph.
    pub(super) fn run(
        self,
    ) -> futures_util::future::BoxFuture<'static, (ToolResult, Option<crate::job::Delivery>)> {
        Box::pin(async move {
            let (executor, call, created) = match self {
                Self::Settled(result) => return (result, None),
                Self::Created {
                    executor,
                    call,
                    created,
                } => (executor, call, created),
            };
            let reported = executor.run_reported(*created, None).await;
            let result = match reported.outcome {
                Ok(result) => ToolResult {
                    call_id: call.id().to_owned(),
                    name: call.name().to_owned(),
                    result: result.output.value,
                    images: result.output.images,
                    is_error: result.is_error,
                },
                Err(error) => failed_result(&call, &executor, error),
            };
            (result, reported.acknowledges)
        })
    }
}

fn failed_result(
    call: &ToolCall,
    executor: &ToolExecutor,
    error: crate::tool::ToolError,
) -> ToolResult {
    let viewer = executor.diagnostic_viewer();
    let output = crate::tool::executor::failure_response(error, viewer);
    failed_tool_result(call.id().to_owned(), call.name().to_owned(), output)
}

/// One `error` result per call, in call order, closing each exchange: the
/// failure view of a call that never ran as a job.
pub(super) fn unrun_results<'a>(
    calls: impl Iterator<Item = (&'a AgentId, crate::session::ModelCallOrigin, &'a str)>,
    error: &str,
) -> Vec<(AgentId, SessionEvent)> {
    calls
        .map(|(agent, origin, name)| {
            let failure = crate::job::JobView::failure(error.into(), None, false);
            let output = crate::tool::ToolOutput::new(failure.into_value());
            let message = Message::Tool(failed_tool_result(origin.call_id, name.into(), output));
            (agent.clone(), SessionEvent::MessageCommitted { message })
        })
        .collect()
}

/// The result of a foreground delegation a restart kept as the released job `job`:
/// not a failure; the retained child resumes on continue or owner input and reports
/// as a job event.
pub(super) fn released_result(
    call_id: &str,
    name: &str,
    job: &crate::job::JobEnvelope,
) -> SessionEvent {
    let view = job.released_view(crate::job::Release::Restarted);
    let result = ToolResult {
        call_id: call_id.into(),
        name: name.into(),
        result: view.into_value(),
        images: Vec::new(),
        is_error: false,
    };
    SessionEvent::MessageCommitted {
        message: Message::Tool(result),
    }
}

/// A failed call's result, reported by no job.
pub(super) fn failed_tool_result(
    call_id: String,
    name: String,
    output: crate::tool::ToolOutput,
) -> ToolResult {
    ToolResult {
        call_id,
        name,
        result: output.value,
        images: output.images,
        is_error: true,
    }
}

impl SessionRuntime {
    /// Plan a response's calls concurrently, then publish their jobs in call
    /// order in one commit before any runs, so a `wait` among them sees its
    /// siblings as outstanding work from the start. A call that fails planning
    /// settles alone.
    pub(super) async fn create_calls(
        &self,
        agent: &AgentId,
        parent: Option<JobId>,
        calls: &[ToolCall],
        origin: MessageSeq,
        location: &skyhook_tools::target::ExecutionLocation,
        capabilities: &CapabilitySet,
    ) -> Vec<CreatedCall> {
        let executors: Vec<_> = (calls.iter())
            .map(|call| {
                (self.executor.clone())
                    .with_location(location.clone())
                    .with_capabilities(capabilities.clone())
                    .with_model_origin(crate::session::ModelCallOrigin {
                        message: origin,
                        call_id: call.id().to_owned(),
                    })
            })
            .collect();
        let plans = calls.iter().zip(&executors).map(|(call, executor)| {
            let arguments = serde_json::Value::Object(call.arguments().clone());
            let kind = crate::tool::catalog::InvocationKind::Model;
            executor.plan(kind, agent.clone(), call.name(), arguments, parent)
        });
        let planned = futures_util::future::join_all(plans).await;
        let created = self.executor.publish(planned).await;
        (created.into_iter().zip(executors).zip(calls))
            .map(|((created, executor), call)| match created {
                Ok(created) => CreatedCall::Created {
                    executor,
                    call: call.clone(),
                    created: Box::new(created),
                },
                Err(error) => CreatedCall::Settled(failed_result(call, &executor, error)),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::tests::*;
    use crate::tests::bounded;

    /// A call that fails before its job exists has no handle, and its metadata
    /// would only repeat the call.
    #[tokio::test]
    async fn preadmission_failure_history_is_a_failure_without_a_job() {
        let call = tool_call(0, "missing-call", "missing", json!({"name":"unadmitted"}));
        let (_root, requests, session) =
            scripted_session([response(vec![call]), answer("done")]).await;
        assert_eq!(session.prompt("run").await.unwrap(), "done");
        let requests = requests.lock().unwrap();
        let results = last_tool_results(&requests[1]);
        assert!(results[0].is_error);
        let view = results[0].result.as_object().unwrap();
        assert_eq!(view["state"], "failed");
        assert_eq!(view.keys().collect::<Vec<_>>(), ["state", "error"]);
    }

    /// A response's calls are created in one commit: a call that fails planning
    /// leaves its siblings created in call order with contiguous ids, and a
    /// refused commit leaves none of them, never the ones before the refusal.
    #[tokio::test]
    async fn a_responses_jobs_are_created_together_in_call_order() {
        for commits in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let sessions = root.path().join("sessions");
            let requests = Requests::default();
            let exec = json!({"command": "true"});
            let calls = vec![
                tool_call(0, "first", "exec", exec.clone()),
                tool_call(1, "unplanned", "missing", json!({})),
                tool_call(2, "third", "exec", exec),
            ];
            let provider = scripted_provider(&requests, [response(calls), answer("done")]);
            let harness = test_harness(root.path(), &sessions, provider).await;
            let session = harness.new_session().await.unwrap();
            let refused = (!commits).then(|| {
                let rule = "BEFORE INSERT ON job WHEN NEW.id = 2";
                crate::session::tests::refuse(&session.runtime.store, rule)
            });
            assert_eq!(session.prompt("run").await.is_ok(), commits);
            drop(refused);
            let created = |jobs: Vec<crate::job::JobEnvelope>| {
                (jobs.into_iter())
                    .map(|job| (job.id.get(), job.origin.unwrap().call_id))
                    .collect::<Vec<_>>()
            };
            if commits {
                let jobs = session.runtime.jobs.list(&session.root).await.unwrap();
                let expected = [(1, "first".to_owned()), (2, "third".to_owned())];
                assert_eq!(created(jobs), expected);
                shutdown_session(session).await;
                continue;
            }
            let id = session.id();
            assert!(bounded(session.shutdown()).await.is_err());
            assert!(session.close().await.is_err());
            let resumed = harness.resume_session(id).await.unwrap();
            let jobs = resumed.runtime.jobs.list(&resumed.root).await.unwrap();
            assert_eq!(created(jobs), []);
            shutdown_session(resumed).await;
        }
    }
}

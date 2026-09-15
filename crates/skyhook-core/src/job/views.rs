//! Job presentation and live metadata queries.

use super::*;

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct JobEnvelope {
    pub id: JobId,
    pub parent: Option<JobId>,
    pub tool: String,
    #[serde(default)]
    pub role: JobRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub state: JobState,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub location: ExecutionLocation,
    #[serde(flatten)]
    pub denial: Option<crate::tool::Denial>,
}

#[derive(Serialize, JsonSchema)]
struct PresentedJob<'a> {
    id: JobId,
    state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<JobId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<&'a std::path::Path>,
    /// A waiting child agent returns a question batch; other jobs may return any output.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "question_or_output_schema")]
    output: Option<&'a Value>,
    /// Source sequence of the last visible child reply. Automatic completed-agent
    /// notifications reference that message instead of repeating the saved result.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_message: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(flatten)]
    denial: Option<crate::tool::Denial>,
}

// Keep question fields discoverable without classifying extensible tool names.
fn question_or_output_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [generator.subschema_for::<crate::agent::QuestionOutput>(), true]
    })
}

/// Minimal job information included in the model's current runtime snapshot.
#[derive(Serialize)]
pub(crate) struct ActiveJob {
    pub(crate) job: JobId,
    pub(crate) tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) state: JobState,
    pub(crate) location: ActiveJobLocation,
    pub(crate) age_seconds: u64,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<progress::AgentProgress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) children: Vec<ActiveJob>,
}

#[derive(Serialize)]
pub(crate) struct ActiveJobLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    pub(crate) workspace: std::path::PathBuf,
}

impl JobEnvelope {
    #[cfg(test)]
    pub(crate) fn presented(
        &self,
        capabilities: &CapabilitySet,
    ) -> Result<Value, serde_json::Error> {
        self.presented_for(capabilities, None, true)
    }

    pub(crate) fn presented_for(
        &self,
        capabilities: &CapabilitySet,
        viewer: Option<&ExecutionLocation>,
        detailed: bool,
    ) -> Result<Value, serde_json::Error> {
        self.presented_with_reference(capabilities, viewer, detailed, None)
    }

    pub(super) fn presented_with_reference(
        &self,
        capabilities: &CapabilitySet,
        viewer: Option<&ExecutionLocation>,
        detailed: bool,
        last_message: Option<u64>,
    ) -> Result<Value, serde_json::Error> {
        serde_json::to_value(PresentedJob {
            id: self.id,
            state: self.state.presented(),
            last_message,
            parent: self.parent.filter(|_| detailed),
            tool: detailed.then_some(self.tool.as_str()),
            name: self
                .name
                .as_deref()
                .filter(|name| detailed && !name.is_empty()),
            target: capabilities
                .contains(Capability::Targets)
                .then_some(self.location.target.as_str()),
            workspace: viewer
                .is_none_or(|location| location.workspace != self.location.workspace)
                .then_some(self.location.workspace.as_path()),
            output: self.output.as_ref(),
            error: self.error.as_deref(),
            denial: self.denial.clone(),
        })
    }
}

pub(crate) fn presented_job_schema(capabilities: &CapabilitySet, many: bool) -> Value {
    let mut envelope = serde_json::to_value(schemars::schema_for!(PresentedJob<'_>))
        .expect("job schema serializes");
    if !capabilities.contains(Capability::Targets) {
        envelope["properties"]
            .as_object_mut()
            .unwrap()
            .remove("target");
    }
    if many {
        let definitions = envelope
            .as_object_mut()
            .unwrap()
            .remove("$defs")
            .unwrap_or_else(|| serde_json::json!({}));
        serde_json::json!({"type":"array", "items":envelope, "$defs":definitions})
    } else {
        envelope
    }
}

impl JobManager {
    /// Inspect launch provenance without claiming output or changing delivery state.
    pub(crate) async fn active_launches(
        &self,
        agent: &AgentId,
    ) -> Vec<(JobId, Option<crate::session::ModelCallOrigin>)> {
        let jobs = self.inner.jobs.lock().await;
        let mut launches = Vec::new();
        for (id, entry) in jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == agent && !entry.state.is_terminal())
        {
            let mut current = entry;
            let origin = loop {
                if &current.agent != agent {
                    break None;
                }
                if let Some(origin) = &current.origin {
                    break Some(origin.clone());
                }
                let Some(parent) = current.parent.and_then(|id| jobs.get(&id)) else {
                    break None;
                };
                current = parent;
            };
            launches.push((*id, origin));
        }
        launches.sort_by_key(|(id, _)| *id);
        launches
    }

    pub(crate) async fn metadata(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let jobs = self.inner.jobs.lock().await;
        Ok(jobs.get(&id).ok_or(JobError::Unknown(id))?.metadata(id))
    }

    pub async fn snapshot(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let mut envelope = {
            let jobs = self.inner.jobs.lock().await;
            jobs.get(&id).ok_or(JobError::Unknown(id))?.envelope(id)
        };
        self.hydrate_envelope(&mut envelope).await?;
        Ok(envelope)
    }

    pub(crate) async fn cancellation_token(
        &self,
        id: JobId,
    ) -> Result<CancellationToken, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.cancellation.clone())
            .ok_or(JobError::Unknown(id))
    }

    pub(crate) async fn authorization_scope(&self, id: JobId) -> Result<Option<u64>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.authorization_scope)
            .ok_or(JobError::Unknown(id))
    }

    pub async fn list(&self, owner: &AgentId) -> Vec<JobEnvelope> {
        let jobs = self.inner.jobs.lock().await;
        let mut output = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner)
            .map(|(id, entry)| entry.metadata(*id))
            .collect::<Vec<_>>();
        output.sort_by_key(|job| job.id);
        output
    }

    /// Current request-time state; progress consumes each committed journal event once.
    pub(crate) async fn active_states(
        &self,
        owner: &AgentId,
        capabilities: &CapabilitySet,
        now_millis: i64,
    ) -> Vec<ActiveJob> {
        let mut progress = self.inner.progress.lock().await;
        self.inner
            .store
            .visit_records_after(progress.sequence, |records| progress.project(records))
            .await;
        let jobs = self.inner.jobs.lock().await;
        let mut states = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner && !entry.state.is_terminal())
            .map(|(id, _)| {
                progress::active_job(
                    *id,
                    &jobs,
                    &progress,
                    capabilities,
                    now_millis,
                    &mut std::collections::HashSet::new(),
                )
            })
            .collect::<Vec<_>>();
        states.sort_by_key(|job| job.job);
        states
    }

    /// Whether a child message or completion/question is ready, without reserving it.
    pub(crate) async fn has_pending(&self, owner: &AgentId) -> bool {
        self.inner
            .jobs
            .lock()
            .await
            .values()
            .any(|entry| &entry.agent == owner && entry.has_pending())
    }

    /// Associate an agent job with the child's actual workspace and target.
    /// The corresponding AgentStarted record persists this association for replay.
    pub(crate) async fn set_agent_location(
        &self,
        job: JobId,
        location: ExecutionLocation,
    ) -> Result<(), JobError> {
        let mut jobs = self.inner.jobs.lock().await;
        jobs.get_mut(&job).ok_or(JobError::Unknown(job))?.location = location;
        Ok(())
    }

    pub async fn has_running(&self, owner: &AgentId) -> bool {
        self.inner
            .jobs
            .lock()
            .await
            .values()
            .any(|entry| &entry.agent == owner && (!entry.state.is_terminal() || entry.suspended()))
    }

    pub async fn is_background(&self, id: JobId) -> Result<bool, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.background)
            .ok_or(JobError::Unknown(id))
    }

    pub async fn images(&self, id: JobId) -> Result<Vec<ImageRef>, JobError> {
        self.inner
            .jobs
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.images.clone())
            .ok_or(JobError::Unknown(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_schema_documents_batches_without_classifying_tool_names() {
        for many in [false, true] {
            let schema = presented_job_schema(&CapabilitySet::default(), many);
            assert!(
                schema["$defs"]["QuestionOutput"]["properties"]
                    .get("questions")
                    .is_some()
            );
            let envelope = if many { &schema["items"] } else { &schema };
            assert!(envelope["properties"].get("role").is_none());
            for (tool, output) in [
                (
                    "delegate",
                    serde_json::json!({"questions":[{"id":"choice", "prompt":"Choose"}]}),
                ),
                ("agent", serde_json::json!("custom prompt")),
            ] {
                let job = serde_json::json!({"id":1, "state":"waiting_input", "tool":tool, "output":output});
                let value = if many { serde_json::json!([job]) } else { job };
                assert!(jsonschema::is_valid(&schema, &value));
            }
        }
    }

    #[tokio::test]
    async fn active_launches_stop_at_agent_boundaries_and_prefer_the_nearest_call() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let child_agent = agent.child(1);
        let origin = |message, call_id: &str| crate::session::ModelCallOrigin {
            message,
            call_id: call_id.into(),
        };
        let (root_origin, child_origin) = (origin(1, "delegate"), origin(2, "child-script"));
        let spec = |agent: &AgentId, tool, parent, origin| JobSpec {
            parent,
            origin,
            ..JobSpec::test(agent.clone(), tool)
        };
        let owner = jobs
            .test_lease(spec(&agent, "agent", None, Some(root_origin.clone())))
            .await;
        let host = jobs
            .test_lease(spec(&child_agent, "host-started", Some(owner.id()), None))
            .await;
        assert_eq!(
            jobs.active_launches(&child_agent).await,
            vec![(host.id(), None)]
        );
        let script = spec(
            &child_agent,
            "script",
            Some(owner.id()),
            Some(child_origin.clone()),
        );
        let child = jobs.test_lease(script).await;
        let shell = jobs
            .test_lease(spec(&child_agent, "shell", Some(child.id()), None))
            .await;
        assert_eq!(
            jobs.active_launches(&agent).await,
            vec![(owner.id(), Some(root_origin))]
        );
        assert_eq!(
            jobs.active_launches(&child_agent).await,
            vec![
                (host.id(), None),
                (child.id(), Some(child_origin.clone())),
                (shell.id(), Some(child_origin))
            ]
        );
    }
}

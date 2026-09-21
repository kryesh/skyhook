//! Job presentation and live metadata queries.

use super::*;
use crate::tool::diagnostic::DiagnosticViewer;

/// An agent's live work, as an interrupt and a `wait` each need to see it.
#[derive(Default)]
pub(crate) struct LiveWork {
    /// Any live job, retained children and background work included.
    pub(crate) any: bool,
    /// Foreground non-agent jobs. They hold the turn and have no resume point, so an
    /// interrupt cancels them; retained children and background work survive it.
    pub(crate) blocking: Vec<JobId>,
    /// A foreground job, child agents included, that is working: neither parked in
    /// a `wait` nor suspended. While one exists the agent cannot act, so a `wait`
    /// defers to it.
    pub(crate) holding: Option<JobId>,
}

/// What a `wait` needs to decide, resolved in one pass over the job map.
pub(crate) struct WaitState {
    pub(crate) holding: Option<JobId>,
    /// Something is pending that this caller has not been shown yet.
    pub(crate) unseen: bool,
    /// The input revision this caller was last shown, if it is a script.
    pub(crate) seen_input: Option<u64>,
    /// The caller is a script's wait, not one the model called.
    pub(crate) hosted: bool,
    /// What to record as the caller's floor if it reports now.
    pub(crate) stamp: u64,
}

fn classify(jobs: &HashMap<JobId, JobEntry>, owner: &AgentId) -> LiveWork {
    // A job parked in a `wait`, and the same agent's jobs hosting it, are waiting
    // for events rather than doing work. Deferring to them would make concurrent
    // waits each sleep until the other ended.
    let mut parked = std::collections::HashSet::new();
    for (id, _) in jobs
        .iter()
        .filter(|(_, entry)| &entry.agent == owner && entry.awaiting_events && entry.live())
    {
        let mut next = Some(*id);
        while let Some(job) = next.filter(|job| parked.insert(*job)) {
            next = jobs
                .get(&job)
                .and_then(|entry| entry.parent)
                .filter(|host| jobs.get(host).is_some_and(|host| &host.agent == owner));
        }
    }
    let mut work = LiveWork::default();
    for (id, entry) in jobs
        .iter()
        .filter(|(_, entry)| &entry.agent == owner && entry.live())
    {
        work.any = true;
        if effectively_background(jobs, *id) {
            continue;
        }
        if entry.role != JobRole::Agent {
            work.blocking.push(*id);
        }
        if !parked.contains(id) && !entry.suspended() {
            work.holding.get_or_insert(*id);
        }
    }
    work
}

/// Whether `id` or any ancestor launched by the same agent is background: a
/// foreground call inside a background script does not hold the agent either.
/// Stops at the agent boundary, whose launch mode belongs to the parent agent.
pub(super) fn effectively_background(jobs: &HashMap<JobId, JobEntry>, id: JobId) -> bool {
    let mut next = jobs.get(&id);
    while let Some(entry) = next {
        if entry.background {
            return true;
        }
        next = entry
            .parent
            .and_then(|parent| jobs.get(&parent))
            .filter(|parent| parent.agent == entry.agent);
    }
    false
}

fn pending(jobs: &HashMap<JobId, JobEntry>, owner: &AgentId) -> bool {
    jobs.values()
        .any(|entry| &entry.agent == owner && entry.has_pending())
}

/// The script hosting a `wait`: what persists across its successive waits.
fn script_host(jobs: &HashMap<JobId, JobEntry>, caller: JobId) -> Option<JobId> {
    let host = jobs.get(&caller)?.parent?;
    (jobs.get(&host)?.role == JobRole::Script).then_some(host)
}

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
    /// Host projection derived from diagnostic facts, never persisted as authority.
    /// Capability-aware views re-render the facts rather than using this cache.
    pub error: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) diagnostic: Option<Diagnostic>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) output_diagnostic: Option<Diagnostic>,
    pub location: ExecutionLocation,
}

/// The single public wire contract for both model and JavaScript job responses.
/// Payload JSON is opaque; only these owned presentation groups are constructed.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub(crate) struct JobView {
    pub(crate) id: Option<JobId>,
    pub(crate) state: JobState,
    pub(crate) has_result: bool,
    pub(crate) result: Value,
    pub(crate) error: Option<String>,
    pub(crate) meta: Option<JobMetadata>,
    pub(crate) presentation: Option<Presentation>,
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub(crate) struct JobMetadata {
    pub(crate) parent: Option<JobId>,
    pub(crate) tool: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) target: Option<String>,
    /// Display metadata; execution keeps its native PathBuf in JobEnvelope.
    pub(crate) workspace: Option<String>,
    /// Source sequence of the last visible child reply.
    pub(crate) last_message: Option<u64>,
    pub(crate) code: Option<crate::tool::DenialCode>,
    pub(crate) executed: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, JsonSchema)]
pub(crate) struct Presentation {
    pub(crate) preview: Option<output::OutputPreview>,
    pub(crate) truncated: Vec<output::OutputTruncation>,
    pub(crate) captures: Vec<output::CaptureDescriptor>,
    /// A waiting child agent returns a question batch rather than a result.
    #[schemars(schema_with = "question_schema")]
    pub(crate) question: Option<Value>,
    pub(crate) notice: Option<String>,
}

impl Presentation {
    pub(crate) fn into_option(self) -> Option<Self> {
        (self.preview.is_some()
            || !self.truncated.is_empty()
            || !self.captures.is_empty()
            || self.question.is_some()
            || self.notice.is_some())
        .then_some(self)
    }
}

impl JobMetadata {
    /// A policy denial is marked so callers can branch without parsing the message.
    fn mark_denied(&mut self, denied: bool) {
        if denied {
            self.code = Some(crate::tool::DenialCode::PermissionDenied);
            self.executed = Some(false);
        }
    }
}

impl JobView {
    pub(crate) fn failure(
        message: String,
        output: Option<Value>,
        denied: bool,
        mut metadata: JobMetadata,
    ) -> Self {
        metadata.mark_denied(denied);
        Self {
            id: None,
            state: JobState::Failed,
            has_result: output.is_some(),
            result: output.unwrap_or(Value::Null),
            error: Some(message),
            meta: Some(metadata),
            presentation: None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("job view serializes")
    }
}

// Keep question fields discoverable without classifying extensible tool names.
fn question_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
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
    pub(crate) fn render_diagnostics<'a>(&mut self, viewer: impl Into<DiagnosticViewer<'a>>) {
        let viewer = viewer.into();
        self.error = self
            .diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.render_for(viewer));
        if let (Some(output), Some(diagnostic)) = (&mut self.output, &self.output_diagnostic) {
            render_output_diagnostic(output, diagnostic, viewer);
        }
    }

    pub(crate) fn rendered_error<'a>(
        &self,
        viewer: impl Into<DiagnosticViewer<'a>>,
    ) -> Option<String> {
        let viewer = viewer.into();
        self.diagnostic
            .as_ref()
            .map(|diagnostic| diagnostic.render_for(viewer))
            .or_else(|| self.error.clone())
    }

    /// Ordinary foreground responses omit redundant launch metadata on success.
    pub(crate) fn response_view<'a>(&self, viewer: impl Into<DiagnosticViewer<'a>>) -> JobView {
        let failed =
            self.error.is_some() || (self.state.is_terminal() && self.state != JobState::Completed);
        self.view(viewer.into(), failed)
    }

    /// Explicit inspection and background handles always retain launch metadata.
    pub(crate) fn metadata_view<'a>(&self, viewer: impl Into<DiagnosticViewer<'a>>) -> JobView {
        self.view(viewer.into(), true)
    }

    fn view(&self, viewer: DiagnosticViewer<'_>, metadata: bool) -> JobView {
        let capabilities = viewer.capabilities;
        let waiting = self.state == JobState::WaitingInput;
        JobView {
            id: Some(self.id),
            state: self.state.presented(),
            has_result: !waiting && self.output.is_some(),
            result: if waiting {
                Value::Null
            } else {
                let mut result = self.output.clone().unwrap_or(Value::Null);
                if let Some(diagnostic) = &self.output_diagnostic {
                    render_output_diagnostic(&mut result, diagnostic, viewer);
                }
                result
            },
            error: self.rendered_error(viewer),
            meta: metadata.then(|| {
                let mut metadata = JobMetadata {
                    parent: self.parent,
                    tool: Some(self.tool.clone()),
                    name: self.name.clone().filter(|name| !name.is_empty()),
                    target: capabilities
                        .contains(Capability::Targets)
                        .then(|| self.location.target.clone()),
                    workspace: Some(self.location.workspace.to_string_lossy().into_owned()),
                    last_message: None,
                    code: None,
                    executed: None,
                };
                metadata.mark_denied(self.diagnostic.as_ref().is_some_and(Diagnostic::is_denial));
                metadata
            }),
            presentation: Presentation {
                question: waiting.then(|| self.output.clone()).flatten(),
                ..Presentation::default()
            }
            .into_option(),
        }
    }
}

/// Only a producer-registered read error slot is presentation-owned. Arbitrary
/// user JSON, including similarly shaped errors, remains opaque.
pub(super) fn render_output_diagnostic(
    result: &mut Value,
    diagnostic: &Diagnostic,
    viewer: DiagnosticViewer<'_>,
) {
    if let Some(message) = result.pointer_mut("/error/message") {
        *message = Value::String(diagnostic.render_for(viewer));
    }
}

pub(crate) fn presented_job_schema(many: bool) -> Value {
    let generator = schemars::generate::SchemaSettings::default()
        .for_serialize()
        .into_generator();
    let schema = if many {
        generator.into_root_schema_for::<Vec<JobView>>()
    } else {
        generator.into_root_schema_for::<JobView>()
    };
    serde_json::to_value(schema).expect("job schema serializes")
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

    /// Read from job `id`'s entry under the jobs lock.
    pub(super) async fn entry<T>(
        &self,
        id: JobId,
        read: impl FnOnce(&JobEntry) -> T,
    ) -> Result<T, JobError> {
        let jobs = self.inner.jobs.lock().await;
        jobs.get(&id).map(read).ok_or(JobError::Unknown(id))
    }

    pub(crate) async fn metadata(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        self.entry(id, |entry| entry.metadata(id)).await
    }

    pub async fn snapshot(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        let mut envelope = self.entry(id, |entry| entry.envelope(id)).await?;
        self.hydrate_envelope(&mut envelope).await?;
        Ok(envelope)
    }

    pub(crate) async fn cancellation_token(
        &self,
        id: JobId,
    ) -> Result<CancellationToken, JobError> {
        self.entry(id, |entry| entry.cancellation.clone()).await
    }

    pub(crate) async fn authorization_scope(&self, id: JobId) -> Result<Option<u64>, JobError> {
        self.entry(id, |entry| entry.authorization_scope).await
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
        pending(&*self.inner.jobs.lock().await, owner)
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
            .any(|entry| &entry.agent == owner && entry.live())
    }

    pub(crate) async fn live_work(&self, owner: &AgentId) -> LiveWork {
        classify(&*self.inner.jobs.lock().await, owner)
    }

    /// Release `owner`'s suspended foreground jobs: they go on in the background,
    /// so waits on them return and their restarts report through delivery.
    pub(crate) async fn release_held(&self, owner: &AgentId) -> Vec<JobId> {
        let mut jobs = self.inner.jobs.lock().await;
        let held: Vec<_> = jobs
            .iter()
            .filter(|(id, entry)| {
                &entry.agent == owner && entry.suspended() && !effectively_background(&jobs, **id)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &held {
            let entry = jobs.get_mut(id).expect("selected above");
            entry.background = true;
            entry.notify.notify_waiters();
        }
        held
    }

    /// Whether `owner` has a suspended foreground job to release.
    pub(crate) async fn has_suspended(&self, owner: &AgentId) -> bool {
        let jobs = self.inner.jobs.lock().await;
        jobs.iter().any(|(id, entry)| {
            &entry.agent == owner && entry.suspended() && !effectively_background(&jobs, *id)
        })
    }

    /// Whether the job ended, other than by a retained interruption.
    pub(crate) async fn settled(&self, id: JobId) -> bool {
        self.entry(id, |entry| entry.state.is_terminal() && !entry.suspended())
            .await
            .unwrap_or(true)
    }

    /// Mark `caller` parked and resolve its decision under one lock, so concurrent
    /// waits classify each other consistently.
    pub(crate) async fn wait_state(&self, owner: &AgentId, caller: JobId) -> WaitState {
        let mut jobs = self.inner.jobs.lock().await;
        if let Some(entry) = jobs.get_mut(&caller)
            && !std::mem::replace(&mut entry.awaiting_events, true)
        {
            self.parked_signal(owner).notify_waiters();
        }
        let host = script_host(&jobs, caller);
        let floor = host.and_then(|host| jobs.get(&host)?.wait_floor);
        let since = floor.map_or(0, |(stamp, _)| stamp);
        WaitState {
            hosted: host.is_some(),
            holding: classify(&jobs, owner).holding,
            unseen: jobs
                .values()
                .any(|entry| &entry.agent == owner && entry.pending_since(since)),
            seen_input: floor.map(|(_, input)| input),
            stamp: current_pending_stamp(),
        }
    }

    /// Resolves once one of `owner`'s jobs next parks in a `wait`. Take it before
    /// `wait_state` so a park in between is not missed.
    pub(crate) fn parked(&self, owner: &AgentId) -> tokio::sync::futures::OwnedNotified {
        self.parked_signal(owner).notified_owned()
    }

    fn parked_signal(&self, owner: &AgentId) -> Arc<Notify> {
        self.inner
            .parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(owner.clone())
            .or_default()
            .clone()
    }

    pub(crate) async fn set_wait_floor(&self, caller: JobId, floor: (u64, u64)) {
        let mut jobs = self.inner.jobs.lock().await;
        if let Some(host) = script_host(&jobs, caller).and_then(|host| jobs.get_mut(&host)) {
            host.wait_floor = Some(floor);
        }
    }

    pub(crate) async fn is_effectively_background(&self, id: JobId) -> Result<bool, JobError> {
        let jobs = self.inner.jobs.lock().await;
        if !jobs.contains_key(&id) {
            return Err(JobError::Unknown(id));
        }
        Ok(effectively_background(&jobs, id))
    }

    pub async fn is_background(&self, id: JobId) -> Result<bool, JobError> {
        self.entry(id, |entry| entry.background).await
    }

    pub async fn images(&self, id: JobId) -> Result<Vec<ImageRef>, JobError> {
        self.entry(id, |entry| entry.images.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(output: Option<Value>) -> JobEnvelope {
        JobEnvelope {
            id: JobId::new(1).unwrap(),
            parent: None,
            tool: "fixture".into(),
            role: JobRole::Tool,
            name: None,
            state: JobState::Completed,
            output,
            error: None,
            diagnostic: None,
            output_diagnostic: None,
            location: ExecutionLocation::root(std::path::PathBuf::from("/work")),
        }
    }

    #[test]
    fn plain_process_response_keeps_model_text_under_two_hundred_bytes() {
        let job = envelope(Some(serde_json::json!({
            "exit_code":0, "stdout":"", "stderr":"", "timed_out":false
        })));
        let view = job.response_view(&CapabilitySet::default()).into_value();
        let wire =
            serde_json::to_vec(&serde_json::json!({"result":view,"is_error":false})).unwrap();
        assert!(
            wire.len() < 200,
            "plain response grew to {} bytes",
            wire.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn display_metadata_with_non_utf8_workspace_is_json_serializable() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut job = envelope(None);
        job.location.workspace = std::ffi::OsString::from_vec(b"/work/\xff".to_vec()).into();
        let view = job.metadata_view(&CapabilitySet::default()).into_value();
        assert_eq!(view["meta"]["workspace"], "/work/\u{fffd}");
    }

    #[test]
    fn compact_responses_preserve_payload_nulls_and_explicit_inspection_metadata() {
        let capabilities = CapabilitySet::default();
        let payload =
            serde_json::json!({"nested": null, "array": [null], "presentation": {"preview": null}});
        let job = envelope(Some(payload.clone()));
        let view = job.response_view(&capabilities).into_value();
        assert_eq!(
            view,
            serde_json::json!({
                "id":1, "state":"completed", "has_result":true,
                "result":payload, "error":null, "meta":null, "presentation":null
            })
        );
        let full = job.metadata_view(&capabilities).into_value();
        assert_eq!(full["result"], view["result"]);
        assert_eq!(full["meta"]["tool"], "fixture");
        assert!(full["meta"].get("target").unwrap().is_null());
        let null = envelope(Some(Value::Null))
            .response_view(&capabilities)
            .into_value();
        assert_eq!(null["has_result"], true);
        let unavailable = envelope(None).metadata_view(&capabilities).into_value();
        assert_eq!(unavailable["has_result"], false);
        assert_eq!(unavailable["result"], Value::Null);
    }

    #[test]
    fn serialization_schema_requires_nullable_keys_in_every_group() {
        let schema = presented_job_schema(false);
        let validator = jsonschema::validator_for(&schema).unwrap();
        let capabilities = CapabilitySet::default();
        let mut job = envelope(Some(serde_json::json!({"nested":null})));
        for (state, full) in [
            (JobState::Completed, false),
            (JobState::Completed, true),
            (JobState::WaitingInput, true),
        ] {
            job.state = state;
            let view = if full {
                job.metadata_view(&capabilities)
            } else {
                job.response_view(&capabilities)
            }
            .into_value();
            assert!(validator.is_valid(&view));
            for group in ["", "/meta", "/presentation"] {
                if let Some(fields) = view.pointer(group).and_then(Value::as_object) {
                    for key in fields.keys() {
                        let mut missing = view.clone();
                        missing
                            .pointer_mut(group)
                            .unwrap()
                            .as_object_mut()
                            .unwrap()
                            .remove(key);
                        assert!(!validator.is_valid(&missing), "{group}/{key}");
                    }
                }
            }
        }
    }

    #[test]
    fn failures_include_metadata_with_or_without_admission() {
        let mut job = envelope(Some(Value::Null));
        job.state = JobState::Failed;
        job.diagnostic = Some(crate::tool::ToolError::Denied("failed".into()).diagnostic());
        let view = job.response_view(&CapabilitySet::default()).into_value();
        assert_eq!(view["meta"]["code"], "permission_denied");
        assert_eq!(view["meta"]["executed"], false);
        let failure = JobView::failure(
            "denied".into(),
            Some(Value::Null),
            true,
            JobMetadata::default(),
        )
        .into_value();
        assert_eq!(failure["id"], Value::Null);
        assert_eq!(failure["has_result"], true);
        assert!(jsonschema::is_valid(&presented_job_schema(false), &failure));
    }

    #[test]
    fn question_schema_documents_batches_without_classifying_tool_names() {
        for many in [false, true] {
            let schema = presented_job_schema(many);
            assert!(
                schema["$defs"]["QuestionOutput"]["properties"]
                    .get("questions")
                    .is_some()
            );
            for (tool, output) in [
                (
                    "delegate",
                    serde_json::json!({"questions":[{"id":"choice", "prompt":"Choose"}]}),
                ),
                ("agent", serde_json::json!("custom prompt")),
            ] {
                let mut job = envelope(Some(output.clone()));
                job.state = JobState::WaitingInput;
                job.tool = tool.into();
                let job = job.metadata_view(&CapabilitySet::default()).into_value();
                assert_eq!(job["presentation"]["question"], output);
                assert_eq!(job["has_result"], false);
                let value = if many { serde_json::json!([job]) } else { job };
                assert!(jsonschema::is_valid(&schema, &value));
            }
        }
    }

    #[tokio::test]
    async fn active_launches_stop_at_agent_boundaries_and_prefer_the_nearest_call() {
        let (root, jobs, agent) = super::super::tests::runtime().await;
        // Origins name committed assistant calls.
        let call = async |agent: &AgentId, id: &str| {
            let call = crate::provider::protocol::ToolCall::new(id, "agent", serde_json::json!({}))
                .unwrap();
            let message = crate::provider::protocol::Message::Assistant(vec![
                crate::provider::protocol::AssistantItem::tool_call(id, 0, call),
            ]);
            let event = crate::session::SessionEvent::MessageCommitted { message };
            crate::session::ModelCallOrigin {
                message: jobs.test_append(agent.clone(), event).await,
                call_id: id.into(),
            }
        };
        let root_origin = call(&agent, "delegate").await;
        let spec = |agent: &AgentId, tool, parent, origin| JobSpec {
            parent,
            origin,
            ..JobSpec::test(agent.clone(), tool)
        };
        let owner = jobs
            .test_lease(spec(&agent, "agent", None, Some(root_origin.clone())))
            .await;
        let store = jobs.store();
        let child_agent =
            crate::session::fixture::start_child(store, &agent, 1, Some(owner.id()), root.path())
                .await;
        let child_origin = call(&child_agent, "child-script").await;
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

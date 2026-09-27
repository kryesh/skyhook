//! Job presentation and live metadata queries.
use super::*;
use crate::session::StateJob;
use crate::tool::{diagnostic::DiagnosticViewer, output::complete};

/// An agent's live work, as an interrupt and a `wait` each need to see it.
#[derive(Default)]
pub(crate) struct LiveWork {
    /// A foreground child agent, which an interrupt retains.
    pub(crate) children: bool,
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
    pub(crate) caller: WaitCaller,
    /// What to record as the caller's floor if it reports now.
    pub(crate) stamp: u64,
}

/// Who called `wait`: the model, whose request boundary consumes what it is
/// shown, or a script, which is shown each event once.
pub(crate) enum WaitCaller {
    Model,
    Script { floor: Option<WaitFloor> },
}

fn classify(jobs: &HashMap<JobId, JobEntry>, owner: &AgentId) -> LiveWork {
    // A job parked in a `wait`, and the same agent's jobs hosting it, are waiting
    // for events rather than doing work. Deferring to them would make concurrent
    // waits each sleep until the other ended.
    let mut parked = std::collections::HashSet::new();
    for (id, _) in jobs.iter().filter(|(_, entry)| {
        &entry.agent == owner
            && matches!(entry.role, RoleState::Wait { parked: true })
            && entry.live()
    }) {
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
        if effectively_background(jobs, *id) {
            continue;
        }
        match entry.child() {
            Some(_) => work.children = true,
            None => work.blocking.push(*id),
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
fn script_host(
    jobs: &mut HashMap<JobId, JobEntry>,
    caller: JobId,
) -> Option<&mut Option<WaitFloor>> {
    let host = jobs.get(&caller)?.parent?;
    match &mut jobs.get_mut(&host)?.role {
        RoleState::Script { wait_floor } => Some(wait_floor),
        _ => None,
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct JobEnvelope {
    pub id: JobId,
    pub parent: Option<JobId>,
    pub tool: String,
    #[serde(default)]
    pub role: JobRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub name: Option<JobName>,
    pub state: JobState,
    pub output: Option<Value>,
    /// The model call that launched the job; its parent is then the calling
    /// agent's own.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) origin: Option<crate::session::ModelCallOrigin>,
    /// The question a waiting job asks, until its owner has seen it.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) question: Option<QuestionOutput>,
    /// Failure facts; every viewer renders them under its own capabilities.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) diagnostic: Option<Diagnostic>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) output_diagnostic: Option<Diagnostic>,
    pub location: ExecutionLocation,
    /// Input would resume it after it ends.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) resumable: bool,
}

/// The single public wire contract for both model and JavaScript job responses.
/// Payload JSON is opaque; only these owned presentation groups are constructed.
///
/// Every absent optional is omitted rather than null, across every group. A
/// completed response with nothing more to read or resume omits `id` and `state`.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct JobView {
    #[schemars(with = "JobId")]
    pub(crate) id: Option<JobId>,
    #[schemars(with = "JobState")]
    pub(crate) state: Option<JobState>,
    /// Precedes the result, so a reader learns what was left out before reading it.
    #[schemars(with = "Presentation")]
    pub(crate) presentation: Option<Presentation>,
    /// A present `null` is a real result, distinct from no result.
    #[serde(default, deserialize_with = "present")]
    pub(crate) result: Option<Value>,
    #[schemars(with = "String", transform = complete)]
    pub(crate) error: Option<String>,
    #[schemars(with = "JobMetadata")]
    pub(crate) meta: Option<JobMetadata>,
}

fn present<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(value).map(Some)
}

/// Launch facts the viewer does not already know: `parent`, `target` and
/// `workspace` only when they differ from the viewing agent's own.
#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct JobMetadata {
    #[schemars(with = "JobId")]
    pub(crate) parent: Option<JobId>,
    #[schemars(with = "String")]
    pub(crate) tool: Option<String>,
    #[schemars(with = "String")]
    pub(crate) name: Option<JobName>,
    #[schemars(with = "String")]
    pub(crate) target: Option<String>,
    /// Display metadata; execution keeps its native PathBuf in JobEnvelope.
    #[schemars(with = "String")]
    pub(crate) workspace: Option<String>,
    #[schemars(with = "crate::tool::DenialCode")]
    pub(crate) code: Option<crate::tool::DenialCode>,
}

#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Presentation {
    #[schemars(with = "output::OutputPreview")]
    pub(crate) preview: Option<output::OutputPreview>,
    /// The whole result's type, with counts, when samples leave parts out.
    pub(crate) shape: Option<Value>,
    #[schemars(with = "Vec<output::OutputTruncation>")]
    pub(crate) truncated: Option<Vec<output::OutputTruncation>>,
    /// Captures a presented result does not already show in full.
    #[schemars(with = "Vec<output::CaptureDescriptor>")]
    pub(crate) captures: Option<Vec<output::CaptureDescriptor>>,
    /// A waiting child agent returns a question batch rather than a result.
    #[schemars(with = "QuestionOutput", transform = complete)]
    pub(crate) question: Option<QuestionOutput>,
    // The schema keeps the plain string the system prompt's JobView type shows.
    #[schemars(with = "String")]
    pub(crate) notice: Option<Notice>,
}

crate::named_enum::named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Notice {
        OutputIncomplete = "Output incomplete.",
    }
}

impl Presentation {
    pub fn preview(&self) -> Option<&output::OutputPreview> {
        self.preview.as_ref()
    }

    pub fn shape(&self) -> Option<&Value> {
        self.shape.as_ref()
    }

    pub fn truncated(&self) -> &[output::OutputTruncation] {
        self.truncated.as_deref().unwrap_or_default()
    }

    pub fn captures(&self) -> &[output::CaptureDescriptor] {
        self.captures.as_deref().unwrap_or_default()
    }

    pub fn questions(&self) -> &[crate::agent::Question] {
        self.question.as_ref().map_or(&[], |batch| &batch.questions)
    }

    pub fn notice(&self) -> Option<Notice> {
        self.notice
    }

    pub(crate) fn into_option(self) -> Option<Self> {
        (self != Self::default()).then_some(self)
    }
}

impl JobMetadata {
    fn into_option(self) -> Option<Self> {
        (self != Self::default()).then_some(self)
    }
}

/// A policy denial is marked so callers can branch without parsing the message.
fn denial_code(denied: bool) -> Option<crate::tool::DenialCode> {
    denied.then_some(crate::tool::DenialCode::PermissionDenied)
}

/// Whether a view answers the viewer's own foreground call, which already names
/// the tool and job, or shows a job from elsewhere: a background handle, a
/// listing, an inspection or an event.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewKind {
    Response,
    Inspection,
}

impl JobView {
    /// None when the call settled before a job was published, or completed with
    /// nothing more to read or resume.
    pub fn id(&self) -> Option<JobId> {
        self.id
    }

    pub fn state(&self) -> JobState {
        self.state.unwrap_or(JobState::Completed)
    }

    pub fn tool(&self) -> Option<&str> {
        self.meta.as_ref().and_then(|meta| meta.tool.as_deref())
    }

    /// None when the job has produced no result, rather than a JSON null result.
    pub fn result(&self) -> Option<&Value> {
        self.result.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn presentation(&self) -> Option<&Presentation> {
        self.presentation.as_ref()
    }

    /// The view without its result, error, preview, notice, question or capture
    /// pages; None when nothing else is left.
    pub fn envelope(&self) -> Option<Self> {
        let presentation = self.presentation.as_ref().and_then(|presentation| {
            let captures = presentation.captures.as_ref().map(|captures| {
                captures
                    .iter()
                    .map(|capture| output::CaptureDescriptor {
                        output: None,
                        ..capture.clone()
                    })
                    .collect()
            });
            Presentation {
                shape: presentation.shape.clone(),
                truncated: presentation.truncated.clone(),
                captures,
                ..Presentation::default()
            }
            .into_option()
        });
        let envelope = Self {
            id: self.id,
            state: self.state,
            meta: self.meta.clone(),
            presentation,
            ..Self::default()
        };
        (envelope != Self::default()).then_some(envelope)
    }

    /// Where the next saved-source page starts: the selected preview's
    /// continuation, then the first truncated field's, then the first
    /// continuing capture page's. A preview without a field continues the
    /// field that was requested.
    pub fn continuation(&self) -> Option<output::Continuation<'_>> {
        let presentation = self.presentation.as_ref()?;
        let preview = presentation.preview.as_ref();
        preview
            .and_then(output::OutputPreview::continuation)
            .or_else(|| {
                (presentation.truncated().iter()).find_map(output::OutputTruncation::continuation)
            })
            .or_else(|| {
                presentation
                    .captures()
                    .iter()
                    .filter_map(|capture| capture.output.as_deref())
                    .find_map(JobView::continuation)
            })
    }

    pub(crate) fn failure(message: String, output: Option<Value>, denied: bool) -> Self {
        Self {
            id: None,
            state: Some(JobState::Failed),
            result: output,
            error: Some(message),
            meta: JobMetadata {
                code: denial_code(denied),
                ..JobMetadata::default()
            }
            .into_option(),
            presentation: None,
        }
    }

    pub(crate) fn into_value(self) -> Value {
        serde_json::to_value(self).expect("job view serializes")
    }
}

impl JobEnvelope {
    /// Render the output diagnostic into its slot for `viewer`.
    pub(crate) fn render_output_diagnostic<'a>(&mut self, viewer: impl Into<DiagnosticViewer<'a>>) {
        if let (Some(output), Some(diagnostic)) = (&mut self.output, &self.output_diagnostic) {
            render_output_diagnostic(output, diagnostic, viewer.into());
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
    }

    /// The viewer's own foreground call: launch metadata only on failure.
    pub(crate) fn response_view<'a>(&self, viewer: impl Into<DiagnosticViewer<'a>>) -> JobView {
        let viewer = viewer.into();
        let result = self.presented_result(viewer);
        self.view(viewer, ViewKind::Response, result, Presentation::default())
    }

    /// Background handles, listings and inspection always retain launch metadata.
    pub(crate) fn metadata_view<'a>(&self, viewer: impl Into<DiagnosticViewer<'a>>) -> JobView {
        let viewer = viewer.into();
        let result = self.presented_result(viewer);
        self.view(
            viewer,
            ViewKind::Inspection,
            result,
            Presentation::default(),
        )
    }

    fn presented_result(&self, viewer: DiagnosticViewer<'_>) -> Option<Value> {
        self.output.clone().map(|mut result| {
            if let Some(diagnostic) = &self.output_diagnostic {
                render_output_diagnostic(&mut result, diagnostic, viewer);
            }
            result
        })
    }

    pub(super) fn view(
        &self,
        viewer: DiagnosticViewer<'_>,
        kind: ViewKind,
        result: Option<Value>,
        presentation: Presentation,
    ) -> JobView {
        let error = self.rendered_error(viewer);
        let failed =
            error.is_some() || (self.state.is_terminal() && self.state != JobState::Completed);
        let presentation = Presentation {
            question: presentation.question.or_else(|| self.question.clone()),
            ..presentation
        }
        .into_option();
        let settled = kind == ViewKind::Response
            && !failed
            && self.state == JobState::Completed
            && !self.resumable
            && presentation.is_none();
        let inspection = kind == ViewKind::Inspection;
        let own = viewer.location();
        let target = viewer
            .capabilities
            .visible_target(&self.location.target)
            .filter(|_| own.is_none_or(|own| own.target != self.location.target));
        let workspace = (own.is_none_or(|own| own.workspace != self.location.workspace))
            .then(|| self.location.workspace.to_string_lossy().into_owned());
        let meta = (inspection || failed).then(|| JobMetadata {
            parent: self.parent.filter(|_| inspection && self.origin.is_none()),
            tool: inspection.then(|| self.tool.clone()),
            name: self.name.clone().filter(|_| inspection),
            target: target.map(ToString::to_string),
            workspace,
            code: denial_code(self.diagnostic.as_ref().is_some_and(Diagnostic::is_denial)),
        });
        JobView {
            id: (!settled).then_some(self.id),
            state: (!settled).then_some(self.state.presented()),
            result,
            error,
            meta: meta.and_then(JobMetadata::into_option),
            presentation,
        }
    }
}

fn render_output_diagnostic(
    result: &mut Value,
    diagnostic: &Diagnostic,
    viewer: DiagnosticViewer<'_>,
) {
    if let Some(message) = output::diagnostic_slot_in(result) {
        *message = Value::String(diagnostic.render_for(viewer));
    }
}

/// The schemas of one job view and of a list of them, as results declare them.
/// A job view is itself a presentation; presenting it again never shortens it.
pub(crate) struct JobViewSchemas {
    pub(crate) one: Value,
    pub(crate) many: Value,
    /// A tool response's own view, which keeps its error and questions whole.
    pub(crate) response: Value,
}

pub(crate) static JOB_VIEW_SCHEMAS: std::sync::LazyLock<JobViewSchemas> =
    std::sync::LazyLock::new(|| {
        use crate::tool::registry::{complete_result_schema, result_schema};
        JobViewSchemas {
            one: complete_result_schema::<JobView>(),
            many: complete_result_schema::<Vec<JobView>>(),
            response: result_schema::<JobView>(),
        }
    });

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
            .filter(|(_, entry)| &entry.agent == agent && entry.end().is_none())
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
        envelope.render_output_diagnostic(&CapabilitySet::default());
        Ok(envelope)
    }

    pub(crate) async fn cancellation_token(
        &self,
        id: JobId,
    ) -> Result<CancellationToken, JobError> {
        self.entry(id, |entry| entry.cancellation.clone()).await
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
    ) -> Vec<StateJob> {
        let mut progress = self.inner.progress.lock().await;
        self.inner
            .store
            .visit_records_after(progress.sequence, |records| progress.project(records))
            .await;
        let jobs = self.inner.jobs.lock().await;
        let mut states = jobs
            .iter()
            .filter(|(_, entry)| &entry.agent == owner && entry.end().is_none())
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
        self.entry(id, JobEntry::settled).await.unwrap_or(true)
    }

    /// Mark `caller` parked and resolve its decision under one lock, so concurrent
    /// waits classify each other consistently.
    pub(crate) async fn wait_state(&self, owner: &AgentId, caller: JobId) -> WaitState {
        let mut jobs = self.inner.jobs.lock().await;
        if let Some(RoleState::Wait { parked }) = jobs.get_mut(&caller).map(|entry| &mut entry.role)
            && !std::mem::replace(parked, true)
        {
            self.parked_signal(owner).notify_waiters();
        }
        let caller = match script_host(&mut jobs, caller) {
            Some(floor) => WaitCaller::Script { floor: *floor },
            None => WaitCaller::Model,
        };
        let since = match caller {
            WaitCaller::Script { floor } => floor.map_or(0, |floor| floor.stamp),
            WaitCaller::Model => 0,
        };
        WaitState {
            holding: classify(&jobs, owner).holding,
            unseen: jobs
                .values()
                .any(|entry| &entry.agent == owner && entry.pending_since(since)),
            caller,
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

    pub(crate) async fn set_wait_floor(&self, caller: JobId, floor: WaitFloor) {
        let mut jobs = self.inner.jobs.lock().await;
        if let Some(host) = script_host(&mut jobs, caller) {
            *host = Some(floor);
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
        self.entry(id, |entry| {
            entry
                .finished()
                .map_or_else(Vec::new, |finished| finished.images.clone())
        })
        .await
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
            origin: None,
            question: None,
            diagnostic: None,
            output_diagnostic: None,
            location: ExecutionLocation::root(std::path::PathBuf::from("/work")),
            resumable: false,
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
    fn views_omit_absent_fields_and_empty_lists_but_keep_a_null_result() {
        let capabilities = CapabilitySet::default();
        let validator = jsonschema::validator_for(&JOB_VIEW_SCHEMAS.one).unwrap();
        let payload =
            serde_json::json!({"nested": null, "array": [null], "presentation": {"preview": null}});
        let job = envelope(Some(payload.clone()));
        let view = job.response_view(&capabilities).into_value();
        assert_eq!(view, serde_json::json!({"result":payload}));
        let full = job.metadata_view(&capabilities).into_value();
        assert_eq!(
            full["meta"],
            serde_json::json!({"tool":"fixture", "workspace":"/work"})
        );
        let null = envelope(Some(Value::Null)).response_view(&capabilities);
        let missing = envelope(None).metadata_view(&capabilities);
        let noticed = JobView {
            presentation: Some(Presentation {
                notice: Some(Notice::OutputIncomplete),
                ..Presentation::default()
            }),
            ..missing.clone()
        };
        assert_eq!(
            noticed.clone().into_value()["presentation"],
            serde_json::json!({"notice":"Output incomplete."})
        );
        assert_eq!(null.clone().into_value().get("result"), Some(&Value::Null));
        assert_eq!(missing.clone().into_value().get("result"), None);
        for view in [null, missing, noticed] {
            let value = view.clone().into_value();
            assert!(validator.is_valid(&value), "{value}");
            assert_eq!(serde_json::from_value::<JobView>(value).unwrap(), view);
        }
    }

    /// An agent's views omit what it already knows: its own call's tool and name,
    /// its own parent, target and workspace, and a completed response's handle.
    #[test]
    fn agent_views_show_only_launch_facts_that_differ_from_the_viewer() {
        let capabilities = CapabilitySet::default();
        let own = ExecutionLocation::root(std::path::PathBuf::from("/work"));
        let viewer = DiagnosticViewer::new(&capabilities, &own);
        let mut job = envelope(Some(Value::Null));
        job.parent = JobId::new(9).ok();
        job.name = Some("reader".parse().unwrap());
        job.origin = Some(crate::session::ModelCallOrigin {
            message: 1.into(),
            call_id: "call".into(),
        });
        assert_eq!(
            job.metadata_view(viewer).meta,
            Some(JobMetadata {
                tool: Some("fixture".into()),
                name: Some("reader".parse().unwrap()),
                ..JobMetadata::default()
            })
        );
        job.origin = None;
        job.location.workspace = "/elsewhere".into();
        let meta = job.metadata_view(viewer).meta.unwrap();
        assert_eq!(
            (meta.parent, meta.workspace.as_deref()),
            (JobId::new(9).ok(), Some("/elsewhere"))
        );
        job.state = JobState::Failed;
        job.diagnostic = Some(crate::tool::ToolError::denied("no").diagnostic());
        assert_eq!(
            job.response_view(viewer).meta,
            Some(JobMetadata {
                workspace: Some("/elsewhere".into()),
                code: Some(crate::tool::DenialCode::PermissionDenied),
                ..JobMetadata::default()
            })
        );
    }

    /// A completed job that input resumes keeps the handle a follow-up needs.
    #[tokio::test]
    async fn a_resumable_completed_response_keeps_its_handle() {
        let (_root, jobs, agent) = super::super::tests::runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(agent, "agent")
        };
        let id = jobs.test_running(spec).await.into_test_id();
        let handler: ResumeHandler = Arc::new(|_, _| Box::pin(async { Ok(ToolOutput::default()) }));
        jobs.set_resume_handler(id, handler).await.unwrap();
        jobs.test_finish(id, Value::Null).await;
        let view = async || {
            let envelope = jobs.snapshot(id).await.unwrap();
            envelope.response_view(&CapabilitySet::default())
        };
        let resumable = view().await;
        assert_eq!(
            (resumable.id(), resumable.state),
            (Some(id), Some(JobState::Completed))
        );
        jobs.clear_resume_handler(id).await;
        assert_eq!(view().await.id(), None);
    }

    #[test]
    fn continuation_prefers_the_preview_then_a_truncation_then_a_capture_page() {
        let field = |field: &str| field.parse::<FieldPointer>().unwrap();
        let page = |at: &str, next_start| {
            output::OutputPreview::Lines(output::LinePage {
                field: Some(field(at)),
                lines: output::PageLines::Text(Vec::new()),
                total_lines: None,
                next_start,
                next_offset: Some(5),
            })
        };
        let view = |presentation| JobView {
            presentation: Some(presentation),
            ..JobView::failure(String::new(), None, false)
        };
        let capture = |at: &str, next_start| output::CaptureDescriptor {
            field: field(at),
            complete: false,
            output: Some(Box::new(view(Presentation {
                preview: Some(page(at, next_start)),
                ..Presentation::default()
            }))),
        };
        let mut presentation = Presentation {
            preview: Some(page("", Some(1))),
            truncated: Some(vec![
                output::OutputTruncation::Elements {
                    field: field("/result/items"),
                    shown: 2,
                    total_elements: 9,
                    kept: None,
                },
                output::OutputTruncation::Text {
                    field: field("/result/stdout"),
                    total_lines: 9,
                    next_start: 4,
                    next_offset: Some(7),
                },
            ]),
            captures: Some(vec![
                capture("/result/end", None),
                capture("/result/custom", Some(8)),
            ]),
            ..Presentation::default()
        };
        let next = |presentation: &Presentation| {
            let view = view(presentation.clone());
            let next = view.continuation();
            next.map(|next| match next {
                output::Continuation::Lines {
                    field,
                    start,
                    offset,
                } => (field.unwrap().as_str().to_owned(), start, offset),
                output::Continuation::Index { field, index } => {
                    (field.unwrap().as_str().to_owned(), index, None)
                }
            })
        };
        assert_eq!(next(&presentation), Some((String::new(), 1, Some(5))));
        presentation.preview = None;
        // An array cut continues at its first element not shown.
        assert_eq!(next(&presentation), Some(("/result/items".into(), 2, None)));
        presentation.truncated.as_mut().unwrap().remove(0);
        assert_eq!(
            next(&presentation),
            Some(("/result/stdout".into(), 4, Some(7)))
        );
        presentation.truncated = None;
        assert_eq!(
            next(&presentation),
            Some(("/result/custom".into(), 8, Some(5)))
        );
        presentation.captures = None;
        assert_eq!(next(&presentation), None);
    }

    #[test]
    fn failures_include_metadata_with_or_without_admission() {
        let mut job = envelope(Some(Value::Null));
        job.state = JobState::Failed;
        job.diagnostic = Some(crate::tool::ToolError::denied("failed").diagnostic());
        let view = job.response_view(&CapabilitySet::default()).into_value();
        assert_eq!(view["meta"]["code"], "permission_denied");
        let failure = JobView::failure("denied".into(), Some(Value::Null), true).into_value();
        assert_eq!(failure.get("id"), None);
        assert_eq!(failure.get("result"), Some(&Value::Null));
        assert!(jsonschema::is_valid(&JOB_VIEW_SCHEMAS.one, &failure));
    }

    #[test]
    fn a_waiting_job_presents_its_question_batch_in_place_of_a_result() {
        for (schema, many) in [
            (&JOB_VIEW_SCHEMAS.one, false),
            (&JOB_VIEW_SCHEMAS.many, true),
        ] {
            assert!(
                schema["$defs"]["QuestionOutput"]["properties"]
                    .get("questions")
                    .is_some()
            );
            let mut job = envelope(None);
            job.state = JobState::WaitingInput;
            job.question = Some(crate::job::tests::question("choice"));
            let job = job.metadata_view(&CapabilitySet::default()).into_value();
            assert_eq!(
                job["presentation"]["question"]["questions"][0]["id"],
                "choice"
            );
            assert_eq!(job.get("result"), None);
            let value = if many { serde_json::json!([job]) } else { job };
            assert!(jsonschema::is_valid(schema, &value));
        }
    }

    #[tokio::test]
    async fn active_launches_stop_at_agent_boundaries_and_prefer_the_nearest_call() {
        let (root, jobs, agent) = super::super::tests::runtime().await;
        // Origins name committed assistant calls.
        let call = async |agent: &AgentId, id: &str| {
            let call = crate::provider::protocol::ToolCall::new(id, "agent", serde_json::json!({}))
                .unwrap();
            let message = crate::session::Message::Assistant(vec![
                crate::provider::protocol::AssistantItem::tool_call(id, 0, call),
            ]);
            let event = crate::session::SessionEvent::MessageCommitted { message };
            crate::session::ModelCallOrigin {
                message: jobs.test_append(agent.clone(), event).await.message(),
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
            crate::session::tests::start_child(store, &agent, 1, Some(owner.id()), root.path())
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
            .test_lease(spec(&child_agent, "exec", Some(child.id()), None))
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

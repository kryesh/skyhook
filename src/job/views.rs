//! Job presentation and live metadata queries.
use super::*;
use crate::session::StateJob;
use crate::tool::{diagnostic::DiagnosticViewer, output::complete};

/// An agent's live work, as an interrupt and a `wait` each need to see it.
#[derive(Default)]
pub(crate) struct LiveWork {
    /// A foreground child agent that an interrupt retains: one no blocking job
    /// launched, as cancelling that job would cancel it too.
    pub(crate) children: bool,
    /// Foreground non-agent jobs. They hold the turn and have no resume point, so an
    /// interrupt cancels them; retained children and background work survive it.
    pub(crate) blocking: Vec<JobId>,
    /// A foreground job, child agents included, that is working: neither parked in
    /// a `wait` nor suspended. While one exists the agent cannot act, so a `wait`
    /// defers to it.
    pub(crate) holding: Option<JobId>,
}

/// What a `wait` decides on, as its owner's jobs stand.
pub(crate) enum Waiting {
    /// Its caller was cancelled.
    Cancelled,
    /// Foreground work holds the agent, which cannot act until it returns.
    Deferred,
    Open {
        /// Something is pending that the caller has not been shown yet.
        unseen: bool,
        /// The input revision a script caller was last shown; the model is shown
        /// input at its request boundary instead.
        seen_input: Option<u64>,
        /// The newest pending stamp, which a script shown this decision has seen.
        stamp: u64,
    },
}

impl Jobs {
    pub(crate) fn live_work(&self, owner: &AgentId) -> LiveWork {
        // A job parked in a `wait`, and the same agent's jobs hosting it, are waiting
        // for events rather than doing work. Deferring to them would make concurrent
        // waits each sleep until the other ended.
        let mut parked = std::collections::HashSet::new();
        let waits = (self.live(owner))
            .filter(|(_, entry)| matches!(entry.role, RoleState::Wait { parked: true }));
        for (id, _) in waits {
            let mut next = Some(id);
            while let Some(job) = next.filter(|job| parked.insert(*job)) {
                next = (self.get(job))
                    .and_then(|entry| entry.parent)
                    .filter(|host| self.get(*host).is_some_and(|host| &host.agent == owner));
            }
        }
        let mut work = LiveWork::default();
        for (id, entry) in self.live(owner) {
            if effectively_background(self, id) {
                continue;
            }
            match entry.child() {
                Some(_) if !launched_by_blocking(self, entry) => work.children = true,
                Some(_) => {}
                None => work.blocking.push(id),
            }
            if !parked.contains(&id) && !entry.suspended() {
                work.holding.get_or_insert(id);
            }
        }
        work
    }
}

/// Whether `entry` descends from blocking work: a live non-agent job of the same
/// agent, foreground wherever `entry` is.
pub(super) fn launched_by_blocking(jobs: &Jobs, entry: &JobFacts) -> bool {
    let mut next = entry.parent.and_then(|parent| jobs.get(parent));
    while let Some(host) = next.filter(|host| host.agent == entry.agent) {
        if host.live() && host.child().is_none() {
            return true;
        }
        next = host.parent.and_then(|parent| jobs.get(parent));
    }
    false
}

/// What releasing `links` journals: they go on in the background, so waits on
/// them return and their outcomes report through delivery.
pub(crate) fn release_records(links: &[(JobId, AgentId)]) -> Vec<(AgentId, SessionEvent)> {
    let released =
        |(job, owner): &(JobId, AgentId)| (owner.clone(), SessionEvent::JobReleased { job: *job });
    links.iter().map(released).collect()
}

/// Whether `id` or any ancestor launched by the same agent is background: a
/// foreground call inside a background script does not hold the agent either.
/// Stops at the agent boundary, whose launch mode belongs to the parent agent.
pub(super) fn effectively_background(jobs: &Jobs, id: JobId) -> bool {
    let mut next = jobs.get(id);
    while let Some(entry) = next {
        if entry.background {
            return true;
        }
        next = entry
            .parent
            .and_then(|parent| jobs.get(parent))
            .filter(|parent| parent.agent == entry.agent);
    }
    false
}

/// The script hosting a `wait`, with what it was last shown across its waits.
fn script_host(jobs: &Jobs, caller: JobId) -> Option<(JobId, Option<WaitFloor>)> {
    let host = jobs.get(caller)?.parent?;
    match jobs.get(host)?.role {
        RoleState::Script { wait_floor } => Some((host, wait_floor)),
        _ => None,
    }
}

impl Jobs {
    /// What hosts are shown of `job`.
    pub(crate) fn view(&self, job: JobId) -> Option<JobEnvelope> {
        Some(self.get(job)?.metadata(job))
    }

    /// Whether `job`, a `wait`, has parked: its deadline is set.
    #[cfg(test)]
    pub(crate) fn parked(&self, job: JobId) -> bool {
        let parked = |entry: &JobFacts| matches!(entry.role, RoleState::Wait { parked: true });
        self.get(job).is_some_and(parked)
    }

    /// What hosts are shown of every held job.
    pub(crate) fn views(&self) -> std::collections::BTreeMap<JobId, JobEnvelope> {
        (self.iter())
            .map(|(job, entry)| (job, entry.metadata(job)))
            .collect()
    }

    /// The links new input breaks, with their owners: `root`'s suspended
    /// foreground child invocations, and every live one of `holders`, whatever
    /// its phase.
    pub(crate) fn releases(&self, root: &AgentId, holders: &[AgentId]) -> Vec<(JobId, AgentId)> {
        let held = (holders.iter())
            .flat_map(|holder| self.live(holder))
            .chain(self.live(root).filter(|(_, entry)| entry.suspended()));
        let links: std::collections::BTreeMap<_, _> = held
            .filter(|(id, entry)| entry.child().is_some() && !effectively_background(self, *id))
            .map(|(id, entry)| (id, entry.agent.clone()))
            .collect();
        links.into_iter().collect()
    }

    /// Whether a child message or completion/question is ready, without reserving it.
    pub(crate) fn has_pending(&self, owner: &AgentId) -> bool {
        self.newest_pending(owner).is_some()
    }

    pub(crate) fn has_live(&self, owner: &AgentId) -> bool {
        self.live(owner).next().is_some()
    }

    /// Stop every child invocation that has yet to take the work that begins it,
    /// as an interrupt stops a running turn, whether or not its owner has a loop;
    /// returns how many it stopped.
    pub(crate) fn preempt_unbegun(&mut self) -> usize {
        let jobs = self.starting_in(Start::Pending).collect::<Vec<_>>();
        for &job in &jobs {
            self.modify(job, |entry, _| {
                entry
                    .child_mut()
                    .map(|child| child.start = Start::Preempted)
            });
        }
        jobs.len()
    }

    /// The running jobs whose child invocation an interrupt stopped before it began.
    pub(crate) fn preempted(&self) -> impl Iterator<Item = JobId> {
        self.starting_in(Start::Preempted)
    }

    fn starting_in(&self, start: Start) -> impl Iterator<Item = JobId> {
        let at = |entry: &JobFacts| {
            entry.end().is_none() && entry.child().is_some_and(|child| child.start == start)
        };
        self.select(at).into_iter().map(|(id, _)| id)
    }

    /// Let the invocations an interrupt stopped before they began begin; returns
    /// how many.
    pub(super) fn resume_preempted(&mut self) -> usize {
        let jobs: Vec<_> = self.preempted().collect();
        for &job in &jobs {
            self.modify(job, |entry, _| {
                entry.child_mut().map(|child| child.start = Start::Pending)
            });
        }
        jobs.len()
    }

    /// Mark `caller`, a `wait`, parked: concurrent waits do not defer to it.
    pub(crate) fn park(&mut self, caller: JobId) {
        self.modify(caller, |entry, _| {
            if let RoleState::Wait { parked } = &mut entry.role {
                *parked = true;
            }
        });
    }

    /// How `owner`'s jobs decide a `wait` by `caller`: foreground work defers it;
    /// a script is shown each event once; and the model is also woken by another
    /// call of its own response that ended, with or without a job, whose outcome
    /// then returns with this batch.
    pub(crate) fn waiting(&self, owner: &AgentId, caller: JobId) -> Waiting {
        let Some(entry) = self.get(caller).filter(|entry| !entry.cancel_requested()) else {
            return Waiting::Cancelled;
        };
        if self.live_work(owner).holding.is_some() {
            return Waiting::Deferred;
        }
        let response = entry.origin.as_ref().map(|origin| origin.message);
        let floor = script_host(self, caller).map(|(_, floor)| floor);
        let answered = floor.is_none()
            && response.is_some_and(|response| {
                // The caller itself, ended, is not another call.
                let own = usize::from(!entry.live());
                self.jobless(owner) == Some(response) || self.ended_from(owner, response) > own
            });
        let since = floor.flatten().map_or(0, |floor| floor.stamp);
        let pending = self
            .newest_pending(owner)
            .is_some_and(|stamp| stamp > since);
        Waiting::Open {
            unseen: answered || pending,
            seen_input: floor.flatten().map(|floor| floor.input),
            stamp: self.stamps.now(),
        }
    }

    /// Record what a script's `wait` by `caller` was shown, so it is not told twice.
    pub(crate) fn shown(&mut self, caller: JobId, shown: WaitFloor) {
        let Some((host, _)) = script_host(self, caller) else {
            return;
        };
        self.modify(host, |host, _| {
            if let RoleState::Script { wait_floor } = &mut host.role {
                *wait_floor = Some(shown);
            }
        });
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JobEnvelope {
    pub id: JobId,
    /// The agent whose job it is.
    pub agent: AgentId,
    /// What it was called with.
    pub arguments: std::sync::Arc<Value>,
    pub invocation: Invocation,
    pub timing: RunTiming,
    pub parent: Option<JobId>,
    pub tool: String,
    pub role: JobRole,
    pub name: Option<JobName>,
    pub state: JobState,
    /// The images its finished run returned.
    pub(crate) images: Vec<ImageRef>,
    /// The model call that launched the job; its parent is then the calling
    /// agent's own.
    pub(crate) origin: Option<crate::session::ModelCallOrigin>,
    /// The question a waiting job asks, until its owner has seen it.
    pub(crate) question: Option<QuestionOutput>,
    /// Failure facts; every viewer renders them under its own capabilities.
    pub diagnostic: Option<Diagnostic>,
    pub location: ExecutionLocation,
    /// Input would resume it after it ends.
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
    /// None when the call settled before a job was published, or completed with
    /// nothing more to read or resume.
    pub id: Option<JobId>,
    pub(crate) state: Option<JobState>,
    /// Precedes the result, so a reader learns what was left out before reading it.
    pub presentation: Option<Presentation>,
    /// A present `null` is a real result, distinct from no result.
    #[serde(default, deserialize_with = "present")]
    pub result: Option<Value>,
    #[schemars(transform = complete)]
    pub error: Option<String>,
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
    pub(crate) parent: Option<JobId>,
    pub(crate) tool: Option<String>,
    #[schemars(with = "String")]
    pub(crate) name: Option<JobName>,
    pub(crate) target: Option<String>,
    /// Display metadata; execution keeps its native PathBuf in JobEnvelope.
    pub(crate) workspace: Option<String>,
    pub(crate) code: Option<crate::tool::DenialCode>,
}

#[serde_with::skip_serializing_none]
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct Presentation {
    pub preview: Option<output::OutputPreview>,
    /// The whole result's type, with counts, when samples leave parts out.
    pub shape: Option<Value>,
    pub(crate) truncated: Option<Vec<output::OutputTruncation>>,
    /// Captures a presented result does not already show in full.
    pub(crate) captures: Option<Vec<output::CaptureDescriptor>>,
    /// A waiting child agent returns a question batch rather than a result.
    #[schemars(transform = complete)]
    pub(crate) question: Option<QuestionOutput>,
    // The schema keeps the plain string the system prompt's JobView type shows.
    #[schemars(with = "String")]
    pub notice: Option<Notice>,
}

crate::named_enum::named_enum! {
    #[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
    pub enum Notice {
        OutputIncomplete = "Output incomplete.",
    }
}

impl Presentation {
    pub fn truncated(&self) -> &[output::OutputTruncation] {
        self.truncated.as_deref().unwrap_or_default()
    }

    pub fn captures(&self) -> &[output::CaptureDescriptor] {
        self.captures.as_deref().unwrap_or_default()
    }

    pub fn questions(&self) -> &[crate::agent::Question] {
        self.question.as_ref().map_or(&[], |batch| &batch.questions)
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
    pub fn state(&self) -> JobState {
        self.state.unwrap_or(JobState::Completed)
    }

    pub fn tool(&self) -> Option<&str> {
        self.meta.as_ref().and_then(|meta| meta.tool.as_deref())
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
    /// Whether it shows under its parent rather than as a root of its owner's view:
    /// its owner's script issued it.
    pub fn nests(&self) -> bool {
        super::nests(self.parent, &self.origin)
    }

    /// Failed or interrupted, and something can restart it: what continuing the
    /// session restarts.
    pub fn retryable(&self) -> bool {
        matches!(self.state, JobState::Failed | JobState::Interrupted) && self.resumable
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

    /// The viewer's own foreground call with its saved `result`: launch
    /// metadata only on failure.
    pub(crate) fn response_view<'a>(
        &self,
        viewer: impl Into<DiagnosticViewer<'a>>,
        result: Option<Value>,
    ) -> JobView {
        self.view(
            viewer.into(),
            ViewKind::Response,
            result,
            Presentation::default(),
        )
    }

    /// Background handles, listings and inspection always retain launch metadata.
    pub(crate) fn metadata_view<'a>(&self, viewer: impl Into<DiagnosticViewer<'a>>) -> JobView {
        let viewer = viewer.into();
        self.view(viewer, ViewKind::Inspection, None, Presentation::default())
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
    pub(crate) fn active_launches(
        &self,
        agent: &AgentId,
    ) -> Vec<(JobId, Option<crate::session::ModelCallOrigin>)> {
        self.facts(|jobs| {
            let mut launches = Vec::new();
            for (id, entry) in jobs.unfinished(agent) {
                let mut current = entry;
                let origin = loop {
                    if &current.agent != agent {
                        break None;
                    }
                    if let Some(origin) = &current.origin {
                        break Some(origin.clone());
                    }
                    let Some(parent) = current.parent.and_then(|id| jobs.get(id)) else {
                        break None;
                    };
                    current = parent;
                };
                launches.push((id, origin));
            }
            launches
        })
    }

    /// The committed response whose call launched job `id` directly.
    pub(crate) async fn call_origin(&self, id: JobId) -> Option<crate::session::MessageSeq> {
        let origin = self.entry(id, |entry| Some(entry.origin.as_ref()?.message));
        origin.await.ok().flatten()
    }

    pub(crate) async fn metadata(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        self.entry(id, |entry| entry.metadata(id)).await
    }

    pub async fn snapshot(&self, id: JobId) -> Result<JobEnvelope, JobError> {
        self.entry(id, |entry| entry.envelope(id)).await
    }

    /// The token job `id` runs under, while it can still run.
    pub(crate) async fn cancellation_token(
        &self,
        id: JobId,
    ) -> Result<CancellationToken, JobError> {
        let handles = self.entry(id, |entry| {
            entry.handles.as_ref().map(|h| h.cancellation.clone())
        });
        handles.await?.ok_or(JobError::AlreadyTerminal(id))
    }

    /// Every job of `owner`, in id order: held, or as the journal leaves it. A
    /// read of the owner's whole history.
    pub async fn list(&self, owner: &AgentId) -> Result<Vec<JobEnvelope>, JobError> {
        let owner = owner.clone();
        self.listed(move |journal| journal.created_by(&owner)).await
    }

    /// `owner`'s unfinished jobs, in id order.
    pub fn active(&self, owner: &AgentId) -> Vec<JobEnvelope> {
        self.facts(|jobs| {
            (jobs.unfinished(owner))
                .map(|(id, entry)| entry.metadata(id))
                .collect()
        })
    }

    /// Current request-time state.
    pub(crate) fn active_states(
        &self,
        owner: &AgentId,
        capabilities: &CapabilitySet,
        now_millis: i64,
    ) -> Vec<StateJob> {
        self.facts(|jobs| {
            (jobs.unfinished(owner))
                .map(|(id, _)| {
                    let path = &mut std::collections::HashSet::new();
                    progress::active_job(id, jobs, capabilities, now_millis, path)
                })
                .collect()
        })
    }

    pub(crate) fn is_effectively_background(&self, id: JobId) -> Result<bool, JobError> {
        self.facts(|jobs| {
            if !jobs.contains(id) {
                return Err(JobError::Unknown(id));
            }
            Ok(effectively_background(jobs, id))
        })
    }

    pub async fn is_background(&self, id: JobId) -> Result<bool, JobError> {
        self.entry(id, |entry| entry.background).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> JobEnvelope {
        JobEnvelope {
            id: JobId::new(1).unwrap(),
            agent: AgentId::root(crate::identity::SessionId::from_bytes([1; 16])),
            arguments: Default::default(),
            invocation: Invocation::default(),
            timing: RunTiming::Unstarted,
            parent: None,
            tool: "fixture".into(),
            role: JobRole::Tool,
            name: None,
            state: JobState::Completed,
            images: Vec::new(),
            origin: None,
            question: None,
            diagnostic: None,
            location: ExecutionLocation::root(std::path::PathBuf::from("/work")),
            resumable: false,
        }
    }

    #[test]
    fn plain_process_response_keeps_model_text_under_two_hundred_bytes() {
        let result = serde_json::json!({
            "exit_code":0, "stdout":"", "stderr":"", "timed_out":false
        });
        let view = (envelope().response_view(&CapabilitySet::default(), Some(result))).into_value();
        let wire =
            serde_json::to_vec(&serde_json::json!({"result":view,"is_error":false})).unwrap();
        assert!(
            wire.len() < 200,
            "plain response grew to {} bytes",
            wire.len()
        );
    }

    #[test]
    fn display_metadata_with_non_utf8_workspace_is_json_serializable() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut job = envelope();
        job.location.workspace = std::ffi::OsString::from_vec(b"/work/\xff".to_vec()).into();
        let view = job.metadata_view(&CapabilitySet::default()).into_value();
        assert_eq!(view["meta"]["workspace"], "/work/\u{fffd}");
    }

    /// A view with every part a replayed job event can carry, including each
    /// untagged preview, line and truncation shape.
    fn presented() -> JobView {
        let field = |field: &str| field.parse::<FieldPointer>().unwrap();
        let page = JobView {
            id: JobId::new(2).ok(),
            state: Some(JobState::Completed),
            result: Some(serde_json::json!("line")),
            error: None,
            meta: None,
            presentation: None,
        };
        JobView {
            id: JobId::new(2).ok(),
            state: Some(JobState::Failed),
            result: None,
            error: Some("denied".into()),
            meta: Some(JobMetadata {
                parent: JobId::new(1).ok(),
                tool: Some("read".into()),
                name: Some("reader".parse().unwrap()),
                target: Some("build".into()),
                workspace: Some("/srv".into()),
                code: Some(crate::tool::DenialCode::PermissionDenied),
            }),
            presentation: Some(Presentation {
                preview: Some(output::OutputPreview::Lines(output::LinePage {
                    field: Some(FieldPointer::result()),
                    lines: output::PageLines::Numbered(vec![output::NumberedLine {
                        line: 3,
                        text: "line".into(),
                    }]),
                    total_lines: Some(1),
                    next_start: None,
                    next_offset: None,
                })),
                shape: Some(serde_json::json!({"items": [3, "integer"]})),
                truncated: Some(vec![
                    output::OutputTruncation::Elements {
                        field: field("/result/items"),
                        shown: 1,
                        total_elements: 3,
                        kept: None,
                    },
                    output::OutputTruncation::Text {
                        field: field("/result/content"),
                        total_lines: 9,
                        next_start: 4,
                        next_offset: None,
                    },
                    output::OutputTruncation::TextGap {
                        field: field("/result/stdout"),
                        total_lines: 9,
                        next_start: 4,
                        tail_start: 8,
                    },
                ]),
                captures: Some(vec![output::CaptureDescriptor {
                    field: field("/result/stdout"),
                    complete: true,
                    output: Some(Box::new(page)),
                }]),
                question: None,
                notice: Some(Notice::OutputIncomplete),
            }),
        }
    }

    #[test]
    fn views_omit_absent_fields_and_empty_lists_but_keep_a_null_result() {
        let capabilities = CapabilitySet::default();
        let validator = jsonschema::validator_for(&JOB_VIEW_SCHEMAS.one).unwrap();
        let payload =
            serde_json::json!({"nested": null, "array": [null], "presentation": {"preview": null}});
        let job = envelope();
        let view = job
            .response_view(&capabilities, Some(payload.clone()))
            .into_value();
        assert_eq!(view, serde_json::json!({"result":payload}));
        let full = job.metadata_view(&capabilities).into_value();
        assert_eq!(
            full["meta"],
            serde_json::json!({"tool":"fixture", "workspace":"/work"})
        );
        let null = envelope().response_view(&capabilities, Some(Value::Null));
        let missing = envelope().metadata_view(&capabilities);
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
        for view in [null, missing, noticed, presented()] {
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
        let mut job = envelope();
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
            job.response_view(viewer, Some(Value::Null)).meta,
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
        let id = jobs.test_resumable(agent, "agent", Value::Null).await;
        jobs.test_finish(id, Value::Null).await;
        let view = async || {
            let envelope = jobs.snapshot(id).await.unwrap();
            envelope.response_view(&CapabilitySet::default(), None)
        };
        let resumable = view().await;
        assert_eq!(
            (resumable.id, resumable.state),
            (Some(id), Some(JobState::Completed))
        );
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
                output::OutputTruncation::TextGap {
                    field: field("/result/stderr"),
                    total_lines: 9,
                    next_start: 3,
                    tail_start: 8,
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
        presentation.truncated.as_mut().unwrap().remove(0);
        // A gap continues at its first line left out.
        assert_eq!(
            next(&presentation),
            Some(("/result/stderr".into(), 3, None))
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
        let mut job = envelope();
        job.state = JobState::Failed;
        job.diagnostic = Some(crate::tool::ToolError::denied("failed").diagnostic());
        let view = (job.response_view(&CapabilitySet::default(), Some(Value::Null))).into_value();
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
            let mut job = envelope();
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
        let owner = JobSpec {
            role: JobRole::Agent,
            ..spec(&agent, "agent", None, Some(root_origin.clone()))
        };
        let owner = jobs.test_lease(owner).await;
        let store = jobs.store();
        let child_agent =
            crate::session::tests::start_child(store, &agent, 1, Some(owner.id()), root.path())
                .await;
        let child_origin = call(&child_agent, "child-script").await;
        let host = jobs
            .test_lease(spec(&child_agent, "host-started", Some(owner.id()), None))
            .await;
        assert_eq!(jobs.active_launches(&child_agent), vec![(host.id(), None)]);
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
            jobs.active_launches(&agent),
            vec![(owner.id(), Some(root_origin))]
        );
        assert_eq!(
            jobs.active_launches(&child_agent),
            vec![
                (host.id(), None),
                (child.id(), Some(child_origin.clone())),
                (shell.id(), Some(child_origin))
            ]
        );
    }
}

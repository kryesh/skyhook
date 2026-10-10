//! The jobs the journal records, as the core folds them, and what reopening settles.
use super::*;
use crate::session::EventRecord;

/// Every job the journal records, with the live facts the core keeps beside them.
#[derive(Default)]
pub(crate) struct Jobs {
    pub(super) entries: HashMap<JobId, JobFacts>,
    /// The job each launched child agent answers through.
    children: HashMap<AgentId, JobId>,
    /// The highest id created so far.
    last: u64,
    pub(super) stamps: Stamps,
    pub(super) progress: progress::Progress,
    pub(super) calls: Calls,
}

/// Live only: model and script calls between their issue and their outcome, as
/// a `wait` beside them needs to know them.
#[derive(Default)]
pub(super) struct Calls {
    /// Per parent job, its issued calls whose jobs are not published yet.
    issuing: HashMap<JobId, usize>,
    /// Per agent, its latest response with a call that ended without a job: a
    /// response's calls all end before the next one commits.
    pub(super) jobless: HashMap<AgentId, MessageSeq>,
}

impl Calls {
    pub(super) fn issue(&mut self, parent: JobId) {
        *self.issuing.entry(parent).or_default() += 1;
    }

    pub(super) fn issued(&mut self, parent: JobId) {
        if let Some(count) = self.issuing.get_mut(&parent) {
            *count -= 1;
            if *count == 0 {
                self.issuing.remove(&parent);
            }
        }
    }

    pub(super) fn published(&self, parent: JobId) -> Option<()> {
        (!self.issuing.contains_key(&parent)).then_some(())
    }
}

impl Jobs {
    /// The first record of `records` whose transition its job's phase, as the
    /// records before it leave it, does not admit.
    pub(crate) fn refused(&self, records: &[EventRecord]) -> Option<RecordSeq> {
        let mut states = HashMap::new();
        for record in records {
            if let SessionEvent::JobCreated { job, .. } = &record.event {
                states.insert(*job, JobState::Queued);
            }
            let Some((job, change)) = JobChange::of(&record.event) else {
                continue;
            };
            let current = states.get(&job).copied();
            let Some(state) = current.or_else(|| self.entries.get(&job).map(JobFacts::state))
            else {
                continue;
            };
            let step = change.step();
            if !state.admits(step) {
                return Some(record.sequence);
            }
            states.insert(job, JobState::after(step));
        }
        None
    }

    /// Fold one committed record: replay and live commits alike. Returns the job
    /// whose lifecycle it moved.
    pub(crate) fn apply(&mut self, record: &EventRecord) -> Option<JobId> {
        self.progress.project(record);
        match &record.event {
            SessionEvent::JobCreated {
                origin,
                job,
                parent,
                tool,
                role,
                name,
                arguments,
                accepts_input,
                background,
                location,
                output_schema,
                ..
            } => {
                self.last = self.last.max(job.get());
                let entry = JobFacts {
                    origin: origin.clone(),
                    output_schema: output_schema.clone(),
                    agent: record.agent.clone(),
                    parent: *parent,
                    tool: tool.clone(),
                    role: RoleState::new(*role),
                    name: name.clone(),
                    arguments: Arc::new(arguments.clone()),
                    created_at_millis: record.timestamp_millis,
                    created: record.sequence,
                    phase: Phase::Queued,
                    accepts_input: *accepts_input,
                    delivery: DeliveryState::Pending {
                        stamp: self.stamps.next(),
                    },
                    background: *background,
                    location: location.clone(),
                    resume: None,
                    finishing: false,
                    reads: Vec::new(),
                    invocation: Invocation::default(),
                    timing: RunTiming::Unstarted,
                    cancel: CancelState::Open,
                };
                self.entries.insert(*job, entry);
                return Some(*job);
            }
            SessionEvent::AgentStarted {
                owner_job: Some(job),
                ..
            } => {
                if let Some(entry) = self.entries.get_mut(job) {
                    let owner = entry.agent.clone();
                    if record.agent.parent().as_ref() == Some(&owner)
                        && let Some(launched) = entry.child_mut()
                        && launched
                            .agent
                            .as_ref()
                            .is_none_or(|child| child == &record.agent)
                    {
                        launched.agent = Some(record.agent.clone());
                        self.children.insert(record.agent.clone(), *job);
                    }
                }
            }
            SessionEvent::MessageCommitted { message } => {
                if let Some(job) = self.children.get(&record.agent).copied()
                    && let Some(reply) = messages::Reply::of(message)
                {
                    let publish = reply.publish(&self.entries, job);
                    if let Some(entry) = self.entries.get_mut(&job) {
                        let sequence = record.sequence.message();
                        let stamps = &mut self.stamps;
                        entry.publish_message(job, sequence, reply.text, publish, stamps);
                    }
                }
            }
            SessionEvent::JobClaimed { job } | SessionEvent::JobInjected { job } => {
                if let Some(entry) = self.entries.get_mut(job) {
                    entry.acknowledge();
                }
            }
            SessionEvent::JobReleased { job } => {
                if let Some(entry) = self.entries.get_mut(job) {
                    entry.background = true;
                }
            }
            SessionEvent::JobMessageReleased { job } => {
                if let Some(child) = self.entries.get_mut(job).and_then(JobFacts::child_mut) {
                    child.release(&mut self.stamps);
                }
            }
            SessionEvent::JobMessageDelivered { job, source, .. } => {
                if let Some(entry) = self.entries.get_mut(job) {
                    entry.deliver_message(*source);
                }
            }
            event => {
                let (job, change) = JobChange::of(event)?;
                let entry = self.entries.get_mut(&job)?;
                entry.enter(change, &mut self.stamps);
                entry.timing = entry.timing.after(entry.state(), record.timestamp_millis);
                return Some(job);
            }
        }
        None
    }

    /// The job whose running invocation `child` answers through, while that
    /// invocation has yet to begin and nothing is finishing it.
    pub(crate) fn unbegun(&self, child: &AgentId) -> Option<JobId> {
        let job = *self.children.get(child)?;
        let entry = self.entries.get(&job)?;
        let live = entry.end().is_none() && !entry.finishing;
        (live && entry.child()?.start != Start::Begun).then_some(job)
    }

    /// Begin the invocation `child` answers through, as it takes the input that
    /// begins it: whether an interrupt stopped that invocation first.
    pub(crate) fn begin(&mut self, child: &AgentId) -> bool {
        let job = self.unbegun(child);
        let launched = job.and_then(|job| self.entries.get_mut(&job)?.child_mut());
        launched.is_some_and(|child| {
            std::mem::replace(&mut child.start, Start::Begun) == Start::Preempted
        })
    }

    /// The id the next created job takes.
    pub(super) fn next_id(&self) -> Result<JobId, JobError> {
        JobId::new(self.last.saturating_add(1)).map_err(|_| JobError::InvalidId)
    }

    /// What reopening settles for the foreground jobs nothing holds any more,
    /// once what was running has ended interrupted: a model call cut short is
    /// read by the interrupted answer reopening gives it; every retained child,
    /// suspended, and every other outcome nobody read, whoever called the job,
    /// is released to report to its owner.
    pub(crate) fn reopened(&self) -> Vec<(AgentId, SessionEvent)> {
        let settled = |job: JobId, entry: &JobFacts| match entry.end() {
            None if entry.resume.is_none() && entry.origin.is_some() => {
                Some(SessionEvent::JobClaimed { job })
            }
            None => Some(SessionEvent::JobReleased { job }),
            Some(_) if entry.unacknowledged() => Some(SessionEvent::JobReleased { job }),
            Some(_) => None,
        };
        let mut jobs: Vec<_> = (self.entries.iter())
            .filter(|(id, _)| !views::effectively_background(&self.entries, **id))
            .collect();
        jobs.sort_by_key(|(id, _)| **id);
        let settled = jobs.into_iter().filter_map(|(id, entry)| {
            settled(*id, entry).map(|event| (entry.agent.clone(), event))
        });
        settled.collect()
    }
}

impl JobManager {
    /// The records that end what a stopped process left running interrupted, and
    /// the outputs they save.
    pub(crate) async fn interrupt_running(
        &self,
    ) -> Result<(Vec<(AgentId, SessionEvent)>, Vec<Prepared>), JobError> {
        let running = |(job, entry): (&JobId, &JobFacts)| {
            (entry.end().is_none()).then(|| (Run(*job, entry.invocation), entry.agent.clone()))
        };
        let running: Vec<_> = self.facts(|jobs| jobs.entries.iter().filter_map(running).collect());
        let (mut interrupted, mut outputs) = (Vec::new(), Vec::new());
        for (run @ Run(job, _), agent) in running {
            let (change, output) = self.finished(run, ToolError::interrupted().into()).await?;
            interrupted.push((agent, change.event(job)));
            outputs.extend(output);
        }
        Ok((interrupted, outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionLocation;
    use crate::identity::JobId;
    use crate::job::{JOB_VIEW_SCHEMAS, JobOutcome, JobRole, JobState, JobTransition};
    use crate::{
        job::output,
        tool::{ToolOutput, policy::CapabilitySet},
    };

    /// An on-disk runtime, so `reopen` can replay its journal.
    async fn runtime() -> (tempfile::TempDir, JobManager, crate::identity::AgentId) {
        let (root, store, agent) = crate::session::tests::on_disk().await;
        (root, JobManager::new(store), agent)
    }

    /// Abandon leases the way a process exit does: cancelled, never finalized.
    fn abandon(leases: impl Send + 'static) {
        std::thread::spawn(move || drop(leases)).join().unwrap();
    }

    /// Drain owners, drop the manager, and replay the durable journal.
    async fn reopen(jobs: JobManager, root: &std::path::Path) -> JobManager {
        let session = jobs.store().id();
        jobs.supervisor().drain().await;
        drop(jobs);
        let (store, _) = SessionStore::open(root, session).await.unwrap();
        crate::job::tests::reopened(store).await
    }

    #[tokio::test]
    async fn diagnostic_replay_filters_targets_and_only_renders_registered_error_slots() {
        use crate::tool::{
            diagnostic::{
                Cause, Effects, FailureSite, IoKind, Operation, PartialContext, PartialDiagnostic,
                PathRole, Subject,
            },
            policy::Capability,
        };
        use serde_json::json;

        let (root, jobs, agent) = runtime().await;
        let location = ExecutionLocation::named(
            "private-build-target".parse().unwrap(),
            "/srv/project".into(),
        );
        let context = PartialContext::new(Operation::Read, Subject::path("missing.txt"))
            .at(FailureSite::Execution(location))
            .effects(Effects::NotStarted)
            .path(PathRole::Requested, "./missing.txt")
            .path(PathRole::Resolved, "/srv/project/missing.txt");
        let diagnostic = PartialDiagnostic::new(
            context.clone(),
            Cause::Io {
                kind: IoKind::NotFound,
                code: Some(2),
                detail: None,
            },
        )
        .resolve();
        let payload = || {
            json!({
                "kind":"error", "path":"missing.txt",
                "error":{"code":"not_found", "message":"producer placeholder"},
                "opaque":{"error":{"message":"opaque user error"}},
                // An offloaded sibling exercises whole-document render caching.
                "text":"ordinary output\n".repeat(800),
            })
        };
        let failed = jobs
            .test_create(JobSpec::test(agent.clone(), "fixture"))
            .await;
        let error = ToolError::io(std::io::Error::from_raw_os_error(2))
            .context(context)
            .with_result(ToolOutput::new(payload()));
        jobs.finish(failed, error.into()).await.unwrap();
        let read = jobs
            .test_create(JobSpec::test(agent.clone(), "fixture"))
            .await;
        jobs.finish(
            read,
            JobOutcome::Completed(
                ToolOutput::new(payload()).with_diagnostic(diagnostic.clone().into()),
            ),
        )
        .await
        .unwrap();
        let opaque = jobs.test_create(JobSpec::test(agent, "read")).await;
        jobs.test_finish(opaque, payload()).await;

        // Durable facts are authoritative; the saved result retains only its slot,
        // never a rendering selected by an earlier reader's capabilities.
        assert!(jobs.output(read).test_document().unwrap()["result"]["error"]["message"].is_null());
        assert_eq!(
            jobs.output(opaque).test_document().unwrap()["result"]["error"]["message"],
            "producer placeholder"
        );

        let privileged: CapabilitySet = [Capability::Targets].into_iter().collect();
        let restricted = CapabilitySet::default();
        // Alternating readers must never observe another capability's rendering.
        let views = async |jobs: &JobManager| {
            let mut views = Vec::new();
            for caps in [&privileged, &restricted, &privileged] {
                for (job, field) in [
                    (failed, None),
                    (read, None),
                    (opaque, None),
                    (read, Some(output::diagnostic_slot())),
                    (failed, Some(crate::job::FieldPointer::root())),
                    (read, Some(crate::job::FieldPointer::result())),
                ] {
                    let mut query = output::OutputArgs::new(job);
                    query.field = field;
                    let view = jobs
                        .inspect_output(query, crate::job::CancellationToken::new(), caps)
                        .await
                        .unwrap();
                    if job != opaque {
                        assert_eq!(
                            view.to_string().contains("private-build-target"),
                            caps.contains(Capability::Targets)
                        );
                    }
                    views.push(view);
                }
            }
            views
        };
        let live = views(&jobs).await;
        let (failed_view, read_view, opaque_view) = (&live[0], &live[1], &live[2]);
        assert_eq!(failed_view["state"], "failed");
        assert_eq!(failed_view["error"], diagnostic.render(&privileged));
        assert_eq!(read_view["state"], "completed");
        assert!(read_view["error"].is_null());
        assert_eq!(read_view["result"]["error"]["code"], "not_found");
        assert_eq!(
            read_view["result"]["error"]["message"],
            diagnostic.render(&privileged)
        );
        assert_eq!(
            read_view["result"]["opaque"]["error"]["message"],
            "opaque user error"
        );
        assert_eq!(
            opaque_view["result"]["error"]["message"],
            "producer placeholder"
        );
        let restored = reopen(jobs, root.path()).await;
        assert_eq!(views(&restored).await, live);
    }

    /// A journal whose transitions are out of lifecycle order is corrupt, not
    /// silently reinterpreted: one refused stops the session.
    #[tokio::test]
    async fn the_journal_refuses_a_transition_the_phase_does_not_admit() {
        let (_root, jobs, agent) = runtime().await;
        let lease = jobs
            .test_running(JobSpec::test(agent.clone(), "exec"))
            .await;
        let regressed = SessionEvent::JobStateChanged {
            job: lease.id(),
            state: JobTransition::Running,
        };
        let before = jobs.store().records().len();
        let refused = jobs.store().append(agent, regressed).await;
        let sequence = RecordSeq::from(before as u64 + 1);
        let Err(SessionError::JournalFailed(failure)) = refused else {
            panic!("a refused transition stops the journal: {refused:?}");
        };
        assert!(matches!(failure.cause(),
            SessionError::IllegalTransition { sequence: refused } if *refused == sequence));
        assert_eq!(jobs.store().records().len(), before);
        assert_eq!(jobs.metadata(lease.id()).unwrap().state, JobState::Running);
    }

    /// A job is timed from its first run to its end, through waits for input, and
    /// anew from a later invocation; one that never ran stays unstarted.
    #[test]
    fn invocations_are_timed_from_their_first_run_to_their_end() {
        use JobTransition::{AwaitingApproval, Running, WaitingInput};
        use RunTiming::{Since, Took, Unstarted};
        let agent = AgentId::root(crate::identity::SessionId::generate().unwrap());
        let (mut jobs, mut at) = (Jobs::default(), 0);
        let mut step = |jobs: &mut Jobs, job: u64, event: SessionEvent| {
            at += 1;
            let mut record = crate::session::tests::record(&agent, at, event);
            record.timestamp_millis = at as i64;
            let job = JobId::new(job).unwrap();
            assert_eq!(jobs.apply(&record), Some(job));
            let view = jobs.view(job).unwrap();
            (view.invocation, view.timing)
        };
        let created =
            |job| crate::session::tests::job_created(JobId::new(job).unwrap(), JobRole::Tool);
        let changed = |state| SessionEvent::JobStateChanged {
            job: JobId::new(1).unwrap(),
            state,
        };
        let finished = |job, state| SessionEvent::JobFinished {
            job: JobId::new(job).unwrap(),
            state,
            diagnostic: None,
            output_diagnostic: None,
            images: Vec::new(),
        };
        let (first, _) = step(&mut jobs, 1, created(1));
        for (event, timing) in [
            (changed(AwaitingApproval), Unstarted),
            (changed(Running), Since(3)),
            (changed(WaitingInput), Since(3)),
            (changed(Running), Since(3)),
            (finished(1, JobEnd::Completed), Took { since: 3, until: 6 }),
        ] {
            assert_eq!(step(&mut jobs, 1, event), (first, timing));
        }
        let (rerun, timing) = step(&mut jobs, 1, changed(Running));
        assert!(rerun != first && timing == Since(7));
        step(&mut jobs, 2, created(2));
        let cancelled = step(&mut jobs, 2, finished(2, JobEnd::Cancelled));
        assert_eq!(cancelled.1, Unstarted);
    }

    #[tokio::test]
    async fn replay_recovers_creation_committed_before_map_publication() {
        let (root, store, agent) = crate::session::tests::on_disk().await;
        let job = JobId::new(7).unwrap();
        let created = crate::session::tests::job_created(job, JobRole::Tool);
        store.append(agent.clone(), created).await.unwrap();
        // A creation no manager ever ran, as a crash leaves it.
        let jobs = reopen(JobManager::new(store), root.path()).await;
        let interrupted = jobs.metadata(job).unwrap();
        assert_eq!(interrupted.state, JobState::Interrupted);
        assert!(
            interrupted
                .rendered_error(&CapabilitySet::default())
                .is_some_and(|error| error.contains("operation interrupted")
                    && !error.contains("session was not running"))
        );
        assert_eq!(
            jobs.test_create(JobSpec::test(agent, "next")).await.get(),
            8
        );
    }

    #[tokio::test]
    async fn denial_survives_replay_and_agent_views_hide_pending_authorization() {
        let (root, jobs, agent) = runtime().await;
        let capabilities = CapabilitySet::default();
        let job = jobs
            .test_approving(JobSpec::test(agent, "exec"))
            .await
            .into_test_id();
        let pending = jobs.snapshot(job).unwrap();
        assert_eq!(
            pending.metadata_view(&capabilities).into_value()["state"],
            "queued"
        );
        let schema = JOB_VIEW_SCHEMAS.one.to_string();
        assert!(!schema.contains("awaiting_approval"));
        jobs.finish(job, ToolError::denied("user reason").into())
            .await
            .unwrap();
        let denied = jobs
            .wait(job, None, true)
            .await
            .unwrap()
            .metadata_view(&capabilities)
            .into_value();
        assert_eq!(denied["meta"]["code"], "permission_denied");
        assert!(denied["error"].as_str().unwrap().contains("user reason"));
        // The persisted terminal event is the source of truth for replay.
        let restored = reopen(jobs, root.path()).await;
        let replayed = restored
            .snapshot(job)
            .unwrap()
            .metadata_view(&capabilities)
            .into_value();
        assert_eq!(replayed, denied);
    }

    #[tokio::test]
    async fn restore_interrupts_active_jobs_and_advances_ids() {
        let (root, manager, agent) = runtime().await;
        let named = ExecutionLocation::named("build".parse().unwrap(), "/srv/project".into());
        let mut leases = Vec::new();
        // Unfinished registered captures must not be presented as a structured result.
        for (location, field, kind, bytes) in [
            (
                ExecutionLocation::root(".".into()),
                "/result/stdout",
                output::CaptureKind::Text,
                "prefix\n",
            ),
            (
                named.clone(),
                "/result/custom",
                output::CaptureKind::Json,
                "{\"partial\":",
            ),
        ] {
            let spec = JobSpec {
                background: true,
                location,
                ..JobSpec::test(agent.clone(), "exec")
            };
            let lease = manager.test_running(spec).await;
            manager
                .output(lease.id())
                .test_capture(field, kind, bytes.as_bytes());
            leases.push(lease);
        }
        abandon(leases);
        let restored = reopen(manager, root.path()).await;
        for (job, field, location) in [
            (1, "/result/stdout", ExecutionLocation::root(".".into())),
            (2, "/result/custom", named),
        ] {
            let id = JobId::new(job).unwrap();
            let args = output::OutputArgs::new(id);
            let view = restored
                .present_output(args, &CapabilitySet::default())
                .await
                .unwrap();
            assert_eq!(view.get("result"), None);
            let capture = serde_json::json!({"field":field,"complete":false});
            assert_eq!(view["presentation"]["captures"][0], capture);
            let snapshot = restored.snapshot(id).unwrap();
            assert_eq!(
                (snapshot.state, snapshot.location),
                (JobState::Interrupted, location)
            );
        }
        let next = restored.create(JobSpec::test(agent, "next")).await.unwrap();
        assert_eq!(next.id().get(), 3);
    }

    fn outcome_image() -> crate::media::ImageRef {
        crate::media::ImageRef {
            file: Some("historical-image".into()),
            format: crate::media::ImageFormat::Png,
            blob: crate::media::BlobRef::of(OUTCOME_IMAGE),
        }
    }

    const OUTCOME_IMAGE: &[u8] = b"historical image";

    fn stored_projection(jobs: &JobManager, id: JobId) -> serde_json::Value {
        jobs.entry(id, |entry| {
            let finished = entry.finished();
            let diagnostic = finished.and_then(|finished| finished.diagnostic.as_ref());
            serde_json::json!({
                "state": entry.state(),
                "images": finished.map_or(&[][..], |finished| finished.images.as_slice()),
                "error": diagnostic.map(|diagnostic| diagnostic.render(&CapabilitySet::default())),
                "denied": diagnostic.is_some_and(|diagnostic| diagnostic.is_denial()),
                "pending": entry.unacknowledged(),
                "resumable": entry.resume.is_some(),
            })
        })
        .unwrap()
    }

    #[tokio::test]
    async fn outcome_application_live_replay_and_interrupted_cancellation_matrix() {
        for case in 0..7 {
            let (root, jobs, agent) = runtime().await;
            jobs.store().store_blob(OUTCOME_IMAGE).await.unwrap();
            let id = jobs
                .test_resumable(agent, "outcome", serde_json::Value::Null)
                .await;
            let question = crate::job::tests::question("partial");
            jobs.request_input(id, question).await.unwrap();
            let result = || {
                ToolOutput::new(serde_json::json!({"result":"saved"}))
                    .with_images(vec![outcome_image()])
            };
            let outcome = match case {
                0 => JobOutcome::Completed(result()),
                1 => ToolError::failed("failed").into(),
                2 => ToolError::failed("partial failure")
                    .with_result(result())
                    .into(),
                3 => ToolError::denied("denied").into(),
                4 => ToolError::cancelled().with_result(result()).into(),
                _ => ToolError::interrupted().into(),
            };
            jobs.finish(id, outcome).await.unwrap();
            if case == 6 {
                jobs.cancel(id).await.unwrap();
                crate::tests::bounded(jobs.wait_settled(id)).await.unwrap();
            }
            let projection = stored_projection(&jobs, id);
            assert_eq!(projection["resumable"], !matches!(case, 4 | 6));
            // The saved payload is never replaced by the in-memory question.
            if matches!(case, 0 | 2 | 4) {
                let document = jobs.output(id).test_document().unwrap();
                assert_eq!(document["result"], serde_json::json!({"result":"saved"}));
            }
            let mut projection = projection;
            let replay = reopen(jobs, root.path()).await;
            let mut replayed = stored_projection(&replay, id);
            // Resume handlers are live-only and never replayed.
            projection["resumable"] = false.into();
            replayed["resumable"] = false.into();
            assert_eq!(projection, replayed);
        }
    }

    /// A release lets go of a held child. A reply it made before is only recorded;
    /// from the release on its replies are queued and its completion is delivered,
    /// live and after reopening.
    #[tokio::test]
    async fn released_child_delivers_its_replies_and_completion_to_its_owner() {
        use crate::job::tests::commit;
        let (root, jobs, owner) = runtime().await;
        let spec = JobSpec {
            accepts_input: true,
            role: JobRole::Agent,
            ..JobSpec::test(owner.clone(), "agent")
        };
        let job = jobs.test_running(spec).await.into_test_id();
        let child =
            crate::session::tests::start_child(jobs.store(), &owner, 1, Some(job), root.path())
                .await;
        // A resumed child runs until its next input.
        let handler: super::super::ResumeHandler = std::sync::Arc::new(|_, mut input| {
            Box::pin(async move {
                input.recv().await;
                Ok(ToolOutput::new(serde_json::json!("done")))
            })
        });
        jobs.set_resume_handler(job, handler).await.unwrap();
        jobs.finish(job, ToolError::interrupted().into())
            .await
            .unwrap();
        commit(&jobs, &child, job, "held").await;
        assert!(!jobs.has_pending(&owner));
        jobs.test_release(&owner).await;
        jobs.send(job, serde_json::json!("resume")).await.unwrap();
        let reply = commit(&jobs, &child, job, "released").await;
        jobs.send(job, serde_json::json!("finish")).await.unwrap();
        crate::tests::bounded(jobs.wait_settled(job)).await.unwrap();

        let pending = async |jobs: &JobManager| {
            let receipt = jobs.pending_delivery(&owner).await.unwrap();
            let messages = receipt.messages().iter().map(|message| message.message);
            let envelopes = (receipt.envelopes().iter())
                .filter(|envelope| envelope.id == job)
                .map(|envelope| envelope.state);
            (messages.collect::<Vec<_>>(), envelopes.collect::<Vec<_>>())
        };
        let expected = (vec![reply], vec![JobState::Completed]);
        assert_eq!(pending(&jobs).await, expected);
        let reopened = reopen(jobs, root.path()).await;
        assert_eq!(pending(&reopened).await, expected);
    }
}

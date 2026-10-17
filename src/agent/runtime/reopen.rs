//! Reopening a saved session: viewing it, and activating it on its first call.

use super::*;

impl Harness {
    /// Open a saved session to view, holding it until it is closed. Nothing is
    /// journaled and no agent runs until the first call that changes it, which
    /// first settles what the process that last ran it left open.
    pub async fn resume_session(&self, id: SessionId) -> Result<SessionHandle, HarnessError> {
        let (store, _) = SessionStore::open(&self.inner.session_root, id).await?;
        let jobs = JobManager::new(store.clone());
        let open = Arc::new(Open(store.clone(), jobs.supervisor().clone()));
        let runtime = SessionRuntime::build(self.inner.clone(), store, jobs).await?;
        let root = AgentId::root(runtime.store.id());
        Ok(SessionHandle {
            runtime,
            root,
            _open: open,
        })
    }
}

impl SessionHandle {
    /// Run the session, once it is only viewed: the first call starts its
    /// activation, which runs on even if that call gives up, and a call while it
    /// prepares the root joins it. A root that cannot be prepared leaves the
    /// session viewed, with nothing journaled, for the next call to try again. A
    /// session that no longer runs refuses the call.
    pub(super) async fn running(&self) -> Result<(), HarnessError> {
        enum Activating {
            Decided(Result<(), SessionError>),
            Started,
            Joined,
        }
        loop {
            let (activated, outcome) = oneshot::channel();
            let runtime = self.runtime.clone();
            let decide = move |state: &mut CoreState| match (&state.phase, state.agents.activation)
            {
                (crate::session::SessionPhase::Viewing, Activation::Preparing) => {
                    Activating::Joined
                }
                (crate::session::SessionPhase::Viewing, Activation::Idle) => {
                    state.agents.activation = Activation::Preparing;
                    let supervisor = runtime.jobs.supervisor().clone();
                    let ends = EndsActivation(runtime.store.clone());
                    let activate = async move {
                        let activation = runtime.activate_root().await;
                        drop(ends);
                        activated.send(activation)
                    };
                    state.defer(move || drop(supervisor.spawn(activate)));
                    Activating::Started
                }
                (phase, _) if phase.runs() => Activating::Decided(Ok(())),
                (phase, _) => Activating::Decided(Err(phase.refusal())),
            };
            match self.runtime.store.live(decide).await {
                Activating::Decided(decided) => return Ok(decided?),
                Activating::Started => {
                    return match outcome.await {
                        Ok(activated) => activated,
                        Err(_) => Err(super::dropped_input(&self.runtime.store).await),
                    };
                }
                Activating::Joined => {
                    let ended = |state: &mut CoreState| {
                        (state.agents.activation == Activation::Idle).then_some(())
                    };
                    self.runtime.store.until(ended).await?;
                }
            }
        }
    }
}

/// Ends the activation however its task does, so the calls that joined it decide again.
struct EndsActivation(crate::session::SessionStore);

impl Drop for EndsActivation {
    fn drop(&mut self) {
        (self.0).enqueue(|state| state.agents.activation = Activation::Idle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TodoStatus;
    use crate::agent::runtime::tests::*;
    use crate::job::JobRole::{self, Tool};
    use crate::session::tests::{attempt, requested};

    /// What a process killed while `job` ran as `role` at `location` leaves
    /// journaled: the job, called from `origin` under `parent`, created, approved
    /// and running.
    fn left_running(
        agent: &AgentId,
        job: JobId,
        role: JobRole,
        (parent, origin): (Option<JobId>, Option<crate::session::ModelCallOrigin>),
        location: &Path,
    ) -> Vec<(AgentId, SessionEvent)> {
        use crate::job::JobTransition::{AwaitingApproval, Running};
        let created = SessionEvent::JobCreated {
            job,
            parent,
            origin,
            tool: role.as_str().into(),
            role,
            name: None,
            arguments: json!({"prompt": "work"}),
            output_schema: None,
            accepts_input: role == JobRole::Agent,
            background: false,
            location: crate::execution::ExecutionLocation::root(location.to_owned()),
        };
        let started =
            [AwaitingApproval, Running].map(|state| SessionEvent::JobStateChanged { job, state });
        let events = std::iter::once(created).chain(started);
        events.map(|event| (agent.clone(), event)).collect()
    }

    /// A session list row's last activity.
    async fn last_millis(sessions: &Path, id: crate::identity::SessionId) -> i64 {
        SessionStore::summary(sessions, id)
            .await
            .unwrap()
            .last_millis
    }

    /// A session answered once, shut down and reopened, ready to journal what a
    /// killed process leaves behind; `context` is its model context's sequence.
    struct Answered {
        root: tempfile::TempDir,
        sessions: PathBuf,
        requests: Requests,
        harness: Harness,
        store: SessionStore,
        agent: AgentId,
        id: crate::identity::SessionId,
        context: RecordSeq,
    }

    async fn answered_then_reopened() -> Answered {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let records = session.runtime.store.records();
        let context = (records.iter())
            .find(|record| matches!(record.event, SessionEvent::ModelContext { .. }))
            .unwrap()
            .sequence;
        let (id, agent) = (session.id(), session.root.clone());
        shutdown_session(session).await;
        let (store, _) = crate::session::tests::reopen(&sessions, id).await;
        Answered {
            root,
            sessions,
            requests,
            harness,
            store,
            agent,
            id,
            context,
        }
    }

    /// A crash can leave an attempt without an outcome and committed calls
    /// without a result, one of whose jobs completed first. Resume settles them
    /// once, in one transaction dated when the crashed process last did
    /// something: each call is answered with the interruption, and the outcome
    /// its waiter never read reaches its owner instead, live and on replay.
    #[tokio::test]
    async fn resume_settles_interrupted_attempts_and_calls_exactly_once() {
        let Answered {
            root,
            sessions,
            harness,
            store,
            agent: root_agent,
            id,
            context,
            ..
        } = answered_then_reopened().await;
        // What a process killed mid-turn leaves behind: one call's job still
        // running, and another's completed before its result committed.
        let calls = ["orphan", "done"]
            .into_iter()
            .enumerate()
            .map(|(position, id)| {
                let call = ToolCall::new(id, "read", json!({"path": "file"})).unwrap();
                AssistantItem::tool_call(id, position as u32, call)
            });
        let assistant = Message::Assistant(calls.collect());
        let committed = SessionEvent::MessageCommitted { message: assistant };
        let launch = store.append(root_agent.clone(), committed).await.unwrap();
        let call = |call_id: &str| crate::session::ModelCallOrigin {
            message: launch.sequence.message(),
            call_id: call_id.into(),
        };
        let [job, done] = [1, 2].map(|job| JobId::new(job).unwrap());
        let completed = SessionEvent::JobFinished {
            job: done,
            state: crate::job::JobEnd::Completed,
            diagnostic: None,
            images: Vec::new(),
        };
        let left =
            |job, call| left_running(&root_agent, job, Tool, (None, Some(call)), root.path());
        let mut journaled = left(job, call("orphan"));
        journaled.extend(left(done, call("done")));
        journaled.push((root_agent.clone(), completed));
        store.append_all(journaled).await.unwrap();
        let request = store.append(root_agent.clone(), requested(context)).await;
        let request = request.unwrap();
        let attempt = attempt(request.sequence.request(), 1);
        let last = store.append(root_agent.clone(), attempt).await.unwrap();
        drop(store);

        for resume in 0..2 {
            let opened = SessionStore::read_records(&sessions, id)
                .await
                .unwrap()
                .len();
            let resumed = harness.resume_session(id).await.unwrap();
            resumed.running().await.unwrap();
            let records = resumed.runtime.store.records();
            let settled = &records[opened..];
            assert_eq!(settled.is_empty(), resume > 0, "{resume}");
            assert!(
                settled
                    .iter()
                    .all(|record| record.timestamp_millis == last.timestamp_millis)
            );
            let interrupted = events!(&records,
                SessionEvent::ModelAttemptInterrupted(attempt) => (attempt.request, attempt.attempt));
            assert_eq!(interrupted, [(request.sequence.request(), 1)]);
            let results = events!(&records,
                SessionEvent::MessageCommitted { message: Message::Tool(result) } => result.clone());
            let placeholders = (results.iter())
                .map(|result| (result.call_id.as_str(), result.is_error))
                .collect::<Vec<_>>();
            assert_eq!(placeholders, [("orphan", true), ("done", true)]);
            // The settled turn stays continuable on every resume, not only the
            // one that settled it.
            let parked = turn(&resumed, &resumed.root);
            assert!(matches!(parked, Turn::Parked(_)), "{resume}: {parked:?}");
            // A placeholder reports only the interruption reopening gave its job:
            // the earlier completion stays pending, live and on replay.
            let replayed = resumed.runtime.jobs.test_replay();
            for jobs in [&resumed.runtime.jobs, &replayed] {
                let pending = jobs.pending_delivery(&resumed.root).await.unwrap();
                let pending = pending.envelopes().iter().map(|envelope| envelope.id);
                assert_eq!(pending.collect::<Vec<_>>(), [done], "{resume}");
            }
            shutdown_session(resumed).await;
        }
    }

    /// A process stopped after a response's results commit, before its next
    /// request, leaves the turn open: reopening ends it interrupted, in the one
    /// transaction that settles the reopen, so `continue` takes the results to the
    /// model without running the call again.
    #[tokio::test]
    async fn reopening_after_results_before_the_next_request_ends_the_turn() {
        let Answered {
            root,
            sessions,
            requests,
            harness: _answered,
            store,
            agent,
            id,
            context,
        } = answered_then_reopened().await;
        // A response whose call ran and whose results committed, and nothing after.
        let text = "go".into();
        let prompt = Message::User(vec![crate::session::UserPart::Text { text }]);
        let prompt = SessionEvent::MessageCommitted { message: prompt };
        store.append(agent.clone(), prompt).await.unwrap();
        let request = store.append(agent.clone(), requested(context)).await;
        let request = request.unwrap().sequence.request();
        store
            .append(agent.clone(), attempt(request, 1))
            .await
            .unwrap();
        let call = ToolCall::new("s", "script", json!({"source": "return 42;"})).unwrap();
        let call = Message::Assistant(vec![AssistantItem::tool_call("s", 0, call)]);
        let call = SessionEvent::MessageCommitted { message: call };
        let message = store.append(agent.clone(), call).await.unwrap();
        let job = JobId::new(1).unwrap();
        let origin = crate::session::ModelCallOrigin {
            message: message.sequence.message(),
            call_id: "s".into(),
        };
        let mut ran = left_running(&agent, job, Tool, (None, Some(origin)), root.path());
        ran.push((
            agent.clone(),
            SessionEvent::JobFinished {
                job,
                state: crate::job::JobEnd::Completed,
                diagnostic: None,
                images: Vec::new(),
            },
        ));
        let result = crate::provider::protocol::ToolResult {
            call_id: "s".into(),
            name: "script".into(),
            result: json!({"result": {"value": 42}}),
            images: Vec::new(),
            is_error: false,
        };
        let completed = SessionEvent::ResponseCompleted {
            attempt: crate::session::AttemptRef {
                request,
                attempt: 1,
            },
            message: message.sequence.message(),
            outcome: crate::session::CompletedOutcome::ToolUse,
        };
        ran.push((agent.clone(), completed));
        let results = SessionEvent::MessageCommitted {
            message: Message::Tool(result),
        };
        ran.push((agent.clone(), results));
        ran.push((agent.clone(), SessionEvent::JobClaimed { job }));
        store.append_all(ran).await.unwrap();
        let prefix = store.records().len();
        drop(store);

        let provider = scripted_provider(&requests, [answer("continued")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        let records = resumed.runtime.store.records();
        let settled: Vec<_> = records[prefix..]
            .iter()
            .map(|r| (&r.agent, &r.event))
            .collect();
        assert_eq!(settled, [(&agent, &SessionEvent::AgentInterrupted)]);
        assert_eq!(bounded(resumed.continue_turn()).await.unwrap(), "continued");
        let last = requests.lock().unwrap().last().unwrap().request.clone();
        use crate::provider::protocol::{Message as Sent, UserContent};
        // Its history ends with the results; only the transient state follows them.
        let runtime = |message: &&Sent| match message {
            Sent::User(parts) => parts.iter().all(UserContent::is_runtime),
            _ => false,
        };
        let history = last.messages().rev().find(|message| !runtime(message));
        assert!(matches!(history, Some(Sent::Tool(_))), "{history:?}");
        assert_eq!(resumed.runtime.jobs.list(&resumed.root).len(), 1);
        shutdown_session(resumed).await;
    }

    /// A crash during a retry backoff, or between a request and its first attempt,
    /// leaves a request waiting for an attempt that never comes. Resume settles the
    /// request as interrupted, once, like an open attempt. What it closes, a running job too, is dated when the crashed process
    /// last did something, so reopening leaves the session's last activity alone,
    /// and so does closing it again with the job still interrupted.
    #[tokio::test]
    async fn resume_settles_requests_left_waiting_for_an_attempt() {
        use crate::session::{AttemptRef, EventRecord, RequestLedger, RequestPhase, RequestSeq};
        let Answered {
            root,
            sessions,
            harness,
            store,
            agent: root_agent,
            id,
            context,
            ..
        } = answered_then_reopened().await;
        let settled = |records: &[EventRecord], request: RequestSeq| {
            let mut ledger = RequestLedger::default();
            for record in records {
                ledger.observe(record);
            }
            ledger.get(request).unwrap().phase.clone()
        };
        let interrupted = |records: &[EventRecord]| count!(records, SessionEvent::AgentInterrupted);
        // What a killed process leaves behind: a running job, and a request failed
        // once, with the retry scheduled but never started.
        let job = crate::identity::JobId::new(1).unwrap();
        let running = left_running(&root_agent, job, Tool, (None, None), root.path());
        store.append_all(running).await.unwrap();
        let retrying = store
            .append(root_agent.clone(), requested(context))
            .await
            .unwrap();
        let attempt = AttemptRef {
            request: retrying.sequence.request(),
            attempt: 1,
        };
        let started = SessionEvent::ModelAttemptStarted(attempt);
        store.append(root_agent.clone(), started).await.unwrap();
        let failed = SessionEvent::ModelFailed {
            attempt,
            failure: crate::session::AttemptFailure::Retrying {
                message: "connection lost".into(),
                kind: crate::provider::ProviderErrorKind::Transport,
                delay: std::time::Duration::from_secs(60).into(),
            },
        };
        let last = store.append(root_agent.clone(), failed).await.unwrap();
        let before = interrupted(&store.records());
        drop(store);
        assert_eq!(last_millis(&sessions, id).await, last.timestamp_millis);
        // Reopen strictly later, so a repair dated now would show.
        crate::tests::past(last.timestamp_millis).await;
        let resumed = harness.resume_session(id).await.unwrap();
        let request = retrying.sequence.request();
        let cut = |attempt| RequestPhase::Interrupted {
            attempt,
            at: last.timestamp_millis,
        };
        // Viewed, it already reads as the settlement leaves it.
        let viewed = resumed.observe().snapshot.ledger;
        assert_eq!(viewed.get(request).unwrap().phase, cut(Some(1)));
        resumed.running().await.unwrap();
        let records = resumed.runtime.store.records();
        assert_eq!(settled(&records, request), cut(Some(1)));
        assert_eq!(interrupted(&records), before + 1);
        let repairs = records.iter().filter(|record| {
            record.sequence > last.sequence
                && matches!(
                    record.event,
                    SessionEvent::JobFinished { .. } | SessionEvent::AgentInterrupted
                )
        });
        let repaired = repairs
            .map(|record| record.timestamp_millis)
            .collect::<Vec<_>>();
        assert_eq!(repaired, [last.timestamp_millis; 2]);
        assert_eq!(last_millis(&sessions, id).await, last.timestamp_millis);
        shutdown_session(resumed).await;
        assert_eq!(last_millis(&sessions, id).await, last.timestamp_millis);

        // Requested, and never attempted.
        let (store, _) = crate::session::tests::reopen(&sessions, id).await;
        let waiting = store
            .append(root_agent.clone(), requested(context))
            .await
            .unwrap();
        let before = interrupted(&store.records());
        drop(store);
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        let records = resumed.runtime.store.records();
        let request = waiting.sequence.request();
        let cut = RequestPhase::Interrupted {
            attempt: None,
            at: waiting.timestamp_millis,
        };
        assert_eq!(settled(&records, request), cut);
        assert_eq!(interrupted(&records), before + 1);
        shutdown_session(resumed).await;
    }

    /// Closing ends running work as a crash would: what it cuts short ends at the
    /// session's last activity, which closing leaves where it was.
    #[tokio::test]
    async fn closing_cuts_running_work_short_at_the_last_activity() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, Arc::new(HangingProvider)).await;
        let session = harness.new_session().await.unwrap();
        let script = tokio::spawn({
            let session = session.clone();
            async move { session.run_script("await sleep(60000);").await }
        });
        let job = job_found(&session, &session.root, |job| {
            job.state == JobState::Running
        })
        .await;
        let id = session.id();
        let before = last_millis(&sessions, id).await;
        // Close strictly later, so an entry dated now would show.
        crate::tests::past(before).await;
        shutdown_session(session).await;
        assert!(bounded(script).await.unwrap().is_err());
        let records = SessionStore::read_records(&sessions, id).await.unwrap();
        let ended =
            events!(&records, SessionEvent::JobFinished { job: ended, .. } if *ended == job => ());
        assert_eq!(ended.len(), 1);
        assert!(
            records
                .iter()
                .all(|record| record.timestamp_millis <= before)
        );
        assert_eq!(last_millis(&sessions, id).await, before);
    }

    /// Closing stops agents in no particular order. A child stopping before the
    /// turn held on it is released leaves that turn releasable, and closing a
    /// settled interruption, with nothing left running, is not activity.
    #[tokio::test]
    async fn shutdown_releases_a_turn_whose_interrupted_child_stopped_first() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let (_harness, session, _, _, parent, job) = held_on_interrupted_child(
            root.path(),
            [answer("child recovered"), answer("root continued")].map(Step::new),
        )
        .await;
        until(&session, job, |job| job.state == JobState::Interrupted).await;
        let sessions = root.path().join("sessions");
        let id = session.id();
        let before = last_millis(&sessions, id).await;
        // Close strictly later, so an entry dated now would show.
        crate::tests::past(before).await;
        let child = session.root.child(1);
        let shutdown = session.runtime.store.enter(Command::BeginShutdown);
        shutdown.await.unwrap();
        let stopped = move |state: &mut CoreState| state.agents.get(&child).is_none().then_some(());
        bounded(session.runtime.store.until(stopped)).await.unwrap();
        shutdown_session(session).await;
        assert!(bounded(parent).await.unwrap().is_err());
        assert_eq!(last_millis(&sessions, id).await, before);
    }

    /// A continue once closing has begun is refused and journals nothing, and the
    /// interruption resumes when the session reopens.
    #[tokio::test]
    async fn a_continue_after_closing_began_leaves_the_interruption_resumable() {
        use crate::job::JobState;
        let root = tempfile::tempdir().unwrap();
        let recovered = (0..4).map(|_| Step::new(answer("recovered")));
        let (harness, session, _, _, parent, job) =
            held_on_interrupted_child(root.path(), recovered).await;
        bounded(session.shutdown()).await.unwrap();
        assert!(bounded(parent).await.unwrap().is_err());
        let closed = session.runtime.store.records();
        let refused = bounded(session.continue_turn()).await;
        assert!(
            matches!(
                refused,
                Err(HarnessError::Session(SessionError::Refused(
                    crate::session::SessionPhase::ShuttingDown
                )))
            ),
            "{refused:?}"
        );
        assert_eq!(session.runtime.store.records(), closed);
        session.close().await.unwrap();

        let resumed = harness.resume_session(session.id()).await.unwrap();
        let state = resumed.runtime.jobs.snapshot(job).unwrap().state;
        assert_eq!(state, JobState::Interrupted);
        bounded(resumed.continue_turn()).await.unwrap();
        assert_eq!(terminal(&resumed, job).await.state, JobState::Completed);
        shutdown_session(resumed).await;
    }

    /// A reopened session journals nothing until a call changes it. One whose root
    /// cannot be prepared, its model no longer configured, stays viewed: every
    /// such call reports why, and what the crashed process left open is neither
    /// settled nor shown running, before its shutdown or after it, when every
    /// such call is refused.
    #[tokio::test]
    async fn a_resume_whose_root_cannot_start_commits_nothing() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let gone = crate::tests::profile("gone-model", false);
        let harness = serving(root.path(), provider.clone(), [("gone", gone)])
            .default_model(model_ref("gone"))
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let (id, agent) = (session.id(), session.root.clone());
        shutdown_session(session).await;
        let job = crashed_running(&sessions, id, &agent).await;
        let crashed = SessionStore::read_records(&sessions, id).await.unwrap();
        let harness = test_harness(root.path(), &sessions, provider).await;
        let resumed = harness.resume_session(id).await.unwrap();
        // Concurrent first calls join an activation, or try again once it failed.
        let (first, second) = tokio::join!(
            resumed.record_status(agent.clone(), "acted".into()),
            resumed.record_status(agent.clone(), "acted".into()),
        );
        for refused in [first, second] {
            let gone = model_ref("gone");
            assert!(
                matches!(&refused, Err(HarnessError::UnknownModel(model)) if *model == gone),
                "{refused:?}"
            );
            let viewed = resumed.observe().snapshot;
            assert_eq!(viewed.phase, crate::session::SessionPhase::Viewing);
            assert_eq!(viewed.jobs[&job].state, crate::job::JobState::Interrupted);
        }
        // Shut down, a viewed session is closed: it reads as it did, can no longer
        // be activated or journal, and is released for another open at once.
        resumed.shutdown().await.unwrap();
        let viewed = resumed.observe().snapshot;
        assert_eq!(viewed.jobs[&job].state, crate::job::JobState::Interrupted);
        let closed = crate::session::SessionPhase::Closed;
        let prompted = resumed.prompt("late").await.map(drop);
        let recorded = resumed.record_status(agent.clone(), "late".into()).await;
        for refused in [prompted, recorded] {
            assert!(
                matches!(&refused, Err(HarnessError::Session(SessionError::Refused(phase))) if *phase == closed),
                "{refused:?}"
            );
        }
        drop(SessionStore::open(&sessions, id).await.unwrap());
        resumed.close().await.unwrap();
        let journal = SessionStore::read_records(&sessions, id).await.unwrap();
        assert_eq!(journal, crashed);
    }

    /// Journal what a crash leaves in session `id`: a job running.
    async fn crashed_running(sessions: &Path, id: SessionId, agent: &AgentId) -> JobId {
        let (store, _) = crate::session::tests::reopen(sessions, id).await;
        let job = JobId::new(1).unwrap();
        let running = left_running(agent, job, Tool, (None, None), Path::new("."));
        store.append_all(running).await.unwrap();
        job
    }

    /// A first call starts the activation and need not see it through: its root
    /// is prepared, and the session activated, without it.
    #[tokio::test]
    async fn a_first_call_given_up_on_still_activates_the_session() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first"), answer("again")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        assert_eq!(session.prompt("hello").await.unwrap(), "first");
        let (id, agent) = (session.id(), session.root.clone());
        shutdown_session(session).await;
        // Something to settle, so activating commits.
        crashed_running(&sessions, id, &agent).await;
        let resumed = harness.resume_session(id).await.unwrap();
        let mut first = Box::pin(resumed.running());
        assert!(futures_util::poll!(&mut first).is_pending());
        drop(first);
        let running = |state: &mut CoreState| state.phase.runs().then_some(());
        bounded(resumed.runtime.store.until(running)).await.unwrap();
        assert_eq!(bounded(resumed.prompt("go")).await.unwrap(), "again");
        shutdown_session(resumed).await;
    }

    /// A turn that failed or was interrupted before the session closed reopens
    /// stopped for that reason and is continued; a finished one leaves nothing to
    /// continue.
    #[tokio::test]
    async fn reopened_failed_or_interrupted_turn_continues() {
        use crate::agent::{AgentActivity, Failure, TurnFailure};
        let stopped = async |session: &SessionHandle| {
            let activity = session.observe().snapshot.activity;
            match &activity[&session.root].state {
                AgentActivity::Stopped(failure) => Some(failure.clone()),
                _ => None,
            }
        };
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let failure = crate::provider::ProviderErrorKind::Protocol.error("not retried");
        let steps = [
            Step::fail(failure),
            Step::new(answer("recovered")),
            Step::new(answer("never released")).midstream(),
            Step::new(answer("continued")),
        ];
        let script = Script::new(steps, &requests);
        let harness = test_harness(root.path(), &sessions, script.clone()).await;
        let session = harness.new_session().await.unwrap();
        let id = session.id();
        session.prompt("fails").await.unwrap_err();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        let failed = stopped(&resumed).await;
        assert!(matches!(
            failed,
            Some(TurnFailure::Failed(Failure::Provider(..)))
        ));
        assert_eq!(resumed.continue_turn().await.unwrap(), "recovered");
        let interrupted = resumed.prompt("interrupted");
        let interrupt = async {
            script.held(2).await;
            resumed.interrupt().await;
        };
        let (result, _) = tokio::join!(interrupted, interrupt);
        result.unwrap_err();
        shutdown_session(resumed).await;

        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(stopped(&resumed).await, Some(TurnFailure::Interrupted));
        assert_eq!(resumed.continue_turn().await.unwrap(), "continued");
        shutdown_session(resumed).await;

        let resumed = harness.resume_session(id).await.unwrap();
        assert_eq!(stopped(&resumed).await, None);
        assert_eq!(resumed.continue_turn().await.unwrap(), "");
        assert_eq!(requests.lock().unwrap().len(), 4);
        shutdown_session(resumed).await;
    }

    /// A selection proves membership in the runtime instance that issued it; the
    /// same session resumed is another instance with its own catalog.
    #[tokio::test]
    async fn selections_do_not_survive_a_resume() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first"), answer("second")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        let model = session.runtime.harness.default_model.clone();
        let stale = session.selection(Some(&model), None).unwrap();
        let id = session.id();
        shutdown_session(session).await;
        let resumed = harness.resume_session(id).await.unwrap();
        let rejected = resumed.prompt_with_options("hello", &[], stale).await;
        assert!(matches!(rejected, Err(HarnessError::UnknownModel(name)) if name == model));
        let fresh = resumed.selection(Some(&model), None).unwrap();
        let answered = resumed.prompt_with_options("hello", &[], fresh).await;
        assert_eq!(answered.unwrap(), "first");
        shutdown_session(resumed).await;
    }

    /// A resumed agent keeps its journaled prompt, tools and capabilities. Live
    /// configuration narrows them: a pinned tool it no longer allows is shown but
    /// never executed, a newly allowed capability is not granted, and a lower
    /// depth limit applies.
    #[tokio::test]
    async fn resumed_agents_keep_their_journaled_contract_and_config_only_narrows_it() {
        use crate::tool::policy::{Capability, CapabilitySet};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let provider = scripted_provider(&requests, [answer("first")]);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let session = harness.new_session().await.unwrap();
        session.prompt("hello").await.unwrap();
        let id = session.id();
        shutdown_session(session).await;
        let original = requests.lock().unwrap()[0].clone();

        let mut narrowed = CapabilitySet::default();
        narrowed.remove(Capability::Write);
        narrowed.insert(Capability::Targets);
        let write = json!({"path": "must-not-exist", "content": "unsafe"});
        let responses = [
            response(vec![tool_call(0, "write", "write", write)]),
            answer("done"),
        ];
        let provider = scripted_provider(&requests, responses);
        let resumed = test_builder(root.path(), &sessions, provider, false)
            .capabilities(narrowed)
            // A lowered depth limit narrows the resumed root instead of refusing it.
            .max_child_depth(0)
            .build()
            .await
            .unwrap()
            .resume_session(id)
            .await
            .unwrap();
        assert_eq!(resumed.prompt("write it").await.unwrap(), "done");
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured[1].tools, original.tools);
        assert_eq!(captured[1].system, original.system);
        let Some(Sent::Tool(results)) = captured[2].messages().last() else {
            panic!("expected the write result");
        };
        assert!(results[0].is_error);
        assert!(results[0].result.to_string().contains("unavailable"));
        assert!(!root.path().join("must-not-exist").exists());
        let records = resumed.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::ModelChanged { .. }), 0);
        shutdown_session(resumed).await;
    }

    /// A reopened session runs the configured modes and capabilities, wider or
    /// narrower than when it last ran. Viewing it, or work that does not run the
    /// root, only warns of a changed mode; the next work that runs the root decides
    /// its mode once, with the message it commits or before a script, moving onto
    /// the configured edition or onto the default mode once its own is gone. The
    /// journal keeps every edition it applied.
    #[tokio::test]
    async fn resume_follows_the_configured_modes_and_journals_their_editions() {
        use crate::tool::policy::{Capability, CapabilitySet, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let mode = |capabilities: &[Capability], instructions: &str| Mode {
            capabilities: capabilities.to_vec(),
            instructions: Some(instructions.parse().unwrap()),
            hint: None,
        };
        let build = |ceiling: CapabilitySet, modes: &[(&str, Mode)], reply: &str| {
            let provider = scripted_provider(&requests, [answer(reply)]);
            let modes = modes.iter().cloned();
            test_builder(root.path(), &sessions, provider, false)
                .capabilities(ceiling)
                .modes(
                    modes
                        .map(|(name, mode)| (name.parse().unwrap(), mode))
                        .collect(),
                )
                .build()
        };
        let look = ("look", mode(&[Capability::Read], "Look."));
        let work = (
            "work",
            mode(&[Capability::Read, Capability::Write], "Work."),
        );
        let narrow: CapabilitySet = [Capability::Read].into_iter().collect();
        let harness = build(narrow, &[look.clone(), work.clone()], "one").await;
        let session = harness.unwrap().new_session().await.unwrap();
        session.prompt("start").await.unwrap();
        let id = session.id();
        shutdown_session(session).await;

        let named = |name: &str, definition: Option<&Mode>| crate::session::ModeSelection {
            name: mode_name(name),
            definition: definition.cloned(),
        };
        let granted = |session: &SessionHandle, name: &str| -> Vec<Capability> {
            let granted = session.mode_capabilities(&mode_name(name)).unwrap();
            granted.iter().collect()
        };
        let in_mode = |session: &SessionHandle, mode: &str| {
            session.selection(None, Some(&mode_name(mode))).unwrap()
        };
        let mut expected = Vec::new();
        // A wider live ceiling widens the session: `work` now grants write.
        let wide = CapabilitySet::default;
        let harness = build(wide(), &[look.clone(), work.clone()], "two");
        let resumed = harness.await.unwrap().resume_session(id).await.unwrap();
        assert!(resumed.startup_warnings().is_empty());
        let widened = granted(&resumed, "work");
        assert!(widened.contains(&Capability::Write));
        // The root's own mode now grants more too, but the message names `work`:
        // that is the one change.
        assert!(resumed.changes_mode(&mode_name("look")));
        let prompt = resumed.prompt_with_options("widen", &[], in_mode(&resumed, "work"));
        assert_eq!(prompt.await.unwrap(), "two");
        expected.push((named("work", Some(&work.1)), widened));
        shutdown_session(resumed).await;

        // Redefining `look`, which the root no longer runs, warns of nothing; its
        // next use records the configured edition.
        let relook = ("look", mode(&[], "Changed."));
        let harness = build(wide(), &[relook.clone(), work.clone()], "three");
        let resumed = harness.await.unwrap().resume_session(id).await.unwrap();
        assert!(resumed.startup_warnings().is_empty());
        assert!(resumed.changes_mode(&mode_name("look")));
        assert!(!resumed.changes_mode(&mode_name("work")));
        // Activating work that does not run the root leaves its mode alone.
        let root = resumed.root_agent().clone();
        resumed.rename(Some("renamed".into())).await.unwrap();
        resumed.record_status(root, "noted".into()).await.unwrap();
        let prompt = resumed.prompt_with_options("next", &[], in_mode(&resumed, "look"));
        assert_eq!(prompt.await.unwrap(), "three");
        expected.push((named("look", Some(&relook.1)), granted(&resumed, "look")));
        shutdown_session(resumed).await;

        // Once `look`, which the root runs, is gone, viewing warns; a script runs the
        // root in the default mode, and the message after it changes nothing more.
        let harness = build(wide(), std::slice::from_ref(&work), "four");
        let resumed = harness.await.unwrap().resume_session(id).await.unwrap();
        let warnings = resumed.startup_warnings().join("\n");
        assert!(warnings.contains("no longer has mode `look`"), "{warnings}");
        assert!(warnings.contains("default mode `work`"), "{warnings}");
        resumed.run_script("return 1;").await.unwrap();
        assert_eq!(resumed.prompt("last").await.unwrap(), "four");
        expected.push((named("work", None), granted(&resumed, "work")));
        shutdown_session(resumed).await;
        // Adopted, it warns no more.
        let harness = build(wide(), std::slice::from_ref(&work), "unused");
        let resumed = harness.await.unwrap().resume_session(id).await.unwrap();
        assert!(resumed.startup_warnings().is_empty());
        shutdown_session(resumed).await;

        let (_store, records) = SessionStore::open(&sessions, id).await.unwrap();
        let changes = events!(&records, SessionEvent::ModeChanged { mode, capabilities } => (mode.clone(), capabilities.clone()));
        assert_eq!(changes, expected);
        // A message's change commits with it; a script's precedes the script.
        let follows =
            records
                .windows(2)
                .filter_map(|pair| match (&pair[0].event, &pair[1].event) {
                    (SessionEvent::ModeChanged { .. }, next) => Some(match next {
                        SessionEvent::MessageCommitted {
                            message: crate::session::Message::User(_),
                        } => "message",
                        SessionEvent::JobCreated { tool, .. } if tool == "script" => "script",
                        _ => "other",
                    }),
                    _ => None,
                });
        assert_eq!(
            follows.collect::<Vec<_>>(),
            ["message", "message", "script"]
        );
        let captured = requests.lock().unwrap().clone();
        let prompt = |index: usize| captured[index].system[0].text.clone();
        assert!(prompt(2).contains("Changed.") && !prompt(2).contains("Look."));
        assert!(prompt(3).contains("Work."));
    }

    /// A retained child accepts owner input after a restart: its loop starts again
    /// under its journaled contract and continues its own history. The events
    /// delivered to its reopened root, which runs a redefined mode, commit in one
    /// transaction with the mode change its turn makes, ahead of the delivery.
    #[tokio::test]
    async fn a_retained_child_resumes_after_a_restart_and_reaches_its_reopened_root() {
        use crate::tool::policy::{Capability, Mode};
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let requests = Requests::default();
        let look = |instructions: &str| Mode {
            capabilities: vec![Capability::Read, Capability::Agents],
            instructions: Some(instructions.parse().unwrap()),
            hint: None,
        };
        let build = |provider, instructions: &str| {
            let modes = [(mode_name("look"), look(instructions))]
                .into_iter()
                .collect();
            test_builder(root.path(), &sessions, provider, false)
                .modes(modes)
                .build()
        };
        let delegate = tool_call(0, "delegate", "agent", json!({"prompt": "work"}));
        let responses = [
            response(vec![delegate]),
            answer("child done"),
            answer("root done"),
        ];
        let provider = scripted_provider(&requests, responses);
        let session = build(provider, "Look.").await.unwrap().new_session().await;
        let session = session.unwrap();
        assert_eq!(session.prompt("delegate").await.unwrap(), "root done");
        let records = session.runtime.store.records();
        let job = events!(&records, SessionEvent::JobCreated { job, tool, .. } if tool == "agent" => *job)
            [0];
        let id = session.id();
        shutdown_session(session).await;

        // Owner input runs the retained child, whose answer reaches the idle root.
        let responses = [answer("child resumed"), answer("root again")];
        let provider = scripted_provider(&requests, responses);
        let harness = build(provider, "Changed.").await.unwrap();
        let resumed = harness.resume_session(id).await.unwrap();
        // The root answered, so continuing only opens the session: it sends the
        // root no input that could carry the mode change first.
        let continued = resumed.continue_turn_with(Selection::default()).await;
        assert!(continued.unwrap().answer.is_none());
        // A later commit beside the journaled child leaves its activity alone.
        let last = resumed
            .runtime
            .store
            .records()
            .last()
            .unwrap()
            .timestamp_millis;
        crate::tests::past(last).await;
        let mut updates = resumed.observe().updates;
        resumed.rename(Some("renamed".into())).await.unwrap();
        let child = resumed.root.child(1);
        while let Ok(event) = updates.try_recv() {
            assert!(!matches!(event.event, RuntimeEvent::Activity { agent, .. } if agent == child));
        }
        resumed.runtime.jobs.send(job, json!("more")).await.unwrap();
        let root = resumed.root.clone();
        let answered = move |state: &mut CoreState| {
            let answers = state.records.iter().filter(|record| {
                record.agent == root
                    && matches!(
                        &record.event,
                        SessionEvent::MessageCommitted {
                            message: Message::Assistant(_),
                        }
                    )
            });
            (answers.count() == 3).then_some(())
        };
        found(&resumed, answered).await;
        let records = resumed.runtime.store.records();
        let changed = (records.iter())
            .position(|record| matches!(record.event, SessionEvent::ModeChanged { .. }))
            .unwrap();
        let (change, delivery) = (&records[changed], &records[changed + 1]);
        let SessionEvent::ModeChanged { mode, .. } = &change.event else {
            unreachable!()
        };
        assert_eq!(mode.definition, Some(look("Changed.")));
        let SessionEvent::MessageCommitted {
            message: Message::User(parts),
        } = &delivery.event
        else {
            panic!("the delivery follows its mode change: {:?}", delivery.event);
        };
        assert!(matches!(parts[..], [UserPart::JobEvents { .. }]));
        // One commit dates every entry it journals alike.
        assert_eq!(change.timestamp_millis, delivery.timestamp_millis);
        let captured = requests.lock().unwrap().clone();
        let last = captured.last().unwrap();
        assert!(last.system[0].text.contains("Changed."));
        let child = captured
            .iter()
            .rev()
            .find(|request| request.context_id == ContextId::from(&child))
            .expect("the child made a request after the restart");
        let history = serde_json::to_string(&child.context).unwrap();
        assert!(history.contains("work") && history.contains("child done"));
        assert!(history.contains("Owner input"));
        shutdown_session(resumed).await;
    }

    /// A session whose turn in `look` ended as `last` says, reopened to answer
    /// `replies` after the configuration redefined `look`.
    async fn reopened_in_a_redefined_mode<const N: usize>(
        root: &Path,
        sessions: &Path,
        last: Step,
        replies: [Step; N],
    ) -> SessionHandle {
        use crate::tool::policy::{Capability, Mode};
        let requests = Requests::default();
        let build = |provider, instructions: &str| {
            let look = Mode {
                capabilities: vec![Capability::Read],
                instructions: Some(instructions.parse().unwrap()),
                hint: None,
            };
            let modes = [(mode_name("look"), look)].into_iter().collect();
            test_builder(root, sessions, provider, false)
                .modes(modes)
                .build()
        };
        let provider = Script::new([last], &requests);
        let session = build(provider, "Look.").await.unwrap().new_session().await;
        let session = session.unwrap();
        let _ = session.prompt("start").await;
        let id = session.id();
        shutdown_session(session).await;
        let provider = Script::new(replies, &requests);
        let resumed = build(provider, "Changed.")
            .await
            .unwrap()
            .resume_session(id)
            .await;
        let resumed = resumed.unwrap();
        let warnings = resumed.startup_warnings().join("\n");
        assert!(warnings.contains("changed mode `look`"), "{warnings}");
        resumed.running().await.unwrap();
        assert!(resumed.changes_mode(&mode_name("look")));
        resumed
    }

    /// A script's mode change begins no turn: an interrupt that lands while it
    /// commits stops nothing, and the script runs under the new mode.
    #[tokio::test]
    async fn an_interrupt_during_a_scripts_mode_change_stops_nothing() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let answered = Step::new(answer("done"));
        let resumed = reopened_in_a_redefined_mode(root.path(), &sessions, answered, []).await;
        let changed: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::ModeChanged { .. });
        let interrupted = interrupt_on(&resumed, changed).await;
        let script = tokio::spawn({
            let resumed = resumed.clone();
            async move { resumed.run_script("return 1;").await }
        });
        assert_eq!(bounded(interrupted).await, 0);
        bounded(script).await.unwrap().unwrap();
        let records = resumed.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);
        assert!(!resumed.changes_mode(&mode_name("look")));
        shutdown_session(resumed).await;
    }

    /// While a script's mode change commits, a reopened root whose turn failed
    /// stays parked, so a `continue` issued then still resumes that turn.
    #[tokio::test]
    async fn a_continue_during_a_scripts_mode_change_resumes_the_parked_turn() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let rejected = crate::provider::ProviderErrorKind::InvalidRequest.error("rejected");
        let replies = [Step::new(answer("continued"))];
        let resumed =
            reopened_in_a_redefined_mode(root.path(), &sessions, Step::fail(rejected), replies)
                .await;
        let changed: fn(&SessionEvent) -> bool =
            |event| matches!(event, SessionEvent::ModeChanged { .. });
        // Queued behind the change: it finds the root still parked.
        let continuing = resumed.clone();
        let continued = on_commit(&resumed.runtime.store, changed, move || async move {
            continuing.continue_turn_with(Selection::default()).await
        });
        let continued = continued.await;
        let script = tokio::spawn({
            let resumed = resumed.clone();
            async move { resumed.run_script("return 1;").await }
        });
        let continued = bounded(continued).await.unwrap();
        assert_eq!(continued.answer.as_deref(), Some("continued"));
        bounded(script).await.unwrap().unwrap();
        let records = resumed.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::ModeChanged { .. }), 1);
        shutdown_session(resumed).await;
    }

    /// How a process was killed while a foreground child worked.
    #[derive(Clone, Copy, Debug)]
    enum Crash {
        /// With the root's call to it open.
        Unanswered,
        /// With the root's call answered as interrupted while the child, restarted,
        /// went on: the answer never let go of it.
        Answered,
        /// With the host's script that called it running.
        HostCalled,
    }

    /// A process killed while a foreground child works leaves its call waiting.
    /// Nothing waits across a restart: resume settles an open call with an error
    /// result and lets go of the child, which retry restarts, and its answer
    /// reaches the root as an event.
    #[tokio::test]
    async fn retry_after_a_crash_delivers_a_retained_foreground_childs_answer() {
        for crash in [Crash::Unanswered, Crash::Answered, Crash::HostCalled] {
            crash_then_retry(crash).await;
        }
    }

    async fn crash_then_retry(crash: Crash) {
        let Answered {
            root,
            sessions,
            requests,
            harness: _answered,
            store,
            agent: root_agent,
            id,
            ..
        } = answered_then_reopened().await;
        // What a process killed while its foreground child worked leaves behind.
        let running = |job, role, called| left_running(&root_agent, job, role, called, root.path());
        let script = crate::identity::JobId::new(1).unwrap();
        let (job, parent, origin) = match crash {
            Crash::HostCalled => {
                let host = running(script, JobRole::Script, (None, None));
                store.append_all(host).await.unwrap();
                (crate::identity::JobId::new(2).unwrap(), Some(script), None)
            }
            Crash::Unanswered | Crash::Answered => {
                let call = ToolCall::new("delegate", "agent", json!({"prompt": "work"})).unwrap();
                let call = AssistantItem::tool_call("delegate", 0, call);
                let message = SessionEvent::MessageCommitted {
                    message: Message::Assistant(vec![call]),
                };
                let launch = store.append(root_agent.clone(), message).await.unwrap();
                let origin = crate::session::ModelCallOrigin {
                    message: launch.sequence.message(),
                    call_id: "delegate".into(),
                };
                (crate::identity::JobId::new(1).unwrap(), None, Some(origin))
            }
        };
        let agent = running(job, JobRole::Agent, (parent, origin));
        store.append_all(agent).await.unwrap();
        let location = crate::execution::ExecutionLocation::root(root.path().to_owned());
        let started = crate::session::tests::child_started(Some(job), location);
        store.append(root_agent.child(1), started).await.unwrap();
        let read = crate::identity::JobId::new(3).unwrap();
        if let Crash::HostCalled = crash {
            store
                .append_all(running(read, Tool, (None, None)))
                .await
                .unwrap();
            let finished = SessionEvent::JobFinished {
                job: read,
                state: crate::job::JobEnd::Completed,
                diagnostic: None,
                images: Vec::new(),
            };
            store.append(root_agent.clone(), finished).await.unwrap();
        }
        if let Crash::Answered = crash {
            let result = crate::provider::protocol::ToolResult {
                call_id: "delegate".into(),
                name: "agent".into(),
                result: json!({"error": "interrupted"}),
                images: Vec::new(),
                is_error: true,
            };
            let events = vec![
                (
                    root_agent.clone(),
                    SessionEvent::JobFinished {
                        job,
                        state: crate::job::JobEnd::Interrupted,
                        diagnostic: Some(crate::tool::ToolError::interrupted().diagnostic()),
                        images: Vec::new(),
                    },
                ),
                (
                    root_agent.clone(),
                    SessionEvent::MessageCommitted {
                        message: Message::Tool(result),
                    },
                ),
                (
                    root_agent.clone(),
                    SessionEvent::JobStateChanged {
                        job,
                        state: crate::job::JobTransition::Running,
                    },
                ),
            ];
            store.append_all(events).await.unwrap();
        }
        drop(store);

        // Steps serve requests in arrival order; the root's continuation and the
        // restarted child race, so every answer reads the same.
        let steps = [(); 3].map(|()| Step::new(answer("recovered")));
        let provider = Script::new(steps, &requests);
        let harness = test_harness(root.path(), &sessions, provider).await;
        let resumed = harness.resume_session(id).await.unwrap();
        resumed.running().await.unwrap();
        assert!(resumed.runtime.jobs.is_background(job).unwrap());
        // What the host never read, cut short or not, reports to the root as well.
        for unread in parent.into_iter().chain(parent.map(|_| read)) {
            assert!(resumed.runtime.jobs.is_background(unread).unwrap());
        }
        bounded(resumed.continue_turn()).await.unwrap();
        // The child's answer reaches the root as a job event, which the root answers.
        let owner = resumed.root.clone();
        let reports = move |events: &[crate::session::JobEvent]| {
            events.iter().any(|event| match event {
                crate::session::JobEvent::Message(message) => message.id == job,
                crate::session::JobEvent::Job(view) => view.id == Some(job),
            })
        };
        let answered = move |state: &mut CoreState| {
            let mut messages = (state.records.iter())
                .filter(|record| record.agent == owner)
                .filter_map(|record| match &record.event {
                    SessionEvent::MessageCommitted { message } => Some(message),
                    _ => None,
                });
            messages.position(|message| {
                let Message::User(parts) = message else {
                    return false;
                };
                (parts.iter())
                    .any(|part| matches!(part, UserPart::JobEvents { events } if reports(events)))
            })?;
            messages
                .any(|message| matches!(message, Message::Assistant(_)))
                .then_some(())
        };
        found(&resumed, answered).await;
        let delivered = requests.lock().unwrap().iter().any(|request| {
            let history = rendered(request);
            history.contains("job_events") && history.contains("recovered")
        });
        assert!(delivered);
        let records = resumed.runtime.store.records();
        assert_eq!(count!(&records, SessionEvent::AgentStarted { .. }), 2);
        shutdown_session(resumed).await;
    }

    #[tokio::test]
    async fn compaction_resume_restores_todos_and_the_first_provider_request() {
        let root = tempfile::tempdir().unwrap();
        let requests = Requests::default();
        let reconciled = vec![
            todo("Inspect queue", TodoStatus::Completed),
            todo("Verify queue fix", TodoStatus::Pending),
        ];
        let mut summary = summary_json();
        summary["todo_reconciliation"] = json!(["Inspection finished"]);
        summary["todos"] = json!(reconciled);
        let mut completed = answer("checkpoint installed");
        let usage = usage(48_000, 2_000, 3_914);
        completed.insert(completed.len() - 1, ResponseEvent::Usage(usage));
        let summary = answer(summary.to_string());
        let responses = [
            completed,
            summary,
            answer("checkpoint used"),
            answer("resumed"),
        ];
        let provider = scripted_provider(&requests, responses);
        let profile = ModelProfile {
            max_context: crate::tests::limit(64_000),
            ..crate::tests::profile("test", false)
        };
        let harness = serving(root.path(), provider, [("test", profile)])
            .build()
            .await
            .unwrap();
        let session = harness.new_session().await.unwrap();
        let todos = &session.runtime.store;
        let root_todos = vec![todo("Inspect queue", TodoStatus::InProgress)];
        todos
            .replace_todos(&session.root, root_todos, None)
            .await
            .unwrap();
        let child = start_child(&session, 1, None).await;
        let child_todos = vec![todo("Independent child work", TodoStatus::InProgress)];
        todos
            .replace_todos(&child, child_todos.clone(), None)
            .await
            .unwrap();
        // Exceed the retention tail; only the high-usage response triggers the checkpoint.
        let history = AssistantItem::text("history", 0, "research ".repeat(40_000));
        let history = Message::Assistant(vec![history]);
        session
            .runtime
            .commit(&session.root, history)
            .await
            .unwrap();
        let answer = session.prompt("Compact this investigation.").await.unwrap();
        assert_eq!(answer, "checkpoint installed");
        // The answer's compaction runs before the next turn's first request.
        let next = session.prompt("Keep going.").await.unwrap();
        assert_eq!(next, "checkpoint used");
        let records = session.runtime.store.records();
        let checkpoints =
            events!(records, SessionEvent::Compaction { checkpoint } => checkpoint.clone());
        let checkpoint = checkpoints.into_iter().next().unwrap();
        assert_eq!(checkpoint.todos, reconciled);
        let prior_request = {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 3);
            assert!(captured[0].response_schema.is_none());
            assert!(captured[1].response_schema.is_some());
            assert!(captured[1].messages().any(|message| matches!(message,
                Sent::Assistant(items) if items == &vec![AssistantItem::text("answer", 0, "checkpoint installed")])));
            captured[0].clone()
        };
        let id = session.id();
        shutdown_session(session).await;

        let resumed = harness.resume_session(id).await.unwrap();
        let todos = &resumed.runtime.store;
        let found = todos.inspect_todos(&resumed.root, None).unwrap();
        assert_eq!(found, reconciled);
        let found = todos.inspect_todos(&child, None).unwrap();
        assert_eq!(found, child_todos);
        assert_eq!(resumed.prompt("Continue.").await.unwrap(), "resumed");
        {
            let captured = requests.lock().unwrap();
            assert_eq!(captured.len(), 4, "exactly one resumed provider invocation");
            let first = &captured[3];
            let persisted = crate::provider::protocol::StateLifetime::Persisted;
            let opening = checkpoint.message.entry(persisted);
            assert_eq!(first.context.first(), Some(&opening));
            assert_eq!(first.system, prior_request.system);
            assert_eq!(first.tools, prior_request.tools);
            assert_eq!(first.model, prior_request.model);
            // do not leak the summary schema
            assert!(first.response_schema.is_none());
            let runtime = request_runtime_state(first);
            assert!(runtime.contains("Inspect queue") && runtime.contains("completed"));
            assert!(runtime.contains("Verify queue fix") && runtime.contains("pending"));
        }
        shutdown_session(resumed).await;
    }
}

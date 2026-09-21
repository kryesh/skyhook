use crate::session::{EventRecord, SessionEvent, SessionStore};

use super::{DeliveryState, JobEntry, JobError, JobManager, JobSpec, JobState};
use crate::provider::protocol::Message;
use crate::tool::ToolError;

pub(super) async fn restore(
    store: SessionStore,
    records: &[EventRecord],
) -> Result<JobManager, JobError> {
    let mut jobs = std::collections::HashMap::new();
    let mut maximum = 0_u64;
    let mut children = std::collections::HashMap::new();
    // Calls already answered: a live job launched by one has no waiter left.
    let mut answered = std::collections::HashSet::new();
    for record in records {
        if let SessionEvent::MessageCommitted {
            message: Message::Tool(results),
        } = &record.event
        {
            for result in results {
                answered.insert((record.agent.clone(), result.call_id.clone()));
            }
        }
        match &record.event {
            SessionEvent::JobCreated {
                origin,
                job,
                parent,
                tool,
                role,
                name,
                accepts_input,
                background,
                authorization_scope,
                location,
                output_schema,
                ..
            } => {
                maximum = maximum.max(job.get());
                let (entry, _receiver) = JobEntry::new(
                    JobSpec {
                        origin: origin.clone(),
                        agent: record.agent.clone(),
                        parent: *parent,
                        tool: tool.clone(),
                        role: *role,
                        name: name.clone(),
                        arguments: serde_json::Value::Null,
                        output_schema: output_schema.clone(),
                        accepts_input: *accepts_input,
                        background: *background,
                        authorization_scope: *authorization_scope,
                        location: location.clone(),
                    },
                    record.timestamp_millis,
                );
                jobs.insert(*job, entry);
            }
            SessionEvent::AgentStarted {
                owner_job: Some(job),
                parent,
                location,
                ..
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.location.clone_from(location);
                    if parent.as_ref() == Some(&entry.agent)
                        && record.agent.parent().as_ref() == Some(&entry.agent)
                        && entry
                            .child
                            .as_ref()
                            .is_none_or(|child| child == &record.agent)
                    {
                        entry.child = Some(record.agent.clone());
                        children.insert(record.agent.clone(), *job);
                    }
                }
            }
            SessionEvent::JobStateChanged { job, state } => {
                if let Some(entry) = jobs.get_mut(job) {
                    if matches!(
                        entry.state,
                        super::JobState::Completed
                            | super::JobState::Failed
                            | super::JobState::Interrupted
                    ) && *state == super::JobState::Running
                    {
                        entry.clear_invocation_output();
                        entry.pend_delivery();
                        // Match live retained reset: an interrupted foreground
                        // invocation still owns its original waiter.
                        if entry.state != super::JobState::Interrupted {
                            entry.background = true;
                        }
                    }
                    if *state == super::JobState::WaitingInput
                        || (entry.state == super::JobState::WaitingInput
                            && *state == super::JobState::Running)
                    {
                        entry.output = None;
                        entry.pend_delivery();
                        entry.background = true;
                    }
                    entry.state = *state;
                }
            }
            SessionEvent::MessageCommitted { message } => {
                if let Some(job) = children.get(&record.agent).copied()
                    && let Some(text) = super::messages::visible_text(message)
                {
                    let deliver = super::views::effectively_background(&jobs, job);
                    if let Some(entry) = jobs.get_mut(&job) {
                        entry.publish_message(job, record.sequence, text, deliver);
                    }
                }
            }
            SessionEvent::JobFinished {
                job,
                state,
                diagnostic,
                output_diagnostic,
                images,
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.apply_finished(
                        *state,
                        images.clone(),
                        diagnostic.clone(),
                        output_diagnostic.clone(),
                    );
                }
            }
            SessionEvent::JobClaimed { job } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.delivery = DeliveryState::Claimed;
                }
            }
            SessionEvent::JobInjected { job, .. } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.delivery = DeliveryState::Injected;
                }
            }
            SessionEvent::JobMessageDelivered { job, source, .. } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.messages.retain(|message| message.message != *source);
                }
            }
            _ => {}
        }
    }
    // A retained child whose call was answered goes on in the background.
    for entry in jobs.values_mut() {
        let released = !entry.state.is_terminal() || entry.state == JobState::Interrupted;
        if released
            && let Some(origin) = &entry.origin
            && answered.contains(&(entry.agent.clone(), origin.call_id.clone()))
        {
            entry.background = true;
        }
    }
    let active = jobs
        .iter()
        .filter_map(|(job, entry)| (!entry.state.is_terminal()).then_some(*job))
        .collect::<Vec<_>>();
    let manager = JobManager::with_jobs(store, jobs, maximum.saturating_add(1).max(1));
    manager.inner.progress.lock().await.project(records);
    for job in active {
        manager.finish(job, ToolError::Interrupted.into()).await?;
    }
    Ok(manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionLocation;
    use crate::identity::JobId;
    use crate::job::{JobOutcome, JobRole, JobState, presented_job_schema};
    use crate::{
        job::output,
        tool::{ToolOutput, policy::CapabilitySet},
    };

    /// An on-disk runtime, so `reopen` can replay its journal.
    async fn runtime() -> (tempfile::TempDir, JobManager, crate::identity::AgentId) {
        let (root, store, agent) = crate::session::fixture::on_disk().await;
        (root, JobManager::new(store), agent)
    }

    /// Drain owners, drop the manager, and replay the durable journal.
    async fn reopen(jobs: JobManager, root: &std::path::Path) -> JobManager {
        let session = jobs.store().id();
        jobs.drain_creations().await;
        jobs.drain_supervisors().await;
        drop(jobs);
        let (store, records) = SessionStore::open(root, session).await.unwrap();
        JobManager::restore(store, &records).await.unwrap()
    }

    #[tokio::test]
    async fn diagnostic_replay_filters_targets_and_only_renders_registered_error_slots() {
        use crate::tool::{
            diagnostic::{
                Cause, Diagnostic, DiagnosticContext, Effects, FailureSite, IoKind, Operation,
                PathRole, Subject,
            },
            policy::Capability,
        };
        use serde_json::json;

        let (root, jobs, agent) = runtime().await;
        let location = ExecutionLocation::named("private-build-target", "/srv/project".into());
        let context = DiagnosticContext::new(Operation::Read, Subject::path("missing.txt"))
            .at(FailureSite::Execution(location))
            .effects(Effects::NotStarted)
            .path(PathRole::Requested, "./missing.txt")
            .path(PathRole::Resolved, "/srv/project/missing.txt");
        let diagnostic = Diagnostic::new(
            context.clone(),
            Cause::Io {
                kind: IoKind::NotFound,
                code: Some(2),
                detail: None,
            },
        );
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
        let error = ToolError::Io(std::io::Error::from_raw_os_error(2))
            .context(context)
            .with_result(ToolOutput::new(payload()));
        jobs.finish(failed, error.into()).await.unwrap();
        let read = jobs
            .test_create(JobSpec::test(agent.clone(), "fixture"))
            .await;
        jobs.finish(
            read,
            JobOutcome::Completed(ToolOutput::new(payload()).with_diagnostic(diagnostic.clone())),
        )
        .await
        .unwrap();
        let opaque = jobs.test_create(JobSpec::test(agent, "read")).await;
        jobs.finish(opaque, JobOutcome::Completed(ToolOutput::new(payload())))
            .await
            .unwrap();

        // Durable facts are authoritative; the saved JSON retains only slots,
        // never a rendering selected by an earlier reader's capabilities.
        assert!(jobs.output(failed).test_document().unwrap()["error"].is_null());
        assert!(jobs.output(read).test_document().unwrap()["result"]["error"]["message"].is_null());
        assert_eq!(
            jobs.output(opaque).test_document().unwrap()["result"]["error"]["message"],
            "producer placeholder"
        );

        let privileged: CapabilitySet = [Capability::Targets].into_iter().collect();
        let restricted = CapabilitySet::default();
        // Alternating readers must never observe another capability's cached rendering.
        let views = async |jobs: &JobManager| {
            let mut views = Vec::new();
            for caps in [&privileged, &restricted, &privileged] {
                for (job, field) in [
                    (failed, None),
                    (read, None),
                    (opaque, None),
                    (failed, Some("/error")),
                    (read, Some("/result/error/message")),
                    (failed, Some("")),
                    (read, Some("/result")),
                ] {
                    let mut query = output::OutputArgs::new(job);
                    query.field = field.map(str::to_owned);
                    let view = jobs.inspect_output(query, caps).await.unwrap();
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
        assert_eq!(failed_view["has_result"], true);
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

    #[tokio::test]
    async fn replay_recovers_creation_committed_before_map_publication() {
        let (root, store, agent) = crate::session::fixture::on_disk().await;
        let job = JobId::new(7).unwrap();
        let created = SessionEvent::JobCreated {
            origin: None,
            job,
            parent: None,
            tool: "committed-without-map".into(),
            role: JobRole::Tool,
            name: None,
            arguments: serde_json::json!({}),
            output_schema: None,
            accepts_input: false,
            background: false,
            authorization_scope: None,
            location: ExecutionLocation::root(".".into()),
        };
        let accepted = store.accept_append(agent.clone(), created).await.unwrap();
        accepted.committed().await.unwrap();
        // Simulate the persisted prefix at a crash between commit and insertion:
        // no JobManager has ever published this creation into its map.
        let jobs = reopen(JobManager::new(store), root.path()).await;
        let interrupted = jobs.metadata(job).await.unwrap();
        assert_eq!(interrupted.state, JobState::Interrupted);
        assert!(
            interrupted
                .error
                .as_deref()
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
        let job = jobs.test_create(JobSpec::test(agent, "shell")).await;
        jobs.transition(job, JobState::AwaitingApproval)
            .await
            .unwrap();
        let pending = jobs.snapshot(job).await.unwrap();
        assert_eq!(
            pending.metadata_view(&capabilities).into_value()["state"],
            "queued"
        );
        let schema = presented_job_schema(false).to_string();
        assert!(!schema.contains("awaiting_approval"));
        jobs.finish(job, ToolError::Denied("user reason".to_owned()).into())
            .await
            .unwrap();
        let denied = jobs
            .wait(job, None, true)
            .await
            .unwrap()
            .metadata_view(&capabilities)
            .into_value();
        assert_eq!(denied["meta"]["code"], "permission_denied");
        assert_eq!(denied["meta"]["executed"], false);
        assert!(denied["error"].as_str().unwrap().contains("user reason"));
        // The persisted terminal event is the source of truth for replay.
        let restored = reopen(jobs, root.path()).await;
        let replayed = restored
            .snapshot(job)
            .await
            .unwrap()
            .metadata_view(&capabilities)
            .into_value();
        assert_eq!(replayed, denied);
    }

    #[tokio::test]
    async fn restore_interrupts_active_jobs_and_advances_ids() {
        let (root, manager, agent) = runtime().await;
        let named = ExecutionLocation::named("build", "/srv/project".into());
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
                ..JobSpec::test(agent.clone(), "shell")
            };
            let lease = manager.test_lease(spec).await;
            manager
                .transition(lease.id(), JobState::Running)
                .await
                .unwrap();
            manager
                .output(lease.id())
                .test_capture(field, kind, bytes.as_bytes());
            leases.push(lease);
        }
        drop(leases);
        let restored = reopen(manager, root.path()).await;
        for (job, field, kind, location) in [
            (
                1,
                "/result/stdout",
                "text",
                ExecutionLocation::root(".".into()),
            ),
            (2, "/result/custom", "json", named),
        ] {
            let id = JobId::new(job).unwrap();
            let args = output::OutputArgs::new(id);
            let view = restored
                .present_output(args, &CapabilitySet::default())
                .await
                .unwrap();
            assert!(view["result"].is_null());
            assert!(!view["has_result"].as_bool().unwrap());
            let capture =
                serde_json::json!({"field":field,"kind":kind,"complete":false, "output":null});
            assert_eq!(view["presentation"]["captures"][0], capture);
            let snapshot = restored.snapshot(id).await.unwrap();
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

    async fn stored_projection(jobs: &JobManager, id: JobId) -> serde_json::Value {
        let entries = jobs.inner.jobs.lock().await;
        let entry = entries.get(&id).unwrap();
        serde_json::json!({
            "state": entry.state, "output": entry.output,
            "images": entry.images, "error": entry.diagnostic.as_ref().map(|diagnostic| diagnostic.render(&CapabilitySet::default())), "denied": entry.diagnostic.as_ref().is_some_and(|diagnostic| diagnostic.is_denial()),
            "pending": entry.delivery == DeliveryState::Pending,
            "resumable": entry.resume.is_some(),
        })
    }

    /// Case 7 is a volatile failure: persistence is unavailable, so the live
    /// entry clears partial output and is not replayed.
    #[tokio::test]
    async fn outcome_application_live_replay_and_interrupted_cancellation_matrix() {
        for case in 0..8 {
            let (root, jobs, agent) = runtime().await;
            jobs.store().store_blob(OUTCOME_IMAGE).await.unwrap();
            let spec = JobSpec {
                accepts_input: true,
                ..JobSpec::test(agent, "outcome")
            };
            let id = jobs.test_create(spec).await;
            let handler: super::super::ResumeHandler = std::sync::Arc::new(|_, _| {
                Box::pin(async { Ok(ToolOutput::new(serde_json::Value::Null)) })
            });
            jobs.set_resume_handler(id, handler).await.unwrap();
            jobs.transition(id, JobState::Running).await.unwrap();
            let question = serde_json::json!({"question":"partial"});
            jobs.request_input(id, question).await.unwrap();
            let result = || {
                ToolOutput::new(serde_json::json!({"result":"saved"}))
                    .with_images(vec![outcome_image()])
            };
            let outcome = match case {
                0 => JobOutcome::Completed(result()),
                1 => ToolError::Failed("failed".into()).into(),
                2 => ToolError::Failed("partial failure".into())
                    .with_result(result())
                    .into(),
                3 => ToolError::Denied("denied".into()).into(),
                4 => ToolError::Cancelled.with_result(result()).into(),
                5 | 6 => ToolError::Interrupted.into(),
                _ => {
                    let mut entries = jobs.inner.jobs.lock().await;
                    let entry = entries.get_mut(&id).unwrap();
                    entry.images = vec![outcome_image()];
                    drop(entries);
                    jobs.fail_volatile(id, "cannot persist".into()).await;
                    let projection = stored_projection(&jobs, id).await;
                    assert_eq!(projection["state"], "failed");
                    assert_eq!(projection["images"], serde_json::json!([]));
                    assert!(
                        projection["error"]
                            .as_str()
                            .unwrap()
                            .contains("cannot persist")
                    );
                    assert!(projection["output"].is_null());
                    continue;
                }
            };
            jobs.finish(id, outcome).await.unwrap();
            if case == 6 {
                jobs.finish(id, ToolError::Cancelled.into()).await.unwrap();
            }
            let projection = stored_projection(&jobs, id).await;
            assert!(projection["output"].is_null());
            assert_eq!(projection["resumable"], !matches!(case, 4 | 6));
            // Projection application must not replace the saved payload by an
            // in-memory question or materialize it into the stored entry.
            if matches!(case, 0 | 2 | 4) {
                let document = jobs.output(id).test_document().unwrap();
                assert_eq!(document["result"], serde_json::json!({"result":"saved"}));
            }
            let mut projection = projection;
            let replay = reopen(jobs, root.path()).await;
            let mut replayed = stored_projection(&replay, id).await;
            // Resume handlers are live-only and never replayed.
            projection["resumable"] = false.into();
            replayed["resumable"] = false.into();
            assert_eq!(projection, replayed);
        }
    }
}

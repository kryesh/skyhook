use crate::session::{EventRecord, SessionEvent, SessionStore};

use super::{DeliveryState, JobEntry, JobError, JobManager, JobOutcome, JobSpec};

pub(super) async fn restore(
    store: SessionStore,
    records: &[EventRecord],
) -> Result<JobManager, JobError> {
    let mut jobs = std::collections::HashMap::new();
    let mut maximum = 0_u64;
    let mut children = std::collections::HashMap::new();
    for record in records {
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
                        entry.delivery = DeliveryState::Pending;
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
                        entry.delivery = DeliveryState::Pending;
                        entry.background = true;
                    }
                    entry.state = *state;
                }
            }
            SessionEvent::MessageCommitted { message } => {
                if let Some(job) = children.get(&record.agent)
                    && let Some(entry) = jobs.get_mut(job)
                    && let Some(text) = super::messages::visible_text(message)
                {
                    entry.publish_message(*job, record.sequence, text);
                }
            }
            SessionEvent::JobFinished {
                job,
                state,
                error,
                images,
                denial,
            } => {
                if let Some(entry) = jobs.get_mut(job) {
                    entry.apply_finished(*state, images.clone(), error.clone(), denial.clone());
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
    let active = jobs
        .iter()
        .filter_map(|(job, entry)| (!entry.state.is_terminal()).then_some(*job))
        .collect::<Vec<_>>();
    let manager = JobManager::with_jobs(store, jobs, maximum.saturating_add(1).max(1));
    manager.inner.progress.lock().await.project(records);
    for job in active {
        manager.finish(job, JobOutcome::Interrupted).await?;
    }
    Ok(manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionLocation;
    use crate::identity::JobId;
    use crate::job::{JobRole, JobState, presented_job_schema};
    use crate::{
        job::output,
        tool::{ToolError, ToolOutput, policy::CapabilitySet},
    };

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
        assert_eq!(
            jobs.metadata(job).await.unwrap().state,
            JobState::Interrupted
        );
        assert_eq!(
            jobs.test_create(JobSpec::test(agent, "next")).await.get(),
            8
        );
    }

    #[tokio::test]
    async fn denial_survives_replay_and_agent_views_hide_pending_authorization() {
        let (root, jobs, agent) = super::super::tests::runtime().await;
        let capabilities = CapabilitySet::default();
        let job = jobs.test_create(JobSpec::test(agent, "shell")).await;
        jobs.transition(job, JobState::AwaitingApproval)
            .await
            .unwrap();
        let pending = jobs.snapshot(job).await.unwrap();
        assert_eq!(pending.presented(&capabilities).unwrap()["state"], "queued");
        let schema = presented_job_schema(&capabilities, false).to_string();
        assert!(!schema.contains("awaiting_approval"));
        jobs.finish(job, ToolError::Denied("user reason".to_owned()).into())
            .await
            .unwrap();
        let denied = jobs
            .wait(job, None, true)
            .await
            .unwrap()
            .presented(&capabilities)
            .unwrap();
        assert_eq!(
            (&denied["code"], &denied["executed"], &denied["error"]),
            (
                &"permission_denied".into(),
                &false.into(),
                &"user reason".into()
            )
        );
        // The persisted terminal event is the source of truth for replay.
        let restored = reopen(jobs, &root.path().join("sessions")).await;
        let replayed = restored
            .snapshot(job)
            .await
            .unwrap()
            .presented(&capabilities)
            .unwrap();
        assert_eq!(replayed, denied);
    }

    #[tokio::test]
    async fn restore_interrupts_active_jobs_and_advances_ids() {
        let (root, manager, agent) = super::super::tests::runtime().await;
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
        let restored = reopen(manager, &root.path().join("sessions")).await;
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
            assert!(view.get("result").is_none());
            let capture = serde_json::json!({"field":field,"kind":kind,"complete":false});
            assert_eq!(view["captures"][0], capture);
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
            "images": entry.images, "error": entry.error, "denial": entry.denial,
            "pending": entry.delivery == DeliveryState::Pending,
            "resumable": entry.resume.is_some(),
        })
    }

    /// Case 7 is a volatile failure: persistence is unavailable, so the live
    /// entry keeps its denial, clears partial output and is not replayed.
    #[tokio::test]
    async fn outcome_application_live_replay_and_interrupted_cancellation_matrix() {
        for case in 0..8 {
            let (root, jobs, agent) = super::super::tests::runtime().await;
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
            let failed = |message: &str, output| JobOutcome::Failed {
                message: message.into(),
                output,
                denial: None,
            };
            let outcome = match case {
                0 => JobOutcome::Completed(result()),
                1 => failed("failed", None),
                2 => failed("partial failure", Some(result())),
                3 => ToolError::Denied("denied".into()).into(),
                4 => JobOutcome::Cancelled,
                5 | 6 => JobOutcome::Interrupted,
                _ => {
                    let mut entries = jobs.inner.jobs.lock().await;
                    let entry = entries.get_mut(&id).unwrap();
                    entry.images = vec![outcome_image()];
                    entry.denial = Some(crate::tool::Denial::permission_denied());
                    drop(entries);
                    jobs.fail_volatile(id, "cannot persist".into()).await;
                    let projection = stored_projection(&jobs, id).await;
                    assert_eq!(
                        (
                            &projection["state"],
                            &projection["images"],
                            &projection["error"]
                        ),
                        (
                            &"failed".into(),
                            &serde_json::json!([]),
                            &"cannot persist".into()
                        )
                    );
                    assert!(projection["output"].is_null() && !projection["denial"].is_null());
                    continue;
                }
            };
            jobs.finish(id, outcome).await.unwrap();
            if case == 6 {
                jobs.finish(id, JobOutcome::Cancelled).await.unwrap();
            }
            let projection = stored_projection(&jobs, id).await;
            assert!(projection["output"].is_null());
            assert_eq!(projection["resumable"], !matches!(case, 4 | 6));
            // Projection application must not replace the saved payload by an
            // in-memory question or materialize it into the stored entry.
            if matches!(case, 0 | 2) {
                let document = jobs.output(id).test_document().unwrap();
                assert_eq!(document["result"], serde_json::json!({"result":"saved"}));
            }
            let mut projection = projection;
            let replay = reopen(jobs, &root.path().join("sessions")).await;
            let mut replayed = stored_projection(&replay, id).await;
            // Resume handlers are live-only and never replayed.
            projection["resumable"] = false.into();
            replayed["resumable"] = false.into();
            assert_eq!(projection, replayed);
        }
    }
}

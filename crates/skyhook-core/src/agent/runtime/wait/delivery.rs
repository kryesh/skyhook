//! Snapshot and present durable child messages and job notifications.

use super::{PendingEventBatch, SessionRuntime};
use crate::provider::protocol::UserContent;

impl SessionRuntime {
    pub(in crate::agent::runtime) async fn pending_event_content(
        &self,
        agent: &crate::identity::AgentId,
        capabilities: &crate::tool::policy::CapabilitySet,
        location: &crate::execution::ExecutionLocation,
    ) -> Result<(Vec<UserContent>, PendingEventBatch), crate::agent::runtime::HarnessError> {
        let pending = self.jobs.pending_delivery(agent).await?;
        let content = self
            .job_event_content(&pending, capabilities, location)
            .await;
        // Never hold an empty receipt's delivery gate across a model request.
        let jobs = (!content.is_empty()).then_some(pending);
        Ok((content, PendingEventBatch { jobs }))
    }

    async fn job_event_content(
        &self,
        pending: &crate::job::PendingDelivery,
        capabilities: &crate::tool::policy::CapabilitySet,
        location: &crate::execution::ExecutionLocation,
    ) -> Vec<UserContent> {
        let mut presented = Vec::new();
        for message in pending.messages() {
            let mut event = serde_json::to_value(message).expect("child messages serialize");
            event["kind"] = serde_json::json!("message");
            presented.push(event);
        }
        for job in pending.envelopes() {
            // The independently delivered last reply is the completion payload.
            // Present only metadata here: inspecting the saved output would also
            // reintroduce large replies through preview/truncation fields.
            if job.tool == "agent"
                && job.state == crate::job::JobState::Completed
                && let Ok(Some(sequence)) = self.jobs.last_agent_message(job.id).await
            {
                let mut metadata = job.clone();
                metadata.output = None;
                let mut view = metadata
                    .presented_for(capabilities, Some(location), true)
                    .expect("job metadata serializes");
                view["last_message"] = serde_json::json!(sequence);
                presented.push(view);
                continue;
            }
            match self
                .jobs
                .inspect_output_for(
                    crate::job::output::OutputArgs::new(job.id),
                    capabilities,
                    location,
                )
                .await
            {
                Ok(view) => presented.push(view),
                Err(error) => presented.push(
                    serde_json::json!({"id":job.id,"state":job.state,"error":error.to_string()}),
                ),
            }
        }
        if presented.is_empty() {
            return Vec::new();
        }
        vec![UserContent::Runtime {
            text: format!(
                "<skyhook_job_events>\n{}\n</skyhook_job_events>",
                serde_json::to_string(&presented).unwrap_or_else(|_| "[]".to_owned())
            ),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::super::receipt::tests::{fixture_child, fixture_message};
    use super::super::tests::*;
    use crate::agent::runtime::*;
    use crate::job::JobState;
    #[tokio::test]
    async fn no_tool_child_reports_survive_queued_input_and_wake_parent_independently() {
        no_tool_child_reports(true, false).await;
    }

    #[tokio::test]
    async fn pending_no_tool_child_reports_survive_queued_input_at_request_boundary() {
        no_tool_child_reports(false, false).await;
    }

    #[tokio::test]
    async fn no_tool_child_reports_survive_pending_runtime_event() {
        no_tool_child_reports(false, true).await;
    }

    async fn no_tool_child_reports(parent_already_waiting: bool, pending_runtime_event: bool) {
        const A: &str = "child-report-A";
        const B: &str = "child-addendum-B";
        let workspace = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(vec![
            (
                "root",
                call(
                    "launch",
                    "agent",
                    json!({
                        "prompt":"report", "model":"child", "name":"reporter", "bg":true
                    }),
                ),
            ),
            ("child", AssistantContent::text("report", 0, A)),
            ("root", call("first-report", "wait", json!({}))),
            ("child", AssistantContent::text("addendum", 0, B)),
            ("root", call("no-repeat", "wait", json!({"timeout":1}))),
            ("root", call("last-report", "wait", json!({}))),
            ("root", answer()),
        ]);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        tracking.release(0);
        tracking.request(1).await;
        tracking.request(2).await;
        let job = running_job(&session, "agent").await;
        let (child, sender) = {
            let agents = runtime.agents.read().unwrap();
            let (child, slot) = agents.iter().find(|(id, _)| **id != session.root).unwrap();
            (child.clone(), slot.sender.clone())
        };
        if pending_runtime_event {
            complete_background_for(&session, &child, "please add an addendum").await;
            assert!(runtime.jobs.has_pending(&child).await);
        } else {
            let sent = bounded(session.run_script(format!(
                "return await tool.job({}).send({{value:'please add an addendum'}});",
                job.get()
            )))
            .await
            .unwrap();
            assert_eq!(sent.value["value"], json!({"accepted":true}));
            // send() acknowledges the job mailbox; occupancy proves forwarding to the
            // child's command queue while A's no-tool provider invoke is still gated.
            bounded(async {
                while sender.capacity() != AGENT_CHANNEL_CAPACITY - 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }
        if parent_already_waiting {
            tracking.release(2);
            running_job(&session, "wait").await;
        }
        tracking.release(1);
        let addendum = tracking.request(3).await;
        assert_eq!(
            serde_json::to_string(&addendum.messages)
                .unwrap()
                .matches("please add an addendum")
                .count(),
            1
        );
        assert!(addendum.messages.iter().any(|message| matches!(message,
            Message::Assistant(content) if content == &tracking.steps[1].content)));
        if !parent_already_waiting {
            assert!(
                tracking
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(step, _)| *step < 4)
            );
            tracking.release(2);
        }
        let first = tracking.request(4).await;
        assert_reason(&first, "first-report", "event");
        let messages = agent_messages(&first);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"], A);
        assert!(
            events(&first).is_empty(),
            "A must not complete the still-working child"
        );
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().state,
            JobState::Running
        );
        assert!(
            !turn.is_finished(),
            "parent must not finish before addendum work"
        );
        tracking.release(4);
        let waiting = tracking.request(5).await;
        assert_reason(&waiting, "no-repeat", "timeout");
        assert_eq!(agent_messages(&waiting), messages);
        assert!(events(&waiting).is_empty());

        // Hold this parent boundary until B and completion are both durable. They
        // remain separate events, but need not wake two separate model requests.
        tracking.release(3);
        child_completed(&session, job).await;
        tracking.release(5);
        let completed = tracking.request(6).await;
        assert_reason(&completed, "last-report", "event");
        let delivered = agent_messages(&completed);
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0], messages[0]);
        assert_eq!(delivered[1]["text"], B);
        let lifecycle = events(&completed);
        assert_eq!(lifecycle.len(), 1);
        assert_child_completion(&lifecycle[0], &delivered[1]);
        let history = serde_json::to_string(&completed.messages).unwrap();
        assert_eq!(
            history.matches(A).count(),
            1,
            "A must not be overwritten or repeated"
        );
        assert_eq!(
            history.matches(B).count(),
            1,
            "completion must not repeat B"
        );
        assert_eq!(
            runtime.jobs.snapshot(job).await.unwrap().output,
            Some(json!(B))
        );
        tracking.release(6);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn intermediate_child_reply_wakes_parent_and_is_injected_once() {
        intermediate_child_reply(true).await;
    }

    #[tokio::test]
    async fn intermediate_child_reply_before_wait_is_delivered_at_request_boundary() {
        intermediate_child_reply(false).await;
    }

    async fn intermediate_child_reply(parent_already_waiting: bool) {
        const REPLY: &str = "intermediate-child-reply-marker";
        const FINAL: &str = "final-child-output-marker";
        let workspace = tempfile::tempdir().unwrap();
        let mut child_tool = call("continue-child", "script", json!({"source":"return 42;"}));
        child_tool.position = 1;
        let reply = vec![AssistantContent::text("child-reply", 0, REPLY), child_tool];
        let tracking = Tracking::responses(vec![
            (
                "root",
                vec![call(
                    "child",
                    "agent",
                    json!({"prompt":"child task", "model":"child", "name":"child-replier", "bg":true}),
                )],
            ),
            ("child", vec![call("child-wait", "wait", json!({}))]),
            ("root", vec![call("waiting", "wait", json!({}))]),
            ("child", reply.clone()),
            (
                "child",
                vec![AssistantContent::text("child-final", 0, FINAL)],
            ),
            ("root", vec![call("again", "wait", json!({"timeout":1}))]),
            ("root", vec![call("finish-wait", "wait", json!({}))]),
            (
                "root",
                vec![call("after-final", "wait", json!({"timeout":1}))],
            ),
            ("root", vec![answer()]),
        ]);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        tracking.release(0);
        tracking.request(1).await;
        let in_flight = tracking.request(2).await;
        assert!(agent_messages(&in_flight).is_empty());
        let child_job = running_job(&session, "agent").await;
        let child = runtime
            .agents
            .read()
            .unwrap()
            .keys()
            .find(|agent| **agent != session.root)
            .unwrap()
            .clone();
        tracking.release(1);
        bounded(async {
            loop {
                if runtime
                    .jobs
                    .list(&child)
                    .await
                    .iter()
                    .any(|job| job.tool == "wait" && job.state == JobState::Running)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if parent_already_waiting {
            tracking.release(2);
            running_job(&session, "wait").await;
        }

        // Send through the public job API, not directly into the agent's queue.
        // The child's next request proves that the input actually reached it.
        let sent = bounded(session.run_script(format!(
            "return await tool.job({}).send({{value:\"parent-reply-request-marker\"}});",
            child_job.get()
        )))
        .await
        .unwrap();
        assert_eq!(sent.value["value"], json!({"accepted":true}));
        let child_request = tracking.request(3).await;
        assert_reason(&child_request, "child-wait", "event");
        assert_eq!(
            serde_json::to_string(&child_request.messages)
                .unwrap()
                .matches("parent-reply-request-marker")
                .count(),
            1
        );
        tracking.release(3);
        // The child has committed text WITH a tool call and reached another invoke.
        // Keep that final response gated until after the parent has read the reply.
        tracking.request(4).await;
        assert_eq!(
            runtime.jobs.snapshot(child_job).await.unwrap().state,
            JobState::Running
        );
        if !parent_already_waiting {
            // No new parent request may start while its current invoke is gated.
            assert!(
                tracking
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(step, _)| *step < 5)
            );
            tracking.release(2);
        }
        let next = tracking.request(5).await;
        assert_reason(&next, "waiting", "event");
        assert_eq!(
            runtime.jobs.snapshot(child_job).await.unwrap().state,
            JobState::Running,
            "the reply must not depend on child completion"
        );
        assert!(events(&next).is_empty(), "a reply is not a job completion");
        let records = runtime.store.records().await;
        let committed = records
            .iter()
            .filter(|record| {
                record.agent == child
                    && matches!(&record.event,
                        SessionEvent::MessageCommitted { message: Message::Assistant(content) }
                            if content == &reply)
            })
            .collect::<Vec<_>>();
        assert_eq!(committed.len(), 1);
        let replies = agent_messages(&next);
        assert_eq!(
            replies,
            vec![json!({
                "kind":"message",
                "id":child_job.get(),
                "name":"child-replier",
                "message":committed[0].sequence,
                "text":REPLY,
            })]
        );
        assert_eq!(
            serde_json::to_string(&next.messages)
                .unwrap()
                .matches(REPLY)
                .count(),
            1,
            "only the runtime envelope should contain the reply"
        );

        tracking.release(5);
        let again = tracking.request(6).await;
        assert_reason(&again, "again", "timeout");
        assert_eq!(
            agent_messages(&again),
            replies,
            "do not inject another copy"
        );
        assert!(events(&again).is_empty());
        tracking.release(4);
        child_completed(&session, child_job).await;
        tracking.release(6);
        let completed = tracking.request(7).await;
        assert_reason(&completed, "finish-wait", "event");
        let all_replies = agent_messages(&completed);
        assert_eq!(&all_replies[..replies.len()], replies.as_slice());
        assert_eq!(all_replies.len(), 2);
        assert_eq!(all_replies[1]["text"], FINAL);
        let notifications = events(&completed);
        assert_eq!(notifications.len(), 1);
        assert_child_completion(&notifications[0], &all_replies[1]);
        tracking.release(7);
        let last = tracking.request(8).await;
        assert_reason(&last, "after-final", "timeout");
        assert_eq!(agent_messages(&last), all_replies);
        assert_eq!(events(&last), notifications);
        tracking.release(8);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn intermediate_child_replies_outlive_a_full_parent_mailbox() {
        child_replies_at_no_tool_boundary(AGENT_CHANNEL_CAPACITY + 1, true).await;
    }

    async fn child_replies_at_no_tool_boundary(reply_count: usize, fill_mailbox: bool) {
        let workspace = tempfile::tempdir().unwrap();
        let mut steps = vec![
            (
                "root",
                vec![call(
                    "child",
                    "agent",
                    json!({"prompt":"child task", "model":"child", "bg":true}),
                )],
            ),
            // Unlike wait, this response has no tool call and can end the parent turn.
            ("root", vec![answer()]),
        ];
        for index in 0..reply_count {
            let mut tool = call(
                &format!("child-tool-{index}"),
                "script",
                json!({"source":"return 42;"}),
            );
            tool.position = 1;
            steps.push((
                "child",
                vec![
                    AssistantContent::text(
                        format!("child-reply-{index}"),
                        0,
                        format!("mailbox-child-reply-{index}"),
                    ),
                    tool,
                ],
            ));
        }
        let child_final = steps.len();
        steps.push((
            "child",
            vec![AssistantContent::text(
                "child-final",
                0,
                "mailbox-child-final",
            )],
        ));
        let parent_reply = steps.len();
        steps.push(("root", vec![call("finish-wait", "wait", json!({}))]));
        let parent_completed = steps.len();
        steps.push(("root", vec![answer()]));
        let tracking = Tracking::responses(steps);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        tracking.release(0);
        tracking.request(1).await;
        let child_job = running_job(&session, "agent").await;
        if fill_mailbox {
            // Fill the command channel while the parent is inside invoke. Durable
            // message events must remain pending even when JobsReady cannot fit.
            while session.root_tx.capacity() > 0 {
                bounded(session.root_tx.send(AgentCommand::JobsReady))
                    .await
                    .unwrap();
            }
            assert_eq!(session.root_tx.capacity(), 0);
        }
        for step in 2..child_final {
            tracking.request(step).await;
            tracking.release(step);
        }
        // More replies than the entire channel capacity have been published without
        // allowing the parent to drain anything. The child must not deadlock here.
        tracking.request(child_final).await;
        assert_eq!(
            runtime.jobs.snapshot(child_job).await.unwrap().state,
            JobState::Running
        );
        assert!(
            tracking
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(step, _)| *step < parent_reply)
        );
        if fill_mailbox {
            assert_eq!(session.root_tx.capacity(), 0);
        }
        tracking.release(1);
        let next = tracking.request(parent_reply).await;
        assert_eq!(
            runtime.jobs.snapshot(child_job).await.unwrap().state,
            JobState::Running,
            "a no-tool parent response must not strand replies until child completion"
        );
        assert!(events(&next).is_empty());
        let replies = agent_messages(&next);
        assert_eq!(
            replies.len(),
            reply_count,
            "a full mailbox must not drop payloads"
        );
        for (index, reply) in replies.iter().enumerate() {
            assert_eq!(reply["text"], format!("mailbox-child-reply-{index}"));
        }
        tracking.release(child_final);
        child_completed(&session, child_job).await;
        tracking.release(parent_reply);
        let completed = tracking.request(parent_completed).await;
        assert_reason(&completed, "finish-wait", "event");
        let all_replies = agent_messages(&completed);
        assert_eq!(
            &all_replies[..reply_count],
            replies.as_slice(),
            "retained replies are exactly once"
        );
        assert_eq!(all_replies.len(), reply_count + 1);
        assert_eq!(all_replies[reply_count]["text"], "mailbox-child-final");
        let notifications = events(&completed);
        assert_eq!(notifications.len(), 1);
        assert_child_completion(&notifications[0], &all_replies[reply_count]);
        tracking.release(parent_completed);
        bounded(turn).await.unwrap().unwrap();
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
    }

    #[tokio::test]
    async fn progress_and_completion_cross_the_same_no_tool_boundary_once() {
        let workspace = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let harness = harness(workspace.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        complete_background(&session, "no-tool-completion").await;
        let (job, child) = fixture_child(&session, workspace.path()).await;
        fixture_message(&session, job, &child, "no-tool-progress").await;
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(agent_messages(&next).len(), 1);
        assert_eq!(events(&next).len(), 1);
        // Both entries must be in a single persisted parent runtime envelope, not
        // separate history/ack transactions or a follow-on idle turn.
        let records = runtime.store.records().await;
        assert!(records.iter().any(|record| {
            record.agent == session.root && matches!(&record.event,
                SessionEvent::MessageCommitted { message: Message::User(content) }
                    if content.len() == 1 && content.iter().all(|block| matches!(block, UserContent::Runtime { .. })))
        }));
        assert!(
            !turn.is_finished(),
            "the earlier answer must not finish the caller's turn"
        );
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}

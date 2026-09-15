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
        let batch = PendingEventBatch {
            jobs,
            store: self.store.clone(),
            agent: agent.clone(),
            message: crate::provider::protocol::Message::User(content.clone()),
        };
        Ok((content, batch))
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
            match self
                .jobs
                .present_output_with(
                    crate::job::output::OutputArgs::new(job.id),
                    capabilities,
                    crate::job::output::OutputOptions::Host {
                        viewer: Some(location),
                        presentation: crate::job::OutputPresentation::Automatic,
                    },
                )
                .await
                .map(crate::job::PresentedOutput::into_view)
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
    use crate::agent::runtime::tests::count;
    use crate::agent::runtime::*;
    use crate::job::JobState;

    #[tokio::test]
    async fn no_tool_child_reports_survive_queued_input_and_pending_runtime_events() {
        // (parent already waiting, input is a pending runtime event rather than a send)
        for (waiting, runtime_event) in [(true, false), (false, false), (false, true)] {
            no_tool_child_reports(waiting, runtime_event).await;
        }
    }

    async fn no_tool_child_reports(parent_already_waiting: bool, pending_runtime_event: bool) {
        const A: &str = "child-report-A";
        const B: &str = "child-addendum-B";
        let launch = json!({"prompt":"report", "model":"child", "name":"reporter", "bg":true});
        let tracking = Tracking::new(vec![
            ("root", call("launch", "agent", launch)),
            ("child", AssistantContent::text("report", 0, A)),
            ("root", call("first-report", "wait", json!({}))),
            ("child", AssistantContent::text("addendum", 0, B)),
            ("root", call("no-repeat", "wait", json!({"timeout":1}))),
            ("root", call("last-report", "wait", json!({}))),
            ("root", answer()),
        ]);
        let (_root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        tracking.request(2).await;
        let job = running_job(&session, &session.root, "agent").await;
        let (child, sender) = only_child(&session);
        if pending_runtime_event {
            complete_background(&session, &child, "please add an addendum").await;
            assert!(runtime.jobs.has_pending(&child).await);
        } else {
            let send =
                format!("return await tool.job({job}).send({{value:'please add an addendum'}});");
            let sent = bounded(session.run_script(send)).await.unwrap();
            assert_eq!(sent.value["value"], json!({"accepted":true}));
            // Occupancy proves forwarding to the child's queue while A's invoke is gated.
            bounded(async {
                while sender.capacity() != AGENT_CHANNEL_CAPACITY - 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }
        if parent_already_waiting {
            tracking.release(2);
            running_job(&session, &session.root, "wait").await;
        }
        tracking.release(1);
        let addendum = tracking.request(3).await;
        let messages = serde_json::to_string(&addendum.messages().collect::<Vec<_>>()).unwrap();
        assert_eq!(messages.matches("please add an addendum").count(), 1);
        assert!(addendum.messages().any(|message| matches!(message,
            Message::Assistant(content) if content == &tracking.steps[1].content)));
        if !parent_already_waiting {
            assert!(!tracking.requested_from(4));
            tracking.release(2);
        }
        let first = tracking.request(4).await;
        assert_reason(&first, "first-report", "event");
        let messages = agent_messages(&first);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"], A);
        // A must not complete the still-working child
        assert!(events(&first).is_empty());
        let state = runtime.jobs.snapshot(job).await.unwrap().state;
        assert_eq!(state, JobState::Running);
        // parent must not finish before addendum work
        assert!(!turn.is_finished());
        tracking.release(4);
        let waiting = tracking.request(5).await;
        assert_reason(&waiting, "no-repeat", "timeout");
        assert_eq!(agent_messages(&waiting), messages);
        assert!(events(&waiting).is_empty());

        // Hold the parent until B and completion are durable; they may share one request.
        tracking.release(3);
        child_completed(&session, job).await;
        tracking.release(5);
        let completed = tracking.pass(6).await;
        assert_reason(&completed, "last-report", "event");
        let delivered = agent_messages(&completed);
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0], messages[0]);
        assert_eq!(delivered[1]["text"], B);
        let lifecycle = events(&completed);
        assert_eq!(lifecycle.len(), 1);
        assert_child_completion(&lifecycle[0], &delivered[1]);
        let history = serde_json::to_string(&completed.messages().collect::<Vec<_>>()).unwrap();
        // A must not be overwritten or repeated
        assert_eq!(history.matches(A).count(), 1);
        // completion must not repeat B
        assert_eq!(history.matches(B).count(), 1);
        let output = runtime.jobs.snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(json!(B)));
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn intermediate_child_replies_wake_parent_or_reach_request_boundary_once() {
        for parent_already_waiting in [true, false] {
            intermediate_child_reply(parent_already_waiting).await;
        }
    }

    async fn intermediate_child_reply(parent_already_waiting: bool) {
        const REPLY: &str = "intermediate-child-reply-marker";
        const FINAL: &str = "final-child-output-marker";
        let mut child_tool = call("continue-child", "script", json!({"source":"return 42;"}));
        child_tool.position = 1;
        let reply = vec![AssistantContent::text("child-reply", 0, REPLY), child_tool];
        let launch =
            json!({"prompt":"child task", "model":"child", "name":"child-replier", "bg":true});
        let final_reply = vec![AssistantContent::text("child-final", 0, FINAL)];
        let after_final = vec![call("after-final", "wait", json!({"timeout":1}))];
        let tracking = Tracking::responses(vec![
            ("root", vec![call("child", "agent", launch)]),
            ("child", vec![call("child-wait", "wait", json!({}))]),
            ("root", vec![call("waiting", "wait", json!({}))]),
            ("child", reply.clone()),
            ("child", final_reply),
            ("root", vec![call("again", "wait", json!({"timeout":1}))]),
            ("root", vec![call("finish-wait", "wait", json!({}))]),
            ("root", after_final),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        let in_flight = tracking.request(2).await;
        assert!(agent_messages(&in_flight).is_empty());
        let child_job = running_job(&session, &session.root, "agent").await;
        let (child, _) = only_child(&session);
        tracking.release(1);
        running_job(&session, &child, "wait").await;
        if parent_already_waiting {
            tracking.release(2);
            running_job(&session, &session.root, "wait").await;
        }

        // Send through the public job API; the child's next request proves delivery.
        let value = "parent-reply-request-marker";
        let send = format!("return await tool.job({child_job}).send({{value:{value:?}}});");
        let sent = bounded(session.run_script(send)).await.unwrap();
        assert_eq!(sent.value["value"], json!({"accepted":true}));
        let child_request = tracking.pass(3).await;
        assert_reason(&child_request, "child-wait", "event");
        let messages =
            serde_json::to_string(&child_request.messages().collect::<Vec<_>>()).unwrap();
        assert_eq!(messages.matches("parent-reply-request-marker").count(), 1);
        // The child committed text WITH a tool call and reached another, gated invoke.
        tracking.request(4).await;
        let state = runtime.jobs.snapshot(child_job).await.unwrap().state;
        assert_eq!(state, JobState::Running);
        if !parent_already_waiting {
            // No new parent request may start while its current invoke is gated.
            assert!(!tracking.requested_from(5));
            tracking.release(2);
        }
        let next = tracking.request(5).await;
        assert_reason(&next, "waiting", "event");
        let state = runtime.jobs.snapshot(child_job).await.unwrap().state;
        // the reply must not depend on child completion
        assert_eq!(state, JobState::Running);
        assert!(events(&next).is_empty(), "a reply is not a job completion");
        let records = runtime.store.records().await;
        let child_records = records.iter().filter(|record| record.agent == child);
        let committed = child_records
            .filter(|record| {
                matches!(&record.event, SessionEvent::MessageCommitted { message: Message::Assistant(content) } if content == &reply)
            })
            .collect::<Vec<_>>();
        assert_eq!(committed.len(), 1);
        let replies = agent_messages(&next);
        let expected = json!({
            "kind":"message",
            "id":child_job.get(),
            "name":"child-replier",
            "message":committed[0].sequence,
            "text":REPLY,
        });
        assert_eq!(replies, vec![expected]);
        let messages = serde_json::to_string(&next.messages().collect::<Vec<_>>()).unwrap();
        let copies = messages.matches(REPLY).count();
        // only the runtime envelope should contain the reply
        assert_eq!(copies, 1);

        tracking.release(5);
        let again = tracking.request(6).await;
        assert_reason(&again, "again", "timeout");
        // do not inject another copy
        assert_eq!(agent_messages(&again), replies);
        assert!(events(&again).is_empty());
        tracking.release(4);
        child_completed(&session, child_job).await;
        tracking.release(6);
        let completed = tracking.pass(7).await;
        assert_reason(&completed, "finish-wait", "event");
        let all_replies = agent_messages(&completed);
        assert_eq!(&all_replies[..replies.len()], replies.as_slice());
        assert_eq!(all_replies.len(), 2);
        assert_eq!(all_replies[1]["text"], FINAL);
        let notifications = events(&completed);
        assert_eq!(notifications.len(), 1);
        assert_child_completion(&notifications[0], &all_replies[1]);
        let last = tracking.pass(8).await;
        assert_reason(&last, "after-final", "timeout");
        assert_eq!(agent_messages(&last), all_replies);
        assert_eq!(events(&last), notifications);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn intermediate_child_replies_outlive_a_full_parent_mailbox() {
        let reply_count = AGENT_CHANNEL_CAPACITY + 1;
        let launch = json!({"prompt":"child task", "model":"child", "bg":true});
        let mut steps = vec![
            ("root", vec![call("child", "agent", launch)]),
            // Unlike wait, this response has no tool call and can end the parent turn.
            ("root", vec![answer()]),
        ];
        for index in 0..reply_count {
            let text = format!("mailbox-child-reply-{index}");
            let reply = AssistantContent::text(format!("child-reply-{index}"), 0, text);
            let script = json!({"source":"return 42;"});
            let mut tool = call(&format!("child-tool-{index}"), "script", script);
            tool.position = 1;
            steps.push(("child", vec![reply, tool]));
        }
        let child_final = steps.len();
        let final_text = AssistantContent::text("child-final", 0, "mailbox-child-final");
        steps.push(("child", vec![final_text]));
        let parent_reply = steps.len();
        steps.push(("root", vec![call("finish-wait", "wait", json!({}))]));
        let parent_completed = steps.len();
        steps.push(("root", vec![answer()]));
        let tracking = Tracking::responses(steps);
        let (_root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        let child_job = running_job(&session, &session.root, "agent").await;
        // Fill the mailbox during invoke; durable messages stay pending without JobsReady.
        while session.root_tx.capacity() > 0 {
            let ready = session.root_tx.send(AgentCommand::JobsReady);
            bounded(ready).await.unwrap();
        }
        for step in 2..child_final {
            tracking.pass(step).await;
        }
        // Publishing more replies than the mailbox holds must not deadlock the child.
        tracking.request(child_final).await;
        let state = runtime.jobs.snapshot(child_job).await.unwrap().state;
        assert_eq!(state, JobState::Running);
        assert!(!tracking.requested_from(parent_reply));
        assert_eq!(session.root_tx.capacity(), 0);
        tracking.release(1);
        let next = tracking.request(parent_reply).await;
        let state = runtime.jobs.snapshot(child_job).await.unwrap().state;
        // a no-tool parent response must not strand replies until child completion
        assert_eq!(state, JobState::Running);
        assert!(events(&next).is_empty());
        let replies = agent_messages(&next);
        // a full mailbox must not drop payloads
        assert_eq!(replies.len(), reply_count);
        for (index, reply) in replies.iter().enumerate() {
            assert_eq!(reply["text"], format!("mailbox-child-reply-{index}"));
        }
        tracking.release(child_final);
        child_completed(&session, child_job).await;
        tracking.release(parent_reply);
        let completed = tracking.pass(parent_completed).await;
        assert_reason(&completed, "finish-wait", "event");
        let all_replies = agent_messages(&completed);
        // retained replies are exactly once
        assert_eq!(&all_replies[..reply_count], replies.as_slice());
        assert_eq!(all_replies.len(), reply_count + 1);
        assert_eq!(all_replies[reply_count]["text"], "mailbox-child-final");
        let notifications = events(&completed);
        assert_eq!(notifications.len(), 1);
        assert_child_completion(&notifications[0], &all_replies[reply_count]);
        bounded(turn).await.unwrap().unwrap();
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
    }

    #[tokio::test]
    async fn progress_and_completion_cross_the_same_no_tool_boundary_once() {
        let tracking = Tracking::new(vec![("root", answer()), ("root", answer())]);
        let (root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        complete_background(&session, &session.root, "no-tool-completion").await;
        let (job, child) = fixture_child(&session, root.path()).await;
        fixture_message(&session, job, &child, "no-tool-progress").await;
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(agent_messages(&next).len(), 1);
        assert_eq!(events(&next).len(), 1);
        // Both entries share one persisted runtime envelope, not separate transactions.
        let records = runtime.store.records().await;
        let root_records = records.iter().filter(|record| record.agent == session.root);
        let envelopes = count!(root_records, SessionEvent::MessageCommitted { message: Message::User(content) } if content.len() == 1 && content.iter().all(|block| matches!(block, UserContent::Runtime { .. })));
        assert_ne!(envelopes, 0);
        // the earlier answer must not finish the caller's turn
        assert!(!turn.is_finished());
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}

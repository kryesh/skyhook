//! Snapshot and present durable child messages and job notifications.
use super::SessionRuntime;
use crate::{
    job::{JobView, PendingDelivery},
    session::{JobEvent, UserPart},
    tool::diagnostic::DiagnosticViewer,
};

impl SessionRuntime {
    /// The owner's pending events, with the receipt whose commit delivers them.
    /// None when nothing is pending, so no empty receipt holds the delivery gate
    /// across a model request.
    pub(in crate::agent::runtime) async fn pending_event_content<'a>(
        &self,
        agent: &crate::identity::AgentId,
        viewer: impl Into<DiagnosticViewer<'a>>,
    ) -> Result<Option<(Vec<UserPart>, PendingDelivery)>, crate::agent::runtime::HarnessError> {
        let pending = self.jobs.pending_delivery(agent).await?;
        let content = self.job_event_content(&pending, viewer.into()).await;
        Ok((!content.is_empty()).then_some((content, pending)))
    }

    async fn job_event_content(
        &self,
        pending: &crate::job::PendingDelivery,
        viewer: DiagnosticViewer<'_>,
    ) -> Vec<UserPart> {
        let mut events: Vec<_> = pending
            .messages()
            .iter()
            .cloned()
            .map(JobEvent::Message)
            .collect();
        for job in pending.envelopes() {
            let view = self
                .jobs
                .present_output_with(
                    crate::job::output::OutputArgs::new(job.id),
                    crate::job::CancellationToken::new(),
                    viewer,
                    crate::job::output::OutputOptions::Host {
                        presentation: crate::job::OutputPresentation::Automatic,
                    },
                )
                .await
                .map(crate::job::PresentedOutput::into_job_view);
            events.push(JobEvent::Job(Box::new(view.unwrap_or_else(|error| {
                JobView {
                    id: Some(job.id),
                    state: job.state.presented(),
                    has_result: false,
                    result: serde_json::Value::Null,
                    error: Some(error.diagnostic().render_for(viewer)),
                    meta: None,
                    presentation: None,
                }
            }))));
        }
        if events.is_empty() {
            return Vec::new();
        }
        vec![UserPart::JobEvents { events }]
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use crate::agent::runtime::tests::count;
    use crate::agent::runtime::*;
    use crate::job::{JobSpec, JobState};

    /// Every runtime job-event envelope of the request, as its parsed entries.
    /// `agent_messages`/`events` flatten across envelopes; batching claims need the
    /// envelope grouping, since one envelope is exactly one delivery snapshot.
    fn envelopes(request: &ModelRequest) -> Vec<Vec<serde_json::Value>> {
        let blocks = request.messages().flat_map(|message| match message {
            Sent::User(content) => content.as_slice(),
            _ => &[],
        });
        blocks
            .filter_map(|block| match block {
                SentPart::Runtime { text } => text
                    .strip_prefix("<skyhook_job_events>\n")
                    .and_then(|events| events.strip_suffix("\n</skyhook_job_events>")),
                _ => None,
            })
            .map(|events| serde_json::from_str(events).unwrap())
            .collect()
    }

    /// Wakes broadcast for `job` so far. Counting wakes rather than observing their
    /// arrival order keeps these assertions independent of any coalescing window:
    /// a wake published before the caller's last observation is already buffered.
    fn drain_wakes(
        wakes: &mut broadcast::Receiver<crate::job::JobCompletion>,
        job: JobId,
    ) -> usize {
        let mut count = 0;
        while let Ok(wake) = wakes.try_recv() {
            count += usize::from(wake.job == job);
        }
        count
    }

    /// A reply released by shutdown's own cancellation must not start a new turn;
    /// it stays journaled for resume.
    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_before_a_pending_child_reply_starts_another_turn() {
        const REPLY: &str = "pending-child-reply";
        let launch = json!({"prompt":"child task", "model":"wait-test/child", "bg":true});
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("delegate", "agent", launch),
                    AssistantItem::tool_call(
                        "hold",
                        1,
                        ToolCall::new("hold", "wait", json!({"timeout":1})).unwrap(),
                    ),
                ],
            ),
            // Never released: the root is parked in `invoke` when shutdown lands.
            ("root", vec![answer()]),
            (
                "child",
                vec![
                    AssistantItem::text("child-reply", 0, REPLY),
                    AssistantItem::tool_call(
                        "child-hold",
                        1,
                        ToolCall::new("child-hold", "wait", json!({})).unwrap(),
                    ),
                ],
            ),
            // Spare, so the bug fails an assertion rather than panicking the provider.
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        tracking.pass(2).await;
        bounded(async {
            while !session.runtime.jobs.has_pending(&session.root).await {
                poll().await;
            }
        })
        .await;
        bounded(session.shutdown()).await.unwrap();
        bounded(session.root_tx.closed()).await;
        assert!(
            !tracking.requested_from(3),
            "stop must not start another turn for a pending reply"
        );
        let records = session.runtime.store.records().await;
        let root_said = records.iter().any(|record| {
            record.agent == session.root
                && matches!(&record.event, SessionEvent::MessageCommitted { message }
                    if serde_json::to_string(message).unwrap_or_default().contains(REPLY))
        });
        assert!(!root_said, "the reply must stay pending, not be presented");
        let child_said = records.iter().any(|record| {
            record.agent != session.root
                && matches!(&record.event, SessionEvent::MessageCommitted { message }
                    if serde_json::to_string(message).unwrap_or_default().contains(REPLY))
        });
        assert!(child_said, "the child's own commit must survive for resume");
        assert!(bounded(turn).await.unwrap().is_err());
    }

    /// A script's foreground `tool.agent(...)` returns the child's answer to the
    /// script alone: no reply is left pending for its waits or for the model.
    #[tokio::test(start_paused = true)]
    async fn script_foreground_child_leaves_no_pending_reply() {
        const FINAL: &str = "readme-first-lines";
        let source = "const answer = await tool.agent({prompt:'read', model:'wait-test/child', name:'read-readme'}); \
            const first = (await tool.wait({timeout:1})).result; \
            const second = (await tool.wait({timeout:1})).result; \
            return [first, second];";
        let tracking = tracking_all(vec![
            (
                "root",
                vec![call("run", "script", json!({"source": source}))],
            ),
            ("child", vec![AssistantItem::text("final", 0, FINAL)]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let script = running_job(&session, &session.root, "script").await;
        tracking.pass(1).await;
        let request = tracking.request(2).await;
        let output = session
            .runtime
            .jobs
            .wait(script, None, false)
            .await
            .unwrap()
            .output;
        let expected = json!([{"reason":"timeout"}, {"reason":"timeout"}]);
        assert_eq!(
            output.as_ref().map(|output| &output["value"]),
            Some(&expected),
            "{output:?}"
        );
        let history = rendered(&request);
        assert!(!history.contains(FINAL), "{history}");
        tracking.release(2);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A `wait` beside a foreground child keeps waiting through the child's progress;
    /// the next request carries the answer in the child's result, not its progress.
    #[tokio::test(start_paused = true)]
    async fn outstanding_foreground_work_defers_wait_resolution() {
        const PROGRESS: &str = "foreground-progress";
        const FINAL: &str = "foreground-final";
        let delegate = json!({"prompt":"work", "model":"wait-test/child", "name":"kid"});
        let waiting = ToolCall::new("waiting", "wait", json!({"timeout":30})).unwrap();
        let hold = ToolCall::new("child-hold", "wait", json!({"timeout":1})).unwrap();
        let tracking = tracking_all(vec![
            (
                "root",
                vec![
                    call("kid", "agent", delegate),
                    AssistantItem::tool_call("waiting", 1, waiting),
                ],
            ),
            (
                "child",
                vec![
                    AssistantItem::text("progress", 0, PROGRESS),
                    AssistantItem::tool_call("child-hold", 1, hold),
                ],
            ),
            ("child", vec![AssistantItem::text("final", 0, FINAL)]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let wait = running_job(&session, &session.root, "wait").await;
        tracking.pass(1).await;
        // A second after the progress, an undeferred wait would have resolved.
        tracking.request(2).await;
        let state = session.runtime.jobs.snapshot(wait).await.unwrap().state;
        assert_eq!(
            state,
            JobState::Running,
            "the foreground child still holds the turn"
        );
        tracking.release(2);
        let request = tracking.request(3).await;
        assert_reason(&request, "waiting", "event");
        let history = rendered(&request);
        assert!(
            !history.contains(PROGRESS) && history.contains(FINAL),
            "{history}"
        );
        tracking.release(3);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A completing child's answer and its completion envelope are one wake: the
    /// answer is published without a wake and the job completion carries both.
    #[tokio::test(start_paused = true)]
    async fn terminal_child_reply_and_completion_reach_the_owner_in_one_batch() {
        const FINAL: &str = "merged-child-final-answer";
        let launch =
            json!({"prompt":"report", "model":"wait-test/child", "name":"merged", "bg":true});
        let tracking = tracking_all(vec![
            ("root", vec![call("launch", "agent", launch)]),
            ("child", vec![AssistantItem::text("final", 0, FINAL)]),
            ("root", vec![call("waiting", "wait", json!({}))]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        let job = running_job(&session, &session.root, "agent").await;
        // Park the owner in an indefinite wait: only a wake can produce its next
        // request, so request count is exactly the number of wakes it observed.
        tracking.pass(2).await;
        running_job(&session, &session.root, "wait").await;
        let mut wakes = session.runtime.jobs.subscribe_completions();
        tracking.release(1);
        child_completed(&session, job).await;
        let woken = tracking.request(3).await;
        assert_reason(&woken, "waiting", "event");
        // Exactly one wake, independent of any coalescing window: the answer is
        // published silently and the job completion is the wake that carries it.
        assert_eq!(drain_wakes(&mut wakes, job), 1);
        // One envelope, holding the answer and the completion that references it.
        let envelopes = envelopes(&woken);
        assert_eq!(
            envelopes.len(),
            1,
            "answer and completion must share one batch"
        );
        let batch = &envelopes[0];
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["kind"], "message");
        assert_eq!(batch[0]["text"], FINAL);
        assert_child_completion(&batch[1], &batch[0]);
        // The wake came from the completion, so the owner never ran on the answer
        // alone: three requests total, and the answer is not copied anywhere else.
        assert_eq!(tracking.requests.lock().unwrap().len(), 4);
        let history = rendered(&woken);
        assert_eq!(history.matches(FINAL).count(), 1);
        tracking.release(3);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A child that answers while its own work is still running has not resolved its
    /// invocation, so the driver wakes the owner for that answer immediately.
    #[tokio::test(start_paused = true)]
    async fn answering_child_with_live_work_wakes_its_owner_before_completing() {
        const PROGRESS: &str = "child-answer-with-live-work";
        const FINAL: &str = "child-answer-after-live-work";
        let launch =
            json!({"prompt":"report", "model":"wait-test/child", "name":"worker", "bg":true});
        let work = json!({"source":"return await receive();", "bg":true});
        let tracking = tracking_all(vec![
            ("root", vec![call("launch", "agent", launch)]),
            ("child", vec![call("work", "script", work)]),
            ("child", vec![AssistantItem::text("progress", 0, PROGRESS)]),
            ("root", vec![call("waiting", "wait", json!({}))]),
            ("root", vec![call("again", "wait", json!({}))]),
            ("child", vec![AssistantItem::text("final", 0, FINAL)]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let jobs = &session.runtime.jobs;
        let turn = prompt(&session);
        tracking.pass(0).await;
        let job = running_job(&session, &session.root, "agent").await;
        // The child agent exists once it has requested a response.
        tracking.request(1).await;
        let (child, _) = only_child(&session);
        tracking.release(1);
        let work = running_job(&session, &child, "script").await;
        tracking.pass(3).await;
        running_job(&session, &session.root, "wait").await;
        // A text-only response, but the child's own background job keeps the
        // invocation open: the answer must not wait for that job to be delivered.
        let mut wakes = session.runtime.jobs.subscribe_completions();
        tracking.release(2);
        let woken = tracking.request(4).await;
        assert_reason(&woken, "waiting", "event");
        // The driver, not the commit, issued this single wake.
        assert_eq!(drain_wakes(&mut wakes, job), 1);
        let replies = agent_messages(&woken);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["text"], PROGRESS);
        assert!(events(&woken).is_empty(), "the child has not completed");
        assert_eq!(jobs.snapshot(job).await.unwrap().state, JobState::Running);

        // Releasing its work lets the child answer again and complete; that answer
        // and the completion then arrive together, as in the merged case above.
        tracking.release(4);
        running_job(&session, &session.root, "wait").await;
        jobs.send(work, json!("released")).await.unwrap();
        tracking.pass(5).await;
        child_completed(&session, job).await;
        let completed = tracking.request(6).await;
        assert_reason(&completed, "again", "event");
        // The second answer completes the invocation, so its completion is again the
        // only wake and presents both entries in one batch.
        assert_eq!(drain_wakes(&mut wakes, job), 1);
        let batch = envelopes(&completed).pop().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["text"], FINAL);
        assert_child_completion(&batch[1], &batch[0]);
        tracking.release(6);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn no_tool_child_reports_survive_queued_input_and_pending_runtime_events() {
        // (parent already waiting, input is a pending runtime event rather than a send)
        for (waiting, runtime_event) in [(true, false), (false, false), (false, true)] {
            no_tool_child_reports(waiting, runtime_event).await;
        }
    }

    async fn no_tool_child_reports(parent_already_waiting: bool, pending_runtime_event: bool) {
        const A: &str = "child-report-A";
        const B: &str = "child-addendum-B";
        let launch =
            json!({"prompt":"report", "model":"wait-test/child", "name":"reporter", "bg":true});
        let tracking = tracking(vec![
            ("root", call("launch", "agent", launch)),
            ("child", AssistantItem::text("report", 0, A)),
            ("root", call("first-report", "wait", json!({}))),
            ("child", AssistantItem::text("addendum", 0, B)),
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
            assert_eq!(sent.value["value"]["result"], json!({"accepted":true}));
            // Occupancy proves forwarding to the child's queue while A's invoke is gated.
            bounded(async {
                while sender.capacity() != AGENT_CHANNEL_CAPACITY - 1 {
                    poll().await;
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
        let messages = rendered(&addendum);
        assert_eq!(messages.matches("please add an addendum").count(), 1);
        assert!(addendum.messages().any(|message| matches!(message,
            Sent::Assistant(content) if content == &[AssistantItem::text("report", 0, A)])));
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
        let history = rendered(&completed);
        // A must not be overwritten or repeated
        assert_eq!(history.matches(A).count(), 1);
        // completion must not repeat B
        assert_eq!(history.matches(B).count(), 1);
        let output = runtime.jobs.snapshot(job).await.unwrap().output;
        assert_eq!(output, Some(json!(B)));
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// A reasoning model's working turn is `reasoning` + a blank text separator +
    /// its calls. The blank block stays in the child's history for replay, but the
    /// turn answered nothing, so it must publish no reply, wake nobody and leave no
    /// delivery behind. Otherwise the owner collects one blank child message per
    /// child turn, which is what a raw `is_empty` projection check produced.
    #[tokio::test(start_paused = true)]
    async fn blank_child_turn_publishes_no_reply_and_never_wakes_the_owner() {
        const FINAL: &str = "child-final-answer-after-blank";
        let child_tool = call_at(
            2,
            "blank-turn-tool",
            "script",
            json!({"source":"return 42;"}),
        );
        let blank = vec![
            AssistantItem::reasoning("thought", 0, "private reasoning", None),
            AssistantItem::text("blank", 1, "\n\n"),
            child_tool,
        ];
        let launch =
            json!({"prompt":"child task", "model":"wait-test/child", "name":"blank", "bg":true});
        let tracking = tracking_all(vec![
            ("root", vec![call("launch", "agent", launch)]),
            ("child", blank.clone()),
            ("root", vec![call("waiting", "wait", json!({}))]),
            ("child", vec![AssistantItem::text("final", 0, FINAL)]),
            ("root", vec![answer()]),
        ]);
        let (_root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.pass(0).await;
        tracking.request(1).await;
        let job = running_job(&session, &session.root, "agent").await;
        let (child, _) = only_child(&session);
        // Park the owner in an indefinite wait: only a wake produces its next request.
        tracking.pass(2).await;
        running_job(&session, &session.root, "wait").await;
        let mut wakes = runtime.jobs.subscribe_completions();

        // The blank turn lands, and the child's next request proves it moved on.
        tracking.release(1);
        tracking.request(3).await;
        assert_eq!(
            drain_wakes(&mut wakes, job),
            0,
            "a blank turn woke the owner"
        );
        assert!(!runtime.jobs.has_pending(&session.root).await);
        assert!(
            !tracking.requested_from(4),
            "the owner left its wait for a blank turn"
        );
        let records = runtime.store.records().await;
        // The child's turn commits whole: the separator is history, though not a reply.
        let committed = records
            .iter()
            .filter(|record| record.agent == child)
            .filter(|record| {
                matches!(&record.event, SessionEvent::MessageCommitted { message: Message::Assistant(content) } if *content == blank)
            });
        assert_eq!(committed.count(), 1);
        let delivered = |records: &[crate::session::EventRecord]| {
            records
                .iter()
                .filter(|record| matches!(&record.event, SessionEvent::JobMessageDelivered { .. }))
                .count()
        };
        assert_eq!(delivered(&records), 0, "a blank turn was delivered");

        // Only the real answer reaches the owner, once, with its completion.
        tracking.release(3);
        child_completed(&session, job).await;
        let woken = tracking.request(4).await;
        assert_reason(&woken, "waiting", "event");
        assert_eq!(drain_wakes(&mut wakes, job), 1);
        let replies = agent_messages(&woken);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["text"], FINAL);
        tracking.release(4);
        bounded(turn).await.unwrap().unwrap();
        assert_eq!(delivered(&runtime.store.records().await), 1);
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn intermediate_child_replies_wake_parent_or_reach_request_boundary_once() {
        for parent_already_waiting in [true, false] {
            intermediate_child_reply(parent_already_waiting).await;
        }
    }

    async fn intermediate_child_reply(parent_already_waiting: bool) {
        const REPLY: &str = "intermediate-child-reply-marker";
        const FINAL: &str = "final-child-output-marker";
        let child_tool = call_at(
            1,
            "continue-child",
            "script",
            json!({"source":"return 42;"}),
        );
        let reply = vec![AssistantItem::text("child-reply", 0, REPLY), child_tool];
        let launch = json!({"prompt":"child task", "model":"wait-test/child", "name":"child-replier", "bg":true});
        let final_reply = vec![AssistantItem::text("child-final", 0, FINAL)];
        let after_final = vec![call("after-final", "wait", json!({"timeout":1}))];
        let tracking = tracking_all(vec![
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
        assert_eq!(sent.value["value"]["result"], json!({"accepted":true}));
        let child_request = tracking.pass(3).await;
        assert_reason(&child_request, "child-wait", "event");
        let messages = rendered(&child_request);
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
        let messages = rendered(&next);
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

    #[tokio::test(start_paused = true)]
    async fn intermediate_child_replies_outlive_a_full_parent_mailbox() {
        let reply_count = AGENT_CHANNEL_CAPACITY + 1;
        let launch = json!({"prompt":"child task", "model":"wait-test/child", "bg":true});
        let mut steps = vec![
            ("root", vec![call("child", "agent", launch)]),
            // Unlike wait, this response has no tool call and can end the parent turn.
            ("root", vec![answer()]),
        ];
        for index in 0..reply_count {
            let text = format!("mailbox-child-reply-{index}");
            let reply = AssistantItem::text(format!("child-reply-{index}"), 0, text);
            // Any cheap tool keeps the child working after its reply.
            let tool = call_at(
                1,
                &format!("child-tool-{index}"),
                "todo",
                json!({"items":[]}),
            );
            steps.push(("child", vec![reply, tool]));
        }
        let child_final = steps.len();
        let final_text = AssistantItem::text("child-final", 0, "mailbox-child-final");
        steps.push(("child", vec![final_text]));
        let parent_reply = steps.len();
        steps.push(("root", vec![call("finish-wait", "wait", json!({}))]));
        let parent_completed = steps.len();
        steps.push(("root", vec![answer()]));
        let tracking = tracking_all(steps);
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

    #[tokio::test(start_paused = true)]
    async fn progress_and_completion_cross_the_same_no_tool_boundary_once() {
        let tracking = tracking(vec![("root", answer()), ("root", answer())]);
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
        let envelopes = count!(root_records, SessionEvent::MessageCommitted { message: Message::User(content) } if content.len() == 1 && content.iter().all(|block| matches!(block, UserPart::JobEvents { .. })));
        assert_ne!(envelopes, 0);
        // the earlier answer must not finish the caller's turn
        assert!(!turn.is_finished());
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    /// Publish through the same durable source-history path as a real child, without
    /// invoking another provider. The owner stays gated while a test prepares receipts.
    async fn fixture_child(session: &SessionHandle, root: &Path) -> (JobId, AgentId) {
        let runtime = &session.runtime;
        let spec = JobSpec {
            background: true,
            role: crate::job::JobRole::Agent,
            ..JobSpec::test(session.root.clone(), "agent")
        };
        let id = runtime.jobs.test_running(spec).await.into_test_id();
        let child = session.root.child(123);
        let started = crate::session::tests::child_started(
            Some(id),
            crate::execution::ExecutionLocation::root(root.to_path_buf()),
        );
        runtime.store.append(child.clone(), started).await.unwrap();
        (id, child)
    }

    async fn fixture_message(
        session: &SessionHandle,
        job: JobId,
        child: &AgentId,
        text: &str,
    ) -> MessageSeq {
        let message = Message::Assistant(vec![AssistantItem::text("fixture-reply", 0, text)]);
        let jobs = &session.runtime.jobs;
        // Fixture progress stands in for a non-terminal child reply: it wakes the owner.
        let committed =
            jobs.commit_child_message(child, job, message, text.to_owned(), true, |_| Vec::new());
        committed.await.unwrap()
    }

    /// The owner's pending events and the receipt that delivers them.
    async fn pending_content(
        session: &SessionHandle,
    ) -> (Vec<UserPart>, crate::job::PendingDelivery) {
        let runtime = &session.runtime;
        let capabilities = &runtime.capabilities;
        let pending = runtime.pending_event_content(session.root_agent(), capabilities);
        pending.await.unwrap().expect("events are pending")
    }

    /// Snapshotting must not consume the only durable copy of a child's progress.
    #[tokio::test(start_paused = true)]
    async fn child_progress_snapshot_survives_abandoned_parent_commits() {
        let tracking = tracking(vec![("root", answer()), ("root", answer())]);
        let (root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let (job, child) = fixture_child(&session, root.path()).await;
        let first = fixture_message(&session, job, &child, "retained-progress").await;
        let (content, batch) = pending_content(&session).await;
        assert_eq!(content.len(), 1);
        drop(batch);
        assert!(runtime.jobs.has_pending(session.root_agent()).await);

        let (retry, batch) = pending_content(&session).await;
        // abandoning a receipt must retain its source message
        assert_eq!(retry, content);
        let sequence = batch.commit(Message::User(retry)).await.unwrap();
        let records = runtime.store.records().await;
        let committed = records
            .iter()
            .find(|record| record.sequence == RecordSeq::from(sequence));
        let committed = committed.unwrap();
        assert_eq!(&committed.agent, session.root_agent());
        assert!(matches!(&committed.event,
            SessionEvent::MessageCommitted { message: Message::User(saved) } if saved == &content));
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        // Equal text is a new independent event when its source sequence differs.
        let second = fixture_message(&session, job, &child, "retained-progress").await;
        assert_ne!(first, second);
        let (remaining, batch) = pending_content(&session).await;
        let remaining = serde_json::to_string(&remaining).unwrap();
        assert!(remaining.contains("retained-progress"));
        drop(batch);
        tracking.release(0);
        let next = tracking.pass(1).await;
        let delivered = agent_messages(&next);
        assert_eq!(delivered.len(), 2);
        let found = (&delivered[0]["message"], &delivered[1]["message"]);
        assert_eq!(found, (&json!(first), &json!(second)));
        assert!(!runtime.jobs.has_pending(session.root_agent()).await);
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_started_commit_finishes_acknowledgments_and_serializes_the_next_snapshot()
    {
        for with_completion in [true, false] {
            cancelled_notification_commit(with_completion).await;
        }
    }

    async fn cancelled_notification_commit(with_completion: bool) {
        let tracking = tracking(vec![("root", answer()), ("root", answer())]);
        let (root, session) = start(&tracking).await;
        let runtime = &session.runtime;
        let turn = prompt(&session);
        tracking.request(0).await;
        let completion = if with_completion {
            Some(complete_background(&session, &session.root, "committed-completion").await)
        } else {
            None
        };
        let (job, child) = fixture_child(&session, root.path()).await;
        fixture_message(&session, job, &child, "committed-progress").await;
        let (content, batch) = pending_content(&session).await;
        // progress and completion share the same envelope
        assert_eq!(content.len(), 1);
        assert!(runtime.jobs.has_pending(session.root_agent()).await);
        let mut records = runtime.store.subscribe();
        {
            let commit = batch.commit(Message::User(content));
            tokio::pin!(commit);
            // Unpolled since scheduling: dropping the caller models an interrupt just
            // after this transaction's ownership boundary.
            assert!(futures_util::poll!(commit.as_mut()).is_pending());
        }
        {
            // An immediate resumed request cannot snapshot progress still owned by
            // the interrupted caller's transaction, even without any terminal job.
            let next_batch = pending_content(&session);
            tokio::pin!(next_batch);
            assert!(futures_util::poll!(next_batch.as_mut()).is_pending());
        }
        bounded(async {
            loop {
                let record = records.recv().await.unwrap();
                if matches!(&record.event, SessionEvent::MessageCommitted { message } if matches!(message, Message::User(_))) {
                    break;
                }
            }
            while runtime.jobs.has_pending(session.root_agent()).await {
                poll().await;
            }
        })
        .await;
        // Duplicate wake commands do not reconstruct already acknowledged output.
        session.root_tx.jobs_ready();
        session.root_tx.jobs_ready();
        tracking.release(0);
        bounded(turn).await.unwrap().unwrap();
        let turn = prompt(&session);
        let next = tracking.pass(1).await;
        assert_eq!(agent_messages(&next).len(), 1);
        let completions = events(&next)
            .iter()
            .map(|event| event["id"].clone())
            .collect::<Vec<_>>();
        let expected = completion.map(|job| json!(job.get()));
        assert_eq!(completions, expected.into_iter().collect::<Vec<_>>());
        bounded(turn).await.unwrap().unwrap();
        session.shutdown().await.unwrap();
    }
}

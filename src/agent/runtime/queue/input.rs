//! Individual queued-input validation, model selection, and commit receipts.

use super::*;

impl SessionRuntime {
    /// Switch the agent to profile `name`, if given and different. Everything is
    /// resolved before the journal append, so a rejected selection changes nothing;
    /// `images` rejects an unsupported profile before any provider context opens.
    pub(in crate::agent::runtime) async fn select_model(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        name: Option<String>,
        images: bool,
    ) -> Result<(), HarnessError> {
        let name = name.filter(|name| name != model_profile);
        let profile = match &name {
            Some(name) => self
                .harness
                .model_profiles
                .get(name)
                .cloned()
                .ok_or_else(|| HarnessError::UnknownModelProfile(name.clone()))?,
            None => context.profile.clone(),
        };
        if images && !profile.supports_images {
            return Err(HarnessError::ImagesUnsupported(profile.model));
        }
        let Some(name) = name else {
            return Ok(());
        };
        let mut replacement = None;
        if profile != context.profile {
            let system = context.template.system().to_vec();
            // The tools stay pinned across a model change.
            let tools = Some(context.template.to_request().tools);
            let opened = self
                .open_agent_context(agent, profile.clone(), system, capabilities, tools, false)
                .await?;
            if !profile.supports_images && opened.contains_images() {
                return Err(HarnessError::ImagesUnsupported(profile.model));
            }
            replacement = Some(opened);
        }
        let profile = crate::session::ProfileSnapshot {
            name: name.clone(),
            profile,
        };
        let event = SessionEvent::ModelChanged { profile };
        self.store.append(agent.clone(), event).await?;
        // No await between installation and its routing projection.
        if let Some(replacement) = replacement {
            *context = replacement;
        }
        *model_profile = name;
        if let Some(live) = self.agents_mut().get_mut(agent) {
            live.model_profile.clone_from(model_profile);
        }
        Ok(())
    }

    pub(in crate::agent::runtime) async fn consume_queued_input(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        input: QueuedInput,
    ) -> bool {
        if self.shutting_down.load(Ordering::Acquire) {
            input.reject(HarnessError::AgentStopped);
            return false;
        }
        if !input.cancellation.try_claim() {
            input.reject(HarnessError::Interrupted);
            return false;
        }
        let QueuedInput {
            content,
            model,
            committed,
            ..
        } = input;
        let images = content.iter().any(UserContent::is_image);
        // ModelChanged precedes the MessageCommitted it applies to.
        let result = async {
            self.select_model(agent, context, model_profile, capabilities, model, images)
                .await?;
            let message = Message::User(content);
            let event = SessionEvent::MessageCommitted {
                message: message.clone(),
            };
            let record = self.store.append(agent.clone(), event).await?;
            context.projected.push((record.sequence, message));
            Ok(())
        }
        .await;
        let consumed = result.is_ok();
        if consumed {
            // Receipt readers must already observe a busy agent when this input
            // starts a fresh turn; publishing afterward races the UI's idle check.
            self.activity(agent, AgentActivity::Working);
        }
        let _ = committed.send(result);
        consumed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[tokio::test]
    async fn running_child_parent_inputs_are_fifo_once_after_tool_or_final_responses() {
        for first_calls_tool in [true, false] {
            running_child_receives_parent_inputs(first_calls_tool).await;
        }
    }

    async fn running_child_receives_parent_inputs(first_calls_tool: bool) {
        let (_root, tracking, session) = start(first_calls_tool).await;
        let runtime = &session.runtime;
        let launch = "return await tool.agent({prompt:'test:child-initial',bg:true});";
        let launched = bounded(session.run_script(launch)).await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let initial = tracking.request(0).await;
        assert_eq!(texts(initial.messages()), ["test:child-initial"]);
        assert!(parent_inputs(initial.messages()).is_empty());
        let sender = {
            let agents = runtime.agents.read().unwrap();
            let mut children = agents.iter().filter(|(id, _)| *id != &session.root);
            let (_, child) = children.next().unwrap();
            assert!(children.next().is_none());
            child.sender.clone()
        };
        // A send acknowledgement alone does not prove forwarding: observe the child's mailbox.
        let forwarded = async |count| {
            bounded(async {
                while sender.capacity() != AGENT_CHANNEL_CAPACITY - count {
                    tokio::task::yield_now().await;
                }
            })
            .await
        };

        // Exercise the public script send as well as the underlying job mailbox.
        let first = json!({"instruction": "test:parent-one", "nested": [1, true]});
        let second = json!("test:parent-two");
        let send = format!("return await tool.job({job}).send({{value:{first}}});");
        let accepted = bounded(session.run_script(send)).await.unwrap();
        assert_eq!(accepted.value["value"], json!({"accepted": true}));
        forwarded(1).await;
        let send = runtime.jobs.send(job, second.clone());
        bounded(send).await.unwrap();
        forwarded(2).await;
        assert_eq!(count(&tracking), 1);

        // Keep wakeups out of the script-driven parent loop; the child mailbox stays live.
        let root_inbox = quiet_root(&session);

        tracking.release(0);
        let next = tracking.request(1).await;
        let (first, second) = (
            format!("Owner input: {first}"),
            format!("Owner input: {second}"),
        );
        assert_eq!(parent_inputs(next.messages()), [first, second]);
        assert_eq!(texts(next.messages()), ["test:child-initial"]);
        if first_calls_tool {
            assert!(next.messages().any(todo_finished));
        } else {
            assert!(next.messages().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("text/0", 0, "answer-0")])));
        }
        let state = runtime.jobs.snapshot(job).await.unwrap().state;
        // The child must answer the parent updates first.
        assert!(!state.is_terminal());

        tracking.release(1);
        let completed = bounded(runtime.jobs.wait(job, None, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        assert_eq!(completed.output, Some(json!("answer-1")));
        drop(root_inbox);
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn queued_fifo_images_and_all_model_changes_commit_before_next_request() {
        let (root, tracking, session) = start(true).await;
        let png = |name| crate::tests::png(name);
        let (first_png, second_png) = (png(b"first image fixture"), png(b"second image fixture"));
        let image = |name: &str, image: &crate::media::Image| crate::media::Attachment::Image {
            file: Some(root.path().join(name)),
            image: image.clone(),
        };
        let turn = prompt(&session, "test:initial");
        let in_flight = tracking.request(0).await;
        assert_eq!(in_flight.model, "first-model");

        let first_image = vec![image("first.png", &first_png)];
        let (first, first_cancel) =
            enqueue(&session, "test:queued-one", first_image, Some("second"));
        buffered(&session, 1).await;
        let second_image = vec![image("second.png", &second_png)];
        let (second, second_cancel) =
            enqueue(&session, "test:queued-two", second_image, Some("third"));
        buffered(&session, 2).await;
        assert!(!first.is_finished() && !second.is_finished());
        assert!(!first_cancel.is_claimed() && !second_cancel.is_claimed());
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(count(&tracking), 1);

        tracking.release(0);
        let next = tracking.request(1).await;
        bounded(first).await.unwrap().unwrap();
        bounded(second).await.unwrap().unwrap();
        // Receipts must not wait for the gated next response.
        assert!(!turn.is_finished());
        assert!(first_cancel.is_claimed() && second_cancel.is_claimed());
        // A committed submission cannot be recalled.
        assert!(!first_cancel.cancel() && !second_cancel.cancel());
        assert_eq!(next.model, "third-model");
        let expected = ["test:initial", "test:queued-one", "test:queued-two"];
        assert_eq!(texts(next.messages()), expected);
        let blocks = next.messages().flat_map(|message| match message {
            Message::User(blocks) => blocks.as_slice(),
            _ => &[],
        });
        let images = blocks.filter_map(|block| match block {
            UserContent::Attachment {
                attachment: crate::media::AttachmentRef::Image(image),
            } => Some(next.blobs.get(&image.blob).unwrap()),
            _ => None,
        });
        let expected_images = [first_png.bytes(), second_png.bytes()];
        assert_eq!(images.collect::<Vec<_>>(), expected_images);
        // The ongoing tool call must finish normally.
        assert!(next.messages().any(todo_finished));
        // Do not collapse intermediate captured model changes.
        assert_eq!(model_changes(&session).await, ["second", "third"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        // Queued inputs must not become additional turns.
        assert_eq!(count(&tracking), 2);
        assert_eq!(texts(&committed(&session).await), expected);
    }

    #[tokio::test]
    async fn queued_input_is_not_lost_at_a_final_response_boundary() {
        let (_root, tracking, session) = start(false).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let (receipt, token_cancel) = enqueue(&session, "test:follow-up", vec![], None);
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        tracking.release(0);
        let next = tracking.request(1).await;
        // The receipt must not wait for the gated continuation response.
        bounded(receipt).await.unwrap().unwrap();
        assert!(token_cancel.is_claimed());
        assert_eq!(texts(next.messages()), ["test:initial", "test:follow-up"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn canceled_buffered_submission_never_commits_text_or_model() {
        let (_root, tracking, session) = start(true).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let (receipt, token_cancel) = enqueue(&session, "test:canceled", vec![], Some("second"));
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        assert!(token_cancel.cancel());
        assert!(token_cancel.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        assert!(bounded(receipt).await.unwrap().is_err());
        assert!(!token_cancel.is_claimed());
        assert_eq!(next.model, "first-model");
        assert_eq!(texts(next.messages()), ["test:initial"]);
        assert!(model_changes(&session).await.is_empty());
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn ordinary_prompt_still_waits_for_its_own_turn() {
        let (_root, tracking, session) = start(true).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let ordinary = prompt(&session, "test:ordinary");
        buffered(&session, 1).await;
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(texts(next.messages()), ["test:initial"]);
        assert!(!ordinary.is_finished() && !turn.is_finished());
        tracking.release(1);
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-1");
        assert_eq!(bounded(ordinary).await.unwrap().unwrap(), "answer-2");
        let last = tracking.request(2).await;
        assert_eq!(texts(last.messages()), ["test:initial", "test:ordinary"]);
        stop(&session).await;
        assert_eq!(count(&tracking), 3);
    }

    #[tokio::test]
    async fn queued_validation_errors_do_not_claim_or_commit() {
        use HarnessError::{ImageLimit, UnknownModelProfile};
        let (_root, tracking, session) = start(false).await;
        let runtime = &session.runtime;
        // One byte over the per-image limit, and one image over the count limit.
        let image = |bytes: &[u8]| crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(bytes),
        };
        let (oversized, small) = (image(&vec![0; MAX_IMAGE_BYTES as usize - 7]), image(b""));
        let before = runtime.store.records().await.len();
        let cases = [
            (Some("missing-model"), vec![], "model"),
            (None, vec![oversized], "size"),
            (None, vec![small; MAX_IMAGES_PER_SUBMISSION + 1], "count"),
        ];
        for (model, attachments, case) in cases {
            let prompt = QueuedPrompt {
                text: "test:invalid".into(),
                attachments,
                options: PromptOptions {
                    model: model.map(str::to_owned),
                },
                cancellation: QueuedPromptCancellation::default(),
            };
            let token_cancel = prompt.cancellation.clone();
            let enqueued = enqueue_prompts(&session, vec![prompt]);
            let error = bounded(enqueued).await.pop().unwrap().unwrap_err();
            if case == "model" {
                assert!(matches!(error, UnknownModelProfile(_)), "{case}");
            } else {
                assert!(matches!(error, ImageLimit), "{case}");
            }
            assert!(!token_cancel.is_claimed(), "{case}");
            assert!(token_cancel.cancel(), "{case}");
            assert_eq!(runtime.store.records().await.len(), before, "{case}");
        }
        assert_eq!(count(&tracking), 0);
        stop(&session).await;
    }
}

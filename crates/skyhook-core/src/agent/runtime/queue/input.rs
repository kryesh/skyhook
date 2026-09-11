//! Individual queued-input validation, model selection, and commit receipts.

use super::*;

impl SessionRuntime {
    pub(in crate::agent::runtime) async fn select_model(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        model: String,
    ) -> Result<(), HarnessError> {
        if model == *model_profile {
            return Ok(());
        }
        let selected = self
            .harness
            .model_profiles
            .get(&model)
            .cloned()
            .ok_or_else(|| HarnessError::UnknownModelProfile(model.clone()))?;
        let replacement = if selected != context.profile {
            let replacement = self
                .open_agent_context(
                    agent,
                    selected.clone(),
                    context.template.system.clone(),
                    capabilities,
                    false,
                )
                .await?;
            if !selected.supports_images && replacement.contains_images() {
                return Err(HarnessError::ImagesUnsupported(selected.model.clone()));
            }
            Some(replacement)
        } else {
            None
        };
        self.store
            .append(
                agent.clone(),
                SessionEvent::ModelChanged {
                    model_profile: model.clone(),
                    max_context: selected.max_context,
                },
            )
            .await?;
        if let Some(replacement) = replacement {
            *context = replacement;
        }
        *model_profile = model;
        if let Some(live) = self
            .agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(agent)
        {
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
            let _ = input.token.cancel();
            let _ = input.committed.send(Err(HarnessError::AgentStopped));
            return false;
        }
        if !input.token.claim() {
            let _ = input.committed.send(Err(HarnessError::Interrupted));
            return false;
        }
        let result = async {
            let profile = match &input.model {
                Some(model) => self
                    .harness
                    .model_profiles
                    .get(model)
                    .ok_or_else(|| HarnessError::UnknownModelProfile(model.clone()))?,
                None => &context.profile,
            };
            if !profile.supports_images
                && input
                    .content
                    .iter()
                    .any(|content| matches!(content, UserContent::Image { .. }))
            {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            if let Some(model) = input.model {
                self.select_model(agent, context, model_profile, capabilities, model)
                    .await?;
            }
            let message = Message::User(input.content);
            let origin = self.commit(agent, message.clone()).await?;
            context.projected.push((origin, message));
            Ok(())
        }
        .await;
        let consumed = result.is_ok();
        if consumed {
            // Receipt readers must already observe a busy agent when this input
            // starts a fresh turn; publishing afterward races the UI's idle check.
            self.activity(agent, AgentActivity::Working);
        }
        let _ = input.committed.send(result);
        consumed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    async fn running_child_receives_parent_inputs(first_calls_tool: bool) {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(first_calls_tool);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let launched = bounded(
            session.run_script("return await tool.agent({prompt:'test:child-initial',bg:true});"),
        )
        .await
        .unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let initial = tracking.request(0).await;
        assert_eq!(texts(&initial.messages), ["test:child-initial"]);
        assert!(parent_inputs(&initial.messages).is_empty());
        let sender = {
            let agents = runtime.agents.read().unwrap();
            let children = agents
                .iter()
                .filter(|(id, _)| *id != &session.root)
                .collect::<Vec<_>>();
            assert_eq!(children.len(), 1);
            children[0].1.sender.clone()
        };

        // Exercise the public script send as well as the underlying job mailbox.
        // Neither input answers a question: the child is inside its gated invoke.
        let first = json!({"instruction": "test:parent-one", "nested": [1, true]});
        let second = json!("test:parent-two");
        let accepted = bounded(session.run_script(format!(
            "return await tool.job({}).send({{value:{first}}});",
            job.get()
        )))
        .await
        .unwrap();
        assert_eq!(accepted.value["value"], json!({"accepted": true}));
        bounded(async {
            while sender.capacity() != AGENT_CHANNEL_CAPACITY - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        bounded(runtime.jobs.send(job, second.clone()))
            .await
            .unwrap();
        // A job send acknowledgement alone does not establish that tools.rs has
        // forwarded the value. Observe the child's actual command mailbox before
        // allowing either a tool response or a final response to finish.
        bounded(async {
            while sender.capacity() != AGENT_CHANNEL_CAPACITY - 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 1);

        // This test drives the parent through scripts and explicitly claims the
        // child's result. Keep message and completion wakeups out of the autonomous
        // parent loop: every visible response, including one before queued input,
        // can now wake the idle parent. The child's real mailbox remains active.
        let (quiet_sender, _quiet_receiver) = mpsc::channel(AGENT_CHANNEL_CAPACITY);
        let root_sender = std::mem::replace(
            &mut runtime
                .agents
                .write()
                .unwrap()
                .get_mut(&session.root)
                .unwrap()
                .sender,
            AgentSender::new(quiet_sender),
        );

        tracking.release(0);
        let next = tracking.request(1).await;
        let expected = [
            format!("Owner input: {first}"),
            format!("Owner input: {second}"),
        ];
        assert_eq!(parent_inputs(&next.messages), expected);
        assert_eq!(texts(&next.messages), ["test:child-initial"]);
        if first_calls_tool {
            assert!(next.messages.iter().any(|message| matches!(message,
                Message::Tool(results) if results.iter().any(|result|
                    result.call_id == "queue-todo" && !result.is_error))));
        } else {
            assert!(next.messages.iter().any(|message| matches!(message,
                Message::Assistant(items) if items == &vec![AssistantContent::text("text/0", 0, "answer-0")])));
        }
        assert!(
            !runtime
                .jobs
                .snapshot(job)
                .await
                .unwrap()
                .state
                .is_terminal(),
            "the child job must not complete before answering the parent updates"
        );

        tracking.release(1);
        let completed = bounded(runtime.jobs.wait(job, None, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        assert_eq!(completed.output, Some(json!("answer-1")));
        runtime
            .agents
            .write()
            .unwrap()
            .get_mut(&session.root)
            .unwrap()
            .sender = root_sender;
        stop(&session).await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn running_child_parent_inputs_are_fifo_once_and_preserve_tool_results() {
        running_child_receives_parent_inputs(true).await;
    }

    #[tokio::test]
    async fn running_child_parent_inputs_continue_after_final_response() {
        running_child_receives_parent_inputs(false).await;
    }

    #[tokio::test]
    async fn queued_fifo_images_and_all_model_changes_commit_before_next_request() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(true);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let first_image = root.path().join("first.png");
        let second_image = root.path().join("second.jpg");
        fs::write(&first_image, b"first image fixture")
            .await
            .unwrap();
        fs::write(&second_image, b"second image fixture")
            .await
            .unwrap();
        let turn = prompt(&session, "test:initial");
        let in_flight = tracking.request(0).await;
        assert_eq!(in_flight.model, "first-model");

        let first_token = QueuedPromptToken::new();
        let first = enqueue(
            &session,
            "test:queued-one",
            vec![first_image],
            Some("second"),
            &first_token,
        );
        buffered(&session, 1).await;
        let second_token = QueuedPromptToken::new();
        let second = enqueue(
            &session,
            "test:queued-two",
            vec![second_image],
            Some("third"),
            &second_token,
        );
        buffered(&session, 2).await;
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        assert!(!first_token.is_claimed());
        assert!(!second_token.is_claimed());
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(tracking.requests.lock().unwrap().len(), 1);

        tracking.release(0);
        let next = tracking.request(1).await;
        bounded(first).await.unwrap().unwrap();
        bounded(second).await.unwrap().unwrap();
        assert!(
            !turn.is_finished(),
            "receipts must not wait for the gated next response"
        );
        assert!(first_token.is_claimed());
        assert!(second_token.is_claimed());
        assert!(
            !first_token.cancel(),
            "a committed submission cannot be recalled"
        );
        assert!(!second_token.cancel());
        assert_eq!(next.model, "third-model");
        assert_eq!(
            texts(&next.messages),
            ["test:initial", "test:queued-one", "test:queued-two"]
        );
        let images = next
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::User(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .filter_map(|block| match block {
                UserContent::Image { image } => Some(image),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (image, expected) in images.iter().zip([
            b"first image fixture".as_slice(),
            b"second image fixture".as_slice(),
        ]) {
            use base64::Engine as _;
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(image.data_base64.as_ref().unwrap())
                    .unwrap(),
                expected
            );
        }
        assert!(
            next.messages.iter().any(|message| matches!(
                message,
                Message::Tool(results) if results.iter().any(|result|
                    result.call_id == "queue-todo" && !result.is_error)
            )),
            "the ongoing tool call must finish normally"
        );

        let records = runtime.store.records().await;
        let changes = records
            .iter()
            .filter_map(|record| match &record.event {
                SessionEvent::ModelChanged { model_profile, .. } => Some(model_profile.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            changes,
            ["second", "third"],
            "do not collapse intermediate captured model changes"
        );
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(
            tracking.requests.lock().unwrap().len(),
            2,
            "queued inputs must not become additional turns"
        );
        assert_eq!(
            texts(&committed(&session).await),
            ["test:initial", "test:queued-one", "test:queued-two"]
        );
    }

    #[tokio::test]
    async fn queued_input_is_not_lost_at_a_final_response_boundary() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let token = QueuedPromptToken::new();
        let receipt = enqueue(&session, "test:follow-up", vec![], None, &token);
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        tracking.release(0);
        let next = tracking.request(1).await;
        bounded(receipt).await.unwrap().unwrap();
        assert!(token.is_claimed());
        assert_eq!(texts(&next.messages), ["test:initial", "test:follow-up"]);
        // A final response may complete the original prompt before the queued
        // continuation starts. The queued receipt still acknowledges its commit
        // without waiting for that continuation's gated provider response.
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn canceled_buffered_submission_never_commits_text_or_model() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(true);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());
        let runtime = &session.runtime;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let token = QueuedPromptToken::new();
        let receipt = enqueue(&session, "test:canceled", vec![], Some("second"), &token);
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        assert!(token.cancel());
        assert!(token.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        assert!(bounded(receipt).await.unwrap().is_err());
        assert!(!token.is_claimed());
        assert_eq!(next.model, "first-model");
        assert_eq!(texts(&next.messages), ["test:initial"]);
        assert!(
            !runtime
                .store
                .records()
                .await
                .iter()
                .any(|record| matches!(record.event, SessionEvent::ModelChanged { .. }))
        );
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session).await), ["test:initial"]);
        assert_eq!(tracking.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ordinary_prompt_still_waits_for_its_own_turn() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(true);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = Arc::new(harness.new_session().await.unwrap());

        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let ordinary = prompt(&session, "test:ordinary");
        buffered(&session, 1).await;
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(texts(&next.messages), ["test:initial"]);
        assert!(!ordinary.is_finished());
        assert!(!turn.is_finished());
        tracking.release(1);
        assert_eq!(bounded(turn).await.unwrap().unwrap(), "answer-1");
        assert_eq!(bounded(ordinary).await.unwrap().unwrap(), "answer-2");
        assert_eq!(
            texts(&tracking.request(2).await.messages),
            ["test:initial", "test:ordinary"]
        );
        stop(&session).await;
        assert_eq!(tracking.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn already_canceled_token_never_starts_a_turn() {
        let root = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = harness.new_session().await.unwrap();
        let runtime = &session.runtime;
        let token = QueuedPromptToken::default();
        assert!(token.cancel());
        assert!(
            bounded(session.enqueue_prompt_with_options(
                "test:already-canceled",
                &[],
                PromptOptions {
                    model: Some("second".into())
                },
                token.clone(),
            ))
            .await
            .is_err()
        );
        assert!(!token.is_claimed());
        stop(&session).await;
        assert!(tracking.requests.lock().unwrap().is_empty());
        assert!(texts(&committed(&session).await).is_empty());
        assert!(
            !runtime
                .store
                .records()
                .await
                .iter()
                .any(|record| matches!(record.event, SessionEvent::ModelChanged { .. }))
        );
    }

    #[tokio::test]
    async fn queued_validation_errors_do_not_claim_or_commit() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let tracking = Tracking::new(false);
        let harness = harness(root.path(), tracking.clone()).await;
        let session = harness.new_session().await.unwrap();
        let runtime = &session.runtime;
        let unsupported = root.path().join("unsupported.txt");
        fs::write(&unsupported, b"not an image").await.unwrap();
        let outside_image = outside.path().join("outside.png");
        fs::write(&outside_image, b"outside").await.unwrap();
        let oversized = root.path().join("oversized.png");
        fs::File::create(&oversized)
            .await
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .await
            .unwrap();
        let before = runtime.store.records().await.len();
        let cases = [
            (Some("missing-model"), vec![], "model"),
            (None, vec![root.path().join("missing.png")], "missing"),
            (None, vec![unsupported], "format"),
            (None, vec![outside_image], "outside"),
            (None, vec![oversized], "size"),
            (
                None,
                vec![root.path().join("missing.png"); MAX_IMAGES_PER_SUBMISSION + 1],
                "count",
            ),
        ];
        for (model, paths, case) in cases {
            let token = QueuedPromptToken::new();
            let error = bounded(session.enqueue_prompt_with_options(
                "test:invalid",
                &paths,
                PromptOptions {
                    model: model.map(str::to_owned),
                },
                token.clone(),
            ))
            .await
            .unwrap_err();
            match case {
                "model" => assert!(matches!(error, HarnessError::UnknownModelProfile(_))),
                "missing" => assert!(matches!(error, HarnessError::Io(_))),
                "format" => assert!(matches!(error, HarnessError::UnsupportedImage)),
                "outside" => assert!(matches!(error, HarnessError::OutsideWorkspace)),
                "size" | "count" => assert!(matches!(error, HarnessError::ImageLimit)),
                _ => unreachable!(),
            }
            assert!(!token.is_claimed(), "{case}");
            assert!(token.cancel(), "{case}");
            assert_eq!(runtime.store.records().await.len(), before, "{case}");
        }
        assert!(tracking.requests.lock().unwrap().is_empty());
        stop(&session).await;
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_resolve_queued_receipts_without_silent_loss() {
        for shutdown in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let tracking = Tracking::new(false);
            let harness = harness(root.path(), tracking.clone()).await;
            let session = Arc::new(harness.new_session().await.unwrap());

            let turn = prompt(&session, "test:initial");
            tracking.request(0).await;
            let token = QueuedPromptToken::new();
            let receipt = enqueue(&session, "test:lifecycle", vec![], None, &token);
            buffered(&session, 1).await;
            assert!(!receipt.is_finished());
            // Permit any continuation after interruption, but never release the
            // interrupted first invocation. No provider response can mask the race.
            tracking.release(1);
            if shutdown {
                bounded(session.shutdown()).await.unwrap();
            } else {
                bounded(session.interrupt()).await;
            }
            let outcome = bounded(receipt).await.unwrap();
            assert!(bounded(turn).await.unwrap().is_err());
            stop(&session).await;
            let count = texts(&committed(&session).await)
                .iter()
                .filter(|text| text.as_str() == "test:lifecycle")
                .count();
            if outcome.is_ok() {
                assert_eq!(
                    count, 1,
                    "successful receipt must correspond to one durable message"
                );
                assert!(token.is_claimed());
            } else {
                assert_eq!(
                    count, 0,
                    "failed receipt must not silently accept the message"
                );
            }
        }
    }
}

//! Individual queued-input validation, model selection, and commit receipts.

use super::*;

struct SelectedModel {
    name: String,
    profile: ModelProfile,
    replacement: Option<AgentContext>,
}

/// A model selection resolved against the destination before any journal
/// append, so an unsupported profile never opens a provider context.
struct PreparedModelSelection<'a> {
    runtime: &'a SessionRuntime,
    agent: &'a AgentId,
    context: &'a mut AgentContext,
    model_profile: &'a mut String,
    selection: Option<SelectedModel>,
}

impl<'a> PreparedModelSelection<'a> {
    /// `images` rejects an unsupported profile before any provider context opens.
    async fn prepare(
        runtime: &'a SessionRuntime,
        agent: &'a AgentId,
        context: &'a mut AgentContext,
        model_profile: &'a mut String,
        capabilities: &CapabilitySet,
        name: Option<String>,
        images: bool,
    ) -> Result<Self, HarnessError> {
        let name = name.filter(|name| name != model_profile);
        let profile = match &name {
            Some(name) => runtime
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
        let selection = match name {
            Some(name) if profile != context.profile => {
                let system = context.template.system().to_vec();
                let replacement = runtime
                    .open_agent_context(agent, profile.clone(), system, capabilities, false)
                    .await?;
                if !profile.supports_images && replacement.contains_images() {
                    return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
                }
                let replacement = Some(replacement);
                Some(SelectedModel {
                    name,
                    profile,
                    replacement,
                })
            }
            Some(name) => Some(SelectedModel {
                name,
                profile,
                replacement: None,
            }),
            None => None,
        };
        Ok(Self {
            runtime,
            agent,
            context,
            model_profile,
            selection,
        })
    }

    /// A durable submission binds its appends to the exact intent attempt.
    // `&mut self`: the provider context is not `Sync`, and the agent loop is `Send`.
    async fn accept(
        &mut self,
        claim: Option<&ClaimedInput>,
        event: SessionEvent,
    ) -> Result<crate::session::AcceptedAppend, HarnessError> {
        let message = matches!(event, SessionEvent::MessageCommitted { .. });
        let attempt = claim.and_then(|claim| claim.attempt);
        let accepted = self
            .runtime
            .store
            .accept_append_bound(self.agent.clone(), attempt, event)
            .await?;
        if let Some(claim) = claim {
            claim.accepted(accepted.identity(), message);
        }
        Ok(accepted)
    }

    async fn install(&mut self, claim: Option<&ClaimedInput>) -> Result<(), HarnessError> {
        let Some(selected) = self.selection.take() else {
            return Ok(());
        };
        let event = SessionEvent::ModelChanged {
            model_profile: selected.name.clone(),
            max_context: selected.profile.max_context,
        };
        self.accept(claim, event).await?.committed().await?;
        // No await between installation and its routing projection. The exact
        // prepared context is installed; no config/provider lookup is repeated.
        if let Some(replacement) = selected.replacement {
            *self.context = replacement;
        }
        *self.model_profile = selected.name;
        if let Some(live) = self
            .runtime
            .agents
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(self.agent)
        {
            live.model_profile.clone_from(self.model_profile);
        }
        Ok(())
    }
}

impl ClaimedInput {
    fn accepted(&self, identity: crate::session::AppendIdentity, is_message: bool) {
        if let Phase::Claimed { appends, message } = &mut *self.input.prepared.token.0.0.phase() {
            appends.push(identity);
            if is_message {
                *message = Some(identity);
            }
        }
    }

    /// Settle the claim and publish the receipt, however the caller was dropped.
    fn settle(self, result: Result<QueuedPromptCommit, HarnessError>) {
        let QueuedInput {
            prepared,
            committed,
        } = self.input;
        let result = result.map_err(|error| prepared.cancellation_handle().failed(error));
        // Release authority before the receipt is observable.
        drop(prepared);
        let _ = committed.send(result);
    }
}

impl SessionRuntime {
    pub(in crate::agent::runtime) async fn select_model(
        &self,
        agent: &AgentId,
        context: &mut AgentContext,
        model_profile: &mut String,
        capabilities: &CapabilitySet,
        model: String,
    ) -> Result<(), HarnessError> {
        PreparedModelSelection::prepare(
            self,
            agent,
            context,
            model_profile,
            capabilities,
            Some(model),
            false,
        )
        .await?
        .install(None)
        .await
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
        let mut claim = match input.try_claim(&self.queue_state) {
            Ok(claim) => claim,
            Err(input) => {
                input.reject(HarnessError::Interrupted);
                return false;
            }
        };
        let draft = &mut claim.input.prepared;
        let images = draft.content.iter().any(UserContent::is_image);
        let (model, content) = (draft.model.clone(), std::mem::take(&mut draft.content));
        // ModelChanged and MessageCommitted retain their event order and
        // partial-prefix meaning: a failure after ModelChanged is indeterminate.
        let result = async {
            let mut prepared = PreparedModelSelection::prepare(
                self,
                agent,
                context,
                model_profile,
                capabilities,
                model,
                images,
            )
            .await?;
            prepared.install(Some(&claim)).await?;
            let message = Message::User(content);
            let event = SessionEvent::MessageCommitted {
                message: message.clone(),
            };
            let accepted = prepared.accept(Some(&claim), event).await?;
            let identity = accepted.identity();
            let record = accepted.committed().await?;
            prepared.context.projected.push((record.sequence, message));
            if let Some(attempt) = claim.attempt {
                let settlement = crate::session::QueueSettlement::Committed {
                    event: identity.event,
                };
                let event = SessionEvent::QueueSettlement {
                    attempt,
                    settlement,
                };
                self.store.append(agent.clone(), event).await?;
            }
            Ok(QueuedPromptCommit {
                submission: claim.input.prepared.identity(),
                append: identity,
            })
        }
        .await;
        let consumed = result.is_ok();
        if consumed {
            // Receipt readers must already observe a busy agent when this input
            // starts a fresh turn; publishing afterward races the UI's idle check.
            self.activity(agent, AgentActivity::Working);
        }
        claim.settle(result);
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
        assert_eq!(tracking.count(), 1);

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
        assert_eq!(tracking.count(), 2);
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
        assert_eq!(tracking.count(), 1);

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
        assert_eq!(tracking.count(), 2);
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
        assert_eq!(tracking.count(), 2);
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
        assert_eq!(tracking.count(), 2);
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
        assert_eq!(tracking.count(), 3);
    }

    #[tokio::test]
    async fn already_canceled_token_never_starts_a_turn() {
        let (_root, tracking, session) = start(false).await;
        let prompt = QueuedPrompt {
            text: "test:already-canceled".into(),
            attachments: vec![],
            options: crate::agent::runtime::tests::model("second"),
            token: QueuedPromptToken::new().unwrap(),
        };
        let token_cancel = prompt.token.cancellation_handle();
        assert!(token_cancel.cancel());
        let enqueued = enqueue_prompts(&session, vec![prompt]);
        assert!(bounded(enqueued).await[0].is_err());
        assert!(!token_cancel.is_claimed());
        stop(&session).await;
        assert_eq!(tracking.count(), 0);
        assert!(texts(&committed(&session).await).is_empty());
        assert!(model_changes(&session).await.is_empty());
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
                token: QueuedPromptToken::new().unwrap(),
            };
            let token_cancel = prompt.token.cancellation_handle();
            let enqueued = enqueue_prompts(&session, vec![prompt]);
            let error = bounded(enqueued).await.pop().unwrap().unwrap_err();
            let QueuedPromptError::Rejected(error) = error else {
                panic!("{case}: {error:?}")
            };
            if case == "model" {
                assert!(matches!(error, UnknownModelProfile(_)), "{case}");
            } else {
                assert!(matches!(error, ImageLimit), "{case}");
            }
            assert!(!token_cancel.is_claimed(), "{case}");
            assert!(token_cancel.cancel(), "{case}");
            assert_eq!(runtime.store.records().await.len(), before, "{case}");
        }
        assert_eq!(tracking.count(), 0);
        stop(&session).await;
    }

    #[tokio::test]
    async fn interrupt_and_shutdown_resolve_queued_receipts_without_silent_loss() {
        for shutdown in [false, true] {
            let (_root, tracking, session) = start(false).await;
            let turn = prompt(&session, "test:initial");
            tracking.request(0).await;
            let (receipt, token_cancel) = enqueue(&session, "test:lifecycle", vec![], None);
            buffered(&session, 1).await;
            assert!(!receipt.is_finished());
            // Permit continuations, never the interrupted first invocation, to expose the race.
            tracking.release(1);
            if shutdown {
                bounded(session.shutdown()).await.unwrap();
            } else {
                bounded(session.interrupt()).await;
            }
            let outcome = bounded(receipt).await.unwrap();
            assert!(bounded(turn).await.unwrap().is_err());
            stop(&session).await;
            let texts = texts(&committed(&session).await);
            let count = texts
                .iter()
                .filter(|text| *text == "test:lifecycle")
                .count();
            // A successful receipt is one durable message; a failed one accepts nothing.
            assert_eq!(count, usize::from(outcome.is_ok()));
            assert!(outcome.is_err() || token_cancel.is_claimed());
        }
    }

    #[tokio::test]
    async fn dropped_enqueue_waiter_does_not_drop_live_projection_or_duplicate_message() {
        let (_root, tracking, session) = start(false).await;
        let turn = prompt(&session, "test:initial");
        tracking.request(0).await;
        let (waiter, cancellation) =
            enqueue(&session, "test:dropped-waiter", vec![], Some("second"));
        buffered(&session, 1).await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        tracking.release(0);
        let next = tracking.request(1).await;
        assert_eq!(next.model, "second-model");
        let expected = ["test:initial", "test:dropped-waiter"];
        assert_eq!(texts(next.messages()), expected);
        assert_eq!(cancellation.recovery().unwrap().appends.len(), 2);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        assert_eq!(texts(&committed(&session).await), expected);
        stop(&session).await;
    }
}

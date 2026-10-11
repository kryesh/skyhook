//! Individual queued-input validation, model selection, and commit receipts.

use super::*;

impl SessionRuntime {
    /// Resolve an input's model and mode, where given and different, without
    /// journaling anything: a rejected selection changes nothing. A mode is the root
    /// agent's capabilities, the tools they allow and its instructions; children it
    /// already started keep their own. The root also adopts the configuration's
    /// current definition of the mode it runs, or the default mode once its own is
    /// gone, when the input names none. `images` rejects an unsupported profile
    /// before any provider context opens.
    // Boxed as a `dyn` future: proving the agent loop `Send` through this body
    // otherwise exceeds the compiler's recursion limit. Exclusive borrows keep it
    // `Send` without requiring the context to be `Sync`.
    pub(in crate::agent::runtime) fn select<'a>(
        &'a self,
        turn: &'a mut TurnContext<'_>,
        context: &'a mut AgentContext,
        settings: &'a mut AgentSettings,
        options: Selection,
        images: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<Selected, HarnessError>> {
        Box::pin(async move {
            let agent = turn.agent;
            let model = options
                .model
                .map(|model| model.name)
                .filter(|name| name != &context.profile.name);
            let snapshot = match &model {
                Some(name) => crate::session::ProfileSnapshot {
                    name: name.clone(),
                    profile: self.model_entry(name)?.profile,
                },
                None => context.profile.clone(),
            };
            let profile = &snapshot.profile;
            if images && !profile.supports_images {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let requested = options.mode.map(|mode| mode.name);
            let current = settings.mode.as_ref();
            let mode = if agent.depth() != 0 {
                // A child keeps the mode it started in.
                match requested {
                    Some(name) if current.is_none_or(|mode| mode.name != name) => {
                        return Err(HarnessError::ModeChangeUnsupported(name));
                    }
                    _ => None,
                }
            } else {
                match self.root_mode(current, requested) {
                    Some(name) => {
                        self.mode_change(agent, current, &settings.capabilities, &name)?
                    }
                    None => None,
                }
            };
            let replacement = if let Some(change) = &mode {
                let system = self.mode_prompt(agent, turn.location, change).await?;
                Some(self.mode_context(agent, snapshot.clone(), system, change)?)
            } else if model.is_some() {
                let system = context.template.system().to_vec();
                // The tools stay pinned across a model change.
                let tools = Some(context.template.to_request().tools);
                let capabilities = &settings.capabilities;
                let opening =
                    self.open_agent_context(agent, snapshot.clone(), system, capabilities, tools);
                Some(opening?)
            } else {
                None
            };
            if !profile.supports_images
                && replacement
                    .as_ref()
                    .is_some_and(AgentContext::contains_images)
            {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let mut events = Vec::new();
            if model.is_some() {
                let profile = snapshot;
                events.push((agent.clone(), SessionEvent::ModelChanged { profile }));
            }
            if let Some(change) = &mode {
                events.push((agent.clone(), change.event()));
            }
            Ok(Selected {
                events,
                replacement,
                mode,
            })
        })
    }

    /// Journal `selected` and the `message` it applies to in one transaction, then
    /// install them. `ModelChanged` and `ModeChanged` precede that message.
    pub(in crate::agent::runtime) async fn adopt(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
        mut selected: Selected,
        message: Option<Message>,
    ) -> Result<(), SessionError> {
        let mut events = std::mem::take(&mut selected.events);
        let message = message.map(|message| SessionEvent::MessageCommitted { message });
        events.extend(message.map(|message| (turn.agent.clone(), message)));
        if !events.is_empty() {
            self.store.append_all(events).await?;
        }
        // No await between installation and its routing projection.
        selected.install(turn, context, settings);
        Ok(())
    }

    /// Deliver the owner's pending events with the selection its next turn makes
    /// unasked, the root's configured mode, in one transaction. A selection that
    /// cannot be prepared delivers nothing: the events stay pending for the input
    /// that resumes the agent. None when nothing was pending.
    pub(in crate::agent::runtime) async fn deliver_selected(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
    ) -> Option<Result<(), HarnessError>> {
        let options = Selection::default();
        let mut selected = match self.select(turn, context, settings, options, false).await {
            Ok(selected) => selected,
            Err(error) => return Some(Err(error)),
        };
        let preceding: Vec<_> = std::mem::take(&mut selected.events)
            .into_iter()
            .map(|(_, event)| event)
            .collect();
        // The delivery commits after the selection, so it is presented under the
        // grants the selection gives.
        let capabilities =
            (selected.mode.as_ref()).map_or(&turn.capabilities, |change| &change.capabilities);
        let viewer = crate::tool::diagnostic::DiagnosticViewer::new(capabilities, turn.location);
        match self.deliver_events(turn.agent, viewer, &preceding).await {
            Ok(None) => None,
            Ok(Some(_)) => {
                selected.install(turn, context, settings);
                Some(Ok(()))
            }
            Err(error) => Some(Err(error)),
        }
    }

    pub(in crate::agent::runtime) async fn consume_queued_input(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        settings: &mut AgentSettings,
        input: QueuedInput,
    ) -> bool {
        if let Err(refused) = self.store.admits(Command::Submit) {
            input.reject(refused.into());
            return false;
        }
        if !input.cancellation.try_claim() {
            input.reject(HarnessError::Interrupted);
            return false;
        }
        let QueuedInput {
            content,
            options,
            committed,
            ..
        } = input;
        let images = content.iter().any(UserPart::is_image);
        let result = async {
            let selected = self
                .select(turn, context, settings, options, images)
                .await?;
            let message = Some(Message::User(content));
            self.adopt(turn, context, settings, selected, message)
                .await?;
            Ok(())
        }
        .await;
        let consumed = result.is_ok();
        let _ = committed.send(result);
        consumed
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::agent::runtime::tests::{answer, inbox, model_ref, scripted_provider};

    #[tokio::test]
    async fn running_child_parent_inputs_are_fifo_once_after_tool_or_final_responses() {
        for first_calls_tool in [true, false] {
            running_child_receives_parent_inputs(first_calls_tool).await;
        }
    }

    async fn running_child_receives_parent_inputs(first_calls_tool: bool) {
        let (_root, tracking, session) = start(first_calls_tool).await;
        let runtime = &session.runtime;
        let launch = "return await tool.agent({prompt:'test:child-initial',name:'kid',bg:true});";
        let launched = bounded(session.run_script(launch)).await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let initial = tracking.request(0).await;
        // A follow-up addressed by name is refused rather than started as a second child.
        let again = "return await tool.agent({prompt:'test:follow-up',name:'kid'});";
        let refused = bounded(session.run_script(again)).await.unwrap().value;
        let error = refused["value"]["error"].as_str().unwrap();
        assert!(error.contains(&format!("tool.job({job}).send")), "{error}");
        assert_eq!(texts(initial.messages()), ["test:child-initial"]);
        assert!(parent_inputs(initial.messages()).is_empty());
        let child = {
            let agents = runtime.agents(|agents| agents.within(&session.root));
            let mut children = agents.into_iter().filter(|id| id != &session.root);
            let child = children.next().unwrap();
            assert!(children.next().is_none());
            child
        };
        // A send acknowledgement alone does not prove forwarding: observe the child's inbox.
        let forwarded = async |count| {
            bounded(async {
                while inbox(&session, &child) != count {
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
        assert_eq!(accepted.value["value"], json!({}));
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
                Sent::Assistant(items) if items == &vec![AssistantItem::text("text/0", 0, "answer-0")])));
        }
        let state = runtime.jobs.snapshot(job).unwrap().state;
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
    async fn child_names_are_owner_scoped_and_survive_completion_but_not_failed_launches() {
        let (_root, tracking, session) = start(false).await;
        let runtime = &session.runtime;
        let root_inbox = quiet_root(&session);
        // Each invocation is a separate script, but the caller owns its children.
        let call = async |owner: &AgentId, source: &str| {
            bounded(runtime.executor.execute(
                owner.clone(),
                "script",
                json!({"source":source}),
                None,
            ))
            .await
            .unwrap()
            .output
            .value["value"]
                .clone()
        };
        let launch = |name| {
            format!("return await tool.agent({{prompt:'test:{name}',name:'{name}',bg:true}});")
        };
        let id =
            |view: &serde_json::Value| serde_json::from_value::<JobId>(view["id"].clone()).unwrap();
        let refused = |view: &serde_json::Value, existing: JobId| {
            assert_eq!(view["state"], "failed");
            let error = view["error"].as_str().unwrap();
            assert!(error.contains("already exists"), "{error}");
            assert!(
                error.contains(&format!("tool.job({existing}).send")),
                "{error}"
            );
        };

        // An admitted tool job that never installs a child must not manufacture
        // a permanent name reservation, even though it has role Agent and a name.
        let invalid = call(
            &session.root,
            "return await tool.agent({prompt:'bad-depth',name:'worker',depth:999});",
        )
        .await;
        assert_eq!(invalid["state"], "failed");
        assert!(invalid["error"].as_str().unwrap().contains("depth"));
        assert!(runtime.jobs.snapshot(id(&invalid)).is_ok());
        let first = call(&session.root, &launch("worker")).await;
        assert_ne!(first["state"], "failed", "{first}");
        let first = id(&first);
        tracking.request(0).await;
        let duplicate = "return await tool.agent({prompt:'duplicate',name:'worker'});";
        refused(&call(&session.root, duplicate).await, first);

        // A second caller may install its own worker while the root's worker is
        // still active. Resolve its identity from the actual installation record.
        let manager = id(&call(
            &session.root,
            "return await tool.agent({prompt:'test:manager',name:'manager',depth:1,bg:true});",
        )
        .await);
        tracking.request(1).await;
        let records = runtime.store.records();
        let manager = records
            .iter()
            .find_map(|record| match &record.event {
                SessionEvent::AgentStarted {
                    owner_job: Some(job),
                    ..
                } if *job == manager => Some(record.agent.clone()),
                _ => None,
            })
            .unwrap();
        let second = call(&manager, &launch("worker")).await;
        assert_ne!(second["state"], "failed", "{second}");
        let second = id(&second);
        assert_ne!(first, second);
        tracking.request(2).await;
        let completed = bounded(runtime.jobs.wait(second, None, true))
            .await
            .unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        refused(&call(&manager, duplicate).await, second);

        tracking.release(0);
        let completed = bounded(runtime.jobs.wait(first, None, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        refused(&call(&session.root, duplicate).await, first);
        // Rejecting a duplicate does not interfere with the intended follow-up
        // path: the original child resumes under its existing ID and history.
        bounded(runtime.jobs.send(first, json!("test:resume")))
            .await
            .unwrap();
        let resumed = tracking.request(3).await;
        assert!(
            parent_inputs(resumed.messages())
                .iter()
                .any(|text| text.contains("test:resume"))
        );
        let completed = bounded(runtime.jobs.wait(first, None, true)).await.unwrap();
        assert_eq!(completed.output, Some(json!("answer-3")));
        drop(root_inbox);
        stop(&session).await;
        assert_eq!(count(&tracking), 4);
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
        let turn = spawn_prompt(&session, "test:initial");
        let in_flight = tracking.request(0).await;
        assert_eq!(in_flight.model.as_str(), "first-model");

        let first_image = vec![image("first.png", &first_png)];
        let (first, first_cancel) = enqueue(
            &session,
            "test:queued-one",
            first_image,
            Some("queue-test/second"),
        );
        buffered(&session, 1).await;
        let second_image = vec![image("second.png", &second_png)];
        let (second, second_cancel) = enqueue(
            &session,
            "test:queued-two",
            second_image,
            Some("queue-test/third"),
        );
        buffered(&session, 2).await;
        assert!(!first.is_finished() && !second.is_finished());
        assert!(!first_cancel.is_claimed() && !second_cancel.is_claimed());
        assert_eq!(texts(&committed(&session)), ["test:initial"]);
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
        assert_eq!(next.model.as_str(), "third-model");
        let expected = ["test:initial", "test:queued-one", "test:queued-two"];
        assert_eq!(texts(next.messages()), expected);
        let blocks = next.messages().flat_map(|message| match message {
            Sent::User(blocks) => blocks.as_slice(),
            _ => &[],
        });
        let images = blocks.filter_map(|block| match block {
            SentPart::Attachment {
                attachment: crate::media::AttachmentRef::Image(image),
            } => Some(next.blobs.get(&image.blob).unwrap()),
            _ => None,
        });
        let expected_images = [first_png.bytes(), second_png.bytes()];
        assert_eq!(images.collect::<Vec<_>>(), expected_images);
        // The ongoing tool call must finish normally.
        assert!(next.messages().any(todo_finished));
        // Do not collapse intermediate captured model changes.
        assert_eq!(
            model_changes(&session),
            ["queue-test/second", "queue-test/third"]
        );
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        // Queued inputs must not become additional turns.
        assert_eq!(count(&tracking), 2);
        assert_eq!(texts(&committed(&session)), expected);
    }

    #[tokio::test]
    async fn selecting_a_model_with_an_equal_profile_switches_its_provider() {
        let root = tempfile::tempdir().unwrap();
        let requests = Default::default();
        let (first, second) = (
            scripted_provider(&requests, [answer("from a")]),
            scripted_provider(&requests, [answer("from b")]),
        );
        let profile = crate::tests::profile("same", false);
        let harness = HarnessBuilder::new(root.path())
            .session_root(root.path().join("sessions"))
            .provider(
                provider_name("a"),
                first.clone(),
                models([("m", profile.clone())]),
            )
            .provider(provider_name("b"), second.clone(), models([("m", profile)]))
            .default_model("a/m".parse().unwrap())
            .build()
            .await
            .unwrap();
        let session = ephemeral_session(&harness).await;
        assert_eq!(session.prompt("test:first").await.unwrap(), "from a");
        let options = session
            .selection(Some(&"b/m".parse().unwrap()), None)
            .unwrap();
        let switched = session.prompt_with_options("test:second", &[], options);
        assert_eq!(bounded(switched).await.unwrap(), "from b");
        assert_eq!((first.remaining(), second.remaining()), (0, 0));
        stop(&session).await;
    }

    #[tokio::test]
    async fn queued_input_is_not_lost_at_a_final_response_boundary() {
        let (_root, tracking, session) = start(false).await;
        let turn = spawn_prompt(&session, "test:initial");
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
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let (receipt, token_cancel) =
            enqueue(&session, "test:canceled", vec![], Some("queue-test/second"));
        buffered(&session, 1).await;
        assert!(!receipt.is_finished());
        assert!(token_cancel.cancel());
        assert!(token_cancel.cancel());
        tracking.release(0);
        let next = tracking.request(1).await;
        assert!(bounded(receipt).await.unwrap().is_err());
        assert!(!token_cancel.is_claimed());
        assert_eq!(next.model.as_str(), "first-model");
        assert_eq!(texts(next.messages()), ["test:initial"]);
        assert!(model_changes(&session).is_empty());
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        assert_eq!(texts(&committed(&session)), ["test:initial"]);
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn ordinary_prompt_still_waits_for_its_own_turn() {
        let (_root, tracking, session) = start(true).await;
        let turn = spawn_prompt(&session, "test:initial");
        tracking.request(0).await;
        let ordinary = spawn_prompt(&session, "test:ordinary");
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
        let (_root, tracking, session) = start(false).await;
        let runtime = &session.runtime;
        let missing = model_ref("missing");
        let unknown = session.selection(Some(&missing), None).unwrap_err();
        assert!(matches!(unknown, HarnessError::UnknownModel(name) if name == missing));
        // One byte over the per-image limit, and one image over the count limit.
        let image = |bytes: &[u8]| crate::media::Attachment::Image {
            file: None,
            image: crate::tests::png(bytes),
        };
        let (oversized, small) = (image(&vec![0; MAX_IMAGE_BYTES as usize - 7]), image(b""));
        let before = runtime.store.records().len();
        let cases = [
            (vec![oversized], "size"),
            (vec![small; MAX_IMAGES_PER_SUBMISSION + 1], "count"),
        ];
        for (attachments, case) in cases {
            let prompt = QueuedPrompt {
                text: "test:invalid".into(),
                attachments,
                options: Selection::default(),
                cancellation: QueuedPromptCancellation::default(),
            };
            let token_cancel = prompt.cancellation.clone();
            let enqueued = enqueue_prompts(&session, vec![prompt]);
            let error = bounded(enqueued).await.pop().unwrap().unwrap_err();
            assert!(matches!(error, HarnessError::ImageLimit), "{case}");
            assert!(!token_cancel.is_claimed(), "{case}");
            assert!(token_cancel.cancel(), "{case}");
            assert_eq!(runtime.store.records().len(), before, "{case}");
        }
        assert_eq!(count(&tracking), 0);
        stop(&session).await;
    }
}

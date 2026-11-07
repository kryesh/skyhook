//! Model and mode selection for an agent's input, and its adoption.

use super::*;

/// The messages an adoption journals with its selection.
pub(in crate::agent::runtime) enum Adopting {
    /// What the agent took itself: a selection alone, or the input that begins
    /// its turn, which commits even when an interrupt stopped that turn first.
    Taken(Option<Message>),
    /// Queued input joining the turn, refused once it is no longer busy.
    Joins(Vec<Message>),
}

impl SessionRuntime {
    /// Resolve an input's model and mode, where given and different, without
    /// journaling anything: a rejected selection changes nothing. A mode is the root
    /// agent's capabilities, the tools they allow and its instructions; children it
    /// already started keep their own. The root also adopts the configuration's
    /// current definition of the mode it runs, or the default mode once its own is
    /// gone, when the input names none. An agent whose model the configuration no
    /// longer has is refused unless the input names another, and `images` rejects
    /// an unsupported profile, before any provider context opens.
    // Boxed as a `dyn` future: proving the agent loop `Send` through this body
    // otherwise exceeds the compiler's recursion limit. Exclusive borrows keep it
    // `Send` without requiring the context to be `Sync`.
    pub(in crate::agent::runtime) fn select<'a>(
        &'a self,
        turn: &'a mut TurnContext<'_>,
        context: &'a mut AgentContext,
        options: Selection,
        images: bool,
    ) -> futures_util::future::BoxFuture<'a, Result<Selected, HarnessError>> {
        Box::pin(async move {
            let agent = turn.agent;
            // An agent whose model is gone runs only on another that its input names.
            let model = match options.model {
                Some(model) => Some(model.name),
                None => {
                    context.provider()?;
                    None
                }
            };
            let model = model.filter(|name| name != &context.model.profile.name);
            let snapshot = match &model {
                Some(name) => crate::session::ProfileSnapshot {
                    name: name.clone(),
                    profile: self.model_entry(name)?.profile,
                },
                None => context.model.profile.clone(),
            };
            let profile = &snapshot.profile;
            if images && !profile.supports_images {
                return Err(HarnessError::ImagesUnsupported(profile.model.clone()));
            }
            let requested = options.mode.map(|mode| mode.name);
            let current = turn.settings.mode.as_ref();
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
                    Some(name) => self.mode_change(agent, current, &name)?,
                    None => None,
                }
            };
            let replacement = if let Some(change) = &mode {
                let (mode, capabilities) = (Some(&change.mode), &change.capabilities);
                let (location, zone) = (turn.location, change.zone.clone());
                let system =
                    self.system_prompt(agent, location, change.depth, mode, capabilities, zone)?;
                let context = self
                    .open_agent_context(agent, snapshot.clone(), system, capabilities, None)
                    .await?;
                Some(context)
            } else if model.is_some() {
                let system = context.model.system.clone();
                // The tools stay pinned across a model change.
                let tools = Some(context.model.tools.clone());
                let capabilities = &turn.settings.capabilities;
                let opening =
                    self.open_agent_context(agent, snapshot.clone(), system, capabilities, tools);
                Some(opening.await?)
            } else {
                None
            };
            if let Some(replacement) = &replacement {
                replacement.admits_images()?;
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

    /// Journal `selected` and the messages it applies to in one transaction,
    /// admitted as a submission when there are any, then install them.
    /// `ModelChanged` and `ModeChanged` precede those messages.
    pub(in crate::agent::runtime) async fn adopt(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
        mut selected: Selected,
        adopting: Adopting,
    ) -> Result<(), HarnessError> {
        let (messages, joins) = match adopting {
            Adopting::Taken(message) => (Vec::from_iter(message), false),
            Adopting::Joins(messages) => (messages, true),
        };
        let command = if messages.is_empty() {
            Command::Journal
        } else {
            Command::Submit
        };
        let mut events = std::mem::take(&mut selected.events);
        let messages = messages.into_iter();
        let messages = messages.map(|message| SessionEvent::MessageCommitted { message });
        events.extend(messages.map(|message| (turn.agent.clone(), message)));
        if !events.is_empty() {
            let agent = turn.agent.clone();
            let plan = move |state: &CoreState| {
                if joins && !state.agents.busy(&agent) {
                    return Err(HarnessError::Interrupted);
                }
                Ok(Plan::journal(events))
            };
            self.store.commit(command, plan).await?;
        }
        // No await between installation and its routing projection.
        selected.install(turn, context);
        Ok(())
    }

    /// Deliver the owner's pending events with the selection its next turn makes
    /// unasked, the root's configured mode, in one transaction. A selection that
    /// cannot be prepared delivers nothing: the events stay pending for the input
    /// that resumes the agent. None when nothing was pending, or an interrupt
    /// stopped the turn first, which leaves them pending.
    pub(in crate::agent::runtime) async fn deliver_selected(
        &self,
        turn: &mut TurnContext<'_>,
        context: &mut AgentContext,
    ) -> Option<Result<(), HarnessError>> {
        let options = Selection::default();
        let mut selected = match self.select(turn, context, options, false).await {
            Ok(selected) => selected,
            Err(error) => return Some(Err(error)),
        };
        let preceding: Vec<_> = std::mem::take(&mut selected.events)
            .into_iter()
            .map(|(_, event)| event)
            .collect();
        // The delivery commits after the selection, so it is presented under the
        // grants the selection gives.
        let capabilities = (selected.mode.as_ref())
            .map_or(&turn.settings.capabilities, |change| &change.capabilities);
        let viewer = crate::tool::diagnostic::DiagnosticViewer::new(capabilities, turn.location);
        let delivery = self.deliver_events(turn.agent, viewer, &preceding, Boundary::Jobs);
        match delivery.await {
            Ok(None) | Err(HarnessError::Interrupted) => None,
            Ok(Some(_)) => {
                selected.install(turn, context);
                Some(Ok(()))
            }
            Err(error) => Some(Err(error)),
        }
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
        let launch = "return await tool.agent({prompt:'test:child-initial',name:'kid',bg:true});";
        let launched = bounded(session.run_script(launch)).await.unwrap();
        let job: JobId = serde_json::from_value(launched.value["value"]["id"].clone()).unwrap();
        let initial = tracking.request(0).await;
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
            let (key, child) = (Key::Agent(child.clone()), child.clone());
            found(&session, [key], move |state| {
                (state.agents.get(&child)?.inbox.len() == count).then_some(())
            })
            .await
        };

        // Exercise the public script send as well as the underlying job mailbox.
        let first = json!({"instruction": "test:parent-one", "nested": [1, true]});
        let second = json!("test:parent-two");
        let send = format!("return (await tool.control({{job:{job}, send:{first}}})).unwrap();");
        let accepted = bounded(session.run_script(send)).await.unwrap();
        assert_eq!(accepted.value["value"], serde_json::Value::Null);
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
        let state = runtime.jobs.snapshot(job).await.unwrap().state;
        // The child must answer the parent updates first.
        assert!(!state.is_terminal());

        tracking.release(1);
        let completed = bounded(runtime.jobs.wait(job, true)).await.unwrap();
        assert_eq!(completed.state, crate::job::JobState::Completed);
        assert_eq!(completed.output, Some(json!({"reply": "answer-1"})));
        drop(root_inbox);
        stop(&session).await;
        assert_eq!(count(&tracking), 2);
    }

    #[tokio::test]
    async fn queued_fifo_images_and_the_last_model_commit_before_next_request() {
        let (root, tracking, session) = start(true).await;
        let first_png = crate::tests::png(b"first image fixture").await;
        let second_png = crate::tests::png(b"second image fixture").await;
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
        // Both batches are one submission: only the selection it ends with is journaled.
        assert_eq!(model_changes(&session), ["queue-test/third"]);
        tracking.release(1);
        bounded(turn).await.unwrap().unwrap();
        stop(&session).await;
        // Queued inputs must not become additional turns.
        assert_eq!(count(&tracking), 2);
        assert_eq!(texts(&committed(&session)), expected);
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
}

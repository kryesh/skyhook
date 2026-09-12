use super::*;

pub struct QueuedInput {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) delivery: Option<QueuedPromptToken>,
    pub(super) text: String,
    pub(super) images: Vec<PathBuf>,
    pub(super) model: String,
}

pub(super) struct QueueDelivery {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) text: String,
    pub(super) images: Vec<PathBuf>,
    pub(super) model: String,
    pub(super) token: QueuedPromptToken,
}

/// Register each UI queue snapshot atomically, including attachment preparation.
pub(super) async fn queue_dispatcher(
    session: SessionHandle,
    mut rx: mpsc::UnboundedReceiver<Vec<QueueDelivery>>,
    tx: mpsc::UnboundedSender<Work>,
) {
    use futures_util::{StreamExt, stream::FuturesOrdered};
    let mut pending = FuturesOrdered::new();
    let mut open = true;
    while open || !pending.is_empty() {
        tokio::select! {
            biased;
            delivery = rx.recv(), if open => match delivery {
                Some(mut deliveries) => {
                    // Coalesce snapshots already waiting on the UI channel too.
                    while let Ok(more) = rx.try_recv() {
                        deliveries.extend(more);
                    }
                    let session = session.clone();
                    pending.push_back(async move {
                        let ids: Vec<_> = deliveries.iter()
                            .map(|input| (input.id, input.generation)).collect();
                        let inputs = deliveries.into_iter().map(|input| {
                            skyhook::agent::QueuedPrompt {
                                text: input.text,
                                paths: input.images,
                                options: skyhook::agent::PromptOptions { model: Some(input.model) },
                                token: input.token,
                            }
                        }).collect();
                        let results = session.enqueue_prompts_with_options(inputs).await;
                        let revision = session.observe().await.snapshot.revision;
                        ids.into_iter().zip(results).map(|((id, generation), result)| {
                            Work::QueueCommitted {
                                session: session.id(), id, generation, revision,
                                result: result.map_err(|error| error.to_string()),
                            }
                        }).collect::<Vec<_>>()
                    });
                }
                None => open = false,
            },
            Some(work) = pending.next(), if !pending.is_empty() => {
                for item in work {
                    let _ = tx.send(item);
                }
            }
        }
    }
}

// A draft identity only scopes UI state/notices; it never names a session directory.

impl App {
    pub fn submit(&mut self, text: String, images: Vec<PathBuf>) {
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        self.reject_pending_questions();
        self.draft_revision = self.draft_revision.wrapping_add(1);
        let queued = self.queued_input(text, images);
        if self.busy() || self.paused || !self.queue.is_empty() {
            self.queue.push_back(queued);
            self.refresh_queue_menu();
            self.deliver_queue();
            self.dirty = true;
            return;
        }
        self.send_input(queued);
    }
    pub(super) fn send_input(&mut self, queued: QueuedInput) {
        let Some(session) = self.session.clone() else {
            self.launch.model.clone_from(&queued.model);
            self.begin_session(PendingStart::Input(queued));
            return;
        };
        let QueuedInput {
            text,
            images,
            model,
            ..
        } = queued;
        self.launch.model.clone_from(&model);
        self.operation = true;
        self.awaiting_initial_input = true;
        self.initial_input_after = self
            .snapshot
            .records
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0);
        self.history.push(text.clone());
        self.history_index = None;
        let tx = self.tx.clone();
        self.set_title(&text);
        let status = self.status.clone();
        if self.remembered_model.as_deref() != Some(model.as_str()) {
            self.remembered_model = Some(model.clone());
            let remembered = model.clone();
            let notices = self.root_notifier();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = state::remember(&remembered) {
                    notices.send(format!("Could not save model selection: {error}"));
                }
            });
        }
        tokio::spawn(async move {
            status.flush().await;
            let result = session
                .prompt_with_options(
                    text,
                    &images,
                    skyhook::agent::PromptOptions { model: Some(model) },
                )
                .await;
            let _ = tx.send(Work::Done {
                session: session.id(),
                result: result.map(|_| ()).map_err(|e| e.to_string()),
            });
        });
        self.dirty = true;
    }
    pub(super) fn cancel_queue_delivery(&mut self) {
        for input in &mut self.queue {
            if input.delivery.as_ref().is_some_and(|token| token.cancel()) {
                input.delivery = None;
            }
        }
    }
    pub(super) fn deliver_queue(&mut self) {
        // Lag recovery replaces the snapshot without replaying each event.
        if self.awaiting_initial_input
            && self.snapshot.records.values().any(|record| {
                record.sequence > self.initial_input_after
                    && &record.agent == self.root_agent()
                    && matches!(
                        &record.event,
                        SessionEvent::MessageCommitted {
                            message: skyhook::provider::protocol::Message::User(_)
                        }
                    )
            })
        {
            self.awaiting_initial_input = false;
        }
        if self.paused
            || self.creating
            || self.stopping
            || self.switch_restore.is_some()
            || self.awaiting_initial_input
            || self.queue.is_empty()
        {
            return;
        }
        let Some(session) = self.session.clone() else {
            // Retain the head across creation, then register the entire queue.
            let input = self.queue.pop_front().unwrap();
            self.refresh_queue_menu();
            self.launch.model.clone_from(&input.model);
            self.begin_session(PendingStart::QueuedInput(input));
            return;
        };
        let sender = self.queue_sender.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(queue_dispatcher(session, rx, self.tx.clone()));
            tx
        });
        let mut deliveries = Vec::new();
        for input in &mut self.queue {
            if input.delivery.is_some() {
                continue;
            }
            input.generation = input.generation.wrapping_add(1);
            let token = QueuedPromptToken::new();
            input.delivery = Some(token.clone());
            let delivery = QueueDelivery {
                id: input.id,
                generation: input.generation,
                text: input.text.clone(),
                images: input.images.clone(),
                model: input.model.clone(),
                token,
            };
            deliveries.push(delivery);
        }
        if !deliveries.is_empty() && sender.send(deliveries).is_err() {
            self.queue_sender = None;
            self.paused = true;
            self.cancel_queue_delivery();
            self.notice("Could not deliver queued messages; resume to retry");
        }
    }
    pub(super) fn queue_committed(
        &mut self,
        id: u64,
        generation: u64,
        revision: u64,
        result: Result<(), String>,
    ) {
        let Some(index) = self.queue.iter().position(|input| {
            input.id == id && input.generation == generation && input.delivery.is_some()
        }) else {
            // Editing, cancellation and retry invalidate old acknowledgements.
            return;
        };
        match result {
            Ok(()) => {
                let input = self.queue.remove(index).unwrap();
                self.queue_activity_revision = self.queue_activity_revision.max(revision);
                self.launch.model.clone_from(&input.model);
                self.history.push(input.text.clone());
                self.history_index = None;
                self.set_title(&input.text);
                if self.remembered_model.as_deref() != Some(input.model.as_str()) {
                    self.remembered_model = Some(input.model.clone());
                    let notices = self.root_notifier();
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = state::remember(&input.model) {
                            notices.send(format!("Could not save model selection: {error}"));
                        }
                    });
                }
            }
            Err(error) => {
                self.queue[index].delivery = None;
                self.paused = true;
                self.cancel_queue_delivery();
                if let Some(paused) = &mut self.switch_restore {
                    *paused = true;
                }
                self.notice(format!(
                    "Queued message was not submitted: {error}. Resume to retry."
                ));
            }
        }
        self.refresh_queue_menu();
    }
    pub(super) fn queued_input(&mut self, text: String, images: Vec<PathBuf>) -> QueuedInput {
        self.next_queued_id += 1;
        QueuedInput {
            id: self.next_queued_id,
            generation: 0,
            delivery: None,
            text,
            images,
            model: self.model.clone(),
        }
    }
    pub(super) fn queue_items(&self) -> Vec<Item> {
        self.queue
            .iter()
            .map(|queued| {
                Item::new(
                    queued.id.to_string(),
                    crate::tui::format::brief(&queued.text, 100),
                    self.launch
                        .config
                        .models
                        .get(&queued.model)
                        .map_or(queued.model.as_str(), |profile| profile.model.as_str()),
                )
            })
            .collect()
    }
    pub(super) fn remove_queued(&mut self, id: u64) -> Option<QueuedInput> {
        let index = self.queue.iter().position(|queued| queued.id == id)?;
        if let Some(token) = &self.queue[index].delivery
            && !token.cancel()
        {
            self.notice("This message has already been submitted to the model");
            return None;
        }
        self.queue.remove(index)
    }
    pub(super) fn refresh_queue_menu(&mut self) {
        if !self
            .menu
            .as_ref()
            .is_some_and(|menu| matches!(menu.kind, MenuKind::Queue))
        {
            return;
        }
        let items = self.queue_items();
        let menu = self.menu.as_mut().unwrap();
        let selected = menu
            .filtered()
            .get(menu.selected)
            .map(|item| item.value.clone());
        menu.items = items;
        menu.selected = selected
            .and_then(|id| menu.filtered().iter().position(|item| item.value == id))
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    #[tokio::test]
    async fn queue_delivery_is_immediate_while_busy_and_cancellation_rejects_stale_acks() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.submit("pending".into(), vec![PathBuf::from("pending.png")]);
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        let id = app.queue[0].id;
        let generation = app.queue[0].generation;
        let token = app.queue[0].delivery.clone().expect("sent while busy");
        app.paused = true;
        app.cancel_queue_delivery();
        assert!(token.cancel());
        assert!(app.queue[0].delivery.is_none());
        app.queue_committed(id, generation, 0, Err("cancelled".into()));
        assert_eq!(app.queue.len(), 1);
        app.paused = false;
        app.deliver_queue();
        assert!(app.queue[0].generation > generation);
        app.queue_committed(id, generation, 0, Ok(()));
        assert_eq!(app.queue.len(), 1);
        assert!(app.history.is_empty());
        let edited = app.remove_queued(id).unwrap();
        assert_eq!(edited.images, [PathBuf::from("pending.png")]);
        assert!(app.queue.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn queue_waits_for_initial_input_and_commit_does_not_finish_original_operation() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.awaiting_initial_input = true;
        app.submit("followup".into(), vec![]);
        app.tick();
        assert!(app.queue[0].delivery.is_none());
        // The initial input commit releases follow-ups before its first request,
        // including when that request first needs compaction.
        let mut committed = app.snapshot.records.values().next_back().unwrap().clone();
        committed.sequence += 1;
        committed.event = SessionEvent::MessageCommitted {
            message: skyhook::provider::protocol::Message::User(vec![
                skyhook::provider::protocol::UserContent::Text {
                    text: "initial".into(),
                },
            ]),
        };
        app.observe(ObservedEvent {
            revision: app.snapshot.revision + 1,
            event: RuntimeEvent::Record(Box::new(committed)),
        });
        assert!(!app.awaiting_initial_input);
        let input = &app.queue[0];
        let id = input.id;
        let generation = input.generation;
        // This unit test synthesizes the ack rather than invoking a provider.
        assert!(input.delivery.as_ref().unwrap().cancel());
        app.work(Work::QueueCommitted {
            session: app.session_id().unwrap(),
            id,
            generation,
            revision: app.snapshot.revision + 1,
            result: Ok(()),
        });
        assert!(app.queue.is_empty());
        assert_eq!(app.history, ["followup"]);
        assert!(
            app.operation,
            "commit is not a turn-completion acknowledgement"
        );
        app.operation = false;
        assert!(
            app.busy(),
            "wait for observation activity after an idle enqueue"
        );
        app.snapshot.revision = app.queue_activity_revision;
        assert!(!app.busy());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn queue_failure_retains_row_and_pauses_remaining_deliveries() {
        let (_root, mut app) = fixture().await;
        app.operation = true;
        app.submit("first".into(), vec![]);
        app.submit("second".into(), vec![]);
        let id = app.queue[0].id;
        let generation = app.queue[0].generation;
        assert!(app.queue[0].delivery.as_ref().unwrap().cancel());
        app.queue_committed(id, generation, 0, Err("attachment missing".into()));
        assert!(app.paused);
        assert_eq!(app.queue.len(), 2);
        assert!(app.queue.iter().all(|input| input.delivery.is_none()));
        assert!(app.history.is_empty());
        app.session.as_ref().unwrap().shutdown().await.unwrap();
    }
}

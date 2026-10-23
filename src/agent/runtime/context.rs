//! One agent's context window and independently owned provider conversation.

use crate::{
    agent::HarnessError,
    identity::AgentId,
    provider::{
        ProviderContext,
        profile::{ModelProfile, StateMode},
        protocol::{ModelRequest, Usage},
    },
    session::{
        CompactionTrigger, EventRecord, Message, ModelContext, OpenedHistory, Projection,
        RecordSeq, Requested, RuntimeState, SessionError, SessionEvent, SessionStore, UserPart,
    },
};
use std::sync::{Arc, mpsc::Receiver};

pub(super) struct AgentContext {
    /// What its requests are sent under: the profile it was opened for, which a
    /// different selection reopens it for, and the prompt and tools it offers.
    pub model: ModelContext,
    pub projected: Projection,
    /// What serves its requests: none while the configuration no longer has its
    /// model, as only a reopened root's can be, until a selection replaces it.
    provider: Option<Box<dyn ProviderContext>>,
    /// The last request's blobs, which the next request reuses.
    pub blobs: crate::media::LoadedBlobs,
    /// What the agent's last completed response reported, which the next request
    /// compacts for, as of the last refresh.
    reported: Option<Usage>,
    /// The agent's latest request and the state ending its tail, which its response
    /// carries.
    requested: Requested,
    /// The last journal sequence reflected in `projected`.
    through: RecordSeq,
    /// The agent's records its history folds, applied after `through`.
    feed: Receiver<Arc<EventRecord>>,
}

impl AgentContext {
    pub async fn open(
        agent: &AgentId,
        model: ModelContext,
        provider: Option<Box<dyn ProviderContext>>,
        store: &SessionStore,
    ) -> Result<Self, SessionError> {
        let OpenedHistory {
            projection,
            through,
            feed,
        } = store.open_history(agent).await?;
        Ok(Self {
            model,
            projected: projection,
            provider,
            blobs: Default::default(),
            reported: store.reported(agent),
            requested: None,
            through,
            feed,
        })
    }

    /// The provider context serving the agent's model, unless the configuration
    /// no longer has that model.
    pub fn provider(&mut self) -> Result<&mut dyn ProviderContext, HarnessError> {
        match &mut self.provider {
            Some(provider) => Ok(provider.as_mut()),
            None => Err(HarnessError::UnknownModel(self.model.profile.name.clone())),
        }
    }

    /// The last journal sequence the history reflects.
    pub fn through(&self) -> RecordSeq {
        self.through
    }

    /// Fold what the journal committed for the agent since the last refresh, from any
    /// producer. A compaction among it replaces the history, which is read again.
    pub async fn refresh(
        &mut self,
        store: &SessionStore,
        agent: &AgentId,
    ) -> Result<(), SessionError> {
        // Read first: the feed then holds every record it takes through `applied`, and
        // the agent's other records change nothing the context folds.
        let applied = store.applied();
        let mut compacted = false;
        for record in self.feed.try_iter() {
            self.through = record.sequence;
            match &record.event {
                SessionEvent::Compaction { .. } => compacted = true,
                _ => self.projected.apply(&mut self.requested, &record),
            }
        }
        self.through = self.through.max(applied);
        self.reported = store.reported(agent);
        if compacted {
            self.projected = store.history(agent, self.through).await?;
        }
        Ok(())
    }

    /// Build the runtime-state tail only when the profile sends it. A persisting
    /// profile commits it to history instead, before the request.
    pub fn tail(&self, runtime: impl FnOnce() -> RuntimeState) -> Option<Message> {
        match self.model.profile.profile.state_mode {
            StateMode::None => None,
            StateMode::Dynamic | StateMode::Persist => Some(Message::State(runtime())),
        }
    }

    /// The next request as the provider receives it: the projected history, then the
    /// tail as this request's own state.
    pub fn request(&self, tail: Option<&Message>) -> ModelRequest {
        ModelRequest {
            context: self.projected.context(tail),
            ..self.model.template()
        }
    }

    /// The reported occupancy the next request compacts for first, if over the threshold:
    /// the completed response's, including cached input and generated output.
    pub fn compaction_due(&self) -> Option<CompactionTrigger> {
        let tokens = self.reported?.occupancy()?;
        needs_compaction(&self.model.profile.profile, tokens)
            .then_some(CompactionTrigger::Occupancy(tokens))
    }

    /// Refused unless its model supports the images its history holds.
    pub fn admits_images(&self) -> Result<(), HarnessError> {
        let profile = &self.model.profile.profile;
        match profile.supports_images || !self.contains_images() {
            true => Ok(()),
            false => Err(HarnessError::ImagesUnsupported(profile.model.clone())),
        }
    }

    fn contains_images(&self) -> bool {
        self.projected
            .messages
            .iter()
            .any(|projected| match &projected.message {
                Message::User(content) => content.iter().any(UserPart::is_image),
                Message::Tool(result) => !result.images.is_empty(),
                Message::Assistant(_) | Message::State(_) => false,
            })
    }
}

/// The share of the input budget (the context window less the output reserve), as a
/// fraction, whose reported use compacts it.
const COMPACTION_THRESHOLD: (u128, u128) = (9, 10);

fn needs_compaction(profile: &ModelProfile, tokens: u64) -> bool {
    let (numerator, denominator) = COMPACTION_THRESHOLD;
    let budget = profile.max_context.get() - profile.max_output.get();
    u128::from(tokens) * denominator >= u128::from(budget) * numerator
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::agent::runtime::tests::{
        Requests, Script, Step, answer, child_launch, ephemeral_session, send, test_harness,
    };
    use crate::provider::{ProviderErrorKind, protocol::ContextId};

    #[test]
    fn completed_usage_compacts_at_ninety_percent_after_reserving_configured_output() {
        let limits = [
            (128_000, 16_384, 100_455),
            (128_001, 1, 115_200),
            (128_002, 1, 115_201),
            (128_000, 127_999, 1),
            (u64::MAX, 1, 16_602_069_666_338_596_453),
        ];
        for (capacity, max_output, threshold) in limits {
            let profile = ModelProfile {
                max_context: crate::tests::limit(capacity),
                max_output: crate::tests::limit(max_output),
                ..crate::tests::profile("test", false)
            };
            // Cache writes are already included in input and must not count twice.
            let total = |tokens: u64| {
                let third = tokens / 3;
                let usage = Usage::prompted(third, third, third, tokens - 2 * third);
                usage.occupancy().unwrap()
            };
            for tokens in [threshold - 1, threshold, threshold + 1] {
                assert_eq!(
                    super::needs_compaction(&profile, total(tokens)),
                    tokens >= threshold,
                    "{capacity} {max_output:?} {tokens}"
                );
            }
            // Each partial sum overflows a u64; a wrapped total would be tiny.
            for overflow in [tests::usage(u64::MAX, 1, 0), tests::usage(u64::MAX, 0, 1)] {
                assert!(super::needs_compaction(
                    &profile,
                    overflow.occupancy().unwrap()
                ));
            }
        }
    }

    #[tokio::test]
    async fn cancelled_and_failed_children_release_their_context() {
        let root = tempfile::tempdir().unwrap();
        let steps = [
            Step::new(answer("done")).gated(),
            Step::new(answer("done")),
            Step::fail(ProviderErrorKind::Protocol.error("not retried")),
            Step::new(answer("done")),
        ];
        let script = Script::new(steps, &Requests::default());
        let sessions = root.path().join("sessions");
        let harness = test_harness(root.path(), &sessions, script.clone()).await;
        let session = ephemeral_session(&harness).await;
        let root_context = ContextId::from(&session.root);
        let jobs = &session.runtime.jobs;
        for (index, fail) in [(1, false), (2, true)] {
            let child = session.root.child(index);
            let spec = crate::job::JobSpec {
                role: crate::job::JobRole::Agent,
                ..crate::job::JobSpec::test(session.root.clone(), "agent")
            };
            let owner_job = jobs.create(spec).await.unwrap().into_test_id();
            let launch = child_launch(&session, None);
            let spawned = session
                .runtime
                .spawn_agent(child.clone(), Some(owner_job), launch);
            spawned.await.unwrap();
            let (done, received) = oneshot::channel();
            let input = AgentCommand::Input {
                options: Default::default(),
                content: vec![UserPart::Text {
                    text: "work".into(),
                }],
                done: Some(RequestCompletion::Child(done)),
            };
            send(&session, &child, input).await;
            if !fail {
                script.request(0).await;
                session.runtime.interrupt_tree(&child).await;
            }
            assert!(received.await.unwrap().is_err());
            // A cancelled child's loop ends; a failed one parks, and its retry
            // opens a context again from its history.
            script.released(&ContextId::from(&child)).await;
            session.prompt("root still usable").await.unwrap();
            assert!(!script.dropped(&root_context));
        }
        assert_eq!(script.opened.load(Ordering::SeqCst), 3);
        session.shutdown().await.unwrap();
        script.released(&root_context).await;
    }
}
